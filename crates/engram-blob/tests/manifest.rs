#![allow(non_snake_case)]
//! The manifest — refcounts, dedup, leases, tombstone ordering, restore.

use engram_blob::{
    ManifestError, Tier, VerifyState, acquire_lease, add_ref, assert_restore_target_empty,
    content_key, manifest_prefix, read_entry, remove_ref, tombstone_prefix,
};
use engram_key::{Namespace, Partition, Realm};
use engram_runtime::{Runtime, SimRuntime};
use engram_store::Store;

fn run_async<F: std::future::Future<Output = ()> + 'static>(seed: u64, f: F) {
    let rt = SimRuntime::new(seed);
    rt.spawn(f);
    rt.run(10_000_000).expect("completes");
}

fn prefixes() -> (engram_key::KeyPrefix, engram_key::KeyPrefix) {
    (
        manifest_prefix(Realm(1), Namespace(1), Partition(1)),
        tombstone_prefix(Realm(1), Namespace(1), Partition(1)),
    )
}

#[test]
fn add_ref_creates_then_DEDUPS_onto_one_entry_and_one_dek() {
    run_async(1, async {
        let store = Store::new();
        let (mp, _) = prefixes();
        let key = content_key(b"the same bytes");

        let rc = add_ref(
            &store,
            &mp,
            &key,
            14,
            Tier::T1Engine,
            None,
            b"wrapped-A".to_vec(),
        )
        .await
        .expect("first ref");
        assert_eq!(rc, 1);
        // The second reference arrives with ITS OWN wrapped DEK — which must
        // be discarded: one content, one object, one DEK.
        let rc = add_ref(
            &store,
            &mp,
            &key,
            14,
            Tier::T1Engine,
            None,
            b"wrapped-B".to_vec(),
        )
        .await
        .expect("dedup ref");
        assert_eq!(rc, 2);
        let e = read_entry(&store, &mp, &key).expect("entry");
        assert_eq!(e.refcount, 2);
        assert_eq!(
            e.wrapped_dek, b"wrapped-A",
            "the FIRST DEK survives the dedup"
        );
        assert_eq!(
            e.verify,
            VerifyState::NotAttempted,
            "nothing measured is nothing measured"
        );
    });
}

#[test]
fn a_dedup_with_a_disagreeing_size_REFUSES() {
    run_async(2, async {
        let store = Store::new();
        let (mp, _) = prefixes();
        let key = content_key(b"content");
        add_ref(&store, &mp, &key, 7, Tier::T1Engine, None, vec![1])
            .await
            .expect("first");
        match add_ref(&store, &mp, &key, 8, Tier::T1Engine, None, vec![2]).await {
            Err(ManifestError::SizeMismatch {
                existing: 7,
                claimed: 8,
            }) => {}
            other => panic!("expected the size refusal, got {other:?}"),
        }
    });
}

#[test]
fn the_last_remove_ref_enqueues_a_tombstone_and_earlier_ones_do_not() {
    run_async(3, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"content");
        add_ref(&store, &mp, &key, 7, Tier::T1Engine, None, vec![1])
            .await
            .expect("ref");
        add_ref(&store, &mp, &key, 7, Tier::T1Engine, None, vec![1])
            .await
            .expect("ref");

        let out = remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        assert_eq!((out.remaining, out.tombstoned), (1, false));
        assert_eq!(store.scan(&tp).len(), 0, "a live blob is never queued");

        let out = remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        assert_eq!((out.remaining, out.tombstoned), (0, true));
        assert_eq!(
            store.scan(&tp).len(),
            1,
            "the LAST reference queues the unlink"
        );
    });
}

#[test]
fn a_live_lease_suppresses_the_tombstone() {
    // The read handle IS the lease: a 50 GiB upload whose transaction commits
    // 90 seconds later is unreferenced BY DESIGN in between. Collecting it
    // would present as "the embedder is flaky".
    run_async(4, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"in flight");
        add_ref(
            &store,
            &mp,
            &key,
            9,
            Tier::T2External,
            Some([9; 32]),
            vec![1],
        )
        .await
        .expect("ref");
        acquire_lease(&store, &mp, &key, 500).await.expect("lease");

        let out = remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        assert_eq!(
            (out.remaining, out.tombstoned),
            (0, false),
            "zero refs but leased"
        );
        assert_eq!(store.scan(&tp).len(), 0);
        assert!(read_entry(&store, &mp, &key).expect("entry").live(100));
        assert!(
            !read_entry(&store, &mp, &key).expect("entry").live(600),
            "expired = not live"
        );
    });
}

