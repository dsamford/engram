#![allow(non_snake_case)]
//! Differential tests for UNDIRECTED fixed hops in the columnar pipeline
//! (`pipeline::plan_and_run_columnar`). Phase 4a/4b1 accepted only DIRECTED hops
//! (`-[:T]->`, `<-[:T]-`) and DECLINED undirected (`-[:T]-`) at `collect_hops`;
//! this exercises the one-line generalisation that maps `RelDir::Undirected =>
//! Dir::Both`, routing undirected through the SAME expand / semijoin / optional
//! hot loops.
//!
//! THE LOAD-BEARING FACT (canaried below): an undirected hop's neighbours are
//! produced BYTE-IDENTICALLY to `run_streaming` (columnar OFF) because both walk
//! the SAME `adjacent_slim(src, Dir::Both)` — OUT neighbours (ascending) then IN
//! neighbours (ascending, an IN-side self-loop deduped inside `adjacent_slim`) —
//! and both REVERSE it: the pipeline via `adj.iter().rev()`, `run_streaming` via
//! the LIFO pop of `expand_var_length`. So the neighbour order (IN-desc then
//! OUT-desc), the self-loop dedup, and RELATIONSHIP ISOMORPHISM (an undirected
//! edge carries ONE `rel.id`, so a walk that would re-traverse it is dropped by
//! the same per-row `used` set) all match with no per-direction special-casing.
//!
//! The contract is `pipeline_core`/`pipeline_semijoin`'s: for every accepted
//! shape `set_columnar_scans(true)` (the columnar path) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full
//! ROW SET *and its order*, byte-for-byte.
//!
//! CANARY (`undirected_single_hop_tie_group_resolves_like_general`): the four
//! tied neighbours of `a0` are split OUT{b0,b1} / IN{b2,b3}, so the production
//! order is `adjacent_slim(Both)` reversed = [b3,b2,b1,b0] = bn [s,r,q,p]. Under
//! `ORDER BY b.bx` (all tied) a LIMIT cut keeps a prefix of THAT order, so
//! neutralising the `expand` `.rev()` (or reordering the `Dir::Both` sides)
//! diverges ON from OFF — proving the order is load-bearing and the pipeline
//! fired.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

// ─── Fixtures ────────────────────────────────────────────────────────────────

/// The general undirected graph. `T` is ONE type whose edges point in MIXED
/// directions between the layers, so an undirected hop must offer BOTH the OUT
/// and the IN legs; `U` is a plain DIRECTED type for the directed+undirected mix.
/// Node creation order fixes ids ascending, which fixes `adjacent_slim`'s
/// per-node peer order.
///   Aa{ak}  a0,a1  (a2 has NO edges — an unmatched outer row for OPTIONAL)
///   Bb{bx,bn}  b0(5,p) b1(7,q) b2(5,r)
///   Cc{cx,cn}  c0(1,m) c1(2,n) c2(1,o)
fn gu() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Aa".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3)]; // a2 edge-less
    let mk_b = |bx: i64, bn: &str| {
        let mut p = BTreeMap::new();
        p.insert("bx".to_string(), Value::Int(bx));
        p.insert("bn".to_string(), Value::Str(bn.to_string()));
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [mk_b(5, "p"), mk_b(7, "q"), mk_b(5, "r")];
    let mk_c = |cx: i64, cn: &str| {
        let mut p = BTreeMap::new();
        p.insert("cx".to_string(), Value::Int(cx));
        p.insert("cn".to_string(), Value::Str(cn.to_string()));
        g.create_node(&["Cc".into()], &p).expect("c")
    };
    let c = [mk_c(1, "m"), mk_c(2, "n"), mk_c(1, "o")];

    // T between Aa and Bb — mixed directions (a0 reaches b0,b1 OUT and b2 IN).
    for (s, t) in [(0usize, 0usize), (0, 1)] {
        g.create_rel(a[s], "T", b[t], &BTreeMap::new()).expect("T");
    }
    g.create_rel(b[2], "T", a[0], &BTreeMap::new()).expect("T"); // b2 -> a0 (IN)
    g.create_rel(a[1], "T", b[0], &BTreeMap::new()).expect("T"); // a1 -> b0 (OUT)

    // T between Bb and Cc — mixed directions.
    g.create_rel(b[0], "T", c[0], &BTreeMap::new()).expect("T"); // b0 -> c0
    g.create_rel(c[1], "T", b[0], &BTreeMap::new()).expect("T"); // c1 -> b0 (IN of b0)
    g.create_rel(b[1], "T", c[2], &BTreeMap::new()).expect("T"); // b1 -> c2
    g.create_rel(b[2], "T", c[1], &BTreeMap::new()).expect("T"); // b2 -> c1

    // U directed, Aa->Bb and Bb->Cc, for the directed+undirected mix.
    for (s, t) in [(0usize, 0usize), (0, 2), (1, 1)] {
        g.create_rel(a[s], "U", b[t], &BTreeMap::new()).expect("U");
    }
    for (s, t) in [(0usize, 0usize), (1, 1), (2, 2)] {
        g.create_rel(b[s], "U", c[t], &BTreeMap::new()).expect("U");
    }
    g
}

