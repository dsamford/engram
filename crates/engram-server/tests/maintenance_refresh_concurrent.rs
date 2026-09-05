#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! The maintenance thread's refresh RACING live Bolt sessions: four workers,
//! a 1 ms tick and a 16-commit refresh threshold, two writer connections
//! creating messages with `HAS_CREATOR`, two reader connections counting a
//! person's messages through the adjacency table the whole time. Each read's
//! count must lie within the writers' acknowledged/attempted bounds for that
//! person at the instants the read began and ended — the observation that a
//! torn or regressed table cannot satisfy.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use engram_bolt::client::Client;
use engram_graph::counters::DERIVED_REFRESHED_BY_MAINTENANCE;
use engram_key::{Namespace, Realm};
use engram_server::counters::MAINTENANCE_REFRESH_RUNS;
use engram_store::Store;

const PERSONS: u64 = 16;
const SEED: u64 = 320;
const WRITES_PER_WRITER: u64 = 1_200;

fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let cfg = engram_server::ServerConfig {
            workers: 4,
            derived_refresh: true,
            refresh_after_writes: 16,
            maintenance_tick: Duration::from_millis(1),
            configure_graph: Some(Arc::new(|g: &engram_graph::Graph| {
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

fn read_query(p: u64) -> String {
    format!("MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {{id: {p}}}) RETURN m.id")
}

#[test]
fn bolt_readers_never_observe_a_torn_or_stale_table_under_the_maintenance_refresh() {
    let addr = start();
    let mut c = connect(&addr);
    for i in 0..PERSONS {
        c.run(&format!("CREATE (:Person {{id: {i}}})")).expect("person");
    }
    for i in 0..SEED {
        c.run(&format!(
            "MATCH (p:Person {{id: {}}}) CREATE (:Message {{id: {i}}})-[:HAS_CREATOR]->(p)",
            i % PERSONS
        ))
        .expect("seed");
    }
    // Build the table the race is over.
    for p in 0..PERSONS {
        assert_eq!(c.run(&read_query(p)).expect("read"), SEED / PERSONS);
    }
    let intent: Arc<Vec<AtomicU64>> = Arc::new((0..PERSONS).map(|_| AtomicU64::new(SEED / PERSONS)).collect());
    let done: Arc<Vec<AtomicU64>> = Arc::new((0..PERSONS).map(|_| AtomicU64::new(SEED / PERSONS)).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let refreshed_before = DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed);
    let runs_before = MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed);

    let writers: Vec<_> = (0..2u64)
        .map(|w| {
            let (addr, intent, done) = (addr.clone(), Arc::clone(&intent), Arc::clone(&done));
            std::thread::spawn(move || {
                let mut c = connect(&addr);
                for i in 0..WRITES_PER_WRITER {
                    let p = (i * 5 + w * 3) % PERSONS;
                    intent[p as usize].fetch_add(1, Ordering::SeqCst);
                    c.run(&format!(
                        "MATCH (p:Person {{id: {p}}}) CREATE (:Message {{id: {}}})-[:HAS_CREATOR]->(p)",
                        SEED + w * WRITES_PER_WRITER + i
                    ))
                    .expect("write");
                    done[p as usize].fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..2u64)
        .map(|r| {
            let (addr, intent, done, stop) =
                (addr.clone(), Arc::clone(&intent), Arc::clone(&done), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut c = connect(&addr);
                let mut reads = 0u64;
                let mut x = 0xA5A5_5A5Au64 + r;
                while !stop.load(Ordering::Relaxed) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let p = x % PERSONS;
                    let lo = done[p as usize].load(Ordering::SeqCst);
                    let got = c.run(&read_query(p)).expect("read");
                    let hi = intent[p as usize].load(Ordering::SeqCst);
                    assert!(
                        lo <= got && got <= hi,
                        "reader {r}: person {p} count {got} outside [{lo}, {hi}] — a torn, stale or \
                         regressed adjacency table under the refresh"
                    );
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|r| r.join().expect("reader")).sum();
    for p in 0..PERSONS {
        assert_eq!(c.run(&read_query(p)).expect("final read"), done[p as usize].load(Ordering::SeqCst));
    }
    let refreshed = DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed) - refreshed_before;
    let runs = MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed) - runs_before;
    eprintln!("[bolt-concurrent] reads={reads} refresh_runs={runs} refreshed={refreshed}");
    assert!(reads >= 200, "too few reads to have raced anything: {reads}");
    assert!(runs >= 5, "the maintenance thread barely ran: {runs}");
    assert!(refreshed >= 1, "the maintenance thread refreshed nothing during the race");
}
