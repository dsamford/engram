#![allow(non_snake_case)]
//! Fix 88b: a property change log is widened to a fraction of the largest
//! index it serves, so an index left IDLE under a write stream catches up
//! instead of rebuilding.
//!
//! Fix 88 made the overflow keep the newest half, which bounded WHO pays at
//! the cap — the index that kept pace no longer rebuilds because a sibling
//! pinned the log. It did not bound how much the idle one pays: on the v190
//! sweep the `Message.id` index sat unprobed through the two write-only
//! levels (~118k id-writes in 40 s, seven halvings of a 16,384-entry log),
//! fell below the floor, and the first `is7-replies` read of the level that
//! followed rebuilt it from 3,055,775 records — 3.5 s, one stalled second,
//! `contention @ 1` at floor 0.03 where v188 had it at 0.48. One rebuild is
//! the semantics fix 116 intended; at SF1 one rebuild is still a stall.
//!
//! The pricing: a log entry is ~100 bytes and a catch-up is one
//! `with_changes` (O(delta log delta) plus one O(base) fold); a rebuild is a
//! store get per row of the label. A log of one entry per eight index rows
//! costs a tenth of the index's memory and turns any idle stretch shorter
//! than that many writes into a catch-up. `PROP_LOG_CAP` (16,384) stays the
//! floor for small indexes; `PROP_LOG_CAP_MAX` (524,288) the ceiling.
//!
//! The corpus below is the SF1 shape at a hundredth: a 160,000-row
//! `Message.id` index earns a 20,000-entry log, and an 18,000-write idle
//! stretch — past the default cap, inside the earned one — must catch up.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, prop_log_cap_for, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CAUGHT_UP: &str = "graph.range index caught up";
const BUILDS: &str = "graph.range index builds";
const GETS: &str = "store.gets";

const PERSONS: i64 = 600;
/// Earns a 20,000-entry log (160,000 / 8), above the 16,384 default.
const MESSAGES: i64 = 160_000;
/// Past the default cap (16,384 — a log that stayed there would have halved
/// and dropped the Message index's window), inside the earned one.
const IDLE_WRITES: i64 = 18_000;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn message(g: &Graph, id: i64) {
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(id));
    m.insert("content".to_string(), Value::Str("x".into()));
    g.create_node(&["Message".into(), "Comment".into()], &m)
        .expect("message");
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX snb_person_id FOR (n:Person) ON (n.id)");
    ddl(&g, "CREATE INDEX snb_message_id FOR (n:Message) ON (n.id)");
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("firstName".to_string(), Value::Str(format!("P{i}")));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    for i in 0..MESSAGES {
        message(&g, i);
    }
    g
}

const PERSON_SEEK: &str = "MATCH (p:Person {id: 7}) RETURN p.firstName AS n";
const MESSAGE_SEEK: &str = "MATCH (m:Message {id: 7}) RETURN m.content AS c";

/// The sweep's idle stretch in miniature: both indexes built, then a write
/// stream past the DEFAULT cap with only the Person index probed. The
/// Message index's next probe catches up — no build, no label walk.
#[test]
fn a_an_idle_index_under_a_large_label_catches_up_after_a_stretch_past_the_default_cap() {
    let g = corpus();
    assert_eq!(rows(&g, MESSAGE_SEEK), vec![vec![Value::Str("x".into())]]);
    assert_eq!(rows(&g, PERSON_SEEK), vec![vec![Value::Str("P7".into())]]);
    for seq in 0..IDLE_WRITES {
        message(&g, 1_000_000 + seq);
        if seq % 500 == 499 {
            let (_, c) = traced(&g, PERSON_SEEK);
            assert_eq!(
                count_of(&c, BUILDS),
                0,
                "the probed sibling keeps pace: {c:?}"
            );
        }
    }
    let (got, c) = traced(&g, MESSAGE_SEEK);
    assert_eq!(got, vec![vec![Value::Str("x".into())]]);
    assert_eq!(
        count_of(&c, BUILDS),
        0,
        "after {IDLE_WRITES} idle writes the Message.id index REBUILT — the log was not \
         widened to the index it protects: {c:?}"
    );
    assert!(count_of(&c, CAUGHT_UP) >= 1, "{c:?}");
    assert!(
        count_of(&c, GETS) < 100,
        "the idle index read {} records — a label walk, not a catch-up",
        count_of(&c, GETS)
    );
    // The mechanism, after the behaviour: the 160,000-row index widened the
    // `id` log to 160,000 / 8. (Asserted last so that a tree without the
    // widening fails on the REBUILD above, which is the claim.)
    assert_eq!(g.prop_log_cap_for_test("id"), Some(20_000));
}

/// The arithmetic, pinned: the floor for small indexes, one entry per eight
/// rows in between, the ceiling for the largest.
#[test]
fn b_the_log_cap_follows_the_index_between_a_floor_and_a_ceiling() {
    assert_eq!(prop_log_cap_for(0), 16_384);
    assert_eq!(
        prop_log_cap_for(6_000),
        16_384,
        "a small index keeps the default"
    );
    assert_eq!(prop_log_cap_for(131_072), 16_384, "exactly the floor");
    assert_eq!(prop_log_cap_for(160_000), 20_000);
    assert_eq!(prop_log_cap_for(3_055_774), 381_971, "SF1's Message.id");
    assert_eq!(
        prop_log_cap_for(9_400_000),
        524_288,
        "SF3's `id` hits the ceiling"
    );
}

/// CONTROL: a small index does not widen its log — the default cap is the
/// bound, exactly as before fix 88b.
#[test]
fn c_a_small_index_keeps_the_default_cap() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX only_person FOR (n:Person) ON (n.pid)");
    for i in 0..800 {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Int(i));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    let _ = rows(&g, "MATCH (p:Person {pid: 5}) RETURN p.pid AS n");
    assert_eq!(g.prop_log_cap_for_test("pid"), Some(16_384));
}
