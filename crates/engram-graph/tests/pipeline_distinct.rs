#![allow(non_snake_case)]
//! Differential tests for a DISTINCT PROJECTION (non-aggregate) in the composable
//! columnar pipeline (`pipeline::plan_and_run_columnar`). The contract is the same
//! as every other `pipeline_*` suite: for every shape the pipeline ACCEPTS,
//! running with `set_columnar_scans(true)` (the pipeline) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full row
//! SET and its ORDER, byte-for-byte — and for every shape it DECLINES the general
//! path answers and the two still agree. The oracle is the same query columnar
//! OFF.
//!
//! What is new here: `RETURN DISTINCT <items>` is recognised as a GROUP-BY with
//! ZERO aggregate sites — the projected items ARE the grouping keys. The dedup is
//! column-native (`reduce_agg_groups`: the raw-id u64 fast path for a bare
//! node/rel key, `agg_key_of` for value keys), first-seen, the SAME canonical-key
//! equivalence `run_streaming` uses; `project_agg_groups` then emits one row per
//! group and the shared tail orders/skips/limits. The load-bearing subtlety:
//! DISTINCT must dedup BEFORE any LIMIT — the group reduction dedups first, so the
//! tail's ORDER BY / SKIP / LIMIT act on the already-distinct set (NEVER a
//! limit-before-dedup, which `native_topk` would do). The crux test
//! `distinct_dedup_before_limit` is constructed so a limit-before-dedup path would
//! give a WRONG answer.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A: Ad{ak}; B: Bd{bx int (ties + a null), bn str, gender str}. T (a->b) carries
/// {since (int; ties + a null), w}. The topology is `pipeline_core`'s tie fixture
/// with rel props + a `gender` column bolted on, so DISTINCT bites in every axis.
/// The edges: a0 -> b0,b1,b2 (all bx=50, distinct bn) is a DISTINCT-bx collapse;
/// a1 -> b4 TWICE (a duplicated edge) is a duplicate (bx=20) LIVE row; a2 -> b0
/// reaches b0 from a SECOND start (a DISTINCT-node collapse); a1 -> b5 (bx null)
/// and a2 -> b6 (bn null) give three-valued keys. U (b->a) mirrors the incoming
/// leg. The crux: the two smallest bx (10, then 20 TWICE) mean a top-k of 3 raw
/// rows keeps {10,20,20} and dedups to TWO rows, while the correct
/// DISTINCT-then-LIMIT-3 is {10,20,30} — three rows.
fn gd() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Ad".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3)];
    let mk_b = |bx: Option<i64>, bn: Option<&str>, gender: &str| {
        let mut p = BTreeMap::new();
        if let Some(v) = bx {
            p.insert("bx".to_string(), Value::Int(v));
        }
        if let Some(s) = bn {
            p.insert("bn".to_string(), Value::Str(s.to_string()));
        }
        p.insert("gender".to_string(), Value::Str(gender.to_string()));
        g.create_node(&["Bd".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some(50), Some("p"), "F"), // b0 — tie group, reached from a0 AND a2
        mk_b(Some(50), Some("q"), "M"), // b1 — tie group
        mk_b(Some(50), Some("r"), "F"), // b2 — tie group
        mk_b(Some(10), Some("a"), "M"), // b3 — smallest bx
        mk_b(Some(20), Some("b"), "F"), // b4 — doubled edge (bx=20 twice live)
        mk_b(None, Some("z"), "M"),     // b5 — bx NULL
        mk_b(Some(30), None, "F"),      // b6 — bn NULL
    ];
    // T edges (a->b) with {since, w}; `since` has ties (5) and a null.
    let t = |s: usize, d: usize, since: Option<i64>, w: i64, ab: &[u64; 3], bb: &[u64; 7]| {
        let mut p = BTreeMap::new();
        if let Some(v) = since {
            p.insert("since".to_string(), Value::Int(v));
        }
        p.insert("w".to_string(), Value::Int(w));
        g.create_rel(ab[s], "T", bb[d], &p).expect("T");
    };
    for (s, d, since, w) in [
        (0, 0, Some(5), 100),
        (0, 1, Some(5), 200), // since tie with a0->b0
        (0, 2, Some(3), 50),
        (0, 3, Some(7), 70),
        (1, 4, Some(2), 400),
        (1, 4, Some(2), 410), // a SECOND a1->b4 edge (distinct rel identity)
        (1, 5, None, 500),    // since NULL
        (2, 6, Some(8), 80),
        (2, 0, Some(5), 90), // since=5 again, across a's
    ] {
        t(s, d, since, w, &a, &b);
    }
    for (s, d) in [(0, 0), (1, 0), (4, 1), (6, 2)] {
        g.create_rel(b[s], "U", a[d], &BTreeMap::new()).expect("U");
    }
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

/// Whether the core hop pipeline produced the answer for `src` with columnar ON.
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

/// Assert ON == OFF (rows AND order) and that the pipeline FIRED.
fn accept(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(
        pipeline_fired(g, src),
        "an accepted DISTINCT shape must FIRE, not fall back: `{src}`"
    );
}

// ─── Convenience literal builders ────────────────────────────────────────────
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(t: &str) -> Value {
    Value::Str(t.to_string())
}

/// The whole accepted sweep: every DISTINCT-projection shape the pipeline now owns
/// must equal the general path row-for-row AND in order, and must FIRE.
#[test]
fn distinct_matches_general_across_shapes() {
    let g = gd();
    let cases: &[&str] = &[
        // DISTINCT over a single value column, no ORDER BY — first-seen order.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x",
        // DISTINCT then ORDER BY (dedup precedes the sort).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx",
        // DISTINCT + ORDER BY + LIMIT (the dedup-before-limit crux).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx LIMIT 3",
        // DISTINCT + ORDER BY + SKIP + LIMIT.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx SKIP 1 LIMIT 2",
        // DISTINCT + ORDER BY DESC (null placement flips).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx DESC",
        // Multi-column dedup, no ORDER BY.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT a.ak AS ak, b.gender AS g",
        // Multi-column dedup + ORDER BY over both columns (by property, so the
        // single-var recognizer accepts each key; ORDER BY over the ALIAS declines
        // and is covered in the decline sweep).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT a.ak AS ak, b.gender AS g ORDER BY a.ak, b.gender",
        // A whole NODE var — dedup by node identity (b0 reached from a0 AND a2).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b",
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b ORDER BY b.bx, b.bn",
        // A rel var: DISTINCT over a rel property, and over the rel identity.
        "MATCH (a:Ad)-[r:T]->(b:Bd) RETURN DISTINCT r.since AS s ORDER BY r.since",
        "MATCH (a:Ad)-[r:T]->(b:Bd) RETURN DISTINCT r.since AS s",
        "MATCH (a:Ad)-[r:T]->(b:Bd) RETURN DISTINCT r",
        // DISTINCT with a WHERE over the END var, and over the START var.
        "MATCH (a:Ad)-[:T]->(b:Bd) WHERE b.bx >= 20 RETURN DISTINCT b.bx AS x ORDER BY b.bx",
        "MATCH (a:Ad)-[:T]->(b:Bd) WHERE a.ak > 1 RETURN DISTINCT b.gender AS g ORDER BY b.gender",
        // DISTINCT over an UNDIRECTED hop (Dir::Both).
        "MATCH (a:Ad)-[:T]-(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx",
        // DISTINCT over the INCOMING leg.
        "MATCH (a:Ad)<-[:U]-(b:Bd) RETURN DISTINCT a.ak AS ak ORDER BY a.ak",
        // DISTINCT with a plain LIMIT (no ORDER BY — window over dedup'd order).
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x LIMIT 3",
    ];
    for src in cases {
        accept(&g, src);
    }
}

/// DISTINCT with NO ORDER BY keeps FIRST-SEEN order — pinned against the exact
/// production order (scan ascending x reverse adjacency), the same order the tie
/// tests in `pipeline_core` prove.
#[test]
fn distinct_first_seen_order_pinned() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "first-seen: columnar vs general disagree");
    // Production order of bx (reverse adjacency per start):
    //   a0 -> b3,b2,b1,b0 = 10,50,50,50 ; a1 -> b5,b4,b4 = null,20,20 ;
    //   a2 -> b0,b6 = 50,30. First-seen DISTINCT: 10, 50, null, 20, 30.
    assert_eq!(
        on,
        vec![
            vec![i(10)],
            vec![i(50)],
            vec![Value::Null],
            vec![i(20)],
            vec![i(30)],
        ],
        "DISTINCT with no ORDER BY must keep first-seen order"
    );
}

