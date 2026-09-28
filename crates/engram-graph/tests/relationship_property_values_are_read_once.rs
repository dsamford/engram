//! A relationship property the pipeline reads is read from the store ONCE, and
//! served from those values until something commits.
//!
//! SNB Interactive IC5 filters every friend's `HAS_MEMBER` edge on
//! `membership.joinDate`: ~4.7M relationship records at SF3, one store read
//! each, on every statement — 13.7 s against Neo4j's 7.2 — for dates that do
//! not change between statements.
//!
//! The answer is the gather's either way; what must hold is that the second
//! statement reads nothing again, that a commit retires what was read (a
//! stale date would be a wrong answer), and that a relationship WITHOUT the
//! property still reads as absent.

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

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const SERVED: &str = "graph.relationship property served from values already read";
const KEPT: &str = "graph.relationship property values kept for the next statement";
const DROPPED: &str = "graph.relationship property values dropped by a commit";

/// Six forums, 300 people; person i is a member of forum (i % 6) with
/// `joinDate = i % 20`, and every tenth membership carries no date at all.
fn memberships() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 5) AS f CREATE (:Forum {id: f})");
    ddl(&g, "UNWIND range(0, 299) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE f.id = p.id % 6 AND p.id % 10 <> 0 \
         CREATE (f)-[:HAS_MEMBER {joinDate: p.id % 20}]->(p)",
    );
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE f.id = p.id % 6 AND p.id % 10 = 0 \
         CREATE (f)-[:HAS_MEMBER]->(p)",
    );
    let _ = g.warm();
    g
}

const Q: &str = "MATCH (f:Forum)-[m:HAS_MEMBER]->(p:Person) WHERE m.joinDate > 12 \
                 RETURN f.id AS forum, count(p) AS c ORDER BY forum";

fn interp(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let (rows, _) = run(g, q);
    g.set_columnar_scans(true);
    rows
}

#[test]
fn the_second_statement_reads_no_relationship_record_again() {
    let g = memberships();
    let want = interp(&g, Q);
    assert!(!want.is_empty(), "vacuous fixture");
    let (first, c1) = run(&g, Q);
    assert_eq!(first, want, "the first run");
    assert!(counter(&c1, KEPT) > 0, "the pipeline did not read through the memo: {c1:?}");
    let (second, c2) = run(&g, Q);
    assert_eq!(second, want, "the second run");
    assert!(counter(&c2, SERVED) > 0, "nothing was served: {c2:?}");
    assert_eq!(counter(&c2, KEPT), 0, "the second run gathered again: {c2:?}");
}

#[test]
fn a_commit_retires_what_was_read() {
    let g = memberships();
    let _ = run(&g, Q);
    // Move every date 13 below the filter: those memberships must drop out.
    ddl(&g, "MATCH ()-[m:HAS_MEMBER]->() WHERE m.joinDate = 13 SET m.joinDate = 0");
    let want = interp(&g, Q);
    let (got, c) = run(&g, Q);
    assert_eq!(got, want, "a stale date was served after the write");
    assert!(counter(&c, DROPPED) > 0, "the commit did not retire the memo: {c:?}");
}

#[test]
fn a_relationship_without_the_property_reads_as_absent() {
    let g = memberships();
    for q in [
        "MATCH (f:Forum)-[m:HAS_MEMBER]->(p:Person) WHERE m.joinDate IS NULL \
         RETURN f.id AS forum, count(p) AS c ORDER BY forum",
        "MATCH (f:Forum)-[m:HAS_MEMBER]->(p:Person) WHERE m.joinDate < 3 OR m.joinDate IS NULL \
         RETURN f.id AS forum, count(p) AS c ORDER BY forum",
    ] {
        let want = interp(&g, q);
        let _ = run(&g, Q); // fill the memo, dates and absences alike
        let (got, _) = run(&g, q);
        assert_eq!(got, want, "{q}");
    }
}

/// The memo's budget is the property-column cache's: with the cache off
/// (budget 0) nothing is kept and the second statement gathers again, with
/// the same answer; with the default it is kept. At SF10 IC5's 14.8M dates
/// passed the memo's own 512 MB constant, whatever the operator had given the
/// column cache, and were gathered again on every statement.
#[test]
fn the_memo_keeps_what_the_column_budget_allows() {
    let g = memberships();
    let want = interp(&g, Q);
    g.set_prop_column_budget(0);
    let (first, c1) = run(&g, Q);
    assert_eq!(first, want);
    assert_eq!(counter(&c1, KEPT), 0, "kept under a zero budget: {c1:?}");
    let (second, c2) = run(&g, Q);
    assert_eq!(second, want);
    assert_eq!(counter(&c2, SERVED), 0, "served what was never kept: {c2:?}");
    g.set_prop_column_budget(64 << 20);
    let (third, c3) = run(&g, Q);
    assert_eq!(third, want);
    assert!(counter(&c3, KEPT) > 0, "{c3:?}");
    let (fourth, c4) = run(&g, Q);
    assert_eq!(fourth, want);
    assert!(counter(&c4, SERVED) > 0, "{c4:?}");
}
