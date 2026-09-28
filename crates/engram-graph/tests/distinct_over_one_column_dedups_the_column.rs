#![allow(non_snake_case)]
//! Fix 94: `RETURN DISTINCT n.p` over a columnar projection's survivors
//! deduplicates the column itself — the production `MATCH (n:UserDataNode
//! {nodeType: 'email'}) WHERE n.userId IS NOT NULL RETURN DISTINCT
//! n.userId AS userId` bound, evaluated, boxed and canonically re-keyed
//! every one of 18,373 survivors to keep two values (12.4 ms against
//! Neo4j's 6.9 on the mirror).
//!
//! The rows are pinned in first-appearance order for strings, for a mixed
//! column of integers and nulls, and against the shapes that keep the
//! row-wise pass.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const ONE_COLUMN: &str = "interp.columnar projection deduplicated its one column";

/// 6,000 emails over three users (user 0 first), 500 emails with no user,
/// 200 posts; a `score` on every third email (0, 1 or 2), none elsewhere.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert(
            "userId".to_string(),
            Value::Str(format!("user-{}", [0, 0, 1, 0, 2, 1][(i % 6) as usize])),
        );
        if i % 3 == 0 {
            m.insert("score".to_string(), Value::Int((i / 3) % 3));
        }
        g.create_node(&["UserDataNode".into()], &m).expect("email");
    }
    for _ in 0..500 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        g.create_node(&["UserDataNode".into()], &m).expect("orphan");
    }
    for i in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("post".into()));
        m.insert("userId".to_string(), Value::Str(format!("poster-{i}")));
        g.create_node(&["UserDataNode".into()], &m).expect("post");
    }
    g
}

fn strs(v: &[&str]) -> Vec<Vec<Value>> {
    v.iter().map(|s| vec![Value::Str((*s).into())]).collect()
}

/// The users in first-appearance order, from the column; a mixed column
/// keeps one null and every integer.
#[test]
fn a_the_distinct_values_come_from_the_column_in_first_appearance_order() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.userId IS NOT NULL RETURN DISTINCT n.userId AS userId",
    );
    assert_eq!(got, strs(&["user-0", "user-1", "user-2"]));
    assert_eq!(count_of(&c, ONE_COLUMN), 1, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.nodeType = 'email' RETURN DISTINCT n.score AS s",
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(0)],
            vec![Value::Null],
            vec![Value::Int(1)],
            vec![Value::Int(2)]
        ]
    );
    assert_eq!(count_of(&c, ONE_COLUMN), 1, "{c:?}");
}

/// CONTROLS: two items, an ORDER BY, and no DISTINCT keep the row-wise
/// pass — and answer the same values.
#[test]
fn b_two_items_an_order_by_and_no_distinct_keep_the_rows() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.userId IS NOT NULL RETURN DISTINCT n.userId AS userId, n.nodeType AS t",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(
        got[0],
        vec![Value::Str("user-0".into()), Value::Str("email".into())]
    );
    assert_eq!(count_of(&c, ONE_COLUMN), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.userId IS NOT NULL RETURN DISTINCT n.userId AS userId ORDER BY userId DESC",
    );
    assert_eq!(got, strs(&["user-2", "user-1", "user-0"]));
    assert_eq!(count_of(&c, ONE_COLUMN), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.userId IS NOT NULL RETURN n.userId AS userId",
    );
    assert_eq!(got.len(), 6000);
    assert_eq!(count_of(&c, ONE_COLUMN), 0, "{c:?}");
}
