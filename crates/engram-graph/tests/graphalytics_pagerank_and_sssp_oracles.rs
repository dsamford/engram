#![allow(non_snake_case)]
//! PageRank and SSSP against LDBC Graphalytics' published validation data.
//!
//! The data here is GENERATED from the fetched oracles rather than transcribed:
//! PageRank's graph is 50 vertices and its expected output 50 floats, which is
//! past the size where hand-copying is trustworthy. Fetched 2026-09-14 from
//! `ldbc/ldbc_graphalytics`, `graphalytics-validation/.../validation-graphs/`.
//!
//! PARAMETERS ARE PART OF THE ORACLE, and both are taken from LDBC's own
//! validation tests rather than guessed:
//!
//! * PageRank — `dampingFactor = 0.85f`, `numberOfIterations = 14`
//!   (`PageRankValidationTest.java:73-74`)
//! * SSSP — source vertex 1, weights on the edge property `weight`
//!   (`SingleSourceShortestPathsValidationTest.java:82`)
//!
//! CDLP's expected labels are the state after exactly five rounds and are wrong
//! at any other count; the first version of that test used ten and recorded a
//! divergence that did not exist. The same applies here: PageRank has no
//! convergence test in the specification, so 14 iterations is the answer's
//! definition, not a budget.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn by_vid(g: &Graph, call: &str, field: &str) -> BTreeMap<i64, Value> {
    let r = run(
        g,
        &format!(
            "CALL {call} YIELD nodeId, {field} WITH nodeId AS n, {field} AS val MATCH (v:V) WHERE id(v) = n RETURN v.vid AS vid, val ORDER BY vid"
        ),
    );
    let mut out = BTreeMap::new();
    for row in &r.rows {
        if let Some(Value::Int(vid)) = row.first() {
            out.insert(*vid, row.get(1).cloned().unwrap_or(Value::Null));
        }
    }
    out
}

const PR_IN: [(u32, &[u32]); 50] = [
    (1, &[19, 21, 22, 27, 31, 37, 45, 48]),
    (2, &[3, 20, 39, 46]),
    (3, &[6, 10, 32, 41, 45]),
    (4, &[15]),
    (5, &[15, 16, 18, 28, 47]),
    (6, &[49]),
    (7, &[6, 27, 43, 46]),
    (8, &[5, 21, 29, 30, 32, 43]),
    (9, &[16, 18, 21, 28, 30, 35, 40]),
    (10, &[9, 13, 28, 29, 33]),
    (11, &[3, 39]),
    (12, &[47, 50]),
    (13, &[7, 12, 17, 32, 48]),
    (14, &[4, 20, 21, 35, 38, 40]),
    (15, &[8, 24, 31, 35, 44]),
    (16, &[]),
    (17, &[5, 9, 11, 16, 26, 37]),
    (18, &[1, 12, 28, 30, 44, 45, 47, 50]),
    (19, &[10, 11, 13, 27, 38]),
    (20, &[15, 25]),
    (21, &[22, 27, 31, 32, 40]),
    (22, &[19, 26, 27, 31]),
    (23, &[22, 35, 36, 38, 40, 46, 47]),
    (24, &[9, 13, 15, 34, 36, 50]),
    (25, &[8, 24, 30, 34, 41, 47]),
    (26, &[7, 31, 37, 40, 44, 47]),
    (27, &[31, 33, 43]),
    (28, &[8, 32, 42, 45]),
    (29, &[1, 2, 12, 14, 16, 19, 20, 36]),
    (30, &[9, 24, 34, 44]),
    (31, &[11, 17, 32, 39, 46, 47]),
    (32, &[2, 28, 29, 30, 31]),
    (33, &[7, 8, 9, 10, 32, 34, 37]),
    (34, &[26, 48]),
    (35, &[3, 10, 17, 24, 26, 28, 33, 41]),
    (36, &[20, 21, 29, 32, 46]),
    (37, &[1, 5, 9, 13, 23, 24]),
    (38, &[2, 22, 50]),
    (39, &[6, 8, 20, 28, 30, 47, 48]),
    (40, &[5, 7, 8, 11, 33, 34, 37, 49]),
    (41, &[24, 43]),
    (42, &[]),
    (43, &[1, 2, 11, 15, 17, 29, 38, 47]),
    (44, &[11, 13, 15]),
    (45, &[5, 11, 12, 21, 24]),
    (46, &[23, 24, 26, 31, 36, 41]),
    (47, &[8, 14, 16, 28, 29, 34, 35, 40, 42, 46, 50]),
    (48, &[8, 19, 30, 35, 38, 43, 50]),
    (49, &[7, 8, 17, 18]),
    (50, &[4, 28, 47]),
];

