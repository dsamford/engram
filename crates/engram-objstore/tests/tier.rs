#![allow(non_snake_case)]
//! The tier seam — taxonomy, memory semantics, and the presence guard.

use engram_objstore::{
    FaultPlan, FaultTier, MemoryTier, ObjectName, ObjectTier, TierError, classify_status,
    resumable_put,
};
use engram_runtime::{Runtime, SimRuntime};

fn run_async<F: std::future::Future<Output = ()> + 'static>(seed: u64, f: F) {
    let rt = SimRuntime::new(seed);
    rt.spawn(f);
    rt.run(10_000_000).expect("completes");
}

fn seg(fill: u8) -> ObjectName {
    ObjectName::Segment { hash: [fill; 32] }
}

// ─── Classification: the fail-closed rule ───────────────────────────────────

#[test]
fn unrecognised_status_is_UNAVAILABLE_never_notfound() {
    // The fatal direction: a proxy's 599 read as NotFound tells the caller
    // the object does not exist, and the caller drops its reference.
    for status in [200, 301, 400, 401, 403, 418, 500, 502, 599] {
        match classify_status(status) {
            TierError::Unavailable { .. } => {}
            other => panic!("status {status} classified {other:?}, not Unavailable"),
        }
    }
    assert_eq!(classify_status(404), TierError::NotFound);
    assert_eq!(classify_status(410), TierError::NotFound);
    assert_eq!(classify_status(429), TierError::Throttled);
    assert_eq!(classify_status(503), TierError::Throttled);
}

// ─── Memory tier semantics — what every adapter is held to ──────────────────

#[test]
fn put_get_head_round_trip() {
    run_async(1, async {
        let tier = MemoryTier::new();
        tier.put(&seg(1), b"hello".to_vec()).await.expect("put");
        assert_eq!(tier.get(&seg(1)).await.expect("get"), b"hello");
        assert_eq!(tier.head(&seg(1)).await.expect("head"), 5);
        assert_eq!(tier.get(&seg(2)).await.unwrap_err(), TierError::NotFound);
        assert_eq!(tier.head(&seg(2)).await.unwrap_err(), TierError::NotFound);
    });
}

#[test]
fn identical_reput_is_idempotent_and_a_differing_one_REFUSES() {
    run_async(2, async {
        let tier = MemoryTier::new();
        tier.put(&seg(1), b"same".to_vec()).await.expect("put");
        tier.put(&seg(1), b"same".to_vec())
            .await
            .expect("identical re-put is success");
        // Different bytes under one content name: one side's hash is wrong,
        // and overwriting would let the corruption pick the winner.
        assert!(matches!(
            tier.put(&seg(1), b"DIFFERENT".to_vec()).await.unwrap_err(),
            TierError::Corrupt { .. }
        ));
        assert_eq!(
            tier.get(&seg(1)).await.expect("get"),
            b"same",
            "the original survives"
        );
    });
}

#[test]
fn beacon_listing_is_numeric_and_only_beacons() {
    run_async(3, async {
        let tier = MemoryTier::new();
        tier.put(&ObjectName::Beacon { seq: 7 }, b"b7".to_vec())
            .await
            .expect("put");
        tier.put(&ObjectName::Beacon { seq: 40 }, b"b40".to_vec())
            .await
            .expect("put");
        tier.put(&seg(9), b"not a beacon".to_vec())
            .await
            .expect("put");
        let mut seqs = tier.list_beacons().await.expect("list");
        seqs.sort_unstable();
        assert_eq!(seqs, vec![7, 40]);
    });
}

#[test]
fn beacon_names_sort_lexicographically_as_numbers() {
    // The zero-padding IS the discovery order; an unpadded 9 would sort
    // above 40 and rewind every restore that trusted the listing's max name.
    let a = ObjectName::Beacon { seq: 9 }.path();
    let b = ObjectName::Beacon { seq: 40 }.path();
    assert!(a < b, "{a} must sort below {b}");
}

// ─── The resumable put, and the fail-open default it guards ─────────────────

