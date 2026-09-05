#![allow(non_snake_case)]
//! Range scans — the L2 layout property exercised through the read path.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn p(realm: u32, part: u32) -> KeyPrefix {
    KeyPrefix {
        realm: Realm(realm),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(part),
    }
}

#[test]
fn a_scan_returns_bodies_ascending_with_current_values() {
    let s = Store::new();
    for (body, v) in [(b"c", 3u8), (b"a", 1), (b"b", 2)] {
        s.put(&p(1, 1), body, StoredValue::Plain(vec![v]))
            .expect("w");
    }
    // Overwrite one so the scan must resolve versions, not first-writes.
    s.put(&p(1, 1), b"b", StoredValue::Plain(vec![9]))
        .expect("w2");

    let rows = s.scan(&p(1, 1));
    assert_eq!(
        rows,
        vec![
            (b"a".to_vec(), vec![1]),
            (b"b".to_vec(), vec![9]),
            (b"c".to_vec(), vec![3]),
        ],
    );
}

#[test]
fn a_scan_NEVER_leaves_its_partition_realm_or_kind() {
    // The containment claim, tested through the read path. The neighbours are
    // planted at the exact adjacent coordinates a boundary bug would leak:
    // partition±1, the next realm, the next kind.
    let s = Store::new();
    s.put(&p(1, 1), b"mine", StoredValue::Plain(vec![1]))
        .expect("w");
    s.put(&p(1, 0), b"below", StoredValue::Plain(vec![2]))
        .expect("w");
    s.put(&p(1, 2), b"above", StoredValue::Plain(vec![3]))
        .expect("w");
    s.put(&p(2, 1), b"other-realm", StoredValue::Plain(vec![4]))
        .expect("w");
    let edge_prefix = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::EDGE,
        partition: Partition(1),
    };
    s.put(&edge_prefix, b"an-edge", StoredValue::Plain(vec![5]))
        .expect("w");

    let rows = s.scan(&p(1, 1));
    assert_eq!(
        rows,
        vec![(b"mine".to_vec(), vec![1])],
        "the scan leaked past its prefix"
    );
}

#[test]
fn a_tombstoned_key_is_ABSENT_from_the_scan() {
    // Absent, not present-with-empty-value: an empty confident row is worse
    // than a missing one — the incumbent's `?? []` lesson, at the scan layer.
    let s = Store::new();
    s.put(&p(1, 1), b"live", StoredValue::Plain(vec![1]))
        .expect("w");
    s.put(&p(1, 1), b"dead", StoredValue::Plain(vec![2]))
        .expect("w");
    s.delete(&p(1, 1), b"dead");

    let rows = s.scan(&p(1, 1));
    assert_eq!(rows, vec![(b"live".to_vec(), vec![1])]);
}

