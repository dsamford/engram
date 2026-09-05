#![allow(non_snake_case)]
//! The signature-homogeneous head/tail layout — the census's verdict, built.
//!
//! The one property every test here defends: **the layout changes WHERE
//! bytes live, never WHAT a reader sees.** Reads are compared before and
//! after compaction byte for byte; later writes and deletes must shadow
//! block rows; multi-version chains, sealed values and non-canonical bytes
//! must refuse the block and stay rows.

use std::collections::BTreeMap;

use engram_key::value::Tag;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::record::{PropertyId, Record};
use engram_store::{COLUMNAR_MIN_ROWS, Store, StoredValue};

fn int64(v: i64) -> Vec<u8> {
    let mut out = vec![Tag::INT64.byte()];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

fn string(s: &str) -> Vec<u8> {
    let mut out = vec![Tag::STRING.byte()];
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

/// A canonical record with the shared signature (props 1: fixed-width id,
/// 2: variable-width name) — the "head" shape.
fn head_record(i: u32) -> Vec<u8> {
    let mut r = Record::new();
    r.set(PropertyId(1), int64(i64::from(i))); // fixed-width column
    r.set(PropertyId(2), string(&format!("name-{i}"))); // variable-width column
    r.encode()
}

/// A different signature — the "tail" shape (too few rows to block).
fn tail_record(i: u32) -> Vec<u8> {
    let mut r = Record::new();
    r.set(PropertyId(7), int64(i64::from(i) + 100));
    r.encode()
}

fn body(i: u32) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}

/// Write `n` head rows and 3 tail rows, returning every (body, value) pair.
fn populate(s: &Store, n: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut written = Vec::new();
    for i in 0..n {
        let v = head_record(i);
        s.put(&prefix(), &body(i), StoredValue::Plain(v.clone()))
            .expect("put");
        written.push((body(i), v));
    }
    for i in 0..3u32 {
        let v = tail_record(i);
        s.put(&prefix(), &body(1000 + i), StoredValue::Plain(v.clone()))
            .expect("put");
        written.push((body(1000 + i), v));
    }
    written
}

#[test]
fn compaction_blocks_the_head_and_reads_are_byte_identical() {
    let s = Store::new();
    let written = populate(&s, COLUMNAR_MIN_ROWS as u32 + 10);
    s.seal().expect("seal");

    // The full read surface BEFORE the layout changes.
    let before_scan = s.scan_at(&prefix(), u64::MAX);

    s.compact();

    let (blocks, rows) = s.columnar_stats();
    assert_eq!(blocks, 1, "one head signature, one block");
    assert_eq!(rows, COLUMNAR_MIN_ROWS + 10, "every head row blocked");

    // Point reads: byte-identical for every key, head and tail alike.
    for (b, v) in &written {
        assert_eq!(
            s.get(&prefix(), b).as_ref(),
            Some(v),
            "get changed for body {b:?}"
        );
    }
    // Scans: identical row set, identical bytes.
    assert_eq!(s.scan_at(&prefix(), u64::MAX), before_scan, "scan changed");
}

#[test]
fn the_tail_signature_stays_in_row_form() {
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();
    let (_, rows) = s.columnar_stats();
    assert_eq!(
        rows, COLUMNAR_MIN_ROWS,
        "exactly the head rows blocked — the 3 tail rows stay rows"
    );
}

#[test]
fn a_later_write_shadows_its_block_row() {
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();

    let newer = head_record(9999);
    s.put(&prefix(), &body(0), StoredValue::Plain(newer.clone()))
        .expect("overwrite");

    assert_eq!(
        s.get(&prefix(), &body(0)),
        Some(newer.clone()),
        "tail write must win"
    );
    let scan: BTreeMap<Vec<u8>, Vec<u8>> = s.scan_at(&prefix(), u64::MAX).into_iter().collect();
    assert_eq!(
        scan.get(&body(0)),
        Some(&newer),
        "scan must serve the tail write"
    );
}

#[test]
fn a_delete_after_compaction_does_not_resurrect_from_the_block() {
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();

    s.delete(&prefix(), &body(1));

    assert_eq!(
        s.get(&prefix(), &body(1)),
        None,
        "deleted key must stay deleted"
    );
    let scan: BTreeMap<Vec<u8>, Vec<u8>> = s.scan_at(&prefix(), u64::MAX).into_iter().collect();
    assert!(
        !scan.contains_key(&body(1)),
        "scan must not resurrect the block row"
    );
    let col = s.scan_column_at(&prefix(), &[], 1, u64::MAX);
    assert!(
        col.iter().all(|(b, _)| b != &body(1)),
        "the column scan must not resurrect the block row either"
    );
}

#[test]
fn multi_version_chains_stay_rows_and_pinned_snapshots_hold() {
    let s = Store::new();
    // Two versions per key, reader pinned between them: compaction must keep
    // both (watermark) and therefore must NOT block these chains.
    for i in 0..COLUMNAR_MIN_ROWS as u32 {
        s.put(&prefix(), &body(i), StoredValue::Plain(head_record(i)))
            .expect("v1");
    }
    let pin_ts = {
        let _pin = s.pin_snapshot();
        // While pinned, write the second generation.
        for i in 0..COLUMNAR_MIN_ROWS as u32 {
            s.put(
                &prefix(),
                &body(i),
                StoredValue::Plain(head_record(i + 5000)),
            )
            .expect("v2");
        }
        s.seal().expect("seal");
        let ts = s.gc_watermark(); // the pinned timestamp
        s.compact();

        let (blocks, rows) = s.columnar_stats();
        assert_eq!((blocks, rows), (0, 0), "two-version chains must stay rows");
        for i in 0..COLUMNAR_MIN_ROWS as u32 {
            assert_eq!(
                s.get_at(&prefix(), &body(i), ts),
                Some(head_record(i)),
                "the pinned snapshot must still read v1"
            );
        }
        ts
    };
    // Pin released: a second compaction may retire v1 and NOW block the rest.
    s.compact();
    let (blocks, rows) = s.columnar_stats();
    assert_eq!(
        (blocks, rows),
        (1, COLUMNAR_MIN_ROWS),
        "single-version chains block after the pin"
    );
    let _ = pin_ts;
}

#[test]
fn non_canonical_record_bytes_stay_rows_and_read_back_verbatim() {
    let s = Store::new();
    // Hand-built record bytes with properties OUT of id order: decodes fine,
    // re-encodes sorted — so blocking it would change its bytes. It must
    // stay a row and read back exactly as written.
    let mut noncanon = Vec::new();
    noncanon.extend_from_slice(&2u32.to_le_bytes());
    noncanon.extend_from_slice(&9u32.to_le_bytes()); // id 9 FIRST
    noncanon.extend_from_slice(&int64(33));
    noncanon.extend_from_slice(&3u32.to_le_bytes()); // id 3 second
    noncanon.extend_from_slice(&int64(44));
    let rec = Record::decode(&noncanon).expect("decodes");
    assert_ne!(
        rec.encode(),
        noncanon,
        "the fixture must actually be non-canonical"
    );

    for i in 0..COLUMNAR_MIN_ROWS as u32 + 5 {
        s.put(&prefix(), &body(i), StoredValue::Plain(noncanon.clone()))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();

    let (blocks, rows) = s.columnar_stats();
    assert_eq!(
        (blocks, rows),
        (0, 0),
        "non-canonical rows must refuse the block"
    );
    assert_eq!(
        s.get(&prefix(), &body(0)),
        Some(noncanon),
        "the original bytes must come back verbatim"
    );
}

#[test]
fn empty_and_foreign_values_self_exclude() {
    let s = Store::new();
    // Adjacency/membership-shaped rows: empty values. Not records — no block.
    for i in 0..COLUMNAR_MIN_ROWS as u32 + 20 {
        s.put(&prefix(), &body(i), StoredValue::Plain(Vec::new()))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();
    assert_eq!(s.columnar_stats(), (0, 0), "non-records must not block");
    assert_eq!(
        s.get(&prefix(), &body(3)),
        Some(Vec::new()),
        "reads unchanged"
    );
}

#[test]
fn the_column_scan_projects_one_property_with_correct_merge() {
    let s = Store::new();
    let n = COLUMNAR_MIN_ROWS as u32;
    populate(&s, n);
    s.seal().expect("seal");
    s.compact();

    // Overwrite one row (fallback path) and delete another (suppression).
    let newer = head_record(7777);
    s.put(&prefix(), &body(2), StoredValue::Plain(newer.clone()))
        .expect("overwrite");
    s.delete(&prefix(), &body(3));

    let col = s.scan_column_at(&prefix(), &[], 1, u64::MAX);
    // Expect: every head row except the deleted one; tail rows lack prop 1.
    assert_eq!(col.len(), n as usize - 1, "head rows minus the deleted one");
    let by_body: BTreeMap<Vec<u8>, Vec<u8>> = col.into_iter().collect();
    let expect_rec = |bytes: &[u8]| -> Vec<u8> {
        Record::decode(bytes)
            .expect("record")
            .get(PropertyId(1))
            .expect("prop 1")
            .to_vec()
    };
    assert_eq!(
        by_body.get(&body(0)),
        Some(&expect_rec(&head_record(0))),
        "block-served value"
    );
    assert_eq!(
        by_body.get(&body(2)),
        Some(&expect_rec(&newer)),
        "the overwrite must win"
    );
    assert!(!by_body.contains_key(&body(3)), "the delete must suppress");
    // And the values are the exact tagged bytes the records carry.
    for i in [1u32, 5, 10] {
        assert_eq!(by_body.get(&body(i)), Some(&expect_rec(&head_record(i))));
    }
}

#[test]
fn recompaction_dissolves_and_rebuilds_blocks() {
    let s = Store::new();
    let n = COLUMNAR_MIN_ROWS as u32;
    populate(&s, n);
    s.seal().expect("seal");
    s.compact();
    assert_eq!(s.columnar_stats().1, n as usize);

    // A second generation lands in the tail, then seals: the two segments
    // compact together, blocks dissolve, retention runs, blocks rebuild.
    for i in 0..n {
        s.put(
            &prefix(),
            &body(i),
            StoredValue::Plain(head_record(i + 9000)),
        )
        .expect("v2");
    }
    s.seal().expect("seal");
    s.compact();

    let (blocks, rows) = s.columnar_stats();
    assert_eq!(
        (blocks, rows),
        (1, n as usize),
        "rebuilt around the new generation"
    );
    for i in 0..n {
        assert_eq!(
            s.get(&prefix(), &body(i)),
            Some(head_record(i + 9000)),
            "the NEW generation must be what reads see"
        );
    }
}

#[test]
fn is_sealed_still_answers_for_blocked_rows() {
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();
    assert_eq!(
        s.is_sealed(&prefix(), &body(0)),
        Some(false),
        "a blocked row is plaintext and must SAY so, not vanish"
    );
}

#[test]
fn sealed_ciphertext_stays_in_row_form() {
    let s = Store::new();
    // Sealed values are opaque bytes under the crypto layer's rules — the
    // layout must not touch them, whatever they happen to decode as.
    for i in 0..COLUMNAR_MIN_ROWS as u32 + 8 {
        s.put(&prefix(), &body(i), StoredValue::Sealed(head_record(i)))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();
    assert_eq!(
        s.columnar_stats(),
        (0, 0),
        "sealed values must refuse the block"
    );
    assert_eq!(
        s.get(&prefix(), &body(2)),
        Some(head_record(2)),
        "reads unchanged"
    );
    assert_eq!(
        s.is_sealed(&prefix(), &body(2)),
        Some(true),
        "and still SAY sealed"
    );
}

#[test]
fn the_column_visitor_hands_over_every_live_entry_once_without_a_map() {
    // The visitor's contract against an INDEPENDENT reference: the row
    // scan's record decode. After compaction the block is overridden four
    // ways — a changed value, a delete, a new version LACKING the property,
    // and a row that was never blocked — and every one must resolve the
    // way the row scan resolves it.
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();
    let mut changed = Record::new();
    changed.set(PropertyId(1), int64(-5));
    changed.set(PropertyId(2), string("changed"));
    s.put(&prefix(), &body(3), StoredValue::Plain(changed.encode()))
        .expect("put");
    s.delete(&prefix(), &body(4));
    let mut lacking = Record::new();
    lacking.set(PropertyId(2), string("no id any more"));
    s.put(&prefix(), &body(5), StoredValue::Plain(lacking.encode()))
        .expect("put");
    s.put(
        &prefix(),
        &body(2000),
        StoredValue::Plain(head_record(2000)),
    )
    .expect("put");

    let expected: BTreeMap<Vec<u8>, Vec<u8>> = s
        .scan_at(&prefix(), u64::MAX)
        .into_iter()
        .filter_map(|(b, v)| {
            let rec = Record::decode(&v).ok()?;
            rec.get(PropertyId(1)).map(|x| (b, x.to_vec()))
        })
        .collect();
    assert_eq!(
        expected.get(&body(3)),
        Some(&int64(-5)),
        "fixture: the override changed the value"
    );
    assert!(
        !expected.contains_key(&body(4)) && !expected.contains_key(&body(5)),
        "fixture"
    );
    assert!(
        expected.contains_key(&body(2000)),
        "fixture: the unblocked row carries the property"
    );

    let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let visited = s.scan_column_range_with(
        &prefix(),
        &[],
        None,
        1,
        u64::MAX,
        usize::MAX,
        &mut |b, v| got.push((b.to_vec(), v.to_vec())),
    );
    assert_eq!(
        visited,
        Some(expected.len() as u64),
        "one hand-over per live entry"
    );
    let got_map: BTreeMap<Vec<u8>, Vec<u8>> = got.iter().cloned().collect();
    assert_eq!(
        got.len(),
        got_map.len(),
        "no key handed over twice from one segment"
    );
    assert_eq!(got_map, expected);
    // The collecting scan is the visitor plus a sort: byte-identical.
    let collected: BTreeMap<Vec<u8>, Vec<u8>> = s
        .scan_column_at(&prefix(), &[], 1, u64::MAX)
        .into_iter()
        .collect();
    assert_eq!(collected, expected);
    // The budget aborts the visit; exactly `budget` entries are allowed.
    let mut n = 0u64;
    assert_eq!(
        s.scan_column_range_with(&prefix(), &[], None, 1, u64::MAX, 10, &mut |_, _| n += 1),
        None
    );
    assert_eq!(n, 10, "the visitor stops at the budget, not before");
    let mut m = 0u64;
    assert_eq!(
        s.scan_column_range_with(
            &prefix(),
            &[],
            None,
            1,
            u64::MAX,
            expected.len(),
            &mut |_, _| m += 1
        ),
        Some(expected.len() as u64)
    );
    // A range restricts the walk to the bodies in [lo, hi).
    let mut in_range: Vec<Vec<u8>> = Vec::new();
    s.scan_column_range_with(
        &prefix(),
        &body(10),
        Some(&body(14)),
        1,
        u64::MAX,
        usize::MAX,
        &mut |b, _| in_range.push(b.to_vec()),
    );
    assert_eq!(in_range, vec![body(10), body(11), body(12), body(13)]);
}

#[test]
fn a_projected_get_reads_only_the_requested_columns_of_a_block_row() {
    use engram_store::Projected;
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();
    // A block row: only the requested columns, ascending by id, nothing
    // assembled — and a property the row lacks is simply absent.
    let got = s.get_projected(&prefix(), &body(3), &[2, 1, 9]);
    assert_eq!(
        got,
        Some(Projected::Columns(vec![
            (1, int64(3)),
            (2, string("name-3"))
        ]))
    );
    assert_eq!(
        s.get_projected(&prefix(), &body(3), &[2]),
        Some(Projected::Columns(vec![(2, string("name-3"))]))
    );
    // The same key through the full get decodes to the same values.
    let full = Record::decode(&s.get(&prefix(), &body(3)).expect("live")).expect("decode");
    assert_eq!(full.get(PropertyId(1)), Some(int64(3).as_slice()));
    // Row form (the tail rows never blocked): the record's bytes.
    let tail = s.get_projected(&prefix(), &body(1000), &[7]);
    assert_eq!(tail, Some(Projected::Record(tail_record(0))));
    // A later write overrides the block row; a delete is gone.
    let mut changed = Record::new();
    changed.set(PropertyId(1), int64(-3));
    s.put(&prefix(), &body(3), StoredValue::Plain(changed.encode()))
        .expect("put");
    assert_eq!(
        s.get_projected(&prefix(), &body(3), &[1]),
        Some(Projected::Record(changed.encode()))
    );
    s.delete(&prefix(), &body(4));
    assert_eq!(s.get_projected(&prefix(), &body(4), &[1]), None);
    assert_eq!(s.get_projected(&prefix(), &body(77777), &[1]), None);
}

#[test]
fn a_point_get_probes_the_block_the_index_names_not_every_block() {
    // Two signatures with DISJOINT id ranges compact into two blocks; a
    // get of a head key must probe one block, and the index — not a walk
    // over every block — must be what picks it.
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    for i in 0..COLUMNAR_MIN_ROWS as u32 {
        let mut r = Record::new();
        r.set(PropertyId(7), int64(i64::from(i)));
        r.set(PropertyId(8), string("other"));
        s.put(&prefix(), &body(5000 + i), StoredValue::Plain(r.encode()))
            .expect("put");
    }
    s.seal().expect("seal");
    s.compact();
    let (v, t) = engram_observe::with_trace(|| s.get(&prefix(), &body(3)));
    assert!(v.is_some(), "the head key is live");
    assert_eq!(
        t.counters().get("store.block probes"),
        Some(&1),
        "one probe for a key held by one of two blocks"
    );
    let (v, t) = engram_observe::with_trace(|| s.get(&prefix(), &body(5003)));
    assert!(v.is_some());
    assert_eq!(t.counters().get("store.block probes"), Some(&1));
    // A key no block holds probes nothing.
    let (v, t) = engram_observe::with_trace(|| s.get(&prefix(), &body(99_999)));
    assert!(v.is_none());
    assert_eq!(t.counters().get("store.block probes"), None);
}

#[test]
fn counting_blocked_rows_assembles_no_record() {
    // A count over compacted rows touches keys only: no block row's record
    // is assembled (counted). The value visitor still assembles them — the
    // counter is what separates the two paths.
    let s = Store::new();
    populate(&s, COLUMNAR_MIN_ROWS as u32);
    s.seal().expect("seal");
    s.compact();
    let (n, t) = engram_observe::with_trace(|| s.count_at(&prefix(), &[], u64::MAX));
    assert_eq!(n, COLUMNAR_MIN_ROWS as u64 + 3);
    assert_eq!(
        t.counters().get("store.block rows assembled"),
        None,
        "{:?}",
        t.counters()
    );
    let (bodies, t) = engram_observe::with_trace(|| s.scan_bodies_prefix(&prefix(), &[]));
    assert_eq!(bodies.len(), COLUMNAR_MIN_ROWS + 3);
    assert_eq!(t.counters().get("store.block rows assembled"), None);
    // Bodies come back FULL, in key order, identical to the value scan's.
    let full: Vec<Vec<u8>> = s
        .scan_body_prefix(&prefix(), &[])
        .into_iter()
        .map(|(b, _)| b)
        .collect();
    assert_eq!(bodies, full);
    let (_, t) =
        engram_observe::with_trace(|| s.for_each_span(&prefix(), &[], u64::MAX, &mut |_, _| true));
    assert_eq!(
        t.counters().get("store.block rows assembled"),
        Some(&(COLUMNAR_MIN_ROWS as u64)),
        "the value visitor assembles every blocked row"
    );
}
