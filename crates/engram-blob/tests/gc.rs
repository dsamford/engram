#![allow(non_snake_case)]
//! GC — UNKNOWN aborts, the unlink ordering, and the resurrection guard.

use engram_blob::{
    Liveness, SweepPlan, Tier, add_ref, content_key, manifest_prefix, plan_sweep,
    process_tombstones, read_entry, remove_ref, tombstone_prefix,
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

// ─── The mark phase ─────────────────────────────────────────────────────────

#[test]
fn a_fully_answered_mark_deletes_exactly_the_dead() {
    let a = content_key(b"a");
    let b = content_key(b"b");
    let c = content_key(b"c");
    match plan_sweep(&[
        (a, Liveness::Live),
        (b, Liveness::Dead),
        (c, Liveness::Live),
    ]) {
        SweepPlan::Deletes(keys) => assert_eq!(keys, vec![b]),
        other => panic!("expected deletes, got {other:?}"),
    }
}

#[test]
fn ONE_unknown_aborts_the_whole_sweep() {
    // The injected-partition shape: tenant B's shard is unreachable, so every
    // blob whose references live there answers Unknown. Skipping them and
    // proceeding would delete tenant B's data for being down. The abort
    // deletes NOTHING — including the honestly-dead candidates, because a
    // sweep that half-runs is a sweep whose next run has different inputs.
    let dead = content_key(b"genuinely dead");
    let partitioned = content_key(b"tenant B's blob");
    match plan_sweep(&[(dead, Liveness::Dead), (partitioned, Liveness::Unknown)]) {
        SweepPlan::Aborted { unknowns } => assert_eq!(unknowns, 1),
        SweepPlan::Deletes(keys) => {
            panic!(
                "the sweep proceeded past an unknown and would delete {}",
                keys.len()
            )
        }
    }
}

#[test]
fn an_empty_mark_is_an_empty_delete_list_not_an_abort() {
    match plan_sweep(&[]) {
        SweepPlan::Deletes(keys) => assert!(keys.is_empty()),
        other => panic!("expected empty deletes, got {other:?}"),
    }
}

// ─── The unlink worker ──────────────────────────────────────────────────────

#[test]
fn the_worker_unlinks_dequeues_and_drops_the_entry() {
    run_async(1, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"bytes");
        add_ref(&store, &mp, &key, 4, Tier::T1Engine, None, b"dek".to_vec())
            .await
            .expect("ref");
        remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        assert_eq!(store.scan(&tp).len(), 1);

        let mut unlinked_keys = Vec::new();
        let report = process_tombstones(&store, &mp, &tp, 100, |k| {
            unlinked_keys.push(*k);
            Ok(())
        });
        assert_eq!((report.unlinked, report.spared, report.deferred), (1, 0, 0));
        assert_eq!(unlinked_keys, vec![key]);
        assert_eq!(store.scan(&tp).len(), 0, "dequeued");
        assert!(
            store.get(&mp, &key).is_none(),
            "the entry — and its wrapped DEK — die here"
        );
    });
}

#[test]
fn a_resurrected_blob_is_SPARED_and_its_tombstone_cleared() {
    // add_ref between enqueue and unlink revives the entry. Unlinking anyway
    // would dangle every new reference — the re-check is the guard.
    run_async(2, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"lazarus");
        add_ref(&store, &mp, &key, 4, Tier::T1Engine, None, vec![1])
            .await
            .expect("ref");
        remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        assert_eq!(store.scan(&tp).len(), 1, "queued");
        // The resurrection.
        add_ref(&store, &mp, &key, 4, Tier::T1Engine, None, vec![2])
            .await
            .expect("re-ref");

        let report = process_tombstones(&store, &mp, &tp, 100, |_| {
            panic!("unlink must not be called for a live entry")
        });
        assert_eq!((report.unlinked, report.spared, report.deferred), (0, 1, 0));
        assert_eq!(store.scan(&tp).len(), 0, "the stale tombstone is cleared");
        assert_eq!(read_entry(&store, &mp, &key).expect("entry").refcount, 1);
    });
}

