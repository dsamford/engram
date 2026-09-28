//! The trigram index, end to end — and the one property it must never break.
//!
//! **THE INDEX MUST RETURN EXACTLY WHAT THE SCAN RETURNS.** It answers a
//! CANDIDATE set that the clause's `WHERE` then re-verifies, so extra
//! candidates cost time and are invisible in the result. A candidate the index
//! FAILS to return is a row the query silently loses, and nothing anywhere
//! looks wrong.
//!
//! Every test here is therefore a differential: run the statement with the
//! index on, run it again with the lever off so the label scan answers, and
//! require the two to agree. That is the shape that would catch a narrowing
//! bug in the query analysis, in the extraction, in the incremental catch-up,
//! or in the planner's choice — anywhere in the path — rather than only in the
//! piece a unit test happened to look at.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn file(g: &Graph, path: &str, content: &str) {
    let mut m = BTreeMap::new();
    m.insert("path".to_string(), Value::Str(path.into()));
    m.insert("content".to_string(), Value::Str(content.into()));
    g.create_node(&["File".into()], &m).expect("file");
}

/// A corpus deliberately shaped like source code: punctuation, identifiers,
/// mixed case, multibyte, and values too short to hold a trigram of their own.
fn corpus(g: &Graph) {
    file(g, "a.rs", "fn parse_expr(input: &str) -> Result<Expr>");
    file(g, "b.rs", "fn parse_stmt(input: &str) -> Result<Stmt>");
    file(g, "c.rs", "pub struct Parser { at: usize }");
    file(g, "d.rs", "impl Parser { fn peek(&self) -> Option<char> }");
    file(g, "e.rs", "// a comment about ::foo:: and ->bar");
    file(g, "f.rs", "let x = a->b->c;");
    file(g, "g.rs", "CONST_VALUE = 42");
    file(g, "h.rs", "");
    file(g, "i.rs", "x");
    file(g, "j.rs", "ab");
    file(g, "k.rs", "héllo wörld");
    file(g, "l.rs", "MiXeD CaSe HeRe");
    file(g, "m.rs", "trailing whitespace   ");
    file(g, "n.rs", "\u{2}sentinel in the data\u{3}");
    file(g, "o.rs", "fn parse_expr duplicate");
    // ENOUGH ROWS FOR THE SEEK TO BE ADMITTED AT ALL. `PROPERTY_SEEK_MIN_LABEL`
    // is 512: below it the planner declines every seek and a differential
    // compares a label scan with itself, passing whatever the index does.
    // An audit found that every differential in this file was vacuous.
    for i in 0..600 {
        file(
            g,
            &format!("filler{i}.rs"),
            &format!("padding line {i} with nothing special"),
        );
    }
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn declare(g: &Graph) {
    ddl(
        g,
        "CREATE TRIGRAM INDEX file_content FOR (f:File) ON (f.content)",
    );
}

/// Run `src` with the index enabled and with it disabled, and require the two
/// to agree. Returns the rows, so a test can also assert what they are.
fn differential(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_trigram_indexes(true);
    let with = run(g, src);
    g.set_trigram_indexes(false);
    let without = run(g, src);
    g.set_trigram_indexes(true);
    assert_eq!(
        with, without,
        "the trigram index answered differently from the scan for `{src}`",
    );
    with
}

// ─── The differential, over every predicate the index serves ───────────────

#[test]
fn a_trigram_seeded_regex_returns_the_same_rows_as_a_scan() {
    let g = g();
    corpus(&g);
    declare(&g);
    for src in [
        "MATCH (f:File) WHERE f.content =~ '.*parse_expr.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ 'fn parse_.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '.*Parser.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '.*->.*->.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '.*::foo::.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '.*nothing here.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ 'x' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '.*wörld.*' RETURN f.path ORDER BY f.path",
        "MATCH (f:File) WHERE f.content =~ '(?i).*mixed.*' RETURN f.path ORDER BY f.path",
    ] {
        differential(&g, src);
    }
}

#[test]
fn a_contains_filter_returns_the_same_rows_as_a_scan() {
    // CONTAINS had NO index path at all before this — the biggest single win
    // in the feature, and the one most likely to be leaned on.
    let g = g();
    corpus(&g);
    declare(&g);
    for needle in [
        "parse_", "Parser", "->", "::foo::", "nothing", "x", "ab", "wörld", "CaSe", "   ",
    ] {
        differential(
            &g,
            &format!(
                "MATCH (f:File) WHERE f.content CONTAINS '{needle}' RETURN f.path ORDER BY f.path"
            ),
        );
    }
}

#[test]
fn an_ends_with_filter_returns_the_same_rows_as_a_scan() {
    // A suffix is not a contiguous span of any sort order, so a range index
    // cannot answer this at all. The end sentinels are what make it indexable.
    let g = g();
    corpus(&g);
    declare(&g);
    for suffix in ["<Expr>", "usize }", "42", "x", "e", "nope"] {
        differential(
            &g,
            &format!(
                "MATCH (f:File) WHERE f.content ENDS WITH '{suffix}' RETURN f.path ORDER BY f.path"
            ),
        );
    }
}

#[test]
fn a_starts_with_filter_returns_the_same_rows_as_a_scan() {
    let g = g();
    corpus(&g);
    declare(&g);
    for prefix in ["fn parse_", "pub", "f", "CONST", "nope"] {
        differential(
            &g,
            &format!(
                "MATCH (f:File) WHERE f.content STARTS WITH '{prefix}' RETURN f.path ORDER BY f.path"
            ),
        );
    }
}

#[test]
fn the_index_actually_answers_rather_than_quietly_declining() {
    // The differentials above would ALL pass if the index were never
    // consulted, which is the way a test like this rots. This one asserts the
    // index was built and used.
    let g = g();
    corpus(&g);
    // A LARGE corpus, deliberately. Over fifteen nodes the planner is right to
    // prefer a scan, and it declines the seek — so a small corpus would make
    // this test assert the opposite of the intended behaviour. The index earns
    // its place only when the label is big enough for the candidate set to be
    // meaningfully smaller, which is exactly the comparison
    // `property_seek_worth_probing` makes.
    for i in 0..2_000 {
        file(
            &g,
            &format!("bulk{i}.rs"),
            &format!("unrelated filler line {i}"),
        );
    }
    declare(&g);
    let (_, counters) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path",
        )
    });
    let c = counters.counters();
    let probed = c
        .get("interp.seed probed a trigram index")
        .copied()
        .unwrap_or(0)
        + c.get("interp.columnar seek probed a declared trigram index")
            .copied()
            .unwrap_or(0);
    assert!(
        probed > 0,
        "the trigram index must actually have been probed; counters: {c:?}",
    );
}

