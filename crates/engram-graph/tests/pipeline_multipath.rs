#![allow(non_snake_case)]
//! Differential tests for the MULTI-PATH columnar pipeline (Phase 4a of
//! `pipeline::plan_and_run_columnar`): `MATCH <path1>, <path2>, …` where each
//! subsequent path re-roots at an ALREADY-BOUND var and its hops introduce NEW
//! end vars — a chain expressed as multiple paths, or a BRANCH. The contract is
//! `pipeline_core`'s: for every accepted shape, `set_columnar_scans(true)` (the
//! columnar expand chain) must equal `set_columnar_scans(false)` (the per-tuple
//! `run_streaming` path) — the full ROW SET *and its order*, byte-for-byte — and
//! every declined shape falls back and still agrees.
//!
//! THE load-bearing fact under test is the MULTI-PATH production order. For a
//! branch `(a)-[:T1]->(b), (a)-[:T2]->(c)`, `run_streaming` nests
//! per-a: per-b-reversed: per-c-reversed. The pipeline reproduces it because the
//! branch expand sources from `a` and walks the existing (a,b) rows in order
//! (outer) × reverse `a`-adjacency (inner), appending `c` innermost. The
//! TIE-GROUP test's LIMIT-kept rows are decided purely by that order; dropping
//! the pipeline's `.rev()` diverges it (the canary), which also proves it fires.
//! RELATIONSHIP ISOMORPHISM is PER-PATH: `run_streaming` re-seeds `Partial.used`
//! per path, so a self-loop reused ACROSS two 1-hop paths is KEPT (the
//! `iso_cross_path_*` tests), while a reuse WITHIN a later multi-hop path is
//! dropped — both matched exactly.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Aa{ak} -T1-> Bb{bx,bn}; Aa -T2-> Cc{cx (ties+null), cn (distinguishing)}
/// (the BRANCH from a); Bb -T3-> Cc (the CHAIN hop); Cc -T4-> Dd{dk} (the third
/// path). Edge creation order is load-bearing (it drives reverse-adjacency): a0
/// fans T1->{b0,b1}, T2->{c0,c1,c2} (the cx=7 tie group), so the branch
/// production order is non-trivial at BOTH inner levels.
fn gp() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Aa".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2)];
    let mk_b = |bx: i64, bn: &str| {
        let mut p = BTreeMap::new();
        p.insert("bx".to_string(), Value::Int(bx));
        p.insert("bn".to_string(), Value::Str(bn.to_string()));
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [mk_b(50, "p"), mk_b(50, "q"), mk_b(10, "a")]; // b0,b1,b2
    let mk_c = |cx: Option<i64>, cn: &str| {
        let mut p = BTreeMap::new();
        if let Some(v) = cx {
            p.insert("cx".to_string(), Value::Int(v));
        }
        p.insert("cn".to_string(), Value::Str(cn.to_string()));
        g.create_node(&["Cc".into()], &p).expect("c")
    };
    let c = [
        mk_c(Some(7), "m"), // c0 — tie group cx=7
        mk_c(Some(7), "n"), // c1 — tie group cx=7
        mk_c(Some(7), "o"), // c2 — tie group cx=7
        mk_c(Some(3), "d"), // c3
        mk_c(None, "e"),    // c4 — cx NULL
    ];
    let mk_d = |dk: i64| {
        let mut p = BTreeMap::new();
        p.insert("dk".to_string(), Value::Int(dk));
        g.create_node(&["Dd".into()], &p).expect("d")
    };
    let d = [mk_d(100), mk_d(200)];

    for (s, t) in [(0, 0), (0, 1), (1, 2)] {
        g.create_rel(a[s], "T1", b[t], &BTreeMap::new())
            .expect("T1");
    }
    // BRANCH edges a->c: a0 fans to the cx=7 tie group in c0,c1,c2 order.
    for (s, t) in [(0, 0), (0, 1), (0, 2), (1, 0)] {
        g.create_rel(a[s], "T2", c[t], &BTreeMap::new())
            .expect("T2");
    }
    // CHAIN edges b->c.
    for (s, t) in [(0, 0), (0, 1), (1, 2), (2, 3)] {
        g.create_rel(b[s], "T3", c[t], &BTreeMap::new())
            .expect("T3");
    }
    // Third-path edges c->d.
    for (s, t) in [(0, 0), (0, 1), (2, 0)] {
        g.create_rel(c[s], "T4", d[t], &BTreeMap::new())
            .expect("T4");
    }
    g
}

