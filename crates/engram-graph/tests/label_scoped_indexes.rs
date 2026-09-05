#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A DECLARED index covers its LABEL'S members, not the whole partition.
//!
//! `CREATE INDEX ... FOR (n:Churn) ON (n.id)` names a label and Cypher means
//! it. The built index ignored the label and covered every node in the
//! partition carrying `id` — on official SF1 that is 3.18M entries for an index
//! the operator scoped to a few hundred nodes, paid at every build, at every
//! fold, and in memory for the life of the process.
//!
//! This is what makes the multi-key seek CHEAP rather than merely correct: the
//! seek probes the declared index, so an index that is three orders of
//! magnitude larger than the label would trade a label scan for a partition
//! scan.
//!
//! The bar is the usual one — the scoped and unscoped arms must answer
//! identically for any pattern that requires the label.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// A corpus where the indexed property is SHARED: 60 `:Churn` nodes carry `id`,
/// and so do 600 `:Bulk` nodes. This is SF1's shape in miniature — `id` is a
/// property most of the corpus has, and the declared index names one label.
fn shared_property_corpus(g: &Graph) {
    ddl(g, "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)");
    for i in 0..60i64 {
        run(g, &format!("CREATE (:Churn {{id: {i}, nonce: {}}})", i % 7));
    }
    for i in 1000..1600i64 {
        run(g, &format!("CREATE (:Bulk {{id: {i}}})"));
    }
}

/// THE point: the declared index holds the LABEL, not the partition.
#[test]
fn a_declared_index_holds_only_its_labels_members() {
    let g = graph();
    g.set_label_scoped_indexes(true);
    shared_property_corpus(&g);

    let scoped = g
        .ensure_range_index_for_test("id", Some("Churn"))
        .expect("scoped index");
    let unscoped = g
        .ensure_range_index_for_test("id", None)
        .expect("unscoped index");

    eprintln!(
        "[label-scoped index] :Churn(id) holds {}, partition-wide holds {}",
        scoped.len(),
        unscoped.len()
    );
    assert_eq!(
        scoped.len(),
        60,
        "the scoped index must hold exactly the label's members"
    );
    assert_eq!(
        unscoped.len(),
        660,
        "and the partition-wide one still holds every node carrying `id` — \
         which is what the scoped index exists to avoid"
    );
    assert!(
        scoped.len() * 10 < unscoped.len(),
        "the whole point is that this is an order of magnitude, not a trim"
    );
}

/// Both arms must answer identically for a pattern that REQUIRES the label.
#[test]
fn scoped_and_unscoped_answer_identically_for_the_label() {
    let mut arms = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_label_scoped_indexes(on);
        shared_property_corpus(&g);
        let mut answers = Vec::new();
        for q in [
            "MATCH (n:Churn {id: 17, nonce: 3}) RETURN n.id",
            "MATCH (n:Churn {id: 17, nonce: 999}) RETURN n.id",
            "MATCH (n:Churn {id: 9999}) RETURN n.id",
            // A pattern on the OTHER label sharing the property: the scoped
            // index must not be consulted for it, and the answer must not move.
            "MATCH (n:Bulk {id: 1200}) RETURN n.id",
            // No label at all — no declared index can apply.
            "MATCH (n {id: 5}) RETURN n.id",
        ] {
            answers.push(rows(&g, q));
        }
        arms.push(answers);
    }
    assert_eq!(
        arms[0], arms[1],
        "scoping may change what the index COSTS, never what it ANSWERS"
    );
}

/// A write to the property still invalidates the scoped index — staleness is
/// judged per PROPERTY, which is conservative and correct.
#[test]
fn a_write_to_the_property_still_invalidates_a_scoped_index() {
    let g = graph();
    g.set_label_scoped_indexes(true);
    shared_property_corpus(&g);
    assert_eq!(rows(&g, "MATCH (n:Churn {id: 5}) RETURN n.id").len(), 1);

    run(&g, "CREATE (:Churn {id: 5000, nonce: 1})");
    assert_eq!(
        rows(&g, "MATCH (n:Churn {id: 5000}) RETURN n.id").len(),
        1,
        "a node created after the index was built must be findable through it"
    );
    run(&g, "MATCH (n:Churn {id: 5000}) DELETE n");
    assert_eq!(
        rows(&g, "MATCH (n:Churn {id: 5000}) RETURN n.id").len(),
        0,
        "and a deleted one must stop being findable"
    );
}

/// A label the index does not cover must not be answered from it. The scoped
/// index is a SUBSET of the property's rows, and a candidate set must always be
/// a superset of the answer.
#[test]
fn a_scoped_index_is_never_used_for_another_label() {
    let g = graph();
    g.set_label_scoped_indexes(true);
    shared_property_corpus(&g);
    // `:Bulk` shares the property but has no declared index. Its rows must
    // still be found — through the partition-wide index or a scan, never
    // through `:Churn`'s.
    assert_eq!(
        rows(&g, "MATCH (n:Bulk {id: 1200}) RETURN n.id").len(),
        1,
        "a label with no declared index must still answer correctly"
    );
    assert_eq!(
        rows(&g, "MATCH (n:Bulk {id: 17}) RETURN n.id").len(),
        0,
        "and must not inherit :Churn's rows — id 17 is a :Churn, not a :Bulk"
    );
}
