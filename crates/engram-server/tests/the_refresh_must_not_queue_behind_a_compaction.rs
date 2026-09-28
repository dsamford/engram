#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Fix 78's differential: the derived refresh must not wait for the storage
//! pass.
//!
//! # What was wrong, and how it hid
//!
//! The maintenance thread owned two jobs whose costs are not comparable. The
//! derived refresh is O(the delta) and exists to do inline what the next
//! reader would otherwise pay for. The storage work beside it — spill, and for
//! a paged store a full `compact_paged_emitting` — is O(CORPUS);
//! `adopt_merged_derived`'s own comment reads *"the merge runs for minutes"*.
//! They shared one loop body with the refresh at the tail, so the refresh
//! could not start until the storage pass returned.
//!
//! For a paged store that is not a rare coincidence. A worker asks for storage
//! after EVERY batch (`if paged || segment_count() >= compact_after || …` —
//! the first disjunct is unconditional), so a store past `compact_after`
//! segments compacts, and the refresh waits for it.
//!
//! It hid because a compaction that retires nothing logs nothing, and because
//! the shape of the starvation is invisible in a long run: on the bench pod a
//! 400 s sweep took one compaction at the front and then ran the pass at its
//! tick for the remaining 380 s, which looks healthy. A 70 s window taken
//! inside that first compaction reported `refresh_runs` frozen at 20 while
//! `adj_repaired` climbed 58 -> 2,282. Two five-arm budget sweeps measured a
//! thread that was not running before a probe sampled `*.seg` against the
//! counter and caught segments collapsing 100 -> 1 at the exact sample the
//! first `derived refresh in … ms` line appeared.
//!
//! # What this file measures
//!
//! One paged server, a corpus big enough that a compaction is not free, and a
//! write stream that keeps asking for both jobs at once. The refresh ask is
//! what drives the pass in both arms — see the tick note below — so the count
//! of completed passes IS the answer to "did the refresh have to wait".
//!
//! ## The tick is deliberately LONG, and that is not a detail
//!
//! `run_server_with_config` has no shutdown, so every arm's server leaks and
//! keeps ticking through the arms that follow, and
//! `MAINTENANCE_REFRESH_RUNS` is a process-wide atomic. With the 1–25 ms tick
//! these suites normally use, a leaked server contributes tens of increments
//! per second to the NEXT arm's delta and the measurement is of the leak. A
//! two-second tick makes an idle leaked server contribute about one, and
//! leaves the ASK as the thing being measured.
//!
//! The arms also run split-ON first. A leak then inflates the OFF arm, which
//! is the direction that makes the assertion harder rather than easier.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_server::counters::MAINTENANCE_REFRESH_RUNS;
use engram_store::Store;

/// Nodes in the corpus a compaction has to merge. Large enough that one pass
/// is worth tens of milliseconds — the whole question is whether the refresh
/// waits for it, and a compaction that costs nothing cannot make anything
/// wait.
const CORPUS: u64 = 60_000;
/// Persons the write stream attaches messages to, so there is a HAS_CREATOR
/// adjacency table to go stale and be refreshed.
const PERSONS: u64 = 64;
/// The measurement window. Both arms get the same wall clock and the same
/// writer, so the only difference between them is where the pass runs.
const WINDOW: Duration = Duration::from_secs(4);

/// The counters are process-wide, so the arms must not run concurrently.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    // The pid alone is shared by every test in this binary; the tag is what
    // keeps two arms from opening the same directory.
    p.push(format!("engram-split-maint-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn serve(dir: &std::path::Path, split: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let paged = dir.join("paged");
    std::fs::create_dir_all(&paged).expect("paged dir");
    let cache = engram_store::paged::BlockCache::new(64 << 20);
    let cfg = ServerConfig {
        workers: 2,
        derived_refresh: true,
        // The ASK is what drives the pass here, not the tick — see the module
        // doc. Small enough that the write stream asks continuously.
        refresh_after_writes: 32,
        maintenance_tick: Duration::from_secs(2),
        // Seal often and compact at two segments, so the storage half of the
        // loop has real work to do for the whole window rather than once.
        seal_after_versions: 1024,
        compact_after_segments: 2,
        paged_dir: Some(paged),
        paged_spill_cache: Some(cache),
        split_maintenance: split,
        configure_graph: Some(Arc::new(|g: &engram_graph::Graph| {
            // Admit the adjacency table immediately: an unbuilt table is never
            // stale and would leave the pass with nothing to do.
            g.set_degree_table_after(0);
        })),
        ..ServerConfig::default()
    };
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || (Store::new(), Realm(1), Namespace(1)),
            cfg,
        );
    });
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("server never came up");
}

