#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! Guard rows under REAL concurrency (W1.1): racing Bolt sessions creating
//! relationships onto a node while another session DETACH DELETEs it must
//! never commit a dangling edge — every round ends with
//! `verify_rel_endpoints` clean. And the accepted hub cost must stay a
//! retry, not a loss: N racing rel-creates on one hub all land.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use engram_bolt::client::Client;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn start() -> (String, Graph) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let store = Store::new();
    let server_store = store.clone();
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_workers(
            listener,
            move || (server_store, Realm(1), Namespace(1)),
            4,
        );
    });
    // A verification handle over the SAME store — reads committed state.
    let g = Graph::new(store, Realm(1), Namespace(1));
    (addr, g)
}

fn connect(addr: &str) -> Client {
    for _ in 0..50 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
    }
    panic!("server never became reachable");
}

#[test]
fn racing_rel_creates_and_detach_deletes_never_leave_a_dangling_edge() {
    let (addr, g) = start();
    let mut setup = connect(&addr);
    const ROUNDS: u64 = 200;
    for i in 0..ROUNDS {
        setup
            .run(&format!("CREATE (:Hub {{k: {i}}})"))
            .expect("hub created");
        let addr_a = addr.clone();
        let addr_b = addr.clone();
        let a = std::thread::spawn(move || {
            let mut c = connect(&addr_a);
            // May legitimately fail (the hub is being deleted) — what may
            // NOT happen is a silently committed dangling edge.
            let _ = c.run(&format!(
                "MATCH (h:Hub {{k: {i}}}) CREATE (h)<-[:R]-(:Sat {{k: {i}}})"
            ));
        });
        let b = std::thread::spawn(move || {
            let mut c = connect(&addr_b);
            let _ = c.run(&format!("MATCH (h:Hub {{k: {i}}}) DETACH DELETE h"));
        });
        a.join().expect("creator thread");
        b.join().expect("deleter thread");
        let bad = g.verify_rel_endpoints().expect("fsck");
        assert_eq!(
            bad,
            Vec::<u64>::new(),
            "round {i}: dangling relationship(s) {bad:?}"
        );
    }
}

#[test]
fn racing_hub_rel_creates_all_land_serialised_through_the_guard() {
    let (addr, g) = start();
    let mut setup = connect(&addr);
    setup.run("CREATE (:Hub {k: 424242})").expect("hub");
    const THREADS: usize = 4;
    const PER: usize = 25;
    let acked = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let addr = addr.clone();
        let acked = Arc::clone(&acked);
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for j in 0..PER {
                match c.run(&format!(
                    "MATCH (h:Hub {{k: 424242}}) CREATE (h)<-[:R]-(:Sat {{t: {t}, j: {j}}})"
                )) {
                    Ok(_) => {
                        acked.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("hub rel-create failed outright: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("writer thread");
    }
    let acked = acked.load(Ordering::Relaxed);
    assert_eq!(acked, (THREADS * PER) as u64, "every create acked");
    let mut c = connect(&addr);
    let degree = c
        .run("MATCH (:Hub {k: 424242})<-[r:R]-() RETURN id(r)")
        .expect("degree read");
    assert_eq!(degree, acked, "every acked edge exists — serialised, not lost");
    assert_eq!(g.verify_rel_endpoints().expect("fsck"), Vec::<u64>::new());
}
