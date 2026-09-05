#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! The maintenance thread's READER-INDEPENDENT publish, end to end over Bolt:
//! a write-only burst, no read, the tick — and the first read afterwards
//! finds its adjacency table current (`graph.adjacency tables built` does not
//! fire, nor does `repaired`), because the maintenance thread already ran
//! `Graph::refresh_stale_derived`. The canary runs the same burst with the
//! refresh OFF and the read pays.
//!
//! The engine's `counted!` traces are thread-local and the server's threads
//! install none, so the observation is the process-wide counters
//! (`engram_graph::counters`, `engram_server::counters`) read as deltas —
//! which is why the two tests here serialise on one lock: both servers live
//! in this process and would otherwise count into each other's window.

use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use engram_bolt::client::Client;
use engram_graph::counters::{ADJ_TABLES_BUILT, ADJ_TABLES_REPAIRED, DERIVED_REFRESHED_BY_MAINTENANCE};
use engram_key::{Namespace, Realm};
use engram_server::counters::MAINTENANCE_REFRESH_RUNS;
use engram_store::Store;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const TICK: Duration = Duration::from_millis(150);

fn start(derived_refresh: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let cfg = engram_server::ServerConfig {
            workers: 1,
            derived_refresh,
            // Tick-driven only: the burst below is shorter than any sane
            // commit-count threshold, and the tick is what catches it.
            refresh_after_writes: 0,
            maintenance_tick: TICK,
            configure_graph: Some(std::sync::Arc::new(|g: &engram_graph::Graph| {
                // The first probe admits a table, so one read builds it.
                g.set_degree_table_after(0);
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
    for _ in 0..100 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server never became reachable");
}

const PERSONS: u64 = 50;
const SEED: u64 = 200;
const BURST: u64 = 1_500;
const READ: &str = "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {id: 7}) RETURN m.id";

/// Seed persons and messages, build the HAS_CREATOR table with one read.
fn seed(c: &mut Client) {
    for i in 0..PERSONS {
        c.run(&format!("CREATE (:Person {{id: {i}}})")).expect("person");
    }
    for i in 0..SEED {
        c.run(&format!(
            "MATCH (p:Person {{id: {}}}) CREATE (:Message {{id: {i}}})-[:HAS_CREATOR]->(p)",
            i % PERSONS
        ))
        .expect("message");
    }
    let built = ADJ_TABLES_BUILT.load(Relaxed);
    assert_eq!(c.run(READ).expect("read"), SEED / PERSONS);
    assert!(
        ADJ_TABLES_BUILT.load(Relaxed) > built,
        "the seeding read must have BUILT the HAS_CREATOR table, or the test proves nothing"
    );
}

/// Wait until `n` further maintenance passes have COMPLETED — the pass that
/// started before the burst ended does not count, so two are waited for.
fn wait_for_passes(n: u64) {
    let target = MAINTENANCE_REFRESH_RUNS.load(Relaxed) + n;
    let deadline = Instant::now() + Duration::from_secs(20);
    while MAINTENANCE_REFRESH_RUNS.load(Relaxed) < target {
        assert!(Instant::now() < deadline, "the maintenance thread never ticked");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn burst(c: &mut Client) {
    for i in 0..BURST {
        c.run(&format!(
            "MATCH (p:Person {{id: {}}}) CREATE (:Message {{id: {}}})-[:HAS_CREATOR]->(p)",
            i % PERSONS,
            SEED + i
        ))
        .expect("burst write");
    }
}

#[test]
fn after_a_write_only_burst_the_tick_refreshes_and_the_first_read_builds_nothing() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let addr = start(true);
    let mut c = connect(&addr);
    seed(&mut c);
    // Quiescent before the burst, so the deltas below are the burst's alone.
    wait_for_passes(1);

    let refreshed_before = DERIVED_REFRESHED_BY_MAINTENANCE.load(Relaxed);
    burst(&mut c);
    wait_for_passes(2);
    assert!(
        DERIVED_REFRESHED_BY_MAINTENANCE.load(Relaxed) > refreshed_before,
        "the tick ran but refreshed nothing after a {BURST}-write burst"
    );

    // THE CLAIM: the first read after the burst finds the table current.
    let built = ADJ_TABLES_BUILT.load(Relaxed);
    let repaired = ADJ_TABLES_REPAIRED.load(Relaxed);
    assert_eq!(c.run(READ).expect("read"), (SEED + BURST) / PERSONS, "the read must see the burst");
    assert_eq!(
        ADJ_TABLES_BUILT.load(Relaxed),
        built,
        "the first read after the burst BUILT an adjacency table — the refresh did not publish"
    );
    assert_eq!(
        ADJ_TABLES_REPAIRED.load(Relaxed),
        repaired,
        "the first read after the burst REPAIRED an adjacency table — the refresh left it stale"
    );
}

/// The canary: refresh OFF, the same burst and the same wait, and the first
/// read pays — it repairs or rebuilds the table itself.
#[test]
fn with_the_refresh_off_the_first_read_pays_for_the_burst() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let addr = start(false);
    let mut c = connect(&addr);
    seed(&mut c);
    let refreshed_before = DERIVED_REFRESHED_BY_MAINTENANCE.load(Relaxed);
    burst(&mut c);
    // The maintenance thread still ticks (paged seals live there) but must
    // not refresh; give it the same window the claim above got.
    std::thread::sleep(TICK * 3);
    assert_eq!(DERIVED_REFRESHED_BY_MAINTENANCE.load(Relaxed), refreshed_before);

    let built = ADJ_TABLES_BUILT.load(Relaxed);
    let repaired = ADJ_TABLES_REPAIRED.load(Relaxed);
    assert_eq!(c.run(READ).expect("read"), (SEED + BURST) / PERSONS);
    assert!(
        ADJ_TABLES_BUILT.load(Relaxed) + ADJ_TABLES_REPAIRED.load(Relaxed) > built + repaired,
        "with the refresh off the read did no adjacency work — the claim above is not \
         testing the refresh"
    );
}