/// Build the corpus in a handful of statements rather than a round trip per
/// row: the point of the corpus is its SIZE, and paying Bolt latency for it
/// would make the fixture longer than the measurement.
fn load(c: &mut Client) {
    c.run(&format!(
        "UNWIND range(0, {}) AS i CREATE (:Filler {{k: i}})",
        CORPUS - 1
    ))
    .expect("filler");
    c.run(&format!(
        "UNWIND range(0, {}) AS i CREATE (:Person {{id: i}})",
        PERSONS - 1
    ))
    .expect("persons");
    // One message each, so the table exists before the window opens.
    c.run(&format!(
        "UNWIND range(0, {}) AS i MATCH (p:Person {{id: i}}) \
         CREATE (:Message {{id: i}})-[:HAS_CREATOR]->(p)",
        PERSONS - 1
    ))
    .expect("seed messages");
    // A read that USES the table, so it is cached and therefore a candidate
    // for the pass. `refresh_stale_derived` refreshes what readers have shown
    // they use and never warms.
    for p in 0..PERSONS {
        c.run(&format!(
            "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {{id: {p}}}) RETURN count(m)"
        ))
        .expect("warm the table");
    }
}

/// Writes for `WINDOW`, each one making the HAS_CREATOR table stale, and
/// returns how many refresh passes COMPLETED while it ran.
fn measure(port: u16) -> (u64, u64) {
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    let before = MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed);
    let start = Instant::now();
    let mut writes = 0u64;
    while start.elapsed() < WINDOW {
        let p = writes % PERSONS;
        c.run(&format!(
            "MATCH (p:Person {{id: {p}}}) \
             CREATE (:Message {{id: {}}})-[:HAS_CREATOR]->(p)",
            1_000_000 + writes
        ))
        .expect("write");
        writes += 1;
    }
    (MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed) - before, writes)
}

/// THE MECHANISM AND ITS CONTROL.
///
/// Same corpus, same writer, same window, same compaction pressure — the only
/// difference is whether the pass runs on its own thread. If the split is
/// doing nothing, the two counts are the same and this fails.
#[test]
fn the_derived_refresh_keeps_its_cadence_while_the_storage_thread_compacts() {
    let _serial = serial();

    // ── Split ON: the refresh has its own thread and its own ask channel.
    let dir_on = scratch("on");
    let on = {
        let port = serve(&dir_on, true);
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        load(&mut c);
        measure(port)
    };

    // ── Split OFF: the refresh is the last statement of the storage loop, so
    // it runs once per loop iteration and every iteration compacts.
    let dir_off = scratch("off");
    let off = {
        let port = serve(&dir_off, false);
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        load(&mut c);
        measure(port)
    };

    // NEITHER DIRECTORY IS REMOVED HERE, deliberately. The servers leak (there
    // is no shutdown) and go on spilling and compacting into them; deleting a
    // live server's paged directory makes it print
    //   "spill FAILED: The system cannot find the path specified"
    // and "paged compaction failed", which reads as a fault in the code under
    // test and is only the test tidying up underneath it. `scratch` clears the
    // directory when it is next created, so the next run starts clean.

    eprintln!(
        "[split-maint] refresh passes in {:?}: ON {} (over {} writes), \
         OFF {} (over {} writes)",
        WINDOW, on.0, on.1, off.0, off.1
    );

    assert!(
        off.1 > 0 && on.1 > 0,
        "both arms must have written something for the counts to mean \
         anything: ON {} writes, OFF {} writes",
        on.1,
        off.1
    );
    assert!(
        on.0 > 0,
        "with the split ON the refresh must run: {} passes over {} writes at \
         one ask per 32 stamps — zero means the ask is not reaching the \
         refresh thread at all",
        on.0,
        on.1
    );
    // THE THRESHOLD IS A MEASUREMENT, NOT A GUESS. The OFF arm's rate is
    // bounded by one pass per storage loop iteration and every iteration
    // compacts a 60k-node store (the run this was set from: `paged-compacted
    // in 293–348 ms` each); the ON arm's is bounded only by the ask rate.
    // Observed on this hardware: ON 181 passes against OFF 13 over the same
    // 4 s and comparable write counts — 13.9x. The floor is 4x, which leaves
    // room for a slower machine without letting a regression through: a slower
    // box makes compaction slower, which lowers OFF and RAISES the ratio, so
    // the observed margin is not the fragile direction.
    //
    // A leaked ON-arm server also keeps ticking through the OFF window and
    // inflates OFF, which makes this conservative rather than optimistic.
    assert!(
        on.0 >= off.0 * 4,
        "the split must let the refresh keep a cadence the storage thread \
         cannot: ON {} passes against OFF {} — under 4x means the pass is \
         still gated by the storage loop, or the corpus is too small for a \
         compaction to make anything wait",
        on.0,
        off.0
    );
}
