//! BM25 full-text, end to end — and the compatibility it must not break.
//!
//! The scoring change is the visible half: a term appearing in every document
//! should stop discriminating, and a short document should beat a long one
//! carrying the query term as often. The FIRST test pairs each of those with
//! its negative — the same corpus under the old term-frequency scorer, which
//! provably does not rank them that way. Without that pairing, "BM25 works"
//! would be a claim about one ranking rather than a demonstration that the
//! ranking changed for the stated reason.
//!
//! The compatibility half is the one that could break a deployment. An index
//! created before scoring was recorded keeps term frequency FOR EVER, and the
//! `(node, score)` YIELD shape never moves, because production call sites bind
//! it.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn doc(g: &Graph, name: &str, title: &str) {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    m.insert("title".to_string(), Value::Str(title.into()));
    g.create_node(&["Doc".into()], &m).expect("doc");
}

const QUERY: &str = "CALL db.index.fulltext.queryNodes('docs', $q) YIELD node, score \
                     RETURN node.name, score";

fn search(g: &Graph, q: &str) -> Vec<String> {
    let stmt = parse_statement(QUERY).expect("parses");
    let mut params = BTreeMap::new();
    params.insert("q".to_string(), Value::Str(q.into()));
    run_query(g, &stmt, params)
        .expect("search")
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.clone(),
            other => panic!("expected a name, got {other:?}"),
        })
        .collect()
}

/// A corpus where "database" is everywhere and "quasar" is in one document.
///
/// The common documents carry the common term THREE times against the rare
/// document's two query terms once each, so raw term frequency scores them
/// strictly higher — which is what makes the negative test a real comparison
/// rather than a tie broken by iteration order.
fn corpus(g: &Graph) {
    doc(g, "rare", "quasar database");
    for i in 0..20 {
        doc(g, &format!("common{i}"), "database database database");
    }
}

// ─── The scoring change, each half paired with its negative ────────────────

#[test]
fn a_bm25_scored_index_ranks_a_rare_term_above_a_common_one() {
    let g = g();
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );

    let ranked = search(&g, "quasar database");
    assert_eq!(
        ranked.first().map(String::as_str),
        Some("rare"),
        "the document with the rare term must come first: {ranked:?}",
    );
}

#[test]
fn the_old_term_frequency_scorer_provably_does_not() {
    // THE NEGATIVE THAT MAKES THE ABOVE MEAN SOMETHING. Under raw term
    // frequency the "common" documents each contain the query term TWICE and
    // the rare document contains its two terms once each — so they tie or beat
    // it, and the rare document is not first. Same corpus, same statement, the
    // scoring stamped `tf` instead.
    let g = g();
    g.set_bm25_by_default(false);
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );

    let ranked = search(&g, "quasar database");
    assert_ne!(
        ranked.first().map(String::as_str),
        Some("rare"),
        "term frequency must NOT rank the rare document first, or this test \
         is not measuring the scoring change: {ranked:?}",
    );
}

#[test]
fn a_short_document_outranks_a_long_one_carrying_the_term_as_often() {
    let g = g();
    doc(&g, "short", "graph");
    doc(
        &g,
        "long",
        "graph and a great many other unrelated words padding the title out",
    );
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    assert_eq!(
        search(&g, "graph"),
        vec!["short".to_string(), "long".into()],
    );
}

// ─── The index and the scan must agree ─────────────────────────────────────

/// Run the same search with the index on and off, and require agreement.
fn differential(g: &Graph, q: &str) -> Vec<String> {
    g.set_bm25_scoring(true);
    let with = search(g, q);
    g.set_bm25_scoring(false);
    let without = search(g, q);
    g.set_bm25_scoring(true);
    assert_eq!(
        with, without,
        "the term index and the scan disagreed for {q:?}",
    );
    with
}

#[test]
fn the_index_and_the_fallback_scan_return_the_same_ranking() {
    // Both arms build the same query plan and call the same summation, so this
    // compares the ORDER exactly rather than approximately.
    let g = g();
    corpus(&g);
    doc(&g, "mixed", "quasar and graph and database");
    doc(&g, "empty", "");
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    for q in [
        "quasar",
        "database",
        "quasar database",
        "graph",
        "nothing matches this",
        "quasar quasar",
        "",
    ] {
        differential(&g, q);
    }
}

