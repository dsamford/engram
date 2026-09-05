#![allow(non_snake_case)]
//! Differential tests for the composable columnar pipeline
//! (`pipeline::plan_and_run_columnar`, Layer-4 STEP 1). The contract: for every
//! shape the pipeline accepts, running with `set_columnar_scans(true)` (the
//! pipeline) must equal `set_columnar_scans(false)` (the per-tuple
//! `run_streaming` path) — the full ROW SET *and its order*, byte-for-byte — and
//! for every shape it declines, the general path answers and the two still
//! agree. The oracle is the same query run with columnar OFF.
//!
//! The load-bearing order fact — a fixed hop's neighbours emitted in REVERSE
//! `adjacent_slim` order (LIFO), a label scan seeded ASCENDING — is exercised by
//! the TIE-GROUP test, whose LIMIT-kept rows are decided purely by production
//! order; dropping the pipeline's `.rev()` diverges it (the canary), which also
//! proves the pipeline fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// a: At{ak}; b: Bt{bx int (ties + null), bn str (distinguishing, one null)}.
/// Edges T (a->b): a same-`a` tie group (a0->b0,b1,b2 all bx=50, distinct bn), a
/// cross-`a` edge into that group (a2->b0), a duplicate edge (a1->b4x2), a
/// null-bx end (b5) and a null-bn end (b6). U (b->a) for the incoming test.
/// Byte-identical to the fixture the recognizer tests use, so the pipeline is
/// proven against the same graph.
fn gt() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["At".into()], &p).expect("a")
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
        g.create_node(&["Bt".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some(50), Some("p")), // b0 — tie group
        mk_b(Some(50), Some("q")), // b1 — tie group
        mk_b(Some(50), Some("r")), // b2 — tie group
        mk_b(Some(10), Some("a")), // b3
        mk_b(Some(20), Some("b")), // b4 (doubled edge)
        mk_b(None, Some("z")),     // b5 — bx NULL
        mk_b(Some(30), None),      // b6 — bn NULL
    ];
    for (s, d) in [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (1, 4),
        (1, 4),
        (1, 5),
        (2, 6),
        (2, 0),
    ] {
        g.create_rel(a[s], "T", b[d], &BTreeMap::new()).expect("T");
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

/// Whether the pipeline fired (produced the answer) for `src` with columnar ON.
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

#[test]
fn core_matches_general_across_shapes() {
    let g = gt();
    // Each of these is an ACCEPTED shape; ON must equal OFF row-for-row AND in
    // order across slicing, SKIP, DESC/ASC, NULL keys, a/b projection, a
    // one-sided WHERE (start OR end), the incoming leg, and an unlabelled end.
    let cases: &[&str] = &[
        // Total order (b.bx, b.bn, a.ak) — LIMIT slicing at several depths.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 1",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 4",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 100",
        // SKIP + LIMIT.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak SKIP 2 LIMIT 3",
        // DESC — NULLs sort FIRST under the reversed comparison.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx DESC, b.bn DESC LIMIT 100",
        // A NULL sort key, ASC — NULLs sort LAST.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn LIMIT 100",
        // ORDER BY over the a side.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bn AS n ORDER BY a.ak, b.bn LIMIT 6",
        // Compound WHERE over the END var + ORDER BY + LIMIT.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx >= 20 AND b.bx < 60 RETURN b.bx AS x, b.bn AS n ORDER BY b.bx DESC, b.bn LIMIT 3",
        // WHERE over the START var + ORDER BY + LIMIT (the recognizers DECLINE
        // this; the pipeline accepts it).
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak > 1 RETURN a.ak AS ak, b.bx AS x ORDER BY b.bx, a.ak LIMIT 5",
        // A boolean ORDER BY key.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx IS NULL, b.bx LIMIT 8",
        // Incoming direction.
        "MATCH (a:At)<-[:U]-(b:Bt) RETURN a.ak AS ak, b.bn AS n ORDER BY b.bn, a.ak LIMIT 3",
        // An unlabelled end (still a two-var hop the pipeline drives).
        "MATCH (a:At)-[:T]->(b) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn LIMIT 4",
        // ORDER BY the RETURN ALIASES (`x`, `n`) rather than the pattern
        // properties — the alias resolver classifies/sorts by the projected
        // expression, byte-identically to the pattern-prop form.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY x, n LIMIT 4",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY x DESC, n ASC LIMIT 100",
        // A MIX of an alias key and a pattern-property key.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x ORDER BY x, a.ak LIMIT 5",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    }
}

/// An `ORDER BY` over the RETURN ALIASES fires the core top-k (it does not fall
/// back): the alias resolver turns `ORDER BY x DESC, n ASC` into a sort by the
/// projected `b.bx` / `b.bn`, which the pipeline vectorises. ON==OFF and it fires.
#[test]
fn core_order_by_alias_fires() {
    let g = gt();
    let src =
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY x DESC, n ASC LIMIT 4";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "alias ORDER BY: columnar vs general disagree");
    assert!(
        pipeline_fired(&g, src),
        "an alias ORDER BY must FIRE the core top-k, not fall back to the streamed chain"
    );
}

/// NULL sort keys, explicit: DESC ⇒ null FIRST, ASC ⇒ null LAST, over the whole
/// (unlimited-enough) row set so the null placement is visible in the values.
#[test]
fn core_null_sort_key_placement() {
    let g = gt();
    // ASC: b5 (bx NULL) sorts LAST.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 100",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "ASC null-last: columnar vs general disagree");
    assert_eq!(
        on.last(),
        Some(&vec![Value::Null]),
        "null must sort last ASC"
    );
    // DESC: b5 (bx NULL) sorts FIRST.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx DESC LIMIT 100",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "DESC null-first: columnar vs general disagree");
    assert_eq!(
        on.first(),
        Some(&vec![Value::Null]),
        "null must sort first DESC"
    );
}

/// SKIP + LIMIT paging, param and literal, against the general path.
#[test]
fn core_skip_limit_paging() {
    let g = gt();
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak SKIP 3 LIMIT 2",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "literal SKIP/LIMIT disagree");
    let mut p = BTreeMap::new();
    p.insert("s".to_string(), Value::Int(1));
    p.insert("l".to_string(), Value::Int(2));
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak SKIP $s LIMIT $l",
        p,
    );
    assert_eq!(on, off, "param SKIP/LIMIT disagree");
}

/// WHERE over the END var (b) — the recognizers' territory, but here through the
/// pipeline's own filter operator.
#[test]
fn core_where_end_var() {
    let g = gt();
    for src in [
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IS NOT NULL RETURN b.bx AS x ORDER BY b.bx, b.bn, a.ak LIMIT 3",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx = 50 RETURN a.ak AS ak, b.bn AS n ORDER BY b.bn, a.ak LIMIT 4",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx < 30 OR b.bn = 'z' RETURN b.bn AS n ORDER BY b.bn LIMIT 5",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "WHERE end var disagree: `{src}`");
    }
}

/// WHERE over the START var (a) — a shape the recognizers DECLINE. The pipeline
/// accepts and vectorises it; must still equal the general path, with and
/// without ORDER BY.
#[test]
fn core_where_start_var() {
    let g = gt();
    for src in [
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak > 1 RETURN a.ak AS ak, b.bx AS x ORDER BY b.bx, a.ak LIMIT 4",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak = 1 RETURN a.ak AS ak, b.bn AS n ORDER BY b.bn LIMIT 8",
        // No ORDER BY/LIMIT — production order, WHERE on the start.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak >= 2 RETURN a.ak AS ak, b.bx AS x",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "WHERE start var disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak > 1 RETURN a.ak AS ak, b.bx AS x ORDER BY b.bx, a.ak LIMIT 4"
        ),
        "start-var WHERE is a pipeline-only shape — it must fire, not fall back"
    );
}

