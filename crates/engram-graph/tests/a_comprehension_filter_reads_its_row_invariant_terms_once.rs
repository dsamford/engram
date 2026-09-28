//! A pattern comprehension evaluates the terms of its filter that read nothing
//! the pattern binds ONCE, not once per match — with the same answer.
//!
//! SNB Interactive IC14 weights each relationship `r` of a shortest path with
//!
//! ```cypher
//! [(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)
//!  WHERE (a.id = startNode(r).id AND b.id = endNode(r).id)
//!     OR (a.id = endNode(r).id AND b.id = startNode(r).id) | 1.0]
//! ```
//!
//! `r` is the OUTER relationship, so `startNode(r).id` is one number for the
//! whole comprehension. Evaluated per match it materialised the node in full
//! every time: 2.41M store reads for about 560k matches at SF3, most of the
//! query's 29 s serial.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn counter(c: &BTreeMap<String, u64>, name: &str) -> u64 {
    c.get(name).copied().unwrap_or(0)
}

const LIFTED: &str = "interp.comprehension filter evaluated its row-invariant terms once";
const FULL: &str = "graph.nodes materialised in full";

/// Person 1 wrote 40 comments replying to posts by person 2 and 70 replying to
/// posts by person 3; person 2 wrote 30 replying to posts by person 1. Person 1
/// KNOWS 2 and 3.
fn exchanges() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(1, 4) AS i CREATE (:Person {id: i})");
    for (writer, target, n) in [(1, 2, 40), (1, 3, 70), (2, 1, 30)] {
        ddl(
            &g,
            &format!(
                "MATCH (w:Person {{id: {writer}}}), (t:Person {{id: {target}}}) \
                 UNWIND range(1, {n}) AS i \
                 CREATE (w)<-[:HAS_CREATOR]-(:Comment {{n: i}})-[:REPLY_OF]->(:Post {{n: i}})-[:HAS_CREATOR]->(t)"
            ),
        );
    }
    ddl(
        &g,
        "MATCH (a:Person {id: 1}), (b:Person {id: 2}), (c:Person {id: 3}) \
         CREATE (a)-[:KNOWS]->(b), (a)-[:KNOWS]->(c)",
    );
    let _ = g.warm();
    g
}

const IC14_WEIGHT: &str = "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
     RETURN p2.id AS other, \
            size([(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person) \
                  WHERE (a.id = startNode(r).id AND b.id = endNode(r).id) \
                     OR (a.id = endNode(r).id AND b.id = startNode(r).id) | 1]) AS w \
     ORDER BY other";

/// IC14's weight with every endpoint term wrapped in a CASE: still PINNED (the
/// pin evaluates any outer expression), never LIFTED (a CASE is not a kind the
/// lift takes), so it is the per-match form of the same filter.
const IC14_WEIGHT_PER_MATCH: &str = "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
     RETURN p2.id AS other, \
            size([(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person) \
                  WHERE (a.id = CASE WHEN true THEN startNode(r).id END \
                         AND b.id = CASE WHEN true THEN endNode(r).id END) \
                     OR (a.id = CASE WHEN true THEN endNode(r).id END \
                         AND b.id = CASE WHEN true THEN startNode(r).id END) | 1]) AS w \
     ORDER BY other";

#[test]
fn ic14s_weight_is_answered_with_its_endpoints_read_once() {
    let g = exchanges();
    let (got, c) = run(&g, IC14_WEIGHT);
    // (1,2): 40 of 1's comments reply to 2's posts, 30 of 2's to 1's.
    // (1,3): 70 of 1's comments reply to 3's posts, none of 3's to 1's.
    assert_eq!(
        got,
        vec![
            vec![Value::Int(2), Value::Int(40 + 30)],
            vec![Value::Int(3), Value::Int(70)],
        ]
    );
    assert!(counter(&c, LIFTED) >= 2, "one lift per comprehension: {c:?}");

    let (per_match, cp) = run(&g, IC14_WEIGHT_PER_MATCH);
    assert_eq!(per_match, got, "the per-match form is the same question");
    assert_eq!(counter(&cp, LIFTED), 0, "a CASE is never lifted: {cp:?}");
    // The two comprehensions walk 1's 110 comments twice and 2's 30 once: 250
    // matches, each of which re-read at least one endpoint in full.
    assert!(
        counter(&cp, FULL) >= counter(&c, FULL) + 250,
        "lifting saved {} full node reads over 250 matches; the per-match form read {}, \
         the lifted one {}",
        counter(&cp, FULL).saturating_sub(counter(&c, FULL)),
        counter(&cp, FULL),
        counter(&c, FULL)
    );
}

#[test]
fn a_filter_that_reads_only_the_pattern_lifts_nothing() {
    let g = exchanges();
    let (got, c) = run(
        &g,
        "MATCH (p:Person {id: 1}) \
         RETURN size([(p)<-[:HAS_CREATOR]-(c:Comment)-[:REPLY_OF]->(q:Post) \
                      WHERE c.n = q.n AND c.n % 2 = 0 | c]) AS k",
    );
    assert_eq!(got, vec![vec![Value::Int(20 + 35)]]);
    assert_eq!(counter(&c, LIFTED), 0, "nothing in it is invariant: {c:?}");
}

#[test]
fn rand_is_evaluated_per_match_and_never_lifted() {
    let g = exchanges();
    let (got, c) = run(
        &g,
        "MATCH (p:Person {id: 1}) \
         RETURN size([(p)<-[:HAS_CREATOR]-(c:Comment) WHERE rand() < 2.0 | c]) AS k",
    );
    assert_eq!(got, vec![vec![Value::Int(110)]]);
    assert_eq!(counter(&c, LIFTED), 0, "rand() moves per match: {c:?}");
}

#[test]
fn a_lifted_term_answers_as_the_per_match_one_does_on_every_shape() {
    // THE CONTROL. Each shape below lifts something and is also written so the
    // lift is impossible (the invariant is re-derived from a pattern variable
    // bound to the same node), and the two must agree row for row.
    let g = exchanges();
    for (lifted, per_match) in [
        (
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person) \
             WHERE a.id = startNode(r).id AND b.id = endNode(r).id | 1]) ORDER BY p2.id",
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person) \
             WHERE a = p1 AND b = p2 | 1]) ORDER BY p2.id",
        ),
        (
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(c:Comment) \
             WHERE a.id IN [startNode(r).id, endNode(r).id] AND c.n > size(keys(r)) + 35 | c]) ORDER BY p2.id",
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(c:Comment) \
             WHERE (a = p1 OR a = p2) AND c.n > 35 | c]) ORDER BY p2.id",
        ),
        (
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(c:Comment) \
             WHERE NOT (endNode(r).id IS NULL) AND a.id = startNode(r).id AND c.n <= p2.id * 10 | c]) ORDER BY p2.id",
            "MATCH (p1:Person {id: 1})-[r:KNOWS]->(p2:Person) \
             RETURN p2.id, size([(a:Person)<-[:HAS_CREATOR]-(c:Comment) \
             WHERE a = p1 AND c.n <= p2.id * 10 | c]) ORDER BY p2.id",
        ),
    ] {
        let (a, c) = run(&g, lifted);
        let (b, _) = run(&g, per_match);
        assert_eq!(a, b, "{lifted}\n  against\n{per_match}");
        assert!(counter(&c, LIFTED) > 0, "the lifted form must lift: {lifted}: {c:?}");
    }
}