#[test]
fn an_undeclared_property_is_not_indexed_and_still_answers() {
    // An index nobody declared is not built on the strength of one query.
    let g = g();
    corpus(&g);
    declare(&g); // over `content`, not `path`
    let rows = differential(
        &g,
        "MATCH (f:File) WHERE f.path CONTAINS 'a.' RETURN f.path ORDER BY f.path",
    );
    assert_eq!(rows, vec![vec![Value::Str("a.rs".into())]]);
}

// ─── Writes ────────────────────────────────────────────────────────────────

#[test]
fn a_trigram_index_survives_the_writes_that_follow_it() {
    let g = g();
    corpus(&g);
    declare(&g);
    // Warm the index.
    differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );

    // Add, change and delete, then ask again.
    file(&g, "z.rs", "fn parse_zzz() {}");
    run(
        &g,
        "MATCH (f:File {path: 'a.rs'}) SET f.content = 'now something else entirely'",
    );
    run(&g, "MATCH (f:File {path: 'b.rs'}) DELETE f");

    let rows = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );
    let paths: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.as_str(),
            other => panic!("expected a path, got {other:?}"),
        })
        .collect();
    assert!(
        paths.contains(&"z.rs"),
        "the new row must be found: {paths:?}"
    );
    assert!(
        !paths.contains(&"a.rs"),
        "the rewritten row must NOT be found by its old content: {paths:?}",
    );
    assert!(
        !paths.contains(&"b.rs"),
        "the deleted row must not be found: {paths:?}",
    );
}

