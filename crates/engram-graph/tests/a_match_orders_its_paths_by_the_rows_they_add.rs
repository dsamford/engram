//! The paths of one MATCH are conjunctive, so their order is a cost decision:
//! `order_connected_paths` takes, among the paths that share a variable with
//! those already planned, the one that ADDS THE FEWEST ROWS walked from its
//! bound nodes (`bound_drive_estimate`) — where it used to take the earliest
//! written. SNB BI bi17 wrote `(forum1)<-[:HAS_MEMBER]->(person3)<-[:HAS_CREATOR]
//! -(message2)` before `(comment)-[:REPLY_OF]->(message2)`: every member's
//! every message per row, filtered afterwards to the one message the comment
//! replies to (82 GB held at SF0.1 after seven minutes).
//!
//! The oracle is the same paths as separate MATCH clauses in written order —
//! equivalent here because no two paths share a relationship type, so the
//! one-MATCH relationship-isomorphism rule has nothing to decide.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const REORDERED: &str = "interp.MATCH path ordered by the rows it adds";

/// A forum with 60 members who wrote 40 messages each; a comment in the
/// forum replying to message 7 of member 13; a second forum and comment that
/// share nothing with the first.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let rel = |a: u64, t: &str, b: u64| {
        g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
    };
    let forum = node("F", 1);
    let comment = node("C", 1);
    rel(comment, "IN", forum);
    let mut target = 0;
    for p in 0..60i64 {
        let person = node("P", p);
        rel(forum, "MEMBER", person);
        for m in 0..40i64 {
            let msg = node("M", p * 100 + m);
            rel(person, "WROTE", msg);
            if p == 13 && m == 7 {
                target = msg;
            }
        }
    }
    rel(comment, "REPLY", target);
    let forum2 = node("F", 2);
    let comment2 = node("C", 2);
    rel(comment2, "IN", forum2);
    // The interpreter's MATCH, which is where the paths are ordered (a
    // columnar recogniser that claims a shape plans its own joins).
    g.set_columnar_scans(false);
    g
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

#[test]
fn a_the_path_that_binds_one_row_goes_before_the_one_that_binds_thousands() {
    let g = corpus();
    for id in [1, 2] {
        let src = format!(
            "MATCH (c:C {{id: {id}}})-[:IN]->(f:F), (f)-[:MEMBER]->(p:P)-[:WROTE]->(m:M), (c)-[:REPLY]->(m) \
             RETURN p.id AS p, m.id AS m ORDER BY p, m"
        );
        let oracle = format!(
            "MATCH (c:C {{id: {id}}})-[:IN]->(f:F) MATCH (f)-[:MEMBER]->(p:P)-[:WROTE]->(m:M) \
             MATCH (c)-[:REPLY]->(m) RETURN p.id AS p, m.id AS m ORDER BY p, m"
        );
        let (want, _) = traced(&g, &oracle);
        let (got, c) = traced(&g, &src);
        assert_eq!(got, want, "{src}");
        assert!(count_of(&c, REORDERED) >= 1, "the one-row path did not go first: {c:?}");
        if id == 1 {
            assert_eq!(got, vec![vec![Value::Int(13), Value::Int(1_307)]]);
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// Paths already written cheapest-first keep their order: nothing is
/// reordered and nothing is counted.
#[test]
fn b_paths_already_in_the_cheapest_order_are_left_alone() {
    let g = corpus();
    let src = "MATCH (c:C {id: 1})-[:IN]->(f:F), (c)-[:REPLY]->(m:M), (f)-[:MEMBER]->(p:P)-[:WROTE]->(m) \
               RETURN p.id AS p, m.id AS m";
    let (got, c) = traced(&g, src);
    assert_eq!(got, vec![vec![Value::Int(13), Value::Int(1_307)]]);
    assert_eq!(count_of(&c, REORDERED), 0, "{c:?}");
    assert_eq!(count_of(&c, "interp.MATCH paths ordered to follow their joins"), 0, "{c:?}");
}
