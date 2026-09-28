//! The WAL writes ZERO-FILLED space ahead of its end, and records land at the
//! LOGICAL end — never after the zeros — across every reopen.
//!
//! Why the space exists: a commit's `fdatasync` must also journal a change of
//! file SIZE, and on XFS/ext4 only blocks that were WRITTEN (not `set_len`'d
//! or `fallocate`d) make an append inside them a data-only sync. On
//! 2026-09-27 an engram commit cost ~5.6 ms at one client on the bench volume
//! against PostgreSQL's ~2.4 ms on the same disk, and PostgreSQL's
//! `wal_init_zero` is the same device.
//!
//! What must not change: recovery reads the zeros as the end of the log and
//! truncates them (it must — bytes past the last valid record can hold a
//! complete, chain-valid record that was never acknowledged), then re-writes
//! the space; and the next append lands right after the last record, so the
//! chain continues and every record replays.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_log::{WAL_HEADER_LEN, WAL_PREALLOC_FIRST, Wal};
use engram_store::{Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

struct TmpWal(std::path::PathBuf);

impl TmpWal {
    fn new(tag: &str) -> TmpWal {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-wal-zeroed-{}-{}-{}.log",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&p);
        TmpWal(p)
    }
}

impl Drop for TmpWal {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn physical(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).expect("meta").len()
}

#[test]
fn records_land_at_the_logical_end_and_the_zeroed_space_follows_it() {
    let tmp = TmpWal::new("end");
    let p = &tmp.0;
    {
        let s = Store::open_wal(p).expect("open");
        s.put(&prefix(), b"k1", StoredValue::Plain(vec![1])).expect("k1");
        s.put(&prefix(), b"k2", StoredValue::Plain(vec![2])).expect("k2");
    }
    let end_1 = Wal::logical_len(p).expect("a WAL");
    assert!(end_1 > WAL_HEADER_LEN as u64, "two records were written");
    assert!(
        physical(p) >= end_1 + WAL_PREALLOC_FIRST / 2,
        "zero-filled space follows the records: physical {} against logical {end_1}",
        physical(p)
    );

    // Reopen twice, appending each time: every append must continue the
    // chain right after the last record, so all of them replay.
    for (i, k) in [b"k3", b"k4"].iter().enumerate() {
        let s = Store::open_wal(p).expect("reopen");
        assert_eq!(s.log_len(), 2 + i as u64, "every earlier record replayed");
        s.put(&prefix(), *k, StoredValue::Plain(vec![3 + i as u8]))
            .expect("append after reopen");
    }
    let s = Store::open_wal(p).expect("final reopen");
    assert_eq!(s.log_len(), 4, "the chain continued across both reopens");
    for (k, v) in [(b"k1", 1u8), (b"k2", 2), (b"k3", 3), (b"k4", 4)] {
        assert_eq!(s.get(&prefix(), k), Some(vec![v]), "{k:?} survives");
    }
    let end_4 = Wal::logical_len(p).expect("a WAL");
    // Four records of the same shape: the logical end grew by exactly two
    // more records' worth, not by the zeroed space in between.
    let per = (end_1 - WAL_HEADER_LEN as u64) / 2;
    assert_eq!(
        end_4,
        WAL_HEADER_LEN as u64 + 4 * per,
        "no record landed after the zero-filled space"
    );
    assert!(physical(p) > end_4, "the space is re-written after every open");
}

#[test]
fn a_long_log_extends_its_zeroed_space_as_it_grows() {
    let tmp = TmpWal::new("grow");
    let p = &tmp.0;
    let s = Store::open_wal(p).expect("open");
    // ~1 MiB of records against a 256 KiB first step: the space must be
    // extended (and each extension doubles), never outrun by more than a
    // record, and every record must replay.
    let big = vec![7u8; 4096];
    for i in 0..256u32 {
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(big.clone()))
            .expect("put");
    }
    let logical = Wal::logical_len(p).expect("a WAL");
    assert!(logical > 1 << 20, "a MiB of log was written: {logical}");
    assert!(
        physical(p) >= logical,
        "the zero-filled space kept ahead of the end: physical {} against logical {logical}",
        physical(p)
    );
    drop(s);
    let r = Store::open_wal(p).expect("reopen");
    assert_eq!(r.log_len(), 256, "every record replays");
    assert_eq!(r.get(&prefix(), &255u32.to_be_bytes()), Some(big));
}
