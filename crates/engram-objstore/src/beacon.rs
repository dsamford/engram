//! The manifest-root beacon — the structural hole R11 closes FIRST.
//!
//! Content-named immutable objects + reconciliation-only listing means that
//! after cluster loss, nothing lets a fresh site discover the current manifest
//! root: every correctness property in the storage layer is downstream of a
//! pointer that would otherwise exist in exactly one place, on deliberately
//! unreplicated local NVMe. So the root ships two ways — in the commit-log
//! replica stream (the LOCAL FLOOR a discoverer carries in), and as a
//! monotonically-named beacon written to object storage on every advance.
//!
//! # Discovery refuses to rewind
//!
//! The dangerous failure is not "no root found"; it is an OLDER root found and
//! believed — a truncated listing, a swept beacon, an eventually-consistent
//! bucket — which silently rewinds the database to an earlier manifest while
//! everything reports success. Discovery therefore takes the local floor and
//! REFUSES any answer below it, and a listed-but-unfetchable newest beacon is
//! `Indeterminate`, never "fall back to the next-oldest".

use engram_observe::{counted, sometimes};

use crate::{ObjectName, ObjectTier, TierError};

/// One advance of the manifest root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootBeacon {
    /// The advance sequence — monotonic, never reused.
    pub seq: u64,
    /// The manifest segment this root names.
    pub manifest: [u8; 32],
}

const BEACON_LEN: usize = 8 + 32 + 32;

impl RootBeacon {
    /// Encode: `seq BE | manifest | blake3(seq BE ‖ manifest)`.
    ///
    /// The trailing hash makes a truncated or zero-padded payload refusable
    /// without any key — a beacon is the one object a fresh site must judge
    /// before it has restored anything to judge with.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BEACON_LEN);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.manifest);
        out.extend_from_slice(blake3::hash(&out).as_bytes());
        out
    }

    /// Decode and verify. `expected_seq` is the sequence the NAME carried —
    /// a beacon copied under the wrong name would otherwise let a stale
    /// payload wear a fresh name, which is exactly the rewind this module
    /// exists to refuse.
    pub fn decode(bytes: &[u8], expected_seq: u64) -> Result<RootBeacon, TierError> {
        if bytes.len() != BEACON_LEN {
            return Err(TierError::Corrupt {
                detail: format!("beacon is {} bytes", bytes.len()),
            });
        }
        let (body, check) = bytes.split_at(8 + 32);
        if blake3::hash(body).as_bytes() != check {
            return Err(TierError::Corrupt {
                detail: "beacon self-check failed".into(),
            });
        }
        let seq = u64::from_be_bytes(body[..8].try_into().expect("8 bytes"));
        if seq != expected_seq {
            return Err(TierError::Corrupt {
                detail: format!("beacon named {expected_seq} carries seq {seq}"),
            });
        }
        let mut manifest = [0u8; 32];
        manifest.copy_from_slice(&body[8..]);
        Ok(RootBeacon { seq, manifest })
    }
}

/// Write the root beacon for one advance.
pub async fn advance_root<T: ObjectTier>(tier: &T, beacon: RootBeacon) -> Result<(), TierError> {
    tier.put(&ObjectName::Beacon { seq: beacon.seq }, beacon.encode())
        .await?;
    counted!("objstore.roots advanced");
    Ok(())
}

/// What discovery concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovered {
    /// The newest root, decoded and verified.
    Root(RootBeacon),
    /// The bucket holds no beacon AND the local floor agrees none ever
    /// existed — a genuinely fresh site. Distinct from every failure below.
    FreshBucket,
}

/// Why discovery refused.
#[derive(Debug)]
pub enum DiscoverError {
    /// The tier refused.
    Tier(TierError),
    /// The newest discoverable root is OLDER than the local floor — a
    /// truncated listing or a lost beacon. Restoring from it would rewind
    /// acknowledged history, so it is refused, loudly, here.
    Stale {
        /// The newest sequence the bucket offered, if any.
        found: Option<u64>,
        /// The floor the caller carried in.
        floor: u64,
    },
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverError::Tier(e) => write!(f, "{e}"),
            DiscoverError::Stale { found, floor } => match found {
                Some(found) => write!(
                    f,
                    "newest discoverable root is {found}, below the local floor {floor} — refusing to rewind"
                ),
                None => write!(
                    f,
                    "no root discoverable but the local floor says {floor} existed — refusing to read absence as fresh"
                ),
            },
        }
    }
}

impl std::error::Error for DiscoverError {}

/// Discover the current root from the bucket alone, against a local floor.
///
/// `floor` is the highest advance the caller has independent evidence for —
/// the commit-log replica stream's copy, or `None` on a site with no history
/// at all. The rules, in order:
///
///  - listing fails → the tier's error, never a guess;
///  - nothing listed + no floor → [`Discovered::FreshBucket`];
///  - nothing listed + a floor → [`DiscoverError::Stale`] — absence is not
///    freshness when something says otherwise;
///  - newest listed below the floor → `Stale`;
///  - newest listed unfetchable or undecodable → that error, NEVER a silent
///    step down to the next-oldest.
pub async fn discover_root<T: ObjectTier>(
    tier: &T,
    floor: Option<u64>,
) -> Result<Discovered, DiscoverError> {
    let seqs = tier.list_beacons().await.map_err(DiscoverError::Tier)?;
    let newest = seqs.iter().copied().max();
    let Some(newest) = newest else {
        return match floor {
            None => Ok(Discovered::FreshBucket),
            Some(floor) => {
                sometimes!("objstore.discovery refused a stale root", true);
                Err(DiscoverError::Stale { found: None, floor })
            }
        };
    };
    if let Some(floor) = floor {
        if newest < floor {
            sometimes!("objstore.discovery refused a stale root", true);
            return Err(DiscoverError::Stale {
                found: Some(newest),
                floor,
            });
        }
    }
    let bytes = tier
        .get(&ObjectName::Beacon { seq: newest })
        .await
        .map_err(|e| match e {
            // Listed but gone: the listing and the bucket disagree, and the
            // honest answer is "cannot determine the root", not the
            // next-oldest — which would be the rewind with extra steps.
            TierError::NotFound => DiscoverError::Tier(TierError::Indeterminate {
                detail: format!("beacon {newest} is listed but not fetchable"),
            }),
            other => DiscoverError::Tier(other),
        })?;
    let beacon = RootBeacon::decode(&bytes, newest).map_err(DiscoverError::Tier)?;
    counted!("objstore.roots discovered");
    Ok(Discovered::Root(beacon))
}