/// DISTINCT dedups BEFORE the sort, so the sorted DISTINCT set is exact (ASC ->
/// null LAST).
#[test]
fn distinct_then_sort_is_exact() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "dedup-then-sort: columnar vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![i(10)],
            vec![i(20)],
            vec![i(30)],
            vec![i(50)],
            vec![Value::Null],
        ],
        "distinct-then-ascending-sort, null last"
    );
}

/// THE CRUX: DISTINCT must dedup BEFORE LIMIT. Data is built so a limit-before-
/// dedup path (`native_topk`, cap = skip+limit) would keep the 3 smallest RAW
/// rows {10, 20, 20} and dedup them to TWO rows [10, 20]; the CORRECT DISTINCT-
/// then-sort-then-LIMIT-3 is THREE rows [10, 20, 30]. ON == OFF == correct proves
/// the routing dedups first.
#[test]
fn distinct_dedup_before_limit() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx LIMIT 3";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "dedup-before-limit: columnar vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(10)], vec![i(20)], vec![i(30)]],
        "DISTINCT must dedup BEFORE LIMIT (limit-before-dedup would give [10,20])"
    );
    // A limit-before-dedup path would return FEWER than 3 rows here.
    assert_eq!(on.len(), 3, "dedup-before-limit must yield 3 distinct rows");
    assert!(
        pipeline_fired(&g, src),
        "the crux DISTINCT shape must FIRE (else the canary is vacuous)"
    );
}

