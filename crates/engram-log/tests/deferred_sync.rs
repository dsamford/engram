//! Deferred sync — the log half of group commit.
//!
//! `CommitLog::sync` is called by every logged write. With deferral on it must
//! perform NO fsync and instead record that one is owed; `sync_now` must pay
//! that debt with exactly one. The counter, not the clock, is the instrument:
//! `log.fsyncs` fires only on a real `Wal::sync`, so "how many fsyncs did this
//! cost" is a number rather than an inference.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;

use engram_key::{Kind, Namespace, Partition, Realm};
use engram_log::{CommitLog, Op, RoutingHeader, Wal};

fn header(ts: u64) -> RoutingHeader {
    RoutingHeader {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(0),
        op: Op::Put,
        commit_ts: ts,
    }
}

fn tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "engram-deferred-sync-{tag}-{}.wal",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn fsyncs(t: &engram_observe::Trace) -> u64 {
    t.counters().get("log.fsyncs").copied().unwrap_or(0)
}

/// Eight deferred commits cost ZERO fsyncs; paying them costs ONE; and the
/// data is then really on disk.
#[test]
fn deferred_sync_owes_one_fsync_for_many_commits() {
    let path = tmp("deferred");
    let (_, wal) = Wal::open(&path).expect("open wal");
    let mut log = CommitLog::new();
    log.attach_sink(wal);
    log.set_deferred_sync(true);

    let ((), t) = engram_observe::with_trace(|| {
        for i in 0..8u64 {
            log.append(header(i), vec![i as u8]);
            log.sync().expect("a deferred sync cannot fail — it does nothing");
        }
    });
    assert_eq!(
        fsyncs(&t),
        0,
        "with deferral on, 8 commits performed {} fsync(s); every one of them should \
         have been recorded as owed instead",
        fsyncs(&t)
    );
    assert!(log.is_dirty(), "8 deferred syncs must leave an fsync owed");

    let ((), t2) = engram_observe::with_trace(|| {
        log.sync_now().expect("sync_now");
    });
    assert_eq!(fsyncs(&t2), 1, "paying the debt must be exactly one fsync");
    assert!(!log.is_dirty(), "a successful sync_now clears the debt");

    // The promise behind the counter: after `sync_now`, everything is on disk.
    // Reopening the file from nothing and counting is the only proof of that
    // which does not trust the code under test.
    drop(log);
    let (entries, _) = Wal::open(&path).expect("reopen");
    assert_eq!(entries.len(), 8, "all 8 entries must be readable after one fsync");
    let _ = std::fs::remove_file(&path);
}

/// The CONTROL: with deferral off — the default — nothing changed. Eight
/// commits, eight fsyncs, never dirty. This is what every existing caller and
/// the simulation lane see.
#[test]
fn per_commit_fsync_is_unchanged_when_not_deferred() {
    let path = tmp("eager");
    let (_, wal) = Wal::open(&path).expect("open wal");
    let mut log = CommitLog::new();
    log.attach_sink(wal);

    let ((), t) = engram_observe::with_trace(|| {
        for i in 0..8u64 {
            log.append(header(i), vec![i as u8]);
            log.sync().expect("sync");
            assert!(!log.is_dirty(), "an eager sync never leaves a debt");
        }
    });
    assert_eq!(
        fsyncs(&t),
        8,
        "with deferral OFF every commit must still fsync — the default behaviour \
         is the durability contract every existing test was written against"
    );
    let _ = std::fs::remove_file(&path);
}

/// An in-memory log has nothing to fsync, so deferral must not mark it dirty —
/// otherwise a store with no WAL would pay for a `sync_now` on every batch.
#[test]
fn an_in_memory_log_never_owes_anything() {
    let mut log = CommitLog::new();
    log.set_deferred_sync(true);
    log.append(header(1), vec![1]);
    log.sync().expect("sync");
    assert!(!log.is_dirty(), "no sink, nothing owed");
    let ((), t) = engram_observe::with_trace(|| {
        log.sync_now().expect("sync_now on an in-memory log is a no-op");
    });
    assert_eq!(fsyncs(&t), 0);
}
