//! A label test inside an aggregate sees the node's labels.
//!
//! SNB BI15's weight is `sum(CASE WHEN msg:Post THEN 10 ELSE 5 END)` over
//! reply pairs whose thread lives in a forum created in a window. At SF3
//! engram answered `msg:Post` FALSE for every message in that aggregate while
//! `RETURN msg:Post` on the same rows said true — the pair's weight came out
//! 35 where PostgreSQL and Neo4j both compute 55, and BI15's answer was wrong.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

/// Two people; a forum with a post by `a`; `b` comments on the post; `a`
/// replies to that comment. So (parent by a, reply by b) is a POST parent,
/// (parent by b, reply by a) is a COMMENT parent.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (a:Person {k: 1}), (b:Person {k: 2}), (a)-[:KNOWS]->(b), \
         (f:Forum {d: 5}), \
         (p:Message:Post {k: 10})-[:HAS_CREATOR]->(a), (f)-[:CONTAINER_OF]->(p), \
         (c1:Message:Comment {k: 11})-[:HAS_CREATOR]->(b), (c1)-[:REPLY_OF]->(p), \
         (c2:Message:Comment {k: 12})-[:HAS_CREATOR]->(a), (c2)-[:REPLY_OF]->(c1)",
    );
    g
}

const SHAPE: &str = "MATCH (p1:Person)<-[:HAS_CREATOR]-(msg:Message)<-[:REPLY_OF]-(reply:Message)-[:HAS_CREATOR]->(p2:Person) \
     MATCH (p1)-[:KNOWS]-(p2) \
     MATCH (msg)-[:REPLY_OF*0..]->(:Post)<-[:CONTAINER_OF]-(f:Forum) WHERE f.d >= 1 AND f.d <= 9 ";

#[test]
fn the_label_test_is_true_for_a_post_inside_a_sum() {
    let g = graph();
    let got = run(
        &g,
        &format!(
            "{SHAPE} RETURN p1.k, p2.k, sum(CASE WHEN msg:Post THEN 10 ELSE 5 END) AS w ORDER BY p1.k"
        ),
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(2), Value::Int(10)],
            vec![Value::Int(2), Value::Int(1), Value::Int(5)],
        ]
    );
}

#[test]
fn the_aggregate_agrees_with_the_rows_it_aggregates() {
    let g = graph();
    let rows = run(
        &g,
        &format!("{SHAPE} RETURN p1.k, msg.k, msg:Post AS isPost ORDER BY p1.k"),
    );
    let mut want: BTreeMap<i64, i64> = BTreeMap::new();
    for r in &rows {
        let (Value::Int(k), Value::Bool(post)) = (&r[0], &r[2]) else {
            panic!("{r:?}")
        };
        *want.entry(*k).or_default() += if *post { 10 } else { 5 };
    }
    for form in [
        "RETURN p1.k AS k, sum(CASE WHEN msg:Post THEN 10 ELSE 5 END) AS w",
        "RETURN id(p1) AS i, p1.k AS k, sum(CASE WHEN msg:Post THEN 10 ELSE 5 END) AS w",
        "WITH p1, CASE WHEN msg:Post THEN 10 ELSE 5 END AS s RETURN p1.k AS k, sum(s) AS w",
        "RETURN p1.k AS k, sum(CASE WHEN 'Post' IN labels(msg) THEN 10 ELSE 5 END) AS w",
    ] {
        let got: BTreeMap<i64, i64> = run(&g, &format!("{SHAPE} {form}"))
            .into_iter()
            .map(|r| {
                let k = r.iter().rev().nth(1).cloned();
                match (k, r.last()) {
                    (Some(Value::Int(k)), Some(Value::Int(w))) => (k, *w),
                    other => panic!("{other:?}"),
                }
            })
            .collect();
        assert_eq!(got, want, "{form}");
    }
}

#[test]
fn a_test_of_the_patterns_own_label_keeps_the_bare_bind() {
    // THE OPTIMISATION STAYS WHERE IT IS SOUND: a label the pattern already
    // requires is carried by the bare bind, so testing it needs no record.
    let g = graph();
    let (rows, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (p1:Person)<-[:HAS_CREATOR]-(msg:Message)<-[:REPLY_OF]-(reply:Message) \
             RETURN p1.k, sum(CASE WHEN msg:Message THEN 1 ELSE 0 END) AS n ORDER BY p1.k",
        )
    });
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );
    assert!(
        t.counters()
            .get("interp.matcher bound a hop end bare")
            .copied()
            .unwrap_or(0)
            > 0,
        "{:?}",
        t.counters()
    );
}