/// Nn{nk} with a self-loop and a back-edge: two 1-hop paths sharing a var can
/// REUSE the self-loop `r_self` (allowed across paths — `run_streaming` re-seeds
/// `used` per path); a single 2-hop path forbids it (within-path isomorphism).
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

/// A two-path CHAIN `(a)-[:T1]->(b), (b)-[:T3]->(c)` must equal the single-path
/// 2-hop `(a)-[:T1]->(b)-[:T3]->(c)`, both against the general path and against
/// each other, row-for-row and in order (distinct rel types, so isomorphism
/// never interacts).
#[test]
fn multipath_chain_equals_single_path_two_hop() {
    let g = gp();
    let pairs: &[(&str, &str)] = &[
        (
            "MATCH (a:Aa)-[:T1]->(b:Bb), (b)-[:T3]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T3]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
        ),
        (
            // No ORDER BY — pure production order.
            "MATCH (a:Aa)-[:T1]->(b:Bb), (b)-[:T3]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T3]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
        ),
    ];
    for (multi, single) in pairs {
        let (m_on, m_off) = both(&g, multi, BTreeMap::new());
        assert_eq!(
            m_on, m_off,
            "multipath chain vs general disagree: `{multi}`"
        );
        let (s_on, s_off) = both(&g, single, BTreeMap::new());
        assert_eq!(s_on, s_off, "single 2-hop vs general disagree: `{single}`");
        assert_eq!(m_on, s_on, "two-path chain != single 2-hop: `{multi}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (b)-[:T3]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn"
        ),
        "a two-path chain is a pipeline shape — it must fire"
    );
}

/// A BRANCH `(a)-[:T1]->(b), (a)-[:T2]->(c)` over many projections/orders — the
/// full row set and order must equal the general path.
#[test]
fn multipath_branch_matches_general_across_shapes() {
    let g = gp();
    let cases: &[&str] = &[
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx, c.cn, b.bn LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
        // ORDER BY the branch far var (c), DESC — NULLs first.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN b.bn AS bn, c.cx AS cx ORDER BY c.cx DESC, b.bn LIMIT 100",
        // SKIP + LIMIT over production order (no ORDER BY).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn SKIP 2 LIMIT 3",
        // Incoming leg on the branch (drive c from b? no — re-root at a, in dir).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY a.ak, c.cn LIMIT 100",
        // An unlabelled branch end.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c) RETURN c.cn AS cn ORDER BY c.cn LIMIT 100",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "branch vs general disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn"
        ),
        "a branch is a pipeline shape — it must fire"
    );
}

/// THE BRANCH TIE TEST + CANARY TARGET. Under `WHERE c.cx = 7` every surviving
/// branch row shares the sort key, so the rows LIMIT keeps are decided PURELY by
/// the multi-path production order (a ascending × reverse T1 × reverse T2).
/// `b.bn`/`c.cn` distinguish the tied members, so a wrong order (dropping either
/// `.rev()`) changes the projected rows and ON != OFF. The exact sequence is
/// asserted at LIMIT 100 to lock the nesting.
#[test]
fn multipath_branch_tie_group_resolves_like_general() {
    let g = gp();
    for lim in 1..=7 {
        let src = format!(
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WHERE c.cx = 7 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx LIMIT {lim}"
        );
        let (on, off) = both(&g, &src, BTreeMap::new());
        assert_eq!(on, off, "branch tie disagrees at LIMIT {lim}: `{src}`");
        assert_eq!(on.len(), lim.min(7), "branch tie size wrong at LIMIT {lim}");
    }
    // Exact production order: per a asc, per b in rev(T1), per c in rev(T2).
    let (on, _) = both(
        &g,
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WHERE c.cx = 7 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx LIMIT 100",
        BTreeMap::new(),
    );
    let want = vec![
        vec![
            Value::Int(1),
            Value::Str("q".into()),
            Value::Str("o".into()),
        ],
        vec![
            Value::Int(1),
            Value::Str("q".into()),
            Value::Str("n".into()),
        ],
        vec![
            Value::Int(1),
            Value::Str("q".into()),
            Value::Str("m".into()),
        ],
        vec![
            Value::Int(1),
            Value::Str("p".into()),
            Value::Str("o".into()),
        ],
        vec![
            Value::Int(1),
            Value::Str("p".into()),
            Value::Str("n".into()),
        ],
        vec![
            Value::Int(1),
            Value::Str("p".into()),
            Value::Str("m".into()),
        ],
        vec![
            Value::Int(2),
            Value::Str("a".into()),
            Value::Str("m".into()),
        ],
    ];
    assert_eq!(on, want, "branch nested production order wrong");
}

