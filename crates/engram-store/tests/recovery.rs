#![allow(non_snake_case)]
//! Recovery: the store rebuilt from its own commit log, and crashes injected
//! at the WAL boundaries.
//!
//! R14's fault catalogue names "kill at every WAL boundary" — here that is
//! `with_crash_at` firing inside the write path, then recovery from the log
//! the crash left behind. The invariant at every boundary is the WAL rule's
//! whole content: **the log is always a superset of the memtable**, so
//! recovery is a pure redo and an acknowledged write is never lost.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::with_crash_at;
use engram_store::{RecoverError, Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn p2() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(2),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(3),
    }
}

// ─── The round trip ─────────────────────────────────────────────────────────

#[test]
fn a_recovered_store_answers_every_read_the_original_did() {
    let s = Store::new();
    let t1 = s
        .put(&prefix(), b"a", StoredValue::Plain(vec![1]))
        .expect("a1");
    s.put(&prefix(), b"a", StoredValue::Plain(vec![2]))
        .expect("a2");
    s.put(&p2(), b"b", StoredValue::Plain(vec![3])).expect("b");
    s.delete(&prefix(), b"gone-after");
    s.put(&prefix(), b"gone-after", StoredValue::Plain(vec![4]))
        .expect("resurrect");
    s.delete(&p2(), b"b");

    let r = Store::recover(&s.log_tail(0)).expect("recovers");

    // Current reads.
    assert_eq!(r.get(&prefix(), b"a"), s.get(&prefix(), b"a"));
    assert_eq!(r.get(&prefix(), b"gone-after"), Some(vec![4]));
    assert_eq!(r.get(&p2(), b"b"), None);
    // SNAPSHOT reads — history replayed with the ORIGINAL timestamps. A
    // recovery that reassigned them would give the replica a different past
    // than the primary, and every snapshot read would disagree across the pair.
    assert_eq!(r.get_at(&prefix(), b"a", t1), Some(vec![1]));
    // And the recovered store's own log carries the same chain.
    assert_eq!(
        r.log_head(),
        s.log_head(),
        "recovery must reproduce the chain, not re-author it"
    );
}

#[test]
fn recovery_is_a_fixed_point() {
    // recover(recover(log)) == recover(log). If a recovery drifts — reordered
    // versions, reassigned ts — the drift compounds per generation and shows
    // up here as diverging heads.
    let s = Store::new();
    for i in 0..10u8 {
        s.put(&prefix(), &[i % 3], StoredValue::Plain(vec![i]))
            .expect("w");
    }
    let once = Store::recover(&s.log_tail(0)).expect("first");
    let twice = Store::recover(&once.log_tail(0)).expect("second");
    assert_eq!(once.log_head(), twice.log_head());
}

#[test]
fn a_sealed_store_recovers_identically() {
    // The log spans seals — a seal is a memory event and the log never heard
    // of it. Recovery lands everything in the tail, and reads must not care.
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    s.seal().expect("seal");
    s.put(&prefix(), b"k", StoredValue::Plain(vec![2]))
        .expect("v2");

    let r = Store::recover(&s.log_tail(0)).expect("recovers");
    assert_eq!(r.get(&prefix(), b"k"), Some(vec![2]));
    assert_eq!(
        r.segment_count(),
        0,
        "segments are layout, not content — none reappear"
    );
}

#[test]
fn a_protected_value_recovers_still_sealed() {
    let s = Store::new();
    let pp = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::PROTECTED_PROPERTY,
        partition: Partition(1),
    };
    s.put(&pp, b"k", StoredValue::Sealed(vec![9]))
        .expect("sealed put");

    let r = Store::recover(&s.log_tail(0)).expect("recovers");
    assert_eq!(
        r.is_sealed(&pp, b"k"),
        Some(true),
        "recovery must not launder ciphertext into plaintext"
    );
}

// ─── Refusals ───────────────────────────────────────────────────────────────

