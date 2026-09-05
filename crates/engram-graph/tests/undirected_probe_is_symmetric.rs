#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! An UNDIRECTED bound-edge probe may be asked from either end; a DIRECTED one
//! may not.
//!
//! # What this guards
//!
//! The fold's `EdgeToBound` predicate answers "is there a `T` between the level
//! var and the bound var" with `edge_count_slim`. For `Dir::Both` the two
//! arguments are interchangeable — `Both` admits an edge in either direction, so
//! the question is symmetric — and the engine now probes from the BOUND side,
//! because that node is fixed for the whole subtree and its adjacency row stays
//! hot, where the level var's row is a different line of a 17M-entry CSR on
//! every call. LSQB q3's triangle close runs it ~12.8M times.
//!
//! For a DIRECTED probe the swap would answer a different question:
//! `(a)-[:T]->(b)` is not `(b)-[:T]->(a)`. This test builds a graph where those
//! two differ on every pair and asserts the engine still distinguishes them.
//!
//! Both halves matter. Without the asymmetric half, swapping unconditionally
//! would pass; without the symmetric half, never swapping would pass.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn count(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one Int from `{src}`, got {other:?}"),
    }
}

/// A STRICTLY ONE-WAY ring: `i -[:FOLLOWS]-> i+1`, and never the reverse. So
/// for every adjacent pair the directed question has opposite answers depending
/// which end asks, while the undirected one has the same answer from both.
const N: i64 = 40;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    for p in 0..N {
        let q = (p + 1) % N;
        stmt(
            &g,
            &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:FOLLOWS]->(b)"),
        );
    }
    g.shared_store().seal();
    g
}

#[test]
fn undirected_agrees_from_both_ends_and_directed_does_not() {
    let g = fixture();

    // UNDIRECTED: written from either end, the same set of pairs.
    let fwd = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]-(b:P) WHERE a.id = 0 RETURN count(*) AS c",
    );
    let rev = count(
        &g,
        "MATCH (b:P)-[:FOLLOWS]-(a:P) WHERE a.id = 0 RETURN count(*) AS c",
    );
    assert_eq!(
        fwd, rev,
        "an undirected probe must answer the same from either end — this is the \
         symmetry the bound-side swap relies on"
    );
    assert_eq!(
        fwd, 2,
        "node 0 is adjacent to 1 (outgoing) and N-1 (incoming), so the \
         undirected degree is 2; if this is not 2 the fixture does not pose the \
         question"
    );

    // DIRECTED: the ring is one-way, so the two directions must NOT agree.
    let out = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P) WHERE a.id = 0 RETURN count(*) AS c",
    );
    let inc = count(
        &g,
        "MATCH (a:P)<-[:FOLLOWS]-(b:P) WHERE a.id = 0 RETURN count(*) AS c",
    );
    assert_eq!(out, 1, "node 0 follows exactly one node");
    assert_eq!(inc, 1, "exactly one node follows node 0");
    // ...and they are DIFFERENT nodes, which is what a swapped directed probe
    // would conflate.
    let same = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P) WHERE a.id = 0 AND b.id = 39 RETURN count(*) AS c",
    );
    let flipped = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P) WHERE a.id = 39 AND b.id = 0 RETURN count(*) AS c",
    );
    assert_eq!(same, 0, "0 does NOT follow 39");
    assert_eq!(flipped, 1, "39 DOES follow 0");
    assert_ne!(
        same, flipped,
        "the directed probe must distinguish its two ends — if a swap were \
         applied to directed hops it would answer the wrong one here"
    );
}

/// The DIRECTED close probed from the BOUND side — `Dir::flipped` is what
/// keeps it the same question, and this is the test that bites if it is
/// forgotten.
///
/// # What this guards
///
/// P-1 (the directed bound-side probe) reads a directed close from the bound
/// endpoint's hot row with the direction flipped. Probing the bound row
/// WITHOUT flipping answers the reverse edge — on this fixture, "a follows b"
/// where the close asks "b follows a". The fixture makes those two answers
/// differ by construction: every ring pair has the forward edge, only the
/// first ten pairs have the backward one, so the true directed 2-cycle count
/// is 20 while the unflipped-probe answer is the forward-edge count, 50.
///
/// # Proven to bite
///
/// Checked by shipping the bound-side swap with the `flipped()` call removed:
/// both assertions fail with 50 where 20 is required.
#[test]
fn a_directed_close_from_the_bound_side_answers_the_directed_question() {
    let g = fixture();
    // Back-edges on the first ten pairs ONLY — the asymmetry the test needs.
    for p in 0..10 {
        stmt(
            &g,
            &format!(
                "MATCH (a:P {{id: {}}}), (b:P {{id: {p}}}) CREATE (a)-[:FOLLOWS]->(b)",
                p + 1
            ),
        );
    }
    g.shared_store().seal();

    // The close as the path's FINAL HOP — `hop_sum`'s `count_edges` arm.
    let cycles = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    );
    assert_eq!(
        cycles, 20,
        "ten mutual pairs, each walked from both ends = 20 directed 2-cycles; \
         any other number means the tracked close's bound-side probe answered \
         a DIFFERENT directed question — the direction was not flipped with \
         the row (the unflipped break measured 0 here)"
    );

    // The close as a SEPARATE SINGLE-HOP PATH — an UNTRACKED close
    // (`!hop.track`), which is `hop_sum`'s `count_edges` arm. The cycle
    // spelling above is a TRACKED close (its path has two hops) and takes the
    // `edges_to_peer_slim` branch instead — the first bite check proved they
    // are different code paths by failing to bite through the wrong one.
    let via_second_path = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P), (b)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    );
    assert_eq!(
        via_second_path, 20,
        "the two-path spelling must agree — 50 here is the unflipped \
         bound-side probe in `count_edges` (the untracked close)"
    );

    // The same question as a WHERE edge predicate — `pred_holds`'s
    // `EdgeToBound` arm, the other site the swap touches.
    let via_where = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]->(b:P) WHERE (b)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    );
    assert_eq!(
        via_where, 20,
        "the WHERE spelling must agree with the cycle spelling — 50 here is \
         the unflipped bound-side probe in `pred_holds`"
    );
}

/// The triangle close itself, over a one-way ring plus chords, so the fold's
/// `EdgeToBound` runs on UNDIRECTED hops against genuinely asymmetric edges.
#[test]
fn an_undirected_triangle_count_is_unchanged_by_the_bound_side_probe() {
    let g = fixture();
    // Chords, still strictly one-way, to create triangles.
    for p in 0..N {
        let q = (p + 2) % N;
        stmt(
            &g,
            &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:FOLLOWS]->(b)"),
        );
    }
    g.shared_store().seal();

    let tri = count(
        &g,
        "MATCH (a:P)-[:FOLLOWS]-(b:P)-[:FOLLOWS]-(c:P)-[:FOLLOWS]-(a) RETURN count(*) AS c",
    );
    // Every i forms a triangle with i+1 and i+2 (edges i->i+1, i+1->i+2, i->i+2,
    // all admitted undirected). N such triangles, each counted once per ordered
    // walk that traverses its three distinct edges: 6 per triangle.
    assert_eq!(
        tri,
        N * 6,
        "the undirected triangle count over a one-way ring with +2 chords must \
         be {} — a probe that answered the DIRECTED question from the wrong end \
         would miss the edges it cannot see",
        N * 6
    );
}
