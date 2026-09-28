//! A pattern comprehension evaluated again on the same bindings answers from
//! the first evaluation instead of walking again.
//!
//! A comprehension is evaluated once per ROW, and a scoring term is usually
//! correlated to one node. A row set that mentions the same node many times
//! therefore walks the same pattern many times for an answer it already had.
//!
//! Measured on SNB BI8 at SF3, which scores every person and then every
//! FRIEND of every person: 1,221,142 expression evaluations against 27,406
//! store reads — 141 s of CPU with no I/O to speak of — where the same scores
//! computed once for all 3,442 people take under a second. Postgres expresses
//! that as a join and an aggregate, evaluated once.
//!
//! What the memo may NOT do is change an answer, and that is most of what is
//! tested here: the same query with the memo unable to help must return what
//! the memo returns, and a write inside the statement must not be answered
//! from a snapshot taken before it.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The caches are keyed on a generation every statement advances, so a test
/// asserting on their counters must not run beside another statement in this
/// binary: the interference does not change an answer, it just empties the
/// cache mid-run and makes the counts unrepeatable. Rust runs tests in one
/// binary on parallel threads, so EVERY test here takes this lock — including
/// the ones that only check answers, because they run statements too and it is
/// their statements that would disturb the others.
static COUNTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

const REUSED: &str = "interp.comprehension answered from an identical earlier row";
const GROUPED: &str = "interp.comprehension answered from its grouped form";
const GROUPING: &str = "interp.comprehension evaluated once for every correlated value";
const NOT_GROUPED: &str = "interp.comprehension declined to group: its result reads the key";

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// BI8's shape: people who all know the same few "hubs", so a score computed
/// per friend is asked for the same friend many times over.
fn corpus(people: i64, hubs: i64, msgs: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |labels: &[&str], k: i64| {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(k));
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        g.create_node(&labels, &m).expect("node")
    };
    let tag = node(&["Tag"], 0);
    let hub_ids: Vec<u64> = (0..hubs).map(|i| node(&["Person"], 900 + i)).collect();
    // each hub has `msgs` tagged messages — the score the comprehension counts
    for (i, h) in hub_ids.iter().enumerate() {
        for j in 0..msgs {
            let m = node(&["Message"], 10_000 + i as i64 * msgs + j);
            g.create_rel(m, "HAS_CREATOR", *h, &BTreeMap::new())
                .expect("creator");
            g.create_rel(m, "HAS_TAG", tag, &BTreeMap::new())
                .expect("tag");
        }
    }
    for i in 0..people {
        let p = node(&["Person"], i);
        for h in &hub_ids {
            g.create_rel(p, "KNOWS", *h, &BTreeMap::new())
                .expect("knows");
        }
    }
    let _ = g.warm();
    g
}

/// Every person's friends scored by the tag — the friend's score depends on
/// the friend alone, so the same few hubs are scored over and over.
const SCORES: &str = "MATCH (tag:Tag {k: 0}) MATCH (p:Person)-[:KNOWS]->(f:Person) \
     WITH tag, p, f \
     RETURN sum(size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(f) | m])) AS score";

#[test]
fn the_same_bindings_are_not_walked_twice() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let g = corpus(40, 3, 5);
    let (rows, t) = engram_observe::with_trace(|| run(&g, SCORES));
    // 40 people x 3 hubs x 5 messages each
    assert_eq!(rows, vec![vec![Value::Int(600)]], "{rows:?}");
    // Either route is a pass here: the memo answers a repeat, and once the
    // site has shown which binding moves, the grouped form answers all of
    // them. What must not happen is 120 walks.
    assert!(
        counter(&t, REUSED) + counter(&t, GROUPED) >= 100,
        "the same three friends were re-walked for every person: {:?}",
        t.counters()
    );
}

#[test]
fn the_comprehension_is_evaluated_once_for_every_correlated_value() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    // DECORRELATION. The score depends on the friend, so it is computed once
    // for every friend and each row is a lookup — which is how Postgres
    // answers BI8's shape, as a join and an aggregate rather than a
    // subquery per row.
    let g = corpus(40, 3, 5);
    let (rows, t) = engram_observe::with_trace(|| run(&g, SCORES));
    assert_eq!(rows, vec![vec![Value::Int(600)]], "{rows:?}");
    assert_eq!(
        counter(&t, GROUPING),
        1,
        "the pattern was grouped more than once: {:?}",
        t.counters()
    );
    assert!(
        counter(&t, GROUPED) >= 100,
        "the grouped form was built but not used: {:?}",
        t.counters()
    );
}

