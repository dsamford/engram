//! The manifest — the MVCC row between the graph's reference and the bytes.
//!
//! `node → manifest` is the ENFORCED half: [`add_ref`]/[`remove_ref`] run as
//! CAS operations in the store's own domain, so the refcount is transactional
//! and exact with zero object-store I/O. `manifest → bytes` is the MEASURED
//! half: [`VerifyState`] with `verified_at`/`verified_coverage`, where
//! `NotAttempted` is a distinct value that never reads as `Intact` — an
//! unmeasured blob is unmeasured, not healthy.
//!
//! # Dedup, and where the DEK lives
//!
//! Two references to one content within one realm resolve to ONE entry with
//! refcount 2 — the [`add_ref`] CAS increments instead of duplicating. The
//! per-object random DEK (convergent encryption being fatally incompatible
//! with shredding) travels WRAPPED under the realm's sealer in the entry;
//! dropping the entry is per-object shredding, rotating the master away from
//! the realm is per-tenant shredding.
//!
//! # The tombstone ordering
//!
//! Dropping the last reference appends a tombstone in the same synchronous
//! section (crash point between, and the window LEAKS — the mark scan finds a
//! zero-ref entry with no tombstone — rather than dangling). The unlink
//! worker in [`crate::gc`] deletes bytes only after re-checking liveness, and
//! dequeues only after the unlink: every crash window leaks, none dangles.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::{counted, crash_point, sometimes};
use engram_store::{Store, StoreError, StoredValue};

use crate::{ContentKey, Tier};

/// The measured half's three values. `NotAttempted` exists so absence of
/// scrubbing is representable — collapsing it into either real answer is the
/// house defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyState {
    /// No scrub has answered yet.
    NotAttempted,
    /// The last scrub found the bytes intact.
    Intact,
    /// The last scrub found damage.
    Damaged,
}

impl VerifyState {
    fn byte(self) -> u8 {
        match self {
            VerifyState::NotAttempted => 0,
            VerifyState::Intact => 1,
            VerifyState::Damaged => 2,
        }
    }

    fn from_byte(b: u8) -> Option<VerifyState> {
        match b {
            0 => Some(VerifyState::NotAttempted),
            1 => Some(VerifyState::Intact),
            2 => Some(VerifyState::Damaged),
            _ => None,
        }
    }
}

/// One manifest entry — the row that owns the blob's lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Live references from the graph. The enforced number.
    pub refcount: u32,
    /// Logical size.
    pub size: u64,
    /// Where the bytes live.
    pub tier: Tier,
    /// The measured half.
    pub verify: VerifyState,
    /// When the last scrub answered (store timestamp; 0 = never).
    pub verified_at: u64,
    /// How much of the object the last scrub covered, in percent. A scrub
    /// that sampled 1% must not wear the same "verified" as a full read.
    pub verified_coverage: u8,
    /// Lease expiry (store timestamp; 0 = no lease). The read handle IS the
    /// lease: liveness is `refcount > 0 OR lease unexpired`, because a blob
    /// between upload and commit — or under an embedder — is unreferenced BY
    /// DESIGN and must not be collectable.
    pub lease_expiry: u64,
    /// T2 locator: the object tier's content hash. None for T0/T1.
    pub locator: Option<[u8; 32]>,
    /// The per-object DEK, wrapped under the realm's sealer. Ciphertext here,
    /// so the manifest row itself stays marker-scan clean.
    pub wrapped_dek: Vec<u8>,
}

const ENTRY_V1: u8 = 1;

impl ManifestEntry {
    /// A fresh entry with one reference and nothing measured.
    pub fn first_ref(
        size: u64,
        tier: Tier,
        locator: Option<[u8; 32]>,
        wrapped_dek: Vec<u8>,
    ) -> Self {
        ManifestEntry {
            refcount: 1,
            size,
            tier,
            verify: VerifyState::NotAttempted,
            verified_at: 0,
            verified_coverage: 0,
            lease_expiry: 0,
            locator,
            wrapped_dek,
        }
    }

