#![allow(non_snake_case)]
//! Differential tests for the FRONTIER-BFS VARIABLE-LENGTH hop in the columnar
//! pipeline (`DataChunk::expand_var_length_bfs`, dispatched by `run_hop`): a
//! bounded `-[:T*1..max]-` hop whose end node a following `WITH DISTINCT <end>` /
//! `RETURN DISTINCT` consumes DISTINCT-only runs set-at-a-time over a visited
//! set, producing each reachable node ONCE at its shortest depth. This is the
//! last IC5 sub-shape: it slots into the DISTINCT projection (`recognise_distinct`)
//! and the multi-stage WITH machinery (`recognise_multistage`), so the full IC5
//! statement `MATCH (a)-[:KNOWS*1..2]-(friend) WHERE a<>friend WITH DISTINCT
//! friend MATCH ... RETURN forum, count(post) ORDER BY count DESC, id` runs
//! end-to-end columnar.
//!
//! THE CONTRACT (as in `pipeline_multistage`/`pipeline_distinct`): for every
//! ACCEPTED shape `set_columnar_scans(true)` (the pipeline BFS) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path, itself the
//! oracle frontier BFS) — the full ROW SET *and its order*, byte-for-byte — and
//! the pipeline BFS must FIRE (a distinct counter). Every DECLINED shape falls
//! back and still agrees, and the BFS must NOT fire.
//!
//! THE BFS SEMANTICS REPRODUCED from `interp::expand_var_length_bfs`:
//!   - a node enters `seen` the first time reached, fixing shortest depth +
//!     single emission. A DIRECTED walk starts with `seen` empty, so a start it
//!     reaches again (a real directed cycle) is emitted. An UNDIRECTED walk
//!     starts with the start seen — it would otherwise re-reach it through the
//!     edge it left by, a walk relationship isomorphism forbids — and emits it
//!     only when a real cycle of at most `max` runs through it
//!     (`interp::shortest_cycle_through`). Until 2026-09-24 both BFS copies
//!     emitted the start on the reverse leg and this suite, comparing them only
//!     with each other, pinned that: every accept is now ALSO held to the
//!     enumeration (`set_frontier_expand(false)`), the oracle both must match.
//!   - FORWARD `adjacent_slim` order (NOT the reversed order the fixed hop uses).
//!   - `depth` runs 1..=max; `depth < max` gates the next frontier (the bound).
//!
//! THREE CANARIES (each: break the operator, this suite's named test FAILS vs the
//! oracle; then restore):
//!   1. drop the cycle test (never emit the undirected start) ->
//!      `reach_set_includes_a_start_on_a_cycle_within_reach` diverges (the start on
//!      the four-cycle disappears); emit it on the reverse leg as before ->
//!      `reach_set_undirected_1_2_exact` diverges (a0 reappears at `*1..2`).
//!   2. use REVERSED adjacency (like the fixed hop) -> `first_seen_order_is_forward_adjacency`
//!      diverges (an order-sensitive DISTINCT reach set flips).
//!   3. drop the `depth < max` bound (go one level too deep) -> `depth_bound_is_exact`
//!      diverges (`*1..2` starts returning the `*1..3` node).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// The REACH fixture. One `:Anchor` node `a0` (the sole scan start) plus `:N`
/// nodes, every node carrying an `x` id property. Directed `T` edges:
///   a0->n1, a0->n2 (depth 1), n1->n3, n2->n3 (depth 2, n3 shared), n3->n4 (depth 3).
/// UNDIRECTED, the only cycle through a0 is a0-n1-n3-n2-a0 — four relationships
/// long — so `*1..2` never returns to a0 (the reverse legs n1->a0, n2->a0 reuse the
/// edge they left by), and `*1..4` does.
fn greach() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |g: &Graph, label: &str, x: i64| {
        let mut p = BTreeMap::new();
        p.insert("x".to_string(), Value::Int(x));
        g.create_node(&[label.to_string()], &p).expect("node")
    };
    let a0 = mk(&g, "Anchor", 0);
    let n1 = mk(&g, "N", 1);
    let n2 = mk(&g, "N", 2);
    let n3 = mk(&g, "N", 3);
    let n4 = mk(&g, "N", 4);
    // Creation order is load-bearing for adjacency order (canary #2).
    for (s, d) in [(a0, n1), (a0, n2), (n1, n3), (n2, n3), (n3, n4)] {
        g.create_rel(s, "T", d, &BTreeMap::new()).expect("T");
    }
    g
}