/// A single Aa node `a0` whose FOUR undirected T-neighbours all TIE on `bx=5`,
/// split OUT{b0,b1} / IN{b2,b3} so the production order is non-trivial. bn is
/// distinct so a tie-group LIMIT cut is observable.
///   adjacent_slim(a0, Both, T) = OUT[b0,b1] ++ IN[b2,b3] = [b0,b1,b2,b3]
///   reversed (production order)  = [b3,b2,b1,b0] = bn [s,r,q,p]
fn gtie() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut p = BTreeMap::new();
    p.insert("ak".to_string(), Value::Int(1));
    let a0 = g.create_node(&["Aa".into()], &p).expect("a0");
    let mk_b = |bn: &str| {
        let mut p = BTreeMap::new();
        p.insert("bx".to_string(), Value::Int(5));
        p.insert("bn".to_string(), Value::Str(bn.to_string()));
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [mk_b("p"), mk_b("q"), mk_b("r"), mk_b("s")]; // b0..b3
    g.create_rel(a0, "T", b[0], &BTreeMap::new()).expect("T"); // OUT
    g.create_rel(a0, "T", b[1], &BTreeMap::new()).expect("T"); // OUT
    g.create_rel(b[2], "T", a0, &BTreeMap::new()).expect("T"); // IN
    g.create_rel(b[3], "T", a0, &BTreeMap::new()).expect("T"); // IN
    g
}

/// A self-loop fixture. `L{k}` nodes; n0 carries an undirected self-loop plus
/// ordinary edges, so `adjacent_slim(n0, Both)` must offer the self-loop ONCE
/// (the O side; the I side's `peer==n0` entry is skipped). Both the pipeline and
/// `run_streaming` read the SAME `adjacent_slim`, so the dedup is shared.
///   L: n0(0) n1(1) n2(2)
///   T: n0->n0 (self), n0->n1, n2->n0
fn gself() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |k: i64| {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(k));
        g.create_node(&["Ll".into()], &p).expect("n")
    };
    let n = [mk(0), mk(1), mk(2)];
    g.create_rel(n[0], "T", n[0], &BTreeMap::new())
        .expect("self");
    g.create_rel(n[0], "T", n[1], &BTreeMap::new())
        .expect("t01");
    g.create_rel(n[2], "T", n[0], &BTreeMap::new())
        .expect("t20");
    g
}

/// A SINGLE undirected edge n0->n1. `(x)-[:T]-(y)-[:T]-(x)` would have to reuse
/// that one edge to close, so relationship isomorphism forbids it and the result
/// is EMPTY — the reuse the oracle also drops.
fn gsingle() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |k: i64| {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(k));
        g.create_node(&["Ll".into()], &p).expect("n")
    };
    let n = [mk(0), mk(1)];
    g.create_rel(n[0], "T", n[1], &BTreeMap::new())
        .expect("t01");
    g
}

/// TWO PARALLEL undirected edges n0->n1 (distinct rel ids). Now `(x)-[:T]-(y)-
/// [:T]-(x)` CAN close over the OTHER edge (isomorphism forbids only reusing the
/// SAME one), so the result is NON-empty — a non-vacuous counterpart to
/// `gsingle`.
fn gpar() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |k: i64| {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(k));
        g.create_node(&["Ll".into()], &p).expect("n")
    };
    let n = [mk(0), mk(1)];
    g.create_rel(n[0], "T", n[1], &BTreeMap::new())
        .expect("r_a");
    g.create_rel(n[0], "T", n[1], &BTreeMap::new())
        .expect("r_b");
    g
}

