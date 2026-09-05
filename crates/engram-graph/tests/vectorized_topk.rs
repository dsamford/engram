#![allow(non_snake_case)]
//! Vectorized hop projection + ORDER BY/LIMIT top-k (Layer-4 increment 3).
//! Differential: the same query with `set_columnar_scans(true)` (this operator)
//! must equal `set_columnar_scans(false)` (the general per-tuple path) — the
//! full ROW SET *and its order*, not just a count. The graph carries a-side and
//! b-side numeric + string props, NULL keys, edge multiplicity (so count !=
//! distinct), and — critically — an ORDER-BY TIE GROUP under one `a` whose
//! members carry DISTINCT projected columns, so the row set LIMIT keeps depends
//! on production order (A ascending × REVERSE adjacency). Break the `.rev()` and
//! the tie test diverges (the canary), which also proves the operator fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// a: At{ak}; b: Bt{bx int (ties + null), bn str (distinguishing, one null)}.
/// Edges T (a->b) with a same-`a` tie group (a0->b0,b1,b2 all bx=50, distinct
/// bn), a cross-`a` edge into that group (a2->b0), a duplicate edge (a1->b4×2),
/// a null-bx end (b5) and a null-bn end (b6). U (b->a) for the incoming test.
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

/// Run `src` with the vectorized operator ON and the general path OFF.
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

#[test]
fn topk_matches_general_across_shapes() {
    let g = gt();
    // Every one of these is this operator's shape; ON must equal OFF row-for-row
    // AND in order — the byte-identity contract, across slicing, SKIP, DESC/ASC,
    // NULL keys, mixed a/b projection, a compound WHERE, and the incoming leg.
    let cases: &[&str] = &[
        // Total order (b.bx, b.bn, a.ak) — LIMIT slicing at several depths.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 1",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 4",
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak LIMIT 100",
        // SKIP + LIMIT.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak SKIP 2 LIMIT 3",
        // DESC — NULLs sort FIRST under the reversed comparison.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx DESC, b.bn DESC LIMIT 5",
        // A NULL sort key (b5.bx is null; b6.bn is null), ASC — NULLs sort LAST.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn LIMIT 100",
        // ORDER BY over the a side.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bn AS n ORDER BY a.ak, b.bn LIMIT 6",
        // Compound WHERE over b (reuses eval_column) + ORDER BY + LIMIT.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE b.bx >= 20 AND b.bx < 60 RETURN b.bx AS x, b.bn AS n ORDER BY b.bx DESC, b.bn LIMIT 3",
        // A boolean ORDER BY key.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx IS NULL, b.bx LIMIT 8",
        // A const ORDER BY key (degenerate — leaves production order intact).
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bn AS n ORDER BY 1, b.bn LIMIT 4",
        // Incoming direction.
        "MATCH (a:At)<-[:U]-(b:Bt) RETURN a.ak AS ak, b.bn AS n ORDER BY b.bn, a.ak LIMIT 3",
        // b-label filter absent from the end (still valid — no b labels).
        "MATCH (a:At)-[:T]->(b) RETURN b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn LIMIT 4",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    }
}

#[test]
fn topk_exact_total_order_values() {
    // A totally value-determined slice (independent of production order): the
    // three smallest bx are 10 (b3), then 20 (b4, doubled edge) twice.
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

#[test]
fn topk_param_bounds() {
    let g = gt();
    let mut p = BTreeMap::new();
    p.insert("s".to_string(), Value::Int(1));
    p.insert("l".to_string(), Value::Int(2));
    let (on, off) = both(
        &g,
        "MATCH (a:At)-[:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS x, b.bn AS n ORDER BY b.bx, b.bn, a.ak SKIP $s LIMIT $l",
        p,
    );
    assert_eq!(on, off, "param SKIP/LIMIT: columnar vs general disagree");
}

/// THE TIE TEST + CANARY TARGET. All qualifying pairs share the sort key
/// (bx=50), so the rows LIMIT keeps are decided purely by the production-order
/// tiebreak. `b.bn` distinguishes the tie-group members, so a wrong production
/// order (e.g. dropping the `.rev()`) changes the projected rows and ON != OFF.
#[test]
fn topk_tie_group_resolves_like_the_general_path() {
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

#[test]
fn topk_declines_and_falls_back_identically() {
    let g = gt();
    // Not this operator's shape: the recognizer declines and the general path
    // answers identically (ON == OFF).
    for src in [
        // WHERE over the START side, not the end.
        "MATCH (a:At)-[:T]->(b:Bt) WHERE a.ak > 1 RETURN b.bx AS x ORDER BY b.bx LIMIT 2",
        // aggregating projection.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x, count(*) AS c ORDER BY c DESC LIMIT 2",
        // variable-length hop.
        "MATCH (a:At)-[:T*1..2]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 2",
        // undirected hop.
        "MATCH (a:At)-[:T]-(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 2",
        // no LIMIT.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx",
        // DISTINCT.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN DISTINCT b.bx AS x ORDER BY b.bx LIMIT 3",
        // an ORDER BY key spanning BOTH variables.
        "MATCH (a:At)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY a.ak + b.bx LIMIT 2",
        // a third variable via a cartesian path.
        "MATCH (a:At)-[:T]->(b:Bt), (c:Bt) RETURN b.bx AS x ORDER BY b.bx, c.bx LIMIT 2",
        // no start label (rel-driven order in the general path — must decline).
        "MATCH (a)-[:T]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx, b.bn LIMIT 3",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
    }
}
