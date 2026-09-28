//! Every `sql` statement a dataset can reach names a table its own
//! `sql_schema` declares — and every column an INSERT or UPDATE writes.
//!
//! # The failure this catches
//!
//! The relational arms are the only ones with a schema. On Bolt,
//! `CREATE (:StressW {c: 1, s: 2})` needs no DDL and cannot name a table that
//! does not exist. On SQL every statement can, and the engine is the first
//! thing to notice — on a pod, after a corpus load, in the middle of a level.
//!
//! Two real defects lived in this gap and neither was visible by reading:
//!
//!   * `synthetic.sql_schema` declared `stressw (c, s)` while `node_create`
//!     wrote `INSERT INTO stressw (id, c, s)`. The column did not exist. The
//!     Bolt arm's Cypher ignores the `id`, so nothing outside the SQL dialect
//!     could have shown it.
//!   * `snb.sql_schema` declared none of `msgnode`, `uniq`, `churn_anchor`,
//!     `churn` or `churn_rel`, because those five tables appear only in the
//!     `synthetic` list — while the write ops that use them are declared
//!     `any`, meaning they render the SAME text on every corpus. Five
//!     profiles, `unique-create` and `delete-churn` among them, would have
//!     failed on their first statement against an SNB database.
//!
//! Both are the same shape: a statement and a schema drifting apart in a file
//! where nothing joined them. This test is that join.
//!
//! # What is NOT checked, said plainly
//!
//! Column references in SELECT lists, WHERE predicates and JOIN conditions.
//! Parsing those correctly means resolving aliases through subqueries and
//! CTEs, and a half-right resolver would fail on correct SQL, which is worse
//! than not checking: a gate people learn to override is not a gate. The two
//! positions that ARE checked — an INSERT's column list and an UPDATE's SET
//! targets — need no alias resolution at all, because a column there belongs
//! unambiguously to the table just named.
//!
//! Types are not checked either, nor is the direction of a join, nor whether
//! an index exists for the access path a statement takes. The last of those is
//! the one that matters most for a fair comparison and it cannot be checked
//! here at all: it is a property of a loaded database and a planner, and
//! `docs/bench/pg-snb-schema.sql` argues it index by index instead.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use engram_bench::catalogue::{Catalogue, Dialect};
use engram_bench::workload::{ALGO_SHAPES, Dataset, Shape};

/// A dataset's declared relational schema: table -> the columns it holds.
///
/// Parsed from the `CREATE TABLE` statements in `sql_schema` rather than
/// declared twice, so a schema this test approves is the schema the loader and
/// the harness build.
fn schema_of(cat: &Catalogue, dataset: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    for stmt in cat
        .dataset_list(dataset, "sql_schema")
        .expect("sql_schema is a list of strings")
    {
        let toks = lex(&stmt);
        let mut i = 0;
        while i < toks.len() {
            if eq(&toks[i], "table") {
                // `CREATE TABLE [IF NOT EXISTS] <name> (`
                let mut j = i + 1;
                while j < toks.len()
                    && (eq(&toks[j], "if") || eq(&toks[j], "not") || eq(&toks[j], "exists"))
                {
                    j += 1;
                }
                let Some(name) = toks.get(j).map(|t| t.to_lowercase()) else {
                    break;
                };
                let cols = column_defs(&toks[j + 1..]);
                out.insert(name, cols);
                break;
            }
            i += 1;
        }
    }
    out
}

/// The column NAMES from a `CREATE TABLE (...)` body: the first identifier of
/// each top-level comma-separated definition.
fn column_defs(toks: &[String]) -> BTreeSet<String> {
    let mut cols = BTreeSet::new();
    if toks.first().map(String::as_str) != Some("(") {
        return cols;
    }
    let mut depth = 0usize;
    let mut want = true;
    for t in toks {
        match t.as_str() {
            "(" => {
                depth += 1;
                if depth == 1 {
                    want = true;
                }
            }
            ")" => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            "," if depth == 1 => want = true,
            _ if depth == 1 && want && is_ident(t) => {
                cols.insert(t.to_lowercase());
                want = false;
            }
            _ => {}
        }
    }
    cols
}

/// Split SQL into identifiers, string literals and single-character
/// punctuation. Crude on purpose: the catalogue's SQL is hand-written, ASCII
/// and quoted with `'` only, and a real parser here would be a second engine
/// to keep correct.
fn lex(sql: &str) -> Vec<String> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != '\'' {
                i += 1;
            }
            i += 1;
            out.push(b[start..i.min(b.len())].iter().collect());
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '{' || c == '}' {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric()
                    || b[i] == '_'
                    || b[i] == '$'
                    || b[i] == '{'
                    || b[i] == '}')
            {
                i += 1;
            }
            out.push(b[start..i].iter().collect());
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

