//! M5.5 — the object-storage seam.
//!
//! # The boundary (risk C12)
//!
//! Real backends (`object_store`/S3, Lore's gRPC) construct their own runtimes
//! and live OUTSIDE the simulation envelope. So the engine's seam is this
//! trait, and the simulation lane runs against [`MemoryTier`] +
//! [`fault::FaultTier`]; network adapters implement the same trait in adapter
//! crates and are covered by the fault-injection integration lane instead. No
//! engine crate may depend on `reqwest`/`tonic` — the c-deps gate enforces the
//! dependency-graph half of that rule.
//!
//! # The error taxonomy is the seam's real content
//!
//! Neither `object_store` nor `opendal` has an `Unavailable` variant — both
//! collapse 500/503/timeout/DNS into a generic error. The wrapper supplies the
//! taxonomy, by classifying STATUS, never by string-matching a `Display` impl,
//! with the fail-closed default: **anything unrecognised becomes
//! [`TierError::Unavailable`], never `NotFound`** — the fatal direction is a
//! network hiccup reading as "the object does not exist".
//!
//! [`TierError::Indeterminate`] is first-class because Lore puts UNKNOWN on
//! the wire as a first-class answer, and because three of its proto defaults
//! are fail-open: a short presence answer reads as PRESENT (`FOUND_IN_CONTEXT
//! = 0`), so [`resumable_put`] refuses an under-answered query instead of
//! skipping the chunks it never asked about.

#![forbid(unsafe_code)]

pub mod beacon;
pub mod fault;
pub mod seal;

pub use beacon::{DiscoverError, Discovered, RootBeacon, advance_root, discover_root};
pub use fault::{FaultPlan, FaultTier};
pub use seal::{FetchError, Placement, PolicyRefused, SegmentKind, StoreRefused, TierPolicy};

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use engram_observe::{Canary, Gate, Registration, Subsystem, counted, sometimes};

/// What an object is named, and therefore what it IS.
///
/// Two shapes on purpose: segments are content-named (the name commits to the
/// bytes, so integrity is checkable by anyone holding the name), beacons are
/// SEQUENCE-named (monotonic, so discovery is "take the largest"). Collapsing
/// them into one string convention would lose the type-level fact that a
/// beacon's name proves recency and a segment's proves content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectName {
    /// A sealed segment, named by the BLAKE3 of its (sealed) bytes.
    Segment {
        /// BLAKE3 of the stored bytes.
        hash: [u8; 32],
    },
    /// The root beacon at one advance.
    Beacon {
        /// The advance sequence — monotonic, never reused.
        seq: u64,
    },
}

impl ObjectName {
    /// The wire path — stable, versioned by prefix.
    pub fn path(&self) -> String {
        match self {
            ObjectName::Segment { hash } => {
                let mut s = String::with_capacity(4 + 64);
                s.push_str("seg/");
                for b in hash {
                    use std::fmt::Write as _;
                    let _ = write!(s, "{b:02x}");
                }
                s
            }
            // Zero-padded so LEXICOGRAPHIC order is numeric order — the
            // property "take the largest listed beacon" rests on.
            ObjectName::Beacon { seq } => format!("root/{seq:020}"),
        }
    }
}

/// The seam's refusal taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierError {
    /// The backend positively asserted absence.
    NotFound,
    /// The backend could not answer — outage, timeout, or ANYTHING
    /// unrecognised. The fail-closed default.
    Unavailable {
        /// Operator-facing detail. Never parsed.
        detail: String,
    },
    /// The backend asked for backoff.
    Throttled,
    /// The lookup failed for an indeterminate reason — Lore's UNKNOWN, and
    /// the mapping for every fail-open default this seam guards.
    Indeterminate {
        /// Operator-facing detail. Never parsed.
        detail: String,
    },
    /// Bytes arrived and failed their integrity check. They were not returned.
    Corrupt {
        /// Operator-facing detail. Never parsed.
        detail: String,
    },
}

impl std::fmt::Display for TierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TierError::NotFound => write!(f, "not found"),
            TierError::Unavailable { detail } => write!(f, "unavailable: {detail}"),
            TierError::Throttled => write!(f, "throttled"),
            TierError::Indeterminate { detail } => write!(f, "indeterminate: {detail}"),
            TierError::Corrupt { detail } => write!(f, "corrupt: {detail}"),
        }
    }
}

impl std::error::Error for TierError {}

