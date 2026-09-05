#![allow(non_snake_case)]
//! Sealing — the property is that it changes WHERE data lives and nothing a
//! reader can observe. Every test here reads the same answers before and after
//! a seal, because "the flush changed my query results" is a storage bug that
//! presents as an application bug three layers up.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_runtime::Runtime as _;
use engram_store::{Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

#[test]
fn a_seal_is_invisible_to_READ_COMMITTED_reads() {
    let s = Store::new();
    s.put(&prefix(), b"a", StoredValue::Plain(vec![1]))
        .expect("a");
    s.put(&prefix(), b"b", StoredValue::Plain(vec![2]))
        .expect("b");

    let before: Vec<_> = [s.get(&prefix(), b"a"), s.get(&prefix(), b"b")].to_vec();
    assert_eq!(s.seal(), Some(0));
    let after: Vec<_> = [s.get(&prefix(), b"a"), s.get(&prefix(), b"b")].to_vec();

    assert_eq!(before, after, "a seal changed what a reader sees");
    assert_eq!(s.segment_count(), 1);
}

#[test]
fn a_snapshot_read_still_sees_the_past_AFTER_the_flush() {
    // The reason versions seal WITH the data. Collapse-on-seal would make
    // every reader positioned before the flush see the future — the L3
    // violation reintroduced by the layer underneath a correct MVCC.
    let s = Store::new();
    let t1 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    let t2 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![2]))
        .expect("v2");

    s.seal().expect("seals");

    assert_eq!(
        s.get_at(&prefix(), b"k", t1),
        Some(vec![1]),
        "the past vanished in the flush"
    );
    assert_eq!(s.get_at(&prefix(), b"k", t2), Some(vec![2]));
    assert_eq!(s.get_at(&prefix(), b"k", t1 - 1), None);
}

#[test]
fn a_tombstone_survives_the_seal() {
    // Dropping tombstones on seal resurrects the value underneath — deletion
    // silently undone by a memory-pressure event nobody asked for.
    let s = Store::new();
    let t1 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    s.delete(&prefix(), b"k");

    s.seal().expect("seals");

    assert_eq!(
        s.get(&prefix(), b"k"),
        None,
        "the delete was undone by the seal"
    );
    assert_eq!(
        s.get_at(&prefix(), b"k", t1),
        Some(vec![1]),
        "…but the past is intact"
    );
}

#[test]
fn writes_after_a_seal_SHADOW_the_segment() {
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    s.seal().expect("seals");
    s.put(&prefix(), b"k", StoredValue::Plain(vec![2]))
        .expect("v2");

    assert_eq!(
        s.get(&prefix(), b"k"),
        Some(vec![2]),
        "the tail must shadow the segment"
    );
}

#[test]
fn a_delete_after_a_seal_hides_the_sealed_value() {
    // The order that catches shadowing bugs the other direction: the newest
    // fact is a tombstone in the TAIL, the value lives in a SEGMENT. A read
    // that consults segments first — or treats tail-miss as miss — resurrects.
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    s.seal().expect("seals");
    s.delete(&prefix(), b"k");

    assert_eq!(
        s.get(&prefix(), b"k"),
        None,
        "the sealed value leaked past the newer tombstone"
    );
}

#[test]
fn versions_split_across_seals_resolve_in_timestamp_order() {
    // Three generations, three homes: segment 0, segment 1, tail. Every
    // timestamp must resolve to its own generation — this is the test that
    // fails if the segment WALK order or the stop-at-first-hit rule is wrong.
    let s = Store::new();
    let t1 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v1");
    s.seal().expect("first seal");
    let t2 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![2]))
        .expect("v2");
    s.seal().expect("second seal");
    let t3 = s
        .put(&prefix(), b"k", StoredValue::Plain(vec![3]))
        .expect("v3");

    assert_eq!(s.segment_count(), 2);
    assert_eq!(s.get_at(&prefix(), b"k", t1), Some(vec![1]));
    assert_eq!(s.get_at(&prefix(), b"k", t2), Some(vec![2]));
    assert_eq!(s.get_at(&prefix(), b"k", t3), Some(vec![3]));
    assert_eq!(s.get(&prefix(), b"k"), Some(vec![3]));
}

#[test]
fn sealing_an_empty_tail_is_refused() {
    // A segment of nothing exists only to be walked past on every read.
    let s = Store::new();
    assert_eq!(s.seal(), None);
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v");
    assert_eq!(s.seal(), Some(0));
    // Immediately sealing again: the tail is empty again.
    assert_eq!(s.seal(), None);
    assert_eq!(s.segment_count(), 1);
}

#[test]
fn cas_reads_THROUGH_the_seal() {
    // The guard must compare against the current committed value wherever it
    // lives. A CAS that only consults the tail sees ABSENT after a flush, and
    // an expect-absent create then succeeds twice — the lost-update bug
    // reintroduced by the storage layer.
    let rt = engram_runtime::SimRuntime::new(2);
    let store = Store::new();
    store
        .put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("seed");
    store.seal().expect("seals");

    let outcome = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let store = store.clone();
        let outcome = outcome.clone();
        rt.spawn(async move {
            // Expect-absent must FAIL: the value exists, in a segment.
            let r = store
                .cas(&prefix(), b"k", None, StoredValue::Plain(vec![9]))
                .await;
            *outcome.borrow_mut() = Some(r);
        });
    }
    rt.run(10_000).expect("completes");

    match outcome.borrow().as_ref().expect("ran") {
        Err(engram_store::StoreError::CasMismatch { current }) => {
            assert_eq!(
                current.as_deref(),
                Some([1u8].as_slice()),
                "the guard saw through the seal"
            );
        }
        other => panic!("expect-absent must mismatch against a sealed value, got {other:?}"),
    }
}

#[test]
fn is_sealed_answers_from_a_segment_too() {
    let s = Store::new();
    s.put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("v");
    s.seal().expect("seals");
    assert_eq!(s.is_sealed(&prefix(), b"k"), Some(false));
}
