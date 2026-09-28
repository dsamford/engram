//! `engram.algo.project` builds an algorithm's graph from ROWS — the weights
//! a query computed — without writing a relationship.
//!
//! SNB BI bi15 weights each KNOWS pair by a computed interaction score. With
//! only stored weights to read, it CREATEd a weighted relationship per pair
//! inside its timed statement, and at SF10 that is a join inside a writing
//! statement: the shape that reached 109 GB for a single slice of bi20's
//! precomputation. LDBC's bi15 writes nothing — GDS builds the graph in
//! memory — and this is that, living only as long as its statement.
//!
//! THE LOAD-BEARING TEST IS DIFFERENTIAL: the same weights, materialised as a
//! relationship type and read through `relationshipWeightProperty`, must give
//! the same answers as the same weights projected from rows.

use std::collections::BTreeMap;
use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> Result<Vec<Vec<Value>>, String> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

/// 60 people in a ring with chords, KNOWS in one direction; `v` gives each pair
/// a computed weight. Person 59 knows nobody: an isolated vertex.
fn people() -> Graph {
    people_sized(60)
}

/// `n` people: a ring over the first `n - 1`, chords from every fifth, and the
/// last one isolated. One row per edge in every setup statement — never a
/// cartesian in a setup write.
fn people_sized(n: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ring = n - 1;
    ddl(&g, &format!("UNWIND range(0, {}) AS i CREATE (:P {{v: i}})", n - 1));
    ddl(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:P {{v: i}}), (b:P {{v: (i + 1) % {ring}}}) \
             CREATE (a)-[:K]->(b)",
            ring - 2
        ),
    );
    ddl(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i WITH i WHERE i % 5 = 0 \
             MATCH (a:P {{v: i}}), (b:P {{v: (i * 7 + 3) % {ring}}}) CREATE (a)-[:K]->(b)",
            ring - 2
        ),
    );
    let _ = g.warm();
    g
}

/// The weight both paths compute: a function of the pair, not a constant.
const WEIGHT: &str = "1.0 / (1.0 + ((a.v * 13 + b.v * 7) % 11))";

fn from_rows(src: i64, dst: i64, k: u32) -> String {
    format!(
        "MATCH (a:P)-[:K]->(b:P) \
         WITH collect({{source: id(a), target: id(b), weight: {WEIGHT}}}) AS edges \
         CALL engram.algo.project({{name: 'g', nodeLabels: ['P'], edges: edges, \
           orientation: 'UNDIRECTED'}}) YIELD projection \
         MATCH (s:P {{v: {src}}}), (t:P {{v: {dst}}}) \
         CALL engram.algo.kshortestpaths.stream({{projection: projection, \
           sourceNode: id(s), targetNode: id(t), k: {k}}}) \
         YIELD index, totalCost, nodeIds \
         RETURN index, totalCost, size(nodeIds) AS hops ORDER BY index"
    )
}

fn from_store(src: i64, dst: i64, k: u32) -> String {
    format!(
        "MATCH (s:P {{v: {src}}}), (t:P {{v: {dst}}}) \
         CALL engram.algo.kshortestpaths.stream({{nodeLabels: ['P'], relationshipTypes: ['W'], \
           relationshipWeightProperty: 'w', orientation: 'UNDIRECTED', \
           sourceNode: id(s), targetNode: id(t), k: {k}}}) \
         YIELD index, totalCost, nodeIds \
         RETURN index, totalCost, size(nodeIds) AS hops ORDER BY index"
    )
}

#[test]
fn rows_and_a_stored_relationship_type_give_the_same_routes() {
    let g = people();
    ddl(&g, &format!("MATCH (a:P)-[:K]->(b:P) CREATE (a)-[:W {{w: {WEIGHT}}}]->(b)"));
    let mut compared = 0;
    for (src, dst) in [(0, 30), (5, 44), (12, 13), (1, 57), (40, 2)] {
        for k in [1, 3] {
            let rows = run(&g, &from_rows(src, dst, k)).expect("from rows");
            let stored = run(&g, &from_store(src, dst, k)).expect("from the store");
            assert_eq!(rows, stored, "{src} -> {dst}, k = {k}");
            assert!(!rows.is_empty(), "{src} -> {dst} found no route; this compares nothing");
            compared += 1;
        }
    }
    assert_eq!(compared, 10);
}

/// Vertices come from `nodeLabels`, not from edge endpoints: person 59 has no
/// edge, and a route to it is NONE — not "that node is not in the projection".
/// bi15 turns that into its required `-1.0`.
#[test]
fn an_isolated_vertex_is_in_the_projection_and_simply_unreachable() {
    let g = people();
    let rows = run(&g, &from_rows(0, 59, 1)).expect("an isolated target is not an error");
    assert!(rows.is_empty(), "a route to an isolated person: {rows:?}");
}

