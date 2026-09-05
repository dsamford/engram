//! The maintenance refresh's accounting UNDER THE WRITE FENCE.
//!
//! A repair (or membership catch-up) publishes at `fenced(epoch)` — clamped
//! below every writer in flight. When a writer registered its fence at the
//! very stamp the slot already holds, the clamp lands the publish ON that
//! stamp and `Slot::publish` loses to the equal stamp: the slot advanced by
//! nothing. `refresh_stale_derived` reported that as `adjacency_repaired`
//! (and `members_caught_up`), bumped `DERIVED_REFRESHED_BY_MAINTENANCE`,
//! logged the table as brought current — and re-repaired the same table on
//! every pass for as long as the writer ran. It must report `Deferred` and
//! leave the counter alone; once the writer is gone the next pass repairs
//! it for real and reports so.
//!
//! The in-flight writer is a detach-delete of a hub node (its fence spans
//! the whole detach — `Graph::delete_node`), started with nothing written
//! between the snapshots' build and its fence read, so its `r` equals the
//! slots' stamp exactly — the equal-stamp case, by construction.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use engram_graph::counters::DERIVED_REFRESHED_BY_MAINTENANCE;
use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const NODES: u64 = 2_000;
/// The hub's `T` relationships — the detach-delete's duration. Long enough
/// for three refresh passes (each repairs ONE row) to run inside it.
const HUB: u64 = 120_000;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn fixture() -> (Arc<Graph>, Vec<u64>, u64, u32) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES).map(|_| g.create_node(&label, &none).expect("node")).collect();
    let hub = g.create_node(&label, &none).expect("hub");
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    for &src in &ids {
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(src, "U", dst, &none).expect("U");
    }
    for _ in 0..HUB {
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(hub, "T", dst, &none).expect("T");
    }
    g.shared_store().seal();
    let u = g.type_tokens_peek(&["U".to_string()]).expect("U")[0];
    (Arc::new(g), ids, hub, u)
}

/// Start the hub's detach-delete on its own thread and wait until it is
/// visibly under way (fence registered, rows going).
fn start_hub_delete(g: &Arc<Graph>, hub: u64) -> std::thread::JoinHandle<()> {
    let h = {
        let g = Arc::clone(g);
        std::thread::spawn(move || g.delete_node(hub, true).expect("detach delete"))
    };
    let before = g.rels_of(hub, Dir::Out, None).expect("rels").len();
    assert_eq!(before, HUB as usize);
    while g.rels_of(hub, Dir::Out, None).expect("rels").len() == before {
        std::thread::yield_now();
    }
    h
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

#[test]
fn a_repair_whose_fenced_publish_loses_is_reported_deferred_not_repaired() {
    let (g, ids, hub, u) = fixture();
    let none = BTreeMap::new();
    // U's table and N's membership, both published at the visible clock
    // with no writer in flight; nothing is written before the delete's
    // fence reads that same clock.
    assert_eq!(g.adjacent_slim(ids[0], Dir::Out, &Some(vec![u])).len(), 1);
    assert_eq!(g.members(Some("N")).expect("members").len(), NODES as usize + 1);
    let del = start_hub_delete(&g, hub);
    // Behind the fence: one U write and one N node make both structures
    // stale (their epochs move past the slots' stamp).
    g.create_rel(ids[1], "U", ids[2], &none).expect("U after the fence registered");
    let extra = g.create_node(&["N".to_string()], &none).expect("N after the fence registered");

    let counter_before = DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed);
    let (first, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    let (second, _) = engram_observe::with_trace(|| g.refresh_stale_derived());
    let still_running = !g.rels_of(hub, Dir::Out, None).expect("rels").is_empty();
    let counter_during = DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed);
    del.join().expect("delete");
    eprintln!("[fenced] first={first:?} second={second:?} writer_still_in_flight={still_running}");
    assert!(still_running, "the fixture is too small: the delete finished before the passes ran");

    // The work WAS done (the repair and the catch-up ran and were fenced)...
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 1, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.membership snapshots caught up"), 1, "{:?}", trace.counters());
    assert!(count(&trace, "graph.publish stamp fenced below an in-flight writer") >= 2, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.adjacency repair publish lost, slot unchanged"), 1, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.membership catch-up publish lost, slot unchanged"), 1, "{:?}", trace.counters());
    // ...but the slots did not move, and the pass must say so.
    for (which, r) in [("first", first), ("second", second)] {
        assert_eq!(r.adjacency_repaired, 0, "{which}: a repair whose publish LOST was reported repaired: {r:?}");
        assert_eq!(r.adjacency_rebuilt, 0, "{which}: {r:?}");
        assert_eq!(r.adjacency_deferred, 1, "{which}: the lost publish must be reported deferred: {r:?}");
        assert_eq!(r.members_caught_up, 0, "{which}: a catch-up whose publish LOST was reported caught up: {r:?}");
        assert_eq!(r.members_rebuilt, 0, "{which}: {r:?}");
        assert_eq!(r.members_deferred, 1, "{which}: {r:?}");
        assert!(!r.any(), "{which}: a pass that advanced nothing claims to have brought something current: {r:?}");
    }
    assert_eq!(
        counter_during, counter_before,
        "DERIVED_REFRESHED_BY_MAINTENANCE was bumped by a pass that advanced no slot"
    );

    // The writer is gone: the next pass repairs and catches up for real.
    let counter_before = DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed);
    let third = g.refresh_stale_derived();
    eprintln!("[settled] third={third:?}");
    assert_eq!((third.adjacency_repaired, third.adjacency_rebuilt, third.adjacency_deferred), (1, 0, 0), "{third:?}");
    assert_eq!((third.members_caught_up, third.members_rebuilt, third.members_deferred), (1, 0, 0), "{third:?}");
    assert_eq!(DERIVED_REFRESHED_BY_MAINTENANCE.load(Ordering::Relaxed), counter_before + 2);
    assert!(!g.refresh_stale_derived().any(), "a fourth pass must find nothing stale");
    // And the structures are right: the reader finds them current, with
    // the post-fence write and the delete both applied.
    let ((row, members), trace) = engram_observe::with_trace(|| {
        (
            g.adjacent_slim(ids[1], Dir::Out, &Some(vec![u])).len(),
            g.members(Some("N")).expect("members"),
        )
    });
    assert_eq!(row, 2, "ids[1]'s original U plus the post-fence one");
    assert_eq!(members.len(), NODES as usize + 1, "{NODES} + the extra node - the deleted hub");
    assert!(members.contains(extra) && !members.contains(hub));
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.membership snapshots built"), 0, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.membership snapshots caught up"), 0, "{:?}", trace.counters());
}
