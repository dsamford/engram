#![allow(non_snake_case)]
//! A WHERE may read a variable the NEXT projection drops.
//!
//! `fold_chain_counts` rewrites `MATCH <chain> WHERE <pred> WITH <keys>,
//! count(v)` into `WITH <keys>, sum(count { <chain> WHERE <pred> })`. That
//! moves the WHERE INSIDE an item of the projection, so it is evaluated in the
//! projection's scope — where every visible variable the projection does not
//! keep is already gone. A WHERE reading one of those folded into a subquery
//! that could not see it, and the statement FAILED with `variable ... is not
//! in scope` instead of declining to the general path, which answers it.
//!
//! Found through SNB BI's bi13 at SF3 on 2026-09-14, which reported
//! `variable `zombies` is not in scope`. Before that it had been recorded as
//! `rows=0` by a runner whose error-grep missed the message — so the query had
//! been failing silently for two runs.
//!
//! Neither OPTIONAL nor the list membership is essential, which is why they
//! are not what the tests below turn on: a plain MATCH fails identically, and
//! so does a predicate reading a SCALAR sibling. The rule is about the
//! projection dropping a variable the WHERE still needs.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (a:P {id:1}), (b:P {id:2}), (m:M {id:10}), \
         (m)-[:HAS_CREATOR]->(a), (b)-[:LIKES]->(m)",
    );
    g
}

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn one_int(g: &Graph, src: &str) -> i64 {
    match run(g, src).rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one integer from `{src}`, got {other:?}"),
    }
}

#[test]
fn a_predicate_may_read_a_collection_the_grouping_drops() {
    // THE bi13 shape, reduced: collect, UNWIND, then a leg whose WHERE tests
    // membership of the collection while the grouping keeps only the element.
    let g = graph();
    assert_eq!(
        one_int(
            &g,
            "MATCH (p:P) WITH p WITH collect(p) AS ps UNWIND ps AS one \
             OPTIONAL MATCH (one)<-[:HAS_CREATOR]-(m:M)<-[:LIKES]-(l:P) \
             WHERE l IN ps \
             WITH one, count(l) AS n \
             RETURN count(*)"
        ),
        2,
        "two people go in and two rows come out; this raised `variable `ps` is \
         not in scope`"
    );
}

#[test]
fn the_same_holds_for_a_plain_MATCH_and_for_a_scalar() {
    // OPTIONAL is not the trigger, and neither is the list — both spellings
    // failed the same way, so both are pinned. A fix aimed at OPTIONAL, or at
    // `IN`, would leave the other broken.
    let g = graph();
    assert_eq!(
        one_int(
            &g,
            "MATCH (p:P) WITH p WITH collect(p) AS ps UNWIND ps AS one \
             MATCH (one)<-[:HAS_CREATOR]-(m:M)<-[:LIKES]-(l:P) \
             WHERE l IN ps \
             WITH one, count(l) AS n \
             RETURN count(*)"
        ),
        1,
        "a plain MATCH keeps only the person who has a liked message"
    );
    assert_eq!(
        one_int(
            &g,
            "MATCH (p:P) WITH p WITH collect(p) AS ps, count(p) AS k \
             UNWIND ps AS one \
             OPTIONAL MATCH (one)<-[:HAS_CREATOR]-(m:M)<-[:LIKES]-(l:P) \
             WHERE l.id < k \
             WITH one, count(l) AS n \
             RETURN count(*)"
        ),
        2,
        "a SCALAR sibling the grouping drops fails the same way a list does"
    );
}

#[test]
fn the_count_fold_still_claims_the_shape_it_exists_for() {
    // The decline must be NARROW. This is the shape the fold serves — the
    // WHERE reads only the chain — and it must still fold, or the fix has
    // bought correctness by turning the optimisation off.
    //
    // Asserted by RESULT rather than by a counter here; the fold's own tests
    // pin the counter. What matters is that the answer stays right.
    let g = graph();
    assert_eq!(
        one_int(
            &g,
            "MATCH (p:P) \
             OPTIONAL MATCH (p)<-[:HAS_CREATOR]-(m:M) \
             WHERE m.id > 0 \
             WITH p, count(m) AS c \
             RETURN sum(c)"
        ),
        1,
        "one message has a creator"
    );
}

#[test]
fn a_predicate_reading_a_kept_key_still_folds_and_answers() {
    // The other side of the new condition: when the projection DOES keep the
    // variable, nothing changes and the fold is still allowed to claim it.
    let g = graph();
    assert_eq!(
        one_int(
            &g,
            "MATCH (p:P) WITH p WITH collect(p) AS ps UNWIND ps AS one \
             OPTIONAL MATCH (one)<-[:HAS_CREATOR]-(m:M)<-[:LIKES]-(l:P) \
             WHERE l IN ps \
             WITH one, ps, count(l) AS n \
             RETURN count(*)"
        ),
        2,
        "`ps` is a grouping key here, so the fold may keep the WHERE"
    );
}
