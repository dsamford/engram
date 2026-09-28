//! Three dialects, one question — checked, not asserted.
//!
//! # The failure this catches
//!
//! The LSQB battery used to exist three times: Cypher in `src/bin/lsqb.rs`,
//! SQL in a PostgreSQL runner script, Kuzu's Cypher in an embedded Kuzu runner. The
//! only thing holding those three to the same question was that their COUNTS
//! agreed — and a count only catches a divergence that changes the answer. A
//! divergence that changes only the WORK — a join written the expensive way
//! round, an anti-join quietly dropped from one arm — leaves the count
//! identical and the timing wrong, which is worse, because nothing fails and
//! the number reaches a table.
//!
//! # What is checked
//!
//! Each query declares a `skeleton`: the multiset of relationship-type
//! traversals, and the counts of optional legs, anti-joins and inequality
//! predicates. This test derives the same four things from EVERY dialect's
//! text and requires all of them to agree with the declaration and with each
//! other. The SQL side reads its joined table names through the catalogue's
//! `table_alias` map, because the relational schema materialises the Message
//! supertype and a symmetric `KNOWS`, so a table name is not a relationship
//! type.
//!
//! The multiset includes anti-join legs. q8 walks `HAS_TAG` three times —
//! twice in the pattern and once inside the `NOT` — and the SQL joins
//! `HAS_TAG_Comment_Tag` three times for the same reason. Counting only the
//! joined legs would let a dialect drop an anti-join and still match.
//!
//! # What is NOT checked, said plainly
//!
//! Join DIRECTION and the column equalities. A SQL body that joined
//! `plc.src = cpc.src` where the Cypher walks `dst` has the same tables in the
//! same multiplicity and would pass here; only the count would catch it, which
//! is where the catalogue's `expected` table comes in. Label vocabulary is not
//! checked either — LadybugDB writes `:Post:Comment` where engram writes
//! `:Message`, which is a declared adaptation rather than a divergence, and a
//! label check would have to encode that exception and would then no longer be
//! checking anything.
//!
//! This is a structural gate, not a proof of equivalence. It is worth having
//! because the thing it does catch — a missing join, a missing anti-join, a
//! dropped `OPTIONAL`, a lost inequality — is exactly the class that a count
//! comparison cannot see when the counts still agree.

use std::collections::BTreeMap;

use engram_bench::catalogue::{Catalogue, Dialect, Skeleton};

/// Count each `[:TYPE]` traversal in a Cypher statement.
///
/// A type may be written as an alternation (`[:A|B]`) in a dialect that stores
/// one table per pair; each alternation counts as ONE traversal of its first
/// member, which is what it is.
fn cypher_types(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some(at) = rest.find("[:") {
        let tail = &rest[at + 2..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '|'))
            .unwrap_or(tail.len());
        let name = tail[..end].split('|').next().unwrap_or("").to_string();
        if !name.is_empty() {
            *out.entry(name).or_default() += 1;
        }
        rest = &tail[end..];
    }
    out
}

/// Count each joined table in a SQL body, mapped back to a relationship type.
fn sql_types(body: &str, alias: &BTreeMap<String, String>) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for tok in body.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if tok.is_empty() {
            continue;
        }
        let name = match alias.get(tok) {
            Some(t) => t.clone(),
            // A pair table: `TYPE_Src_Dst`, where the type may itself carry
            // underscores. Recognised by the two trailing capitalised segments.
            None => {
                let parts: Vec<&str> = tok.split('_').collect();
                if parts.len() < 3
                    || !tok
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_alphabetic())
                {
                    continue;
                }
                let ty = parts[..parts.len() - 2].join("_");
                if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                    continue;
                }
                ty
            }
        };
        *out.entry(name).or_default() += 1;
    }
    out
}

fn multiset(types: &[String]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for t in types {
        *m.entry(t.clone()).or_default() += 1;
    }
    m
}