/// WHERE over EACH bound var of a branch — the pipeline's own filter operator
/// over any of a / b / c.
#[test]
fn multipath_where_over_each_bound_var() {
    let g = gp();
    for src in [
        // WHERE on the first var (a).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WHERE a.ak > 1 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cn, b.bn LIMIT 100",
        // WHERE on the first-path end (b).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WHERE b.bx = 50 RETURN b.bn AS bn, c.cn AS cn ORDER BY b.bn, c.cn LIMIT 100",
        // WHERE on the branch end (c).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WHERE c.cx IS NOT NULL AND c.cx < 7 RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cn LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "branch WHERE disagree: `{src}`");
    }
}

/// A branch feeding a GROUP-BY-COUNT and a COLLECT — the aggregate path over the
/// multi-path chunk. Node-identity grouping, value grouping and a
/// production-ordered collect all fold byte-identically to `run_streaming`.
#[test]
fn multipath_branch_aggregate() {
    let g = gp();
    for src in [
        // Value-key group-by-count.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        // Node-identity group-by (the fast path).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) WITH a, count(*) AS n RETURN a.ak AS ak, n ORDER BY ak",
        // Production-ordered collect over the branch far var.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, collect(c.cn) AS cns ORDER BY ak",
        // Global aggregate over the whole branch.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN count(*) AS n, count(DISTINCT c) AS dc",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "branch aggregate disagree: `{src}`");
    }
    assert!(
        aggregate_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc) RETURN a.ak AS ak, count(*) AS n ORDER BY ak"
        ),
        "a branch group-by-count must fire the aggregate pipeline"
    );
}

/// THREE paths: `(a)-[:T1]->(b), (a)-[:T2]->(c), (c)-[:T4]->(d)` — the third
/// path re-roots at `c` (bound by the second). Full row set + order equal the
/// general path.
#[test]
fn multipath_three_paths() {
    let g = gp();
    for src in [
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T4]->(d:Dd) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn, d.dk AS dk ORDER BY d.dk, c.cn, b.bn, a.ak LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T4]->(d:Dd) RETURN a.ak AS ak, c.cn AS cn, d.dk AS dk",
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T4]->(d:Dd) WHERE c.cx = 7 RETURN c.cn AS cn, d.dk AS dk ORDER BY c.cn, d.dk LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "three-path disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2]->(c:Cc), (c)-[:T4]->(d:Dd) RETURN a.ak AS ak, d.dk AS dk"
        ),
        "a three-path pattern must fire"
    );
}

