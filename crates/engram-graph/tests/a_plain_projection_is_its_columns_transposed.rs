#![allow(non_snake_case)]
//! Fix 126: when every projected item is a bare COLUMN LOCAL, the rows are
//! the walk's columns transposed and nothing else.
//!
//! `MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 5000` spent
//! its whole per-member loop clearing a `Scope`, binding two locals into it,
//! and evaluating two `Expr::Var` lookups back out — five thousand times, to
//! move two values the walk already had in hand. That statement is the
//! platform benchmark's `plat-limit-listing`, one of only two shapes where
//! engram trails Neo4j.
//!
//! The emit TAKES each value out of its column, exactly as `Walk::bind` does,
//! which is why the recogniser refuses a projection naming the same column
//! twice: the second reader would see the `Null` left behind. Test `d` is
//! that case, and it is the one that would otherwise return a silent `null`.
//!
//! Canary, run: dropping the `cis.contains(&ci)` guard makes `d` return
//! `[Int(0), Null]` for `RETURN p.id AS a, p.id AS b` while every other test
//! here stays green.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const EMITTED: &str = "interp.columnar projection emitted its rows from the columns";

const PERSONS: i64 = 500;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, t) = engram_observe::with_trace(|| rows(g, src));
    (r, t.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("firstName".to_string(), Value::Str(format!("P{i}")));
        // Absent on every third person, so the Null path is exercised and the
        // answer is not simply "every column has a value".
        if i % 3 != 0 {
            m.insert("nickname".to_string(), Value::Str(format!("N{i}")));
        }
        g.create_node(&["Person".into()], &m).expect("person");
    }
    g.shared_store().seal();
    g
}

#[test]
fn a_the_transpose_fires_and_answers_by_arithmetic() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 50",
    );
    assert!(
        count_of(&c, EMITTED) > 0,
        "the transpose did not fire, so this file proves nothing: {c:?}"
    );
    let want: Vec<Vec<Value>> = (0..50)
        .map(|i| vec![Value::Int(i), Value::Str(format!("P{i}"))])
        .collect();
    assert_eq!(
        got, want,
        "pinned against the corpus, not against a first read"
    );
}

#[test]
fn b_a_missing_value_is_null_not_a_skipped_row() {
    let g = corpus();
    let got = rows(
        &g,
        "MATCH (p:Person) RETURN p.id AS id, p.nickname AS nick LIMIT 6",
    );
    let want: Vec<Vec<Value>> = (0..6)
        .map(|i| {
            let nick = if i % 3 == 0 {
                Value::Null
            } else {
                Value::Str(format!("N{i}"))
            };
            vec![Value::Int(i), nick]
        })
        .collect();
    assert_eq!(got, want, "an absent property changed the row shape");
}

#[test]
fn c_the_shapes_that_need_the_scope_still_decline() {
    let g = corpus();
    // Each of these must NOT take the transpose: a bare node, DISTINCT, an
    // ORDER BY, an expression, and a projected id.
    for q in [
        "MATCH (p:Person) RETURN p LIMIT 5",
        "MATCH (p:Person) RETURN DISTINCT p.firstName AS n LIMIT 5",
        "MATCH (p:Person) RETURN p.id AS id ORDER BY id DESC LIMIT 5",
        "MATCH (p:Person) RETURN p.id + 1 AS id LIMIT 5",
        "MATCH (p:Person) RETURN id(p) AS i LIMIT 5",
    ] {
        let (got, c) = traced(&g, q);
        assert_eq!(
            count_of(&c, EMITTED),
            0,
            "the transpose claimed a shape that needs the scope: {q}"
        );
        assert!(!got.is_empty(), "{q} returned nothing");
    }
}

#[test]
fn d_the_same_column_twice_declines_rather_than_returning_null() {
    // The emit TAKES values out of the column, so a projection naming one
    // column twice would read `Null` the second time. The recogniser refuses
    // it and the per-member loop answers instead.
    let g = corpus();
    let (got, c) = traced(&g, "MATCH (p:Person) RETURN p.id AS a, p.id AS b LIMIT 3");
    assert_eq!(
        count_of(&c, EMITTED),
        0,
        "the transpose served a projection naming one column twice: {c:?}"
    );
    let want: Vec<Vec<Value>> = (0..3).map(|i| vec![Value::Int(i), Value::Int(i)]).collect();
    assert_eq!(got, want, "the second copy of the column came back wrong");
}

#[test]
fn e_the_transpose_answers_exactly_what_the_per_member_loop_answers() {
    // The whole claim, stated as a differential against the shape the
    // recogniser declines: `RETURN p.id, p.firstName` and
    // `RETURN p.id, p.firstName, p.id` cover the same members with the same
    // values, so column one and column two must agree between them.
    let g = corpus();
    let fast = rows(
        &g,
        "MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 200",
    );
    let slow = rows(
        &g,
        "MATCH (p:Person) RETURN p.id AS id, p.firstName AS name, p.id AS again LIMIT 200",
    );
    assert_eq!(fast.len(), slow.len());
    for (f, s) in fast.iter().zip(&slow) {
        assert_eq!(f[0], s[0], "the id column disagrees between the two paths");
        assert_eq!(
            f[1], s[1],
            "the name column disagrees between the two paths"
        );
    }
}
