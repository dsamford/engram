#![allow(non_snake_case)]
//! Fix 116: a property declared on TWO labels has two label-scoped range
//! indexes reading ONE change log (the log is keyed by the property). After a
//! write, the index probed first caught up and pruned the log behind its own
//! stamp, and the other — its snapshot now below the log's floor — REBUILT
//! from every record of its label instead of catching up.
//!
//! On the SF1 stress sweep that was `Person.id` and `Message.id`: every
//! `MATCH (m:Message {id: …})<-[:REPLY_OF]-(c:Comment)` after a write read
//! all 3,055,787 Message records (7.3 s), read-heavy fell 259 → 8 ops/s, and
//! no write-free read could see it. The log now keeps what the oldest
//! sibling still needs; both indexes catch up incrementally.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CAUGHT_UP: &str = "graph.range index caught up";
/// The scoped catch-up ran and had no rows for THIS label — correct when the
/// write stream belongs to another label.
const NOTHING_FOR_LABEL: &str = "graph.range index catch-up had nothing for this label";
const BUILDS: &str = "graph.range index builds";
const KEPT: &str = "graph.property log kept for an older sibling index";
const GETS: &str = "store.gets";

const PERSONS: i64 = 600;
const MESSAGES: i64 = 6_000;

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

/// `Person.id` and `Message.id` both declared — the SNB catalogue's shape —
/// over 600 persons and 6,000 messages replying to the first 300.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX snb_person_id FOR (n:Person) ON (n.id)");
    ddl(&g, "CREATE INDEX snb_message_id FOR (n:Message) ON (n.id)");
    let mut persons = Vec::with_capacity(PERSONS as usize);
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("firstName".to_string(), Value::Str(format!("P{i}")));
        persons.push(g.create_node(&["Person".into()], &m).expect("person"));
    }
    let mut messages = Vec::with_capacity(MESSAGES as usize);
    for i in 0..MESSAGES {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("content".to_string(), Value::Str("x".repeat(40)));
        let id = g
            .create_node(&["Message".into(), "Comment".into()], &m)
            .expect("message");
        g.create_rel(
            id,
            "HAS_CREATOR",
            persons[(i % PERSONS) as usize],
            &BTreeMap::new(),
        )
        .expect("creator");
        if i >= 300 {
            g.create_rel(
                id,
                "REPLY_OF",
                messages[(i % 300) as usize],
                &BTreeMap::new(),
            )
            .expect("reply");
        }
        messages.push(id);
    }
    g
}

const PERSON_SEEK: &str = "MATCH (p:Person {id: 7}) RETURN p.firstName AS n";
const MESSAGE_SEEK: &str =
    "MATCH (m:Message {id: 7})<-[:REPLY_OF]-(c:Comment) RETURN c.id AS id ORDER BY id LIMIT 25";

fn stress_write(g: &Graph, seq: i64) {
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(1_000_000 + seq));
    m.insert("content".to_string(), Value::Str("stress".into()));
    let id = g
        .create_node(&["Message".into(), "Comment".into()], &m)
        .expect("stress message");
    g.create_rel(id, "HAS_CREATOR", 1 + (seq as u64 % 50), &BTreeMap::new())
        .ok();
}

/// The read-heavy mix, in miniature: a write, a Person seek, a Message seek
/// — repeated. Every Message seek after the first must CATCH UP, never
/// rebuild, and the answer is pinned throughout.
#[test]
fn a_the_second_index_catches_up_after_its_sibling_pruned_the_log() {
    let g = corpus();
    // Messages 307, 607, …, 5707 reply to message 7: nineteen of them, by
    // arithmetic — pinned against the corpus, not against the first read.
    let want: Vec<Vec<Value>> = (0..19).map(|k| vec![Value::Int(307 + 300 * k)]).collect();
    let cold = rows(&g, MESSAGE_SEEK);
    assert_eq!(cold, want, "the COLD read (indexes just built)");
    let _ = rows(&g, PERSON_SEEK); // both indexes built once
    let mut builds_after_first = 0;
    let mut caught_up = 0;
    for round in 0..6 {
        stress_write(&g, round);
        let (_, c) = traced(&g, PERSON_SEEK);
        assert_eq!(
            count_of(&c, BUILDS),
            0,
            "round {round} person seek rebuilt: {c:?}"
        );
        let (got, c) = traced(&g, MESSAGE_SEEK);
        assert_eq!(got, want, "round {round}");
        builds_after_first += count_of(&c, BUILDS);
        caught_up += count_of(&c, CAUGHT_UP);
        assert!(
            count_of(&c, GETS) < 200,
            "round {round}: the message seek read {} records — a rebuild, not a catch-up: {c:?}",
            count_of(&c, GETS)
        );
    }
    assert_eq!(
        builds_after_first, 0,
        "the Message.id index rebuilt in the mix"
    );
    assert!(
        caught_up >= 6,
        "the Message.id index caught up {caught_up} of 6 rounds"
    );
}

/// The mechanism, pinned directly: after a write and a Person seek, the log
/// still reaches the Message index's older snapshot (the prune stopped at
/// the sibling), and the Message seek catches up in one step.
#[test]
fn b_the_log_is_kept_for_the_older_sibling() {
    let g = corpus();
    let _ = rows(&g, PERSON_SEEK);
    let _ = rows(&g, MESSAGE_SEEK);
    stress_write(&g, 100);
    let (_, c) = traced(&g, PERSON_SEEK);
    // The write was a :Message; this seek is :Person. Requiring a CATCH-UP here
    // required the :Person index to apply :Message rows — the pollution
    // reproduced in `scoped_index_catch_up.rs`. With the label filter restored
    // the catch-up runs and correctly has nothing of its own.
    //
    // This test's actual subject — the log is KEPT for the older sibling — is
    // the assertion below, and it is untouched.
    assert!(
        count_of(&c, CAUGHT_UP) + count_of(&c, NOTHING_FOR_LABEL) >= 1,
        "the Person seek must resolve without a rebuild: {c:?}"
    );
    assert!(
        count_of(&c, KEPT) >= 1,
        "the prune must stop at the Message index's snapshot: {c:?}"
    );
    let (_, c) = traced(&g, MESSAGE_SEEK);
    assert!(count_of(&c, CAUGHT_UP) >= 1, "{c:?}");
    assert_eq!(count_of(&c, BUILDS), 0, "{c:?}");
}

/// CONTROL: a property declared on ONE label prunes exactly as before —
/// nothing is kept for a sibling that does not exist.
#[test]
fn c_a_single_index_prunes_as_before() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX only_person FOR (n:Person) ON (n.pid)");
    for i in 0..800 {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Int(i));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    let seek = "MATCH (p:Person {pid: 5}) RETURN p.pid AS n";
    let _ = rows(&g, seek);
    let mut m = BTreeMap::new();
    m.insert("pid".to_string(), Value::Int(9_999));
    g.create_node(&["Person".into()], &m).expect("person");
    let (got, c) = traced(&g, seek);
    assert_eq!(got, vec![vec![Value::Int(5)]]);
    assert!(count_of(&c, CAUGHT_UP) >= 1, "{c:?}");
    assert_eq!(count_of(&c, KEPT), 0, "{c:?}");
}