/// The arrays are a pure function of the edge SET: the same edges arriving in
/// a different order answer identically, ties and all.
///
/// The ORDER is permuted directly — forward, reversed, and scrambled by a
/// prime stride — rather than hoped for from a parallel executor: the first
/// version of this test ran the rows under a 4-wide executor, and its own
/// guard found they were never driven in parallel, so it had compared a serial
/// run with a serial run. `WEIGHT` takes 11 values, so equal-cost routes tie
/// and k = 3 exercises how the ties break.
#[test]
fn the_order_rows_arrive_in_changes_nothing() {
    let g = people();
    let ordered = |src: i64, dst: i64, order: &str| {
        format!(
            "MATCH (a:P)-[:K]->(b:P) \
             WITH collect({{source: id(a), target: id(b), weight: {WEIGHT}}}) AS edges \
             WITH {order} AS edges \
             CALL engram.algo.project({{name: 'g', nodeLabels: ['P'], edges: edges, \
               orientation: 'UNDIRECTED'}}) YIELD projection \
             MATCH (s:P {{v: {src}}}), (t:P {{v: {dst}}}) \
             CALL engram.algo.kshortestpaths.stream({{projection: projection, \
               sourceNode: id(s), targetNode: id(t), k: 3}}) \
             YIELD index, totalCost, nodeIds \
             RETURN index, totalCost, nodeIds ORDER BY index"
        )
    };
    for (src, dst) in [(0, 30), (5, 44), (1, 57)] {
        let forward = run(&g, &ordered(src, dst, "edges")).expect("forward");
        assert!(!forward.is_empty(), "{src} -> {dst} found no route; this compares nothing");
        for order in [
            "reverse(edges)",
            "[i IN range(0, size(edges) - 1) | edges[(i * 7919) % size(edges)]]",
        ] {
            assert_eq!(
                run(&g, &ordered(src, dst, order)).expect(order),
                forward,
                "{src} -> {dst}: the routes changed when the edges arrived as {order}"
            );
        }
    }
}

/// A weight the query did not compute is refused BY NAME, never defaulted.
#[test]
fn a_missing_or_non_numeric_weight_is_refused() {
    let g = people();
    for bad in ["null", "'heavy'"] {
        let q = format!(
            "MATCH (a:P)-[:K]->(b:P) \
             WITH collect({{source: id(a), target: id(b), weight: {bad}}}) AS edges \
             CALL engram.algo.project({{name: 'g', nodeLabels: ['P'], edges: edges}}) \
             YIELD projection RETURN projection"
        );
        let err = run(&g, &q).expect_err("a bad weight must be refused");
        assert!(err.contains("weight"), "the refusal must name the weight: {err}");
    }
}

/// STATEMENT-SCOPED: the handle means nothing to the next statement.
#[test]
fn a_projection_does_not_outlive_its_statement() {
    let g = people();
    let rows = run(
        &g,
        "MATCH (a:P)-[:K]->(b:P) \
         WITH collect({source: id(a), target: id(b), weight: 1.0}) AS edges \
         CALL engram.algo.project({name: 'g', nodeLabels: ['P'], edges: edges}) \
         YIELD projection RETURN projection",
    )
    .expect("build");
    let Some(Value::Str(handle)) = rows.first().and_then(|r| r.first()).cloned() else {
        panic!("no handle: {rows:?}");
    };
    let err = run(
        &g,
        &format!(
            "MATCH (s:P {{v: 0}}), (t:P {{v: 30}}) \
             CALL engram.algo.kshortestpaths.stream({{projection: '{handle}', \
               sourceNode: id(s), targetNode: id(t), k: 1}}) \
             YIELD totalCost RETURN totalCost"
        ),
    )
    .expect_err("a projection from a finished statement must be gone");
    assert!(err.contains("no projection"), "{err}");
}

/// `projection` already fixed the vertices, edges, weights and orientation;
/// naming any of them beside it is refused, not silently resolved.
#[test]
fn a_projection_cannot_be_combined_with_a_stored_selection() {
    let g = people();
    for extra in [
        "nodeLabels: ['P']",
        "relationshipTypes: ['K']",
        "relationshipWeightProperty: 'w'",
        "orientation: 'UNDIRECTED'",
    ] {
        let q = format!(
            "MATCH (a:P)-[:K]->(b:P) \
             WITH collect({{source: id(a), target: id(b), weight: 1.0}}) AS edges \
             CALL engram.algo.project({{name: 'g', nodeLabels: ['P'], edges: edges}}) \
             YIELD projection \
             MATCH (s:P {{v: 0}}), (t:P {{v: 30}}) \
             CALL engram.algo.kshortestpaths.stream({{projection: projection, {extra}, \
               sourceNode: id(s), targetNode: id(t), k: 1}}) \
             YIELD totalCost RETURN totalCost"
        );
        let err = run(&g, &q).expect_err(&format!("`{extra}` beside `projection`"));
        assert!(err.contains("cannot be combined"), "{extra}: {err}");
    }
}

/// Priced before it is built, and the refusal names its lever.
#[test]
fn a_projection_over_the_byte_budget_is_refused_by_name() {
    let g = people();
    g.set_algo_byte_ceiling(64);
    let err = run(
        &g,
        "MATCH (a:P)-[:K]->(b:P) \
         WITH collect({source: id(a), target: id(b), weight: 1.0}) AS edges \
         CALL engram.algo.project({name: 'g', nodeLabels: ['P'], edges: edges}) \
         YIELD projection RETURN projection",
    )
    .expect_err("over budget");
    assert!(err.contains("ENGRAM_ALGO_BYTE_CEILING"), "{err}");
}
