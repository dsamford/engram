//! §5.5's decision, made measurable — and the cheap alternative it is decided
//! against.
//!
//! # What §5.5 was scheduled to guard against
//!
//! §5.2 emits the derived bases from a compaction, and §5.3 stops the
//! maintenance pass rebuilding them. So the compaction rate IS the refresh
//! rate, and the plan named the risk: *"if Phase 4's tiering makes full
//! compactions rare, the CSR goes stale, the overlay grows and the rebuild
//! returns — which would quietly undo 5.3 and 5.4 together."* The answer on the
//! table was a multi-level CSR (6 days, and it changes `AdjTable::slice`'s
//! arity at every hop site). The plan also fixed the rule in advance, so it
//! could not be re-litigated later:
//!
//! > **if a 30-minute paged SF1 soak shows `ADJ_TABLES_BUILT > 0` or full
//! > compactions at less than one per soak, build 5.5.**
//!
//! # Half the rule is answered by construction
//!
//! "Full compactions at less than one per soak" presupposes a tiering that can
//! choose a PARTIAL merge. The paged compactor cannot: `compact_paged_observed`
//! merges the entire sealed set and has no partial mode, and its own test pins
//! that a compaction leaves exactly one segment. So there is no full:partial
//! ratio to measure — every paged compaction is full, and the risk of a partial
//! merge emitting a partial CSR is structurally absent rather than merely
//! unlikely.
//!
//! What remains is a RATE: does compaction happen often enough. Both existing
//! triggers — segment count and tombstone density — are proportional to write
//! volume, so a store that writes lightly is exactly the store whose bases go
//! unrefreshed. That is what the cadence floor addresses, and it is the "cheap
//! alternative to try before spending the 6 d" the plan named.
//!
//! # What this file does NOT establish
//!
//! Whether the floor is *needed* on a real corpus. That is a 30-minute paged
//! SF1 soak on the bench pod, and no number in this programme has been taken
//! from the pod yet. This file makes the rule's inputs measurable and proves
//! the mechanism works; the decision itself is a pod measurement and is
//! recorded as outstanding rather than assumed either way.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use engram_bolt::client::Client;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_server::counters::{PAGED_COMPACTIONS, PAGED_COMPACTIONS_BY_CADENCE};
use engram_store::Store;

/// The counters are process-wide, so the arms must not run concurrently.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-cadence-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

/// A PAGED server — the mode the cadence governs. A resident store compacts on
/// its own schedule and is not what §5.2 emits from.
fn serve_paged(dir: &std::path::Path, interval: Option<Duration>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let paged = dir.join("paged");
    std::fs::create_dir_all(&paged).expect("paged dir");
    let cache = engram_store::paged::BlockCache::new(8 << 20);
    let cfg = ServerConfig {
        workers: 1,
        // Seal often, so segments accumulate and there is something to merge.
        seal_after_versions: 64,
        // NEVER by count: the count trigger would mask the cadence entirely,
        // and a floor that only fires when another trigger would have fired
        // anyway is a floor that measures nothing.
        compact_after_segments: usize::MAX,
        // NEVER by tombstone density either, for the same reason.
        tombstone_ratio: 1.0,
        maintenance_tick: Duration::from_millis(25),
        compact_max_interval: interval,
        paged_dir: Some(paged),
        paged_spill_cache: Some(cache),
        ..ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || (Store::new(), Realm(1), Namespace(1)),
            cfg,
        );
    });
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("server never came up");
}

fn write_some(c: &mut Client, n: u64) {
    for i in 0..n {
        c.run(&format!("CREATE (:C {{k: {i}}})")).expect("create");
    }
}

