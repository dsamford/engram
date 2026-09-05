#![allow(non_snake_case)]
//! Sealed segments on the tier — the placement rule and the isolation gate.

use engram_crypto::{Sealer, Secret, derive_dek};
use engram_key::Realm;
use engram_objstore::{
    FaultPlan, FaultTier, FetchError, MemoryTier, ObjectName, ObjectTier, Placement, PolicyRefused,
    SegmentKind, StoreRefused, TierError, TierPolicy,
    seal::{fetch_sealed, store_sealed},
};
use engram_runtime::{Runtime, SimRuntime};

fn run_async<F: std::future::Future<Output = ()> + 'static>(seed: u64, f: F) {
    let rt = SimRuntime::new(seed);
    rt.spawn(f);
    rt.run(10_000_000).expect("completes");
}

const MASTER: [u8; 32] = [0x5A; 32];

fn policy() -> TierPolicy {
    TierPolicy::new(Placement::Tier1, Placement::Tier0Pinned).expect("valid policy")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

// ─── The round trip ─────────────────────────────────────────────────────────

#[test]
fn a_sealed_segment_round_trips_through_the_tier() {
    run_async(1, async {
        let tier = MemoryTier::new();
        let dek = derive_dek(&MASTER, Realm(3));
        let mut sealer = Sealer::new(derive_dek(&MASTER, Realm(3)), 0);
        let plain = Secret::new(b"segment payload".to_vec());

        let name = store_sealed(&tier, &policy(), SegmentKind::Plain, &mut sealer, &plain)
            .await
            .expect("store");
        let back = fetch_sealed(&tier, &dek, &name).await.expect("fetch");
        assert_eq!(back.expose(), b"segment payload");
    });
}

#[test]
fn a_flipped_byte_is_CORRUPT_and_the_bytes_are_withheld() {
    run_async(2, async {
        let tier = MemoryTier::new();
        let dek = derive_dek(&MASTER, Realm(3));
        let mut sealer = Sealer::new(derive_dek(&MASTER, Realm(3)), 0);
        let name = store_sealed(
            &tier,
            &policy(),
            SegmentKind::Plain,
            &mut sealer,
            &Secret::new(b"payload".to_vec()),
        )
        .await
        .expect("store");

        let plan = FaultPlan {
            flip_byte: 255,
            ..FaultPlan::none()
        };
        let flaky = FaultTier::new(tier, plan, 11);
        match fetch_sealed(&flaky, &dek, &name).await {
            Err(FetchError::Tier(TierError::Corrupt { .. })) => {}
            other => panic!("expected corrupt, got {other:?}"),
        }
    });
}

#[test]
fn the_wrong_realms_key_cannot_open_a_fetched_segment() {
    run_async(3, async {
        let tier = MemoryTier::new();
        let mut sealer_a = Sealer::new(derive_dek(&MASTER, Realm(1)), 0);
        let name = store_sealed(
            &tier,
            &policy(),
            SegmentKind::Plain,
            &mut sealer_a,
            &Secret::new(b"tenant A's rows".to_vec()),
        )
        .await
        .expect("store");

        let dek_b = derive_dek(&MASTER, Realm(2));
        match fetch_sealed(&tier, &dek_b, &name).await {
            Err(FetchError::Sealed(_)) => {}
            other => panic!("expected the sealed refusal, got {other:?}"),
        }
    });
}

// ─── The placement rule ─────────────────────────────────────────────────────

#[test]
fn a_policy_routing_vectors_to_tier_1_REFUSES_TO_EXIST() {
    // The boot-refusing gate: the config is invalid, so there is no window
    // where a running system holds it.
    assert_eq!(
        TierPolicy::new(Placement::Tier1, Placement::Tier1).unwrap_err(),
        PolicyRefused::VectorsOnTier1
    );
}

#[test]
fn a_vector_segment_handed_to_the_uploader_is_refused_BEFORE_any_byte_moves() {
    run_async(4, async {
        let tier = MemoryTier::new();
        let mut sealer = Sealer::new(derive_dek(&MASTER, Realm(1)), 0);
        match store_sealed(
            &tier,
            &policy(),
            SegmentKind::Vector,
            &mut sealer,
            &Secret::new(b"vectors".to_vec()),
        )
        .await
        {
            Err(StoreRefused::PinnedToTier0) => {}
            other => panic!("expected the pin refusal, got {other:?}"),
        }
        assert_eq!(tier.object_count(), 0, "nothing was uploaded");
        assert_eq!(
            sealer.next_nonce(),
            0,
            "and no nonce was spent on a refused store"
        );
    });
}

// ─── The crash point ────────────────────────────────────────────────────────

#[test]
fn a_crash_between_seal_and_put_loses_the_upload_not_a_reference() {
    // The order is seal → name → CRASH WINDOW → put. A crash here loses an
    // upload nothing referenced (recovery re-uploads idempotently by content
    // name); the reverse order would publish a name the bucket cannot serve.
    let tier = MemoryTier::new();
    let t = tier.clone();
    let crashed = engram_observe::with_crash_at("objstore.between_seal_and_put", move || {
        run_async(5, async move {
            let mut sealer = Sealer::new(derive_dek(&MASTER, Realm(1)), 0);
            let _ = store_sealed(
                &t,
                &policy(),
                SegmentKind::Plain,
                &mut sealer,
                &Secret::new(b"doomed".to_vec()),
            )
            .await;
            panic!("unreachable: the crash point fires first");
        });
    });
    assert!(crashed.is_err(), "the armed crash point must fire");
    assert_eq!(
        tier.object_count(),
        0,
        "the crash lost the UPLOAD — the bucket holds nothing"
    );
}

// ─── The isolation kill-test (M5.5's kill criterion) ────────────────────────

#[test]
fn the_tier_holds_zero_plaintext_bytes_of_any_tenant() {
    // Tenant B's marker, stored through the protected path; then the tier's
    // ENTIRE byte surface is scanned. Zero hits required.
    run_async(6, async {
        let marker = b"TENANT-B-MARKER-7f3a9c";
        let tier = MemoryTier::new();
        let mut sealer_b = Sealer::new(derive_dek(&MASTER, Realm(2)), 0);
        store_sealed(
            &tier,
            &policy(),
            SegmentKind::Plain,
            &mut sealer_b,
            &Secret::new(marker.to_vec()),
        )
        .await
        .expect("store");

        assert!(
            !contains(&tier.all_bytes(), marker),
            "tenant B's plaintext marker is readable in the bucket"
        );

        // THE MANDATORY VIOLATING CONTROL: the same marker stored PLAIN must
        // be found, or the scanner is dead and the zero above is unmeasured.
        tier.put(&ObjectName::Segment { hash: [0xEE; 32] }, marker.to_vec())
            .await
            .expect("the violating put");
        assert!(
            contains(&tier.all_bytes(), marker),
            "the scanner cannot find a PLAIN marker — the zero above proved nothing"
        );
    });
}
