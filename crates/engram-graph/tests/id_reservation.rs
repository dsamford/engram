#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A serving session reserves a RANGE of ids per counter write.
//!
//! `next_id` held the process-global `alloc` mutex across a FULL LOGGED
//! `store.put` — which itself takes the commit-log mutex, allocates a
//! timestamp, BLAKE3-hashes, writes the WAL buffer, takes a tail shard latch
//! and spins on the visibility barrier. `CREATE (a)-[:R]->(b)` paid that three
//! times, and every OCC retry re-minted DURABLY: 1.80 `alloc` acquisitions per
//! acked op with OCC on against 1.00 with it off.
//!
//! Bulk ingest has reserved ranges since it existed. Serving now does too, with
//! one difference that matters: the serving reservation is LOGGED. The WAL's
//! contract is that replay restores every acknowledged write, and a counter
//! advanced only in memory would let a replayed store re-mint ids it had
//! already handed out.
//!
//! The invariant these defend is not "ids are dense". It is **an id is never
//! reused** — which is what the counter row holding the reserved END buys.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn node(g: &Graph, k: i64) -> u64 {
    let mut props = BTreeMap::new();
    props.insert("k".to_string(), Value::Int(k));
    g.create_node(&["T".to_string()], &props).expect("create")
}

/// Ids stay DENSE within a run, and still start at 1. A reservation that
/// changed the first id, or left holes mid-run, would break every fixture that
/// names an id — and would be a gratuitous compatibility break.
#[test]
fn ids_are_dense_within_a_run_and_still_start_at_one() {
    for reservation in [0usize, 1, 2, 256, 4096] {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        g.set_id_reservation(reservation);
        let ids: Vec<u64> = (0..300).map(|k| node(&g, k)).collect();
        assert_eq!(ids[0], 1, "reservation {reservation}: the first id is 1");
        for (i, w) in ids.windows(2).enumerate() {
            assert_eq!(
                w[1],
                w[0] + 1,
                "reservation {reservation}: id {i} -> {i} must be contiguous \
                 within a run; a reservation refills without leaving a hole"
            );
        }
    }
}

/// The invariant that actually matters: no id is EVER handed out twice, under
/// concurrency, on every reservation size.
#[test]
fn no_id_is_ever_reused_under_concurrency() {
    for reservation in [0usize, 1, 256] {
        let graph = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
        graph.set_id_reservation(reservation);
        const THREADS: usize = 8;
        const PER: usize = 200;

        let mut handles = Vec::with_capacity(THREADS);
        for ti in 0..THREADS {
            let graph = Arc::clone(&graph);
            handles.push(std::thread::spawn(move || {
                (0..PER)
                    .map(|j| node(&graph, (ti * PER + j) as i64))
                    .collect::<Vec<u64>>()
            }));
        }
        let mut all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread"))
            .collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            total,
            "reservation {reservation}: {} duplicate id(s) across {THREADS} threads",
            total - all.len()
        );
    }
}

/// A restart must never re-mint an id the previous run handed out. The counter
/// row holds the reserved END, so the unused tail is abandoned as a GAP — which
/// is the price of the reservation and the reason it is safe.
#[test]
fn a_restart_never_reuses_an_id_it_already_handed_out() {
    let store = Store::new();
    let first: Vec<u64> = {
        let g = Graph::new(store.clone(), Realm(1), Namespace(1));
        g.set_id_reservation(256);
        (0..10).map(|k| node(&g, k)).collect()
    };
    // A second Graph over the SAME store is the in-process stand-in for a
    // restart: it has no reservation cached and must re-read the counter.
    let second: Vec<u64> = {
        let g = Graph::new(store.clone(), Realm(1), Namespace(1));
        g.set_id_reservation(256);
        (0..10).map(|k| node(&g, k)).collect()
    };
    let high = *first.iter().max().expect("non-empty");
    for id in &second {
        assert!(
            *id > high,
            "a restart minted {id}, at or below the previous run's high-water \
             mark {high} — the counter row must hold the reserved END, not the \
             last id used"
        );
    }
    // And the gap is real, which is what makes the invariant hold.
    assert!(
        second[0] > high + 1,
        "the abandoned tail should show as a gap ({high} -> {})",
        second[0]
    );
}

/// Non-vacuity: the lever really switches the path. Without this the tests
/// above could all be running the one-put-per-id arm and passing.
#[test]
fn the_lever_actually_switches_the_allocation_path() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_id_reservation(256);
    let (_, on) = engram_observe::with_trace(|| {
        for k in 0..50 {
            node(&g, k);
        }
    });
    let served = on
        .counters()
        .get("graph.id served from a reservation")
        .copied()
        .unwrap_or(0);
    let minted = on
        .counters()
        .get("graph.id reservations minted")
        .copied()
        .unwrap_or(0);
    assert!(
        served >= 45,
        "ON arm: most ids must come from a reservation, got {served} (minted {minted})"
    );
    assert!(
        minted <= 2,
        "ON arm: 50 nodes must not mint 50 reservations, got {minted}"
    );

    let g2 = Graph::new(Store::new(), Realm(1), Namespace(1));
    g2.set_id_reservation(0);
    let (_, off) = engram_observe::with_trace(|| {
        for k in 0..50 {
            node(&g2, k);
        }
    });
    assert_eq!(
        off.counters()
            .get("graph.id served from a reservation")
            .copied()
            .unwrap_or(0),
        0,
        "OFF arm must take the one-logged-put-per-id path — it is the control"
    );
}
