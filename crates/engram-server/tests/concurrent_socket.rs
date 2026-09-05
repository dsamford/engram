#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! End-to-end concurrent writes through the Bolt server: N clients on N worker
//! threads, all writing the ONE shared graph. This is the M3 payoff — the server
//! is no longer a single engine thread, so different connections run in parallel
//! (the D2 revision) — and the proof it stays correct: every write must land, so
//! a final scan sees exactly the number written (a lost write or a duplicate id
//! collision would show fewer rows).

use std::net::TcpListener;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn start(workers: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_workers(
            listener,
            || (Store::new(), Realm(1), Namespace(1)),
            workers,
        );
    });
    addr
}

fn connect(addr: &str) -> Client {
    // The listener is bound before the server thread starts its accept loop, so
    // the connection sits in the backlog until a worker picks it up; a couple of
    // retries covers the spawn window without a sleep.
    for _ in 0..50 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
    }
    panic!("server never became reachable");
}

#[test]
fn concurrent_connections_write_the_shared_graph_without_loss() {
    const WORKERS: usize = 8;
    const CLIENTS: usize = 8;
    const PER: usize = 25;
    let addr = start(WORKERS);

    let mut handles = Vec::with_capacity(CLIENTS);
    for ci in 0..CLIENTS {
        let addr = addr.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            for j in 0..PER {
                // Each client writes distinct data on its OWN connection, which
                // is pinned to worker (conn_id % WORKERS) — so with 8 clients the
                // writes fan across all 8 workers against the one shared graph.
                let rows = c
                    .run(&format!("CREATE (:Item {{c: {ci}, j: {j}}})"))
                    .expect("create");
                assert_eq!(rows, 0, "CREATE returns no rows");
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    // A fresh connection scans the shared graph: exactly every write landed.
    let total = (CLIENTS * PER) as u64;
    let mut c = connect(&addr);
    let seen = c.run("MATCH (n:Item) RETURN n").expect("scan");
    assert_eq!(
        seen, total,
        "every concurrent write must be present — a lost write or an id collision \
         would leave fewer than {total} rows"
    );
}
