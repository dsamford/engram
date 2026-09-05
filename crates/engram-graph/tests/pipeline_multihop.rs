#![allow(non_snake_case)]
//! Differential tests for the MULTI-hop columnar pipeline (Phase 2 of
//! `pipeline::plan_and_run_columnar`). The contract is the same as
//! `pipeline_core`: for every shape the pipeline accepts, running with
//! `set_columnar_scans(true)` (the columnar expand chain + native project/top-k)
//! must equal `set_columnar_scans(false)` (the per-tuple `run_streaming` path)
//! — the full ROW SET *and its order*, byte-for-byte — and for every shape it
//! declines, the general path answers and the two still agree.
//!
//! THE load-bearing fact under test is the MULTI-LEVEL production order: a 2-hop
//! path emits in NESTED reverse-adjacency (seed ascending; per seed its hop-1
//! neighbours reversed; per those, hop-2 neighbours reversed). The TIE-GROUP
//! test's LIMIT-kept rows are decided purely by that order — dropping the
//! pipeline's `.rev()` diverges it (the canary), which also proves the pipeline
//! fires. RELATIONSHIP ISOMORPHISM across hops is exercised by `iso_*` against a
//! graph with a self-loop and a back-edge: without the per-row `used` set the
//! columnar path would emit a reuse the streaming path forbids.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A 3-layer (plus a 4th for the 3-hop) directed graph:
/// Aa{ak} -T1-> Bb{bx int (ties+null), bn str (null)} -T2-> Cc{cx int
/// (ties+null), cn str (null)} -T3-> Dd{dk}. Edge creation order is load-bearing
/// (it drives reverse-adjacency), so the fan-outs are chosen so the nested
/// production order is non-trivial: a0 fans to b0,b1; b0 fans to c0,c1; b1 to
/// c2,c3 — four tied (cx=7 or 3) 2-hop rows whose kept set under LIMIT depends
/// on BOTH levels of reversal.
fn gm() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Aa".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3)];
    let mk_b = |bx: Option<i64>, bn: Option<&str>| {
        let mut p = BTreeMap::new();
        if let Some(v) = bx {
            p.insert("bx".to_string(), Value::Int(v));
        }
        if let Some(s) = bn {
            p.insert("bn".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some(50), Some("p")), // b0
        mk_b(Some(50), Some("q")), // b1
        mk_b(Some(10), Some("a")), // b2
        mk_b(None, Some("z")),     // b3 — bx NULL, no outgoing T2
        mk_b(Some(20), None),      // b4 — bn NULL
    ];
    let mk_c = |cx: Option<i64>, cn: Option<&str>| {
        let mut p = BTreeMap::new();
        if let Some(v) = cx {
            p.insert("cx".to_string(), Value::Int(v));
        }
        if let Some(s) = cn {
            p.insert("cn".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["Cc".into()], &p).expect("c")
    };
    let c = [
        mk_c(Some(7), Some("m")), // c0 — tie group cx=7
        mk_c(Some(7), Some("n")), // c1 — tie group cx=7
        mk_c(Some(7), Some("o")), // c2 — tie group cx=7
        mk_c(Some(3), Some("d")), // c3
        mk_c(None, Some("e")),    // c4 — cx NULL
        mk_c(Some(9), None),      // c5 — cn NULL
    ];
    let mk_d = |dk: i64| {
        let mut p = BTreeMap::new();
        p.insert("dk".to_string(), Value::Int(dk));
        g.create_node(&["Dd".into()], &p).expect("d")
    };
    let d = [mk_d(100), mk_d(200), mk_d(300)];

    for (s, t) in [(0, 0), (0, 1), (1, 2), (1, 3), (1, 4), (2, 0)] {
        g.create_rel(a[s], "T1", b[t], &BTreeMap::new())
            .expect("T1");
    }
    for (s, t) in [(0, 0), (0, 1), (1, 2), (1, 3), (2, 4), (4, 5)] {
        g.create_rel(b[s], "T2", c[t], &BTreeMap::new())
            .expect("T2");
    }
    for (s, t) in [(0, 0), (0, 1), (1, 2), (3, 0)] {
        g.create_rel(c[s], "T3", d[t], &BTreeMap::new())
            .expect("T3");
    }
    g
}

/// Nn{nk}, with a self-loop and a back-edge so a 2-hop `(x)-[:R]->(y)-[:R]->(z)`
/// has walks that would REUSE a relationship — the isomorphism check must drop
/// exactly those (e.g. `(n0)-[r_self]->(n0)-[r_self]->(n0)` is forbidden).
fn gi() -> Graph {
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

/// Whether the pipeline fired (produced the answer) for `src` with columnar ON.
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

#[test]
fn multihop_matches_general_across_shapes() {
    let g = gm();
    let cases: &[&str] = &[
        // 2-hop, ORDER BY the FAR var (c).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bx AS bx, c.cx AS cx, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 3",
        // 2-hop, ORDER BY the MIDDLE var (b).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN b.bx AS bx, c.cn AS cn ORDER BY b.bx, c.cn LIMIT 100",
        // 2-hop, ORDER BY the FIRST var (a).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY a.ak, c.cn LIMIT 100",
        // DESC over the far var — NULLs sort FIRST.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx, c.cn AS cn ORDER BY c.cx DESC, c.cn DESC LIMIT 100",
        // A NULL far-var sort key, ASC — NULLs sort LAST.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx, c.cn AS cn ORDER BY c.cx, c.cn LIMIT 100",
        // Mixed-var ORDER BY (each key still single-var).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bx AS bx, c.cx AS cx ORDER BY a.ak, b.bx, c.cx LIMIT 100",
        // SKIP + LIMIT.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, c.cn, a.ak SKIP 2 LIMIT 3",
        // 3-hop, ORDER BY the far var (d).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc)-[:T3]->(d:Dd) RETURN a.ak AS ak, c.cn AS cn, d.dk AS dk ORDER BY d.dk, c.cn, a.ak LIMIT 100",
        // 3-hop, ORDER BY an interior var (c).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc)-[:T3]->(d:Dd) RETURN c.cx AS cx, d.dk AS dk ORDER BY c.cx, d.dk LIMIT 100",
        // A DISTINCT projection over the 2-hop chain (dedup on the far var) — no
        // ORDER BY (first-seen), and dedup-before-LIMIT with ORDER BY (the crux
        // shape, over a multi-hop chain).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN DISTINCT c.cx AS cx",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN DISTINCT c.cx AS cx ORDER BY c.cx LIMIT 3",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN DISTINCT c.cx AS cx ORDER BY c.cx LIMIT 3"
        ),
        "a DISTINCT projection over a multi-hop chain is pipeline-only — it must fire"
    );
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 3"
        ),
        "a 2-hop project+top-k is pipeline-only — it must fire, not fall back"
    );
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc)-[:T3]->(d:Dd) RETURN d.dk AS dk ORDER BY d.dk LIMIT 3"
        ),
        "a 3-hop project+top-k must fire, not fall back"
    );
}