/// THE MECHANISM AND ITS CONTROL, as a pair.
///
/// With both volume triggers disabled, a store that keeps writing must still
/// reach a compacted state on the cadence — and must NOT on the arm without
/// one. The control is what makes the first half mean anything: without it,
/// "compaction happened" could be any trigger at all.
#[test]
fn the_cadence_floor_compacts_a_store_neither_volume_trigger_asks_about() {
    let _serial = serial();
    // Earlier tests' servers are never stopped; let a leaked multi-segment store
    // finish its one remaining floor compaction before this window opens.
    std::thread::sleep(Duration::from_millis(1500));

    // ── The arm WITHOUT a floor: no trigger can fire, so nothing compacts.
    let dir_off = scratch("off");
    let before_off = PAGED_COMPACTIONS.load(Ordering::Relaxed);
    {
        let port = serve_paged(&dir_off, None);
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        write_some(&mut c, 400);
        std::thread::sleep(Duration::from_millis(600));
    }
    let off = PAGED_COMPACTIONS.load(Ordering::Relaxed) - before_off;
    assert_eq!(
        off, 0,
        "with the count and density triggers both disabled and no floor, \
         NOTHING may compact — if this fires, the arm below proves nothing \
         about the cadence"
    );
    let _ = std::fs::remove_dir_all(&dir_off);

    // ── The arm WITH a floor: the same workload reaches a compacted state.
    let dir_on = scratch("on");
    let before_on = PAGED_COMPACTIONS.load(Ordering::Relaxed);
    let before_cad = PAGED_COMPACTIONS_BY_CADENCE.load(Ordering::Relaxed);
    {
        let port = serve_paged(&dir_on, Some(Duration::from_millis(1)));
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        write_some(&mut c, 400);
        std::thread::sleep(Duration::from_millis(600));
    }
    let on = PAGED_COMPACTIONS.load(Ordering::Relaxed) - before_on;
    let by_cadence = PAGED_COMPACTIONS_BY_CADENCE.load(Ordering::Relaxed) - before_cad;
    let _ = std::fs::remove_dir_all(&dir_on);

    eprintln!(
        "[cadence] paged compactions: {on} with a floor ({by_cadence} attributed \
         to the cadence), {off} without one"
    );
    assert!(
        on > 0,
        "a store writing steadily with a cadence floor must reach a compacted \
         state even when no volume trigger asks: {on}"
    );
    assert_eq!(
        on, by_cadence,
        "and every one of them must be ATTRIBUTED to the cadence — with both \
         volume triggers disabled, a compaction credited elsewhere means the \
         attribution is wrong and the counter cannot decide the rule"
    );
}

/// The floor does NOT compact a store with one segment.
///
/// A floor that fires regardless would rewrite a single segment to itself on a
/// timer for the life of the process — pure write amplification against a store
/// that is already compacted, and exactly the policy the resident path's
/// count-only scheduling was criticised for.
#[test]
fn the_floor_does_not_rewrite_an_already_compacted_store() {
    let _serial = serial();
    let dir = scratch("single");
    // The servers of earlier tests are never stopped, and since the floor also
    // reaches a store that STOPPED writing, a leaked multi-segment store can
    // compact once more in the background. Let those settle (each ends at one
    // segment and then never fires again) before this window opens.
    std::thread::sleep(Duration::from_millis(1500));
    let before = PAGED_COMPACTIONS.load(Ordering::Relaxed);
    {
        // No writes at all: nothing is ever sealed, so the sealed set stays
        // at or below one segment however long the floor waits.
        let _port = serve_paged(&dir, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(500));
    }
    let n = PAGED_COMPACTIONS.load(Ordering::Relaxed) - before;
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[cadence] compactions on an idle single-segment store: {n}");
    assert_eq!(
        n, 0,
        "the floor must not rewrite a store that is already one segment: {n}"
    );
}

