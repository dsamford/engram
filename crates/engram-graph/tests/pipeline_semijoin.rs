#![allow(non_snake_case)]
//! Differential tests for the SEMIJOIN / CYCLE columnar pipeline (Phase 4b1 of
//! `pipeline::plan_and_run_columnar`): a multi-path `MATCH` whose LAST hop of a
//! path CLOSES onto an ALREADY-BOUND var — a connecting path / cycle, the core
//! of LDBC SNB IC5 minus OPTIONAL. Phase 4a required every hop's end var to be
//! NEW; 4b1 additionally accepts a path's FINAL hop landing on a bound var,
//! recorded as a SEMIJOIN step (no new column, one row per closing edge).
//!
//! The contract is `pipeline_core`/`pipeline_multipath`'s: for every accepted
//! shape, `set_columnar_scans(true)` (the columnar semijoin) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full
//! ROW SET *and its order*, byte-for-byte — and every declined shape falls back
//! and still agrees.
//!
//! WHAT THE SEMIJOIN REPRODUCES from `run_streaming` (`expand_var_length` with a
//! bound end var, the slim path):
//!   - REVERSE adjacency: closing edges are walked in reverse `adjacent_slim`
//!     order (the LIFO stack-pop order), like every expand.
//!   - TARGET-CLOSE: an edge is kept iff its peer equals the row's bound target
//!     id (the `target_ok` check at depth 1).
//!   - ROW MULTIPLICATION: one output row PER closing edge — two parallel edges
//!     connecting the pair yield TWO rows (each edge is its own stack completion).
//!   - PER-PATH isomorphism: a multi-hop path's closing edge may not reuse a rel
//!     the path already walked; a later 1-hop semijoin path RE-SEEDS `used` empty
//!     so a cross-path rel reuse is KEPT.
//!   - NO closing edge ⇒ the source row is DROPPED (non-OPTIONAL semantics).
//!
//! CANARY (see `semijoin_row_multiplication_is_load_bearing`): neutralising the
//! per-edge emission (emit at most one row per source row) drops the doubled
//! closing edge and the multiplication assertion + the differential both fail —
//! proving the one-row-per-edge behaviour is load-bearing and the pipeline fires.
//! NOTE on the REVERSE canary: unlike an expand's `.rev()` (which reorders the
//! DISTINCT new-var values it appends and so is observable), the semijoin's
//! closing edges of one source row all copy the SAME ids and carry no rel column
//! — they are byte-identical rows, so reversing them is a permutation of equal
//! elements and is NOT observable through output. Byte-identity therefore holds
//! for either iteration direction; the reverse is retained only for structural
//! fidelity with `expand`.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A triangle-capable graph. A{ak}-T1->B{bx,bn}; A-T2->C{cx,cn}; C-T3->B is the
/// CLOSING hop. Edge creation order is load-bearing (it drives reverse
/// adjacency). The T3 edges are laid out to exercise: a plain close, a source
/// (c2) with NO closing edge (its triangle rows drop), a B (b2) that is never a
/// T3 target (its rows drop), and a DOUBLE parallel edge (c1->b0 twice) that
/// MULTIPLIES its row.
fn gtri() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Aa".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2)]; // a0, a1
    let mk_b = |bx: i64, bn: &str| {
        let mut p = BTreeMap::new();
        p.insert("bx".to_string(), Value::Int(bx));
        p.insert("bn".to_string(), Value::Str(bn.to_string()));
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [mk_b(5, "p"), mk_b(5, "q"), mk_b(9, "r")]; // b0,b1,b2 (bx=5 tie: b0,b1)
    let mk_c = |cx: i64, cn: &str| {
        let mut p = BTreeMap::new();
        p.insert("cx".to_string(), Value::Int(cx));
        p.insert("cn".to_string(), Value::Str(cn.to_string()));
        g.create_node(&["Cc".into()], &p).expect("c")
    };
    let c = [mk_c(7, "m"), mk_c(7, "n"), mk_c(3, "o")]; // c0,c1,c2

    // T1 a->b.
    for (s, t) in [(0, 0), (0, 1), (0, 2), (1, 0)] {
        g.create_rel(a[s], "T1", b[t], &BTreeMap::new())
            .expect("T1");
    }
    // T2 a->c.
    for (s, t) in [(0, 0), (0, 1), (0, 2), (1, 0)] {
        g.create_rel(a[s], "T2", c[t], &BTreeMap::new())
            .expect("T2");
    }
    // T3 c->b (the CLOSING hop). c1->b0 is DOUBLED; c2 has none; nothing hits b2.
    for (s, t) in [(0, 0), (0, 1), (1, 0), (1, 0)] {
        g.create_rel(c[s], "T3", b[t], &BTreeMap::new())
            .expect("T3");
    }
    g
}