/// Multi-column DISTINCT: the (a.ak, b.gender) pairs collapse to the distinct set,
/// sorted by both keys.
#[test]
fn distinct_multi_column_exact() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT a.ak AS ak, b.gender AS g ORDER BY a.ak, b.gender";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "multi-column DISTINCT: columnar vs general disagree"
    );
    assert!(pipeline_fired(&g, src), "multi-column DISTINCT must fire");
    assert_eq!(
        on,
        vec![
            vec![i(1), s("F")],
            vec![i(1), s("M")],
            vec![i(2), s("F")],
            vec![i(2), s("M")],
            vec![i(3), s("F")],
        ],
        "distinct (ak, gender) pairs"
    );
}

/// DISTINCT over a whole NODE var dedups by node IDENTITY — b0 is reached from two
/// starts (a0 and a2) yet appears once; the seven B nodes collapse to seven rows.
#[test]
fn distinct_whole_node_by_identity() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "DISTINCT node: columnar vs general disagree");
    assert_eq!(
        on.len(),
        7,
        "seven distinct B nodes (b0 reached twice collapses to one)"
    );
}

/// DISTINCT over a REL property value dedups the ties/null; the sorted distinct
/// `since` set is exact.
#[test]
fn distinct_rel_property_exact() {
    let g = gd();
    let src = "MATCH (a:Ad)-[r:T]->(b:Bd) RETURN DISTINCT r.since AS s ORDER BY r.since";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "DISTINCT rel prop: columnar vs general disagree");
    assert!(pipeline_fired(&g, src), "DISTINCT rel prop must fire");
    assert_eq!(
        on,
        vec![
            vec![i(2)],
            vec![i(3)],
            vec![i(5)],
            vec![i(7)],
            vec![i(8)],
            vec![Value::Null],
        ],
        "distinct since values, ascending, null last"
    );
}

