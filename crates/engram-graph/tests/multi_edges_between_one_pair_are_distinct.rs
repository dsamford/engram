#![allow(non_snake_case)]
//! FINBENCH PREREQUISITE: N edges of ONE type between ONE ordered pair.
//!
//! Five FinBench edge types (`transfer`, `withdraw`, `signIn`, `deposit`,
//! `repay`) permit multiplicity N between the same ordered pair, and the task
//! force's profiling reports the hub multiplicity distribution is itself
//! power-law. An engine that keys adjacency by `(src, dst)`, or that
//! deduplicates, does not fail loudly on such a corpus -- it loads clean,
//! answers every query, and reports a smaller graph than the one it was given.
//!
//! That is the failure this file exists to make impossible to ship unnoticed.
//! It needs no corpus and no loader, because the question is about the store.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}
fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}
fn count(g: &Graph, src: &str) -> i64 {
    let r = run(g, src);
    match r.rows.first().and_then(|row| row.first()) {
        Some(engram_cypher::Value::Int(n)) => *n,
        other => panic!("expected one integer row from `{src}`, got {other:?}"),
    }
}

#[test]
fn three_transfers_between_one_pair_are_three_relationships() {
    let g = graph();
    run(&g, "CREATE (:Account {id: 1})");
    run(&g, "CREATE (:Account {id: 2})");
    for amt in [10, 20, 30] {
        run(
            &g,
            &format!(
                "MATCH (a:Account {{id: 1}}), (b:Account {{id: 2}}) \
                 CREATE (a)-[:TRANSFER {{amount: {amt}}}]->(b)"
            ),
        );
    }
    let n = count(
        &g,
        "MATCH (:Account {id: 1})-[t:TRANSFER]->(:Account {id: 2}) RETURN count(t) AS n",
    );
    assert_eq!(
        n, 3,
        "three TRANSFER edges were written between one ordered pair and the \
         store reports {n}. A corpus with power-law edge multiplicity would \
         load clean and measure a smaller graph than the one supplied."
    );
}

#[test]
fn each_of_the_multi_edges_keeps_its_own_properties() {
    // Counting three is necessary but not sufficient: three edges that all
    // report the LAST amount would still count three.
    let g = graph();
    run(&g, "CREATE (:Account {id: 1})");
    run(&g, "CREATE (:Account {id: 2})");
    for amt in [10, 20, 30] {
        run(
            &g,
            &format!(
                "MATCH (a:Account {{id: 1}}), (b:Account {{id: 2}}) \
                 CREATE (a)-[:TRANSFER {{amount: {amt}}}]->(b)"
            ),
        );
    }
    let r = run(
        &g,
        "MATCH (:Account {id: 1})-[t:TRANSFER]->(:Account {id: 2}) \
         RETURN t.amount AS a ORDER BY a",
    );
    let got: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(engram_cypher::Value::Int(n)) => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![10, 20, 30],
        "the three multi-edges must each keep their own amount; got {got:?}"
    );
}

#[test]
fn the_sum_over_multi_edges_is_the_sum_of_all_of_them() {
    // The shape FinBench's TCR queries actually take: an aggregate over the
    // edges between a pair. A collapse shows here as a wrong total rather
    // than as a missing row.
    let g = graph();
    run(&g, "CREATE (:Account {id: 1})");
    run(&g, "CREATE (:Account {id: 2})");
    for amt in [10, 20, 30] {
        run(
            &g,
            &format!(
                "MATCH (a:Account {{id: 1}}), (b:Account {{id: 2}}) \
                 CREATE (a)-[:TRANSFER {{amount: {amt}}}]->(b)"
            ),
        );
    }
    let n = count(
        &g,
        "MATCH (:Account {id: 1})-[t:TRANSFER]->(:Account {id: 2}) \
         RETURN sum(t.amount) AS n",
    );
    assert_eq!(n, 60, "sum over the multi-edges must be 60, got {n}");
}