/// Nn{nk} with a self-loop and a back-edge — the per-path isomorphism fixture
/// (shared with `pipeline_multipath`). `r_self` (n0->n0) can be reused ACROSS a
/// path boundary but not WITHIN one path.
fn giso() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |nk: i64| {
        let mut p = BTreeMap::new();
        p.insert("nk".to_string(), Value::Int(nk));
        g.create_node(&["Nn".into()], &p).expect("n")
    };
    let n = [mk(0), mk(1)];
    g.create_rel(n[0], "R", n[0], &BTreeMap::new())
        .expect("self"); // r_self
    g.create_rel(n[0], "R", n[1], &BTreeMap::new())
        .expect("r01");
    g.create_rel(n[1], "R", n[0], &BTreeMap::new())
        .expect("r10");
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON and the general path OFF.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the non-aggregate pipeline fired for `src` with columnar ON.
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

/// Whether the AGGREGATE pipeline fired for `src` with columnar ON.
fn aggregate_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline aggregate runs")
        .copied()
        == Some(1)
}

/// Convenience: `Int` / `Str` cell builders for `want` vectors.
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(t: &str) -> Value {
    Value::Str(t.to_string())
}

/// THE TRIANGLE across shapes. `(a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc),
/// (c)-[:T3]->(b)` — the last hop CLOSES onto the bound `b`. The full row set and
/// order must equal the general path over projections, ORDER BY, SKIP/LIMIT.
#[test]
fn semijoin_triangle_matches_general_across_shapes() {
    let g = gtri();
    let cases: &[&str] = &[
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn, a.ak LIMIT 100",
        // ORDER BY the closed-onto var b, DESC.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bx AS bx, c.cn AS cn ORDER BY b.bx DESC, c.cn, a.ak LIMIT 100",
        // SKIP + LIMIT over pure production order.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn SKIP 1 LIMIT 2",
        // A restated (subset) label on the bound end — must still close.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b:Bb) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn, a.ak LIMIT 100",
        // The single-path form of the same closure: (a)-[:T2]->(c)-[:T3]->(b)
        // is one 2-hop path whose LAST hop closes onto b (bound by path 1).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn, a.ak LIMIT 100",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "triangle vs general disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn"
        ),
        "a connecting-path triangle is a semijoin shape — it must fire"
    );
}

/// The closed row SET is exactly what the closing edges license — no b2 (never a
/// T3 target), no c2 (no closing edge), and the c1->b0 DOUBLE edge present twice.
/// Asserted as an exact ordered `want` under a total ORDER BY.
#[test]
fn semijoin_triangle_exact_rows() {
    let g = gtri();
    let src = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "exact-rows vs general disagree");
    // a0: (b0=p,c0=m), (b0=p,c1=n)x2, (b1=q,c0=m); a1: (b0=p,c0=m). Sorted by
    // (ak, bn, cn) with the tie (1,p,n) doubled adjacent.
    let want = vec![
        vec![i(1), s("p"), s("m")],
        vec![i(1), s("p"), s("n")],
        vec![i(1), s("p"), s("n")], // the DOUBLE closing edge c1->b0
        vec![i(1), s("q"), s("m")],
        vec![i(2), s("p"), s("m")],
    ];
    assert_eq!(on, want, "closed triangle row set/order wrong");
    // b2 ("r") never appears (nothing closes onto it); c2 ("o") never appears
    // (it has no T3 edge), so its triangle rows are DROPPED.
    assert!(
        !on.iter().any(|r| r[1] == s("r")),
        "b2 is never a T3 target — must be absent"
    );
    assert!(
        !on.iter().any(|r| r[2] == s("o")),
        "c2 has no closing edge — its rows must drop (non-OPTIONAL)"
    );
}

