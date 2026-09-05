//! Sealed segments on the object tier, and the placement rule.
//!
//! # Nothing leaves tier 0 in plaintext
//!
//! A tier-1 bucket is somebody else's disk. Every segment is sealed under its
//! realm's DEK (M5's seam) BEFORE it is named, so the content name commits to
//! the ciphertext and the bucket never holds a byte a bucket operator can
//! read. The isolation kill-test greps the tier's raw bytes for a marker —
//! with the mandatory violating control, because a scanner that cannot find a
//! plain marker proves nothing by finding no sealed one.
//!
//! # The placement rule (R11), from a real object-store's published limits
//!
//! ~750 requests/sec per bucket-or-IP and 256 concurrent sessions per source
//! IP; 4 GB of vectors at ≥1 MiB granules is ~3,906 GETs per full scan —
//! 5.2 s, the whole bucket budget for ONE query. So: **vector segments never
//! go to tier 1**, enforced twice with distinct semantics: [`TierPolicy::new`]
//! REFUSES TO CONSTRUCT a config routing vectors there (the boot-refusing
//! gate), and [`store_sealed`] refuses any segment whose placement is not
//! tier 1 (reachable through the legitimate tier-0-pinned placement, so both
//! guards are independently observable — the crypto seam's masked-canary
//! lesson applied at design time).

use engram_crypto::{Dek, OpenError, Sealer, Secret};
use engram_observe::{counted, crash_point, sometimes};

use crate::{ObjectName, ObjectTier, TierError};

/// What kind of rows a segment holds — the input to placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Ordinary sealed rows.
    Plain,
    /// Vector index segments — scan-hot, latency-critical.
    Vector,
}

/// Where a segment kind is allowed to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Stays on local NVMe (or fully cache-resident). Never uploaded.
    Tier0Pinned,
    /// Sealed and uploaded to the object tier.
    Tier1,
}

/// Why a policy refused to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyRefused {
    /// The config routed vector segments to tier 1 — the boot-refusing gate.
    VectorsOnTier1,
}

impl std::fmt::Display for PolicyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyRefused::VectorsOnTier1 => write!(
                f,
                "vector segments may not be placed on tier 1 — a full scan there costs the whole bucket budget"
            ),
        }
    }
}

impl std::error::Error for PolicyRefused {}

/// The placement policy. Constructing one validates it, so a policy that
/// exists is a policy the rule holds for.
#[derive(Debug, Clone, Copy)]
pub struct TierPolicy {
    plain: Placement,
    vector: Placement,
}

impl TierPolicy {
    /// A policy, or the boot refusal.
    pub fn new(plain: Placement, vector: Placement) -> Result<TierPolicy, PolicyRefused> {
        if vector == Placement::Tier1 {
            return Err(PolicyRefused::VectorsOnTier1);
        }
        Ok(TierPolicy { plain, vector })
    }

    /// Where this kind lives.
    pub fn placement(&self, kind: SegmentKind) -> Placement {
        match kind {
            SegmentKind::Plain => self.plain,
            SegmentKind::Vector => self.vector,
        }
    }
}

/// Why a store was refused before any byte moved.
#[derive(Debug)]
pub enum StoreRefused {
    /// This kind's placement is not tier 1 — it stays local. Not an error in
    /// the policy; an error in the CALL.
    PinnedToTier0,
    /// The tier refused.
    Tier(TierError),
}

impl std::fmt::Display for StoreRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreRefused::PinnedToTier0 => {
                write!(
                    f,
                    "this segment kind is pinned to tier 0 and never uploaded"
                )
            }
            StoreRefused::Tier(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreRefused {}

/// Seal a segment's bytes and store them content-named on the tier.
///
/// The name is the BLAKE3 of the SEALED bytes — computed after sealing, so the
/// name commits to what the bucket actually holds and any fetcher can verify
/// without a key. Returns the name; the caller owns writing it into the
/// manifest (and [`crate::advance_root`] owns making that durable).
pub async fn store_sealed<T: ObjectTier>(
    tier: &T,
    policy: &TierPolicy,
    kind: SegmentKind,
    sealer: &mut Sealer,
    plain: &Secret,
) -> Result<ObjectName, StoreRefused> {
    if policy.placement(kind) != Placement::Tier1 {
        sometimes!("objstore.store refused a mis-tiered segment", true);
        return Err(StoreRefused::PinnedToTier0);
    }
    let envelope = sealer.seal(plain);
    let hash = *blake3::hash(&envelope).as_bytes();
    let name = ObjectName::Segment { hash };
    // A crash HERE loses an upload the manifest never referenced — recovery
    // re-uploads idempotently by content name. The other order (publish the
    // name, then upload) turns a crash into a dangling manifest reference,
    // which is why the crash point sits on this side.
    crash_point("objstore.between_seal_and_put");
    tier.put(&name, envelope)
        .await
        .map_err(StoreRefused::Tier)?;
    counted!("objstore.segments stored");
    Ok(name)
}

/// Why a fetch failed.
#[derive(Debug)]
pub enum FetchError {
    /// The tier refused (including `Corrupt` for a hash mismatch — the bytes
    /// were NOT returned).
    Tier(TierError),
    /// The envelope did not authenticate under this key — wrong realm,
    /// tampered, or truncated; ONE refusal on purpose, exactly as
    /// [`engram_crypto::open`] states it.
    Sealed(OpenError),
    /// The name is not a segment name.
    NotASegment,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Tier(e) => write!(f, "{e}"),
            FetchError::Sealed(e) => write!(f, "{e}"),
            FetchError::NotASegment => write!(f, "not a segment name"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Fetch a sealed segment: get, VERIFY THE CONTENT NAME, then open.
///
/// The hash check runs before the key is even consulted — corrupt bytes never
/// reach the AEAD, and a caller never receives bytes that fail either check.
pub async fn fetch_sealed<T: ObjectTier>(
    tier: &T,
    dek: &Dek,
    name: &ObjectName,
) -> Result<Secret, FetchError> {
    let ObjectName::Segment { hash } = name else {
        return Err(FetchError::NotASegment);
    };
    let bytes = tier.get(name).await.map_err(FetchError::Tier)?;
    if blake3::hash(&bytes).as_bytes() != hash {
        sometimes!("objstore.get refused corrupt bytes", true);
        return Err(FetchError::Tier(TierError::Corrupt {
            detail: "fetched bytes do not match their content name".into(),
        }));
    }
    let plain = engram_crypto::open(dek, &bytes).map_err(FetchError::Sealed)?;
    counted!("objstore.segments fetched");
    Ok(plain)
}
