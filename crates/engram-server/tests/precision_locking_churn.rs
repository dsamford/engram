#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! §7 under real concurrency: precision locking must change LATENCY, not
//! ANSWERS.
//!
//! The single-threaded arms are covered elsewhere — `engram-graph`'s
//! `precision_locking.rs` pins the phantom in both directions, and the TCK
//! ratchet runs identically with the lever on and off. Neither can see the
//! thing that actually goes wrong with a predicate validator in production:
//!
//! - **A livelock.** It aborts MORE, and every abort is a Bolt-level retry. A
//!   validator that makes a churn workload retry without converging turns an
//!   isolation upgrade into a hang, and the symptom is a workload that never
//!   finishes rather than one that fails.
//! - **A reconciliation break.** Aborts are invisible to the client — the Bolt
//!   loop re-runs them — so the only way to see that they stayed CORRECT is to
//!   count what the clients acked against what the store holds.
//!
//! So the assertion is the plan's own: **acked creates − acked deletes ==
//! survivors**, on the arm that has precision locking on, under the churn shape
//! that this whole programme is about.

use std::net::TcpListener;

use engram_bolt::client::Client;
use engram_cypher::Value;
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CLIENTS: usize = 4;
const PER: usize = 40;

fn start(precision: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let cfg = engram_server::ServerConfig {
            workers: 2,
            configure_graph: Some(std::sync::Arc::new(move |g: &engram_graph::Graph| {
                g.set_precision_locking(precision);
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
    for _ in 0..200 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("server never came up");
}

/// The first column of the first row, as an integer.
///
/// `Client::query` hands back one `Value` per ROW, and a row is a `List` of its
/// columns — so the count is one level deeper than it looks.
fn count_of(c: &mut Client, q: &str) -> i64 {
    match c.query(q).expect("count").first() {
        Some(Value::List(cols)) => match cols.first() {
            Some(Value::Int(n)) => *n,
            other => panic!("expected an integer count, got {other:?}"),
        },
        other => panic!("expected a row, got {other:?}"),
    }
}

/// The churn shape: each client creates its own nodes and deletes half of them,
/// with a `MATCH` on a label every other client is also writing to — so every
/// statement's predicate is one a concurrent commit can satisfy, which is
/// exactly the condition precision locking acts on.
fn churn(addr: &str) -> (i64, i64) {
    {
        let mut c = connect(addr);
        c.run("CREATE (:Anchor {a: 0})").expect("seed");
    }
    let mut handles = Vec::with_capacity(CLIENTS);
    for ci in 0..CLIENTS {
        let addr = addr.to_string();
        handles.push(std::thread::spawn(move || {
            let mut c = connect(&addr);
            let (mut created, mut deleted) = (0i64, 0i64);
            for j in 0..PER {
                let k = ci * PER + j;
                c.run(&format!("CREATE (:Churn {{k: {k}, owner: {ci}}})"))
                    .expect("create");
                created += 1;
                if j % 2 == 0 {
                    // A MATCH on a predicate other clients are concurrently
                    // satisfying — the phantom-prone shape.
                    c.run(&format!("MATCH (n:Churn {{k: {k}, owner: {ci}}}) DELETE n"))
                        .expect("delete");
                    deleted += 1;
                }
            }
            (created, deleted)
        }));
    }
    let mut created = 0i64;
    let mut deleted = 0i64;
    for h in handles {
        let (c, d) = h.join().expect("client");
        created += c;
        deleted += d;
    }
    (created, deleted)
}

/// RECONCILIATION on the ON arm, and on the OFF arm as its control.
///
/// Both arms must reconcile. The point is not that precision locking makes the
/// count right — it was right before — but that turning it on does not make it
/// wrong, and does not hang: the workload has to FINISH, which is itself the
/// livelock assertion.
#[test]
fn churn_reconciles_with_precision_locking_on() {
    for precision in [false, true] {
        let addr = start(precision);
        let (created, deleted) = churn(&addr);
        let mut c = connect(&addr);
        let survivors = count_of(&mut c, "MATCH (n:Churn) RETURN count(n)");
        eprintln!(
            "[precision churn] precision={precision}: acked {created} creates, \
             {deleted} deletes, {survivors} survivors"
        );
        assert!(
            created > 0 && deleted > 0,
            "the fixture must actually churn, or reconciling is vacuous"
        );
        assert_eq!(
            created - deleted,
            survivors,
            "acked creates minus acked deletes must equal survivors \
             (precision={precision}) — every abort is retried by the Bolt loop, \
             so an abort that changed the OUTCOME shows up here and nowhere else"
        );
    }
}

/// The ON arm must not answer differently, either. Reconciliation counts rows;
/// this compares the actual contents.
#[test]
fn the_arms_answer_identically() {
    let read = |precision: bool| -> Vec<i64> {
        let addr = start(precision);
        let mut c = connect(&addr);
        for k in 0..30 {
            c.run(&format!("CREATE (:P {{k: {k}, tag: {}}})", k % 3))
                .expect("create");
        }
        for k in (0..30).step_by(4) {
            c.run(&format!("MATCH (n:P {{k: {k}}}) DELETE n"))
                .expect("delete");
        }
        c.query("MATCH (n:P {tag: 1}) RETURN n.k ORDER BY n.k")
            .expect("read")
            .into_iter()
            .filter_map(|v| match v {
                Value::List(cols) => match cols.first() {
                    Some(Value::Int(n)) => Some(*n),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    let off = read(false);
    let on = read(true);
    eprintln!("[precision churn] tag=1 survivors: {on:?}");
    assert!(!on.is_empty(), "the fixture must return rows to compare");
    assert_eq!(
        on, off,
        "precision locking changes which transactions ABORT, and an abort is \
         retried — so the answers a client sees must be identical"
    );
}