/// An undirected-close triangle. T1/T2 are directed; T3 (the CLOSING hop) is
/// queried undirected and its edges are mixed-direction so the close exercises
/// both legs.
///   Xx{xk}  x0(1)
///   Yy{yv,yn}  y0(5,p) y1(5,q)
///   Zz{zw,zn}  z0(1,m)
///   T1 x0->y0, x0->y1 ; T2 x0->z0 ; T3 z0->y0, y1->z0
fn gtri() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut xp = BTreeMap::new();
    xp.insert("xk".to_string(), Value::Int(1));
    let x0 = g.create_node(&["Xx".into()], &xp).expect("x0");
    let mk_y = |yv: i64, yn: &str| {
        let mut p = BTreeMap::new();
        p.insert("yv".to_string(), Value::Int(yv));
        p.insert("yn".to_string(), Value::Str(yn.to_string()));
        g.create_node(&["Yy".into()], &p).expect("y")
    };
    let y = [mk_y(5, "p"), mk_y(5, "q")];
    let mut zp = BTreeMap::new();
    zp.insert("zw".to_string(), Value::Int(1));
    zp.insert("zn".to_string(), Value::Str("m".to_string()));
    let z0 = g.create_node(&["Zz".into()], &zp).expect("z0");
    g.create_rel(x0, "T1", y[0], &BTreeMap::new()).expect("T1");
    g.create_rel(x0, "T1", y[1], &BTreeMap::new()).expect("T1");
    g.create_rel(x0, "T2", z0, &BTreeMap::new()).expect("T2");
    g.create_rel(z0, "T3", y[0], &BTreeMap::new()).expect("T3"); // z0 -> y0
    g.create_rel(y[1], "T3", z0, &BTreeMap::new()).expect("T3"); // y1 -> z0 (IN of z0)
    g
}

// ─── Harness (identical to the other pipeline_*.rs suites) ────────────────────

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON and the general path OFF; return both.
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

fn fired(g: &Graph, src: &str, counter: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(counter).copied() == Some(1)
}

/// Whether the non-aggregate pipeline (core / semijoin) fired for `src`.
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    fired(g, src, "interp.pipeline hop runs")
}

/// Whether the group-by-aggregate pipeline fired for `src`.
fn aggregate_fired(g: &Graph, src: &str) -> bool {
    fired(g, src, "interp.pipeline aggregate runs")
}

/// Whether the OPTIONAL left-join pipeline fired for `src`.
fn opt_fired(g: &Graph, src: &str) -> bool {
    fired(g, src, "interp.pipeline optional runs")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(t: &str) -> Value {
    Value::Str(t.to_string())
}

// ─── Single hop ──────────────────────────────────────────────────────────────

/// A single undirected hop across shapes: plain projection, ORDER BY the near /
/// far var, DESC, SKIP/LIMIT. ON must equal OFF byte-for-byte and the pipeline
/// must fire.
#[test]
fn undirected_single_hop_matches_general_across_shapes() {
    let g = gu();
    let cases: &[&str] = &[
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn",
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 100",
        // ORDER BY the FAR (undirected-reached) var.
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bx AS bx, b.bn AS bn ORDER BY b.bx, b.bn, a.ak LIMIT 100",
        // DESC over the far var.
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn ORDER BY b.bx DESC, b.bn DESC, a.ak DESC LIMIT 100",
        // SKIP + LIMIT over the pure production order (no ORDER BY key on bn).
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak SKIP 1 LIMIT 3",
        // The reversed textual form is the SAME undirected hop.
        "MATCH (b:Bb)-[:T]-(a:Aa) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 100",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected single-hop disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 5"
        ),
        "an undirected single hop is pipeline-only — it must fire"
    );
}