/// DISTINCT over a bare NODE var dedups by node IDENTITY (the u64 fast path in
/// the group-by reduction — a node's canonical key is `(tag, id)`, injective in
/// the id), NOT by property VALUE. Two B nodes carry IDENTICAL properties, so a
/// value-dedup would wrongly collapse them to one; identity-dedup keeps BOTH.
/// b0 is reached from TWO starts (a0 and a1) yet appears ONCE — the identity
/// collapse. ON == OFF, and the pipeline FIRES.
#[test]
fn distinct_node_identity_u64_path() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut p = BTreeMap::new();
    p.insert("k".to_string(), Value::Int(1)); // IDENTICAL props on both B nodes
    let a0 = g.create_node(&["NA".into()], &BTreeMap::new()).expect("a0");
    let a1 = g.create_node(&["NA".into()], &BTreeMap::new()).expect("a1");
    let b0 = g.create_node(&["NB".into()], &p).expect("b0");
    let b1 = g.create_node(&["NB".into()], &p).expect("b1");
    g.create_rel(a0, "E", b0, &BTreeMap::new()).expect("e");
    g.create_rel(a0, "E", b1, &BTreeMap::new()).expect("e");
    g.create_rel(a1, "E", b0, &BTreeMap::new()).expect("e"); // b0 reached twice

    let src = "MATCH (a:NA)-[:E]->(b:NB) RETURN DISTINCT b";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "DISTINCT node identity: columnar vs general disagree"
    );
    assert!(
        pipeline_fired(&g, src),
        "DISTINCT over a node var must fire"
    );
    // Two property-identical nodes stay DISTINCT (identity, not value); b0 reached
    // twice collapses to one — so exactly two rows. First-seen order is production
    // order (reverse adjacency of a0's edges b0,b1 => b1,b0), so {b1, b0}.
    let ids: Vec<u64> = on
        .iter()
        .map(|row| match &row[0] {
            Value::Node { id, .. } => *id,
            other => panic!("expected a node value, got {other:?}"),
        })
        .collect();
    assert_eq!(
        ids,
        vec![b1, b0],
        "identity-dedup keeps both property-identical nodes; b0 (reached twice) appears once"
    );
}

/// Every DISTINCT shape OUTSIDE the core path: the pipeline DECLINES and the
/// general path answers identically (ON == OFF), and the pipeline did NOT fire.
#[test]
fn distinct_declines_and_falls_back_identically() {
    let g = gd();
    let declines: &[&str] = &[
        // A DISTINCT that also AGGREGATES — the aggregate recognizer's concern; it
        // declines `proj.distinct`, and the core path declines the aggregate item.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.gender AS g, count(*) AS c ORDER BY g",
        // DISTINCT * — a star projection.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT *",
        // (A DISTINCT projection over a frontier-BFS VARIABLE-LENGTH hop is now an
        // ACCEPTED shape — see `pipeline_varlen.rs`.)
        // (A multi-stage `WITH DISTINCT … RETURN` is an ACCEPTED shape since fix
        // 29 (v105) — see `distinct_with_tail_fires_and_agrees` below.)
        // A DISTINCT projection with no start label (rel-driven order).
        "MATCH (a)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx LIMIT 3",
        // ORDER BY a projection ALIAS (not a pattern var/prop) — the single-var
        // ORDER BY recognizer declines it; the general path answers identically.
        "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT a.ak AS ak, b.gender AS g ORDER BY ak, g",
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

/// The multi-stage `WITH DISTINCT <keys> RETURN <over the keys>` form is
/// ACCEPTED since fix 29 (v105): it fires on the pipeline and agrees with the
/// general path.
#[test]
fn distinct_with_tail_fires_and_agrees() {
    let g = gd();
    let src = "MATCH (a:Ad)-[:T]->(b:Bd) WITH DISTINCT b RETURN b.bx AS x ORDER BY x";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "`{src}`");
    assert!(pipeline_fired(&g, src), "fires: `{src}`");
}

/// The differential is non-vacuous: an accepted DISTINCT shape fires with columnar
/// ON and does NOT fire with columnar OFF.
#[test]
fn distinct_pipeline_fires_only_when_on() {
    let g = gd();
    let accepted = "MATCH (a:Ad)-[:T]->(b:Bd) RETURN DISTINCT b.bx AS x ORDER BY b.bx LIMIT 3";
    assert!(pipeline_fired(&g, accepted), "must fire when ON");
    g.set_columnar_scans(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, accepted, BTreeMap::new()));
    assert_eq!(
        trace.counters().get("interp.pipeline hop runs").copied(),
        None,
        "pipeline must not fire when columnar is OFF"
    );
    g.set_columnar_scans(true);
}