#[test]
fn a_snapshot_scan_sees_the_PAST_including_resurrections() {
    let s = Store::new();
    let t1 = s
        .put(&p(1, 1), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    let t2 = s.delete(&p(1, 1), b"k");
    let t3 = s
        .put(&p(1, 1), b"k", StoredValue::Plain(vec![3]))
        .expect("v3");

    assert_eq!(s.scan_at(&p(1, 1), t1), vec![(b"k".to_vec(), vec![1])]);
    assert_eq!(
        s.scan_at(&p(1, 1), t2),
        vec![],
        "at the tombstone the key is gone"
    );
    assert_eq!(s.scan_at(&p(1, 1), t3), vec![(b"k".to_vec(), vec![3])]);
}

#[test]
fn a_scan_merges_tail_and_segments_with_the_tail_winning() {
    let s = Store::new();
    s.put(&p(1, 1), b"sealed-only", StoredValue::Plain(vec![1]))
        .expect("w");
    s.put(&p(1, 1), b"shadowed", StoredValue::Plain(vec![2]))
        .expect("w");
    s.seal().expect("seal");
    s.put(&p(1, 1), b"shadowed", StoredValue::Plain(vec![9]))
        .expect("w2");
    s.put(&p(1, 1), b"tail-only", StoredValue::Plain(vec![3]))
        .expect("w3");

    let rows = s.scan(&p(1, 1));
    assert_eq!(
        rows,
        vec![
            (b"sealed-only".to_vec(), vec![1]),
            (b"shadowed".to_vec(), vec![9]),
            (b"tail-only".to_vec(), vec![3]),
        ],
    );
}

#[test]
fn a_tail_tombstone_hides_a_SEALED_value_in_the_scan() {
    // The scan-shaped version of the shadowing test: the newest fact is a
    // tombstone in the tail, the value lives in a segment. A scan that unions
    // instead of resolving resurrects the row.
    let s = Store::new();
    s.put(&p(1, 1), b"k", StoredValue::Plain(vec![1]))
        .expect("w");
    s.seal().expect("seal");
    s.delete(&p(1, 1), b"k");

    assert_eq!(
        s.scan(&p(1, 1)),
        vec![],
        "the sealed value leaked past the tail tombstone"
    );
}

#[test]
fn a_scan_of_an_empty_prefix_is_an_empty_RESULT() {
    let s = Store::new();
    s.put(&p(1, 2), b"elsewhere", StoredValue::Plain(vec![1]))
        .expect("w");
    assert_eq!(s.scan(&p(1, 1)), vec![]);
}

#[test]
fn scans_survive_seal_and_compaction_unchanged() {
    let s = Store::new();
    for i in 0..10u8 {
        s.put(&p(1, 1), &[i], StoredValue::Plain(vec![i]))
            .expect("w");
    }
    let before = s.scan(&p(1, 1));
    s.seal().expect("seal");
    let after_seal = s.scan(&p(1, 1));
    s.compact();
    let after_compact = s.scan(&p(1, 1));
    assert_eq!(before, after_seal);
    assert_eq!(before, after_compact);
    assert_eq!(before.len(), 10);
}

#[test]
fn scan_body_prefix_returns_exactly_the_matching_rows_with_FULL_bodies() {
    // The bounded scan the adjacency rows were shaped for: O(matches), and
    // the body comes back whole (prefix included), not stripped.
    let store = Store::new();
    let p = p(1, 1);
    for (body, v) in [
        (b"O\x01aaa".to_vec(), b"in".to_vec()),
        (b"O\x01bbb".to_vec(), b"in".to_vec()),
        (b"O\x02ccc".to_vec(), b"out-other-node".to_vec()),
        (b"I\x01ddd".to_vec(), b"out-other-tag".to_vec()),
    ] {
        store.put(&p, &body, StoredValue::Plain(v)).expect("put");
    }
    let rows = store.scan_body_prefix(&p, b"O\x01");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|(b, _)| b.starts_with(b"O\x01")),
        "only the prefix's rows"
    );
    assert!(
        rows.iter().any(|(b, _)| b == b"O\x01aaa"),
        "bodies are FULL"
    );
    // The empty prefix is the whole partition — identical to scan().
    assert_eq!(store.scan_body_prefix(&p, b"").len(), store.scan(&p).len());
    // And a prefix past everything is empty, not an error.
    assert!(store.scan_body_prefix(&p, b"Z").is_empty());
}

#[test]
fn the_visitor_scan_agrees_with_scan_at_on_a_mixed_store() {
    // Tail rows, two sealed generations, columnar blocks, tombstones and
    // overrides — the visitor must visit EXACTLY scan_at's pairs, in order,
    // and stop early on demand.
    use engram_key::value::Tag;
    use engram_store::record::{PropertyId, Record};
    let s = Store::new();
    let int64 = |v: i64| -> Vec<u8> {
        let mut out = vec![Tag::INT64.byte()];
        out.extend_from_slice(&v.to_le_bytes());
        out
    };
    let rec = |v: i64| -> Vec<u8> {
        let mut r = Record::new();
        r.set(PropertyId(1), int64(v));
        r.encode()
    };
    for i in 0..200u32 {
        s.put(
            &p(1, 9),
            &i.to_be_bytes(),
            StoredValue::Plain(rec(i64::from(i))),
        )
        .expect("put");
    }
    s.seal().expect("seal");
    s.compact(); // blocks form
    for i in 200..260u32 {
        s.put(
            &p(1, 9),
            &i.to_be_bytes(),
            StoredValue::Plain(rec(i64::from(i))),
        )
        .expect("put");
    }
    s.seal().expect("seal");
    for i in (0..40u32).step_by(2) {
        s.put(&p(1, 9), &i.to_be_bytes(), StoredValue::Plain(rec(-1)))
            .expect("override");
    }
    for i in 100..120u32 {
        s.delete(&p(1, 9), &i.to_be_bytes());
    }
    let expected = s.scan_at(&p(1, 9), u64::MAX);
    let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let n = s.for_each_span(&p(1, 9), &[], u64::MAX, &mut |b, v| {
        got.push((b.to_vec(), v.to_vec()));
        true
    });
    assert_eq!(got, expected, "identical pairs in identical order");
    assert_eq!(n as usize, expected.len());
    assert_eq!(s.count_at(&p(1, 9), &[], u64::MAX) as usize, expected.len());
    // Early stop.
    let mut seen = 0usize;
    s.for_each_span(&p(1, 9), &[], u64::MAX, &mut |_, _| {
        seen += 1;
        seen < 10
    });
    assert_eq!(seen, 10, "false stops the walk");
}