/// ROW MULTIPLICATION + THE CANARY. Two parallel closing edges (c1->b0 twice)
/// yield TWO identical rows, matching `run_streaming`'s per-edge completion.
/// Neutralising the semijoin's per-edge emission (a `break` after the first
/// matching edge) makes this row appear ONCE — the differential and this count
/// assertion both fail. That is the canary; it also proves the pipeline fires.
#[test]
fn semijoin_row_multiplication_is_load_bearing() {
    let g = gtri();
    let src = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "row-multiplication vs general disagree");
    let doubled = vec![i(1), s("p"), s("n")];
    let n = on.iter().filter(|r| **r == doubled).count();
    assert_eq!(
        n, 2,
        "the doubled closing edge c1->b0 must yield TWO rows (one per edge)"
    );
    assert!(pipeline_fired(&g, src), "the semijoin shape must fire");
}

/// A TIE GROUP cut by LIMIT. Under `WHERE c.cx = 7` (drops c2) with `ORDER BY
/// b.bx` (b0,b1 share bx=5, b2 excluded), the surviving rows tie on the sort key,
/// so which rows LIMIT keeps is decided purely by the semijoin production order.
/// ON must equal OFF at every LIMIT, and the count is `min(limit, group)`.
#[test]
fn semijoin_tie_group_resolves_like_general() {
    let g = gtri();
    // Total closed rows under c.cx=7 (c2 excluded): a0:(p,m),(p,n)x2,(q,m);
    // a1:(p,m) = 5 rows, all bx=5 (b0/b1), so ORDER BY b.bx is one tie group.
    let full = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WHERE c.cx = 7 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY b.bx LIMIT 100";
    let (on_full, off_full) = both(&g, full, BTreeMap::new());
    assert_eq!(on_full, off_full, "tie full vs general disagree");
    let total = on_full.len();
    assert_eq!(total, 5, "expected 5 closed rows under c.cx=7");
    for lim in 1..=total + 1 {
        let src = format!(
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WHERE c.cx = 7 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY b.bx LIMIT {lim}"
        );
        let (on, off) = both(&g, &src, BTreeMap::new());
        assert_eq!(on, off, "tie disagrees at LIMIT {lim}: `{src}`");
        assert_eq!(on.len(), lim.min(total), "tie size wrong at LIMIT {lim}");
    }
}

/// WHERE over EACH bound var of the closed triangle — the pipeline's filter
/// operator over a (start), b (closed-onto) and c (connector) alike.
#[test]
fn semijoin_where_over_each_bound_var() {
    let g = gtri();
    for src in [
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WHERE a.ak = 1 RETURN b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WHERE b.bn = 'p' RETURN a.ak AS ak, c.cn AS cn ORDER BY a.ak, c.cn LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WHERE c.cx > 5 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "semijoin WHERE disagree: `{src}`");
    }
}

/// A CONNECTING PATH feeding a GROUP-BY-COUNT and a COLLECT — the aggregate path
/// over the semijoin chunk. Value-key count, node-identity group-by (fast path),
/// a production-ordered collect and a global aggregate all fold byte-identically
/// to `run_streaming`.
#[test]
fn semijoin_triangle_aggregate() {
    let g = gtri();
    for src in [
        // Value-key group-by-count over the closed rows.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        // Node-identity group-by on the closed-onto var b (the fast path).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) WITH b, count(*) AS n RETURN b.bn AS bn, n ORDER BY bn",
        // Production-ordered collect of the connector over the closed-onto b.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN b.bn AS bn, collect(c.cn) AS cns ORDER BY bn",
        // Global aggregate over the whole closed set (count multiplies per edge;
        // DISTINCT collapses the doubled edge).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN count(*) AS n, count(DISTINCT c) AS dc",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "semijoin aggregate disagree: `{src}`");
    }
    // Value-key count: a0 has 4 closed rows, a1 has 1.
    let (agg, _) = both(
        &g,
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        BTreeMap::new(),
    );
    assert_eq!(agg, vec![vec![i(1), i(4)], vec![i(2), i(1)]], "count wrong");
    assert!(
        aggregate_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, count(*) AS n ORDER BY ak"
        ),
        "a connecting-path group-by-count must fire the aggregate pipeline"
    );
}

