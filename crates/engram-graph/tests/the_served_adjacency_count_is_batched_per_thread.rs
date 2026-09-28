// A real thread is the subject: its share must reach the global when it EXITS.
#![allow(clippy::disallowed_methods)]
//! `ADJ_SERVED_BY_TABLE` — the denominator the server prints every tick — is
//! counted per THREAD and added to the process-wide figure in batches and when
//! the thread exits. It was a `fetch_add` on every adjacency visit a table
//! served: one cache line that every morsel worker wrote on every hop, ~19M
//! times for 1/20 of SF3's pairs in SNB BI bi15's weighting join.
//!
//! What the batching must not lose is the count: a thread's served visits have
//! reached the global by the time the thread has exited. Its trace counts the
//! same visits exactly (`graph.adjacency visit served by a table`, beside the
//! batched add — NOT `graph.adjacency tables reused`, which every table caller
//! counts, the length hints and the bound-peer probes included). This binary
//! holds one test, and the calling thread visits nothing while the worker
//! runs, so the global grows by exactly the worker's count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

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

/// 100 people who each KNOW the next three, 10 messages each, every other one
/// a reply to a message by the next person. One row per edge in every setup
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
    let _ = g.warm();
    g
}

const JOIN: &str = "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
     OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
     WITH pA, pB, count(m1) AS i RETURN count(*) AS pairs, sum(i) AS interactions";

#[test]
fn a_threads_served_visits_reach_the_global_when_it_exits() {
    let g = Arc::new(social());
    let _ = rows(&g, JOIN); // admits and builds the tables
    let before = engram_graph::ADJ_SERVED_BY_TABLE.load(Relaxed);
    let worker = Arc::clone(&g);
    let served = std::thread::spawn(move || {
        let (_, t) = engram_observe::with_trace(|| rows(&worker, JOIN));
        t.counters()
            .get("graph.adjacency visit served by a table")
            .copied()
            .unwrap_or(0)
    })
    .join()
    .expect("the worker thread");
    let after = engram_graph::ADJ_SERVED_BY_TABLE.load(Relaxed);
    assert!(
        served > 100,
        "only {served} visits were served by a table; this checks nothing"
    );
    assert_eq!(
        after - before,
        served,
        "the thread served {served} visits from tables and exited; the global must have grown by exactly that"
    );
}