fn count_of(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// Anti-joins, in whichever way a dialect spells one.
fn anti(text: &str) -> usize {
    // `NOT EXISTS` covers Kuzu's Cypher and SQL; `NOT (` covers openCypher's
    // pattern predicate. A statement never uses both spellings.
    let exists = count_of(text, "NOT EXISTS");
    if exists > 0 {
        exists
    } else {
        count_of(text, "NOT (")
    }
}

fn derived(text: &str, optional_marker: &str) -> (usize, usize, usize) {
    (
        count_of(text, optional_marker),
        anti(text),
        count_of(text, "<>"),
    )
}

#[test]
fn every_lsqb_dialect_walks_the_same_relationships_the_same_number_of_times() {
    let cat = Catalogue::load().expect("catalogue");
    let alias = cat.table_alias().expect("table_alias");
    let names = cat.lsqb_names().expect("lsqb.queries");
    assert_eq!(names.len(), 9, "LSQB defines nine queries");
    let mut checked = 0usize;
    for name in &names {
        let sk: Skeleton = cat.lsqb_skeleton(name).expect("skeleton");
        let declared = multiset(&sk.types);

        for dialect in [Dialect::Cypher, Dialect::CypherLadybug] {
            let e = cat.lsqb(name, dialect).expect("entry");
            assert_eq!(
                cypher_types(&e.text),
                declared,
                "{name}/{}: relationship traversals disagree with the declared skeleton",
                dialect.key()
            );
            let (opt, an, neq) = derived(&e.text, "OPTIONAL MATCH");
            assert_eq!(opt, sk.optional, "{name}/{}: optional legs", dialect.key());
            assert_eq!(an, sk.anti, "{name}/{}: anti-joins", dialect.key());
            assert_eq!(
                neq,
                sk.inequalities,
                "{name}/{}: inequality predicates",
                dialect.key()
            );
            checked += 1;
        }

        let sql = cat.lsqb(name, Dialect::Sql).expect("sql entry");
        assert_eq!(
            sql_types(&sql.text, &alias),
            declared,
            "{name}/sql: joined tables disagree with the declared skeleton"
        );
        let (opt, an, neq) = derived(&sql.text, "LEFT JOIN");
        assert_eq!(opt, sk.optional, "{name}/sql: LEFT JOINs");
        assert_eq!(an, sk.anti, "{name}/sql: anti-joins");
        assert_eq!(neq, sk.inequalities, "{name}/sql: inequality predicates");
        checked += 1;
    }
    // A loop that checked nothing passes every assertion it never made.
    assert_eq!(checked, 27, "nine queries in three dialects");
}

#[test]
fn the_extractors_would_notice_a_dropped_join() {
    // THE CANARY. A structural check that passed everything would be worse
    // than none, so each extractor is broken deliberately and required to
    // disagree.
    let full = "MATCH (a)-[:KNOWS]-(b)-[:KNOWS]-(c)-[:HAS_INTEREST]->(t) \
                WHERE NOT (a)-[:KNOWS]-(c) AND a <> c RETURN count(*) AS count";
    let dropped = "MATCH (a)-[:KNOWS]-(b)-[:HAS_INTEREST]->(t) \
                   WHERE NOT (a)-[:KNOWS]-(b) AND a <> b RETURN count(*) AS count";
    assert_ne!(cypher_types(full), cypher_types(dropped));
    assert_eq!(cypher_types(full).get("KNOWS"), Some(&3));
    assert_eq!(cypher_types(full).get("HAS_INTEREST"), Some(&1));

    let alias: BTreeMap<String, String> = [("knows_sym".to_string(), "KNOWS".to_string())]
        .into_iter()
        .collect();
    let sql = "FROM knows_sym k1 JOIN knows_sym k2 ON k2.src = k1.dst \
               JOIN HAS_INTEREST_Person_Tag pit ON pit.src = k2.dst \
               WHERE k1.src <> k2.dst AND NOT EXISTS (SELECT 1 FROM knows_sym x \
               WHERE x.src = k1.src AND x.dst = k2.dst)";
    assert_eq!(sql_types(sql, &alias).get("KNOWS"), Some(&3));
    assert_eq!(sql_types(sql, &alias).get("HAS_INTEREST"), Some(&1));
    let no_anti = "FROM knows_sym k1 JOIN knows_sym k2 ON k2.src = k1.dst \
                   JOIN HAS_INTEREST_Person_Tag pit ON pit.src = k2.dst \
                   WHERE k1.src <> k2.dst";
    assert_ne!(sql_types(sql, &alias), sql_types(no_anti, &alias));
    assert_eq!(anti(sql), 1);
    assert_eq!(anti(no_anti), 0);

    // An optional leg dropped from one arm.
    assert_eq!(
        count_of("A OPTIONAL MATCH B OPTIONAL MATCH C", "OPTIONAL MATCH"),
        2
    );
    assert_eq!(count_of("A LEFT JOIN B LEFT JOIN C", "LEFT JOIN"), 2);
}

#[test]
fn every_lsqb_dialect_ends_in_a_count_and_derives_its_own_probe() {
    // The probe derivation is what makes a zero provable, and it depends on
    // the count suffix. A dialect entry that broke the contract would silently
    // stop proving zeros.
    let cat = Catalogue::load().expect("catalogue");
    for name in cat.lsqb_names().expect("names") {
        for d in [Dialect::Cypher, Dialect::CypherLadybug] {
            let e = cat.lsqb(&name, d).expect("entry");
            assert!(
                e.text.ends_with("RETURN count(*) AS count"),
                "{name}/{}: must end in the count suffix",
                d.key()
            );
        }
        let sql = cat.lsqb(&name, Dialect::Sql).expect("sql");
        assert!(
            sql.text.starts_with("FROM "),
            "{name}/sql: the entry is a FROM body, wrapped by sql_shape"
        );
        let count = cat.sql_count(&sql.text).expect("count shape");
        let probe = cat.sql_probe(&sql.text).expect("probe shape");
        assert!(count.starts_with("SELECT count(*) FROM "));
        assert!(probe.starts_with("SELECT 1 FROM ") && probe.ends_with(" LIMIT 1"));
        // The probe keeps the WHERE clauses: an anti-join query's zero is only
        // provable with the NOT applied.
        assert!(probe.contains(sql.text.as_str()));
    }
}