/// A plain projection with NO ORDER BY and NO LIMIT — the shape the recognizers
/// DECLINE (they require a LIMIT). The pipeline emits live rows in PRODUCTION
/// ORDER; must equal the general path exactly, WHERE on either side or none.
#[test]
fn core_plain_projection_production_order() {
    let g = gt();
    for src in [
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx >= 20 RETURN a.ak AS ak, b.bx AS x",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak <= 2 RETURN b.bn AS n",
        // No ORDER BY but a LIMIT (allowed — LIMIT without ORDER BY is a window
        // over production order).
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x LIMIT 3",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x SKIP 2 LIMIT 3",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "plain projection disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n"
        ),
        "a plain no-ORDER-BY hop is pipeline-only — it must fire, not fall back"
    );
}

/// THE TIE TEST + CANARY TARGET. All qualifying pairs share the sort key
/// (bx=50), so the rows LIMIT keeps are decided PURELY by production order
/// (scan ascending x REVERSE adjacency). `b.bn` distinguishes the tie-group
/// members, so a wrong production order (dropping the pipeline's `.rev()`)
/// changes the projected rows and ON != OFF.
#[test]
fn core_tie_group_resolves_like_general() {
    let g = gt();
    // a0 -> {b0,b1,b2} (bx=50, bn p/q/r) and a2 -> b0: four tied pairs.
    for lim in 1..=4 {
        let src = format!(
            "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx = 50 RETURN a.ak AS ak, b.bn AS n ORDER BY b.bx LIMIT {lim}"
        );
        let (on, off) = both(&g, &src, BTreeMap::new());
        assert_eq!(on, off, "tie resolution disagrees at LIMIT {lim}: `{src}`");
        assert_eq!(on.len(), lim.min(4), "tie group size wrong at LIMIT {lim}");
    }
}

