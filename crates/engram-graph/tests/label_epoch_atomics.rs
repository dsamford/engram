#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A label's change epoch is read from an ATOMIC beside the log, not from the
//! log itself.
//!
//! `label_epoch` used to take a READ lock on `label_log` — the same lock
//! `note_membership_of` takes in WRITE mode on every node create and every node
//! delete. So every `members()` read serialised against every membership write.
//! That is the mechanism behind the derived-refresh tax appearing at ONE
//! client: the maintenance pass and the single writer contend on one lock.
//!
//! The epoch is what tells a reader its cached membership snapshot is stale, so
//! the failure mode of getting this wrong is not slowness — it is a reader
//! judging a stale snapshot CURRENT and answering without the change. These
//! tests are about that, on both arms.
//!
//! There are TWO writers to the log: the autocommit path (`note_membership_of`)
//! and the TRANSACTION COMMIT path (`touched.labels`). The second was missed on
//! the first attempt at this change, and it is the one every buffered write
//! takes.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Membership answers must be identical on both arms, across creates, deletes
/// and label changes — the epoch decides whether a cached snapshot is reused,
/// so a stale one shows up here as a MISSING or LINGERING row.
#[test]
fn membership_is_identical_on_both_arms() {
    let mut finals = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_label_epoch_atomics(on);

        for i in 0..80i64 {
            run(&g, &format!("CREATE (:Thing {{k: {i}}})"));
            // Read BETWEEN writes: this is what populates and then must
            // invalidate the membership snapshot.
            let seen = rows(&g, "MATCH (n:Thing) RETURN n.k").len();
            assert_eq!(
                seen,
                (i + 1) as usize,
                "arm on={on}: after {} creates the label must hold {}",
                i + 1,
                i + 1
            );
        }
        // Deletes, interleaved with reads for the same reason.
        for i in 0..40i64 {
            run(&g, &format!("MATCH (n:Thing {{k: {i}}}) DELETE n"));
            let seen = rows(&g, "MATCH (n:Thing) RETURN n.k").len();
            assert_eq!(
                seen,
                (80 - i - 1) as usize,
                "arm on={on}: a delete must leave the membership snapshot stale"
            );
        }
        // A label change moves a node between two labels.
        run(&g, "MATCH (n:Thing {k: 50}) SET n:Extra");
        let extra = rows(&g, "MATCH (n:Extra) RETURN n.k").len();
        let thing = rows(&g, "MATCH (n:Thing) RETURN n.k").len();
        finals.push((thing, extra));
    }
    assert_eq!(
        finals[0], finals[1],
        "the epoch may change where it is READ from, never what a reader sees"
    );
    assert_eq!(finals[0], (40, 1));
}

/// THE regression this file exists for: the TRANSACTION COMMIT path must bump
/// the epoch too.
///
/// `touched.labels` records into the log at commit, and the first version of
/// the atomic epoch bumped only in the autocommit path. A buffered membership
/// change then landed in the log while the epoch stayed behind — so a reader
/// judged its snapshot current and answered WITHOUT the change.
#[test]
fn a_transactional_membership_change_moves_the_epoch() {
    for on in [false, true] {
        let g = graph();
        g.set_label_epoch_atomics(on);
        run(&g, "CREATE (:Seed {s: 1})");
        // Populate the membership snapshot so there is something to invalidate.
        assert_eq!(rows(&g, "MATCH (n:Boxed) RETURN n.b").len(), 0);

        // A BUFFERED write: this goes through the transaction commit path, not
        // `note_membership_of`.
        let q = parse_statement("CREATE (:Boxed {b: 1})").expect("parse");
        let txn = g.open_txn();
        let (txn, r) = g.with_txn(txn, || run_query(&g, &q, BTreeMap::new()));
        r.expect("buffered create");
        g.commit_owned(txn).expect("commit");

        assert_eq!(
            rows(&g, "MATCH (n:Boxed) RETURN n.b").len(),
            1,
            "arm on={on}: a TRANSACTIONAL membership change must invalidate the \
             cached snapshot — if the commit path does not bump the epoch, the \
             reader judges its stale snapshot current and answers 0"
        );

        // And a buffered DELETE, the other direction.
        let q2 = parse_statement("MATCH (n:Boxed {b: 1}) DELETE n").expect("parse");
        let txn2 = g.open_txn();
        let (txn2, r2) = g.with_txn(txn2, || run_query(&g, &q2, BTreeMap::new()));
        r2.expect("buffered delete");
        g.commit_owned(txn2).expect("commit");

        assert_eq!(
            rows(&g, "MATCH (n:Boxed) RETURN n.b").len(),
            0,
            "arm on={on}: and a transactional delete must too"
        );
    }
}

/// Under real threads: readers and writers on one shared graph must never see a
/// membership snapshot that misses a committed change.
#[test]
fn concurrent_readers_never_see_a_stale_membership() {
    use std::sync::Arc;
    let g = Arc::new(graph());
    g.set_label_epoch_atomics(true);
    run(&g, "CREATE (:Warm {w: 0})");

    const WRITES: i64 = 200;
    let writer = {
        let g = Arc::clone(&g);
        std::thread::spawn(move || {
            for i in 1..=WRITES {
                run(&g, &format!("CREATE (:Warm {{w: {i}}})"));
            }
        })
    };
    let reader = {
        let g = Arc::clone(&g);
        std::thread::spawn(move || {
            let mut high = 0usize;
            for _ in 0..400 {
                let n = rows(&g, "MATCH (n:Warm) RETURN n.w").len();
                // Monotone: a count that goes DOWN means a reader answered from
                // a snapshot older than one it had already served.
                assert!(
                    n >= high,
                    "membership went backwards: {high} -> {n}, so a stale \
                     snapshot was judged current"
                );
                high = n;
                std::thread::yield_now();
            }
            high
        })
    };
    writer.join().expect("writer");
    let _ = reader.join().expect("reader");

    assert_eq!(
        rows(&g, "MATCH (n:Warm) RETURN n.w").len(),
        (WRITES + 1) as usize,
        "every committed create must be visible once the writer has finished"
    );
}