#[test]
fn a_lease_extends_and_never_shortens() {
    run_async(5, async {
        let store = Store::new();
        let (mp, _) = prefixes();
        let key = content_key(b"x");
        add_ref(&store, &mp, &key, 1, Tier::T1Engine, None, vec![])
            .await
            .expect("ref");
        acquire_lease(&store, &mp, &key, 900).await.expect("lease");
        acquire_lease(&store, &mp, &key, 400)
            .await
            .expect("shorter lease");
        assert_eq!(
            read_entry(&store, &mp, &key).expect("entry").lease_expiry,
            900,
            "a later, shorter lease must not shorten an earlier one"
        );
    });
}

#[test]
fn a_crash_between_decrement_and_tombstone_LEAKS_and_never_dangles() {
    // The window's failure shape is the acceptable one: refcount 0, no
    // tombstone — a leak the mark scan finds. The unacceptable shape (bytes
    // gone while a reference lives) is unreachable from this ordering.
    let store = Store::new();
    let (mp, tp) = prefixes();
    let key = content_key(b"doomed");
    let (s, m, t) = (store.clone(), mp, tp);
    let crashed =
        engram_observe::with_crash_at("blob.between_decrement_and_tombstone", move || {
            let rt = SimRuntime::new(6);
            rt.spawn(async move {
                add_ref(&s, &m, &key, 3, Tier::T1Engine, None, vec![])
                    .await
                    .expect("ref");
                let _ = remove_ref(&s, &m, &t, &key, 0).await;
            });
            let _ = rt.run(10_000_000);
        });
    assert!(crashed.is_err(), "the crash point must fire");
    let e = read_entry(&store, &mp, &key).expect("entry survives");
    assert_eq!(e.refcount, 0, "the decrement landed");
    assert_eq!(
        store.scan(&tp).len(),
        0,
        "the tombstone did not — this is the LEAK"
    );
    // And the leak is FINDABLE: a zero-ref, lease-free entry with no queue row
    // is exactly what the mark phase enumerates.
    assert!(!e.live(0));
}

#[test]
fn concurrent_add_refs_count_exactly() {
    // Two writers CAS-ing one entry: the count must be exact, not
    // last-writer-wins — this is the engine-enforced half being enforced.
    let store = Store::new();
    let (mp, _) = prefixes();
    let key = content_key(b"contended");
    let rt = SimRuntime::new(7);
    for _ in 0..8 {
        let s = store.clone();
        rt.spawn(async move {
            add_ref(&s, &mp, &key, 5, Tier::T1Engine, None, vec![0])
                .await
                .expect("ref");
        });
    }
    rt.run(50_000_000).expect("completes");
    assert_eq!(read_entry(&store, &mp, &key).expect("entry").refcount, 8);
}

#[test]
fn a_restore_into_a_live_store_REFUSES_up_front() {
    run_async(8, async {
        let store = Store::new();
        let (mp, _) = prefixes();
        assert!(
            assert_restore_target_empty(&store, &mp).is_ok(),
            "empty target passes"
        );
        add_ref(
            &store,
            &mp,
            &content_key(b"live"),
            1,
            Tier::T1Engine,
            None,
            vec![],
        )
        .await
        .expect("ref");
        let err = assert_restore_target_empty(&store, &mp).unwrap_err();
        assert_eq!(err.rows, 1);
    });
}

#[test]
fn missing_entries_refuse_by_name() {
    run_async(9, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"never added");
        assert!(matches!(
            read_entry(&store, &mp, &key),
            Err(ManifestError::Missing)
        ));
        assert!(matches!(
            remove_ref(&store, &mp, &tp, &key, 0).await,
            Err(ManifestError::Missing)
        ));
        assert!(matches!(
            acquire_lease(&store, &mp, &key, 10).await,
            Err(ManifestError::Missing)
        ));
    });
}