/// WHERE over the MIDDLE var and over the FAR var — vectorised by the pipeline's
/// own filter operator over any bound var.
#[test]
fn multihop_where_middle_and_far() {
    let g = gm();
    for src in [
        // WHERE on the middle var (b).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE b.bx = 50 RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE b.bx >= 20 AND b.bx < 60 RETURN b.bx AS bx, c.cn AS cn ORDER BY b.bx DESC, c.cn LIMIT 5",
        // WHERE on the far var (c).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE c.cx IS NOT NULL RETURN c.cx AS cx, c.cn AS cn ORDER BY c.cx, c.cn LIMIT 100",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE c.cx = 7 RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cn, a.ak LIMIT 4",
        // WHERE on the FIRST var (a).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE a.ak > 1 RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "multi-hop WHERE disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE b.bx = 50 RETURN c.cn AS cn ORDER BY c.cx, c.cn LIMIT 3"
        ),
        "a middle-var WHERE must fire through the pipeline"
    );
}

/// A 2-hop PLAIN projection — no ORDER BY, no LIMIT — must emit live rows in the
/// nested production order, byte-identical to the general path.
#[test]
fn multihop_plain_projection_production_order() {
    let g = gm();
    for src in [
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE c.cx = 7 RETURN a.ak AS ak, c.cn AS cn",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cn AS cn LIMIT 3",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cn AS cn SKIP 2 LIMIT 3",
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc)-[:T3]->(d:Dd) RETURN a.ak AS ak, c.cn AS cn, d.dk AS dk",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "multi-hop plain projection disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN a.ak AS ak, b.bn AS bn, c.cn AS cn"
        ),
        "a plain 2-hop projection is pipeline-only — it must fire"
    );
}

/// THE MULTI-LEVEL TIE TEST + CANARY TARGET. Under `WHERE c.cx = 7` every
/// surviving 2-hop row shares the sort key (cx=7), so the rows LIMIT keeps are
/// decided PURELY by the nested production order (seed ascending x reverse T1 x
/// reverse T2). `c.cn` distinguishes the tied members, so a wrong production
/// order (dropping either level's `.rev()`) changes the projected rows and
/// ON != OFF. Neutralising the `expand` `.rev()` must fail this test.
#[test]
fn multihop_tie_group_resolves_like_general() {
    let g = gm();
    // cx=7 survivors, in nested production order:
    //   (a0,b1,c2), (a0,b0,c1), (a0,b0,c0), (a2,b0,c1), (a2,b0,c0)  →  cn o,n,m,n,m
    for lim in 1..=5 {
        let src = format!(
            "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE c.cx = 7 RETURN a.ak AS ak, c.cn AS cn ORDER BY c.cx LIMIT {lim}"
        );
        let (on, off) = both(&g, &src, BTreeMap::new());
        assert_eq!(on, off, "tie resolution disagrees at LIMIT {lim}: `{src}`");
        assert_eq!(on.len(), lim.min(5), "tie group size wrong at LIMIT {lim}");
    }
}