#[test]
fn recovery_REFUSES_a_broken_chain_outright() {
    // Replaying up to a break would silently restore a prefix and report it as
    // the database — the absent-read-as-good defect at the worst possible
    // layer. A partial recovery that says so beats a complete-looking one.
    let s = Store::new();
    s.put(&prefix(), b"a", StoredValue::Plain(vec![1]))
        .expect("a");
    s.put(&prefix(), b"b", StoredValue::Plain(vec![2]))
        .expect("b");

    let mut entries = s.log_tail(0);
    entries[0].payload[5] ^= 1;
    assert!(matches!(
        Store::recover(&entries),
        Err(RecoverError::BrokenChain { seq: 0 })
    ));
}

#[test]
fn recovery_refuses_a_gapped_log() {
    let s = Store::new();
    for i in 0..3u8 {
        s.put(&prefix(), &[i], StoredValue::Plain(vec![i]))
            .expect("w");
    }
    let mut entries = s.log_tail(0);
    entries.remove(1);
    assert!(matches!(
        Store::recover(&entries),
        Err(RecoverError::SequenceGap {
            expected: 1,
            found: 2
        })
    ));
}

// ─── Crashes at the WAL boundaries ──────────────────────────────────────────

#[test]
fn crash_BEFORE_the_log_append_loses_the_write_cleanly() {
    // The un-acknowledged side of the rule: a crash before the append means
    // the write never happened anywhere. Nothing to redo, nothing half-done.
    let s = Store::new();
    s.put(&prefix(), b"pre", StoredValue::Plain(vec![1]))
        .expect("pre");

    let crashed = with_crash_at("store.before_log_append", || {
        s.put(&prefix(), b"lost", StoredValue::Plain(vec![9]))
            .expect("never returns");
    });
    assert!(crashed.is_err(), "the crash point must fire");

    let r = Store::recover(&s.log_tail(0)).expect("recovers");
    assert_eq!(r.get(&prefix(), b"pre"), Some(vec![1]));
    assert_eq!(
        r.get(&prefix(), b"lost"),
        None,
        "an unlogged write must not survive"
    );
    // And the surviving store agrees — the crash left no half-published state.
    assert_eq!(s.get(&prefix(), b"lost"), None);
}

#[test]
fn crash_BETWEEN_log_and_publish_is_REDONE_by_recovery() {
    // The boundary the ordering exists for. The entry is durable, the memtable
    // never saw it: the pre-crash store under-reports, and recovery REDOES the
    // write from the log. Publish-then-log at this boundary would instead lose
    // an acknowledged write — the unrecoverable direction.
    let s = Store::new();
    s.put(&prefix(), b"pre", StoredValue::Plain(vec![1]))
        .expect("pre");

    let crashed = with_crash_at("store.between_log_and_publish", || {
        s.put(&prefix(), b"redo-me", StoredValue::Plain(vec![7]))
            .expect("never returns");
    });
    assert!(crashed.is_err(), "the crash point must fire");

    // The crashed process's memtable never published it…
    assert_eq!(s.get(&prefix(), b"redo-me"), None);
    // …but the log has it, and recovery replays it.
    let r = Store::recover(&s.log_tail(0)).expect("recovers");
    assert_eq!(
        r.get(&prefix(), b"redo-me"),
        Some(vec![7]),
        "the logged write must be redone"
    );
    assert_eq!(r.get(&prefix(), b"pre"), Some(vec![1]));
}

#[test]
fn crash_on_DELETE_at_the_same_boundary_redoes_the_tombstone() {
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v");

    let crashed = with_crash_at("store.between_log_and_publish", || {
        s.delete(&prefix(), b"k");
    });
    assert!(crashed.is_err());

    assert_eq!(
        s.get(&prefix(), b"k"),
        Some(vec![1]),
        "the crashed process never published the tombstone"
    );
    let r = Store::recover(&s.log_tail(0)).expect("recovers");
    assert_eq!(
        r.get(&prefix(), b"k"),
        None,
        "the logged delete must be redone"
    );
}

