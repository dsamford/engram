#![allow(non_snake_case)]
//! `UNION` inside `CALL { }`.
//!
//! Refused by name until SNB BI's Q4 needed it. That query's subquery counts
//! each person's messages in one arm and adds back the top-forum members who
//! have NONE in the other, with `UNION ALL` between them and a `sum()` over
//! the result — so the feature is not a convenience there, it is the only way
//! to say "and zero for everyone else" in one subquery.
//!
//! The rule is the top-level one applied at subquery scope: every arm runs
//! against the SAME seed row, the arms concatenate in order, and `UNION`
//! without `ALL` deduplicates.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn err(g: &Graph, src: &str) -> String {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    match run_query(g, &q, BTreeMap::new()) {
        Ok(r) => panic!("expected a refusal, got {r:?}"),
        Err(e) => format!("{e}"),
    }
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

#[test]
fn both_arms_run_and_concatenate() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let r = run(
        &g,
        "CALL { RETURN 1 AS x UNION ALL RETURN 2 AS x } RETURN x",
    );
    assert_eq!(ints(&r), vec![1, 2], "both arms contribute a row");
}

#[test]
fn UNION_without_ALL_deduplicates_and_UNION_ALL_does_not() {
    // The two spellings must actually DIFFER, or the `all` flag is being
    // ignored and one of them is quietly wrong.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    assert_eq!(
        ints(&run(
            &g,
            "CALL { RETURN 7 AS x UNION ALL RETURN 7 AS x } RETURN x"
        )),
        vec![7, 7],
        "UNION ALL keeps the duplicate"
    );
    assert_eq!(
        ints(&run(
            &g,
            "CALL { RETURN 7 AS x UNION RETURN 7 AS x } RETURN x"
        )),
        vec![7],
        "UNION without ALL drops it"
    );
}

#[test]
fn each_arm_sees_the_same_seed_row() {
    // The property that makes this a SUBQUERY rather than a second statement.
    // A per-arm seed that went missing would show up as an empty arm, and with
    // `UNION ALL` the other arm's rows would still come back — so the count is
    // asserted, not just non-emptiness.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:N {v: 10}), (:N {v: 20})");
    let r = run(
        &g,
        "MATCH (n:N) CALL { WITH n RETURN n.v AS y UNION ALL WITH n RETURN n.v * 2 AS y }          RETURN y ORDER BY y",
    );
    assert_eq!(
        ints(&r),
        vec![10, 20, 20, 40],
        "two seed rows x two arms; 20 appears twice because 10*2 and 20 collide          under UNION ALL, which is exactly the duplicate that must survive"
    );
}

#[test]
fn arms_that_project_different_columns_are_refused() {
    // MORE important inside a subquery than at top level: the caller binds the
    // subquery's columns back into the outer row by NAME, so disagreeing arms
    // would bind different names on different rows rather than fail.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let e = err(
        &g,
        "CALL { RETURN 1 AS x UNION ALL RETURN 2 AS y } RETURN x",
    );
    assert!(
        e.contains("same columns"),
        "the refusal must name the disagreement: {e}"
    );
}

#[test]
fn the_BI4_shape_adds_back_the_rows_with_no_messages() {
    // bi4 in miniature. Two Persons are members of a Forum; only one of them
    // wrote a Message. The first arm counts messages, the second contributes a
    // 0 for every member, and the outer `sum()` folds the two together — so
    // the member with nothing must come back with 0 rather than vanish.
    //
    // This is the behaviour the whole feature exists for: an INNER JOIN over
    // the message pattern alone silently drops that person, and a result that
    // is merely SHORTER is the failure nobody notices.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (f:Forum {id: 1}),
                (a:Person {id: 10}), (b:Person {id: 20}),
                (m:Message {id: 100}),
                (f)-[:HAS_MEMBER]->(a), (f)-[:HAS_MEMBER]->(b),
                (m)-[:HAS_CREATOR]->(a)",
    );
    let r = run(
        &g,
        "MATCH (f:Forum)          CALL {            WITH f            MATCH (f)-[:HAS_MEMBER]->(p:Person)<-[:HAS_CREATOR]-(m:Message)            RETURN p, count(DISTINCT m) AS messageCount          UNION ALL            WITH f            MATCH (f)-[:HAS_MEMBER]->(p:Person)            RETURN p, 0 AS messageCount          }          RETURN p.id AS personId, sum(messageCount) AS messageCount          ORDER BY personId",
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
        vec![(10, 1), (20, 0)],
        "person 20 wrote nothing and must still appear, with 0: {:?}",
        r.rows
    );
}