fn is_ident(t: &str) -> bool {
    t.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn eq(t: &str, kw: &str) -> bool {
    t.eq_ignore_ascii_case(kw)
}

/// Words that can follow FROM/JOIN/INTO without being a table.
const NOT_A_TABLE: &[&str] = &["select", "lateral", "values", "only"];

/// Every table a statement names, and every column it writes by name.
///
/// CTE names are collected first and subtracted: `WITH m AS (INSERT ...)` puts
/// `m` in a FROM position, and a check that demanded a table called `m` would
/// fire on correct SQL.
fn referenced(sql: &str) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let toks = lex(sql);
    let mut ctes = BTreeSet::new();
    for w in toks.windows(3) {
        if eq(&w[1], "as") && w[2] == "(" && is_ident(&w[0]) {
            ctes.insert(w[0].to_lowercase());
        }
    }

    let mut tables = BTreeSet::new();
    let mut writes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let is_from = eq(t, "from") || eq(t, "join");
        let is_into = eq(t, "into");
        let is_update = eq(t, "update");
        if !(is_from || is_into || is_update) {
            i += 1;
            continue;
        }
        let Some(name) = toks.get(i + 1) else { break };
        if !is_ident(name) || NOT_A_TABLE.iter().any(|k| eq(name, k)) {
            i += 1;
            continue;
        }
        // A function call in a FROM position (`generate_series(...)`) is not a
        // table.
        if toks.get(i + 2).map(String::as_str) == Some("(") && !is_into {
            i += 2;
            continue;
        }
        let lower = name.to_lowercase();
        if !ctes.contains(&lower) {
            tables.insert(lower.clone());
        }
        let mut j = i + 2;

        if is_into && toks.get(j).map(String::as_str) == Some("(") {
            // `INSERT INTO t (a, b, c)` -- the one place a column belongs
            // unambiguously to the table just named.
            let mut depth = 0usize;
            let entry = writes.entry(lower.clone()).or_default();
            while j < toks.len() {
                match toks[j].as_str() {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    tok if depth == 1 && is_ident(tok) => {
                        entry.insert(tok.to_lowercase());
                    }
                    _ => {}
                }
                j += 1;
            }
        } else if is_update {
            // `UPDATE t SET a = ..., b = ...` -- same argument. Depth-aware,
            // because `coalesce(hits, 0)` carries a comma that is not a
            // separator.
            while j < toks.len() && !eq(&toks[j], "set") {
                j += 1;
            }
            j += 1;
            let entry = writes.entry(lower.clone()).or_default();
            let mut depth = 0usize;
            let mut want = true;
            while j < toks.len() {
                match toks[j].as_str() {
                    "(" => depth += 1,
                    ")" => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    "," if depth == 0 => want = true,
                    tok if depth == 0 && eq(tok, "where") => break,
                    tok if depth == 0 && want && is_ident(tok) => {
                        entry.insert(tok.to_lowercase());
                        want = false;
                    }
                    _ => {}
                }
                j += 1;
            }
        }

        // A comma-separated FROM list: `FROM person a, person b`.
        if is_from {
            let mut k = i + 2;
            if toks
                .get(k)
                .is_some_and(|t| is_ident(t) && !eq(t, "on") && !eq(t, "where") && !eq(t, "join"))
            {
                k += 1; // the alias
            }
            if toks.get(k).map(String::as_str) == Some(",") {
                if let Some(next) = toks.get(k + 1) {
                    if is_ident(next) && !ctes.contains(&next.to_lowercase()) {
                        tables.insert(next.to_lowercase());
                    }
                }
            }
        }
        i += 1;
    }
    (tables, writes)
}

/// Every `sql` statement the named dataset can reach, tagged with where it
/// came from so a failure names the catalogue entry and not just the SQL.
fn statements_for(cat: &Catalogue, ds: Dataset) -> Vec<(String, String)> {
    let family = ds.family().name();
    let mut out = Vec::new();

    // Read shapes: the dataset's own set, plus the algorithm set, because a
    // profile may override the dataset's with it. Every `algo-*` shape is
    // declared `unsupported` for SQL and drops out below -- included anyway so
    // that a future one which is not cannot slip past.
    let mut shapes: Vec<&Shape> = ds.shapes().iter().collect();
    shapes.extend(ALGO_SHAPES.iter());
    for s in shapes {
        let e = cat.read_shape(s.name, Dialect::Sql).expect("read shape");
        if e.status.runnable() && !e.text.is_empty() {
            out.push((format!("read_shapes.{}", s.name), e.text));
        }
    }

    for op in cat.write_op_names().expect("write ops") {
        let e = cat.write_op(&op, family, Dialect::Sql).expect("write op");
        if e.status.runnable() && !e.text.is_empty() {
            out.push((format!("write_ops.{op}.{family}|any"), e.text));
        }
    }

    // An integrity probe named `-snb` or `-synthetic` belongs to that corpus;
    // the rest apply to both.
    for probe in [
        "hot-counter-synthetic",
        "hot-counter-snb",
        "uniq-duplicates",
        "uniq-population",
        "rel-endpoints-synthetic",
        "rel-endpoints-snb",
        "churn-survivors-total",
        "churn-survivors-worker",
        "churn-duplicates",
        "churn-anchor-rels",
        "churn-rel-bare",
        "churn-rel-bound",
    ] {
        let mine = match (probe.ends_with("-snb"), probe.ends_with("-synthetic")) {
            (true, _) => ds.family() == Dataset::Snb,
            (_, true) => ds.family() == Dataset::Synthetic,
            _ => true,
        };
        if !mine {
            continue;
        }
        let e = cat.integrity_probe(probe, Dialect::Sql).expect("probe");
        if e.status.runnable() && !e.text.is_empty() {
            out.push((format!("integrity_probes.{probe}"), e.text));
        }
    }

    // Fixtures: the dataset's own group, and the two profile-scoped ones,
    // which run whatever the corpus is.
    for group in [ds.fixture_group(), "unique-create", "delete-churn"] {
        for list in ["indexes", "probes", "setup"] {
            for stmt in cat.fixture(group, Dialect::Sql, list).expect("fixture") {
                out.push((format!("fixtures.{group}.sql.{list}"), stmt));
            }
        }
    }

    // The dataset-level statements. `sql_schema` itself is excluded: it is the
    // declaration being checked against, not a use of it.
    for key in ["sql_seed", "sql_seed_post"] {
        for stmt in cat.dataset_list(family, key).expect("dataset list") {
            out.push((format!("datasets.{family}.{key}"), stmt));
        }
    }
    for key in ["sql_attach", "sql_census"] {
        if let Some(stmt) = cat.dataset_str(family, key).expect("dataset scalar") {
            out.push((format!("datasets.{family}.{key}"), stmt));
        }
    }
    out
}

