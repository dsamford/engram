#![allow(non_snake_case)]
//! Fix 88: a change log that overflows keeps its NEWEST HALF, so the index
//! that was current a moment before the overflow catches up instead of
//! rebuilding from every record of its label.
//!
//! The v189 trace of the write-heavy sweep named the stall exactly: every
//! stalled second was one `MATCH (m:Message {id: …})<-[:REPLY_OF]-(c:Comment)`
//! reader carrying `index.label-scoped builds = 1` and 3,055,775 store gets
//! (one per Message at SF1), 4.4–5.7 s, with the other five readers behind
//! its build guard — four times in a 30-second level, at 0 ops/s. The
//! `Message.id` index had caught up a millisecond earlier. What stranded it
//! was the `id` property log: keyed by the PROPERTY, shared by `Person.id`
//! and `Message.id`, and pinned by whichever sibling is not being probed
//! (fix 116 keeps the log for the oldest sibling), it reached its cap every
//! ~9 s at ~1,700 id-writes/s — and `ChangeLog::record`'s overflow CLEARED
//! it and set the floor to the epoch, which put every snapshot below the
//! floor, the current one included.
//!
//! The overflow now drops the oldest half and sets the floor to the last
//! dropped stamp. The active index (probed within half a cap of writes)
//! always finds its entries; the idle sibling — whose stamp is below the
//! dropped half — rebuilds ONCE on its own next probe, which is the cost
//! fix 116's comment attributed to it and which every index paid instead.
//!
//! The corpus is fix 116's (Person.id and Message.id declared) and the
//! write stream is longer than the log's cap, with only the Message index
//! probed on the way — the shape of the sweep.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CAUGHT_UP: &str = "graph.range index caught up";
const BUILDS: &str = "graph.range index builds";
/// The scoped catch-up ran and had no rows for THIS label — the correct
/// outcome when the write stream belongs to another label.
const NOTHING_FOR_LABEL: &str = "graph.range index catch-up had nothing for this label";
const GETS: &str = "store.gets";

const PERSONS: i64 = 600;
const MESSAGES: i64 = 6_000;
/// `PROP_LOG_CAP` is 16,384; the stream passes it with room to overflow
/// TWICE (each overflow keeps half, so the second comes 8,192 writes after
/// the first).
const STREAM: i64 = 26_624;
/// Writes between Message probes: well inside the kept half, so the active
/// index is never more than this far behind the log.
const BURST: i64 = 1_024;

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

/// The sweep in miniature: a stream of id-carrying Message writes longer
/// than the log's cap, the Message index probed every burst, the Person
/// index never. Every Message probe after the first CATCHES UP — through
/// both overflows — and the answer is pinned throughout.
#[test]
fn a_the_active_index_catches_up_through_the_logs_overflow() {
    let g = corpus();
    let want: Vec<Vec<Value>> = (0..19).map(|k| vec![Value::Int(307 + 300 * k)]).collect();
    assert_eq!(rows(&g, MESSAGE_SEEK), want, "the COLD read");
    let _ = rows(&g, PERSON_SEEK); // both indexes built; Person.id now pins the log
    let mut probes = 0;
    let mut caught_up = 0;
    let mut seq = 0;
    while seq < STREAM {
        for _ in 0..BURST {
            stress_write(&g, seq);
            seq += 1;
        }
        let (got, c) = traced(&g, MESSAGE_SEEK);
        probes += 1;
        assert_eq!(got, want, "after {seq} writes");
        assert_eq!(
            count_of(&c, BUILDS),
            0,
            "after {seq} writes the Message.id index REBUILT — the log's overflow stranded \
             the index that was current a burst earlier: {c:?}"
        );
        assert!(
            count_of(&c, GETS) < 200,
            "after {seq} writes the message seek read {} records — a rebuild, not a catch-up",
            count_of(&c, GETS)
        );
        caught_up += count_of(&c, CAUGHT_UP);
    }
    assert!(probes >= 26, "{probes} probes");
    assert_eq!(
        caught_up, probes,
        "every probe after a burst must catch up (the log covers it): {caught_up} of {probes}"
    );
}

/// The idle sibling pays its own rebuild, once: its stamp is below the
/// dropped half, so its next probe builds over ITS label (600 persons), and
/// the probe after that catches up again. This is the cost fix 116 said an
/// unprobed sibling would carry; before fix 88 the ACTIVE sibling carried
/// it too, every overflow.
#[test]
fn b_the_idle_sibling_rebuilds_once_on_its_own_next_probe() {
    let g = corpus();
    let _ = rows(&g, MESSAGE_SEEK);
    let _ = rows(&g, PERSON_SEEK);
    for seq in 0..STREAM {
        stress_write(&g, seq);
        if seq % BURST == BURST - 1 {
            let _ = rows(&g, MESSAGE_SEEK); // the active sibling keeps pace
        }
    }
    let (got, c) = traced(&g, PERSON_SEEK);
    assert_eq!(got, vec![vec![Value::Str("P7".into())]]);
    assert_eq!(
        count_of(&c, BUILDS),
        1,
        "the idle Person.id index must rebuild exactly once, over its own label: {c:?}"
    );
    stress_write(&g, STREAM);
    let (got, c) = traced(&g, PERSON_SEEK);
    assert_eq!(got, vec![vec![Value::Str("P7".into())]]);
    assert_eq!(
        count_of(&c, BUILDS),
        0,
        "and catches up from then on: {c:?}"
    );
    // NOT `CAUGHT_UP >= 1` any more, and the change is the point.
    //
    // `stress_write` creates :Message/:Comment nodes carrying `id`; this probe
    // is :Person.id. Requiring a CATCH-UP here required the :Person index to
    // apply :Message rows — the pollution `scoped_index_catch_up.rs`
    // reproduces (one :Message {id: 99} grew a one-entry :Person.id index to
    // two, and probing id=99 returned the Message). With the label filter
    // restored the catch-up runs, finds nothing of its own, and says so.
    //
    // What this test is actually for — the idle sibling rebuilds ONCE and not
    // again — is unchanged and still asserted by `BUILDS == 0` above.
    let resolved = count_of(&c, CAUGHT_UP) + count_of(&c, NOTHING_FOR_LABEL);
    assert!(
        resolved >= 1,
        "the probe must resolve without rebuilding — caught up, or correctly          found nothing of its own label: {c:?}"
    );
}