/// Relationship isomorphism is PER-PATH. Two 1-hop paths sharing `y` may REUSE
/// the self-loop across the path boundary (`run_streaming` re-seeds `used` per
/// path), so `(0)-[r_self]->(0), (0)-[r_self]->(0)` — the row (0,0,0) — is KEPT,
/// whereas the single-path 2-hop forbids it. The pipeline must match BOTH.
#[test]
fn iso_cross_path_reuse_is_kept() {
    let g = giso();
    let multi = "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk ORDER BY x.nk, y.nk, z.nk LIMIT 100";
    let (m_on, m_off) = both(&g, multi, BTreeMap::new());
    assert_eq!(m_on, m_off, "cross-path iso disagree: `{multi}`");
    assert!(
        m_on.contains(&vec![Value::Int(0), Value::Int(0), Value::Int(0)]),
        "cross-path reuse of the self-loop must be KEPT (per-path `used`)"
    );
    // The single-path 2-hop FORBIDS the same reuse (within-path isomorphism).
    let single = "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk ORDER BY x.nk, y.nk, z.nk LIMIT 100";
    let (s_on, s_off) = both(&g, single, BTreeMap::new());
    assert_eq!(s_on, s_off, "single 2-hop iso disagree: `{single}`");
    assert!(
        !s_on.contains(&vec![Value::Int(0), Value::Int(0), Value::Int(0)]),
        "within a single path the self-loop reuse must be dropped"
    );
    assert!(
        pipeline_fired(&g, multi),
        "the cross-path iso shape must run through the pipeline"
    );
}

/// Isomorphism WITHIN a later multi-hop path is still enforced: path 2 is a
/// 2-hop `(y)-[:R]->(z)-[:R]->(w)`; a walk reusing its own first rel is dropped,
/// exactly as the general path does — and the reset means path 2 never sees
/// path 1's rel.
#[test]
fn iso_within_later_path_is_enforced() {
    let g = giso();
    for src in [
        "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(z:Nn)-[:R]->(w:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk, w.nk AS wk ORDER BY x.nk, y.nk, z.nk, w.nk LIMIT 100",
        "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(z:Nn)-[:R]->(w:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk, w.nk AS wk",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "within-later-path iso disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (x:Nn)-[:R]->(y:Nn), (y)-[:R]->(z:Nn)-[:R]->(w:Nn) RETURN x.nk AS xk, w.nk AS wk"
        ),
        "a later multi-hop path must fire through the pipeline"
    );
}

/// Every one of these is OUTSIDE the pipeline's expand/semijoin shapes (a FINAL
/// hop closing onto a bound var IS accepted as a semijoin in 4b1 — see
/// `pipeline_semijoin.rs`; a NON-final close is not): the pipeline DECLINES and
/// the general path answers identically (ON == OFF), and the pipeline did NOT
/// fire.
#[test]
fn multipath_declines_and_falls_back_identically() {
    let g = gp();
    let declines: &[&str] = &[
        // NON-final hop onto a bound var: `(a)-[:T2]->(c)` closes onto `c` but a
        // further hop follows, so it is NOT a path's final hop — only a FINAL
        // close is a semijoin (4b1); a mid-path close still declines.
        "MATCH (a:Aa)-[:T2]->(c:Cc), (c)-[:T4]->(c)-[:T4]->(d:Dd) RETURN d.dk AS dk ORDER BY d.dk LIMIT 5",
        // DISJOINT — the second path's start var is not bound (a cartesian).
        "MATCH (a:Aa)-[:T1]->(b:Bb), (c:Cc)-[:T4]->(d:Dd) RETURN a.ak AS ak, d.dk AS dk ORDER BY a.ak, d.dk LIMIT 100",
        // (A bound relationship variable on the second path is now ACCEPTED —
        // see `pipeline_relvar.rs`.)
        // A variable-length hop on the second path.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (b)-[:T3*1..2]->(c:Cc) RETURN c.cn AS cn ORDER BY c.cn LIMIT 5",
        // A relationship-property test on the second path.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (a)-[:T2 {w: 1}]->(c:Cc) RETURN c.cn AS cn ORDER BY c.cn LIMIT 5",
        // A second-path start restating a label the bound var lacks.
        "MATCH (a:Aa)-[:T1]->(b:Bb), (b:Cc)-[:T3]->(c:Cc) RETURN c.cn AS cn ORDER BY c.cn LIMIT 5",
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

/// The pipeline must still DECLINE the unrelated / census-style shapes it never
/// owned — a single-var scan and a hop aggregate over a single path — unchanged
/// by the multi-path generalisation (the digest must not move).
#[test]
fn multipath_leaves_census_shapes_declining() {
    let g = gp();
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
