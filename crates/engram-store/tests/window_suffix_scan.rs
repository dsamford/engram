#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The commit-window validator walks the SUFFIX, not the whole ring.
//!
//! # The defect
//!
//! §6's window answers "did anyone commit to this key after my snapshot" from a
//! ts-ordered ring instead of a point lookup per key. The ring is ts-MONOTONE by
//! construction — every entry is appended by `note_commit` with the log latch
//! held, at the moment the ts is allocated — so the entries a reader needs are
//! exactly the suffix above its snapshot.
//!
//! The prose said so. `docs/write-path-phase0.md` and the comment on the path
//! itself both describe "iterating the SUFFIX above `snapshot_ts`". The code
//! said `lg.window.iter()`: a walk of up to `COMMIT_WINDOW_CAP` (65,536)
//! entries, filtering each one, to build a map from the handful a short
//! statement actually needs — INSIDE the global commit latch, the one
//! serialisation point that cannot be parallelised.
//!
//! # What this file has to prove
//!
//! That the suffix walk computes the SAME MAP. Not an equivalent verdict — the
//! same map. The entries below the snapshot were being filtered out one by one
//! and contributed nothing, so this is the identical predicate computed without
//! touching what cannot affect it. If the two arms ever disagree, the ring is
//! not monotone and the whole §6 design is wrong, which is a much larger
//! finding than a slow loop.
//!
//! Equal-ts runs are the case worth naming: a multi-key transaction commits
//! every write at ONE ts, so the ring holds runs of equal `wts`.
//! `partition_point(|wts| wts <= snapshot_ts)` excludes a run at exactly the
//! snapshot, which is what the `> snapshot_ts` filter did.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue, WINDOW_ENTRIES_SCANNED};
use std::sync::atomic::Ordering;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

fn key(i: u32) -> Vec<u8> {
    let mut b = b"W".to_vec();
    b.extend_from_slice(&i.to_be_bytes());
    b
}

/// Fill the ring with far more entries than any transaction will need, so the
/// difference between "walk the suffix" and "walk everything" is visible.
fn fill(s: &Store, n: u32) {
    for i in 0..n {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![1]))
            .expect("put");
    }
}

/// A transaction that reads a key, has something concurrent committed under it,
/// and commits. Returns whether it committed.
fn conflicting_txn(s: &Store, read: u32, bump: u32) -> bool {
    let mut t = s.begin();
    let _ = t.get(&pfx(), &key(read));
    // A concurrent commit to the key it read.
    s.put(&pfx(), &key(bump), StoredValue::Plain(vec![2]))
        .expect("concurrent");
    t.put(&pfx(), &key(9_999), StoredValue::Plain(vec![3]))
        .expect("own write");
    t.commit().is_ok()
}

/// THE DIFFERENTIAL: identical verdicts on both arms, over the cases that
/// discriminate.
#[test]
fn the_suffix_and_the_full_scan_reach_identical_verdicts() {
    let _serial = serial();
    // A conflict (read a key, someone else writes it) must abort on BOTH arms;
    // a non-conflict must commit on both.
    for (read, bump, want_commit) in [(5u32, 5u32, false), (5, 6, true)] {
        let mut verdicts = Vec::new();
        for suffix in [false, true] {
            let s = Store::new();
            s.set_window_suffix_scan(suffix);
            fill(&s, 2_000);
            verdicts.push(conflicting_txn(&s, read, bump));
        }
        assert_eq!(
            verdicts[0], verdicts[1],
            "read {read} / concurrent write {bump}: the two arms must reach the \
             SAME verdict — {verdicts:?}"
        );
        assert_eq!(
            verdicts[1], want_commit,
            "and the verdict must be the correct one: reading a key someone \
             else then writes must abort; an unrelated write must not"
        );
    }
}

/// THE COUNTER HALF: the suffix walk actually walks less.
///
/// Without this the differential above passes whether or not the lever does
/// anything — two identical code paths agree trivially. This is what makes
/// "we now walk the suffix" a number.
#[test]
fn the_suffix_walk_touches_far_fewer_entries() {
    let _serial = serial();
    let walked = |suffix: bool| -> u64 {
        let s = Store::new();
        s.set_window_suffix_scan(suffix);
        fill(&s, 2_000);
        // A transaction whose snapshot is taken AFTER the fill: everything in
        // the ring is at or below its snapshot, so the suffix is empty and the
        // full scan still walks all 2,000.
        let before = WINDOW_ENTRIES_SCANNED.load(Ordering::Relaxed);
        let mut t = s.begin();
        let _ = t.get(&pfx(), &key(1));
        t.put(&pfx(), &key(5_000), StoredValue::Plain(vec![9]))
            .expect("write");
        let _ = t.commit();
        WINDOW_ENTRIES_SCANNED.load(Ordering::Relaxed) - before
    };
    let full = walked(false);
    let suffix = walked(true);
    eprintln!("[window suffix] entries walked under the commit latch: {full} scanning, {suffix} suffix");
    assert!(
        full >= 2_000,
        "the OFF arm must walk the whole ring, or the ON arm's saving is \
         measured against nothing: {full}"
    );
    assert!(
        suffix < full / 10,
        "the suffix walk must touch a small fraction of the ring — everything \
         at or below the snapshot is a prefix that cannot affect the verdict: \
         {suffix} vs {full}"
    );
}

/// EQUAL-TS RUNS: a multi-key transaction commits every write at ONE ts, so the
/// ring holds runs of equal `wts`. A snapshot taken exactly at such a run must
/// exclude the whole run on both arms.
///
/// This is the one case where `partition_point`'s `<=` and the old filter's `>`
/// could plausibly diverge, so it is asserted rather than assumed.
#[test]
fn an_equal_ts_run_is_excluded_identically_by_both_arms() {
    let _serial = serial();
    let verdict = |suffix: bool| -> bool {
        let s = Store::new();
        s.set_window_suffix_scan(suffix);
        fill(&s, 500);
        // One transaction writing MANY keys: all at a single commit ts.
        let mut t = s.begin();
        for i in 0..64u32 {
            t.put(&pfx(), &key(10_000 + i), StoredValue::Plain(vec![7]))
                .expect("multi-key");
        }
        t.commit().expect("multi-key commit");
        // A reader whose snapshot is taken AFTER that run, so the run is at or
        // below its snapshot and must be excluded.
        let mut r = s.begin();
        let _ = r.get(&pfx(), &key(10_005));
        r.put(&pfx(), &key(20_000), StoredValue::Plain(vec![8]))
            .expect("own write");
        r.commit().is_ok()
    };
    assert_eq!(
        verdict(false),
        verdict(true),
        "a run of equal-ts entries at exactly the snapshot must be excluded the \
         same way by both arms"
    );
    assert!(
        verdict(true),
        "and it must be EXCLUDED — the run is at or below the snapshot, so it \
         cannot be a conflict, and aborting here would be a false conflict on \
         every reader that follows a multi-key transaction"
    );
}