/// THE TIE TEST + CANARY TARGET. `a0`'s four T-neighbours all tie on `bx=5`, so
/// which rows `ORDER BY b.bx LIMIT k` keeps is decided PURELY by the undirected
/// production order [b3,b2,b1,b0] = bn [s,r,q,p]. ON must equal OFF at every
/// LIMIT, and the exact prefix is pinned — neutralising the `expand` `.rev()`
/// (or reordering the `Dir::Both` sides) changes the kept rows and fails this.
#[test]
fn undirected_single_hop_tie_group_resolves_like_general() {
    let g = gtie();
    let expect = [s("s"), s("r"), s("q"), s("p")];
    for lim in 1..=5usize {
        let src = format!("MATCH (a:Aa)-[:T]-(b:Bb) RETURN b.bn AS bn ORDER BY b.bx LIMIT {lim}");
        let (on, off) = both(&g, &src, BTreeMap::new());
        assert_eq!(on, off, "undirected tie disagrees at LIMIT {lim}: `{src}`");
        let want: Vec<Vec<Value>> = expect
            .iter()
            .take(lim.min(4))
            .map(|v| vec![v.clone()])
            .collect();
        assert_eq!(on, want, "undirected tie prefix wrong at LIMIT {lim}");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb) RETURN b.bn AS bn ORDER BY b.bx LIMIT 2"
        ),
        "the undirected tie shape must fire (non-vacuous canary)"
    );
}

/// WHERE over the undirected ENDPOINT var (`b`), vectorised by the pipeline's
/// own filter operator; and over the start var, for good measure.
#[test]
fn undirected_where_over_endpoint() {
    let g = gu();
    for src in [
        "MATCH (a:Aa)-[:T]-(b:Bb) WHERE b.bx = 5 RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 100",
        "MATCH (a:Aa)-[:T]-(b:Bb) WHERE b.bx > 5 RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 100",
        "MATCH (a:Aa)-[:T]-(b:Bb) WHERE a.ak = 1 RETURN b.bn AS bn ORDER BY b.bx, b.bn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected WHERE disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb) WHERE b.bx = 5 RETURN b.bn AS bn ORDER BY b.bn LIMIT 3"
        ),
        "an endpoint WHERE over an undirected hop must fire"
    );
}

// ─── Multi hop and mixed direction ───────────────────────────────────────────

/// A 2-hop UNDIRECTED-then-UNDIRECTED path. The nested production order (seed
/// ascending × reverse-adjacency at each hop, both `Dir::Both`) must match the
/// general path across projection / ORDER BY / SKIP-LIMIT.
#[test]
fn undirected_multi_hop_matches_general() {
    let g = gu();
    for src in [
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY a.ak SKIP 1 LIMIT 4",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) WHERE b.bx = 5 RETURN b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected multi-hop disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, a.ak LIMIT 3"
        ),
        "a 2-hop undirected path must fire"
    );
}

/// A chain that MIXES a directed and an undirected hop, in both orders. Each hop
/// maps independently (`Dir::Out`/`Dir::In`/`Dir::Both`), so the mix is the same
/// nested production order as the general path.
#[test]
fn undirected_mixed_direction_chain_matches_general() {
    let g = gu();
    for src in [
        // undirected T, then directed U.
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:U]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100",
        // directed U, then undirected T.
        "MATCH (a:Aa)-[:U]->(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100",
        // directed IN, then undirected.
        "MATCH (b:Bb)<-[:U]-(a:Aa), (b)-[:T]-(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY a.ak, b.bn, c.cn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "mixed-direction chain disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb)-[:U]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY a.ak, c.cn LIMIT 3"
        ),
        "a directed+undirected mixed chain must fire"
    );
}

// ─── Self-loop dedup ─────────────────────────────────────────────────────────

/// The OUT/IN self-loop dedup: `adjacent_slim(n0, Both)` offers n0's self-loop
/// ONCE (O side), so `(n0)-[:T]-(n0)` is a single row, and a 2-hop walk cannot
/// re-traverse the self-loop. Both paths read the same `adjacent_slim`, so they
/// agree; the row `(0,0)` appears EXACTLY once.
#[test]
fn undirected_self_loop_dedup_matches_general() {
    let g = gself();
    for src in [
        "MATCH (x:Ll)-[:T]-(y:Ll) RETURN x.k AS xk, y.k AS yk ORDER BY x.k, y.k",
        "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(z:Ll) RETURN x.k AS xk, y.k AS yk, z.k AS zk ORDER BY x.k, y.k, z.k LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "self-loop disagree: `{src}`");
    }
    // `(n0)-[:T]-(n0)` — the self-loop — is present exactly once (not doubled).
    let (on, _) = both(
        &g,
        "MATCH (x:Ll)-[:T]-(y:Ll) RETURN x.k AS xk, y.k AS yk",
        BTreeMap::new(),
    );
    let loops = on.iter().filter(|r| **r == vec![i(0), i(0)]).count();
    assert_eq!(
        loops, 1,
        "the undirected self-loop must appear exactly once"
    );
    // A 2-hop walk cannot reuse the self-loop: `(0)-[self]-(0)-[self]-(0)` absent.
    let (two, _) = both(
        &g,
        "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(z:Ll) RETURN x.k AS xk, y.k AS yk, z.k AS zk",
        BTreeMap::new(),
    );
    assert!(
        !two.contains(&vec![i(0), i(0), i(0)]),
        "reusing the self-loop twice must be dropped"
    );
    assert!(
        pipeline_fired(&g, "MATCH (x:Ll)-[:T]-(y:Ll) RETURN x.k AS xk, y.k AS yk"),
        "the self-loop shape must fire"
    );
}