#[test]
fn an_armed_point_that_is_never_reached_is_a_FINDING() {
    // A schedule that never reaches the boundary tested nothing — and Ok is
    // how with_crash_at says so. This pins that the harness distinguishes
    // "survived the crash" from "never crashed", which print identically in a
    // harness that only checks the end state.
    let out = with_crash_at("store.between_log_and_publish", || 42);
    assert_eq!(
        out.ok(),
        Some(42),
        "no write ran, so the point must not fire"
    );
}

#[test]
fn a_real_panic_is_NOT_swallowed_as_an_injected_crash() {
    // A genuine bug must fail the test as itself. A harness that catches every
    // unwind converts assertion failures into "recovered cleanly".
    let result = std::panic::catch_unwind(|| {
        let _ = with_crash_at("store.before_log_append", || {
            panic!("a genuine bug");
        });
    });
    assert!(
        result.is_err(),
        "the real panic must propagate through the harness"
    );
}

#[test]
fn recovery_takes_timestamps_FROM_THE_HEADER_not_from_a_counter() {
    // Today the store's own log always has commit_ts == seq + 1, so a recovery
    // that re-counted timestamps would produce identical results and a canary
    // against it could not fire — measured, not assumed. But the FORMAT
    // supports gaps (group commit will assign one ts to N entries), so this
    // test constructs that future log by hand: timestamps 5 and 9, nothing in
    // between. A re-counting recovery collapses them to 1 and 2, and every
    // snapshot read below disagrees.
    use engram_log::{CommitLog, Op, RoutingHeader};
    let mut log = CommitLog::new();
    let hdr = |ts: u64| RoutingHeader {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
        op: Op::Put,
        commit_ts: ts,
    };
    // payload = body_len:u32 BE | body | value
    let payload = |body: &[u8], value: &[u8]| {
        let mut p = (body.len() as u32).to_be_bytes().to_vec();
        p.extend_from_slice(body);
        p.extend_from_slice(value);
        p
    };
    log.append(hdr(5), payload(b"k", &[1]));
    log.append(hdr(9), payload(b"k", &[2]));

    let r = Store::recover(log.tail(0)).expect("recovers");
    assert_eq!(r.get_at(&prefix(), b"k", 4), None, "before the first write");
    assert_eq!(r.get_at(&prefix(), b"k", 5), Some(vec![1]));
    assert_eq!(
        r.get_at(&prefix(), b"k", 8),
        Some(vec![1]),
        "the gap belongs to v1"
    );
    assert_eq!(r.get_at(&prefix(), b"k", 9), Some(vec![2]));
    // And the clock resumes PAST the highest replayed ts, so the next write
    // cannot mint a timestamp history already used.
    let next = r
        .put(&prefix(), b"k", StoredValue::Plain(vec![3]))
        .expect("post-recovery write");
    assert!(
        next > 9,
        "the recovered clock must clear the replayed history, got {next}"
    );
}

// ─── Durable WAL (open_wal): the log outlives the process ────────────────────
//
// `Store::recover` above rebuilds from an in-memory entry stream — it proves
// the REDO is correct but assumes the log is still in RAM. `open_wal` closes
// that assumption: the log is a file, `fsync`'d before each write is
// acknowledged, so a store dropped (a clean stand-in for `kill -9`) and
// reopened answers every committed read. The crash-safety invariant is that a
// TORN tail — a commit that began but was never fully written and `fsync`'d —
// is discarded on reopen, while every earlier `fsync`'d record survives.

/// A temp WAL path that removes itself. No `tempfile` dev-dep, so the name is
/// made unique by pid + a process-local counter.
struct TmpWal(std::path::PathBuf);

