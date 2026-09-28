//! An `engram.algo.*` call after a MATCH is configured by EACH row.
//!
//! The call evaluated its configuration map against the first input row and
//! replayed that one result for every row. With a constant map that is only
//! an optimisation; with `sourceNode: id(p)` it answered every source with the
//! first source's distances. SNB BI19 at SF3 returned no rows: its first
//! source was an isolated person, so every source "reached" only itself.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

/// k0 is isolated; k1..k5 form a weighted chain.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "UNWIND range(0, 5) AS i CREATE (:V {k: i})");
    run(
        &g,
        "UNWIND range(1, 4) AS i MATCH (a:V {k: i}), (b:V {k: i + 1}) CREATE (a)-[:E {w: toFloat(i)}]->(b)",
    );
    g
}

const PER_ROW: &str = "MATCH (s:V) WHERE s.k IN __KS__ WITH s ORDER BY __ORDER__ \
    CALL engram.algo.sssp.stream({nodeLabels: ['V'], relationshipTypes: ['E'], \
      orientation: 'UNDIRECTED', relationshipWeightProperty: 'w', sourceNode: id(s)}) \
    YIELD nodeId, distance \
    WITH s, count(distance) AS reached, sum(distance) AS total \
    RETURN s.k, reached, total ORDER BY s.k";

fn per_row(g: &Graph, ks: &str, order: &str) -> Vec<Vec<Value>> {
    run(
        g,
        &PER_ROW.replace("__KS__", ks).replace("__ORDER__", order),
    )
}

#[test]
fn each_source_gets_its_own_distances_whatever_comes_first() {
    let g = graph();
    let want = vec![
        vec![Value::Int(0), Value::Int(1), Value::Float(0.0)],
        vec![Value::Int(1), Value::Int(5), Value::Float(20.0)],
        vec![Value::Int(3), Value::Int(5), Value::Float(15.0)],
    ];
    // the isolated source first — the order that broke BI19 — and last
    assert_eq!(per_row(&g, "[0, 1, 3]", "s.k ASC"), want);
    assert_eq!(per_row(&g, "[0, 1, 3]", "s.k DESC"), want);
}

#[test]
fn each_row_matches_the_call_run_on_its_own() {
    let g = graph();
    let batched = per_row(&g, "[0, 1, 2, 3, 4, 5]", "s.k DESC");
    for row in &batched {
        let Value::Int(k) = row[0] else {
            panic!("{row:?}")
        };
        let alone = per_row(&g, &format!("[{k}]"), "s.k");
        assert_eq!(alone, vec![row.clone()], "source {k}");
    }
}

#[test]
fn a_constant_configuration_still_runs_once() {
    let g = graph();
    let (rows, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "UNWIND range(1, 4) AS i \
             CALL engram.algo.wcc.stream({nodeLabels: ['V'], relationshipTypes: ['E']}) \
             YIELD nodeId, componentId RETURN count(*)",
        )
    });
    assert_eq!(rows, vec![vec![Value::Int(24)]], "6 nodes x 4 rows");
    assert_eq!(
        t.counters()
            .get("interp.algorithm run shared by rows with the same configuration")
            .copied()
            .unwrap_or(0),
        3,
        "the first row runs it, the other three reuse it: {:?}",
        t.counters()
    );
}