/// The floor reaches a store that NEVER writes.
///
/// The compaction block ran only behind a seal, and a seal needs a non-empty
/// tail: a paged store opened from disk with several segments and only ever
/// READ -- the platform's mirror -- never re-entered it, so `--compact-every`
/// was inert on exactly the store it was added for (13 sealed segments for the
/// life of every process, every prefix walk on the k-way owned-range path;
/// 2026-09-04). The directory is prepared WITHOUT a server so no write ever
/// reaches the served store; the control arm (no floor) proves that nothing
/// else compacts it.
#[test]
fn the_floor_reaches_a_store_that_never_writes() {
    let _serial = serial();
    std::thread::sleep(Duration::from_millis(1500));

    // A multi-segment paged directory, made directly: rows, several seals,
    // spilled to disk, the resident store dropped.
    fn prepare(dir: &std::path::Path) -> usize {
        let paged = dir.join("paged");
        std::fs::create_dir_all(&paged).expect("paged dir");
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        for round in 0..4u64 {
            for i in 0..50u64 {
                let q = engram_cypher::parse_statement(&format!("CREATE (:C {{k: {}}})", round * 100 + i))
                    .expect("parse");
                engram_graph::run_query(&g, &q, Default::default()).expect("create");
            }
            g.shared_store().seal().expect("seal");
        }
        let cache = engram_store::paged::BlockCache::new(8 << 20);
        let n = g.shared_store().spill_sealed_into(&paged, &cache).expect("spill");
        assert!(n > 1, "the fixture must hold several sealed segments, spilled {n}");
        n
    }

    fn serve_readonly(dir: &std::path::Path, interval: Option<Duration>) -> u16 {
        let paged = dir.join("paged");
        let (store, cache) = Store::open_paged_dir(&paged, 8 << 20).expect("open paged dir");
        assert!(store.segment_count() > 1, "reopened with {} segment(s)", store.segment_count());
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let cfg = ServerConfig {
            workers: 1,
            seal_after_versions: 64,
            compact_after_segments: usize::MAX,
            tombstone_ratio: 1.0,
            maintenance_tick: Duration::from_millis(25),
            compact_max_interval: interval,
            paged_dir: Some(paged),
            paged_spill_cache: Some(cache),
            ..ServerConfig::default()
        };
        std::thread::spawn(move || {
            let _ = engram_server::run_server_with_config(
                listener,
                move || (store, Realm(1), Namespace(1)),
                cfg,
            );
        });
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(20));
            if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
                return port;
            }
        }
        panic!("server never came up");
    }

    // Control: no floor, no writes -> nothing may compact.
    let dir_off = scratch("ro-off");
    prepare(&dir_off);
    let before_off = PAGED_COMPACTIONS.load(Ordering::Relaxed);
    let _p = serve_readonly(&dir_off, None);
    std::thread::sleep(Duration::from_millis(800));
    let off = PAGED_COMPACTIONS.load(Ordering::Relaxed) - before_off;
    assert_eq!(off, 0, "a read-only multi-segment store with no floor must not compact ({off})");

    // The floor: the same store, only ever read, compacts on an idle tick.
    let dir_on = scratch("ro-on");
    prepare(&dir_on);
    let before_on = PAGED_COMPACTIONS.load(Ordering::Relaxed);
    let before_cad = PAGED_COMPACTIONS_BY_CADENCE.load(Ordering::Relaxed);
    let _p = serve_readonly(&dir_on, Some(Duration::from_millis(100)));
    std::thread::sleep(Duration::from_millis(1200));
    let on = PAGED_COMPACTIONS.load(Ordering::Relaxed) - before_on;
    let by_cadence = PAGED_COMPACTIONS_BY_CADENCE.load(Ordering::Relaxed) - before_cad;
    eprintln!("[cadence] read-only store: {on} compaction(s) with a floor ({by_cadence} by cadence), {off} without");
    assert!(on > 0, "a read-only multi-segment store with a floor must be compacted on an idle tick: {on}");
    assert_eq!(on, by_cadence, "and attributed to the cadence -- no volume trigger can fire on a store that never writes");
    let _ = std::fs::remove_dir_all(&dir_off);
    let _ = std::fs::remove_dir_all(&dir_on);
}