#[test]
fn a_trigram_index_survives_an_unrelated_write() {
    // The cache-currency shape: a write to a DIFFERENT property must not
    // invalidate this index, and must not make it answer wrongly either.
    let g = g();
    corpus(&g);
    declare(&g);
    let before = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );
    run(&g, "MATCH (f:File {path: 'c.rs'}) SET f.unrelated = 1");
    let after = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );
    assert_eq!(before, after);
}

#[test]
fn a_non_string_value_under_the_indexed_property_does_not_lose_rows() {
    // The index disables itself rather than answering over the rows it could
    // read — a candidate set that skipped a row is a wrong answer, not a
    // caveated one. What must survive is the ANSWER, via the scan.
    let g = g();
    corpus(&g);
    declare(&g);
    let mut m = BTreeMap::new();
    m.insert("path".to_string(), Value::Str("num.rs".into()));
    m.insert("content".to_string(), Value::Int(42));
    g.create_node(&["File".into()], &m)
        .expect("numeric content");

    differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );
}

#[test]
fn a_write_inside_a_transaction_is_visible_to_a_trigram_probe() {
    // A transaction's buffered rows are not in the store, so the index cannot
    // have seen them. Their ids join the candidate set unfiltered, which is
    // what keeps the answer a superset inside a transaction.
    let g = g();
    corpus(&g);
    declare(&g);
    run(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path",
    );

    let rows = run(
        &g,
        "CREATE (n:File {path: 'txn.rs', content: 'fn parse_in_a_txn()'}) \
         WITH n MATCH (f:File) WHERE f.content CONTAINS 'parse_' RETURN f.path ORDER BY f.path",
    );
    let paths: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.as_str(),
            other => panic!("expected a path, got {other:?}"),
        })
        .collect();
    assert!(
        paths.contains(&"txn.rs"),
        "a row written in this statement must be visible to the probe: {paths:?}",
    );
}

// ─── Catalogue ─────────────────────────────────────────────────────────────

#[test]
fn a_declared_trigram_index_is_listed_by_show_indexes() {
    let g = g();
    declare(&g);
    let rows = run_stmt(
        &g,
        &parse_any("SHOW INDEXES").expect("parses"),
        BTreeMap::new(),
    )
    .expect("show")
    .rows;
    let found = rows.iter().any(|r| {
        matches!(&r[0], Value::Str(n) if n == "file_content")
            && matches!(&r[1], Value::Str(t) if t == "TRIGRAM")
    });
    assert!(
        found,
        "SHOW INDEXES must report the trigram index: {rows:?}"
    );
}

#[test]
fn declaring_the_same_trigram_index_twice_is_refused_unless_if_not_exists() {
    let g = g();
    declare(&g);
    let dup =
        parse_any("CREATE TRIGRAM INDEX file_content FOR (f:File) ON (f.content)").expect("parses");
    assert!(
        run_stmt(&g, &dup, BTreeMap::new()).is_err(),
        "a duplicate index name must be refused",
    );
    ddl(
        &g,
        "CREATE TRIGRAM INDEX file_content IF NOT EXISTS FOR (f:File) ON (f.content)",
    );
}