// ─── Relationship isomorphism (one undirected edge) ──────────────────────────

/// A walk that could REUSE the one undirected edge. With a SINGLE edge n0-n1,
/// `(x)-[:T]-(y)-[:T]-(x)` can only close by re-traversing it, which isomorphism
/// forbids — so the result is EMPTY, exactly as the oracle. With TWO PARALLEL
/// edges the close is licensed over the OTHER edge, so it is NON-empty. Both
/// match the general path, and the pipeline fires in both (the empty result is a
/// genuine firing, not a decline).
#[test]
fn undirected_relationship_isomorphism_drops_reuse() {
    let close =
        "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(x) RETURN x.k AS xk, y.k AS yk ORDER BY x.k, y.k LIMIT 100";

    let g1 = gsingle();
    let (on1, off1) = both(&g1, close, BTreeMap::new());
    assert_eq!(on1, off1, "single-edge reuse close disagree");
    assert!(
        on1.is_empty(),
        "reusing the one undirected edge to close must be dropped: {on1:?}"
    );
    assert!(
        pipeline_fired(&g1, close),
        "the empty close must FIRE, not decline"
    );

    let g2 = gpar();
    let (on2, off2) = both(&g2, close, BTreeMap::new());
    assert_eq!(on2, off2, "parallel-edge reuse close disagree");
    assert!(
        !on2.is_empty(),
        "a parallel undirected edge licenses the close — it must be non-empty"
    );
    assert!(pipeline_fired(&g2, close), "the parallel close must fire");

    // The OPEN 2-hop form also matches (a reuse-free walk survives on both).
    let open = "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(z:Ll) RETURN x.k AS xk, y.k AS yk, z.k AS zk ORDER BY x.k, y.k, z.k LIMIT 100";
    let (o1, f1) = both(&g1, open, BTreeMap::new());
    assert_eq!(o1, f1, "single-edge open 2-hop disagree");
    assert!(
        o1.is_empty(),
        "one edge cannot make a reuse-free 2-hop walk: {o1:?}"
    );
}

// ─── Semijoin close, aggregate, optional ─────────────────────────────────────

/// An undirected CLOSING hop (semijoin). `(x)-[:T1]->(y), (x)-[:T2]->(z),
/// (z)-[:T3]-(y)` closes onto the bound `y` over an undirected T3 whose edges run
/// both ways, so both legs of the close are exercised. ON == OFF across shapes.
#[test]
fn undirected_semijoin_close_matches_general() {
    let g = gtri();
    for src in [
        "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz), (z)-[:T3]-(y) RETURN x.xk AS xk, y.yn AS yn, z.zn AS zn ORDER BY x.xk, y.yn, z.zn LIMIT 100",
        // The single-path form: the final hop of a 2-hop path closes onto y.
        "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz)-[:T3]-(y) RETURN x.xk AS xk, y.yn AS yn, z.zn AS zn ORDER BY x.xk, y.yn, z.zn LIMIT 100",
        // WHERE over the closed-onto var.
        "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz), (z)-[:T3]-(y) WHERE y.yv = 5 RETURN y.yn AS yn ORDER BY y.yn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected semijoin disagree: `{src}`");
    }
    // Both triangles close (z0->y0 out-leg, y1->z0 in-leg) — non-vacuous.
    let (on, _) = both(
        &g,
        "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz), (z)-[:T3]-(y) RETURN y.yn AS yn ORDER BY y.yn",
        BTreeMap::new(),
    );
    assert_eq!(
        on,
        vec![vec![s("p")], vec![s("q")]],
        "both undirected closes"
    );
    assert!(
        pipeline_fired(
            &g,
            "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz), (z)-[:T3]-(y) RETURN y.yn AS yn"
        ),
        "an undirected semijoin close must fire"
    );
}