    /// Encode v1.
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(1 + 4 + 8 + 1 + 1 + 8 + 1 + 8 + 1 + 32 + 2 + self.wrapped_dek.len());
        out.push(ENTRY_V1);
        out.extend_from_slice(&self.refcount.to_be_bytes());
        out.extend_from_slice(&self.size.to_be_bytes());
        out.push(match self.tier {
            Tier::T0Inline => 0,
            Tier::T1Engine => 1,
            Tier::T2External => 2,
        });
        out.push(self.verify.byte());
        out.extend_from_slice(&self.verified_at.to_be_bytes());
        out.push(self.verified_coverage);
        out.extend_from_slice(&self.lease_expiry.to_be_bytes());
        match &self.locator {
            Some(l) => {
                out.push(32);
                out.extend_from_slice(l);
            }
            None => out.push(0),
        }
        let dek_len = u16::try_from(self.wrapped_dek.len()).expect("wrapped dek fits u16");
        out.extend_from_slice(&dek_len.to_be_bytes());
        out.extend_from_slice(&self.wrapped_dek);
        out
    }

    /// Decode v1, refusing truncation and unknown values.
    pub fn decode(bytes: &[u8]) -> Result<ManifestEntry, ManifestError> {
        fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], ManifestError> {
            let s = bytes
                .get(*at..*at + n)
                .ok_or_else(|| ManifestError::Malformed("truncated".into()))?;
            *at += n;
            Ok(s)
        }
        fn mal(d: &str) -> ManifestError {
            ManifestError::Malformed(d.to_string())
        }
        let mut at = 0usize;
        if take(bytes, &mut at, 1)?[0] != ENTRY_V1 {
            return Err(mal("unknown version"));
        }
        let refcount = u32::from_be_bytes(take(bytes, &mut at, 4)?.try_into().expect("4"));
        let size = u64::from_be_bytes(take(bytes, &mut at, 8)?.try_into().expect("8"));
        let tier = match take(bytes, &mut at, 1)?[0] {
            0 => Tier::T0Inline,
            1 => Tier::T1Engine,
            2 => Tier::T2External,
            _ => return Err(mal("unknown tier")),
        };
        let verify = VerifyState::from_byte(take(bytes, &mut at, 1)?[0])
            .ok_or_else(|| mal("unknown verify state"))?;
        let verified_at = u64::from_be_bytes(take(bytes, &mut at, 8)?.try_into().expect("8"));
        let verified_coverage = take(bytes, &mut at, 1)?[0];
        let lease_expiry = u64::from_be_bytes(take(bytes, &mut at, 8)?.try_into().expect("8"));
        let locator = match take(bytes, &mut at, 1)?[0] {
            0 => None,
            32 => {
                let mut l = [0u8; 32];
                l.copy_from_slice(take(bytes, &mut at, 32)?);
                Some(l)
            }
            _ => return Err(mal("bad locator length")),
        };
        let dek_len = u16::from_be_bytes(take(bytes, &mut at, 2)?.try_into().expect("2")) as usize;
        let wrapped_dek = take(bytes, &mut at, dek_len)?.to_vec();
        if at != bytes.len() {
            return Err(mal("trailing bytes"));
        }
        Ok(ManifestEntry {
            refcount,
            size,
            tier,
            verify,
            verified_at,
            verified_coverage,
            lease_expiry,
            locator,
            wrapped_dek,
        })
    }

    /// Liveness at `now`: the enforced refcount OR an unexpired lease.
    pub fn live(&self, now: u64) -> bool {
        self.refcount > 0 || self.lease_expiry > now
    }
}

/// Manifest refusals.
#[derive(Debug)]
pub enum ManifestError {
    /// A stored row did not decode.
    Malformed(String),
    /// The entry does not exist.
    Missing,
    /// A dedup hit whose size disagrees — a content-key collision or a
    /// caller bug; either way, refusing beats silently sharing bytes between
    /// two different contents.
    SizeMismatch {
        /// Size on the existing entry.
        existing: u64,
        /// Size the caller claimed.
        claimed: u64,
    },
    /// The store refused.
    Store(StoreError),
    /// The CAS lost too many rounds — contention beyond the retry budget.
    Contended,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Malformed(d) => write!(f, "malformed manifest entry: {d}"),
            ManifestError::Missing => write!(f, "no manifest entry"),
            ManifestError::SizeMismatch { existing, claimed } => {
                write!(
                    f,
                    "content key exists with size {existing}, caller claimed {claimed}"
                )
            }
            ManifestError::Store(e) => write!(f, "{e}"),
            ManifestError::Contended => write!(f, "manifest CAS exhausted its retries"),
        }
    }
}

impl std::error::Error for ManifestError {}

const MAX_CAS_RETRIES: usize = 32;

/// The manifest's key prefix for a realm.
pub fn manifest_prefix(realm: Realm, ns: Namespace, partition: Partition) -> KeyPrefix {
    KeyPrefix {
        realm,
        namespace: ns,
        kind: Kind::BLOB_MANIFEST,
        partition,
    }
}

/// The tombstone queue's key prefix for a realm.
pub fn tombstone_prefix(realm: Realm, ns: Namespace, partition: Partition) -> KeyPrefix {
    KeyPrefix {
        realm,
        namespace: ns,
        kind: Kind::BLOB_TOMBSTONE,
        partition,
    }
}

/// Read an entry.
pub fn read_entry(
    store: &Store,
    prefix: &KeyPrefix,
    key: &ContentKey,
) -> Result<ManifestEntry, ManifestError> {
    match store.get(prefix, key) {
        Some(bytes) => ManifestEntry::decode(&bytes),
        None => Err(ManifestError::Missing),
    }
}

