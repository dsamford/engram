//! Boot warming builds the COUNT STORE, so the first query does not.
//!
//! A graph opened over data it did not write defers its count store to the
//! first read, and every planned statement reads it. Measured on SNB BI at
//! SF3: `MATCH (t:TagClass) RETURN count(t)` — 71 nodes — took 61 s as the
//! first statement after a restart, while `RETURN 1` took 0 s and the same
//! count took 0 s the second time. The whole first-query tax on that server
//! was three store-wide key walks, on the client's thread.
//!
//! This is the fourth instance of the mistake `Graph::warm` records: a
//! structure every query uses, built lazily, on a path warming did not cover.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, counters, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn seeded() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "UNWIND range(0, 199) AS i CREATE (:Person {id: i})");
    run(&g, "UNWIND range(0, 9) AS i CREATE (:TagClass {id: i})");
    run(
        &g,
        "UNWIND range(0, 98) AS i MATCH (a:Person {id: i}), (b:Person {id: i + 1}) \
         CREATE (a)-[:KNOWS]->(b)",
    );
    g
}

/// The shape a restarted server is in: the same data, opened by a process
/// that did not write it, so its counts are deferred.
fn reopened(g: &Graph) -> Graph {
    let recovered = Store::recover(&g.shared_store().log_tail(0)).expect("recover");
    Graph::new(recovered, Realm(1), Namespace(1))
}

fn rebuilds(t: &engram_observe::Trace) -> u64 {
    t.counters()
        .get("graph.stats rebuilt")
        .copied()
        .unwrap_or(0)
}

#[test]
fn a_warmed_graph_answers_its_first_query_without_rebuilding_counts() {
    let g = reopened(&seeded());
    let (_, warm) = engram_observe::with_trace(|| g.warm());
    assert_eq!(rebuilds(&warm), 1, "warming built the count store once");

    let (rows, first) =
        engram_observe::with_trace(|| run(&g, "MATCH (t:TagClass) RETURN count(t) AS n"));
    assert_eq!(rows, vec![vec![Value::Int(10)]]);
    assert_eq!(
        rebuilds(&first),
        0,
        "the first query after warming must not rebuild counts: {:?}",
        first.counters()
    );
}

#[test]
fn an_unwarmed_graph_still_rebuilds_on_first_read() {
    // THE CONTROL. Without it the test above would pass on a build where the
    // count store was never deferred at all, and say nothing about warming.
    let g = reopened(&seeded());
    let (_, first) =
        engram_observe::with_trace(|| run(&g, "MATCH (t:TagClass) RETURN count(t) AS n"));
    assert_eq!(rebuilds(&first), 1, "a reopened graph defers its counts");
}

#[test]
fn warmed_counts_equal_maintained_counts() {
    // A warmed store must be the object the writer maintained — a fast wrong
    // count would mislead every plan that estimates from it.
    let written = seeded();
    let g = reopened(&written);
    g.warm();
    for q in [
        "MATCH (n) RETURN count(n) AS n",
        "MATCH (p:Person) RETURN count(p) AS n",
        "MATCH (t:TagClass) RETURN count(t) AS n",
        "MATCH ()-[r:KNOWS]->() RETURN count(r) AS n",
        "MATCH ()-[r]->() RETURN count(r) AS n",
    ] {
        assert_eq!(run(&g, q), run(&written, q), "{q}");
    }
    assert_eq!(g.count_label_nodes("Person"), 200);
    assert_eq!(g.count_all_rels(), 99);
}

#[test]
fn warming_a_graph_that_maintains_its_counts_rebuilds_nothing() {
    // A store this process wrote keeps its counts from the first write;
    // warming it must not pay for a walk it does not need.
    let g = seeded();
    let (_, warm) = engram_observe::with_trace(|| g.warm());
    assert_eq!(rebuilds(&warm), 0, "{:?}", warm.counters());
}

#[test]
fn the_warm_is_counted_where_it_happens() {
    // ENGAGEMENT. The boot line reads this counter; a pass that silently
    // built nothing must be distinguishable from one that built the store.
    // Other tests in this binary may warm too, so only an increase is proof.
    let g = reopened(&seeded());
    let before = counters::WARM_STATS.load(Relaxed);
    g.warm();
    assert!(counters::WARM_STATS.load(Relaxed) > before);
}
