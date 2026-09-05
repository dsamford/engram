//! The grouping/DISTINCT equivalence contract, PINNED against behaviour
//! measured on the live Neo4j server 2026-08-21 (see
//! docs/remediation-plan.md, item G0). Before this contract existed the
//! engine carried FOUR dedup implementations that disagreed with each
//! other — strict grouping, strict projection-DISTINCT, strict UNION, and
//! eq3-based aggregate-DISTINCT — and three of them with Neo4j.
use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_params(g, src, BTreeMap::new())
}

fn run_params(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

#[test]
fn numerically_equal_int_and_float_are_one_group_one_distinct_row() {
    // Measured: UNWIND [1, 1.0, 2] … count(*) → {1: 2}, {2: 1}, with the
    // FIRST-SEEN value as the representative.
    let g = graph();
    assert_eq!(
        run(
            &g,
            "UNWIND [1, 1.0, 2] AS x RETURN x, count(*) AS c ORDER BY c DESC"
        ),
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );
    assert_eq!(
        run(&g, "UNWIND [1, 1.0, 2] AS x RETURN DISTINCT x"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    assert_eq!(
        run(
            &g,
            "UNWIND [1, 1.0, 2] AS x RETURN collect(DISTINCT x) AS xs"
        ),
        vec![vec![Value::List(vec![Value::Int(1), Value::Int(2)])]]
    );
    // Float first: the representative is the float, the KEY is still one.
    assert_eq!(
        run(
            &g,
            "UNWIND [1.0, 1, 2] AS x RETURN x, count(*) AS c ORDER BY c DESC"
        ),
        vec![
            vec![Value::Float(1.0), Value::Int(2)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );
}

#[test]
fn no_cross_family_equivalence() {
    // Measured: 1 and '1' are two groups.
    let g = graph();
    let rows = run(&g, "UNWIND [1, '1'] AS x RETURN x, count(*) AS c");
    assert_eq!(rows.len(), 2, "int and string never share a key: {rows:?}");
}

#[test]
fn null_is_one_key_outside_aggregates_and_skipped_inside_them() {
    // Measured: DISTINCT keeps ONE null row; grouping groups nulls
    // (count 2); count(DISTINCT) over [null, null, 1, 1.0] is 1 — nulls
    // are skipped as aggregate INPUTS, and 1/1.0 unify.
    let g = graph();
    assert_eq!(
        run(&g, "UNWIND [null, null, 1] AS x RETURN DISTINCT x"),
        vec![vec![Value::Null], vec![Value::Int(1)]]
    );
    assert_eq!(
        run(
            &g,
            "UNWIND [null, null, 1] AS x RETURN x, count(*) AS c ORDER BY c DESC"
        ),
        vec![
            vec![Value::Null, Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
        ]
    );
    assert_eq!(
        run(
            &g,
            "UNWIND [null, null, 1, 1.0] AS x RETURN count(DISTINCT x) AS c"
        ),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn nan_never_collapses() {
    // Measured: two sqrt(-1) rows BOTH survive DISTINCT — NaN is not
    // equivalent even to itself. NaN arrives via a parameter because the
    // contract is about values, not about any function that produces them.
    let g = graph();
    let params: BTreeMap<String, Value> = [(
        "xs".to_string(),
        Value::List(vec![
            Value::Float(f64::NAN),
            Value::Float(f64::NAN),
            Value::Int(1),
        ]),
    )]
    .into_iter()
    .collect();
    let rows = run_params(&g, "UNWIND $xs AS x RETURN DISTINCT x", params.clone());
    assert_eq!(rows.len(), 3, "each NaN is its own key: {rows:?}");
    let rows = run_params(&g, "UNWIND $xs AS x RETURN count(DISTINCT x) AS c", params);
    assert_eq!(rows, vec![vec![Value::Int(3)]]);
}

#[test]
fn negative_zero_keys_as_zero() {
    // 0.0 = -0.0 is TRUE, so they are one key — and one with Int(0).
    let g = graph();
    let params: BTreeMap<String, Value> = [(
        "xs".to_string(),
        Value::List(vec![Value::Float(0.0), Value::Float(-0.0), Value::Int(0)]),
    )]
    .into_iter()
    .collect();
    let rows = run_params(&g, "UNWIND $xs AS x RETURN count(DISTINCT x) AS c", params);
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn union_distinct_shares_the_same_equivalence() {
    let g = graph();
    assert_eq!(
        run(
            &g,
            "UNWIND [1] AS x RETURN x UNION UNWIND [1.0, 2] AS x RETURN x"
        ),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]],
        "UNION dedup unifies numerics exactly as grouping does"
    );
}
