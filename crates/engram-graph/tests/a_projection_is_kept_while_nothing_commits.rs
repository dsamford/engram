#![allow(non_snake_case)]
//! An algorithm projection is KEPT between statements while nothing commits,
//! and a weighted one reads its weights in one gather.
//!
//! `algo_graph` is a pure function of committed state, memoised within one
//! statement only, so every statement rebuilt it. On LDBC Graphalytics SSSP
//! over datagen-7_5-fb (34,185,747 edges) each call read 163-190 s, the
//! weights one full relationship record per edge, against BFS's 4.7 s on the
//! same graph, and a warm-up call before the timed repetitions could not take
//! the build out of them. A projection built at commit clock `t` now serves
//! every statement that runs while the clock reads `t`, the rule the
//! relationship memo keeps its values by. The weights come from one sorted
//! gather of the property, and each edge takes its OWN weight: parallel edges
//! between one pair used to share whichever weight was read last.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn traced(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, trace) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, trace.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn count(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const BUILT: &str = "algo.graph built";
const KEPT: &str = "algo.graph reused across statements: nothing committed since its build";
const GATHER: &str = "algo.weights read in one gather";
const REVERSE: &str = "algo.reverse built";
const FULL_RELS: &str = "graph.rels materialised in full";

/// A chain of forty `:P`, each linked to the next by a `:W {w}`, and a second,
/// PARALLEL edge from 0 to 1: the first created weighs 1.0, the second 5.0.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 39) AS i CREATE (:P {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 38) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) \
         CREATE (a)-[:W {w: 1.0 + i}]->(b)",
    );
    ddl(&g, "MATCH (a:P {id: 0}), (b:P {id: 1}) CREATE (a)-[:W {w: 5.0}]->(b)");
    g
}

const SSSP: &str = "MATCH (s:P {id: 0}) CALL engram.algo.sssp.stream({nodeLabels: ['P'], \
    relationshipTypes: ['W'], orientation: 'NATURAL', relationshipWeightProperty: 'w', \
    sourceNode: id(s)}) YIELD node, distance RETURN node.id AS id, distance ORDER BY id";

const WCC: &str = "CALL engram.algo.wcc.stats({nodeLabels: ['P'], relationshipTypes: ['W'], \
    orientation: 'NATURAL'}) YIELD nodeCount RETURN nodeCount";

/// The distance to node `k` along the chain: 1 + 2 + ... + k (the edge from
/// `i` weighs `1 + i`), with the parallel 5.0 edge never the shorter.
fn chain_distance(k: i64) -> f64 {
    (1..=k).map(|i| i as f64).sum()
}

#[test]
fn a_a_projection_is_kept_for_the_next_statement() {
    let g = graph();
    let (first, c1) = traced(&g, SSSP);
    assert_eq!(count(&c1, BUILT), 1, "{c1:?}");
    assert_eq!(count(&c1, KEPT), 0, "{c1:?}");
    let (second, c2) = traced(&g, SSSP);
    assert_eq!(second, first, "the kept projection answered differently");
    assert_eq!(count(&c2, BUILT), 0, "the second statement rebuilt it: {c2:?}");
    assert_eq!(count(&c2, KEPT), 1, "{c2:?}");
}

#[test]
fn b_a_commit_makes_the_next_statement_rebuild() {
    let g = graph();
    let (before, _) = traced(&g, SSSP);
    // The last edge gets heavier: node 39's distance moves.
    ddl(&g, "MATCH (:P {id: 38})-[r:W]->(:P {id: 39}) SET r.w = 100.0");
    let (after, c) = traced(&g, SSSP);
    assert_eq!(count(&c, BUILT), 1, "a commit must invalidate the kept projection: {c:?}");
    assert_eq!(count(&c, KEPT), 0, "{c:?}");
    assert_eq!(before[39][1], Value::Float(chain_distance(39)));
    assert_eq!(after[39][1], Value::Float(chain_distance(38) + 100.0));
}

#[test]
fn c_parallel_edges_keep_their_own_weights() {
    let g = graph();
    let (rows, c) = traced(&g, SSSP);
    assert_eq!(rows.len(), 40);
    // 0 -> 1 has two edges, 1.0 and 5.0; the shortest route takes the 1.0.
    assert_eq!(rows[1], vec![Value::Int(1), Value::Float(1.0)], "{rows:?}");
    for (k, row) in rows.iter().enumerate() {
        assert_eq!(row[1], Value::Float(chain_distance(k as i64)), "node {k}: {rows:?}");
    }
    assert!(count(&c, GATHER) >= 40, "the weights were not gathered: {c:?}");
    assert_eq!(count(&c, FULL_RELS), 0, "a relationship was decoded in full: {c:?}");
}

#[test]
fn d_the_reverse_is_built_once_and_only_for_the_kernels_that_read_it() {
    let g = graph();
    let (_, c) = traced(&g, SSSP);
    assert_eq!(count(&c, REVERSE), 0, "SSSP never reads the reverse: {c:?}");
    let (first, c1) = traced(&g, WCC);
    assert_eq!(count(&c1, REVERSE), 1, "{c1:?}");
    let (second, c2) = traced(&g, WCC);
    assert_eq!(second, first);
    assert_eq!(count(&c2, BUILT), 0, "{c2:?}");
    assert_eq!(count(&c2, REVERSE), 0, "the kept projection's reverse was rebuilt: {c2:?}");
}

/// Two graphs built by the same statements sit at the same commit clock, so
/// the keep is keyed by the graph as well: the second graph, whose last edge
/// is heavier, builds its own projection and answers from its own weights.
#[test]
fn f_another_graph_at_the_same_clock_builds_its_own() {
    let a = graph();
    let b = graph();
    ddl(&b, "MATCH (:P {id: 38})-[r:W]->(:P {id: 39}) SET r.w = 100.0");
    ddl(&a, "MATCH (:P {id: 38})-[r:W]->(:P {id: 39}) SET r.w = 39.0");
    let (ra, ca) = traced(&a, SSSP);
    assert_eq!(count(&ca, BUILT), 1, "{ca:?}");
    let (rb, cb) = traced(&b, SSSP);
    assert_eq!(count(&cb, BUILT), 1, "graph b was served graph a's projection: {cb:?}");
    assert_eq!(count(&cb, KEPT), 0, "{cb:?}");
    assert_eq!(ra[39][1], Value::Float(chain_distance(39)));
    assert_eq!(rb[39][1], Value::Float(chain_distance(38) + 100.0));
}

#[test]
fn e_a_transaction_with_buffered_writes_neither_reads_nor_keeps_one() {
    let g = graph();
    let _ = traced(&g, SSSP);
    g.begin_txn().expect("begin");
    let w = parse_statement("MATCH (a:P) WHERE a.id < 3 SET a.touched = 1 RETURN count(*) AS n")
        .expect("parses");
    run_query(&g, &w, BTreeMap::new()).expect("the write runs");
    let (_, c) = traced(&g, SSSP);
    g.commit_txn().expect("commit");
    assert_eq!(count(&c, KEPT), 0, "a transaction with buffered writes read a kept projection: {c:?}");
    assert_eq!(count(&c, BUILT), 1, "{c:?}");
}