/// A value-determined slice, independent of production order: the three smallest
/// non-null bx are 10 (b3), then 20 (b4, doubled edge) twice.
#[test]
fn core_exact_total_order_values() {
    let g = gt();
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IS NOT NULL RETURN b.bx AS x ORDER BY b.bx, b.bn, a.ak LIMIT 3",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "columnar vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![Value::Int(10)],
            vec![Value::Int(20)],
            vec![Value::Int(20)],
        ],
        "wrong slice"
    );
}

/// Every one of these is OUTSIDE the core read chain: the pipeline DECLINES and
/// the general path answers identically (ON == OFF), and — critically — the
/// pipeline did NOT fire (so it never perturbs a shape it does not own).
#[test]
fn core_declines_and_falls_back_identically() {
    let g = gt();
    let declines: &[&str] = &[
        // variable-length hop.
        "MATCH (a:At)-[:T*1..2]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 2",
        // an inline relationship property MAP (`rel_satisfies` equality — still
        // declined; a bound relationship variable is now ACCEPTED and lives in
        // `pipeline_relvar.rs`).
        "MATCH (a:At)-[:T {w: 1}]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 2",
        // aggregating projection.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, count(*) AS c ORDER BY c DESC LIMIT 2",
        // a third variable via a cartesian path.
        "MATCH (a:At)-[:T]->(b:Bt), (c:Bt) RETURN b.bx AS x ORDER BY b.bx, c.bx LIMIT 2",
        // an ORDER BY key spanning BOTH variables.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY a.ak + b.bx LIMIT 2",
        // (A DISTINCT projection is now ACCEPTED — see `pipeline_distinct.rs`.)
        // ORDER BY with no LIMIT (unbounded top-k — decline).
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx",
        // no start label (rel-driven order in the general path — must decline).
        "MATCH (a)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx, b.bn LIMIT 3",
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

/// The pipeline must not perturb unrelated single-variable / aggregate shapes:
/// they decline (do not fire) and still answer identically.
#[test]
fn core_declines_unrelated_shapes() {
    let g = gt();
    for src in [
        // a bare single-variable scan (the columnar-projection operator's turf).
        "MATCH (n:At) RETURN n.ak AS ak ORDER BY n.ak",
        // a hop aggregate (not a projection).
        "MATCH (a:At)-[:T]->(b:Bt) RETURN count(*) AS c",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "unrelated shape disagree: `{src}`");
        assert!(
            !pipeline_fired(&g, src),
            "pipeline must not claim an unrelated shape: `{src}`"
        );
    }
}

/// The pipeline fires (does not silently decline) for its accepted shapes with
/// columnar ON, and does NOT fire with columnar OFF — so the differential above
/// is non-vacuous.
#[test]
fn core_pipeline_fires_when_on() {
    let g = gt();
    let accepted =
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x ORDER BY b.bx, a.ak LIMIT 4";
    assert!(pipeline_fired(&g, accepted), "pipeline must fire when ON");
    // With columnar OFF the pipeline gate closes: the counter is absent.
    g.set_columnar_scans(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, accepted, BTreeMap::new()));
    assert_eq!(
        trace.counters().get("interp.pipeline hop runs").copied(),
        None,
        "pipeline must not fire when columnar is OFF"
    );
    g.set_columnar_scans(true);
}