/// `src` by ENUMERATION — the frontier walk off, the general path: the oracle
/// every frontier answer must match (as a set; the walk's order is its own).
fn enumerated(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    g.set_frontier_expand(false);
    g.set_columnar_scans(false);
    let rows = run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows;
    g.set_frontier_expand(true);
    g.set_columnar_scans(true);
    rows
}

/// Run `src` with the pipeline ON and OFF; return `(on, off)` row sets in order.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let run = |g: &Graph, p: BTreeMap<String, Value>| {
        let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
        run_query(g, &q, p)
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    };
    g.set_columnar_scans(true);
    let on = run(g, params.clone());
    g.set_columnar_scans(false);
    let off = run(g, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// The count of a named counter after running `src` with the pipeline ON.
fn counter_after(g: &Graph, src: &str, name: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| {
        let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
        run_query(g, &q, BTreeMap::new()).expect("run");
    });
    trace.counters().get(name).copied().unwrap_or(0)
}

/// Whether the columnar frontier-BFS var-length operator fired for `src` (ON).
fn bfs_fired(g: &Graph, src: &str) -> bool {
    counter_after(g, src, "interp.pipeline var-length BFS ran") > 0
}

/// Content assert robust to order (order is checked separately by the in-order
/// ON==OFF differential on the same query).
fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    v.sort_by_key(|row| format!("{row:?}"));
    v
}

// ─── ACCEPTS: the BFS reach set, byte-identical ON==OFF ─────────────────────────

/// The core reach set: undirected `*1..2` from `a0`, DISTINCT end, ordered. The
/// set is {n1, n2, n3}: a0 is NOT reached — the way back within two
/// relationships is the edge it left by (see `greach`).
#[test]
fn reach_set_undirected_1_2_exact() {
    let g = greach();
    let src = "MATCH (a:Anchor)-[:T*1..2]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "ON must equal OFF for the BFS reach set");
    assert_eq!(on, enumerated(&g, src), "the walk must answer as the enumeration");
    assert_eq!(
        on,
        vec![vec![i(1)], vec![i(2)], vec![i(3)]],
        "undirected *1..2 reaches {{n1,n2,n3}}, not the start"
    );
    assert!(
        bfs_fired(&g, src),
        "the columnar BFS must fire on an accept"
    );
}

/// Depths are EXACT (canary #3's target): `*1..1`, `*1..2`, `*1..3` DIRECTED from
/// a0 reach nested sets {n1,n2} ⊂ {n1,n2,n3} ⊂ {n1,n2,n3,n4}. Dropping the
/// `depth < max` bound makes `*1..2` leak n4.
#[test]
fn depth_bound_is_exact() {
    let g = greach();
    let cases: &[(&str, Vec<Vec<Value>>)] = &[
        (
            "MATCH (a:Anchor)-[:T*1..1]->(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
            vec![vec![i(1)], vec![i(2)]],
        ),
        (
            "MATCH (a:Anchor)-[:T*1..2]->(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
            vec![vec![i(1)], vec![i(2)], vec![i(3)]],
        ),
        (
            "MATCH (a:Anchor)-[:T*1..3]->(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
            vec![vec![i(1)], vec![i(2)], vec![i(3)], vec![i(4)]],
        ),
    ];
    for (src, want) in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "ON==OFF for `{src}`");
        assert_eq!(&on, want, "exact reach set for `{src}`");
        assert!(bfs_fired(&g, src), "BFS must fire for `{src}`");
    }
}

/// FORWARD adjacency order (canary #2's target). Directed `*1..1` from a0, DISTINCT
/// end, NO ORDER BY — so the raw first-seen (frontier x forward-adjacency) order is
/// itself under test. Reversing adjacency flips the two depth-1 rows.
#[test]
fn first_seen_order_is_forward_adjacency() {
    let g = greach();
    let src = "MATCH (a:Anchor)-[:T*1..1]->(b) RETURN DISTINCT b.x AS x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "ON must equal OFF including ORDER (forward adjacency, first-seen)"
    );
    assert_eq!(
        sorted(on.clone()),
        vec![vec![i(1)], vec![i(2)]],
        "the reach set content is {{n1,n2}} regardless of order"
    );
    assert!(bfs_fired(&g, src), "BFS must fire");
}

