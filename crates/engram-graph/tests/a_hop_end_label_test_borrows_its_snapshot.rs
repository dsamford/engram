//! A hop end's one-label membership test BORROWS the published snapshot.
//!
//! `mat_end` asked `Graph::members` for an OWNED view per end — twice — and
//! an owned view clones the slot's `Arc`, the snapshot's and six inside the
//! view, then drops them all: ~16 atomic writes per end, every one on a cache
//! line every worker shares. SNB BI bi15's weighting join made 57.7M of them
//! for 1/20 of SF3's pairs, and with its continuation split across 40
//! workers the join spent ~4x the serial run's CPU — user time, not a lock.
//! `Graph::label_contains` / `members_ref` take the snapshot through its
//! arc-swap guard instead and touch no refcount.
//!
//! What must NOT change is which snapshot answers. A borrowed one is served
//! only where the owned path would have served it as current, so a write
//! that moves the label is seen by the next statement. Every answer here is
//! checked against the same statement with the columnar paths off (a record
//! read per end, no membership test at all).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

fn traced(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, t) = engram_observe::with_trace(|| rows(g, q));
    (r, t.counters().clone())
}

/// The same statement with no membership test anywhere: every end read.
fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, q);
    g.set_columnar_scans(true);
    r
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const BORROWED: &str = "graph.membership tested through a borrowed snapshot";
const CURRENT: &str = "graph.membership snapshots current";
const STILL_CURRENT: &str = "graph.membership snapshots still current";
const BARE: &str = "interp.matcher bound a hop end bare";

/// 100 people who each KNOW the next three; 10 messages each, every other one
/// a reply to a message by the next person along; and 2 drafts each, which
/// hang off `HAS_CREATOR` like a message but are not one — so the label test
/// has ends to reject as well as to admit. One row per edge in every setup
/// statement.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 99) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 99) AS i UNWIND range(1, 3) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 100}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS m MATCH (p:Person {id: m % 100}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m})",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS i WITH i WHERE i % 2 = 0 \
         MATCH (a:Message {id: i}), (b:Message {id: (i + 101) % 1000}) \
         CREATE (a)-[:REPLY_OF]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 199) AS k MATCH (p:Person {id: k % 100}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Draft {id: k})",
    );
    let _ = g.warm();
    g
}

const JOIN: &str = "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
     OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
     WITH pA, pB, count(m1) AS i RETURN count(*) AS pairs, sum(i) AS interactions";

/// bi15's weighting join: every end's label test is borrowed, and the
/// answer is the record-read control's.
#[test]
fn every_hop_end_label_test_is_borrowed() {
    let g = social();
    let want = control(&g, JOIN);
    assert!(
        matches!(want.first().and_then(|r| r.get(1)), Some(Value::Int(n)) if *n > 0),
        "no pair interacts; this compares nothing: {want:?}"
    );
    let _ = rows(&g, JOIN); // the first run builds the memberships
    let (got, c) = traced(&g, JOIN);
    assert_eq!(got, want, "the borrowed label test changed the answer");
    let borrowed = get(&c, BORROWED);
    assert!(
        get(&c, BARE) > 1_000 && borrowed >= get(&c, BARE),
        "every bare-bound end should have been tested through a borrowed snapshot: {c:?}"
    );
    // Hits the OWNED path still served — per clause, not per end.
    let owned = (get(&c, CURRENT) + get(&c, STILL_CURRENT)).saturating_sub(borrowed);
    assert!(
        owned * 20 < borrowed,
        "{owned} owned membership views against {borrowed} borrowed: an owned view per end is back: {c:?}"
    );
}

/// Person 0's messages, by id — a demanded end, so every one is BOUND through
/// the label test (a count could be answered from degrees and test nothing).
const OF_PERSON_0: &str =
    "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m:Message) RETURN m.id AS id ORDER BY id";

fn ids(r: &[Vec<Value>]) -> Vec<i64> {
    r.iter()
        .map(|row| match row.first() {
            Some(Value::Int(n)) => *n,
            other => panic!("not an id: {other:?}"),
        })
        .collect()
}

/// A borrowed snapshot is served only while the owned path would call it
/// current: a label removed, then restored, is seen by the very next
/// statement, and a write to ANOTHER label leaves it served as still current.
#[test]
fn a_write_that_moves_the_label_is_seen_by_the_next_statement() {
    let g = social();
    let all: Vec<i64> = (0..10).map(|k| k * 100).collect();
    assert_eq!(ids(&rows(&g, OF_PERSON_0)), all, "person 0 wrote messages 0, 100, …, 900");
    assert_eq!(ids(&control(&g, OF_PERSON_0)), all);
    let (_, c) = traced(&g, OF_PERSON_0);
    assert!(get(&c, BORROWED) > 0, "the label test never borrowed: {c:?}");

    // Person 0's message 300 stops being a Message…
    ddl(&g, "MATCH (m:Message {id: 300}) REMOVE m:Message");
    let (after_remove, c) = traced(&g, OF_PERSON_0);
    let without: Vec<i64> = all.iter().copied().filter(|&i| i != 300).collect();
    assert_eq!(ids(&after_remove), without, "a removed label was still admitted: {c:?}");
    assert_eq!(after_remove, control(&g, OF_PERSON_0));

    // …and is one again.
    ddl(&g, "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m {id: 300}) SET m:Message");
    let (restored, c) = traced(&g, OF_PERSON_0);
    assert_eq!(ids(&restored), all, "a restored label was still rejected: {c:?}");
    assert_eq!(restored, control(&g, OF_PERSON_0));

    // A write to ANOTHER label advances the clock but not this label: its
    // snapshot is still current by the label's own epoch, and borrowed as
    // such — no snapshot is at the clock now, so no hit may say it is.
    let _ = rows(&g, OF_PERSON_0);
    ddl(&g, "CREATE (:Unrelated {id: 1})");
    let (unmoved, c) = traced(&g, OF_PERSON_0);
    assert_eq!(ids(&unmoved), all);
    assert!(
        get(&c, BORROWED) > 0 && get(&c, CURRENT) == 0 && get(&c, STILL_CURRENT) >= get(&c, BORROWED),
        "an untouched label should be borrowed as still current: {c:?}"
    );
}