/// A value-determined slice over 2 hops, independent of production order: the
/// smallest far-var keys. c3(cx=3) is reached only via a0->b1->c3; then the
/// cx=7 group.
#[test]
fn multihop_exact_total_order_values() {
    let g = gm();
    let (on, off) = both(
        &g,
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) WHERE c.cx IS NOT NULL RETURN c.cx AS cx, c.cn AS cn ORDER BY c.cx, c.cn, a.ak LIMIT 3",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "columnar vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![Value::Int(3), Value::Str("d".into())],
            vec![Value::Int(7), Value::Str("m".into())],
            vec![Value::Int(7), Value::Str("m".into())],
        ],
        "wrong value-ordered slice"
    );
}

/// Relationship isomorphism across a 2-hop chain: the columnar path must drop a
/// walk that reuses a relationship, exactly as the streaming path does. Without
/// the per-row `used` set, `(n0)-[r_self]->(n0)-[r_self]->(n0)` would leak in.
#[test]
fn iso_two_hop_relationship_isomorphism() {
    let g = gi();
    for src in [
        "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk ORDER BY x.nk, y.nk, z.nk LIMIT 100",
        "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk",
        "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn)-[:R]->(w:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk, w.nk AS wk ORDER BY x.nk, y.nk, z.nk, w.nk LIMIT 100",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "isomorphism disagree: `{src}`");
    }
    // Non-vacuous: the streaming path emits reuse-free rows and the pipeline
    // fired, so the agreement above is a real isomorphism check.
    assert!(
        pipeline_fired(
            &g,
            "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk ORDER BY x.nk, y.nk, z.nk LIMIT 100"
        ),
        "the isomorphism test must run through the pipeline"
    );
    // The forbidden self-loop-twice row is absent from BOTH.
    let (on, _) = both(
        &g,
        "MATCH (x:Nn)-[:R]->(y:Nn)-[:R]->(z:Nn) RETURN x.nk AS xk, y.nk AS yk, z.nk AS zk",
        BTreeMap::new(),
    );
    assert!(
        !on.contains(&vec![Value::Int(0), Value::Int(0), Value::Int(0)]),
        "the reuse `(0)-[r_self]->(0)-[r_self]->(0)` must be dropped"
    );
}

/// Every one of these is OUTSIDE the multi-hop core chain: the pipeline DECLINES
/// and the general path answers identically (ON == OFF), and the pipeline did
/// NOT fire.
#[test]
fn multihop_declines_and_falls_back_identically() {
    let g = gm();
    let declines: &[&str] = &[
        // a var-length hop IN the chain.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2*1..2]->(c:Cc) RETURN c.cx AS cx ORDER BY c.cx LIMIT 2",
        // (A bound relationship variable on a hop is now ACCEPTED, including in a
        // multi-hop chain — see `pipeline_relvar.rs`.)
        // a relationship property test on a hop.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2 {w: 1}]->(c:Cc) RETURN c.cx AS cx ORDER BY c.cx LIMIT 2",
        // an ORDER BY key spanning TWO variables.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx ORDER BY a.ak + c.cx LIMIT 2",
        // a second, disjoint path (a third variable via a cartesian path).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc), (e:Dd) RETURN c.cx AS cx ORDER BY c.cx, e.dk LIMIT 2",
        // ORDER BY with no LIMIT (unbounded top-k — decline).
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx ORDER BY c.cx",
        // a NON-final hop closing onto a bound var: `(a)-[:T1]->(a)` re-binds the
        // start mid-chain, so it is not a path's FINAL hop — only a final close is
        // a semijoin (4b1); a mid-chain self-join still declines. (The FINAL-close
        // `(a)-[:T1]->(b)-[:T2]->(a)` IS now accepted — see `pipeline_semijoin.rs`.)
        "MATCH (a:Aa)-[:T1]->(a)-[:T2]->(c:Cc) RETURN c.cx AS cx ORDER BY c.cx LIMIT 2",
        // no start label (rel-driven order — must decline).
        "MATCH (a)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx ORDER BY c.cx LIMIT 2",
        // (A DISTINCT projection over a multi-hop chain is now ACCEPTED — see
        // `pipeline_distinct.rs`; the multi-hop accept is exercised below.)
        // an aggregating projection.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN c.cx AS cx, count(*) AS n ORDER BY n DESC LIMIT 2",
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
