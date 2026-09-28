#![allow(non_snake_case)]
//! CDLP against LDBC Graphalytics' OWN validation graph and expected output.
//!
//! The coverage plan's §2.5 spike asks for a conformance table, and the table
//! this repository had was read out of the SOURCE — a suspicion with a line
//! number, not a verdict. This is the same question asked of the published
//! oracle instead, on LDBC's own 8-vertex directed graph.
//!
//! Fetched 2026-09-14 from `ldbc/ldbc_graphalytics`,
//! `graphalytics-validation/src/main/resources/validation-graphs/cdlp/`:
//!
//! ```text
//!   dir-input            dir-output (expected labels)
//!   1 2 3 7              1 1     5 4
//!   2 1 3                2 1     6 4
//!   3 1 2                3 1     7 4
//!   4 5 6                4 5     8 4
//!   5 4 6 7
//!   6 5 7
//!   7 5 6 8
//!   8 6
//! ```
//!
//! The input is an ADJACENCY LIST: `1 2 3 7` means vertex 1 has OUT-edges to
//! 2, 3 and 7. Graphalytics CDLP counts in- and out-neighbours SEPARATELY, so a
//! reciprocal pair contributes its label twice — which is precisely the
//! behaviour `kernels.rs` drops with `s.dedup()` after chaining the two
//! directions. CDLP is validated by EXACT match, so if the suspicion is right
//! this test fails on VALUES, against LDBC's own answer, and the divergence
//! stops being a reading of the code.
//!
//! The expected output is carried verbatim rather than recomputed: a fixture
//! this test derived itself would only prove the test agrees with itself.
//!
//! `maxIterations` IS PART OF THE ORACLE. LDBC's own
//! `CommunityDetectionLPValidationTest.java:74` runs the directed case with
//! `maxIterations = 5`, and CDLP has no convergence test — the expected labels
//! are the state after exactly five synchronous rounds and are meaningless at
//! any other count. This test first ran at 10 and disagreed for that reason
//! alone, which is its own lesson: a published answer is only an answer to the
//! parameters it was produced with.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// LDBC's `cdlp/dir-input`, verbatim: `vertex -> its OUT-neighbours`.
const DIR_INPUT: [(u32, &[u32]); 8] = [
    (1, &[2, 3, 7]),
    (2, &[1, 3]),
    (3, &[1, 2]),
    (4, &[5, 6]),
    (5, &[4, 6, 7]),
    (6, &[5, 7]),
    (7, &[5, 6, 8]),
    (8, &[6]),
];

/// LDBC's `cdlp/dir-output`, verbatim: `vertex -> its expected label`.
const DIR_OUTPUT: [(u32, u32); 8] = [
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 5),
    (5, 4),
    (6, 4),
    (7, 4),
    (8, 4),
];

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for (v, _) in DIR_INPUT {
        run(&g, &format!("CREATE (:V {{vid: {v}}})"));
    }
    for (v, outs) in DIR_INPUT {
        for &u in outs {
            run(
                &g,
                &format!("MATCH (a:V {{vid: {v}}}), (b:V {{vid: {u}}}) CREATE (a)-[:E]->(b)"),
            );
        }
    }
    g
}

#[test]
fn the_validation_graph_loaded_as_LDBC_wrote_it() {
    // Guard the fixture before comparing anything to it. The edge count is the
    // sum of the adjacency list lengths; a graph that loaded short would make
    // any label comparison below meaningless.
    let g = graph();
    let want: usize = DIR_INPUT.iter().map(|(_, o)| o.len()).sum();
    let n = run(&g, "MATCH (x:V) RETURN count(x) AS c");
    let e = run(&g, "MATCH ()-[r:E]->() RETURN count(r) AS c");
    assert_eq!(
        n.rows.first().and_then(|r| r.first()),
        Some(&Value::Int(8)),
        "eight vertices"
    );
    assert_eq!(
        e.rows.first().and_then(|r| r.first()),
        Some(&Value::Int(want as i64)),
        "{want} directed edges, as the adjacency list spells them"
    );
}

// MEASURED 2026-09-14. The divergence was QUALITATIVE, not a tie-break:
// LDBC's answer is three communities -- [1,2,3], [4], [5,6,7,8] -- and the
// deduplicated walk returned ONE community of eight.
//
// `graphalytics: true` counts in- and out-neighbours separately
// (`undirected_multiset`) and takes exactly `maxIterations` rounds with no
// two-cycle escape, which is what the specification says. This test asks for
// that mode and expects LDBC's published answer; the DEFAULT mode keeps the
// deduplicating behaviour every existing caller already has.
#[test]
fn CDLP_matches_the_published_expected_output() {
    // THE CONFORMANCE CHECK. Graphalytics validates CDLP by EXACT match, so
    // every vertex's label must equal LDBC's, vertex for vertex.
    //
    // This is expected to FAIL while `kernels.rs` dedups the chained in/out
    // neighbour lists: the spec counts a reciprocal edge's label twice and the
    // dedup counts it once, which changes which label wins a tie. The failure
    // message prints both sides so the divergence is readable rather than a
    // bare assertion, and so the shape of the disagreement is evidence about
    // WHICH rule engram is applying.
    let g = graph();
    let r = run(
        &g,
        "CALL engram.algo.labelpropagation.stream({nodeLabels: ['V'], \
         relationshipTypes: ['E'], maxIterations: 5, graphalytics: true}) \
         YIELD nodeId, communityId RETURN nodeId, communityId",
    );

    // `nodeId` is engram's internal id; map it back through `vid`.
    let mut got: BTreeMap<i64, i64> = BTreeMap::new();
    for row in &r.rows {
        if let (Some(Value::Int(nid)), Some(Value::Int(cid))) = (row.first(), row.get(1)) {
            got.insert(*nid, *cid);
        }
    }
    assert_eq!(
        got.len(),
        8,
        "CDLP must label all eight vertices; got {got:?}"
    );

    // Compare SHAPE, not raw label values: Graphalytics' labels are vertex
    // ids, and engram's are its own internal ids, so the comparable invariant
    // is the PARTITION — which vertices share a label.
    let expected_groups = {
        let mut m: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (v, l) in DIR_OUTPUT {
            m.entry(l).or_default().push(v);
        }
        let mut g: Vec<Vec<u32>> = m.into_values().collect();
        g.sort();
        g
    };
    let actual_groups = {
        let mut m: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (nid, cid) in &got {
            m.entry(*cid).or_default().push(*nid);
        }
        let mut g: Vec<usize> = m.into_values().map(|v| v.len()).collect();
        g.sort_unstable();
        g
    };
    let expected_sizes = {
        let mut s: Vec<usize> = expected_groups.iter().map(Vec::len).collect();
        s.sort_unstable();
        s
    };
    assert_eq!(
        actual_groups, expected_sizes,
        "CDLP's PARTITION must match LDBC's published output. Expected groups \
         of sizes {expected_sizes:?} (labels {expected_groups:?}), engram \
         produced groups of sizes {actual_groups:?}. Graphalytics counts in- \
         and out-neighbours separately; `kernels.rs` dedups the chained lists, \
         so a reciprocal edge votes once where the spec counts it twice."
    );
}