const PR_OUT: [(i64, f64); 50] = [
    (1, 0.01230514588446495),
    (2, 0.01851622003726),
    (3, 0.02089512714725092),
    (4, 0.0117658538242089),
    (5, 0.01812188699760293),
    (6, 0.01370603172547424),
    (7, 0.01766993644629599),
    (8, 0.03400137250799818),
    (9, 0.02253772178242931),
    (10, 0.0131561420050695),
    (11, 0.02654403475508309),
    (12, 0.01386383239731235),
    (13, 0.0203086450538747),
    (14, 0.00902512774017356),
    (15, 0.03672808695956838),
    (16, 0.01771992643552917),
    (17, 0.02031073797506916),
    (18, 0.01298439013813405),
    (19, 0.01267137259396788),
    (20, 0.01679586725172487),
    (21, 0.01911653027692032),
    (22, 0.0128981741116451),
    (23, 0.00882485667115226),
    (24, 0.03290195003741163),
    (25, 0.01067032002801407),
    (26, 0.0236953012325217),
    (27, 0.01673916517786888),
    (28, 0.03375421104823002),
    (29, 0.02465138400978424),
    (30, 0.02526104021117763),
    (31, 0.03431971273393954),
    (32, 0.03497314211893426),
    (33, 0.01458158825232488),
    (34, 0.02164031394989468),
    (35, 0.02020836868133508),
    (36, 0.01508472096454471),
    (37, 0.01476737354061426),
    (38, 0.01319038598708231),
    (39, 0.02360994727883383),
    (40, 0.0180994355386105),
    (41, 0.01394375154429174),
    (42, 0.01357868803688285),
    (43, 0.02524445976750379),
    (44, 0.01988024806748353),
    (45, 0.01694403111121369),
    (46, 0.02259342804847692),
    (47, 0.03719089314603851),
    (48, 0.02035602345369202),
    (49, 0.01710526843866147),
    (50, 0.02454782687642342),
];

const SSSP_V: [u32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

const SSSP_E: [(u32, u32, f64); 13] = [
    (1, 2, 0.5),
    (1, 3, 5.0),
    (1, 4, 5.0),
    (2, 5, 0.5),
    (3, 4, 2.0),
    (5, 6, 0.5),
    (6, 3, 0.5),
    (6, 10, 23.0),
    (7, 1, 1.0),
    (7, 8, 3.2),
    (8, 10, 0.2),
    (9, 10, 0.1),
    (10, 7, 8.0),
];

const SSSP_OUT: [(i64, f64); 10] = [
    (1, 0.0),
    (2, 0.5),
    (3, 2.0),
    (4, 4.0),
    (5, 1.0),
    (6, 1.5),
    (7, 32.5),
    (8, 35.7),
    (9, f64::INFINITY),
    (10, 24.5),
];

#[test]
fn the_pagerank_fixture_is_the_published_graph() {
    // 50 vertices and the sum of the adjacency lists' lengths in edges. A
    // graph that loaded short would make every score below wrong for a reason
    // that has nothing to do with the kernel.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for (v, _) in PR_IN {
        run(&g, &format!("CREATE (:V {{vid: {v}}})"));
    }
    let mut edges = 0usize;
    for (v, outs) in PR_IN {
        for &u in outs {
            run(
                &g,
                &format!("MATCH (a:V {{vid: {v}}}), (b:V {{vid: {u}}}) CREATE (a)-[:E]->(b)"),
            );
            edges += 1;
        }
    }
    let n = run(&g, "MATCH (x:V) RETURN count(x) AS c");
    let e = run(&g, "MATCH ()-[r:E]->() RETURN count(r) AS c");
    assert_eq!(
        n.rows.first().and_then(|r| r.first()),
        Some(&Value::Int(50))
    );
    assert_eq!(
        e.rows.first().and_then(|r| r.first()),
        Some(&Value::Int(edges as i64))
    );
    assert_eq!(PR_OUT.len(), 50, "one expected score per vertex");
}

