#![allow(non_snake_case)]
//! The WAL FRONTS THE PAGED TAIL: a bigger-than-RAM store is also durable.
//!
//! Before this, `--paged-dir` and `--data-dir` were two modes that traded
//! durability against capacity: the paged store lost its unsealed tail on
//! every crash (its durability was at seal boundaries only), and the
//! WAL-backed store had to hold its whole history resident. Now
//! [`Store::open_paged_dir_with_wal`] replays the WAL into the tail and
//! attaches it, every acknowledged write is fsync'd first, and a spill
//! checkpoints the WAL behind the segments it wrote — the file is ROTATED to
//! an anchor `(first_seq, prev_hash)` so the suffix still verifies.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_log::Wal;
use engram_store::{OpenWalError, Store, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-pagedwal-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        TmpDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn wal(&self) -> std::path::PathBuf {
        self.0.join("engram.wal")
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn val(i: u32) -> Vec<u8> {
    vec![7, (i % 251) as u8, (i / 251) as u8]
}

fn put(s: &Store, i: u32) {
    s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(val(i)))
        .expect("put");
}

fn assert_present(s: &Store, range: std::ops::Range<u32>, what: &str) {
    for i in range {
        assert_eq!(
            s.get(&pfx(), &i.to_be_bytes()),
            Some(val(i)),
            "{what}: key {i} missing or wrong"
        );
    }
}

/// 600 rows sealed into paged segments on disk, opened with a fresh WAL.
fn paged_corpus(dir: &TmpDir) -> (Store, std::sync::Arc<engram_store::paged::BlockCache>) {
    {
        let s = Store::new();
        for i in 0..600u32 {
            put(&s, i);
            if i % 200 == 199 {
                s.seal();
            }
        }
        s.into_paged(dir.path(), 8 << 20).expect("into_paged");
        // Dropped: the segment files are the corpus now.
    }
    Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("open paged + wal")
}

/// Writes acknowledged after a paged open survive the process: they are in
/// the WAL, and the next paged open replays them into the tail.
#[test]
fn a_writes_after_a_paged_open_survive_a_crash() {
    let dir = TmpDir::new("survive");
    {
        let (s, _cache) = paged_corpus(&dir);
        assert_eq!(s.tail_versions(), 0, "a fresh WAL replays nothing");
        assert!(s.has_wal());
        for i in 600..640u32 {
            put(&s, i);
        }
        // No seal, no checkpoint: the 40 rows exist only in the tail and the
        // WAL. Dropping the store is the crash.
    }
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen");
    assert_eq!(s.tail_versions(), 40, "the WAL replayed the unsealed tail");
    assert_present(&s, 0..640, "after the crash");
    assert_eq!(Wal::read_anchor(&dir.wal()).expect("anchor").first_seq, 0);
    assert_eq!(s.log_len(), 40);
}

/// A spill reports the log boundary its segments made durable, and a
/// checkpoint there rotates the WAL: the file's anchor moves to the boundary,
/// only the records since it remain, and a later open replays exactly those.
#[test]
fn b_a_spill_checkpoints_the_wal_behind_the_segments_it_wrote() {
    let dir = TmpDir::new("checkpoint");
    {
        let (s, cache) = paged_corpus(&dir);
        for i in 600..660u32 {
            put(&s, i);
        }
        assert!(s.seal().is_some());
        let (converted, below) = s
            .spill_sealed_into_reporting(dir.path(), &cache)
            .expect("spill");
        assert_eq!(converted, 1);
        assert_eq!(below, Some(60), "the seal's log boundary: 60 logged records");
        let dropped = s.checkpoint_wal(60).expect("checkpoint");
        assert_eq!(dropped, 60);
        let anchor = Wal::read_anchor(&dir.wal()).expect("anchor");
        assert_eq!(anchor.first_seq, 60);
        // The chain goes on from the anchor: five more records, seq 60..65.
        for i in 660..665u32 {
            put(&s, i);
        }
        assert_eq!(s.log_len(), 65);
    }
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen");
    assert_eq!(s.tail_versions(), 5, "only the records since the checkpoint replay");
    assert_eq!(s.log_len(), 65, "the sequence resumes at the anchor");
    assert_present(&s, 0..665, "after the checkpoint and a crash");
    // And the rotated file is small: header + five records, not sixty-five.
    let size = std::fs::metadata(dir.wal()).expect("meta").len();
    assert!(size < 64 + 5 * 200, "rotated WAL is {size} bytes");
}

/// A crash BETWEEN a spill and its checkpoint leaves the WAL holding records a
/// segment already has: the open replays the chain but not the rows, so
/// nothing lands in the tail twice, and writing on from there still works.
#[test]
fn c_a_crash_between_spill_and_checkpoint_replays_no_row_twice() {
    let dir = TmpDir::new("between");
    {
        let (s, cache) = paged_corpus(&dir);
        for i in 600..630u32 {
            put(&s, i);
        }
        s.seal();
        let (converted, below) = s
            .spill_sealed_into_reporting(dir.path(), &cache)
            .expect("spill");
        assert_eq!((converted, below), (1, Some(30)));
        // No checkpoint — the crash lands here.
    }
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen");
    assert_eq!(s.tail_versions(), 0, "every WAL record is already in a segment on disk");
    assert_eq!(s.log_len(), 30, "the chain still runs through them");
    assert_present(&s, 0..630, "after the between-crash");
    put(&s, 630);
    assert_eq!(s.log_len(), 31);
    drop(s);
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen again");
    assert_eq!(s.tail_versions(), 1);
    assert_present(&s, 0..631, "after writing on");
}

/// A rotated WAL is REFUSED by the whole-history open: its records are a
/// suffix whose prefix lives in segments, and replaying it as the database
/// would silently drop everything before the anchor.
#[test]
fn d_a_rotated_wal_is_refused_by_the_whole_history_open() {
    let dir = TmpDir::new("refused");
    {
        let (s, cache) = paged_corpus(&dir);
        put(&s, 600);
        s.seal();
        let (_, below) = s
            .spill_sealed_into_reporting(dir.path(), &cache)
            .expect("spill");
        s.checkpoint_wal(below.expect("boundary")).expect("checkpoint");
    }
    match Store::open_wal(&dir.wal()) {
        Err(OpenWalError::Format(engram_log::WalError::Rotated { first_seq })) => {
            assert_eq!(first_seq, 1);
        }
        other => panic!("a rotated WAL must be refused, got {:?}", other.map(|_| ())),
    }
    // Still byte-for-byte intact — the refusal touched nothing.
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("paged open");
    assert_present(&s, 0..601, "after the refusal");
}

/// A torn record at the end of a ROTATED file is dropped and the rest
/// replays against the anchor — the same torn-tail rule as before, now for a
/// chain that does not start at genesis.
#[test]
fn e_a_torn_tail_after_a_rotation_is_dropped_not_the_prefix() {
    let dir = TmpDir::new("torn");
    {
        let (s, cache) = paged_corpus(&dir);
        put(&s, 600);
        s.seal();
        let (_, below) = s
            .spill_sealed_into_reporting(dir.path(), &cache)
            .expect("spill");
        s.checkpoint_wal(below.expect("boundary")).expect("checkpoint");
        for i in 601..611u32 {
            put(&s, i);
        }
    }
    // Tear the last record: drop five bytes off the file.
    let path = dir.wal();
    let len = std::fs::metadata(&path).expect("meta").len();
    let f = std::fs::OpenOptions::new().write(true).open(&path).expect("open");
    f.set_len(len - 5).expect("tear");
    drop(f);
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen torn");
    assert_eq!(s.tail_versions(), 9, "nine complete records replay, the torn tenth is dropped");
    assert_present(&s, 0..610, "the intact prefix");
    assert_eq!(s.get(&pfx(), &610u32.to_be_bytes()), None, "the torn record is gone");
    assert_eq!(s.log_len(), 10, "anchor 1 + nine records");
}

/// Group commit keeps fsyncing the RIGHT file across a checkpoint: the
/// shared fsync handle is swapped with the sink, so a write acknowledged
/// through `sync_pending` after a rotation is in the new file.
#[test]
fn f_group_commit_fsyncs_the_rotated_file() {
    let dir = TmpDir::new("group");
    {
        let (s, cache) = paged_corpus(&dir);
        s.set_group_commit(true);
        put(&s, 600);
        assert!(s.sync_pending().expect("sync"));
        s.seal();
        let (_, below) = s
            .spill_sealed_into_reporting(dir.path(), &cache)
            .expect("spill");
        s.checkpoint_wal(below.expect("boundary")).expect("checkpoint");
        for i in 601..604u32 {
            put(&s, i);
        }
        assert!(s.sync_pending().expect("sync after the rotation"));
    }
    let (s, _cache) =
        Store::open_paged_dir_with_wal(dir.path(), 8 << 20, &dir.wal()).expect("reopen");
    assert_eq!(s.tail_versions(), 3);
    assert_present(&s, 0..604, "after a group-committed rotation");
}