/// An undirected hop feeding a GROUP-BY-COUNT and a COLLECT — the aggregate path
/// over the undirected chunk folds byte-identically to the general path.
#[test]
fn undirected_feeds_group_by_count() {
    let g = gu();
    for src in [
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, count(b) AS n ORDER BY ak",
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, collect(b.bn) AS bns ORDER BY ak",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected aggregate disagree: `{src}`");
    }
    assert!(
        aggregate_fired(
            &g,
            "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, count(*) AS n ORDER BY ak"
        ),
        "an undirected group-by-count must fire the aggregate pipeline"
    );
}

/// An undirected hop under OPTIONAL MATCH: the left join runs the undirected
/// expand per outer row (the outer var is bound, so no reversal), and an outer
/// row with no undirected neighbour keeps its null-fill (count 0). ON == OFF.
#[test]
fn undirected_optional_matches_general() {
    let g = gu();
    for src in [
        "MATCH (a:Aa) OPTIONAL MATCH (a)-[:T]-(b:Bb) RETURN a.ak AS ak, count(b) AS c ORDER BY ak",
        "MATCH (a:Aa) OPTIONAL MATCH (a)-[:T]-(b:Bb) RETURN a.ak AS ak, count(*) AS cs, count(b) AS cb ORDER BY ak",
        "MATCH (a:Aa) OPTIONAL MATCH (a)-[:T]-(b:Bb) RETURN a.ak AS ak, collect(b.bn) AS bns ORDER BY ak",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "undirected OPTIONAL disagree: `{src}`");
    }
    // a2 has no T neighbour — present with count 0 (null-fill), not absent.
    let (on, _) = both(
        &g,
        "MATCH (a:Aa) OPTIONAL MATCH (a)-[:T]-(b:Bb) RETURN a.ak AS ak, count(b) AS c ORDER BY ak",
        BTreeMap::new(),
    );
    assert!(
        on.contains(&vec![i(3), i(0)]),
        "the edge-less outer a2 must be present with count 0: {on:?}"
    );
    assert!(
        opt_fired(
            &g,
            "MATCH (a:Aa) OPTIONAL MATCH (a)-[:T]-(b:Bb) RETURN a.ak AS ak, count(b) AS c ORDER BY ak"
        ),
        "an undirected OPTIONAL must fire the left-join pipeline"
    );
}

// ─── Count fold over undirected hops ─────────────────────────────────────────

/// The COUNT-FOLD triple (fold ON / fold OFF / general) for `src`, asserting
/// agreement and whether the fold fired.
fn fold_triple(g: &Graph, src: &str, fires: bool) -> Vec<Vec<Value>> {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let on = rows(g, src, BTreeMap::new());
    engram_graph::pipeline::set_count_fold(false);
    let fold_off = rows(g, src, BTreeMap::new());
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(false);
    let general = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(true);
    assert_eq!(on, general, "fold ON vs general disagree: `{src}`");
    assert_eq!(fold_off, general, "fold OFF vs general disagree: `{src}`");
    // WHETHER the fold fires is a property of the pattern AS WRITTEN, so it is
    // read with the COUNT-ONLY JOIN REORDER (operator C) held off: that pass
    // exists to rewrite a `count(*)`-only pattern into an equivalent one the
    // fold CAN take, and `pipeline_count_reorder.rs` pins it. The row agreement
    // above is still taken with every default on.
    engram_graph::pipeline::set_count_only_reorder(false);
    let did_fire = fired(g, src, "interp.pipeline count fold");
    engram_graph::pipeline::set_count_only_reorder(true);
    assert_eq!(did_fire, fires, "count fold firing: `{src}`");
    on
}

