#![allow(non_snake_case)]
//! The root beacon and the restore drill — discovery that refuses to rewind.

use engram_objstore::{
    DiscoverError, Discovered, FaultPlan, FaultTier, MemoryTier, ObjectName, ObjectTier,
    RootBeacon, TierError, advance_root, discover_root,
};
use engram_runtime::{Runtime, SimRuntime};

fn run_async<F: std::future::Future<Output = ()> + 'static>(seed: u64, f: F) {
    let rt = SimRuntime::new(seed);
    rt.spawn(f);
    rt.run(10_000_000).expect("completes");
}

fn beacon(seq: u64, fill: u8) -> RootBeacon {
    RootBeacon {
        seq,
        manifest: [fill; 32],
    }
}

// ─── The beacon format ──────────────────────────────────────────────────────

#[test]
fn a_beacon_round_trips() {
    let b = beacon(42, 7);
    let decoded = RootBeacon::decode(&b.encode(), 42).expect("decodes");
    assert_eq!(decoded, b);
}

#[test]
fn a_truncated_or_tampered_beacon_REFUSES() {
    let bytes = beacon(42, 7).encode();
    // Truncated.
    assert!(matches!(
        RootBeacon::decode(&bytes[..bytes.len() - 1], 42),
        Err(TierError::Corrupt { .. })
    ));
    // Zero-padded to the right length — the proto-default shape.
    let mut padded = bytes[..bytes.len() - 1].to_vec();
    padded.push(0);
    assert!(matches!(
        RootBeacon::decode(&padded, 42),
        Err(TierError::Corrupt { .. })
    ));
    // One flipped manifest byte.
    let mut tampered = bytes.clone();
    tampered[12] ^= 1;
    assert!(matches!(
        RootBeacon::decode(&tampered, 42),
        Err(TierError::Corrupt { .. })
    ));
}

#[test]
fn a_beacon_under_the_wrong_name_REFUSES() {
    // A stale payload copied under a fresh name is the rewind wearing a
    // disguise: the payload self-check passes, so the NAME check is the only
    // thing standing.
    let bytes = beacon(42, 7).encode();
    assert!(matches!(
        RootBeacon::decode(&bytes, 43),
        Err(TierError::Corrupt { .. })
    ));
}

// ─── The restore drill ──────────────────────────────────────────────────────

#[test]
fn a_fresh_site_discovers_the_newest_root_from_the_bucket_alone() {
    run_async(1, async {
        let bucket = MemoryTier::new();
        for seq in 1..=5 {
            advance_root(&bucket, beacon(seq, seq as u8))
                .await
                .expect("advance");
        }
        // Cluster loss: a fresh site holds a handle to the BUCKET and nothing
        // else. Its floor comes from the replicated commit-log stream.
        let fresh = bucket.fresh_site();
        let found = discover_root(&fresh, Some(5)).await.expect("discover");
        assert_eq!(found, Discovered::Root(beacon(5, 5)));
    });
}

#[test]
fn an_empty_bucket_with_no_floor_is_FRESH_and_with_a_floor_is_STALE() {
    run_async(2, async {
        let bucket = MemoryTier::new();
        // Genuinely new site: nothing anywhere says a root ever existed.
        assert_eq!(
            discover_root(&bucket, None).await.expect("fresh"),
            Discovered::FreshBucket
        );
        // But if the commit-log stream says advance 9 happened, an empty
        // listing is not freshness — it is a lost bucket, and reading it as
        // fresh silently discards nine advances of history.
        match discover_root(&bucket, Some(9)).await {
            Err(DiscoverError::Stale {
                found: None,
                floor: 9,
            }) => {}
            other => panic!("expected the stale refusal, got {other:?}"),
        }
    });
}

#[test]
fn a_truncated_listing_below_the_floor_REFUSES_instead_of_rewinding() {
    run_async(3, async {
        let bucket = MemoryTier::new();
        for seq in 1..=5 {
            advance_root(&bucket, beacon(seq, seq as u8))
                .await
                .expect("advance");
        }
        // The listing loses its largest entry — eventual consistency, a sweep,
        // a short page. Discovery sees newest=4 against floor=5.
        let plan = FaultPlan {
            truncate_listing: 255,
            ..FaultPlan::none()
        };
        let flaky = FaultTier::new(bucket, plan, 7);
        match discover_root(&flaky, Some(5)).await {
            Err(DiscoverError::Stale {
                found: Some(4),
                floor: 5,
            }) => {}
            other => panic!("expected the stale refusal, got {other:?}"),
        }
    });
}

#[test]
fn a_listed_but_unfetchable_newest_beacon_is_INDETERMINATE_never_the_next_oldest() {
    // MemoryTier's listing is derived from its object map, so the
    // listing/store disagreement (an eventually-consistent bucket, a listing
    // page older than a sweep) needs a wrapper that OVER-lists.
    struct OverListing(MemoryTier);
    impl ObjectTier for OverListing {
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
            let mut seqs = self.0.list_beacons().await?;
            seqs.push(2); // the listing still promises the swept beacon
            Ok(seqs)
        }
        async fn present(&self, n: &[ObjectName]) -> Result<Vec<bool>, TierError> {
            self.0.present(n).await
        }
    }
    run_async(4, async {
        let bucket = MemoryTier::new();
        advance_root(&bucket, beacon(1, 1)).await.expect("advance");
        // Stepping down to beacon 1 would be the rewind with extra steps; the
        // answer is "cannot determine", loudly.
        match discover_root(&OverListing(bucket), Some(1)).await {
            Err(DiscoverError::Tier(TierError::Indeterminate { .. })) => {}
            other => panic!("expected indeterminate, got {other:?}"),
        }
    });
}

#[test]
fn discovery_without_a_floor_still_verifies_what_it_finds() {
    run_async(5, async {
        let bucket = MemoryTier::new();
        // A corrupt beacon planted at the newest name.
        bucket
            .put(&ObjectName::Beacon { seq: 3 }, vec![0u8; 72])
            .await
            .expect("plant");
        match discover_root(&bucket, None).await {
            Err(DiscoverError::Tier(TierError::Corrupt { .. })) => {}
            other => panic!("expected corrupt, got {other:?}"),
        }
    });
}