/// PER-PATH isomorphism for the CLOSING edge. In the single 2-hop path
/// `(x)-[:R]->(y)-[:R]->(x)`, the closing R may not reuse the R already walked,
/// so the self-loop round-trip (0,0) is DROPPED. Split into two 1-hop paths
/// `(x)-[:R]->(y), (y)-[:R]->(x)`, the closing path RE-SEEDS `used` empty, so the
/// same self-loop reuse is KEPT and (0,0) appears. The pipeline must match BOTH.
#[test]
fn semijoin_per_path_isomorphism_on_the_closing_edge() {
    let g = giso();
    // Single 2-hop path: within-path iso forbids reusing the self-loop.
    let single = "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(x) RETURN x.nk AS xk, y.nk AS yk ORDER BY x.nk, y.nk LIMIT 100";
    let (s_on, s_off) = both(&g, single, BTreeMap::new());
    assert_eq!(s_on, s_off, "single-path closing iso disagree: `{single}`");
    assert!(
        !s_on.contains(&vec![i(0), i(0)]),
        "within one path the self-loop reuse on the close must DROP (0,0)"
    );
    // Two 1-hop paths: the closing path re-seeds `used`, so reuse is KEPT.
    let multi = "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(x) RETURN x.nk AS xk, y.nk AS yk ORDER BY x.nk, y.nk LIMIT 100";
    let (m_on, m_off) = both(&g, multi, BTreeMap::new());
    assert_eq!(m_on, m_off, "cross-path closing iso disagree: `{multi}`");
    assert!(
        m_on.contains(&vec![i(0), i(0)]),
        "across paths the self-loop reuse on the close must be KEPT (0,0)"
    );
    assert!(pipeline_fired(&g, single), "single-path close must fire");
    assert!(pipeline_fired(&g, multi), "cross-path close must fire");
}

/// DECLINE shapes: each carries a feature the semijoin does not model, so the
/// pipeline DECLINES and the general path answers identically (ON == OFF) and the
/// pipeline did NOT fire.
#[test]
fn semijoin_declines_and_falls_back_identically() {
    let g = gtri();
    let declines: &[&str] = &[
        // A NON-final hop onto a bound var: the close is followed by another hop,
        // so it is not a path's FINAL hop (only a final close is a semijoin).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b)-[:T1]->(z:Bb) RETURN a.ak AS ak ORDER BY a.ak LIMIT 5",
        // OPTIONAL on the closing path (that is Phase 4b2, not 4b1).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) OPTIONAL MATCH (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 5",
        // (A bound relationship variable on the closing hop is now ACCEPTED —
        // see `pipeline_relvar.rs`.)
        // A relationship-property test on the closing hop.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3 {w: 1}]->(b) RETURN a.ak AS ak ORDER BY a.ak LIMIT 5",
        // A variable-length closing hop.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3*1..2]->(b) RETURN a.ak AS ak ORDER BY a.ak LIMIT 5",
        // A restated label on the bound end the var never carried (extra constraint).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b:Cc) RETURN a.ak AS ak ORDER BY a.ak LIMIT 5",
        // A property test on the bound end (the close never applied it).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b {bx: 5}) RETURN a.ak AS ak ORDER BY a.ak LIMIT 5",
    ];
    for src in declines {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
        assert!(
            !pipeline_fired(&g, src),
            "pipeline should have DECLINED, not fired: `{src}`"
        );
    }
}

