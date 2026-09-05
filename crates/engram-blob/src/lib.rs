//! M6 — large media, referenced not embedded (R9).
//!
//! The graph stores a [`BlobRef`] (~hundreds of bytes). The bytes live in an
//! external content-addressed store. Between them sits the MANIFEST ENTRY —
//! an MVCC row in the engine's own transactional KV domain
//! ([`engram_key::Kind::BLOB_MANIFEST`]) owning the refcount, locator,
//! verification state and key pointer.
//!
//! **The split that matters, in the schema and not a comment:**
//!
//! - `node → manifest entry` is ENGINE-ENFORCED — transactional, exact, zero
//!   object-store I/O ([`manifest::add_ref`] / [`manifest::remove_ref`]).
//! - `manifest entry → bytes` is MEASURED, never enforced — an object-store
//!   outage must not fail a graph write. It lives in
//!   [`manifest::VerifyState`] + `verified_at` + `verified_coverage`, and
//!   `NotAttempted` is a distinct state that never reads as `Intact`.
//!
//! Convergent encryption is REJECTED (fatal, not a trade-off: a key derived
//! from the plaintext is a key nothing can destroy — crypto-shredding and
//! message-locked keys are mutually exclusive). Instead: per-object random
//! DEK, wrapped under the realm's sealer, dedup at the manifest — two
//! references to one content within one tenant resolve to one entry, one
//! object, one DEK, refcount 2.

#![forbid(unsafe_code)]

pub mod chunks;
pub mod gc;
pub mod manifest;

pub use chunks::{ChunkError, OpenReport, SealedBlob, open_range, seal_chunked};
pub use gc::{Liveness, SweepPlan, UnlinkReport, plan_sweep, process_tombstones};
pub use manifest::{
    ManifestEntry, ManifestError, RemoveOutcome, RestoreTargetNotEmpty, VerifyState, acquire_lease,
    add_ref, assert_restore_target_empty, manifest_prefix, read_entry, remove_ref,
    tombstone_prefix,
};

use engram_observe::{Canary, Gate, Registration, Subsystem};

/// The content key: BLAKE3 of the PLAINTEXT content. Dedup keys on it within
/// one realm; it never appears in a sort-ordered key position outside the
/// manifest's own (protected-by-construction) domain.
pub type ContentKey = [u8; 32];

/// Compute a content key.
pub fn content_key(plaintext: &[u8]) -> ContentKey {
    *blake3::hash(plaintext).as_bytes()
}

/// What the GRAPH stores — the reference half of the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRef {
    /// The content key — the manifest entry's identity.
    pub content_key: ContentKey,
    /// Logical size in bytes.
    pub size: u64,
    /// Which tier holds the bytes.
    pub tier: Tier,
}

/// R9's tiering. The boundaries are DECISIONS with their reasons attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// ≤ 4 KiB: inline in the node record. 4 KiB and not larger because the
    /// record already carries a 4 KiB vector — the inline budget is contended.
    T0Inline,
    /// 4 KiB – 1 MiB: key-value-separated in the store's own MVCC domain
    /// ([`engram_key::Kind::BLOB_T1`]). The CRDT-snapshot tier: mutable-by-
    /// replacement + high churn is exactly what content addressing turns into
    /// unbounded garbage, so T1 and not T2.
    T1Engine,
    /// Above 1 MiB: external content-addressed store. The boundary is
    /// PROVISIONAL — the per-object round-trip cost driving it is an estimate,
    /// and M6's kill criterion re-derives it from measurement. The COUNT rule
    /// rides with it: T2 is for objects that are large, not merely numerous —
    /// ten million 200 KB thumbnails belong in T1 regardless.
    T2External,
}

/// T0's upper bound, inclusive.
pub const T0_MAX: u64 = 4096;
/// T1's upper bound, inclusive — provisional, see [`Tier::T2External`].
pub const T1_MAX: u64 = 1_048_576;

/// Place a blob by size. Size alone — the count rule is an operator decision
/// this function cannot see, which is why the tier is recorded on the
/// [`BlobRef`] rather than re-derived at read time.
pub fn place(size: u64) -> Tier {
    if size <= T0_MAX {
        Tier::T0Inline
    } else if size <= T1_MAX {
        Tier::T1Engine
    } else {
        Tier::T2External
    }
}

/// The idempotency job key: content-addressed input × versioned transform ⇒
/// deterministic output address ⇒ **a re-run is a lookup, not work**. No job
/// table, no second source of truth.
pub fn job_key(
    content: &ContentKey,
    pipeline_version: u32,
    model_id_at_version: &str,
    seg_params_hash: &[u8; 32],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(content);
    h.update(&pipeline_version.to_be_bytes());
    // Length-prefixed so (model "ab", params starting "c…") cannot collide
    // with (model "a", params starting "bc…").
    h.update(&(model_id_at_version.len() as u32).to_be_bytes());
    h.update(model_id_at_version.as_bytes());
    h.update(seg_params_hash);
    *h.finalize().as_bytes()
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The blob layer, as a registered subsystem.
pub struct BlobLayer;

impl Subsystem for BlobLayer {
    const NAME: &'static str = "blob";

    fn register() -> Registration {
        Registration::new()
            .crash_point("blob.between_decrement_and_tombstone")
            .crash_point("blob.between_unlink_and_dequeue")
            .sometimes("blob.dedup hit")
            .sometimes("blob.chunk refused")
            .sometimes("blob.gc aborted on unknown")
            .sometimes("blob.lease kept a zero-ref blob")
            .sometimes("blob.tombstone spared a resurrected blob")
            .counter("blob.refs added")
            .counter("blob.refs removed")
            .counter("blob.tombstones enqueued")
            .counter("blob.tombstones cleared")
            .counter("blob.chunks sealed")
            .counter("blob.chunks opened")
            .counter("blob.sweeps planned")
            .gate(
                Gate::new(
                    "UNKNOWN aborts the sweep, never skips",
                    Canary::new("skip unknowns and assert the partitioned tenant's blob lands in the delete list"),
                ),
            )
            .gate(
                Gate::new(
                    "every crash window leaks, none dangles",
                    Canary::new("unlink before the resurrect re-check and assert a re-added blob dangles"),
                )
                .and_canary(Canary::new("crash between decrement and tombstone and assert the mark scan still finds the leak")),
            )
            .gate(
                Gate::new(
                    "a chunk is immovable and a truncation is loud",
                    Canary::new("drop the position from the binding and assert two swapped chunks open"),
                )
                .and_canary(Canary::new("drop the count from the binding and assert a truncated blob reads clean")),
            )
            .gate(
                Gate::new(
                    "liveness includes the lease",
                    Canary::new("ignore leases in remove_ref and assert an in-flight blob is tombstoned"),
                ),
            )
            .gate(
                Gate::new(
                    "a restore requires an empty target",
                    Canary::new("skip the emptiness precondition and assert a restore into a live store reads as success"),
                ),
            )
    }
}
