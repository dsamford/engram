#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! Uniqueness under REAL concurrency, over Bolt (W1.2): N racing sessions
//! creating the same fresh value must admit EXACTLY ONE — the rest get a
//! clean constraint violation, never a silent duplicate. The MERGE variant
//! must converge on one node with every session succeeding.

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
fn racing_creates_of_one_value_admit_exactly_one() {
    let (addr, g) = start();
    let mut setup = connect(&addr);
    setup
        .run("CREATE CONSTRAINT ru FOR (n:R) REQUIRE n.u IS UNIQUE")
        .expect("constraint");
    const THREADS: usize = 8;
    const ROUNDS: u64 = 20;
    for round in 0..ROUNDS {
        let ok = Arc::new(AtomicU64::new(0));
        let refused = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let addr = addr.clone();
            let ok = Arc::clone(&ok);
            let refused = Arc::clone(&refused);
            handles.push(std::thread::spawn(move || {
                let mut c = connect(&addr);
                match c.run(&format!("CREATE (:R {{u: {round}}})")) {
                    Ok(_) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        assert!(
                            msg.contains("already exists") || msg.contains("Constraint"),
                            "round {round}: unexpected failure: {msg}"
                        );
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("creator");
        }
        assert_eq!(
            (ok.load(Ordering::Relaxed), refused.load(Ordering::Relaxed)),
            (1, (THREADS - 1) as u64),
            "round {round}: exactly one winner"
        );
        let mut c = connect(&addr);
        assert_eq!(
            c.run(&format!("MATCH (n:R {{u: {round}}}) RETURN id(n)"))
                .expect("count"),
            1,
            "round {round}: population is one"
        );
    }
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn racing_merges_of_one_value_converge_on_one_node() {
    let (addr, g) = start();
    let mut setup = connect(&addr);
    setup
        .run("CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE")
        .expect("constraint");
    setup
        .run("CREATE INDEX m_u FOR (n:M) ON (n.u)")
        .expect("index");
    const THREADS: usize = 8;
    const ROUNDS: u64 = 10;
    for round in 0..ROUNDS {
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let addr = addr.clone();
            handles.push(std::thread::spawn(move || {
                let mut c = connect(&addr);
                // MERGE must always succeed: the losers' re-runs take the
                // match arm against the winner's node.
                c.run(&format!("MERGE (n:M {{u: {round}}})"))
                    .expect("merge succeeds");
            }));
        }
        for h in handles {
            h.join().expect("merger");
        }
        let mut c = connect(&addr);
        assert_eq!(
            c.run(&format!("MATCH (n:M {{u: {round}}}) RETURN id(n)"))
                .expect("count"),
            1,
            "round {round}: MERGE converged"
        );
    }
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}