fn pr_graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for (v, _) in PR_IN {
        run(&g, &format!("CREATE (:V {{vid: {v}}})"));
    }
    for (v, outs) in PR_IN {
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
fn PageRank_matches_the_published_expected_output() {
    // PAGERANK CONFORMS, and this test was written expecting it not to.
    //
    // The coverage plan read TWO divergences out of the source. One was already
    // refuted by reading `fixpoint.rs:112` -- `dangling` IS primed before the
    // loop, so iteration 1 does not omit sink redistribution. The second looked
    // solid: `fixpoint.rs:205` breaks when `delta <= tolerance`, and
    // Graphalytics specifies exactly `numberOfIterations` rounds with no
    // convergence test.
    //
    // Measured against LDBC's own 50-vertex graph at damping 0.85 and 14
    // iterations, the scores match within 1e-6 -- the epsilon Graphalytics
    // validates PageRank by. The early break exists but does not fire here:
    // the default tolerance is tighter than the movement at iteration 14, so
    // the run takes all fourteen rounds anyway.
    //
    // THAT IS NOT A PROOF THE BREAK IS HARMLESS. A graph that converges sooner
    // would stop early and diverge, and this fixture cannot see that. The break
    // stays on the record as a conformance RISK rather than a confirmed defect,
    // which is a weaker and more accurate claim than either the plan's or my
    // own prediction.
    let g = pr_graph();
    let got = by_vid(
        &g,
        "engram.algo.pagerank.stream({nodeLabels: ['V'], relationshipTypes: ['E'],          dampingFactor: 0.85, maxIterations: 14, graphalytics: true})",
        "score",
    );
    let mut worst = 0.0f64;
    let mut worst_at = 0i64;
    for (vid, want) in PR_OUT {
        if let Some(Value::Float(f)) = got.get(&vid) {
            let d = (f - want).abs();
            if d > worst {
                worst = d;
                worst_at = vid;
            }
        }
    }
    // Graphalytics validates PageRank by EPSILON, not exact equality.
    assert!(
        worst < 1e-6,
        "PageRank must match LDBC's published scores within 1e-6; worst          divergence {worst} at vertex {worst_at}"
    );
}

#[test]
fn SSSP_matches_the_published_expected_output() {
    // The weighted graph is given as separate .v and .e files, and the weight
    // rides on the edge property `weight`. Vertex 9 is unreachable from 1 and
    // LDBC writes `Infinity` -- a DIFFERENT sentinel from BFS's i64::MAX, which
    // is why `at_conformant` distinguishes them.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for v in SSSP_V {
        run(&g, &format!("CREATE (:V {{vid: {v}}})"));
    }
    for (a, b, w) in SSSP_E {
        run(
            &g,
            &format!(
                "MATCH (x:V {{vid: {a}}}), (y:V {{vid: {b}}}) CREATE (x)-[:E {{weight: {w}}}]->(y)"
            ),
        );
    }
    let got = by_vid(
        &g,
        "engram.algo.sssp.stream({nodeLabels: ['V'], relationshipTypes: ['E'],          sourceNode: 1, relationshipWeightProperty: 'weight', graphalytics: true})",
        "distance",
    );
    let mut mismatched = Vec::new();
    for (vid, want) in SSSP_OUT {
        match got.get(&vid) {
            Some(Value::Float(f)) if (f - want).abs() < 1e-9 => {}
            Some(Value::Float(f)) if f.is_infinite() && want.is_infinite() => {}
            other => mismatched.push(format!("vid {vid}: want {want}, got {other:?}")),
        }
    }
    assert!(
        mismatched.is_empty(),
        "SSSP must match LDBC's published distances, including `Infinity` for          the unreachable vertex 9: {mismatched:?}"
    );
}

#[test]
fn graphalytics_pagerank_runs_every_round_it_was_asked_for() {
    // THE GATE, asserted rather than assumed. Graphalytics has no convergence
    // test: the expected scores ARE the state after exactly
    // `numberOfIterations` rounds. `fixpoint::run` stops early when
    // `delta <= tolerance`, so a graph that settles sooner would return a
    // different vector -- and the LDBC validation graph does NOT settle early
    // at 14, which is why the conformance test passed even before the break
    // was gated. A fixture that cannot distinguish the two states cannot
    // defend the gate.
    //
    // This one can: a two-node cycle reaches its fixed point immediately, so
    // the default mode converges and stops while the conformant mode must run
    // the full cap. The `iterations` column is what separates them.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for v in 1..=2 {
        run(&g, &format!("CREATE (:V {{vid: {v}}})"));
    }
    for (a, b) in [(1, 2), (2, 1)] {
        run(
            &g,
            &format!("MATCH (x:V {{vid: {a}}}), (y:V {{vid: {b}}}) CREATE (x)-[:E]->(y)"),
        );
    }
    let iters = |conformant: bool| -> i64 {
        let r = run(
            &g,
            &format!(
                "CALL engram.algo.pagerank.stats({{nodeLabels: ['V'],                  relationshipTypes: ['E'], dampingFactor: 0.85,                  maxIterations: 20, graphalytics: {conformant}}})                  YIELD iterations RETURN iterations"
            ),
        );
        match r.rows.first().and_then(|row| row.first()) {
            Some(Value::Int(n)) => *n,
            other => panic!("expected an iteration count, got {other:?}"),
        }
    };
    let conformant = iters(true);
    assert_eq!(
        conformant, 20,
        "under `graphalytics: true` PageRank must run all 20 rounds; it ran          {conformant}, so the convergence break is still firing"
    );
    let default = iters(false);
    assert!(
        default < 20,
        "the DEFAULT must still converge early on a graph that settles -- it          ran {default} of 20, so this fixture cannot tell the modes apart and          proves nothing about the gate"
    );
}
