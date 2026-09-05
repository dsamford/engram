#![allow(non_snake_case)]
//! MVCC semantics and the protected-KIND gate.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoreError, StoredValue};

fn prefix(kind: Kind) -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind,
        partition: Partition(1),
    }
}

// ─── The protected-KIND gate ────────────────────────────────────────────────

#[test]
fn a_plaintext_put_to_a_protected_KIND_is_refused() {
    let s = Store::new();
    let err = s.put(
        &prefix(Kind::PROTECTED_PROPERTY),
        b"k",
        StoredValue::Plain(b"secret".to_vec()),
    );
    assert_eq!(
        err,
        Err(StoreError::ProtectedKindPlaintext { kind_byte: 0x80 })
    );
    // And NOTHING landed — a refusal that still wrote would be worse than none.
    assert_eq!(s.get(&prefix(Kind::PROTECTED_PROPERTY), b"k"), None);
}

#[test]
fn an_UNKNOWN_protected_block_kind_is_refused_too() {
    // The forward-compatibility direction: 0x9A has never been assigned, and a
    // build that admitted plaintext under it would make a newer build's
    // protected KIND leak on every older node in a rolling deploy.
    let s = Store::new();
    let err = s.put(
        &prefix(Kind::from_byte(0x9A)),
        b"k",
        StoredValue::Plain(vec![1]),
    );
    assert_eq!(
        err,
        Err(StoreError::ProtectedKindPlaintext { kind_byte: 0x9A })
    );
}

#[test]
fn a_sealed_put_to_a_protected_KIND_lands() {
    let s = Store::new();
    s.put(
        &prefix(Kind::PROTECTED_PROPERTY),
        b"k",
        StoredValue::Sealed(vec![9, 9]),
    )
    .expect("sealed put lands");
    assert_eq!(
        s.get(&prefix(Kind::PROTECTED_PROPERTY), b"k"),
        Some(vec![9, 9])
    );
    assert_eq!(
        s.is_sealed(&prefix(Kind::PROTECTED_PROPERTY), b"k"),
        Some(true)
    );
}

#[test]
fn plain_puts_to_unprotected_KINDs_are_ordinary() {
    let s = Store::new();
    s.put(&prefix(Kind::NODE), b"k", StoredValue::Plain(vec![1]))
        .expect("lands");
    assert_eq!(s.is_sealed(&prefix(Kind::NODE), b"k"), Some(false));
}

#[test]
fn an_invalid_kind_is_refused_before_anything_else() {
    let s = Store::new();
    assert_eq!(
        s.put(
            &prefix(Kind::from_byte(0x00)),
            b"k",
            StoredValue::Plain(vec![])
        ),
        Err(StoreError::InvalidKind(0x00)),
    );
}

// ─── MVCC ───────────────────────────────────────────────────────────────────

#[test]
fn READ_COMMITTED_get_returns_the_newest_committed_value() {
    let s = Store::new();
    let p = prefix(Kind::NODE);
    s.put(&p, b"k", StoredValue::Plain(vec![1])).expect("v1");
    s.put(&p, b"k", StoredValue::Plain(vec![2])).expect("v2");
    assert_eq!(s.get(&p, b"k"), Some(vec![2]));
}

#[test]
fn snapshot_reads_are_OPT_IN_and_see_the_past() {
    // Snapshot as the default is the L3 bug; as an explicit request it is a
    // feature. `get_at` never becomes the default read path.
    let s = Store::new();
    let p = prefix(Kind::NODE);
    let t1 = s.put(&p, b"k", StoredValue::Plain(vec![1])).expect("v1");
    let t2 = s.put(&p, b"k", StoredValue::Plain(vec![2])).expect("v2");

    assert_eq!(s.get_at(&p, b"k", t1), Some(vec![1]));
    assert_eq!(s.get_at(&p, b"k", t2), Some(vec![2]));
    assert_eq!(
        s.get_at(&p, b"k", t1 - 1),
        None,
        "before the first write there was nothing"
    );
}

#[test]
fn a_delete_is_a_tombstone_not_an_erasure() {
    let s = Store::new();
    let p = prefix(Kind::NODE);
    let t1 = s.put(&p, b"k", StoredValue::Plain(vec![1])).expect("v1");
    let t2 = s.delete(&p, b"k");

    assert_eq!(s.get(&p, b"k"), None, "the current read sees absence");
    // A reader positioned before the tombstone still sees the value — which is
    // what makes deletion safe under concurrent snapshot readers.
    assert_eq!(s.get_at(&p, b"k", t1), Some(vec![1]));
    assert_eq!(s.get_at(&p, b"k", t2), None);
    // And the sealed question about a tombstone is "no value", not false.
    assert_eq!(s.is_sealed(&p, b"k"), None);
}

#[test]
fn commit_timestamps_are_strictly_monotonic() {
    // The commit clock is a counter, not wall time: wall time can jump
    // backwards, and a commit stamp that goes backwards mints a key that sorts
    // as newer than reality. This pins the stamp source.
    let s = Store::new();
    let p = prefix(Kind::NODE);
    let mut last = 0;
    for i in 0..10u8 {
        let ts = s.put(&p, &[i], StoredValue::Plain(vec![i])).expect("lands");
        assert!(ts > last, "ts {ts} did not advance past {last}");
        last = ts;
    }
}

#[test]
fn keys_differing_only_in_partition_do_not_collide() {
    // A guard against logical-key construction bugs: the prefix components
    // must all survive into the identity, or two tenants' rows merge.
    let s = Store::new();
    let a = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    };
    let b = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(2),
    };
    s.put(&a, b"k", StoredValue::Plain(vec![1])).expect("a");
    s.put(&b, b"k", StoredValue::Plain(vec![2])).expect("b");
    assert_eq!(s.get(&a, b"k"), Some(vec![1]));
    assert_eq!(s.get(&b, b"k"), Some(vec![2]));
}

#[test]
fn keys_differing_only_in_BODY_do_not_collide() {
    // Found by a canary: dropping the body from the logical key merged every
    // entity in a partition into one version chain, and NO test noticed —
    // every existing test used either one body or distinct partitions. The
    // partition test above covers the prefix half; this covers the body half,
    // and together they pin that the WHOLE key is the identity.
    let s = Store::new();
    let p = prefix(Kind::NODE);
    s.put(&p, b"a", StoredValue::Plain(vec![1])).expect("a");
    s.put(&p, b"b", StoredValue::Plain(vec![2])).expect("b");
    assert_eq!(s.get(&p, b"a"), Some(vec![1]));
    assert_eq!(s.get(&p, b"b"), Some(vec![2]));
}