/// The count of a named `counted!` counter after running `src` with columnar ON.
fn counter(g: &Graph, src: &str, key: &str) -> Option<u64> {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied()
}

/// THE COUNTED CLOSE. A semijoin with no rel variable and an EMPTY isomorphism
/// base (a 1-hop closing path, or any close before its path used a rel) emits
/// its rows by the closing-edge MULTIPLICITY from `Graph::edge_count_slim`
/// instead of walking the row — the DOUBLED c1->b0 edge still yields TWO rows
/// (byte-identical, the walk's rows all copy the same ids). The single-path
/// 2-hop form closes with a NON-empty base (its opening rel), so it still
/// walks and the counter stays silent — pinned both ways.
#[test]
fn semijoin_counted_close_keeps_the_doubled_edge() {
    let g = gtri();
    let comma = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100";
    let (on, off) = both(&g, comma, BTreeMap::new());
    assert_eq!(on, off, "counted close vs general disagree");
    let doubled = vec![i(1), s("p"), s("n")];
    assert_eq!(
        on.iter().filter(|r| **r == doubled).count(),
        2,
        "the counted close must still emit the doubled edge twice"
    );
    assert_eq!(
        counter(&g, comma, "interp.pipeline semijoin counted close"),
        Some(1),
        "a 1-hop closing path takes the counted close"
    );
    let chained = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc)-[:T3]->(b) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100";
    let (on2, off2) = both(&g, chained, BTreeMap::new());
    assert_eq!(on2, off2, "walked close vs general disagree");
    assert_eq!(on2, on, "both spellings close the same rows");
    assert_eq!(
        counter(&g, chained, "interp.pipeline semijoin counted close"),
        None,
        "a close whose path already used a rel must WALK (used-rel exclusion)"
    );
    // A bound rel var on the close needs every rel id: walked, never counted.
    let relvar = "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[r:T3]->(b) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 100";
    let (on3, off3) = both(&g, relvar, BTreeMap::new());
    assert_eq!(on3, off3, "rel-var close vs general disagree");
    assert_eq!(counter(&g, relvar, "interp.pipeline semijoin counted close"), None);
}

/// A TRIANGLE under `count(*)` (the fold triple: fold ON / fold OFF / general).
///
/// A triangle is a JOIN, not a tree — but only until the close's TARGET
/// materialises. `plan_count_fold` un-folds a close target that is not on the
/// level's ancestor chain (rather than un-folding the level, which materialised
/// that same target anyway PLUS the level's whole chain), so `b` becomes a real
/// column and what remains — the seed's `T2` fan-out closing onto a bound `b` —
/// IS a tree. The fold therefore FIRES here and the count is exact; the fold is
/// checked against fold-OFF and the general path on every spelling below, which
/// is the evidence that the plan change is a capability and not a divergence.
///
/// (Before the un-fold-the-target rule the whole statement declined and the
/// materialised `DataChunk::semijoin` counted close served it. That operator is
/// pinned by `semijoin_counted_close_keeps_the_doubled_edge` and by the
/// last arm here, whose close target is bound AFTER the fold's root and so
/// cannot satisfy the position rule.)
#[test]
fn semijoin_triangle_count_folds_once_the_close_target_materialises() {
    let g = gtri();
    for (src, fires) in [
        (
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN count(*) AS n",
            true,
        ),
        (
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
            true,
        ),
        (
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc)-[:T3]->(b) RETURN count(*) AS n",
            true,
        ),
        // The SAME triangle with the close's target bound AFTER the fold's root
        // (`b` is now the last var): materialising `b` does not help — it is not
        // below the root's end var, so the position rule fails and the level
        // un-folds. The count still agrees, over the materialised semijoin.
        (
            "MATCH (a:Aa)-[:T2]->(c:Cc), (a)-[:T1]->(b:Bb), (c)-[:T3]->(b) RETURN count(*) AS n",
            false,
        ),
    ] {
        g.set_columnar_scans(true);
        engram_graph::pipeline::set_count_fold(true);
        let on = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(false);
        let fold_off = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(true);
        g.set_columnar_scans(false);
        let general = rows(&g, src, BTreeMap::new());
        g.set_columnar_scans(true);
        assert_eq!(on, general, "triangle count vs general: `{src}`");
        assert_eq!(fold_off, general, "fold OFF vs general: `{src}`");
        // WHETHER the fold fires is a property of the pattern AS WRITTEN, so it
        // is read with the COUNT-ONLY JOIN REORDER (operator C) held off: that
        // pass exists to rewrite a `count(*)`-only pattern into an equivalent
        // one the fold CAN take (`pipeline_count_reorder.rs` pins it), and this
        // case is exactly the triangle it re-orders. The row agreement above is
        // still taken with every default on.
        engram_graph::pipeline::set_count_only_reorder(false);
        let did_fire = counter(&g, src, "interp.pipeline count fold").is_some();
        engram_graph::pipeline::set_count_only_reorder(true);
        assert_eq!(did_fire, fires, "count fold firing: `{src}`");
        assert!(aggregate_fired(&g, src), "the aggregate operator answers: `{src}`");
    }
    // 5 closed rows (the doubled edge counted twice) — the same number whether
    // the triangle folded or not.
    for src in [
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T3]->(b) RETURN count(*) AS n",
        "MATCH (a:Aa)-[:T2]->(c:Cc), (a)-[:T1]->(b:Bb), (c)-[:T3]->(b) RETURN count(*) AS n",
    ] {
        let (on, _) = both(&g, src, BTreeMap::new());
        assert_eq!(on, vec![vec![i(5)]], "`{src}`");
    }
}

