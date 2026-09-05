#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The conflict-class counter (O0 of `docs/write-concurrency-ceiling.md`)
//! names WHAT the OCC re-runs collide on.
//!
//! RC1 measured 0.88 whole-statement re-executions per acknowledged write on
//! `rel-hub` but could not say what they collided ON — and the two candidate
//! fixes are chosen by that answer. A guard row (`'G' | node id`, written by
//! BOTH endpoints of every relationship create) is a shared-key class a finer
//! conflict unit could remove; a hot property write is a true conflict only
//! ordering can help. Building either without this evidence would be guessing.
//!
//! So this test proves the INSTRUMENT before anything is built on it, in both
//! directions — a classifier that answered "guard" to everything would pass
//! the hub test alone.

use std::net::TcpListener;
use std::sync::atomic::Ordering;

use engram_bolt::client::Client;
use engram_bolt::counters::{CONFLICT_CLASS, conflict_classes};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The counters are process-global statics and cargo runs these two tests in
/// the same binary, in parallel — each `reset()` would wipe the other's counts
/// and both would read a blend. Serialised, not because the code needs it but
/// because the INSTRUMENT is global.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const GUARD: usize = 0;
const ENTITY: usize = 4;
const CLIENTS: usize = 8;
const PER: usize = 40;

fn start(workers: usize) -> String {
    start_with(workers, true)
}

/// `exempt` is the guard put-vs-put lever (RC1 / O3). The OFF arm is the world
/// O0 diagnosed; the ON arm is the world after the fix.
fn start_with(workers: usize, exempt: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let cfg = engram_server::ServerConfig {
            workers,
            configure_graph: Some(std::sync::Arc::new(move |g: &engram_graph::Graph| {
                g.set_guard_put_put_exempt(exempt);
            })),
            ..engram_server::ServerConfig::default()
        };
        let _ = engram_server::run_server_with_config(
            listener,
            || (Store::new(), Realm(1), Namespace(1)),
            cfg,
        );
    });
    addr
}

fn connect(addr: &str) -> Client {
    for _ in 0..50 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
    }
    panic!("server never became reachable");
}

fn reset() {
    for c in CONFLICT_CLASS.iter() {
        c.store(0, Ordering::Relaxed);
    }
}

fn total(v: &[u64; 6]) -> u64 {
    v.iter().sum()
}

/// THE DIAGNOSIS (exemption OFF). N sessions creating relationships onto ONE
/// hub. Every create writes the guard row of BOTH endpoints, so the hub's
/// guard is the key all of them share — the `rel-hub` shape, and the evidence
/// that chose RC1's fix.
#[test]
fn without_the_exemption_hub_creates_bucket_as_guard_conflicts() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset();
    let addr = start_with(6, false);
    {
        let mut c = connect(&addr);
        c.run("CREATE (:Hub {k: 0})").expect("hub");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for ci in 0..CLIENTS {
        let addr = addr.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for j in 0..PER {
                let _ = c.run(&format!(
                    "MATCH (h:Hub {{k: 0}}) CREATE (h)-[:R]->(:Leaf {{c: {ci}, j: {j}}})"
                ));
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }

    let classes = conflict_classes();
    assert!(
        total(&classes) > 0,
        "the hub workload must produce conflicts at all, else this proves \
         nothing: {classes:?}"
    );
    assert!(
        classes[GUARD] * 2 >= total(&classes),
        "a shared hub's conflicts are GUARD-row conflicts — at least half — \
         got [guard, marker, adjacency, membership, entity, other] = {classes:?}"
    );
}

/// THE FIX (exemption ON), measured by the same instrument. The guard
/// conflicts the test above records are gone — that is what RC1 does, and
/// pinning it here means a regression shows up as conflicts reappearing
/// rather than as a throughput number nobody reads.
#[test]
fn with_the_exemption_hub_creates_stop_conflicting_on_the_guard() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset();
    let addr = start_with(6, true);
    {
        let mut c = connect(&addr);
        c.run("CREATE (:Hub {k: 0})").expect("hub");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for ci in 0..CLIENTS {
        let addr = addr.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for j in 0..PER {
                let _ = c.run(&format!(
                    "MATCH (h:Hub {{k: 0}}) CREATE (h)-[:R]->(:Leaf {{c: {ci}, j: {j}}})"
                ));
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    let classes = conflict_classes();
    assert_eq!(
        classes[GUARD], 0,
        "the exemption removes guard conflicts entirely on this shape: {classes:?}"
    );
}

/// The other direction: eight sessions incrementing ONE property on ONE node.
/// No relationship is created, so no guard row is shared — a classifier
/// reading the wrong byte would still say "guard" here.
#[test]
fn a_hot_property_write_does_not_bucket_as_guard() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset();
    let addr = start(6);
    {
        let mut c = connect(&addr);
        c.run("CREATE (:Hot {k: 0, hits: 0})").expect("seed");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let addr = addr.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for _ in 0..PER {
                let _ = c.run("MATCH (n:Hot {k: 0}) SET n.hits = n.hits + 1");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }

    let classes = conflict_classes();
    assert!(
        total(&classes) > 0,
        "the hot-key workload must produce conflicts: {classes:?}"
    );
    assert_eq!(
        classes[GUARD], 0,
        "a hot PROPERTY write shares no guard row: {classes:?}"
    );
    assert!(
        classes[ENTITY] > 0,
        "and it must land in the entity-record bucket rather than 'other': \
         {classes:?}"
    );
}