#[test]
fn resumable_put_uploads_only_the_missing_chunks() {
    run_async(4, async {
        let tier = MemoryTier::new();
        tier.put(&seg(1), b"one".to_vec())
            .await
            .expect("pre-existing");
        let report = resumable_put(
            &tier,
            &[
                (seg(1), b"one".to_vec()),
                (seg(2), b"two".to_vec()),
                (seg(3), b"three".to_vec()),
            ],
        )
        .await
        .expect("resume");
        assert_eq!(report.already_present, 1);
        assert_eq!(report.uploaded, 2);
        assert_eq!(tier.get(&seg(3)).await.expect("uploaded"), b"three");
    });
}

#[test]
fn an_under_answered_presence_query_is_INDETERMINATE_not_present() {
    // Lore's proto: results are index-correlated and FOUND_IN_CONTEXT = 0 is
    // the default, so a short answer READS AS PRESENT for the chunks it says
    // nothing about — and the upload skips exactly the chunks that were never
    // stored. The guard refuses the answer instead of reading it.
    run_async(5, async {
        let plan = FaultPlan {
            under_answer: 255,
            ..FaultPlan::none()
        };
        let tier = FaultTier::new(MemoryTier::new(), plan, 99);
        let err = resumable_put(
            &tier,
            &[(seg(1), b"one".to_vec()), (seg(2), b"two".to_vec())],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, TierError::Indeterminate { .. }),
            "got {err:?}"
        );
        assert_eq!(
            tier.inner().object_count(),
            0,
            "nothing was uploaded on a refused answer"
        );
    });
}

#[test]
fn the_violating_control_a_padded_shortfall_LOSES_a_chunk() {
    // The control for the guard above: a tier that pads its shortfall with
    // "present" (the proto-default reading) makes resumable_put skip a chunk
    // that was NEVER STORED — observable as absence after a reported success.
    // If this test ever stops losing the chunk, the guard's premise changed
    // and the guard needs re-deriving.
    struct PaddingTier(MemoryTier);
    impl ObjectTier for PaddingTier {
        async fn put(&self, n: &ObjectName, b: Vec<u8>) -> Result<(), TierError> {
            self.0.put(n, b).await
        }
        async fn get(&self, n: &ObjectName) -> Result<Vec<u8>, TierError> {
            self.0.get(n).await
        }
        async fn head(&self, n: &ObjectName) -> Result<usize, TierError> {
            self.0.head(n).await
        }
        async fn list_beacons(&self) -> Result<Vec<u64>, TierError> {
            self.0.list_beacons().await
        }
        async fn present(&self, names: &[ObjectName]) -> Result<Vec<bool>, TierError> {
            let mut a = self.0.present(names).await?;
            a.pop();
            a.push(true); // the fail-open reading: unanswered = FOUND
            Ok(a)
        }
    }
    run_async(6, async {
        let tier = PaddingTier(MemoryTier::new());
        let report = resumable_put(
            &tier,
            &[(seg(1), b"one".to_vec()), (seg(2), b"two".to_vec())],
        )
        .await
        .expect("the padded answer LOOKS complete, so the put succeeds");
        assert_eq!(
            report.already_present, 1,
            "the phantom chunk read as present"
        );
        assert_eq!(
            tier.0.get(&seg(2)).await.unwrap_err(),
            TierError::NotFound,
            "and it was never uploaded — this is the data loss the length guard exists to stop"
        );
    });
}

// ─── The fault tier is deterministic ────────────────────────────────────────

#[test]
fn fault_injection_is_deterministic_per_seed() {
    let outcomes = |seed: u64| {
        let mut got: Vec<bool> = Vec::new();
        run_async_collect(seed, &mut got);
        got
    };
    fn run_async_collect(seed: u64, out: &mut Vec<bool>) {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let c = collected.clone();
        let rt = SimRuntime::new(1);
        rt.spawn(async move {
            let plan = FaultPlan {
                unavailable: 128,
                ..FaultPlan::none()
            };
            let tier = FaultTier::new(MemoryTier::new(), plan, seed);
            for i in 0..16u8 {
                let r = tier
                    .put(&ObjectName::Segment { hash: [i; 32] }, vec![i])
                    .await;
                c.borrow_mut().push(r.is_ok());
            }
        });
        rt.run(10_000_000).expect("completes");
        *out = collected.borrow().clone();
    }
    let a = outcomes(1234);
    let b = outcomes(1234);
    let c = outcomes(5678);
    assert_eq!(a, b, "same seed, same faults");
    assert_ne!(
        a, c,
        "different seed, different faults (16 ops at p=0.5 collide with p 2^-16)"
    );
}