/// Classify an HTTP status into the taxonomy — the adapter-side rule, kept in
/// the engine crate so every adapter uses ONE classification and the tests can
/// canary it without a network.
///
/// Fail-closed: an unrecognised status is `Unavailable`, never `NotFound`. A
/// TLS misconfiguration, a proxy's 599, a new AWS status — all must read as
/// "cannot reach", because the other reading deletes data (a caller told
/// `NotFound` re-uploads or, worse, drops its reference).
pub fn classify_status(status: u16) -> TierError {
    match status {
        404 | 410 => TierError::NotFound,
        429 | 503 => TierError::Throttled,
        s => TierError::Unavailable {
            detail: format!("status {s}"),
        },
    }
}

/// The object tier — the seam adapters implement.
///
/// Async because the real implementations are network calls; the simulation
/// implementations complete after one deliberate yield so interleavings exist
/// for the executor to explore.
pub trait ObjectTier {
    /// Store bytes under a name. Content-named objects are immutable: a put to
    /// an existing segment name with identical bytes is idempotent success.
    fn put(&self, name: &ObjectName, bytes: Vec<u8>)
    -> impl Future<Output = Result<(), TierError>>;

    /// Fetch bytes by name.
    fn get(&self, name: &ObjectName) -> impl Future<Output = Result<Vec<u8>, TierError>>;

    /// Presence, without payload transfer. Returns the stored length.
    fn head(&self, name: &ObjectName) -> impl Future<Output = Result<usize, TierError>>;

    /// The beacon sequences this tier holds. A tier with no listing (Lore)
    /// returns `Indeterminate` — which is an ANSWER, and the reason beacons
    /// live on a listing-capable tier.
    fn list_beacons(&self) -> impl Future<Output = Result<Vec<u64>, TierError>>;

    /// Presence for a batch of names. The CONTRACT — enforced by the caller in
    /// [`resumable_put`], because the wire cannot enforce it — is one answer
    /// per asked name, in order.
    fn present(&self, names: &[ObjectName]) -> impl Future<Output = Result<Vec<bool>, TierError>>;
}

/// Yield exactly once, then complete — gives the executor an interleaving
/// point where a network round trip will be.
pub(crate) struct YieldOnce {
    polled: bool,
}

