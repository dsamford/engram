#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! How many bytes a write copies into a log payload — the measurement that
//! decides whether the concatenation is worth removing.
//!
//! `log_payload` is the third copy of every record's bytes: `rec.encode()`
//! produces them, this concatenates them with the body into a fresh allocation,
//! and the tail copies them again. Removing it means hashing the parts
//! incrementally and writing from an `IoSlice` — cleverness in the WAL path,
//! which is the last place to put it on an assumption.
//!
//! Two changes already shrink the term before it is measured: volatile guard
//! rows keep two of six rows per edge out of the log entirely, and the log is
//! released at a seal rather than retained for the process lifetime. So this
//! file exists to answer "how much is left", and to keep answering it — a
//! sizing decision made once and never re-checked is a decision that rots.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use engram_cypher::parse_statement;
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::{LOG_BYTES_COPIED, Store};

/// The counter is process-wide, so measurements must not interleave.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn stmt(g: &Graph, src: &str) -> Result<(), String> {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g.commit_owned(txn).map_err(|e| format!("{e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            Err(format!("{e:?}"))
        }
    }
}

/// Bytes copied per acked statement, for the two shapes the write path is
/// dominated by.
#[test]
fn report_bytes_copied_per_acked_write() {
    let _serial = serial();

    // Relationship create: the six-rows-per-edge shape.
    let g = graph();
    for i in 0..40i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})")).expect("seed");
    }
    let before = LOG_BYTES_COPIED.load(Ordering::Relaxed);
    const N: u64 = 100;
    for i in 0..N as i64 {
        let a = i % 40;
        let b = (a + 1) % 40;
        stmt(
            &g,
            &format!("MATCH (x:P {{id: {a}}}), (y:P {{id: {b}}}) CREATE (x)-[:R]->(y)"),
        )
        .expect("rel");
    }
    let rel_bytes = LOG_BYTES_COPIED.load(Ordering::Relaxed) - before;

    // Node create: the simplest write there is.
    let g2 = graph();
    let before2 = LOG_BYTES_COPIED.load(Ordering::Relaxed);
    for i in 0..N as i64 {
        stmt(&g2, &format!("CREATE (:Q {{id: {i}, tag: 'x'}})")).expect("node");
    }
    let node_bytes = LOG_BYTES_COPIED.load(Ordering::Relaxed) - before2;

    eprintln!(
        "[log payload] bytes copied per acked statement: \
         rel-create {:.1} B, node-create {:.1} B",
        rel_bytes as f64 / N as f64,
        node_bytes as f64 / N as f64
    );

    // The instrument must actually count, or the number above is a zero nobody
    // questioned. This is the non-vacuity half.
    assert!(
        rel_bytes > 0 && node_bytes > 0,
        "the counter must move: rel {rel_bytes}, node {node_bytes}"
    );
    // And it must be proportional to the work: 100 statements cannot copy less
    // than 100 record headers' worth.
    assert!(
        rel_bytes > N * 4,
        "each statement copies at least a length prefix per row it logs"
    );
}

/// VOLATILE GUARD ROWS keep bytes out of the log payload entirely — the change
/// that shrinks this term before it is measured, shown rather than asserted.
#[test]
fn volatile_guards_reduce_the_bytes_copied() {
    let _serial = serial();
    let measure = |volatile: bool| -> u64 {
        let g = graph();
        g.set_volatile_guards(volatile);
        for i in 0..10i64 {
            stmt(&g, &format!("CREATE (:P {{id: {i}}})")).expect("seed");
        }
        let before = LOG_BYTES_COPIED.load(Ordering::Relaxed);
        for i in 0..20i64 {
            let a = i % 10;
            let b = (a + 1) % 10;
            stmt(
                &g,
                &format!("MATCH (x:P {{id: {a}}}), (y:P {{id: {b}}}) CREATE (x)-[:R]->(y)"),
            )
            .expect("rel");
        }
        LOG_BYTES_COPIED.load(Ordering::Relaxed) - before
    };
    let with = measure(true);
    let without = measure(false);
    eprintln!("[log payload] 20 rel-creates copy {with} B volatile, {without} B logged");
    assert!(
        with < without,
        "a guard row that never reaches the log copies no payload bytes: \
         {with} vs {without}"
    );
}