impl TmpWal {
    fn new(tag: &str) -> TmpWal {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-wal-{}-{}-{}.log",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&p); // a prior run must not leak in
        TmpWal(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpWal {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_wal_backed_store_reopens_with_every_committed_write() {
    let tmp = TmpWal::new("roundtrip");
    {
        let s = Store::open_wal(tmp.path()).expect("open empty wal");
        s.put(&prefix(), b"a", StoredValue::Plain(vec![1]))
            .expect("a1");
        s.put(&prefix(), b"a", StoredValue::Plain(vec![2]))
            .expect("a2");
        s.put(&p2(), b"b", StoredValue::Plain(vec![3])).expect("b");
        s.delete(&p2(), b"b");
        // `s` dropped here: the in-memory memtable is gone — only the `fsync`'d
        // file remains. A store that recovered from RAM would prove nothing.
    }
    let r = Store::open_wal(tmp.path()).expect("reopen");
    assert_eq!(r.get(&prefix(), b"a"), Some(vec![2]), "last write wins");
    assert_eq!(r.get(&p2(), b"b"), None, "the delete is durable too");

    // The reopened store appends to the SAME file, AFTER the recovered tail.
    r.put(&prefix(), b"c", StoredValue::Plain(vec![4]))
        .expect("post-recovery write");
    drop(r);
    let again = Store::open_wal(tmp.path()).expect("reopen 2");
    assert_eq!(
        again.get(&prefix(), b"c"),
        Some(vec![4]),
        "an append after recovery persists"
    );
    assert_eq!(
        again.get(&prefix(), b"a"),
        Some(vec![2]),
        "and the original history is still there"
    );
}

#[test]
fn a_torn_tail_record_is_discarded_and_the_prefix_survives() {
    let tmp = TmpWal::new("torn");
    {
        let s = Store::open_wal(tmp.path()).expect("open");
        s.put(&prefix(), b"k1", StoredValue::Plain(vec![1]))
            .expect("k1");
        s.put(&prefix(), b"k2", StoredValue::Plain(vec![2]))
            .expect("k2");
    }
    // Simulate a crash mid-append: a partial record's bytes reached the file
    // but the write never completed or `fsync`'d. These are torn tail bytes.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path())
            .expect("open for append");
        f.write_all(&[0xAB; 40]).expect("write torn tail");
        f.sync_all().expect("sync torn");
    }
    let r = Store::open_wal(tmp.path()).expect("reopen tolerates a torn tail");
    assert_eq!(r.get(&prefix(), b"k1"), Some(vec![1]));
    assert_eq!(
        r.get(&prefix(), b"k2"),
        Some(vec![2]),
        "every fsync'd record survives the torn tail"
    );
    // The torn bytes were truncated on open, so a fresh append chains cleanly
    // onto k2 rather than after garbage — proven by a THIRD reopen SEEING it.
    r.put(&prefix(), b"k3", StoredValue::Plain(vec![3]))
        .expect("append after truncation");
    drop(r);
    let again = Store::open_wal(tmp.path()).expect("reopen after clean append");
    assert_eq!(
        again.get(&prefix(), b"k3"),
        Some(vec![3]),
        "the post-truncation append is itself durable and recoverable"
    );
    assert_eq!(
        again.log_len(),
        3,
        "exactly three good records — the torn tail left no trace"
    );
}

#[test]
fn corruption_mid_log_keeps_the_longest_valid_prefix() {
    // Bit-rot (or a torn write) that lands in the MIDDLE of the file, not at the
    // very end, must not silently drop the corrupt record and splice the good
    // suffix back on — that would report a database with a hole in its history.
    // The rule is "recover the longest valid prefix": everything from the first
    // record that fails its own chain hash onward is dropped, the clean prefix
    // before it survives, and the file is truncated to that prefix.
    let tmp = TmpWal::new("midrot");
    {
        let s = Store::open_wal(tmp.path()).expect("open");
        s.put(&prefix(), b"k1", StoredValue::Plain(vec![1]))
            .expect("k1");
        s.put(&prefix(), b"k2", StoredValue::Plain(vec![2]))
            .expect("k2");
        s.put(&prefix(), b"k3", StoredValue::Plain(vec![3]))
            .expect("k3");
    }
    // Each record here is 73 bytes: 8 (seq) + 22 (header) + 4 (payload len) + 7
    // (payload = 4-byte body-len | 2-byte body | 1-byte value) + 32 (hash).
    // Records begin AFTER the file header, so every offset below is measured
    // from `WAL_HEADER_LEN` — without that the arithmetic lands inside record
    // ONE and this test would assert that a corrupt first record leaves a
    // surviving prefix, which is the opposite of the rule.
    // Flip a payload byte of the SECOND record; its stored hash no longer
    // matches, so replay stops there.
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path())
            .expect("open rw");
        let corrupt_at = engram_log::WAL_HEADER_LEN + 73 + 8 + 22 + 4; // rec 2's first payload byte
        f.seek(SeekFrom::Start(corrupt_at as u64)).expect("seek");
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).expect("read");
        byte[0] ^= 0xFF;
        f.seek(SeekFrom::Start(corrupt_at as u64))
            .expect("seek back");
        f.write_all(&byte).expect("write");
        f.sync_all().expect("sync");
    }
    let r = Store::open_wal(tmp.path()).expect("reopen recovers the clean prefix");
    assert_eq!(
        r.get(&prefix(), b"k1"),
        Some(vec![1]),
        "the prefix survives"
    );
    assert_eq!(
        r.get(&prefix(), b"k2"),
        None,
        "the corrupt record is dropped, not repaired"
    );
    assert_eq!(
        r.get(&prefix(), b"k3"),
        None,
        "and nothing AFTER the break is spliced back on"
    );
    assert_eq!(
        r.log_len(),
        1,
        "exactly the one clean record before the break"
    );
}

