#![allow(non_snake_case)]
//! Vectorized hop-filter-count (Layer-4 increment 1). Differential: the same
//! query with `set_columnar_scans(true)` (this vectorized operator) must equal
//! `set_columnar_scans(false)` (the general per-tuple path), row-for-row. The
//! graph has a NULL property, a duplicate edge, and shared ends so count(*)
//! exercises multiplicity and the three-valued WHERE drop.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let a: Vec<u64> = (0..3)
        .map(|_| g.create_node(&["Aa".into()], &BTreeMap::new()).expect("a"))
        .collect();
    // b0=10, b1=20, b2=NULL (no prop), b3=30
    let mk_b = |prop: Option<i64>| {
        let mut p = BTreeMap::new();
        if let Some(v) = prop {
            p.insert("prop".to_string(), Value::Int(v));
        }
        g.create_node(&["Bb".into()], &p).expect("b")
    };
    let b = [mk_b(Some(10)), mk_b(Some(20)), mk_b(None), mk_b(Some(30))];
    // R: a->b, with a duplicate a0->b0 and shared ends.
    for (s, d) in [(0, 0), (0, 0), (0, 1), (1, 1), (1, 2), (2, 3), (2, 2)] {
        g.create_rel(a[s], "R", b[d], &BTreeMap::new()).expect("R");
    }
    // S: b->a (for the incoming-direction test).
    for (s, d) in [(0, 0), (1, 0), (2, 1), (3, 2)] {
        g.create_rel(b[s], "S", a[d], &BTreeMap::new()).expect("S");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

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

fn one(n: i64) -> Vec<Vec<Value>> {
    vec![vec![Value::Int(n)]]
}

#[test]
fn vectorized_equals_general_and_is_correct() {
    let g = g();
    let cases: &[(&str, i64)] = &[
        // b0(10),b1(20) pass; b2(null) drops; b3(30) fails. a0->b0 ×2 + a0->b1 + a1->b1 = 4.
        (
            "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < 25 RETURN count(*) AS c",
            4,
        ),
        // b1(20),b3(30) pass. a0->b1 + a1->b1 + a2->b3 = 3.
        (
            "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop >= 20 RETURN count(*) AS c",
            3,
        ),
        // b0 only. a0->b0 ×2 = 2.
        (
            "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop = 10 RETURN count(*) AS c",
            2,
        ),
        // b0(10),b3(30) pass (b1 excluded; b2 null <> 20 = null drops). a0->b0 ×2 + a2->b3 = 3.
        (
            "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop <> 20 RETURN count(b) AS c",
            3,
        ),
        // incoming: b0,b1 -> a0 pass (<25); b2->a1 null drop; b3->a2 fail. = 2.
        (
            "MATCH (a:Aa)<-[:S]-(b:Bb) WHERE b.prop < 25 RETURN count(*) AS c",
            2,
        ),
        // a b with NO in-set edges under a stricter bound: only b0(10) < 15. a0->b0 ×2 = 2.
        (
            "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < 15 RETURN count(*) AS c",
            2,
        ),
    ];
    for (src, want) in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
        assert_eq!(on, one(*want), "wrong count for `{src}`");
    }
}

#[test]
fn vectorized_handles_a_param_rhs() {
    let g = g();
    let mut p = BTreeMap::new();
    p.insert("lim".to_string(), Value::Int(25));
    let (on, off) = both(
        &g,
        "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < $lim RETURN count(*) AS c",
        p,
    );
    assert_eq!(on, off, "param rhs: columnar vs general disagree");
    assert_eq!(on, one(4), "param rhs count wrong");
}

#[test]
fn declines_shapes_it_cannot_prove_and_falls_back() {
    let g = g();
    // Each is NOT this operator's shape; the recognizer must decline and the
    // general path answers identically (columnar ON == OFF).
    for src in [
        // multi-hop
        "MATCH (a:Aa)-[:R]->(b:Bb)-[:R]->(c) WHERE b.prop < 25 RETURN count(*) AS c",
        // ORDER BY on the aggregate
        "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < 25 RETURN count(*) AS c ORDER BY c",
        // non-count projection
        "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < 25 RETURN b.prop AS c ORDER BY c",
        // a non-constant rhs (references a)
        "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < a.prop RETURN count(*) AS c",
        // a filter on the START side, not the end
        "MATCH (a:Aa)-[:R]->(b:Bb) WHERE a.prop < 25 RETURN count(*) AS c",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
    }
}

// ---------------------------------------------------------------------------
// Layer-4 increment 2: an ARBITRARY predicate over `b` (compound comparisons,
// three-valued AND/OR/XOR/NOT, prop-vs-prop, IS NULL, string equality, params)
// via the vectorized column-expression evaluator. Same differential contract.
// ---------------------------------------------------------------------------

