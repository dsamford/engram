//! REVIEW PROBE: the per-slot build guard's loser re-checks only
//! `snap.at >= epoch` (lib.rs `adj_table_snapshot_reporting`, after
//! `enter_build`). With the write fence, the winner's publish stamp is
//! CLAMPED below any writer in flight at the moment it published — so while
//! a writer is in flight, the winner publishes below the epoch the loser
//! asked for, the loser judges the freshly built table stale, and REBUILDS
//! it again from a full span walk instead of repairing it from the log (a
//! repair it would have taken had it arrived a moment later, outside the
//! guard). N workers missing at once no longer do one build: they do N,
//! serially, each a whole walk.
//!
//! Two arms: two readers racing to build an unbuilt table, and the
//! maintenance refresh rebuilding a stale table while a reader waits for it
//! — the case `derived_refresh_offpath.rs` says "finds it published".
//!
//! The in-flight writer is a detach-delete of a hub node: its fence is held
//! for the whole detach (lib.rs `delete_node`), which is long enough to
//! order everything deterministically.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier, Mutex};

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Both tests count `ADJ_TABLES_BUILT`, which is PROCESS-GLOBAL, over a
/// window of seconds, and cargo runs them on two threads: the second test's
/// first build landed inside the first test's control window and read as a
/// broken single-flight (`[control] builds=2`). One at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Over `ADJ_REPAIR_MAX` (4,096), so a burst of one `U` write per node is
/// beyond repair by cost (`6,000 + 6,000 × 32 ≥ 6,000`) and must REBUILD.
const NODES: u64 = 6_000;
/// The hub's `T` relationships — the detach-delete's duration.
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

fn built() -> u64 {
    engram_graph::counters::ADJ_TABLES_BUILT.load(Ordering::Relaxed)
}

/// Start the hub's detach-delete on its own thread and wait until it is
/// visibly under way (its fence registered, rows going).
fn start_hub_delete(g: &Arc<Graph>, hub: u64) -> std::thread::JoinHandle<()> {
    let h = {
        let g = Arc::clone(g);
        std::thread::spawn(move || g.delete_node(hub, true).expect("detach delete"))
    };
    let before = g.rels_of(hub, Dir::Out, None).expect("rels").len();
    assert_eq!(before, HUB as usize);
    loop {
        let now = g.rels_of(hub, Dir::Out, None).expect("rels").len();
        if now < before {
            break;
        }
        std::thread::yield_now();
    }
    h
}

fn readers_race(g: &Arc<Graph>, ids: &[u64], u: u32, n: usize) -> (u64, u64, u64) {
    let barrier = Arc::new(Barrier::new(n));
    let before = built();
    let hs: Vec<_> = (0..n)
        .map(|i| {
            let (g, barrier, node) = (Arc::clone(g), Arc::clone(&barrier), ids[i]);
            std::thread::spawn(move || {
                barrier.wait();
                let (_, trace) = engram_observe::with_trace(|| g.adjacent_slim(node, Dir::Out, &Some(vec![u])).len());
                let c = trace.counters();
                (
                    c.get("graph.adjacency tables built by another worker").copied().unwrap_or(0),
                    c.get("graph.adjacency tables repaired").copied().unwrap_or(0),
                )
            })
        })
        .collect();
    let mut by_other = 0;
    let mut repaired = 0;
    for h in hs {
        let (o, r) = h.join().expect("reader");
        by_other += o;
        repaired += r;
    }
    (built() - before, by_other, repaired)
}

#[test]
fn with_a_writer_in_flight_every_loser_behind_the_build_guard_rebuilds_again() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // CONTROL: no writer in flight — N readers missing at once do ONE build.
    {
        let (g, ids, _hub, u) = fixture();
        let (builds, by_other, repaired) = readers_race(&g, &ids, u, 4);
        eprintln!("[control] builds={builds} by_other={by_other} repaired={repaired}");
        assert!(builds >= 1);
        assert_eq!(
            builds, 1,
            "control: {builds} builds for one table with no writer in flight — single-flight is broken outright"
        );
    }
    // With the hub's detach-delete in flight (fence registered at `r`), one
    // `U` write after it (epoch(U) > r), then N readers race to build U's table.
    let (g, ids, hub, u) = fixture();
    let none = BTreeMap::new();
    let del = start_hub_delete(&g, hub);
    g.create_rel(ids[1], "U", ids[2], &none).expect("U after the fence registered");
    let (builds, by_other, repaired) = readers_race(&g, &ids, u, 4);
    let still_running = !g.rels_of(hub, Dir::Out, None).expect("rels").is_empty();
    del.join().expect("delete");
    eprintln!("[fenced] builds={builds} by_other={by_other} repaired={repaired} writer_still_in_flight={still_running}");
    assert!(still_running, "the fixture is too small: the delete finished before the readers raced");
    assert_eq!(
        builds, 1,
        "{builds} FULL REBUILDS of one table by 4 readers missing at once while a writer was in flight \
         (by_other={by_other}, repaired={repaired}): the winner published below the epoch (fenced), and \
         every loser behind the guard re-checks `snap.at >= epoch` only, so it walks the whole span again \
         instead of repairing from the log"
    );
}