#[test]
fn the_index_survives_the_writes_that_follow_it() {
    let g = g();
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    assert_eq!(differential(&g, "quasar").len(), 1);

    doc(&g, "late", "quasar arriving late");
    run(
        &g,
        "MATCH (d:Doc {name: 'rare'}) SET d.title = 'nothing here now'",
    );

    let found = differential(&g, "quasar");
    assert!(found.contains(&"late".to_string()), "{found:?}");
    assert!(
        !found.contains(&"rare".to_string()),
        "a rewritten document must not be found by its old text: {found:?}",
    );
}

#[test]
fn a_node_that_gains_the_label_joins_the_corpus() {
    // A fulltext index spans labels x properties, so its population moves when
    // MEMBERSHIP moves — and a document joining changes every other document's
    // idf, so getting this wrong makes the whole ranking wrong rather than
    // merely dropping a row.
    let g = g();
    corpus(&g);
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str("draft".into()));
    m.insert("title".to_string(), Value::Str("quasar in a draft".into()));
    g.create_node(&["Draft".into()], &m).expect("draft");

    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    assert_eq!(differential(&g, "quasar"), vec!["rare".to_string()]);

    run(&g, "MATCH (d:Draft {name: 'draft'}) SET d:Doc");
    let found = differential(&g, "quasar");
    assert!(
        found.contains(&"draft".to_string()),
        "a node that gained the label must join the corpus: {found:?}",
    );
}

// ─── Compatibility ─────────────────────────────────────────────────────────

#[test]
fn an_index_created_before_the_scoring_field_still_scores_by_term_frequency() {
    // THE COMPATIBILITY GUARANTEE. A catalogue row written before `scoring`
    // existed has no such key, and an absent key means term frequency — so a
    // deployment that upgrades does not silently re-rank.
    //
    // Simulated by stamping `tf` explicitly, which produces the same row an
    // older build wrote: the field is only written when it is BM25.
    let g = g();
    g.set_bm25_by_default(false);
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );

    // Turning the BM25 lever on must not change a Tf index's answer: its
    // scoring lives in its row, not in a switch.
    g.set_bm25_scoring(true);
    let on = search(&g, "quasar database");
    g.set_bm25_scoring(false);
    let off = search(&g, "quasar database");
    assert_eq!(on, off, "a Tf index must ignore the BM25 lever entirely");
    assert_ne!(on.first().map(String::as_str), Some("rare"));
}

#[test]
fn the_yield_signature_is_still_exactly_node_and_score() {
    // Bound by production call sites. If this ever needs changing, the change
    // is a break for every one of them.
    let g = g();
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    let q = parse_statement(
        "CALL db.index.fulltext.queryNodes('docs', 'quasar') YIELD node, score \
         RETURN node, score",
    )
    .expect("parses");
    let r = run_query(&g, &q, BTreeMap::new()).expect("runs");
    assert_eq!(r.columns, vec!["node".to_string(), "score".into()]);
    assert!(matches!(r.rows[0][0], Value::Node { .. }));
    assert!(matches!(r.rows[0][1], Value::Float(_)));
}

#[test]
fn show_indexes_still_reports_a_fulltext_index_as_fulltext() {
    let g = g();
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    let rows = run_stmt(
        &g,
        &parse_any("SHOW INDEXES").expect("parses"),
        BTreeMap::new(),
    )
    .expect("show")
    .rows;
    assert!(
        rows.iter()
            .any(|r| matches!(&r[1], Value::Str(t) if t == "FULLTEXT")),
        "{rows:?}",
    );
}

#[test]
fn a_query_matching_nothing_answers_nothing_rather_than_everything() {
    let g = g();
    corpus(&g);
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    assert!(differential(&g, "supercalifragilistic").is_empty());
    assert!(differential(&g, "   ---   ").is_empty());
}
