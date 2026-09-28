#![allow(non_snake_case)]
//! A typed relationship match walks that type's adjacency, not every
//! relationship in the store.
//!
//! `Graph::for_each_rel` resolved the type to a token and then scanned the
//! WHOLE relationship partition, applying the type as a per-record test.
//! Records sit in id order, so the cost had nothing to do with the type's
//! selectivity and everything to do with where that type's records happened
//! to land: at SF3, `MATCH (a)-[r:CHURN]->(b) RETURN id(r)` read ~50M records
//! over 22 s to yield 696 edges, and `MATCH (a)-[r:KNOWS]->(b) RETURN id(r)
//! LIMIT 5` cost the same 22 s — the producer's stop fires correctly, but
//! only once the scan REACHES the first record of the type.
//!
//! The out-table is already the index that wants: every relationship has
//! exactly one source, so each edge of a type appears exactly once across the
//! `b'O'` rows, and `SlimAdj` carries the relationship id.
//!
//! EVERY TEST HERE IS A ROW COMPARISON PLUS A CANARY. The rows must be
//! identical on both paths — this is an optimisation, so a difference is a
//! bug — and the counter must show WHICH path ran, because a row assertion
//! alone passes just as happily when the fast path silently never fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const VIA_TABLE: &str = "graph.rel-driven seed walked the adjacency table";
const VIA_SCAN: &str = "graph.rel-driven seed scanned the whole partition";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    v
}

/// A store where `RARE` is a needle: `common` edges dominate the partition
/// and are created FIRST, so a partition scan must cross all of them before
/// reaching the first `RARE` record — the id-order effect the fix removes.
fn needle_in_a_haystack() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["N".into()], &m).expect("node")
    };
    let ns: Vec<u64> = (0..60).map(&mut mk).collect();
    // The haystack, created first so it owns the low relationship ids.
    for w in ns.windows(2) {
        g.create_rel(w[0], "common", w[1], &BTreeMap::new())
            .expect("common");
    }
    // The needles.
    for i in 0..4 {
        g.create_rel(ns[i * 3], "RARE", ns[i * 3 + 1], &BTreeMap::new())
            .expect("rare");
    }
    let _ = g.warm();
    g
}

const RARE: &str = "MATCH (a)-[r:RARE]->(b) RETURN id(a), id(r), id(b)";

#[test]
fn the_adjacency_walk_and_the_partition_scan_return_the_same_edges() {
    let g = needle_in_a_haystack();
    let (via_table, t_on) = engram_observe::with_trace(|| rows(&g, RARE));

    // Budget 0 is `adj_tables_usable() == false`: the same declaration the
    // table makes when the data exceeds a non-zero budget, so this is the
    // fall-back path a large store takes, not a test-only branch.
    g.set_adj_table_max_entries(0);
    let (via_scan, t_off) = engram_observe::with_trace(|| rows(&g, RARE));

    assert_eq!(
        sorted(via_table.clone()),
        sorted(via_scan),
        "the adjacency walk and the partition scan disagree about :RARE"
    );
    assert_eq!(via_table.len(), 4, "four :RARE edges were created");

    // THE CANARY. Without these the row comparison above passes even if the
    // fast path never ran and both arms scanned.
    assert_eq!(counter(&t_on, VIA_TABLE), 1, "the table walk did not run");
    assert_eq!(counter(&t_on, VIA_SCAN), 0, "it scanned anyway");
    assert_eq!(
        counter(&t_off, VIA_TABLE),
        0,
        "budget 0 still walked a table"
    );
    assert_eq!(counter(&t_off, VIA_SCAN), 1, "budget 0 did not scan");
}

