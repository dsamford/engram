//! M8 — the replica, backup/restore verification, and PITR.
//!
//! # The replica verifies AS IT CONSUMES
//!
//! A follower that trusts its feed is a copy of whatever the feed became.
//! [`Replica::apply`] recomputes the hash chain entry by entry against its
//! OWN head — a tampered entry, a fork, or a gap refuses at the entry that
//! broke, with its sequence named. Retransmitted already-applied entries are
//! skipped idempotently (catch-up overlaps are normal); a FUTURE sequence is
//! a gap and never skipped over, because "skip the hole and keep going" is
//! how a replica silently diverges while reporting healthy.
//!
//! # Restore verification is independent of the writer
//!
//! [`verify_restore`] takes the ENTRIES and the restored store, and checks
//! the chain, the counts, and — decisively — that the restored log's head
//! hash equals the head recomputed from the source entries. The push's own
//! account of itself is never the evidence (the Lore lesson, applied to
//! backups).

use engram_log::{ChainVerify, CommitLog, Entry};
use engram_observe::{Canary, Gate, Registration, Subsystem, counted, sometimes};

use crate::{RecoverError, Store};

/// A follower over the commit-log stream.
pub struct Replica {
    store: Store,
    next_seq: u64,
    head_hash: [u8; 32],
}

impl Default for Replica {
    fn default() -> Self {
        Self::new()
    }
}

/// What one apply batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedReport {
    /// Entries applied by this call.
    pub applied: usize,
    /// Entries skipped as already-applied (retransmit overlap).
    pub skipped: usize,
    /// The replica's high-water sequence after the call (next expected).
    pub next_seq: u64,
    /// The highest commit timestamp applied so far.
    pub applied_ts: u64,
}

/// Why an apply refused. The batch stops AT the refusing entry; everything
/// before it in the batch is applied and stays applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// A sequence past the expected one — a hole in the feed.
    Gap {
        /// The sequence the replica expected next.
        expected: u64,
        /// The sequence that arrived.
        found: u64,
    },
    /// The entry's hash does not extend the replica's chain — tampered, or a
    /// fork from a different history.
    ChainMismatch {
        /// The refusing entry's sequence.
        seq: u64,
    },
    /// The entry's payload does not decode.
    Malformed {
        /// The refusing entry's sequence.
        seq: u64,
    },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Gap { expected, found } => {
                write!(
                    f,
                    "sequence gap: expected {expected}, got {found} — refusing to skip the hole"
                )
            }
            ApplyError::ChainMismatch { seq } => {
                write!(
                    f,
                    "entry {seq} does not extend this replica's chain (tamper or fork)"
                )
            }
            ApplyError::Malformed { seq } => write!(f, "entry {seq} has an undecodable payload"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl Replica {
    /// An empty replica.
    pub fn new() -> Replica {
        Replica {
            store: Store::new(),
            next_seq: 0,
            head_hash: CommitLog::genesis_hash(),
        }
    }

    /// The read side. Reads see exactly what has been applied, at the
    /// primary's own timestamps.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The next sequence this replica expects.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The highest commit timestamp applied (0 if nothing).
    pub fn applied_ts(&self) -> u64 {
        self.store.now_ts()
    }

    /// Whether a bookmark (a primary commit timestamp) is visible here.
    pub fn bookmark_satisfied(&self, bookmark_ts: u64) -> bool {
        self.applied_ts() >= bookmark_ts
    }

    /// Apply a batch from the primary's log stream.
    pub fn apply(&mut self, entries: &[Entry]) -> Result<AppliedReport, ApplyError> {
        let mut applied = 0usize;
        let mut skipped = 0usize;
        for e in entries {
            if e.seq < self.next_seq {
                // Already applied — but "already applied" means THIS entry,
                // not "some entry at this sequence". A fork retransmitting a
                // different history at an old seq must refuse, not skip; the
                // identity check is against the replica's own log. (The fork
                // test found exactly this: the first cut skipped by sequence
                // alone and waved a divergent entry through.)
                let mine = self.store.log_tail(e.seq);
                match mine.first() {
                    Some(own) if own.hash == e.hash => {
                        sometimes!("replica.skipped already-applied", true);
                        skipped += 1;
                        continue;
                    }
                    _ => {
                        sometimes!("replica.refused a chain mismatch", true);
                        return Err(ApplyError::ChainMismatch { seq: e.seq });
                    }
                }
            }
            if e.seq > self.next_seq {
                sometimes!("replica.refused a gap", true);
                return Err(ApplyError::Gap {
                    expected: self.next_seq,
                    found: e.seq,
                });
            }
            let expect = CommitLog::chain_hash(&self.head_hash, e.seq, &e.header, &e.payload);
            if expect != e.hash {
                sometimes!("replica.refused a chain mismatch", true);
                return Err(ApplyError::ChainMismatch { seq: e.seq });
            }
            self.store
                .apply_replicated(e)
                .map_err(|_| ApplyError::Malformed { seq: e.seq })?;
            self.head_hash = e.hash;
            self.next_seq = e.seq + 1;
            applied += 1;
            counted!("replica.entries applied");
        }
        Ok(AppliedReport {
            applied,
            skipped,
            next_seq: self.next_seq,
            applied_ts: self.applied_ts(),
        })
    }
}

/// Point-in-time recovery: rebuild the state as of commit timestamp `ts`.
///
/// The recovered prefix must actually BE a prefix — commit timestamps are
/// monotone in the log by construction, and an out-of-order stamp means the
/// entries are not this store's log, which is a refusal rather than a sort.
pub fn recover_to(entries: &[Entry], ts: u64) -> Result<Store, RecoverError> {
    let cut = entries.partition_point(|e| e.header.commit_ts <= ts);
    if let Some(bad) = entries[..cut]
        .iter()
        .zip(entries[..cut].iter().skip(1))
        .find(|(a, b)| a.header.commit_ts > b.header.commit_ts)
    {
        return Err(RecoverError::MalformedPayload { seq: bad.1.seq });
    }
    counted!("replica.pitr recoveries");
    Store::recover(&entries[..cut])
}

/// A restore verification verdict — reached from the SOURCE ENTRIES and the
/// restored store only, never from the restorer's own report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreVerdict {
    /// The restored store's log matches the source chain exactly.
    Faithful {
        /// Entries verified.
        entries: u64,
    },
    /// The source chain itself does not verify.
    SourceBroken {
        /// Where.
        seq: u64,
    },
    /// The restored store holds a different history.
    Diverged {
        /// The source's entry count.
        source_entries: u64,
        /// The restored store's entry count.
        restored_entries: u64,
        /// Whether the head hashes matched (counts can agree while content
        /// does not — the head is the decisive check).
        heads_match: bool,
    },
}