/// A CLOSE ONTO THE SEED inside a fold: `(x)-[:R]->(y)-[:R]->(x)` under
/// `count(*)` folds `y` with the closing edge counted by multiplicity, minus
/// the opening rel (rel-iso) — the self-loop round trip drops, exactly as the
/// row form above. Split into two paths the close re-seeds `used` and the
/// reuse is kept. The fold fires on both, and both agree with the general path.
#[test]
fn semijoin_close_onto_seed_folds_with_rel_iso() {
    let g = giso();
    for (src, fires) in [
        ("MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(x) RETURN count(*) AS n", true),
        ("MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(x) RETURN count(*) AS n", true),
        ("MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(x) RETURN x.nk AS xk, count(*) AS n ORDER BY xk", true),
    ] {
        g.set_columnar_scans(true);
        engram_graph::pipeline::set_count_fold(true);
        let on = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(false);
        let fold_off = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(true);
        g.set_columnar_scans(false);
        let general = rows(&g, src, BTreeMap::new());
        g.set_columnar_scans(true);
        assert_eq!(on, general, "folded close vs general: `{src}`");
        assert_eq!(fold_off, general, "fold OFF vs general: `{src}`");
        assert_eq!(
            counter(&g, src, "interp.pipeline count fold").is_some(),
            fires,
            "fold firing: `{src}`"
        );
    }
    // Single path: (0→1→0) and (1→0→1) only — the self-loop round trip drops: 2.
    // Two paths: plus (0→0→0) over the reused self-loop: 3.
    let (single, _) = both(&g, "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(x) RETURN count(*) AS n", BTreeMap::new());
    let (multi, _) = both(&g, "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(x) RETURN count(*) AS n", BTreeMap::new());
    assert_eq!(single, vec![vec![i(2)]], "within-path rel-iso on the folded close");
    assert_eq!(multi, vec![vec![i(3)]], "cross-path reuse kept on the folded close");
}

/// The census / unrelated shapes the semijoin generalisation must NOT perturb —
/// a single-var scan and a single-hop aggregate still DECLINE (the digest must
/// not move).
#[test]
fn semijoin_leaves_census_shapes_declining() {
    let g = gtri();
    for src in [
        "MATCH (n:Aa) RETURN n.ak AS ak ORDER BY n.ak",
        "MATCH (a:Aa)-[:T1]->(b:Bb) RETURN count(*) AS c",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "census shape disagree: `{src}`");
        assert!(
            !pipeline_fired(&g, src),
            "pipeline must not claim a census shape: `{src}`"
        );
    }
}
