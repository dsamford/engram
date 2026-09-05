//! GC — where this design earns its keep.
//!
//! The mark phase's liveness question has THREE answers: [`Liveness::Live`],
//! [`Liveness::Dead`], and [`Liveness::Unknown`] — the graph could not answer
//! (partition, timeout, index rebuild, a scan that returned before
//! completing).
//!
//! **`Unknown` aborts the sweep. It does not skip the object.** Skipping
//! unknowns and proceeding silently deletes anything whose REFERENCING side
//! was in the unknown region: a mark run while one tenant's shard is
//! unreachable observes zero references from that tenant, concludes all of
//! that tenant's blobs are garbage, and deletes them. A GC that treats an
//! outage as absence is a data-loss weapon aimed at whichever tenant happened
//! to be down — and there is no store-side undo beneath it. The abort is
//! COUNTED and the plan says why, because a GC that aborts silently is a GC
//! that never runs, and nobody notices until the volume is full.

use engram_key::KeyPrefix;
use engram_observe::{counted, crash_point, sometimes};
use engram_store::Store;

use crate::ContentKey;
use crate::manifest::{ManifestError, read_entry};

/// The mark phase's three-valued answer for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A reference (or unexpired lease) was observed.
    Live,
    /// The scan COMPLETED and observed no reference and no live lease.
    Dead,
    /// The scan could not answer. Not dead. Not skippable.
    Unknown,
}

/// What a sweep plan concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepPlan {
    /// Every candidate was answered; these are the deletions.
    Deletes(Vec<ContentKey>),
    /// At least one answer was `Unknown` — the WHOLE sweep aborts, deleting
    /// nothing, and the count is carried so the abort is observable where
    /// sweeps are scheduled.
    Aborted {
        /// How many candidates answered `Unknown`.
        unknowns: usize,
    },
}

/// Plan a sweep from the mark phase's answers.
pub fn plan_sweep(answers: &[(ContentKey, Liveness)]) -> SweepPlan {
    let unknowns = answers
        .iter()
        .filter(|(_, l)| *l == Liveness::Unknown)
        .count();
    counted!("blob.sweeps planned");
    if unknowns > 0 {
        sometimes!("blob.gc aborted on unknown", true);
        return SweepPlan::Aborted { unknowns };
    }
    SweepPlan::Deletes(
        answers
            .iter()
            .filter(|(_, l)| *l == Liveness::Dead)
            .map(|(k, _)| *k)
            .collect(),
    )
}

/// What the unlink worker did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkReport {
    /// Tombstones whose bytes were unlinked and whose queue rows cleared.
    pub unlinked: usize,
    /// Tombstones SPARED because the entry came back to life (a reference
    /// re-added, or a fresh lease) between enqueue and unlink.
    pub spared: usize,
    /// Tombstones left queued because the unlink refused (retried next run).
    pub deferred: usize,
}

/// Process the tombstone queue: for each queued content key, RE-CHECK
/// liveness, unlink the bytes, then dequeue — in that order.
///
/// - The re-check is the resurrection guard: [`crate::manifest::add_ref`] on
///   a tombstoned key revives the entry, and unlinking without re-reading
///   would dangle every reference added since the enqueue.
/// - Unlink-then-dequeue means a crash between them re-runs the unlink
///   (which must therefore be idempotent — `unlink` returning "already gone"
///   as success); dequeue-then-unlink would turn the same crash into bytes
///   nothing will ever collect WITH the queue believing them gone.
///
/// `unlink` is the byte-deletion hook (a T1 store delete, a T2 bucket
/// delete). `Err` defers the tombstone to the next run — an object-store
/// outage must not fail the worker.
pub fn process_tombstones(
    store: &Store,
    manifest_prefix: &KeyPrefix,
    tomb_prefix: &KeyPrefix,
    now: u64,
    mut unlink: impl FnMut(&ContentKey) -> Result<(), String>,
) -> UnlinkReport {
    let mut report = UnlinkReport {
        unlinked: 0,
        spared: 0,
        deferred: 0,
    };
    for (body, _) in store.scan(tomb_prefix) {
        let Ok(key) = <ContentKey>::try_from(body.as_slice()) else {
            // A malformed queue row is deferred forever rather than guessed
            // at; it will show up in the deferred count every run.
            report.deferred += 1;
            continue;
        };
        // The resurrection re-check, against the CURRENT entry.
        match read_entry(store, manifest_prefix, &key) {
            Ok(e) if e.live(now) => {
                sometimes!("blob.tombstone spared a resurrected blob", true);
                store.delete(tomb_prefix, &key);
                report.spared += 1;
                continue;
            }
            Ok(_) | Err(ManifestError::Missing) => {}
            Err(_) => {
                // A manifest row that cannot be READ is an unknown, and an
                // unknown is never dead.
                report.deferred += 1;
                continue;
            }
        }
        match unlink(&key) {
            Ok(()) => {
                // A crash HERE leaks the queue row — the next run re-unlinks
                // (idempotent) and clears it. The reverse order would dangle.
                crash_point("blob.between_unlink_and_dequeue");
                store.delete(tomb_prefix, &key);
                // The manifest entry itself is dropped with the bytes: the
                // wrapped DEK dies here, which is the per-object shred.
                store.delete(manifest_prefix, &key);
                counted!("blob.tombstones cleared");
                report.unlinked += 1;
            }
            Err(_) => report.deferred += 1,
        }
    }
    report
}
