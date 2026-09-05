#![allow(non_snake_case)]
//! Anti-pattern workloads — the shapes that break LSM/MVCC stores in
//! production, run as CORRECTNESS tests. The bench harness times these
//! same shapes; here every one must stay RIGHT, not fast: hot-key version
//! storms, tombstone graveyards, pinned-reader starvation, segment pileup,
//! boundary keys, oversized values, and the columnar layout's worst cases.

use engram_key::value::Tag;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::record::{PropertyId, Record};
use engram_store::{COLUMNAR_MIN_ROWS, Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn int64(v: i64) -> Vec<u8> {
    let mut out = vec![Tag::INT64.byte()];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

#[test]
fn a_hot_key_with_10k_versions_reads_correctly_before_and_after_retirement() {
    // The MVCC anti-pattern: one key overwritten relentlessly. The chain
    // walk must return the newest version at every point, the seal must not
    // disturb it, and compaction must retire the graveyard.
    let s = Store::new();
    for v in 0..10_000i64 {
        s.put(&prefix(), b"hot", StoredValue::Plain(int64(v)))
            .expect("put");
    }
    assert_eq!(
        s.get(&prefix(), b"hot"),
        Some(int64(9_999)),
        "newest wins in the tail"
    );
    s.seal().expect("seal");
    assert_eq!(
        s.get(&prefix(), b"hot"),
        Some(int64(9_999)),
        "newest wins from the segment"
    );
    let (retired, _) = s.compact();
    assert_eq!(retired, 9_999, "every unreachable version retires");
    assert_eq!(
        s.get(&prefix(), b"hot"),
        Some(int64(9_999)),
        "and the survivor is the newest"
    );
}

#[test]
fn a_tombstone_graveyard_scans_to_only_the_living() {
    // 95% of keys deleted: the scan must return exactly the survivors, at
    // every stage — tail, sealed, compacted — and compaction must purge the
    // tombstones outright rather than carry a graveyard forever.
    let s = Store::new();
    let n = 2_000u32;
    for i in 0..n {
        s.put(
            &prefix(),
            &i.to_be_bytes(),
            StoredValue::Plain(int64(i64::from(i))),
        )
        .expect("put");
    }
    for i in 0..n {
        if i % 20 != 0 {
            s.delete(&prefix(), &i.to_be_bytes());
        }
    }
    let live = (n as usize).div_ceil(20);
    assert_eq!(
        s.scan_at(&prefix(), u64::MAX).len(),
        live,
        "tail scan sees only the living"
    );
    s.seal().expect("seal");
    assert_eq!(
        s.scan_at(&prefix(), u64::MAX).len(),
        live,
        "sealed scan agrees"
    );
    s.compact();
    assert_eq!(
        s.scan_at(&prefix(), u64::MAX).len(),
        live,
        "compacted scan agrees"
    );
    for i in (0..n).step_by(20) {
        assert_eq!(
            s.get(&prefix(), &i.to_be_bytes()),
            Some(int64(i64::from(i))),
            "survivor {i} intact"
        );
    }
    assert_eq!(
        s.get(&prefix(), &1u32.to_be_bytes()),
        None,
        "the dead stay dead"
    );
}

#[test]
fn a_pinned_reader_starves_retirement_and_release_unblocks_it() {
    // The long-running-reader anti-pattern: while the pin lives, versions
    // it can reach must survive ANY number of compactions; when it drops,
    // the next compaction collects.
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(int64(1)))
        .expect("v1");
    let pin = s.pin_snapshot();
    let pinned_ts = s.gc_watermark();
    for v in 2..=500i64 {
        s.put(&prefix(), b"k", StoredValue::Plain(int64(v)))
            .expect("v");
    }
    s.seal().expect("seal");
    for _ in 0..3 {
        s.compact();
        assert_eq!(
            s.get_at(&prefix(), b"k", pinned_ts),
            Some(int64(1)),
            "the pinned read must survive every compaction"
        );
    }
    drop(pin);
    let (retired, _) = s.compact();
    assert!(
        retired > 0,
        "release must unblock retirement (retired {retired})"
    );
    assert_eq!(
        s.get(&prefix(), b"k"),
        Some(int64(500)),
        "the newest survives collection"
    );
}

#[test]
fn boundary_keys_at_the_0xFF_edge_scan_and_read_exactly() {
    // The scan upper bound is computed by incrementing the last non-0xFF
    // byte — a body of ALL 0xFF bytes rides that carry logic's edge.
    let s = Store::new();
    let edge = vec![0xFFu8; 16];
    let near = {
        let mut k = vec![0xFFu8; 15];
        k.push(0xFE);
        k
    };
    s.put(&prefix(), &edge, StoredValue::Plain(int64(1)))
        .expect("put");
    s.put(&prefix(), &near, StoredValue::Plain(int64(2)))
        .expect("put");
    s.put(&prefix(), b"", StoredValue::Plain(int64(3)))
        .expect("empty body is a key too");
    assert_eq!(s.get(&prefix(), &edge), Some(int64(1)));
    assert_eq!(s.get(&prefix(), b""), Some(int64(3)));
    let all = s.scan_at(&prefix(), u64::MAX);
    assert_eq!(all.len(), 3, "the edge keys are IN the scan");
    // A body-prefix scan right at the edge must find exactly the edge keys.
    let ff = s.scan_body_prefix(&prefix(), &[0xFFu8; 15]);
    assert_eq!(ff.len(), 2, "prefix scan at the carry edge");
    s.seal().expect("seal");
    s.compact();
    assert_eq!(
        s.scan_body_prefix(&prefix(), &[0xFFu8; 15]).len(),
        2,
        "and after compaction"
    );
}

#[test]
fn a_10MB_value_round_trips_through_every_layer() {
    // Oversized single values: no layer may truncate, split, or choke.
    let s = Store::new();
    let big: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
    s.put(&prefix(), b"big", StoredValue::Plain(big.clone()))
        .expect("put");
    assert_eq!(
        s.get(&prefix(), b"big").as_deref(),
        Some(big.as_slice()),
        "tail read"
    );
    s.seal().expect("seal");
    assert_eq!(
        s.get(&prefix(), b"big").as_deref(),
        Some(big.as_slice()),
        "sealed read"
    );
    s.compact();
    assert_eq!(
        s.get(&prefix(), b"big").as_deref(),
        Some(big.as_slice()),
        "compacted read"
    );
    let entries = s.log_tail(0);
    let recovered = Store::recover(&entries).expect("recover");
    assert_eq!(
        recovered.get(&prefix(), b"big").as_deref(),
        Some(big.as_slice()),
        "recovered"
    );
}

#[test]
fn segment_pileup_reads_correctly_across_64_generations() {
    // The LSM read-amplification anti-pattern: many seals, no compaction.
    // Every read must still resolve to the newest generation, then one
    // compaction collapses the pile without changing a single answer.
    let s = Store::new();
    let n = 200u32;
    for generation in 0..64i64 {
        for i in 0..n {
            s.put(
                &prefix(),
                &i.to_be_bytes(),
                StoredValue::Plain(int64(generation * 1_000 + i64::from(i))),
            )
            .expect("put");
        }
        s.seal().expect("seal");
    }
    assert_eq!(s.segment_count(), 64);
    let before = s.scan_at(&prefix(), u64::MAX);
    assert_eq!(before.len(), n as usize);
    for i in 0..n {
        assert_eq!(
            s.get(&prefix(), &i.to_be_bytes()),
            Some(int64(63 * 1_000 + i64::from(i))),
            "key {i} must read the newest generation across the pile"
        );
    }
    s.compact();
    assert_eq!(s.segment_count(), 1, "the pile collapses to one segment");
    assert_eq!(
        s.scan_at(&prefix(), u64::MAX),
        before,
        "with identical answers"
    );
}

#[test]
fn every_row_a_unique_signature_is_the_columnar_worst_case_and_stays_correct() {
    // Signature explosion: no group reaches the head threshold, so nothing
    // blocks — the layout must degrade to plain rows, never to wrong reads.
    let s = Store::new();
    let n = COLUMNAR_MIN_ROWS as u32 * 4;
    for i in 0..n {
        let mut r = Record::new();
        // A distinct property id per row: every signature unique.
        r.set(PropertyId(1_000 + i), int64(i64::from(i)));
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(r.encode()))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();
    assert_eq!(s.columnar_stats(), (0, 0), "no group reaches the threshold");
    for i in (0..n).step_by(17) {
        let got = s.get(&prefix(), &i.to_be_bytes()).expect("present");
        let rec = Record::decode(&got).expect("decodes");
        assert_eq!(
            rec.get(PropertyId(1_000 + i)),
            Some(int64(i64::from(i)).as_slice())
        );
    }
}

#[test]
fn half_the_block_overwritten_still_merges_every_read_path_correctly() {
    // Block-then-churn: compact into blocks, then overwrite half the rows.
    // Point reads, scans and column scans must all serve the override for
    // the churned half and the block for the quiet half.
    let s = Store::new();
    let n = COLUMNAR_MIN_ROWS as u32 * 2;
    let make = |i: u32, salt: i64| -> Vec<u8> {
        let mut r = Record::new();
        r.set(PropertyId(1), int64(i64::from(i) + salt));
        r.encode()
    };
    for i in 0..n {
        s.put(&prefix(), &i.to_be_bytes(), StoredValue::Plain(make(i, 0)))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();
    assert_eq!(s.columnar_stats().1, n as usize);
    for i in 0..n / 2 {
        s.put(
            &prefix(),
            &i.to_be_bytes(),
            StoredValue::Plain(make(i, 1_000_000)),
        )
        .expect("overwrite");
    }
    for i in 0..n {
        let want = if i < n / 2 {
            make(i, 1_000_000)
        } else {
            make(i, 0)
        };
        assert_eq!(
            s.get(&prefix(), &i.to_be_bytes()),
            Some(want),
            "point read for {i}"
        );
    }
    assert_eq!(s.scan_at(&prefix(), u64::MAX).len(), n as usize);
    let col = s.scan_column_at(&prefix(), &[], 1, u64::MAX);
    assert_eq!(col.len(), n as usize, "column scan sees every row once");
    for (body, v) in col {
        let i = u32::from_be_bytes(body.as_slice().try_into().expect("4 bytes"));
        let salt = if i < n / 2 { 1_000_000 } else { 0 };
        assert_eq!(v, int64(i64::from(i) + salt), "column value for {i}");
    }
}

#[test]
fn interleaved_writes_and_scans_always_read_their_own_writes() {
    // Read-your-writes under churn, crossing seal boundaries mid-stream.
    let s = Store::new();
    for i in 0..1_000u32 {
        s.put(
            &prefix(),
            &i.to_be_bytes(),
            StoredValue::Plain(int64(i64::from(i))),
        )
        .expect("put");
        assert_eq!(
            s.get(&prefix(), &i.to_be_bytes()),
            Some(int64(i64::from(i))),
            "immediate read-back of {i}"
        );
        if i % 100 == 99 {
            s.seal().expect("seal");
        }
        if i % 250 == 249 {
            s.compact();
        }
        if i % 333 == 0 && i > 0 {
            let scan = s.scan_at(&prefix(), u64::MAX);
            assert_eq!(
                scan.len(),
                i as usize + 1,
                "scan mid-churn sees every write so far"
            );
        }
    }
}

#[test]
fn an_unlogged_put_reads_back_but_replay_never_sees_it() {
    // The bulk-load contract stated as a test: the row is real to every
    // reader, invisible to recovery-by-replay, and counted so the trade
    // is never silent.
    let store = Store::new();
    let g = prefix();
    store
        .put(&g, b"logged", StoredValue::Plain(vec![1]))
        .unwrap();
    store
        .put_unlogged(&g, b"bulk", StoredValue::Plain(vec![2]))
        .unwrap();
    assert_eq!(store.get(&g, b"bulk").as_deref(), Some(&[2u8][..]), "reads");
    assert_eq!(store.unlogged_count(), 1, "counted");
    let replayed = Store::recover(&store.log_tail(0)).expect("recover");
    assert_eq!(
        replayed.get(&g, b"logged").as_deref(),
        Some(&[1u8][..]),
        "logged row survives replay"
    );
    assert!(
        replayed.get(&g, b"bulk").is_none(),
        "unlogged row must NOT survive replay — durability is by re-ingest"
    );
}