/// `Dir::Both` inside the fold: the O-then-I visit with the I-side self-loop
/// skipped is `adjacent_slim_visit`'s rule, shared by `expand` and the fold's
/// `adjacent_slim_for_each`, so an undirected `count(*)` chain folds
/// byte-identically — mixed-direction hops, a directed/undirected mix, the
/// self-loop fixture, and keyed by the seed.
#[test]
fn undirected_count_folds_like_general() {
    let g = gu();
    for src in [
        "MATCH (a:Aa)-[:T]-(b:Bb) RETURN count(*) AS n",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN count(*) AS n",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:U]->(c:Cc) RETURN count(*) AS n",
        "MATCH (a:Aa)-[:U]->(b:Bb)-[:T]-(c:Cc) RETURN count(*) AS n",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        "MATCH (a:Aa)-[:T]-(b:Bb)-[:T]-(c:Cc) WHERE a.ak = 1 RETURN count(*) AS n",
    ] {
        fold_triple(&g, src, true);
    }
    let gs = gself();
    for src in [
        "MATCH (x:Ll)-[:T]-(y:Ll) RETURN count(*) AS n",
        "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(z:Ll) RETURN count(*) AS n",
        "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(z:Ll) RETURN x.k AS xk, count(*) AS n ORDER BY xk",
    ] {
        fold_triple(&gs, src, true);
    }
    // The self-loop is offered ONCE under Both: n0's undirected degree is 3
    // (self, n1, n2), so the 1-hop count over {n0,n1,n2} is 3 + 1 + 1 = 5.
    let on = fold_triple(&gs, "MATCH (x:Ll)-[:T]-(y:Ll) RETURN count(*) AS n", true);
    assert_eq!(on, vec![vec![i(5)]], "undirected self-loop counted once");
}

/// The undirected CLOSE ONTO THE SEED inside a fold, over one edge and over a
/// parallel pair: rel-iso forbids reusing the one edge (count 0) but licenses
/// the other of a parallel pair (count 2: each direction of the pair) — the
/// same drop/keep `undirected_relationship_isomorphism_drops_reuse` pins on
/// the row form, now counted inside the fold.
#[test]
fn undirected_close_onto_seed_folds_with_rel_iso() {
    let close = "MATCH (x:Ll)-[:T]-(y:Ll)-[:T]-(x) RETURN count(*) AS n";
    let on1 = fold_triple(&gsingle(), close, true);
    assert_eq!(on1, vec![vec![i(0)]], "one edge cannot close: zero row");
    let on2 = fold_triple(&gpar(), close, true);
    assert_eq!(
        on2,
        vec![vec![i(4)]],
        "a parallel pair closes over the other edge, from either end, either way"
    );
    // The undirected triangle close (a sibling close) is a JOIN only until the
    // close's TARGET materialises: `plan_count_fold` un-folds `y` rather than
    // the level, and the seed's T2 fan-out closing onto a bound `y` IS a tree,
    // so the fold FIRES. The closing multiplicity is `edge_count_slim`'s over
    // `Dir::Both` — the same call the materialised counted close makes — and
    // `fold_triple` checks ON == OFF == general, which is the evidence.
    fold_triple(
        &gtri(),
        "MATCH (x:Xx)-[:T1]->(y:Yy), (x)-[:T2]->(z:Zz), (z)-[:T3]-(y) RETURN count(*) AS n",
        true,
    );
    // The same triangle with `y` bound AFTER the fold's root: materialising it
    // cannot satisfy the position rule, so the level un-folds and the count is
    // served by the materialised undirected semijoin. Still agrees.
    fold_triple(
        &gtri(),
        "MATCH (x:Xx)-[:T2]->(z:Zz), (x)-[:T1]->(y:Yy), (z)-[:T3]-(y) RETURN count(*) AS n",
        false,
    );
}

/// Non-vacuity: the accepted undirected shapes FIRE with columnar ON and do NOT
/// fire with columnar OFF, so every differential above is a real ON-vs-OFF check.
#[test]
fn undirected_fires_only_when_columnar_on() {
    let g = gu();
    let src = "MATCH (a:Aa)-[:T]-(b:Bb) RETURN a.ak AS ak, b.bn AS bn ORDER BY a.ak, b.bn LIMIT 5";
    assert!(pipeline_fired(&g, src), "must fire with columnar ON");
    g.set_columnar_scans(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src, BTreeMap::new()));
    assert_eq!(
        trace.counters().get("interp.pipeline hop runs").copied(),
        None,
        "must NOT fire with columnar OFF"
    );
    g.set_columnar_scans(true);
}