/// Add a reference: create the entry, or DEDUP onto the existing one.
///
/// Returns the new refcount. On a dedup hit the existing entry's size must
/// match — and the caller's wrapped DEK is DISCARDED in favour of the
/// existing one, which is the point: one content, one object, one DEK.
pub async fn add_ref(
    store: &Store,
    prefix: &KeyPrefix,
    key: &ContentKey,
    size: u64,
    tier: Tier,
    locator: Option<[u8; 32]>,
    wrapped_dek: Vec<u8>,
) -> Result<u32, ManifestError> {
    for _ in 0..MAX_CAS_RETRIES {
        let current = store.get(prefix, key);
        let (expect, next) = match &current {
            None => {
                let e = ManifestEntry::first_ref(size, tier, locator, wrapped_dek.clone());
                (None, e)
            }
            Some(bytes) => {
                let mut e = ManifestEntry::decode(bytes)?;
                if e.size != size {
                    return Err(ManifestError::SizeMismatch {
                        existing: e.size,
                        claimed: size,
                    });
                }
                sometimes!("blob.dedup hit", true);
                e.refcount += 1;
                (Some(bytes.clone()), e)
            }
        };
        match store
            .cas(
                prefix,
                key,
                expect.as_deref(),
                StoredValue::Plain(next.encode()),
            )
            .await
        {
            Ok(_) => {
                counted!("blob.refs added");
                return Ok(next.refcount);
            }
            Err(StoreError::CasMismatch { .. }) => continue,
            Err(e) => return Err(ManifestError::Store(e)),
        }
    }
    Err(ManifestError::Contended)
}

/// What dropping a reference concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// References remaining.
    pub remaining: u32,
    /// Whether a tombstone was enqueued (last ref gone AND no live lease).
    pub tombstoned: bool,
}

/// Drop a reference. At zero — and only with no unexpired lease — append the
/// tombstone in the same synchronous section.
///
/// The crash window between the decrement and the tombstone LEAKS (a zero-ref
/// entry with no tombstone, findable by the mark scan) and never dangles. A
/// live lease suppresses the tombstone: the lease holder is mid-read, and the
/// mark scan enqueues it after expiry instead.
pub async fn remove_ref(
    store: &Store,
    prefix: &KeyPrefix,
    tomb_prefix: &KeyPrefix,
    key: &ContentKey,
    now: u64,
) -> Result<RemoveOutcome, ManifestError> {
    for _ in 0..MAX_CAS_RETRIES {
        let Some(bytes) = store.get(prefix, key) else {
            return Err(ManifestError::Missing);
        };
        let mut e = ManifestEntry::decode(&bytes)?;
        if e.refcount == 0 {
            return Ok(RemoveOutcome {
                remaining: 0,
                tombstoned: false,
            });
        }
        e.refcount -= 1;
        match store
            .cas(prefix, key, Some(&bytes), StoredValue::Plain(e.encode()))
            .await
        {
            Ok(_) => {
                counted!("blob.refs removed");
                let mut tombstoned = false;
                if e.refcount == 0 {
                    if e.lease_expiry > now {
                        sometimes!("blob.lease kept a zero-ref blob", true);
                    } else {
                        crash_point("blob.between_decrement_and_tombstone");
                        store
                            .put(tomb_prefix, key, StoredValue::Plain(Vec::new()))
                            .map_err(ManifestError::Store)?;
                        counted!("blob.tombstones enqueued");
                        tombstoned = true;
                    }
                }
                return Ok(RemoveOutcome {
                    remaining: e.refcount,
                    tombstoned,
                });
            }
            Err(StoreError::CasMismatch { .. }) => continue,
            Err(e) => return Err(ManifestError::Store(e)),
        }
    }
    Err(ManifestError::Contended)
}

/// Extend (never shorten) the entry's lease. The read handle IS the lease.
pub async fn acquire_lease(
    store: &Store,
    prefix: &KeyPrefix,
    key: &ContentKey,
    expires_at: u64,
) -> Result<(), ManifestError> {
    for _ in 0..MAX_CAS_RETRIES {
        let Some(bytes) = store.get(prefix, key) else {
            return Err(ManifestError::Missing);
        };
        let mut e = ManifestEntry::decode(&bytes)?;
        e.lease_expiry = e.lease_expiry.max(expires_at);
        match store
            .cas(prefix, key, Some(&bytes), StoredValue::Plain(e.encode()))
            .await
        {
            Ok(_) => return Ok(()),
            Err(StoreError::CasMismatch { .. }) => continue,
            Err(e) => return Err(ManifestError::Store(e)),
        }
    }
    Err(ManifestError::Contended)
}

/// Why a restore refused to start.
#[derive(Debug)]
pub struct RestoreTargetNotEmpty {
    /// Rows already present under the manifest prefix.
    pub rows: usize,
}

impl std::fmt::Display for RestoreTargetNotEmpty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "restore target already holds {} manifest row(s) — a restore into a live store passes every downstream check without doing anything",
            self.rows
        )
    }
}

impl std::error::Error for RestoreTargetNotEmpty {}

/// The restore precondition, and the FIRST check because no later check can
/// replace it: a restore into a live store trivially passes every downstream
/// content check, so "target is empty" is the only observation that can
/// distinguish a restore from a no-op.
pub fn assert_restore_target_empty(
    store: &Store,
    prefix: &KeyPrefix,
) -> Result<(), RestoreTargetNotEmpty> {
    let rows = store.scan(prefix).len();
    if rows != 0 {
        return Err(RestoreTargetNotEmpty { rows });
    }
    Ok(())
}