#[test]
fn a_reader_waiting_on_the_refresh_rebuild_rebuilds_again_when_the_publish_was_fenced() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (g, ids, hub, u) = fixture();
    // This test is about what happens when the REFRESH'S REBUILD publishes
    // under a fence. §5.3 defaults that rebuild off — the mechanism is gated,
    // not deleted — so the arm that still has one is the arm to test it on.
    g.set_demote_adjacency_rebuild(false);
    let none = BTreeMap::new();
    // U's table exists, then a burst over the cap makes it stale beyond
    // repair (6,000 changed nodes > 4,096; work ≥ the 6,000-entry table).
    assert_eq!(g.adjacent_slim(ids[0], Dir::Out, &Some(vec![u])).len(), 1);
    for &src in &ids[..NODES as usize] {
        g.create_rel(src, "U", ids[0], &none).expect("burst");
    }
    let del = start_hub_delete(&g, hub);
    g.create_rel(ids[3], "U", ids[4], &none).expect("U after the fence registered");
    let before = built();
    let barrier = Arc::new(Barrier::new(2));
    let counter = |t: &engram_observe::Trace, k: &str| t.counters().get(k).copied().unwrap_or(0);
    let refresh = {
        let (g, barrier) = (Arc::clone(&g), Arc::clone(&barrier));
        std::thread::spawn(move || {
            barrier.wait();
            let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
            (
                report,
                counter(&trace, "graph.adjacency tables built"),
                counter(&trace, "graph.adjacency tables repaired"),
            )
        })
    };
    let reader = {
        let (g, barrier, node) = (Arc::clone(&g), Arc::clone(&barrier), ids[5]);
        std::thread::spawn(move || {
            barrier.wait();
            let (rows, trace) = engram_observe::with_trace(|| g.adjacent_slim(node, Dir::Out, &Some(vec![u])).len());
            (
                rows,
                counter(&trace, "graph.adjacency tables built"),
                counter(&trace, "graph.adjacency tables repaired"),
            )
        })
    };
    let (report, refresh_built, refresh_repaired) = refresh.join().expect("refresh");
    let (rows, reader_built, reader_repaired) = reader.join().expect("reader");
    let builds = built() - before;
    let still_running = !g.rels_of(hub, Dir::Out, None).expect("rels").is_empty();
    del.join().expect("delete");
    eprintln!(
        "[refresh+reader] report={report:?} builds={builds} refresh_built={refresh_built} \
         refresh_repaired={refresh_repaired} reader_built={reader_built} reader_repaired={reader_repaired} \
         rows={rows} writer_still_in_flight={still_running}"
    );
    assert!(still_running, "the fixture is too small: the delete finished first");
    // WHICH of the two wins the build guard is timing (under a loaded host
    // the reader has won it); the invariant is the same either way. Exactly
    // one span walk happened, by whichever won...
    assert_eq!(
        builds, 1,
        "{builds} rebuilds of U's table: one of them rebuilt it (fenced publish) and the one that waited \
         behind the guard rebuilt it AGAIN (refresh_built={refresh_built}, reader_built={reader_built}) — \
         the winner's rebuild was not taken off the loser's path, it was doubled onto it"
    );
    assert_eq!(
        refresh_built + reader_built,
        1,
        "the one build must be the refresh's or the reader's, not both: refresh={refresh_built} reader={reader_built}"
    );
    assert_eq!(
        report.adjacency_rebuilt as u64, refresh_built,
        "the refresh's report must say whether IT walked the span: {report:?}"
    );
    // ...the loser was brought current from the log (the winner's fenced
    // publish is below the epoch, so "not current" is repaired, never
    // walked again)...
    assert!(
        refresh_repaired + reader_repaired >= 1,
        "the loser never repaired the winner's fenced snapshot from the log \
         (refresh_repaired={refresh_repaired}, reader_repaired={reader_repaired}): {report:?}"
    );
    // ...and the reader saw the acknowledged rows: ids[5]'s original `U`
    // and the burst's — the post-fence write is on ids[3], not here.
    assert_eq!(rows, 2, "the reader must observe every acknowledged U row of ids[5]");
}