/// Richer graph: b nodes carry three properties (x, y int; name str) with a
/// NULL in each, `a` carries `k`, and edge multiplicity is: b0×2, b1×1, b2×2,
/// b3×1, b4×2 (8 (a,b) pairs) so count(*) exercises multiplicity.
fn g2() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |k: Option<i64>| {
        let mut p = BTreeMap::new();
        if let Some(v) = k {
            p.insert("k".to_string(), Value::Int(v));
        }
        g.create_node(&["Aa2".into()], &p).expect("a")
    };
    let a = [mk_a(Some(100)), mk_a(Some(0))];
    let mk_b = |x: Option<i64>, y: Option<i64>, name: Option<&str>| {
        let mut p = BTreeMap::new();
        if let Some(v) = x {
            p.insert("x".to_string(), Value::Int(v));
        }
        if let Some(v) = y {
            p.insert("y".to_string(), Value::Int(v));
        }
        if let Some(s) = name {
            p.insert("name".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["Bb2".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some(5), Some(10), Some("alice")),
        mk_b(Some(15), Some(10), Some("bob")),
        mk_b(None, Some(20), Some("alice")), // x NULL
        mk_b(Some(8), None, Some("carol")),  // y NULL
        mk_b(Some(12), Some(12), None),      // name NULL
    ];
    for (s, d) in [
        (0, 0),
        (0, 0),
        (0, 1),
        (0, 2),
        (1, 2),
        (1, 3),
        (1, 4),
        (1, 4),
    ] {
        g.create_rel(a[s], "T", b[d], &BTreeMap::new()).expect("T");
    }
    g
}

#[test]
fn general_predicate_equals_general_and_is_correct() {
    let g = g2();
    let cases: &[(&str, i64)] = &[
        // AND range: b0(5),b3(8),b4(12) in [5,15); b1(15) fails hi; b2(null) Unknown.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x >= 5 AND b.x < 15 RETURN count(*) AS c",
            5,
        ),
        // OR: b0(x<6) T; b2(x null OR y20>15 → True wins); b3 (F OR y null Unknown → drop).
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x < 6 OR b.y > 15 RETURN count(*) AS c",
            4,
        ),
        // NOT: keep x>=10 → b1(15),b4(12); b2(null) NOT Unknown = Unknown → drop.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE NOT b.x < 10 RETURN count(*) AS c",
            3,
        ),
        // prop-vs-prop: only b0 (5<10); b2/b3 have a null operand → drop; b4 12<12 F.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x < b.y RETURN count(*) AS c",
            2,
        ),
        // string eq: b0,b2 = "alice"; b4(name null) drops.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.name = 'alice' RETURN count(*) AS c",
            4,
        ),
        // string eq (single, null-adjacent): only b3 "carol".
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.name = 'carol' RETURN count(*) AS c",
            1,
        ),
        // IS NULL: only b4.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.name IS NULL RETURN count(*) AS c",
            2,
        ),
        // IS NOT NULL: b0,b1,b3,b4 (b2.x null).
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x IS NOT NULL RETURN count(*) AS c",
            6,
        ),
        // compound AND across two columns: only b0 (y=10 AND name=alice).
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.y = 10 AND b.name = 'alice' RETURN count(*) AS c",
            2,
        ),
        // XOR: exactly one side true. b0(x<10 T, y>15 F)→T; b1(F,F)→F; b2(null Unknown → drop);
        // b3(x<10 T, y null Unknown → Unknown drop); b4(F,F)→F.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x < 10 XOR b.y > 15 RETURN count(*) AS c",
            2,
        ),
        // Nested NOT(AND): the inner AND is always False (x<0 is false for all),
        // so NOT keeps every row = 8 pairs. This is the canary case: it is the
        // only shape where mishandling `False ∧ Unknown` (b2: Unknown ∧ False)
        // is observable, because NOT False = keep but NOT Unknown = drop.
        (
            "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE NOT (b.x < 0 AND b.y IS NULL) RETURN count(*) AS c",
            8,
        ),
    ];
    for (src, want) in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
        assert_eq!(on, one(*want), "wrong count for `{src}`");
    }
}

#[test]
fn general_predicate_handles_params_in_a_compound() {
    let g = g2();
    let mut p = BTreeMap::new();
    p.insert("lim".to_string(), Value::Int(13));
    p.insert("nm".to_string(), Value::Str("alice".to_string()));
    let (on, off) = both(
        &g,
        "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x < $lim AND b.name = $nm RETURN count(*) AS c",
        p,
    );
    assert_eq!(on, off, "param compound: columnar vs general disagree");
    assert_eq!(on, one(2), "param compound count wrong"); // only b0
}

#[test]
fn general_predicate_declines_unvectorizable_and_falls_back() {
    let g = g2();
    for src in [
        // a function call in the predicate — eval_column has no vectorized form.
        "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE abs(b.x) < 10 RETURN count(*) AS c",
        // a string operator (a non-comparison BinOp).
        "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.name STARTS WITH 'a' RETURN count(*) AS c",
        // references the START variable `a`, not just `b`.
        "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x < a.k RETURN count(*) AS c",
        // non-const arithmetic operand (b.x + 1).
        "MATCH (a:Aa2)-[:T]->(b:Bb2) WHERE b.x + 1 > 10 RETURN count(*) AS c",
        // a third variable via a second (cartesian) path — recognizer declines.
        "MATCH (a:Aa2)-[:T]->(b:Bb2), (c:Bb2) WHERE b.x < c.x RETURN count(*) AS c",
    ] {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
    }
}
