#![allow(non_snake_case)]
//! A relationship pattern used as a BOOLEAN VALUE, not only as a filter.
//!
//! LDBC SNB Interactive IC7 returns
//!
//!     not((liker)-[:KNOWS]-(person)) AS isNew
//!
//! — a bare pattern in a projection. Strict openCypher calls that
//! `UnexpectedSyntax` and this engine refused it, naming `exists(...)` as the
//! remedy. It was the ONE statement of the twenty in the Interactive catalogue
//! that could not be read at all, so the family could never reach a full parse
//! over a spelling the benchmark itself publishes and Neo4j accepts.
//!
//! Parsing it is not enough to claim it: a pattern predicate has to EVALUATE to
//! a boolean in a projection exactly as it does in a `WHERE`, and both answers
//! have to be right — the pattern that exists and the pattern that does not.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

/// Three people; 1 knows 2 and does not know 3.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 1..=3 {
        run(&g, &format!("CREATE (:Person {{id: {i}}})"));
    }
    run(
        &g,
        "MATCH (a:Person {id: 1}), (b:Person {id: 2}) CREATE (a)-[:KNOWS]->(b)",
    );
    g
}

#[test]
fn the_fixture_holds_one_edge_and_one_non_edge() {
    // Guard first: if the KNOWS edge failed to write, every assertion below
    // would read `false` for both rows and look like agreement.
    let g = graph();
    let r = run(&g, "MATCH ()-[k:KNOWS]->() RETURN count(k) AS c");
    assert_eq!(r.rows.first().and_then(|x| x.first()), Some(&Value::Int(1)));
}

#[test]
fn a_bare_pattern_in_a_projection_answers_true_and_false() {
    let g = graph();
    let r = run(
        &g,
        "MATCH (p:Person {id: 1}), (o:Person) WHERE o.id <> 1 \
         RETURN o.id AS id, (p)-[:KNOWS]-(o) AS known ORDER BY id",
    );
    let got: Vec<(i64, bool)> = r
        .rows
        .iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(Value::Int(i)), Some(Value::Bool(b))) => Some((*i, *b)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![(2, true), (3, false)],
        "person 1 knows 2 and not 3; a pattern in a projection must say so, \
         got {:?}",
        r.rows
    );
}

#[test]
fn the_IC7_spelling_negated_is_the_one_that_was_refused() {
    // `not(<pattern>)` is IC7's exact construct, and the negation matters: a
    // parser that accepted the pattern but mis-associated the `not` would give
    // the right shape and the wrong answers.
    let g = graph();
    let r = run(
        &g,
        "MATCH (p:Person {id: 1}), (o:Person) WHERE o.id <> 1 \
         RETURN o.id AS id, not((p)-[:KNOWS]-(o)) AS isNew ORDER BY id",
    );
    let got: Vec<(i64, bool)> = r
        .rows
        .iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(Value::Int(i)), Some(Value::Bool(b))) => Some((*i, *b)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        vec![(2, false), (3, true)],
        "isNew is the NEGATION: 2 is known so false, 3 is not so true. Got {:?}",
        r.rows
    );
}

#[test]
fn a_pattern_still_filters_in_a_WHERE() {
    // The position that always worked must keep working: this change permits a
    // new position, it does not move the old one.
    let g = graph();
    let r = run(
        &g,
        "MATCH (p:Person {id: 1}), (o:Person) WHERE (p)-[:KNOWS]-(o) RETURN o.id AS id",
    );
    assert_eq!(
        r.rows.len(),
        1,
        "exactly one person is known to 1; got {:?}",
        r.rows
    );
}