#[test]
fn an_unlink_refusal_DEFERS_the_tombstone_to_the_next_run() {
    // An object-store outage must not fail the worker — and must not lose the
    // queue row, which is the only thing that guarantees a retry.
    run_async(3, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"unreachable bucket");
        add_ref(
            &store,
            &mp,
            &key,
            4,
            Tier::T2External,
            Some([7; 32]),
            vec![],
        )
        .await
        .expect("ref");
        remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");

        let report =
            process_tombstones(&store, &mp, &tp, 100, |_| Err("bucket unavailable".into()));
        assert_eq!((report.unlinked, report.spared, report.deferred), (0, 0, 1));
        assert_eq!(
            store.scan(&tp).len(),
            1,
            "still queued — the retry is the queue row"
        );
        assert!(
            store.get(&mp, &key).is_some(),
            "the entry survives an unlink refusal"
        );

        // The next run, bucket back: cleared.
        let report = process_tombstones(&store, &mp, &tp, 100, |_| Ok(()));
        assert_eq!(report.unlinked, 1);
        assert_eq!(store.scan(&tp).len(), 0);
    });
}

#[test]
fn a_crash_between_unlink_and_dequeue_RERUNS_idempotently() {
    // The crash window's shape: bytes gone, queue row still there. The next
    // run re-unlinks (idempotent — "already gone" is success) and dequeues.
    // The reverse order would leave bytes nothing will ever collect while the
    // queue believes them gone.
    let store = Store::new();
    let (mp, tp) = prefixes();
    let key = content_key(b"crash here");
    let unlinks = std::rc::Rc::new(std::cell::RefCell::new(0usize));

    let (s, u) = (store.clone(), unlinks.clone());
    let crashed = engram_observe::with_crash_at("blob.between_unlink_and_dequeue", move || {
        let rt = SimRuntime::new(4);
        let (s2, u2) = (s.clone(), u.clone());
        rt.spawn(async move {
            add_ref(&s2, &mp, &key, 4, Tier::T1Engine, None, vec![])
                .await
                .expect("ref");
            remove_ref(&s2, &mp, &tp, &key, 100).await.expect("remove");
            let _ = process_tombstones(&s2, &mp, &tp, 100, |_| {
                *u2.borrow_mut() += 1;
                Ok(())
            });
        });
        let _ = rt.run(10_000_000);
    });
    assert!(crashed.is_err(), "the crash point must fire");
    assert_eq!(*unlinks.borrow(), 1, "the unlink ran");
    assert_eq!(
        store.scan(&tp).len(),
        1,
        "the dequeue did NOT — the row survives the crash"
    );

    // The re-run: unlink again (idempotent), then clear.
    let report = process_tombstones(&store, &mp, &tp, 100, |_| {
        *unlinks.borrow_mut() += 1;
        Ok(())
    });
    assert_eq!(report.unlinked, 1);
    assert_eq!(
        *unlinks.borrow(),
        2,
        "re-unlinked — which is why unlink must be idempotent"
    );
    assert_eq!(store.scan(&tp).len(), 0);
}

#[test]
fn a_leased_zero_ref_entry_is_spared_by_the_worker_too() {
    // Belt and braces with remove_ref's suppression: a tombstone that somehow
    // exists for a leased entry (enqueued before the lease, say) must still
    // be spared at unlink time — liveness is re-checked at the LAST moment.
    run_async(5, async {
        let store = Store::new();
        let (mp, tp) = prefixes();
        let key = content_key(b"leased");
        add_ref(&store, &mp, &key, 4, Tier::T1Engine, None, vec![])
            .await
            .expect("ref");
        // Tombstone first (no lease yet)…
        remove_ref(&store, &mp, &tp, &key, 100)
            .await
            .expect("remove");
        // …then the lease arrives (an embedder opened a read handle).
        engram_blob::acquire_lease(&store, &mp, &key, 900)
            .await
            .expect("lease");

        let report = process_tombstones(&store, &mp, &tp, 100, |_| {
            panic!("unlink must not run under a live lease")
        });
        assert_eq!((report.unlinked, report.spared), (0, 1));
    });
}