/// The start is EMITTED when a real cycle brings the walk back to it (canary
/// #1's target): undirected `*1..4`, NO `WHERE`, and a0-n1-n3-n2-a0 is four long,
/// so a0 is in the reach set. Never emitting the undirected start would drop it.
#[test]
fn reach_set_includes_a_start_on_a_cycle_within_reach() {
    let g = greach();
    let src = "MATCH (a:Anchor)-[:T*1..4]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "ON==OFF");
    assert_eq!(on, enumerated(&g, src), "the walk must answer as the enumeration");
    assert!(
        on.contains(&vec![i(0)]),
        "the start a0 (x=0), on a four-cycle, must be present at *1..4"
    );
    assert!(bfs_fired(&g, src), "BFS must fire");
}

/// The clause WHERE is applied DOWNSTREAM: `WHERE a<>b` would remove the start
/// had the walk reached it. Undirected `*1..2` with `WHERE a<>b`: {n1,n2,n3}.
#[test]
fn where_a_ne_b_removes_the_re_reached_start_downstream() {
    let g = greach();
    let src = "MATCH (a:Anchor)-[:T*1..2]-(b) WHERE a <> b RETURN DISTINCT b.x AS x ORDER BY b.x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "ON==OFF with the downstream WHERE");
    assert_eq!(
        on,
        vec![vec![i(1)], vec![i(2)], vec![i(3)]],
        "WHERE a<>b removes the re-reached start a0"
    );
    assert!(
        !on.contains(&vec![i(0)]),
        "a0 must be filtered out by the downstream WHERE"
    );
    assert!(
        bfs_fired(&g, src),
        "BFS must still fire (WHERE is downstream)"
    );
}

/// Directed vs undirected differ (and both are exact ON==OFF): directed `*1..2`
/// never goes back up an edge; undirected reaches n1..n3 from both sides, and —
/// like directed — not a0, which only a four-cycle returns to.
#[test]
fn directed_and_undirected_both_exact() {
    let g = greach();
    let dir = "MATCH (a:Anchor)-[:T*1..2]->(b) RETURN DISTINCT b.x AS x ORDER BY b.x";
    let und = "MATCH (a:Anchor)-[:T*1..2]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x";
    let (on_d, off_d) = both(&g, dir, BTreeMap::new());
    let (on_u, off_u) = both(&g, und, BTreeMap::new());
    assert_eq!(on_d, off_d, "directed ON==OFF");
    assert_eq!(on_u, off_u, "undirected ON==OFF");
    assert_eq!(
        on_d,
        vec![vec![i(1)], vec![i(2)], vec![i(3)]],
        "directed set"
    );
    assert_eq!(on_u, enumerated(&g, und), "undirected: as the enumeration");
    assert_eq!(on_d, enumerated(&g, dir), "directed: as the enumeration");
    assert_eq!(
        on_u,
        vec![vec![i(1)], vec![i(2)], vec![i(3)]],
        "undirected set: not the start, which no two-relationship walk returns to"
    );
    assert!(bfs_fired(&g, dir) && bfs_fired(&g, und), "both fire");
}

/// A named type never minted matches nothing (empty tokens -> produce nothing).
/// ON==OFF (both empty). Fix 101: a hop over a type that holds no
/// relationship (a never-minted one included) is answered before any
/// operator runs, so the BFS need not fire — the statement is counted as
/// matched nothing instead.
#[test]
fn unminted_type_matches_nothing() {
    let g = greach();
    let src = "MATCH (a:Anchor)-[:NOPE*1..2]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "ON==OFF");
    assert!(on.is_empty(), "an unminted type reaches nothing");
    assert_eq!(
        counter_after(
            &g,
            src,
            "interp.match over a relationship type with no live relationship matched nothing"
        ),
        1,
        "answered before any operator"
    );
}

// ─── ACCEPT: the full IC5 shape end-to-end ─────────────────────────────────────