#[test]
fn an_unselective_predicate_declines_the_seek_rather_than_losing_to_the_scan() {
    // THE PERFORMANCE GUARANTEE, AS A COUNTER RATHER THAN A STOPWATCH. A
    // benchmark on a contended machine cannot tell "the seek lost" from "the
    // machine was busy"; this can. A predicate matching most of the label must
    // DECLINE — the candidate set is nearly the whole label, and reading a
    // record per candidate is strictly worse than the column scan it replaced.
    let g = g();
    for i in 0..2_000 {
        file(&g, &format!("f{i}.rs"), &format!("the ordinary line {i}"));
    }
    declare(&g);

    let (_, counters) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (f:File) WHERE f.content CONTAINS 'the' RETURN count(f)",
        )
    });
    let c = counters.counters();
    let probed = c
        .get("interp.seed probed a trigram index")
        .copied()
        .unwrap_or(0)
        + c.get("interp.columnar seek probed a declared trigram index")
            .copied()
            .unwrap_or(0);
    assert_eq!(
        probed, 0,
        "a predicate matching the whole label must not seek; counters: {c:?}",
    );

    // THE NEGATIVE THAT MAKES THAT MEAN SOMETHING: a selective predicate over
    // the SAME corpus and the same index does seek. Without this pair, a
    // trigram index that never worked at all would pass the assertion above.
    file(&g, "rare.rs", "a zqx marker nobody else carries");
    let (_, counters) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (f:File) WHERE f.content CONTAINS 'zqx' RETURN count(f)",
        )
    });
    let c = counters.counters();
    let probed = c
        .get("interp.seed probed a trigram index")
        .copied()
        .unwrap_or(0)
        + c.get("interp.columnar seek probed a declared trigram index")
            .copied()
            .unwrap_or(0);
    assert!(
        probed > 0,
        "a selective predicate over the same index must seek; counters: {c:?}",
    );
}

#[test]
fn a_node_that_gains_the_label_after_the_build_is_still_found() {
    // A CONFIRMED CORRECTNESS BUG, PINNED. The index is LABEL-SCOPED: it is
    // built over the label's members. Its currency was tested against the
    // PROPERTY's epoch alone, so `SET n:File` — which moves the membership and
    // writes no property — left the index judged current and the new node
    // invisible to every text predicate. Silently: the query simply returned
    // one row fewer.
    let g = g();
    for i in 0..2_000 {
        file(&g, &format!("f{i}.rs"), &format!("the ordinary line {i}"));
    }
    // The node exists, with the indexed content, BEFORE the index is built —
    // so that the only thing that moves afterwards is the LABEL. Creating it
    // later would write `content` and advance the property epoch, which is a
    // different path and would mask this one.
    run(
        &g,
        "CREATE (d:Draft {path: 'late.rs', content: 'a zqx marker arriving late'})",
    );
    declare(&g);

    // Warm the index over the label as it stands.
    let before = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'zqx' RETURN f.path ORDER BY f.path",
    );
    assert!(before.is_empty(), "the Draft is not a File yet: {before:?}");

    // The membership moves, and NO property is written.
    run(&g, "MATCH (d:Draft {path: 'late.rs'}) SET d:File");

    let after = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'zqx' RETURN f.path ORDER BY f.path",
    );
    assert_eq!(
        after,
        vec![vec![Value::Str("late.rs".into())]],
        "a node that gained the label must be found by a text predicate",
    );
}

#[test]
fn a_node_that_loses_the_label_is_no_longer_found() {
    // The mirror. A membership change in the other direction must not leave a
    // row visible to a label-scoped index after it has left the label.
    let g = g();
    for i in 0..2_000 {
        file(&g, &format!("f{i}.rs"), &format!("the ordinary line {i}"));
    }
    file(&g, "leaving.rs", "a zqx marker about to leave");
    declare(&g);

    let before = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'zqx' RETURN f.path ORDER BY f.path",
    );
    assert_eq!(before, vec![vec![Value::Str("leaving.rs".into())]]);

    run(&g, "MATCH (f:File {path: 'leaving.rs'}) REMOVE f:File");

    let after = differential(
        &g,
        "MATCH (f:File) WHERE f.content CONTAINS 'zqx' RETURN f.path ORDER BY f.path",
    );
    assert!(
        after.is_empty(),
        "a node that left the label must not be returned: {after:?}",
    );
}