#[test]
fn a_self_loop_is_yielded_exactly_once() {
    // A self-loop is the shape that double-counts if an implementation
    // collects a type from BOTH the out- and in-tables. It has one source, so
    // the out-table holds it once.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(0));
    let n = g.create_node(&["N".into()], &m).expect("node");
    g.create_rel(n, "LOOP", n, &BTreeMap::new()).expect("loop");
    let _ = g.warm();

    let q = "MATCH (a)-[r:LOOP]->(b) RETURN id(r)";
    let (via_table, t) = engram_observe::with_trace(|| rows(&g, q));
    g.set_adj_table_max_entries(0);
    let via_scan = rows(&g, q);

    assert_eq!(
        via_table.len(),
        1,
        "a self-loop yielded {} rows",
        via_table.len()
    );
    assert_eq!(
        sorted(via_table),
        sorted(via_scan),
        "the two paths disagree"
    );
    assert_eq!(counter(&t, VIA_TABLE), 1, "the table walk did not run");
}

#[test]
fn an_edge_deleted_after_the_walk_began_is_not_a_phantom_row() {
    // The table names relationship ids; `rel` resolves each through the
    // ordinary read path. A relationship the table still names but the store
    // no longer holds must read `None` and be SKIPPED, not yielded as a row
    // pointing at nothing.
    let g = needle_in_a_haystack();
    let before = rows(&g, RARE);
    assert_eq!(before.len(), 4);

    let ids = rows(&g, "MATCH ()-[r:RARE]->() RETURN id(r)");
    let Value::Int(victim) = ids[0][0].clone() else {
        panic!("id(r) is an Int")
    };
    let q = format!("MATCH ()-[r:RARE]->() WHERE id(r) = {victim} DELETE r");
    let stmt = parse_statement(&q).expect("parse delete");
    run_query(&g, &stmt, BTreeMap::new()).expect("delete");

    let after = rows(&g, RARE);
    assert_eq!(after.len(), 3, "the deleted edge is still being yielded");

    g.set_adj_table_max_entries(0);
    assert_eq!(
        sorted(after),
        sorted(rows(&g, RARE)),
        "after a delete the two paths disagree"
    );
}

#[test]
fn a_type_no_relationship_carries_yields_nothing_on_both_paths() {
    // An unminted type has no table, which is a DECLINE, not an empty answer
    // — the two must not be confused, so the scan path has to agree.
    let g = needle_in_a_haystack();
    let q = "MATCH (a)-[r:NEVER_MINTED]->(b) RETURN id(r)";
    let via_table = rows(&g, q);
    g.set_adj_table_max_entries(0);
    let via_scan = rows(&g, q);
    assert!(via_table.is_empty(), "an unminted type yielded rows");
    assert_eq!(
        via_table, via_scan,
        "the two paths disagree on an absent type"
    );
}

#[test]
fn a_write_in_the_same_statement_is_visible_to_a_later_typed_match() {
    // A transaction with buffered adjacency rows must fall THROUGH to the
    // span scan, which overlays the pending writes. If the table walk took
    // this it would answer from committed state and miss the new edge.
    let g = needle_in_a_haystack();
    let q = "MATCH (a:N {id: 0}), (b:N {id: 5}) \
             CREATE (a)-[:RARE]->(b) \
             WITH 1 AS _x \
             MATCH ()-[r:RARE]->() RETURN count(r)";
    let stmt = parse_statement(q).expect("parse");
    let out = run_query(&g, &stmt, BTreeMap::new()).expect("run");
    assert_eq!(
        out.rows,
        vec![vec![Value::Int(5)]],
        "the edge created earlier in the statement was not seen"
    );
}

#[test]
fn a_typed_match_never_builds_a_table_the_admission_gate_refused() {
    // THE REGRESSION THIS FILE'S FIRST CUT CAUSED. The seed reached for
    // `adj_table`, which admits a BUILD, so a typed scan published a
    // whole-store table on a query thread — past a gate the deployment had
    // shut — and `adjacency_probe_slim`'s pre-admission test then found a
    // table where it had asserted none could exist.
    //
    // Preferring a table is not a reason to create one: with the gate shut
    // the seed must fall through to the span scan and still answer exactly.
    let g = needle_in_a_haystack();
    g.set_degree_table_after(u64::MAX); // never admit a table build

    let (out, t) = engram_observe::with_trace(|| rows(&g, RARE));
    assert_eq!(out.len(), 4, "the gated path lost rows");
    assert_eq!(
        counter(&t, "graph.adjacency tables built"),
        0,
        "the typed seed built a table past the admission gate"
    );
}