/// The IC5 fixture: `:Person` graph over KNOWS, then LIKES/CONTAINER_OF to forums.
/// KNOWS: p0-p1, p0-p2, p1-p3. Undirected `*1..2` from p0 reaches {p0,p1,p2,p3};
/// `WHERE p0<>friend` -> {p1,p2,p3}. LIKES: p1->po0,po1; p2->po2; p3->po3,po4.
/// CONTAINER_OF: f0->po0,po1,po2 (fx=100); f1->po3,po4 (fx=200). So per-forum
/// post counts over the friends are f0=3, f1=2.
fn gic5() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |g: &Graph, label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.to_string()], &p).expect("node")
    };
    let p0 = mk(&g, "Person", "pid", 0);
    let p1 = mk(&g, "Person", "pid", 1);
    let p2 = mk(&g, "Person", "pid", 2);
    let p3 = mk(&g, "Person", "pid", 3);
    for (s, d) in [(p0, p1), (p0, p2), (p1, p3)] {
        g.create_rel(s, "KNOWS", d, &BTreeMap::new())
            .expect("KNOWS");
    }
    let f0 = mk(&g, "Forum", "fx", 100);
    let f1 = mk(&g, "Forum", "fx", 200);
    let po: Vec<u64> = (0..5).map(|k| mk(&g, "Post", "px", k)).collect();
    for (f, pi) in [(f0, 0usize), (f0, 1), (f0, 2), (f1, 3), (f1, 4)] {
        g.create_rel(f, "CONTAINER_OF", po[pi], &BTreeMap::new())
            .expect("CONTAINER_OF");
    }
    for (pers, pi) in [(p1, 0usize), (p1, 1), (p2, 2), (p3, 3), (p3, 4)] {
        g.create_rel(pers, "LIKES", po[pi], &BTreeMap::new())
            .expect("LIKES");
    }
    g
}

/// The whole IC5 statement, columnar end-to-end: var-length -> WITH DISTINCT ->
/// second MATCH (join) -> group-by count -> ORDER BY count DESC, id. ON==OFF
/// exactly, and the columnar BFS fired.
#[test]
fn ic5_end_to_end_on_equals_off() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)-[:LIKES]->(post)<-[:CONTAINER_OF]-(forum) \
               RETURN forum.fx AS fx, count(post) AS c ORDER BY c DESC, fx";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "IC5 ON must equal OFF row-for-row and in order");
    assert_eq!(
        on,
        vec![vec![i(100), i(3)], vec![i(200), i(2)]],
        "per-forum post counts over p0's 1..2-hop friends, ordered by count DESC"
    );
    assert!(
        bfs_fired(&g, src),
        "the stage-1 var-length BFS must fire columnar"
    );
    // The WHOLE statement ran through the columnar multi-stage path (var-length ->
    // WITH DISTINCT -> join -> group-by count), not a partial columnar prefix.
    assert_eq!(
        counter_after(&g, src, "interp.pipeline multistage runs"),
        1,
        "IC5 must run end-to-end through the columnar multi-stage pipeline"
    );
}

// ─── DECLINES: each falls back (ON==OFF) and the BFS does NOT fire ──────────────

/// Every var-length shape the pipeline does NOT reproduce as a frontier BFS: it
/// declines to the enumerating general path, ON==OFF still holds, and the columnar
/// BFS operator never runs.
#[test]
fn declines_and_falls_back_without_firing() {
    let g = greach();
    let declines: &[&str] = &[
        // UNBOUNDED `*` — no finite max.
        "MATCH (a:Anchor)-[:T*]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
        // `*2..3` — min != 1 (shortest-depth-once soundness needs min 1).
        "MATCH (a:Anchor)-[:T*2..3]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
        // A PATH VARIABLE binds the walk — the enumerating path owns it.
        "MATCH p=(a:Anchor)-[:T*1..2]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
        // A RELATIONSHIP VARIABLE (the rel list) — the enumerating path owns it.
        "MATCH (a:Anchor)-[r:T*1..2]-(b) RETURN DISTINCT b.x AS x ORDER BY b.x",
        // NOT DISTINCT-consumed: a plain RETURN — the end's multiplicity matters.
        "MATCH (a:Anchor)-[:T*1..2]-(b) RETURN b.x AS x",
        // NOT DISTINCT-consumed: a non-distinct WITH carrying the end forward.
        "MATCH (a:Anchor)-[:T*1..2]-(b) WITH b MATCH (b)-[:T]->(c) RETURN c.x AS x",
    ];
    for src in declines {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
        assert!(
            !bfs_fired(&g, src),
            "the columnar BFS must NOT fire on a decline: `{src}`"
        );
    }
}
