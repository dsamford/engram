#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A cyclic count-only pattern must be ordered by its PEAK intermediate, not by
//! the cheapest next step.
//!
//! # The defect this pins
//!
//! `reorder_pattern`'s greedy takes the path with the smallest immediate
//! fan-out, and prefers a path with BOTH ends bound because "a pure close adds
//! no rows". That second rule is true only of a SINGLE-hop close. On LSQB q2 —
//!
//! ```cypher
//! MATCH (person1:Person)-[:KNOWS]-(person2:Person),
//!       (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)-[:HAS_CREATOR]->(person2)
//! RETURN count(*)
//! ```
//!
//! — seeded at `person1`, neither path is both-bound, so the greedy takes KNOWS
//! (fan-out ~36) and builds 356k rows. The comment path is then both-bound and
//! taken as a "free close", but it is THREE hops: it still expands each row's
//! ~212 comments before closing onto the bound `person2`. Taking the comment
//! path FIRST holds the intermediate at ~2.1M and leaves KNOWS a real one-hop
//! close.
//!
//! Measured on the pod against official LDBC SF1, `ENGRAM_TRACE_COUNTERS=1`:
//!
//! | statement | `graph.adjacency tables reused` |
//! |---|---|
//! | q2 as written | **201,912,362** |
//! | the same chain with the cycle REMOVED | **3,073,484** |
//!
//! 65.7x, for an answer of 943,416 rows. engram took 40,368 ms where Neo4j took
//! 2,773 — the ONLY LSQB query it lost by more than 1.5x.
//!
//! # What this test asserts
//!
//! On a q2-shaped fixture, with the search ON and OFF (`set_order_peak_search`):
//! the COUNT IS IDENTICAL — the rewrite is unobservable, so a difference would
//! be a wrong answer, not a slow one — and the ON arm visits dramatically fewer
//! adjacency rows. The count equality is the assertion that matters; the work
//! ratio is what says the search did anything at all.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const Q2: &str = "MATCH (person1:Person)-[:KNOWS]-(person2:Person), \
                  (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)-[:HAS_CREATOR]->(person2) \
                  RETURN count(*) AS count";

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

/// A q2-shaped world: people who know each other, and comments replying to
/// posts, where the comment's author and the post's author are BOTH people.
///
/// The shape is what matters, not the size: `PERSONS` people each `KNOWS`
/// `FRIENDS` others, and each person authors `COMMENTS` comments, each replying
/// to a post authored by someone. So the KNOWS side is small per person and the
/// comment side is large per person — SF1's ratio, which is what makes one
/// ordering explode and the other not.
const PERSONS: i64 = 60;
const FRIENDS: i64 = 6;
const COMMENTS: i64 = 40;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..PERSONS {
        run(&g, &format!("CREATE (:Person {{id: {i}}})"));
    }
    for i in 0..PERSONS {
        for k in 1..=FRIENDS {
            let j = (i + k) % PERSONS;
            run(
                &g,
                &format!(
                    "MATCH (a:Person {{id: {i}}}), (b:Person {{id: {j}}}) CREATE (a)-[:KNOWS]->(b)"
                ),
            );
        }
    }
    // One post per person, authored by them.
    for i in 0..PERSONS {
        run(
            &g,
            &format!(
                "MATCH (p:Person {{id: {i}}}) CREATE (:Post {{id: {i}}})-[:HAS_CREATOR]->(p)"
            ),
        );
    }
    // Each person authors COMMENTS comments, each replying to a spread of posts.
    for i in 0..PERSONS {
        for c in 0..COMMENTS {
            let post = (i * 7 + c) % PERSONS;
            run(
                &g,
                &format!(
                    "MATCH (p:Person {{id: {i}}}), (t:Post {{id: {post}}}) \
                     CREATE (t)<-[:REPLY_OF]-(:Comment {{id: {}}})-[:HAS_CREATOR]->(p)",
                    i * COMMENTS + c
                ),
            );
        }
    }
    g.shared_store().seal();
    g
}

fn count_and_work(g: &Graph) -> (i64, u64) {
    let q = parse_statement(Q2).expect("parse");
    let (r, t) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
    let r = r.expect("run q2");
    let count = match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("q2 must answer one Int, got {other:?}"),
    };
    let work = t
        .counters()
        .get("graph.adjacency tables reused")
        .copied()
        .unwrap_or(0);
    (count, work)
}

#[test]
fn the_cyclic_count_is_unchanged_and_the_ordering_costs_far_less() {
    let g = fixture();

    engram_graph::pipeline::set_order_peak_search(false);
    let (greedy_count, greedy_work) = count_and_work(&g);
    engram_graph::pipeline::set_order_peak_search(true);
    let (search_count, search_work) = count_and_work(&g);

    eprintln!(
        "[q2-shape] greedy: count {greedy_count}, {greedy_work} adjacency rows\n\
         [q2-shape] search: count {search_count}, {search_work} adjacency rows"
    );

    // THE ASSERTION THAT MATTERS. The reorder is unobservable — both orderings
    // enumerate the same set of matches — so any difference here is a wrong
    // answer and not a slower plan.
    assert_eq!(
        search_count, greedy_count,
        "the peak-aware ordering changed the ANSWER; it may only change the plan"
    );
    assert!(
        greedy_count > 0,
        "the fixture must produce cyclic matches, or both arms trivially agree"
    );

    // AND the search must actually do something. Without this the equality
    // above would pass just as well if the search never fired.
    assert!(
        search_work < greedy_work,
        "the search visited {search_work} adjacency rows against the greedy's \
         {greedy_work} — it did not change the ordering, so this fixture does \
         not pose the question the pod measured at 65.7x"
    );
}
