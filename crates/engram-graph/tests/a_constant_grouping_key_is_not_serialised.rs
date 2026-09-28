#![allow(non_snake_case)]
//! A grouping key that cannot split groups is not serialised per row.
//!
//! `agg_key_of` encodes EVERY key value for EVERY row. When a key is a large
//! collected list that is identical on every row, that is O(rows x list) to
//! produce keys that are all the same. SNB BI's bi10 carries a 17,743-node
//! list across 22,842 rows; measured at SF3 on 2026-09-15, `WITH near,
//! collect(b) AS far` did not finish in 180 s, while the two variable-length
//! expansions that BUILD those lists took 1 s and 0 s. The traversal was never
//! the cost. Neo4j answers the whole query in 0 s, and the shape is LDBC's own
//! text.
//!
//! The claim that makes it safe: a `WITH` whose items ALL aggregate has no
//! grouping key, so it emits EXACTLY ONE ROW, so every name it binds has one
//! value downstream. Such a name cannot separate two groups.
//!
//! These tests are about ANSWERS. A key wrongly called constant MERGES groups
//! that must stay apart, which is the worst failure available here, so the
//! cases that must NOT be treated as constant are tested alongside the one
//! that must.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (:P {g: 1, v: 10}), (:P {g: 1, v: 11}), (:P {g: 2, v: 20}), (:P {g: 2, v: 21})",
    );
    g
}

#[test]
fn the_bi10_shape_answers_with_the_constant_key_left_out() {
    // `WITH collect(a) AS near` has no grouping item, so `near` is one value
    // for every row after it — the key is dropped and re-attached from the
    // group's template.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WITH collect(a) AS near \
         MATCH (b:P) WITH near, collect(b) AS far \
         RETURN size(near), size(far)",
    );
    let row = r.rows.first().expect("one row");
    assert_eq!(
        (row.first(), row.get(1)),
        (Some(&Value::Int(4)), Some(&Value::Int(4))),
        "both lists hold all four nodes: {:?}",
        r.rows
    );
    assert_eq!(r.rows.len(), 1, "one group, so one row: {:?}", r.rows);
}

#[test]
fn a_REAL_grouping_key_still_splits_groups() {
    // The failure that would matter: `g` VARIES per row and must keep its
    // groups apart. It is not bound by a no-key aggregation, so it is not
    // constant and must still be keyed.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WITH a.g AS grp, count(*) AS n RETURN grp, n ORDER BY grp",
    );
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => Some((*a, *b)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, 2), (2, 2)],
        "two groups of two, NOT one merged group: {:?}",
        r.rows
    );
}

#[test]
fn a_constant_alongside_a_real_key_does_not_collapse_the_real_one() {
    // The mixed case: one constant key and one varying key in the same
    // projection. Dropping the constant must not drop the other.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WITH collect(a) AS near \
         MATCH (b:P) WITH near, b.g AS grp, count(*) AS n \
         RETURN grp, n, size(near) ORDER BY grp",
    );
    let got: Vec<(i64, i64, i64)> = r
        .rows
        .iter()
        .filter_map(|row| match (row.first(), row.get(1), row.get(2)) {
            (Some(Value::Int(a)), Some(Value::Int(b)), Some(Value::Int(c))) => Some((*a, *b, *c)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, 2, 4), (2, 2, 4)],
        "still two groups, each carrying the constant list: {:?}",
        r.rows
    );
}

#[test]
fn a_name_REBOUND_after_the_aggregation_is_no_longer_constant() {
    // `xs` is constant after the collect, then UNWIND rebinds it per row. If
    // the analysis still called it constant, these groups would merge.
    let g = graph();
    let r = run(
        &g,
        "MATCH (a:P) WITH collect(a.g) AS gs \
         UNWIND gs AS gs \
         WITH gs, count(*) AS n RETURN gs, n ORDER BY gs",
    );
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => Some((*a, *b)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, 2), (2, 2)],
        "UNWIND rebinds `gs`, so it groups normally: {:?}",
        r.rows
    );
}
