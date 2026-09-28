#![allow(non_snake_case)]
//! The OPTIONAL fold runs on the executor, not on the calling thread alone.
//!
//! `fold_optional_leg` looped over every outer row on the calling thread, and
//! its own comment said so ("This runs on the calling thread, so the flush is
//! direct"). At SF10 that was the whole of LSQB q7: decomposed warm, the label
//! scan costs 0 s, the two hops 3 s, and the two OPTIONAL legs 17 s of a 20 s
//! query. It was the only LSQB query PostgreSQL beat.
//!
//! It is the SAME defect `expand` had one call up, whose comment records the
//! symptom exactly: "the entire benchmark ran on one core of 44 while `query
//! parallelism ON: width 44` sat in the log above it -- q6 took 487 s with 43
//! cores idle." The optional fold was missed by that fix.
//!
//! Measured after, at SF10: width 1 unchanged (32 s -> 31 s), width 44 twenty
//! seconds -> eight. Scaling 1.68x -> 3.9x.
//!
//! WHAT THIS FILE GUARDS is the property a timing cannot: that the VALUES do
//! not move. A fold that spreads over morsels and returns a different count is
//! a worse outcome than a slow one, and the per-morsel `FoldState` (each with
//! its own memo) is exactly the kind of split that could drift.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn count_of(r: &QueryResult) -> i64 {
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one integer row, got {other:?}"),
    }
}

/// q7's shape in miniature: a two-hop base chain and two OPTIONAL legs, with
/// enough rows that a morsel split is more than one morsel, and with a
/// deliberate ZERO-MATCH row — the left join's `max(1, ·)` is what makes the
/// fold admissible at all, and it is the case a careless merge drops.
fn graph(rows: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:Tag {id: 1})");
    run(&g, "CREATE (:Person {id: 1})");
    for i in 0..rows {
        run(&g, &format!("CREATE (:Message {{id: {i}}})"));
        run(
            &g,
            &format!("MATCH (m:Message {{id: {i}}}), (t:Tag {{id: 1}}) CREATE (m)-[:HAS_TAG]->(t)"),
        );
        run(
            &g,
            &format!(
                "MATCH (m:Message {{id: {i}}}), (p:Person {{id: 1}}) \
                 CREATE (m)-[:HAS_CREATOR]->(p)"
            ),
        );
        // Every THIRD message gets a liker and a reply; the rest match neither
        // optional and must still contribute exactly one row each.
        if i % 3 == 0 {
            run(
                &g,
                &format!(
                    "MATCH (m:Message {{id: {i}}}), (p:Person {{id: 1}}) \
                     CREATE (m)<-[:LIKES]-(p)"
                ),
            );
            run(&g, &format!("CREATE (:Comment {{id: {i}}})"));
            run(
                &g,
                &format!(
                    "MATCH (m:Message {{id: {i}}}), (c:Comment {{id: {i}}}) \
                     CREATE (m)<-[:REPLY_OF]-(c)"
                ),
            );
        }
    }
    g
}

const Q7: &str = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                  OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
                  OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
                  RETURN count(*) AS count";

#[test]
fn the_fixture_has_matched_and_unmatched_rows() {
    // Guard the fixture: if no message had an optional match, or if all did,
    // the count below would be right for the wrong reason and the `max(1, ·)`
    // left-join rule would never be exercised.
    let g = graph(30);
    assert_eq!(
        count_of(&run(&g, "MATCH (m:Message) RETURN count(m) AS c")),
        30
    );
    assert_eq!(
        count_of(&run(
            &g,
            "MATCH (:Message)<-[:LIKES]-(:Person) RETURN count(*) AS c"
        )),
        10,
        "ten of thirty messages are liked, so twenty null-fill"
    );
}

#[test]
fn the_count_is_the_left_join_product_whatever_the_split() {
    // 30 messages, each with one tag and one creator. 10 have a liker and a
    // reply (weight 1 x 1), 20 have neither (weight max(1,0) x max(1,0) = 1).
    // So the count is 30 -- every row weighs exactly 1 here, which is the
    // strictest possible check on the merge: any dropped or duplicated morsel
    // row changes it.
    let g = graph(30);
    assert_eq!(
        count_of(&run(&g, Q7)),
        30,
        "one row per message: the optionals multiply by 1 whether they match"
    );
}

#[test]
fn a_multi_match_optional_multiplies_and_survives_the_merge() {
    // Now make the weights differ per row, so a merge that reordered or
    // mis-assigned morsel results would show up as a wrong TOTAL rather than
    // needing row-by-row inspection.
    let g = graph(9);
    // Message 0 gets two more likers: its leg weight becomes 3.
    for p in 2..=3 {
        run(&g, &format!("CREATE (:Person {{id: {p}}})"));
        run(
            &g,
            &format!(
                "MATCH (m:Message {{id: 0}}), (p:Person {{id: {p}}}) \
                 CREATE (m)<-[:LIKES]-(p)"
            ),
        );
    }
    // 9 messages: ids 0,3,6 have a liker and a reply. id 0 now has 3 likers.
    // weights: id0 = 3*1 = 3, id3 = 1, id6 = 1, the other six = 1 each.
    // total = 3 + 1 + 1 + 6 = 11.
    assert_eq!(
        count_of(&run(&g, Q7)),
        11,
        "message 0 weighs 3 (three likers), every other message weighs 1"
    );
}

#[test]
fn a_larger_row_count_gives_the_same_answer_as_a_small_one() {
    // The morsel split is a function of the row count, so the same logical
    // graph at two sizes exercises different split arithmetic. The counts must
    // scale exactly: N messages, each weighing 1.
    for n in [7usize, 64, 257] {
        let g = graph(n);
        assert_eq!(
            count_of(&run(&g, Q7)),
            n as i64,
            "with {n} messages the left-join count must be {n}"
        );
    }
}
