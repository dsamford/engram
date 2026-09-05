//! Compaction is asked for by TOMBSTONE DENSITY, not only by segment count.
//!
//! A segment count cannot tell a store of live rows from one that is mostly
//! deletions waiting to be reclaimed. Under a create/delete churn the
//! tombstones accumulate and every scan, every prefix walk and every
//! `merge_span` keeps paying for them until the count threshold happens to
//! fire — so a delete-heavy workload gets slower and slower with nothing in the
//! schedule that notices. This is the shape RocksDB's
//! `CompactOnDeletionCollector` and Cassandra's `tombstone_threshold` exist for.
//!
//! The server runs IN-PROCESS so the trigger's counter is readable here.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_server::counters::COMPACTIONS_ASKED_FOR_TOMBSTONES;
use engram_store::Store;

/// The counter is process-wide, so the two arms must not run concurrently.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-tombstone-trigger-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn serve_with(dir: &std::path::Path, cfg: ServerConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let wal = dir.join("engram.wal");
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || {
                let s = Store::open_wal(&wal).expect("open wal");
                (s, Realm(1), Namespace(1))
            },
            cfg,
        );
    });
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("server never came up");
}

/// Create then delete, so the sealed set fills with tombstones.
fn churn(c: &mut Client, rounds: u64) {
    for i in 0..rounds {
        c.run(&format!("CREATE (:Churned {{k: {i}}})")).expect("create");
        c.run(&format!("MATCH (n:Churned {{k: {i}}}) DELETE n"))
            .expect("delete");
    }
}

/// The delete-aware arm asks for compaction; the count-only arm does not.
///
/// Asserted as a PAIR. A test that only showed the trigger firing could not
/// distinguish "the tombstone rule fired" from "the segment count fired and the
/// counter happens to be incremented nearby".
#[test]
fn tombstone_density_asks_for_compaction_and_the_count_only_arm_does_not() {
    let _serial = serial();

    // Count-only: the ratio threshold is 1.0, which nothing can exceed.
    let dir_off = scratch("off");
    let before_off = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed);
    {
        let port = serve_with(
            &dir_off,
            ServerConfig {
                workers: 1,
                // Seal often, so there are many chances to ask.
                seal_after_versions: 64,
                // Never by count, so only the tombstone rule could fire.
                compact_after_segments: usize::MAX,
                tombstone_ratio: 1.0,
                tombstone_min_versions: 16,
                ..ServerConfig::default()
            },
        );
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        churn(&mut c, 300);
    }
    let off = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed) - before_off;
    assert_eq!(
        off, 0,
        "with the ratio threshold at 1.0 the delete-aware rule must never fire \
         — this arm is the control, and if it fires the other arm proves nothing"
    );
    let _ = std::fs::remove_dir_all(&dir_off);

    // Delete-aware: the same workload, a reachable threshold.
    let dir_on = scratch("on");
    let before_on = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed);
    {
        let port = serve_with(
            &dir_on,
            ServerConfig {
                workers: 1,
                seal_after_versions: 64,
                compact_after_segments: usize::MAX,
                tombstone_ratio: 0.2,
                tombstone_min_versions: 16,
                ..ServerConfig::default()
            },
        );
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        churn(&mut c, 300);
    }
    let on = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed) - before_on;
    eprintln!("[tombstone trigger] asks: ratio 0.2 -> {on}, ratio 1.0 -> {off}");
    assert!(
        on > 0,
        "a create/delete churn must cross a 0.2 tombstone ratio and ask for \
         compaction — segment count alone would never have noticed"
    );
    let _ = std::fs::remove_dir_all(&dir_on);
}

/// The floor keeps a tiny store from asking on every seal. Three tombstones in
/// a four-row store is a 75% ratio and means nothing.
#[test]
fn a_tiny_store_does_not_ask_on_every_seal() {
    let _serial = serial();
    let dir = scratch("floor");
    let before = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed);
    {
        let port = serve_with(
            &dir,
            ServerConfig {
                workers: 1,
                seal_after_versions: 8,
                compact_after_segments: usize::MAX,
                tombstone_ratio: 0.2,
                // A floor far above anything this workload can reach.
                tombstone_min_versions: 1_000_000,
                ..ServerConfig::default()
            },
        );
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        churn(&mut c, 60);
    }
    let asked = COMPACTIONS_ASKED_FOR_TOMBSTONES.load(Ordering::Relaxed) - before;
    assert_eq!(
        asked, 0,
        "below the version floor the ratio must not be consulted — a store of \
         four rows, three of them tombstones, is not a compaction candidate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
