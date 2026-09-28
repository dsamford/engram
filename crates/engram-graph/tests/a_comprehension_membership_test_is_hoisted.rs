#![allow(non_snake_case)]
//! A comprehension's membership test against a loop-invariant list is hoisted.
//!
//! `[n IN far WHERE NOT n IN near]` evaluates `n IN near` once per element of
//! `far`, and `IN` is a LINEAR SCAN, so the comprehension costs |far| x |near|.
//!
//! SNB BI's bi10 is 22,842 x 17,743 — 405 MILLION `eq3` calls. Measured at SF3
//! on 2026-09-15 it did not finish in 300 s, while the two variable-length
//! expansions that BUILD those lists took 1 s and 0 s. The traversal was never
//! the cost; the set difference was. Neo4j answers the whole query in 0 s.
//!
//! These tests pin the ANSWER, not the speed — a fast wrong set difference
//! would be worse than a slow right one. The shapes that must fall back to the
//! scan are tested too, because the hoist is only equivalent for a homogeneous
//! list of NODES: nodes compare by identity, while `eq3` coerces Int/Float,
//! compares DateTimes by instant and recurses into Lists.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn ints(r: &QueryResult) -> Vec<i64> {
    let mut v: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        })
        .collect();
    v.sort_unstable();
    v
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (:P {v: 1}), (:P {v: 2}), (:P {v: 3}), (:P {v: 4})",
    );
    g
}

#[test]
fn a_set_difference_over_nodes_keeps_exactly_the_non_members() {
    // THE bi10 shape: two node lists, one filtered against the other.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WHERE a.v <= 2 WITH collect(a) AS near \
         MATCH (b:P) WITH near, collect(b) AS far \
         WITH [n IN far WHERE NOT n IN near] AS cand \
         UNWIND cand AS c RETURN c.v ORDER BY c.v",
    );
    assert_eq!(
        ints(&r),
        vec![3, 4],
        "four nodes minus the two in `near` leaves exactly 3 and 4"
    );
}

#[test]
fn the_un_negated_form_keeps_exactly_the_members() {
    // `IN` rather than `NOT IN` — the hoist must not invert the sense.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WHERE a.v <= 2 WITH collect(a) AS near \
         MATCH (b:P) WITH near, collect(b) AS far \
         WITH [n IN far WHERE n IN near] AS keep \
         UNWIND keep AS c RETURN c.v ORDER BY c.v",
    );
    assert_eq!(ints(&r), vec![1, 2], "the intersection, not its complement");
}

#[test]
fn an_empty_haystack_keeps_everything_under_negation() {
    // An empty list is not hoisted (nothing to build a set from) and must
    // still answer: `NOT n IN []` is TRUE for every n.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WHERE a.v > 100 WITH collect(a) AS near \
         MATCH (b:P) WITH near, collect(b) AS far \
         WITH [n IN far WHERE NOT n IN near] AS cand \
         UNWIND cand AS c RETURN c.v ORDER BY c.v",
    );
    assert_eq!(ints(&r), vec![1, 2, 3, 4], "nothing is excluded");
}

#[test]
fn a_scalar_list_is_not_hoisted_and_still_answers_by_eq3() {
    // Ints and Floats COERCE under `eq3` (`1 = 1.0` is true), which a set of
    // node ids cannot express — so this shape must take the scan. If the hoist
    // ever claimed it, this is the test that fails.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let r = run(
        &g,
        "WITH [1, 2, 3] AS xs, [1.0, 3.0] AS ys \
         RETURN [x IN xs WHERE NOT x IN ys] AS out",
    );
    let got = r.rows.first().and_then(|row| row.first()).cloned();
    assert_eq!(
        got,
        Some(Value::List((vec![Value::Int(2)]).into())),
        "1 and 3 match their float twins under eq3, so only 2 survives: {got:?}"
    );
}

#[test]
fn a_map_over_the_survivors_still_runs() {
    // The hoisted path binds the comprehension variable lazily, only when
    // there IS a map. A map must therefore still see its binding.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WHERE a.v <= 2 WITH collect(a) AS near \
         MATCH (b:P) WITH near, collect(b) AS far \
         RETURN [n IN far WHERE NOT n IN near | n.v] AS vs",
    );
    let got = r.rows.first().and_then(|row| row.first()).cloned();
    assert_eq!(
        got,
        Some(Value::List((vec![Value::Int(3), Value::Int(4)]).into())),
        "the map projects v off each survivor: {got:?}"
    );
}