/// Releasing the in-memory commit log does not weaken WAL durability.
///
/// `CommitLog::entries` was retained for the process lifetime and never
/// truncated by the server — at ~150 B per version that is the term which put
/// a paged SF1 load at ~17 GB. The server now releases it at a seal, and this
/// is the argument for why that is sound, executed rather than asserted:
/// `append_prehashed` writes each record to the durable sink BEFORE pushing it
/// to the vector, and `open_wal` recovers from the FILE, so the vector is pure
/// in-memory retention.
#[test]
fn truncating_the_in_memory_log_does_not_weaken_wal_durability() {
    let tmp = TmpWal::new("truncate");
    let (len_before, head_before) = {
        let s = Store::open_wal(tmp.path()).expect("open empty wal");
        for i in 0..64u8 {
            s.put(&prefix(), &[i], StoredValue::Plain(vec![i]))
                .expect("put");
        }
        s.seal();

        let upto = s.log_len();
        let head_before = s.log_head();
        let dropped = s.truncate_log_below(upto);
        // Non-vacuity: a truncation that dropped nothing would let every
        // assertion below pass while proving nothing at all.
        assert!(
            dropped > 0,
            "the truncation must actually release entries, or this test is vacuous"
        );
        assert!(
            s.log_tail(0).is_empty(),
            "the in-memory vector is what gets released"
        );
        // The sequence allocator and the chain must be unaffected: `len` counts
        // truncated entries, and the dropped prefix's hash is carried forward.
        assert_eq!(s.log_len(), upto, "log_len must survive truncation");
        assert_eq!(s.log_head(), head_before, "the chain head must not move");

        // A write AFTER the release must still reach the file.
        s.put(&prefix(), b"after", StoredValue::Plain(vec![99]))
            .expect("post-truncation write");
        (s.log_len(), s.log_head())
        // `s` dropped: only the fsync'd file remains.
    };
    assert!(len_before > 0 && head_before != [0u8; 32]);

    let r = Store::open_wal(tmp.path()).expect("reopen");
    for i in 0..64u8 {
        assert_eq!(
            r.get(&prefix(), &[i]),
            Some(vec![i]),
            "entry {i} must survive a truncated in-memory log — recovery reads \
             the FILE, not the retained vector"
        );
    }
    assert_eq!(
        r.get(&prefix(), b"after"),
        Some(vec![99]),
        "and a write made after the release is durable too"
    );
}