/// Verify a restore independently.
pub fn verify_restore(source: &[Entry], restored: &Store) -> RestoreVerdict {
    let (source_len, source_head) = match CommitLog::verify_entries(source) {
        ChainVerify::Intact { len, head } => (len, head),
        ChainVerify::Broken { seq } | ChainVerify::SequenceGap { expected: seq, .. } => {
            sometimes!("restore.verifier refused", true);
            return RestoreVerdict::SourceBroken { seq };
        }
    };
    let restored_len = restored.log_len();
    let restored_head = restored.log_head();
    if restored_len != source_len || restored_head != source_head {
        sometimes!("restore.verifier refused", true);
        return RestoreVerdict::Diverged {
            source_entries: source_len,
            restored_entries: restored_len,
            heads_match: restored_head == source_head,
        };
    }
    counted!("replica.restores verified");
    RestoreVerdict::Faithful {
        entries: source_len,
    }
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// Replication and restore, as a registered subsystem.
pub struct Replication;

impl Subsystem for Replication {
    const NAME: &'static str = "replica";

    fn register() -> Registration {
        Registration::new()
            .crash_point("replica.between_verify_and_apply")
            .sometimes("replica.refused a gap")
            .sometimes("replica.refused a chain mismatch")
            .sometimes("replica.skipped already-applied")
            .sometimes("restore.verifier refused")
            .counter("replica.entries applied")
            .counter("replica.pitr recoveries")
            .counter("replica.restores verified")
            .gate(
                Gate::new(
                    "a replica never skips a hole",
                    Canary::new("advance next_seq past a gap and assert the divergent read is caught"),
                ),
            )
            .gate(
                Gate::new(
                    "the chain is verified at APPLY, against the replica's own head",
                    Canary::new("trust the entry's stored hash and assert a re-hashed fork applies"),
                ),
            )
            .gate(
                Gate::new(
                    "restore verification is independent of the restorer",
                    Canary::new("verify from the restorer's report and assert a dropped-entry restore reads faithful"),
                ),
            )
    }
}
