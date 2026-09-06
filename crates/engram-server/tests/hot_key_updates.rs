#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! Concurrent updates of ONE node through the Bolt server, asserted for
//! correctness rather than timed — the `contention` profile of the stress
//! harness. A throughput number on a workload that loses updates is not a
//! number.
//!
//! Two guarantees, two tests:
//!
//! 1. RECORD-level: `SET` reads the node's record, changes one property and
//!    writes the record back. Two sessions setting DIFFERENT properties must
//!    not each write back a record missing the other's — the per-entity write
//!    latch (`Graph::entity_latches`) gives this.
//! 2. STATEMENT-level: `SET n.hits = n.hits + 1` reads `n.hits` when `MATCH`
//!    materialises the node, before the write. Two sessions can both read 5
//!    and both write 6. No latch inside the write path can cover a read that
//!    happened before it; this needs the statement to run as a transaction
//!    whose commit validates its read-set. SERIALISABLE AUTOCOMMIT is that,
//!    and it is on by default — `concurrent_autocommit_increments_of_one_node_all_land`
//!    asserts every increment lands, and
//!    `without_serialisable_autocommit_concurrent_increments_are_lost` is its
//!    canary, showing the loss with the lever off.
//!
//!    (This paragraph described the guarantee as unbuilt and the test as
//!    `#[ignore]`d long after both had changed. Neither is true: there is no
//!    `#[ignore]` in this file and all four tests run.)

use std::net::TcpListener;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn start(workers: usize) -> String {
    start_with(workers, true, true)
}

/// A server whose graphs have the entity latches and serialisable autocommit
/// on or off — the canaries' arms are the `false` ones.
fn start_with(workers: usize, entity_latching: bool, serialisable: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let cfg = engram_server::ServerConfig {
            workers,
            configure_graph: Some(std::sync::Arc::new(move |g: &engram_graph::Graph| {
                g.set_entity_latching(entity_latching);
                g.set_serialisable_autocommit(serialisable);
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

/// Runs the distinct-property workload and returns how many of the eight
/// properties did NOT end at their final value.
fn properties_lost(addr: &str) -> usize {
    {
        let mut c = connect(addr);
        c.run("CREATE (:Hot {k: 0})").expect("seed");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for ci in 0..CLIENTS {
        let addr = addr.to_string();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for j in 0..PER {
                c.run(&format!("MATCH (n:Hot {{k: 0}}) SET n.p{ci} = {j}"))
                    .expect("set");
            }
        }));
    }
    for h in handles {
        h.join().expect("client");
    }
    let mut c = connect(addr);
    let last = PER - 1;
    (0..CLIENTS)
        .filter(|ci| {
            c.run(&format!("MATCH (n:Hot {{k: 0}}) WHERE n.p{ci} = {last} RETURN n"))
                .expect("read")
                != 1
        })
        .count()
}

fn connect(addr: &str) -> Client {
    for _ in 0..50 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
    }
    panic!("server never became reachable");
}

const WORKERS: usize = 8;
const CLIENTS: usize = 8;
const PER: usize = 200;

/// Every client repeatedly sets ITS OWN property on the one node. At the end
/// the node must carry all eight properties at their final values: a record
/// written back from a stale read would have dropped another client's.
#[test]
fn concurrent_sets_of_different_properties_on_one_node_keep_every_property() {
    let lost = properties_lost(&start(WORKERS));
    assert_eq!(
        lost, 0,
        "{lost} propert(ies) did not hold their final value: a concurrent SET of \
         another property wrote a record back without them"
    );
}

/// The canary: with the latches OFF the same workload must lose a property,
/// or the test above proves nothing. A race is not deterministic, so up to
/// three fresh servers are tried; one observed loss is the proof.
#[test]
fn without_the_entity_latch_a_concurrent_set_loses_a_property() {
    for _ in 0..24 {
        // Serialisable autocommit OFF too: with it on, the conflict
        // validation catches the interleaving the latch is meant to catch,
        // and the canary would be testing the wrong mechanism.
        if properties_lost(&start_with(WORKERS, false, false)) > 0 {
            return;
        }
    }
    panic!(
        "24 runs with the entity latches off lost nothing. Either the latch is not what this test proves, or this host does not interleave the two writers often enough to observe the race it depends on."
    );
}

/// Runs the increment workload and returns `(acknowledged, final value)`.
fn increments(addr: &str) -> (u64, u64) {
    {
        let mut c = connect(addr);
        c.run("CREATE (:Hot {k: 0, hits: 0})").expect("seed");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let addr = addr.to_string();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            let mut acked = 0u64;
            for _ in 0..PER {
                c.run("MATCH (n:Hot {k: 0}) SET n.hits = coalesce(n.hits, 0) + 1")
                    .expect("increment");
                acked += 1;
            }
            acked
        }));
    }
    let acked: u64 = handles.into_iter().map(|h| h.join().expect("client")).sum();
    // The client reports row COUNTS, so read the value as a count of rows.
    let mut c = connect(addr);
    let hits = c
        .run("MATCH (n:Hot {k: 0}) UNWIND range(1, n.hits) AS i RETURN i")
        .expect("read");
    (acked, hits)
}

/// The statement-level read-modify-write. Every increment acknowledged must
/// be in the final value: each statement runs as a read-validated
/// transaction, a loser's commit aborts and the statement re-runs on the new
/// value. Before this existed, 756 of 1,600 landed.
#[test]
fn concurrent_autocommit_increments_of_one_node_all_land() {
    let (acked, hits) = increments(&start(WORKERS));
    assert_eq!(
        hits, acked,
        "{acked} increments were acknowledged but the node holds {hits}: \
         {} update(s) LOST to a concurrent read-modify-write",
        acked - hits
    );
}

/// The canary: with serialisable autocommit OFF the same workload must lose
/// increments, or the test above proves nothing. A race, so fresh servers are
/// tried until one loses.
///
/// 24 attempts rather than 3, for the reason the entity-latch canary above
/// records: a bound tuned where cores are plentiful becomes a false failure on
/// a two-core runner, and a canary that cannot observe its own race reports
/// exactly what a broken mechanism reports.
#[test]
fn without_serialisable_autocommit_concurrent_increments_are_lost() {
    for _ in 0..24 {
        let (acked, hits) = increments(&start_with(WORKERS, true, false));
        if hits < acked {
            return;
        }
    }
    panic!(
        "24 runs without serialisable autocommit lost nothing. Either the transaction is not what this test proves, or this host does not interleave the writers often enough to observe the race."
    );
}
