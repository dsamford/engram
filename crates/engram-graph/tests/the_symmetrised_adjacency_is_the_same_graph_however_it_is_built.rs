#![allow(non_snake_case)]
//! LCC, CDLP and Louvain pinned to their values, so the symmetrised adjacency
//! can be rebuilt underneath them.
//!
//! `undirected(g, rev)` converts an `AlgoGraph` — which is CSR, three flat
//! arrays — into `Vec<Vec<u32>>`, one heap allocation per vertex. At the SF10
//! SNB corpus (V = 32,653,609, E = 180,619,897) that is 784 MB of Vec headers
//! and ~522 MB of allocator metadata across 32.6 MILLION allocations, against
//! 261 MB of offsets and two allocations for the same data in CSR. `triangles`
//! holds two such structures at once, so LCC pays ~5.5 GB before counting a
//! single triangle.
//!
//! Three kernels call it: `kernels.rs` LCC, CDLP and Louvain. The codebase
//! already knows the pattern — `betweenness` reuses its scratch "at `V` sources
//! this is the difference between `V` allocations and four" — so `undirected`
//! is the outlier rather than the convention.
//!
//! THESE VALUES WERE CAPTURED FROM THE CURRENT IMPLEMENTATION, not derived by
//! hand. That is what makes them useful here: the claim being defended is that
//! a representation change alters nothing, and the only way to hold a refactor
//! to that is to pin what it started from. A value that is WRONG today stays
//! wrong and is caught by the conformance work, not by this file.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

/// A graph with the three shapes the symmetrisation has to get right:
/// a triangle (so LCC is non-zero), a RECIPROCAL pair (which is the edge CDLP
/// dedups and the spec counts twice), and a pendant (degree 1, so the `d < 2`
/// branch is exercised).
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..6 {
        run(&g, &format!("CREATE (:N {{id: {i}}})"));
    }
    // triangle 0-1-2
    for (a, b) in [(0, 1), (1, 2), (2, 0)] {
        run(
            &g,
            &format!("MATCH (a:N {{id: {a}}}), (b:N {{id: {b}}}) CREATE (a)-[:E]->(b)"),
        );
    }
    // reciprocal 3<->4
    for (a, b) in [(3, 4), (4, 3)] {
        run(
            &g,
            &format!("MATCH (a:N {{id: {a}}}), (b:N {{id: {b}}}) CREATE (a)-[:E]->(b)"),
        );
    }
    // pendant 5 hanging off 0
    run(
        &g,
        "MATCH (a:N {id: 0}), (b:N {id: 5}) CREATE (a)-[:E]->(b)",
    );
    g
}

fn floats(r: &QueryResult) -> Vec<String> {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    engram_cypher::Value::Float(f) => format!("{f:.6}"),
                    engram_cypher::Value::Int(i) => i.to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn the_fixture_is_the_shape_the_symmetrisation_must_handle() {
    // Guard the fixture before pinning anything to it: a graph that failed to
    // build would make every assertion below pass over an empty result.
    let g = graph();
    let n = run(&g, "MATCH (x:N) RETURN count(x) AS c");
    let e = run(&g, "MATCH ()-[r:E]->() RETURN count(r) AS c");
    assert_eq!(floats(&n), vec!["6"], "six vertices");
    assert_eq!(
        floats(&e),
        vec!["6"],
        "six directed edges: 3 triangle + 2 reciprocal + 1 pendant"
    );
}

#[test]
fn LCC_is_unchanged_by_how_the_adjacency_is_stored() {
    let g = graph();
    let r = run(
        &g,
        "CALL engram.algo.localclusteringcoefficient.stream({nodeLabels: ['N'], relationshipTypes: ['E']}) \
         YIELD nodeId, coefficient RETURN coefficient ORDER BY coefficient DESC",
    );
    assert_eq!(
        floats(&r),
        vec![
            "1.000000", "1.000000", "0.333333", "0.000000", "0.000000", "0.000000"
        ],
        "LCC changed. Captured before the symmetrised adjacency was rebuilt in \
         CSR; a representation change must not move a coefficient."
    );
}

#[test]
fn CDLP_is_unchanged_by_how_the_adjacency_is_stored() {
    let g = graph();
    let r = run(
        &g,
        "CALL engram.algo.labelpropagation.stream({nodeLabels: ['N'], relationshipTypes: ['E'], \
         maxIterations: 10}) YIELD nodeId, communityId RETURN communityId ORDER BY communityId",
    );
    assert_eq!(
        floats(&r),
        vec!["1", "1", "1", "1", "4", "5"],
        "CDLP changed. The reciprocal pair 3<->4 is the edge whose handling is \
         at issue: it must be symmetrised the SAME way after the rebuild, \
         whatever the spec says it ought to be."
    );
}

#[test]
fn Louvain_is_unchanged_by_how_the_adjacency_is_stored() {
    let g = graph();
    let r = run(
        &g,
        "CALL engram.algo.louvain.stream({nodeLabels: ['N'], relationshipTypes: ['E']}) \
         YIELD nodeId, communityId RETURN communityId ORDER BY communityId",
    );
    assert_eq!(
        floats(&r),
        vec!["1", "1", "1", "1", "4", "4"],
        "Louvain changed. It shares `undirected` with LCC and CDLP, so it is \
         the third thing a rebuild can silently move."
    );
}