impl YieldOnce {
    pub(crate) fn new() -> Self {
        YieldOnce { polled: false }
    }
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.polled {
            std::task::Poll::Ready(())
        } else {
            self.polled = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// The in-memory tier — the simulation lane's backend, and the semantics every
/// adapter is held to.
#[derive(Clone, Default)]
pub struct MemoryTier {
    objects: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
}

impl MemoryTier {
    /// An empty tier.
    pub fn new() -> Self {
        Self::default()
    }

    /// A NEW HANDLE over the same bucket — the restore drill's "fresh site":
    /// local state gone, the bucket's contents intact.
    pub fn fresh_site(&self) -> MemoryTier {
        MemoryTier {
            objects: Rc::clone(&self.objects),
        }
    }

    /// Every stored byte, flattened — the marker-scan surface. The isolation
    /// gate greps THIS, because "the tier holds only ciphertext" is a claim
    /// about the bytes at rest, not about any API's return value.
    pub fn all_bytes(&self) -> Vec<u8> {
        self.objects
            .borrow()
            .values()
            .flat_map(|v| v.iter().copied())
            .collect()
    }

    /// Number of stored objects.
    pub fn object_count(&self) -> usize {
        self.objects.borrow().len()
    }

    /// Test-only demolition: remove one object by name, modelling a lost or
    /// swept object underneath a listing that already promised it.
    pub fn remove(&self, name: &ObjectName) -> bool {
        self.objects.borrow_mut().remove(&name.path()).is_some()
    }
}

impl ObjectTier for MemoryTier {
    async fn put(&self, name: &ObjectName, bytes: Vec<u8>) -> Result<(), TierError> {
        YieldOnce::new().await;
        let path = name.path();
        let mut objects = self.objects.borrow_mut();
        if let Some(existing) = objects.get(&path) {
            // Immutable namespace: same bytes is idempotent success, different
            // bytes under one name is a caller bug surfaced, never a silent
            // overwrite — for a content-named object it means the caller's
            // hash disagrees with ours, and one of us is corrupting.
            if existing == &bytes {
                return Ok(());
            }
            return Err(TierError::Corrupt {
                detail: format!("put would change immutable object {path}"),
            });
        }
        objects.insert(path, bytes);
        counted!("objstore.objects put");
        Ok(())
    }

    async fn get(&self, name: &ObjectName) -> Result<Vec<u8>, TierError> {
        YieldOnce::new().await;
        self.objects
            .borrow()
            .get(&name.path())
            .cloned()
            .ok_or(TierError::NotFound)
    }

    async fn head(&self, name: &ObjectName) -> Result<usize, TierError> {
        YieldOnce::new().await;
        self.objects
            .borrow()
            .get(&name.path())
            .map(Vec::len)
            .ok_or(TierError::NotFound)
    }

    async fn list_beacons(&self) -> Result<Vec<u64>, TierError> {
        YieldOnce::new().await;
        Ok(self
            .objects
            .borrow()
            .range("root/".to_string().."root0".to_string())
            .filter_map(|(k, _)| k.strip_prefix("root/").and_then(|s| s.parse().ok()))
            .collect())
    }

    async fn present(&self, names: &[ObjectName]) -> Result<Vec<bool>, TierError> {
        YieldOnce::new().await;
        let objects = self.objects.borrow();
        Ok(names
            .iter()
            .map(|n| objects.contains_key(&n.path()))
            .collect())
    }
}

/// What a resumable put did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReport {
    /// Chunks the tier already held.
    pub already_present: usize,
    /// Chunks uploaded by this call.
    pub uploaded: usize,
}

/// Upload only the missing chunks — with the fail-open default guarded.
///
/// Lore's `QueryResponse` echoes no addresses; correlation is index-only, and
/// `FOUND_IN_CONTEXT = 0` is the proto3 default — so a short, truncated or
/// zero-padded answer reads as PRESENT for exactly the chunks it says nothing
/// about, the upload skips them, and the manifest still hashes correctly
/// because it was computed locally. The one guard that closes that:
/// **`answers.len() == names.len()`, and any shortfall is `Indeterminate`** —
/// refused, never read.
pub async fn resumable_put<T: ObjectTier>(
    tier: &T,
    chunks: &[(ObjectName, Vec<u8>)],
) -> Result<ResumeReport, TierError> {
    let names: Vec<ObjectName> = chunks.iter().map(|(n, _)| *n).collect();
    let answers = tier.present(&names).await?;
    if answers.len() != names.len() {
        sometimes!("objstore.presence answer refused as indeterminate", true);
        return Err(TierError::Indeterminate {
            detail: format!(
                "asked about {} chunks, answered for {}",
                names.len(),
                answers.len()
            ),
        });
    }
    let mut report = ResumeReport {
        already_present: 0,
        uploaded: 0,
    };
    for ((name, bytes), present) in chunks.iter().zip(answers) {
        if present {
            report.already_present += 1;
        } else {
            tier.put(name, bytes.clone()).await?;
            report.uploaded += 1;
        }
    }
    counted!("objstore.resumable puts");
    Ok(report)
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The object-storage seam, as a registered subsystem.
pub struct ObjectStorage;

impl Subsystem for ObjectStorage {
    const NAME: &'static str = "objstore";

    fn register() -> Registration {
        Registration::new()
            .crash_point("objstore.between_seal_and_put")
            .sometimes("objstore.get refused corrupt bytes")
            .sometimes("objstore.discovery refused a stale root")
            .sometimes("objstore.presence answer refused as indeterminate")
            .sometimes("objstore.fault injected")
            .sometimes("objstore.store refused a mis-tiered segment")
            .counter("objstore.objects put")
            .counter("objstore.segments stored")
            .counter("objstore.segments fetched")
            .counter("objstore.roots advanced")
            .counter("objstore.roots discovered")
            .counter("objstore.resumable puts")
            .gate(
                Gate::new(
                    "tier bytes are ciphertext, per realm",
                    Canary::new("store the marker plain and assert the tier scan finds it — the violating control"),
                )
                .and_canary(Canary::new("fetch a sealed segment under the wrong realm's key and assert refusal")),
            )
            .gate(
                Gate::new(
                    "corrupt bytes never reach a caller",
                    Canary::new("return fetched bytes without the hash check and assert the flipped byte is caught"),
                ),
            )
            .gate(
                Gate::new(
                    "discovery never silently rewinds",
                    Canary::new("truncate the listing below the local floor and assert refusal, not an older root"),
                )
                .and_canary(Canary::new("make the listed-but-unfetchable beacon skip to the next-oldest and assert refusal")),
            )
            .gate(
                Gate::new(
                    "vector segments never leave tier 0",
                    Canary::new("construct a policy routing vectors to tier 1 and assert boot refuses"),
                )
                .and_canary(Canary::new("hand store_sealed a vector segment and assert the mis-tier refusal")),
            )
            .gate(
                Gate::new(
                    "an under-answered presence query refuses",
                    Canary::new("pad the shortfall as present and assert the never-uploaded chunk is detected"),
                ),
            )
    }
}