#[test]
fn every_sql_statement_names_only_tables_its_dataset_declares() {
    let cat = Catalogue::load().expect("catalogue");
    let mut findings = Vec::new();

    for ds in [Dataset::Synthetic, Dataset::Snb] {
        let family = ds.family().name();
        let schema = schema_of(&cat, family);
        assert!(
            !schema.is_empty(),
            "dataset {family} declares no sql_schema, so nothing here is checked -- \
             an absent declaration must not read as a pass"
        );
        for (whence, sql) in statements_for(&cat, ds) {
            let (tables, writes) = referenced(&sql);
            for t in &tables {
                if !schema.contains_key(t) {
                    findings.push(format!(
                        "{family}: {whence} names table `{t}`, which {family}.sql_schema does not create\n    {sql}"
                    ));
                }
            }
            for (t, cols) in &writes {
                let Some(declared) = schema.get(t) else {
                    continue;
                };
                for c in cols {
                    if !declared.contains(c) {
                        findings.push(format!(
                            "{family}: {whence} writes `{t}.{c}`, which {family}.sql_schema does not declare\n    {sql}"
                        ));
                    }
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "the SQL dialect names {} thing(s) its schema does not create:\n{}",
        findings.len(),
        findings.join("\n")
    );
}

#[test]
fn the_check_can_see_a_table_that_is_missing_and_a_column_that_is_not_declared() {
    // A guard nobody has watched fail is not known to be a guard. These are
    // the two defects that motivated the file, reproduced against a schema
    // written here so the check is exercised whether or not the catalogue ever
    // regresses again.
    let schema: BTreeMap<String, BTreeSet<String>> = [(
        "stressw".to_string(),
        ["c".to_string(), "s".to_string()].into_iter().collect(),
    )]
    .into_iter()
    .collect();

    let (tables, writes) = referenced("INSERT INTO stressw (id, c, s) VALUES (1, 2, 3)");
    assert!(tables.contains("stressw"));
    assert!(
        !schema["stressw"].contains("id"),
        "the fixture must reproduce the real defect: stressw had no id column"
    );
    assert_eq!(
        writes["stressw"],
        ["c", "id", "s"].iter().map(|s| (*s).to_string()).collect()
    );

    let (tables, _) = referenced(
        "WITH a AS (SELECT cid, nonce FROM churn_anchor WHERE cid = 1), \
         n AS (INSERT INTO churn (id, cid, nonce) SELECT 1, 1, 1 FROM a RETURNING id, cid, nonce) \
         INSERT INTO churn_rel (acid, anonce, id, nonce) SELECT 1, 1, n.id, n.nonce FROM n",
    );
    assert!(
        tables.contains("churn_anchor") && tables.contains("churn") && tables.contains("churn_rel"),
        "all three churn tables must be seen: {tables:?}"
    );
    assert!(
        !tables.contains("a") && !tables.contains("n"),
        "a CTE name in a FROM position is not a table: {tables:?}"
    );
    for t in ["churn_anchor", "churn", "churn_rel"] {
        assert!(
            !schema.contains_key(t),
            "the fixture must reproduce the real defect: snb.sql_schema declared no {t}"
        );
    }

    // And the UPDATE arm, which is what `contention` rests on.
    let (tables, writes) =
        referenced("UPDATE person SET hits = coalesce(hits, 0) + 1 WHERE id = 0");
    assert!(tables.contains("person"));
    assert_eq!(
        writes["person"],
        ["hits"].iter().map(|s| (*s).to_string()).collect(),
        "the comma inside coalesce(hits, 0) must not read as a second SET target"
    );
}
