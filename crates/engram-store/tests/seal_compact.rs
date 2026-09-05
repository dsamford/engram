//! Sealing and online compaction.
//!
//! A store's tail is read from behind the hot latch the writers hold; a sealed
//! segment is read lock-free. `open_wal` therefore seals the history it
//! replays, and `compact` merges segments WITHOUT holding the latch, carrying
//! over anything sealed while it worked.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
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
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "engram-seal-{tag}-{}-{}.wal",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&p);
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
fn open_wal_seals_the_replayed_history_so_it_is_read_lock_free() {
    let tmp = TmpWal::new("seal");
    {
        let s = Store::open_wal(tmp.path()).expect("open");
        for i in 0..50u32 {
            s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(vec![i as u8]))
                .expect("put");
        }
    }
    let r = Store::open_wal(tmp.path()).expect("reopen");
    assert_eq!(r.tail_versions(), 0, "the replayed history must not sit in the tail");
    assert_eq!(r.segment_count(), 1, "one sealed segment holds it");
    for i in 0..50u32 {
        assert_eq!(r.get(&prefix(), &i.to_be_bytes()), Some(vec![i as u8]), "key {i}");
    }
    // New writes land in the tail; a seal drains them into a second segment;
    // a compaction merges the two.
    r.put(&prefix(), b"new", StoredValue::Plain(vec![9])).expect("put");
    assert_eq!(r.tail_versions(), 1);
    assert!(r.seal().is_some());
    assert_eq!(r.tail_versions(), 0);
    assert_eq!(r.segment_count(), 2);
    r.compact();
    assert_eq!(
        r.segment_count(),
        2,
        "tiered: a 50-key base is left alone under a 1-key young run"
    );
    assert_eq!(r.get(&prefix(), b"new"), Some(vec![9]));
    assert_eq!(r.get(&prefix(), &7u32.to_be_bytes()), Some(vec![7]));
}

/// Size-tiered: a large base segment is left alone while the young run is
/// merged — and a tombstone in the young run over a key the base still holds
/// must SURVIVE that merge, or the key resurrects. Once the young run outgrows
/// the base, everything merges and the tombstone finally goes.
#[test]
fn compaction_keeps_a_dominant_base_and_a_tombstone_that_shadows_it() {
    let s = Store::new();
    for i in 0..20_000u32 {
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(vec![1])).expect("base");
    }
    s.seal();
    assert_eq!(s.segment_count(), 1);
    // Young run: an update of a base key, a delete of another, 300 new keys.
    s.put(&prefix(), &5u32.to_be_bytes(), StoredValue::Plain(vec![2])).expect("update");
    s.delete(&prefix(), &7u32.to_be_bytes());
    for i in 100_000..100_300u32 {
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(vec![3])).expect("young");
    }
    s.seal();
    s.seal(); // empty: refused
    assert_eq!(s.segment_count(), 2);
    let (_retired, _dropped) = s.compact();
    assert_eq!(s.segment_count(), 2, "a 20k base is more than twice a 302-key young run and is kept");
    assert_eq!(s.get(&prefix(), &5u32.to_be_bytes()), Some(vec![2]), "the update shadows the base");
    assert_eq!(s.get(&prefix(), &7u32.to_be_bytes()), None, "the tombstone over a base key survived the tiered merge");
    assert_eq!(s.get(&prefix(), &8u32.to_be_bytes()), Some(vec![1]), "an untouched base key");
    assert_eq!(s.get(&prefix(), &100_299u32.to_be_bytes()), Some(vec![3]));
    // Grow the young run past the base: a full merge, one segment, and the
    // deleted key is still absent (the tombstone did its job, then retired).
    for i in 200_000..225_000u32 {
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(vec![4])).expect("grow");
        if i % 5_000 == 0 {
            s.seal();
        }
    }
    s.seal();
    s.compact();
    assert_eq!(s.segment_count(), 1, "the young run outgrew the base: everything merged");
    assert_eq!(s.get(&prefix(), &7u32.to_be_bytes()), None);
    assert_eq!(s.get(&prefix(), &5u32.to_be_bytes()), Some(vec![2]));
    assert_eq!(s.get(&prefix(), &19_999u32.to_be_bytes()), Some(vec![1]));
    assert_eq!(s.get(&prefix(), &224_999u32.to_be_bytes()), Some(vec![4]));
}

/// A writer that seals every few hundred versions races a compactor that
/// runs back to back. Every key written must be readable at the end, and the
/// compactor must have carried over the segments sealed underneath it rather
/// than dropping them — the abandon path would leave a segment count that
/// never shrinks; the lost-data path would lose keys.
#[test]
fn compaction_runs_online_and_keeps_every_key_under_concurrent_seals() {
    let s = Store::new();
    const KEYS: u32 = 20_000;
    let done = std::sync::atomic::AtomicBool::new(false);
    let compactions = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|sc| {
        let s2 = s.clone();
        let done = &done;
        let compactions = &compactions;
        sc.spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                if s2.segment_count() >= 2 {
                    s2.compact();
                    compactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    std::thread::yield_now();
                }
            }
        });
        for i in 0..KEYS {
            s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(i.to_le_bytes().to_vec()))
                .expect("put");
            if i % 250 == 249 {
                s.seal();
            }
        }
        done.store(true, std::sync::atomic::Ordering::Release);
    });
    s.seal();
    let (_, _) = s.compact();
    for i in 0..KEYS {
        assert_eq!(
            s.get(&prefix(), &i.to_be_bytes()),
            Some(i.to_le_bytes().to_vec()),
            "key {i} lost across an online compaction"
        );
    }
    assert!(
        s.segment_count() <= 3,
        "tiered: at most a base, the merged young run and one carried-over seal — got {}",
        s.segment_count()
    );
    assert!(
        compactions.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "the compactor never ran concurrently with the writer — this test proved nothing"
    );
}
