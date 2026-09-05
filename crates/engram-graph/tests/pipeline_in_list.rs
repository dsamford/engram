#![allow(non_snake_case)]
//! Differential tests for the composable columnar pipeline's WHERE `IN`-list
//! filter (`eval_column`'s `Expr::In` arm, reused by `DataChunk::filter`, the
//! group-by reduction and the top-k ORDER BY). The contract is the same as
//! `pipeline_core`/`pipeline_aggregate`: for every shape the pipeline accepts,
//! running with `set_columnar_scans(true)` (the vectorised `IN` membership) must
//! equal `set_columnar_scans(false)` (the per-tuple `run_streaming` path, whose
//! `eval::Expr::In` is the ORACLE) — the full ROW SET *and its order*, byte for
//! byte — and for every shape it declines, the general path answers and the two
//! still agree.
//!
//! THE load-bearing fact under test is openCypher's THREE-VALUED membership: a
//! non-member needle against a list CONTAINING a null is Unknown, not false —
//! observable only where Unknown and false diverge, i.e. under `NOT`. The
//! `NOT (b.bx IN [10, null])` case drops every row (Unknown ⇒ NOT ⇒ Unknown ⇒
//! filtered), while treating Unknown as false would KEEP the non-members (the
//! canary). It also proves the pipeline fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// a: At{ak}; b: Bt{bx int (ties + null), bn str (distinguishing, one null)}.
/// Edges T (a->b): a0->b0,b1,b2,b3; a1->b4,b4,b5; a2->b6,b0. U (b->a) for the
/// incoming test. Byte-identical to `pipeline_core`'s fixture, so the `IN`
/// filter is proven against the same graph the other pipeline tests use.
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
        mk_b(Some(50), Some("p")), // b0
        mk_b(Some(50), Some("q")), // b1
        mk_b(Some(50), Some("r")), // b2
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

/// Run `src`, panicking on error (the accepted, non-erroring shapes).
fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// The rows of a query, or its error message — so an ERRORING decline can be
/// compared across the ON/OFF paths.
type RowsRes = Result<Vec<Vec<Value>>, String>;

/// Run `src`, returning Ok(rows) or Err(message) — for the decline case whose
/// general path ERRORS (both ON and OFF must error identically).
fn run_res(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> RowsRes {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
    run_query(g, &q, params)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
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

/// The Result-returning counterpart, so an ERRORING decline can be compared.
fn both_res(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> (RowsRes, RowsRes) {
    g.set_columnar_scans(true);
    let on = run_res(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = run_res(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the non-aggregate pipeline (`finish`) fired for `src`, columnar ON.
fn pipeline_fired(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

/// Whether the group-by-COUNT pipeline (`finish_aggregate`) fired, columnar ON.
fn agg_fired(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .counters()
        .get("interp.pipeline aggregate runs")
        .copied()
        == Some(1)
}

/// `WHERE b.prop IN [<literals>]` over a hop — the accepted end-var shape. ON
/// must equal OFF across membership, ordering and slicing; and it must FIRE.
#[test]
fn in_list_end_var_literals() {
    let g = gt();
    let cases: &[&str] = &[
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 50] RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [50] RETURN b.bn AS n ORDER BY b.bn",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bn IN ['p', 'z', 'a'] RETURN b.bx AS x, b.bn AS n ORDER BY b.bn",
        // No ORDER BY — production order; still fires and agrees.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [50, 30] RETURN a.ak AS ak, b.bx AS x",
        // ORDER BY + LIMIT top-k over an IN-filtered set.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 50] RETURN b.bx AS x, b.bn AS n ORDER BY b.bx DESC, b.bn LIMIT 3",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "IN end-var disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 50] RETURN a.ak AS ak, b.bx AS x",
            BTreeMap::new(),
        ),
        "a WHERE IN over the end var must fire the pipeline, not fall back"
    );
}

/// `WHERE b.prop IN $param` — a param LIST folded once and broadcast. A param
/// that resolves to null makes the whole predicate null (every row dropped),
/// exactly like a literal null list.
#[test]
fn in_list_param() {
    let g = gt();
    let mut p = BTreeMap::new();
    p.insert(
        "xs".to_string(),
        Value::List(vec![Value::Int(10), Value::Int(30)]),
    );
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN $xs RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn",
        p.clone(),
    );
    assert_eq!(on, off, "IN $param list disagree");
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN $xs RETURN b.bx AS x",
            p,
        ),
        "a WHERE IN $param must fire the pipeline"
    );
    // A null param list ⇒ Null ⇒ every row dropped.
    let mut pn = BTreeMap::new();
    pn.insert("xs".to_string(), Value::Null);
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN $xs RETURN b.bx AS x ORDER BY b.bx",
        pn,
    );
    assert_eq!(on, off, "IN null-param disagree");
    assert!(on.is_empty(), "a null list drops every row");
}

/// A list CONTAINING a null: a non-member needle is Unknown (dropped in a plain
/// WHERE, matching openCypher); a member is still True. The differential proves
/// the vectorised path reproduces `eval::In` exactly.
#[test]
fn in_list_with_null_element() {
    let g = gt();
    // Plain filter: only b3 (bx=10) is a definite member; every other row is
    // Unknown (50/20/30 vs [10,null]) or null-needle (b5) ⇒ dropped.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, null] RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "IN [.., null] plain disagree");
    assert_eq!(
        on,
        vec![vec![Value::Int(10), Value::Str("a".into())]],
        "only the definite member survives"
    );
}

/// A NULL needle (b.bx is null, b5) vs a non-empty list ⇒ Null ⇒ dropped; vs an
/// EMPTY list `[]` ⇒ false ⇒ dropped. Both agree with the general path.
#[test]
fn in_list_null_needle_and_empty_list() {
    let g = gt();
    // Non-empty list: the null-bx row (b5) is dropped; the rest match by value.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 30, 50] RETURN b.bn AS n ORDER BY b.bn",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "null-needle vs nonempty list disagree");
    assert!(
        !on.iter().any(|r| r == &vec![Value::Str("z".into())]),
        "the null-bx row (b5, bn='z') must be dropped"
    );
    // Empty list: EVERY row dropped (false, not null — but both filter out).
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [] RETURN b.bx AS x",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "IN [] disagree");
    assert!(on.is_empty(), "IN [] drops every row");
}

/// `IN` combined with AND / OR / NOT — it composes as a boolean/null column
/// exactly like every other predicate.
#[test]
fn in_list_combined_boolean() {
    let g = gt();
    for src in [
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [50, 20] AND b.bn IS NOT NULL RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [50] OR b.bn = 'z' RETURN b.bx AS x, b.bn AS n ORDER BY b.bn",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE NOT (b.bx IN [50]) RETURN b.bx AS x ORDER BY b.bx",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 50] OR a.ak IN [3] RETURN a.ak AS ak, b.bx AS x ORDER BY a.ak, b.bx",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "IN combined disagree: `{src}`");
    }
}