#[test]
fn a_result_that_reads_the_correlated_node_is_not_grouped() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    // THE GUARD. Grouped, the correlated variable is bound to the node the
    // pattern reached rather than to the row's own value, and the two can
    // carry different properties. A map that reads it must therefore keep
    // walking per row — and must still be right.
    let g = corpus(12, 2, 3);
    let q = "MATCH (tag:Tag {k: 0}) MATCH (p:Person)-[:KNOWS]->(f:Person)          WITH tag, p, f          RETURN sum(size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(f) | f.k])) AS n";
    let (rows, t) = engram_observe::with_trace(|| run(&g, q));
    // 12 people x 2 hubs x 3 messages, each contributing one element
    assert_eq!(rows, vec![vec![Value::Int(72)]], "{rows:?}");
    assert_eq!(
        counter(&t, GROUPING),
        0,
        "it grouped a comprehension whose result reads the key: {:?}",
        t.counters()
    );
    assert!(counter(&t, NOT_GROUPED) >= 1, "{:?}", t.counters());
}

#[test]
fn the_answer_is_what_it_would_be_without_the_memo() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    // The memo can only help when a binding REPEATS, so a corpus where every
    // person knows a different friend exercises the same query with nothing
    // to reuse. Both must agree on the arithmetic.
    let shared = corpus(40, 3, 5);
    let total_shared = run(&shared, SCORES);

    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("k".to_string(), Value::Int(0));
    let tag = g.create_node(&["Tag".into()], &m).expect("tag");
    let mut expected = 0i64;
    for i in 0..40i64 {
        let mut pm = BTreeMap::new();
        pm.insert("k".to_string(), Value::Int(i));
        let p = g.create_node(&["Person".into()], &pm).expect("p");
        let f = g.create_node(&["Person".into()], &pm).expect("f");
        g.create_rel(p, "KNOWS", f, &BTreeMap::new())
            .expect("knows");
        for j in 0..(i % 4) {
            let msg = g.create_node(&["Message".into()], &pm).expect("m");
            g.create_rel(msg, "HAS_CREATOR", f, &BTreeMap::new())
                .expect("creator");
            g.create_rel(msg, "HAS_TAG", tag, &BTreeMap::new())
                .expect("tag");
            let _ = j;
            expected += 1;
        }
    }
    let _ = g.warm();
    assert_eq!(run(&g, SCORES), vec![vec![Value::Int(expected)]]);
    assert_eq!(total_shared, vec![vec![Value::Int(600)]]);
}

#[test]
fn a_write_in_the_same_statement_is_not_answered_from_before_it() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    // THE CORRECTNESS CASE. A statement that writes and then reads must see
    // its own write; an answer remembered from before it would be wrong, so
    // the memo declines while a transaction holds writes.
    let g = corpus(4, 1, 2);
    let before = run(&g, SCORES);
    assert_eq!(
        before,
        vec![vec![Value::Int(8)]],
        "4 people x 1 hub x 2 msgs"
    );

    // add a message to the hub inside one statement, then score again
    let rows = run(
        &g,
        "MATCH (tag:Tag {k: 0}) MATCH (h:Person {k: 900}) \
         CREATE (m:Message {k: 1})-[:HAS_CREATOR]->(h) \
         WITH tag, h, m CREATE (m)-[:HAS_TAG]->(tag) \
         WITH tag \
         MATCH (p:Person)-[:KNOWS]->(f:Person) \
         RETURN sum(size([(tag)<-[:HAS_TAG]-(msg:Message)-[:HAS_CREATOR]->(f) | msg])) AS score",
    );
    assert_eq!(
        rows,
        vec![vec![Value::Int(12)]],
        "4 people x 1 hub x 3 msgs"
    );
}

#[test]
fn a_later_statement_sees_a_committed_write() {
    let _serial = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    // And across statements: the memo is held for one read snapshot, so a
    // committed write must be visible to the next one.
    let g = corpus(4, 1, 2);
    assert_eq!(run(&g, SCORES), vec![vec![Value::Int(8)]]);
    run(
        &g,
        "MATCH (tag:Tag {k: 0}), (h:Person {k: 900}) \
         CREATE (m:Message {k: 2})-[:HAS_CREATOR]->(h) \
         WITH tag, m CREATE (m)-[:HAS_TAG]->(tag) RETURN count(*)",
    );
    assert_eq!(
        run(&g, SCORES),
        vec![vec![Value::Int(12)]],
        "the second statement answered from the first's snapshot"
    );
}