/// `IN` over the START var (a) — a shape the whole-shape recognizers DECLINE
/// (their WHERE is end-var only); the pipeline accepts and vectorises it. Must
/// equal the general path AND fire.
#[test]
fn in_list_start_var() {
    let g = gt();
    for src in [
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak IN [1, 3] RETURN a.ak AS ak, b.bx AS x ORDER BY a.ak, b.bx",
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak IN [2] RETURN a.ak AS ak, b.bn AS n ORDER BY b.bn",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "IN start-var disagree: `{src}`");
    }
    assert!(
        pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak IN [1, 3] RETURN a.ak AS ak, b.bx AS x",
            BTreeMap::new(),
        ),
        "a start-var WHERE IN is pipeline-only — it must fire"
    );
}

/// `IN` feeding a group-by-COUNT — a global aggregate and a keyed group-by, both
/// filtered by an IN over the end var. ON must equal OFF and the AGGREGATE
/// operator must fire.
#[test]
fn in_list_group_by_count() {
    let g = gt();
    // Global aggregate.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [50, 20] RETURN count(*) AS c",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "IN global count disagree");
    // Keyed group-by-count.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 50] RETURN b.bx AS x, count(*) AS c ORDER BY x",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "IN group-by count disagree");
    assert!(
        agg_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [10, 20, 50] RETURN b.bx AS x, count(*) AS c ORDER BY x",
            BTreeMap::new(),
        ),
        "a WHERE IN feeding a group-by-count must fire the aggregate operator"
    );
}

/// DECLINE shapes fall back to the general path, which answers IDENTICALLY:
///  - rhs references a var (`[a.ak, 50]`) — a non-const list, out of scope.
///  - rhs a non-list constant (`IN 5`) — the general path ERRORS; ON errors too.
#[test]
fn in_list_declines_fall_back() {
    let g = gt();
    // Non-const list (references the start var) — the general path evaluates it
    // per-row; the pipeline declines and gets the identical answer.
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [a.ak, 50] RETURN a.ak AS ak, b.bx AS x ORDER BY a.ak, b.bx",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "non-const IN list disagree");
    assert!(
        !pipeline_fired(
            &g,
            "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN [a.ak, 50] RETURN a.ak AS ak, b.bx AS x",
            BTreeMap::new(),
        ),
        "a non-const IN list must DECLINE, not fire"
    );
    // Non-list constant rhs — both paths raise the SAME type error.
    let (on, off) = both_res(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx IN 5 RETURN b.bx AS x",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "IN <non-list const> must error identically");
    assert!(
        on.is_err(),
        "IN <non-list const> is a type error on both paths"
    );
}

/// CANARY: the three-valued null rule is observable under `NOT`. With correct
/// membership, `NOT (b.bx IN [10, null])` drops EVERY row (a non-member is
/// Unknown ⇒ NOT ⇒ Unknown ⇒ filtered; a member's NOT is false; a null needle is
/// Unknown). Treating Unknown as false would instead KEEP the non-members — the
/// perturbation the report exercises. ON must equal OFF, and the pipeline fires.
#[test]
fn in_list_not_null_canary() {
    let g = gt();
    // A PLAIN projection (no ORDER BY) so the pipeline fires — an ORDER BY with
    // no LIMIT would decline. Production order; the row set is what the canary
    // watches (empty when correct, every non-member when perturbed).
    let src =
        "MATCH (a:At)-[:T]->(b:Bt) WHERE NOT (b.bx IN [10, null]) RETURN b.bx AS x, b.bn AS n";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "NOT(IN [.., null]) disagree");
    assert!(
        on.is_empty(),
        "every row is Unknown under NOT ⇒ filtered (three-valued)"
    );
    assert!(
        pipeline_fired(&g, src, BTreeMap::new()),
        "the NOT(IN) canary must fire the pipeline"
    );
}
