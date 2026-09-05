//! The storage tail: MVCC memtable, entity write locks, native CAS, and the
//! protected-KIND gate.
//!
//! # L3 — why READ COMMITTED with write locks, stated once more where the code is
//!
//! The plan calls this the largest single correction in the research. The
//! incumbent's `guarded-write.ts` is its only correct CAS, and its correctness
//! depends on **write-lock-then-read** ordering: take the lock, read the
//! CURRENT committed value, compare, write. Under snapshot isolation the read
//! returns the transaction's snapshot instead, **every contender's guard
//! passes**, and ownership/tenancy/governance writes lose updates silently —
//! no error, no log line, just the last writer winning a race nobody knew ran.
//!
//! [`Store::cas`] is therefore the supported primitive, and its read happens
//! after lock acquisition by construction — the method's body IS the ordering.
//!
//! # The protected-KIND gate
//!
//! > The storage layer rejects a plaintext put to a protected `KIND`
//! > unconditionally. The planner is advisory; **the keyspace is the gate**.
//!
//! [`StoredValue`] makes the caller SAY which of the two states its bytes are
//! in, and [`Store::put`] refuses `Plain` for any protected KIND — including
//! KINDs this build has never heard of, because `Kind::is_protected` is a
//! range check. There is no flag to disable it and no privileged caller.
//!
//! # Concurrency model
//!
//! D2 was "one shard, one thread, cooperative tasks; the store is `!Sync` on
//! purpose." It is REVISED (2026-08-25) for the concurrent-write program: the
//! store is now `Send + Sync`, because morsel-driven parallel execution and
//! MVCC-OCC require it to cross threads. Integrity is preserved by a stronger
//! mechanism than single-thread simulation — result-determinism, Loom/shuttle
//! interleaving search, and a serializability checker (redesign M2–M4).
//!
//! Interior state is an `Arc<RwLock<State>>`, taken through `st`/`st_mut` in one
//! place. This is a COARSE latch and a deliberate M2 stepping stone: it makes
//! the type thread-safe with no behaviour change, and M3 refines it into
//! fine-grained latches (the immutable sealed segments never need locking; only
//! the mutable tail, `next_ts`, `pins` and `locks` do) so a reader on a snapshot
//! never blocks a writer.
//!
//! Locks (the per-entity `Locks`) still exist because AWAIT POINTS interleave
//! tasks — a transaction that awaits mid-flight can be raced by another task,
//! which is precisely how the CAS test in `tests/cas.rs` runs eight contenders
//! through one lock.

#![forbid(unsafe_code)]

pub mod adjacency;
pub mod columnar;
pub mod dirlock;
pub mod index;
pub mod overlay;
pub mod paged;
pub mod record;
pub mod replica;
pub mod segment;
mod compact_paged;
pub use compact_paged::MergeObserver;
pub mod sst;
pub(crate) mod tail;
pub mod vector;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use engram_key::{KeyPrefix, encode_key};
use engram_log::{CommitLog, Entry, Op, RoutingHeader, Wal};
use engram_observe::{Canary, Gate, Registration, Subsystem, counted, crash_point, sometimes};

pub use adjacency::{
    AdjacencyError, EdgeDir, EdgeType, NodeAt, PeerRef, add_edge, chunk_key_body, chunk_stats,
    degree, find_half_edges, neighbors, remove_edge,
};
pub use columnar::COLUMNAR_MIN_ROWS;
pub use index::{IndexDef, IndexKey, RangeAnswer, RangeIndex};
pub use overlay::{
    NamespaceRegistry, NamespaceRole, OverlayError, SYSTEM_REALM, SystemWriteCap, TenantSession,
    system_put,
};
pub use record::{PropertyId, Record, RecordError, get_property};
pub use replica::{AppliedReport, ApplyError, Replica, RestoreVerdict, recover_to, verify_restore};
pub use segment::{Projected, SealedSegment, Segment};
pub use vector::{SearchAnswer, VectorError, VectorIndex, encode_f32_vector};

// ─── Values, and the gate on them ───────────────────────────────────────────

/// A value as the store receives it: the caller states which of the two
/// security states the bytes are in.
///
/// An enum rather than a boolean parameter because a boolean defaults — a
/// call site that forgets it picks whatever `false` means, silently. Here a
/// caller that has not thought about encryption cannot construct the argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredValue {
    /// Plaintext. Refused for protected KINDs, unconditionally.
    Plain(Vec<u8>),
    /// Ciphertext under the tenant DEK. The store does not inspect it.
    Sealed(Vec<u8>),
}

impl StoredValue {
    fn bytes(&self) -> &[u8] {
        match self {
            StoredValue::Plain(b) | StoredValue::Sealed(b) => b,
        }
    }
}

/// Transaction commits aborted by validation — the production-visible twin
/// of the thread-local `counted!("store.txn conflicts")`, on the
/// [`engram_log::FSYNCS`] pattern. Monotonic; never read by engine logic.
pub static TXN_CONFLICTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Range-index FULL BUILDS — a scan of the whole group. The expensive one:
/// at SF1 the `Message` group is ~3M rows.
///
/// The three index counters exist because §9's per-shape evidence is
/// circumstantial. `is7-replies` anchors on `Message.id`, the one property the
/// `balanced` write inserts into, and its p50 goes 0.16 -> 18 ms while reads on
/// a quiet property move 1.2-1.5x. That is a correlation between "the write
/// touches it" and "the read slows down"; it does not say WHICH of build,
/// catch-up or fold is being paid, and the arithmetic for a per-catch-up clone
/// does not obviously reach 18 ms. Counting each separately turns the question
/// into division rather than argument: against the profile's read count, these
/// give the per-read frequency of each, and frequency times known cost is the
/// answer.
pub static INDEX_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Range-index INCREMENTAL catch-ups (`with_changes`) — the cheap-per-call
/// path that clones and re-sorts `added`, bounded by `FOLD_AT`.
pub static INDEX_CATCHUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Range-index overlay FOLDS — O(base), so one is worth many catch-ups.
pub static INDEX_FOLDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Span reads that took ALL 64 tail shard read latches and therefore EXCLUDED
/// every writer for their duration — see the probe in `merge_span`.
///
/// The latches are taken only when the tail is NON-EMPTY, so this is zero on a
/// pure-read workload (the tail drains at the seal) and zero on a pure-write one
/// (no span reads are issued). It counts only in a MIX, which is exactly where
/// engram loses to Neo4j: 0.63x on write-heavy and 0.75x on balanced against
/// 1.49x-3.98x on the pure profiles.
pub static SPAN_READS_EXCLUDING_WRITERS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Span reads that skipped the latches because the tail was empty — the
/// control. Without it, a high exclusion count could mean "reads are frequent"
/// rather than "reads exclude writers", and the two arms are the whole point.
pub static SPAN_READS_LATCH_FREE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Rows merged while those latches were held. The merge's work — and therefore
/// the window every writer waited on — is proportional to this, and rows are
/// the honest proxy for a duration the engine crates may not measure
/// (`Instant::now` is clippy-banned there).
pub static SPAN_ROWS_UNDER_LATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Entries of the commit window this validator actually walked.
///
/// The window is ts-monotone, so only the SUFFIX above a reader's snapshot can
/// contain anything it needs. This counts what was walked, so "we now walk the
/// suffix" is a number rather than an argument — and so the OFF arm's full-ring
/// scan is visible next to it.
pub static WINDOW_ENTRIES_SCANNED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Rows of the commit delta §7's predicate pass will examine before it gives up
/// on a commit and leaves read-set validation to stand alone.
///
/// Sized against what the pass COSTS, not against what it would like: each
/// accepted row is a store read plus a record decode, executed under the global
/// commit latch. 4,096 is the same order as `ADJ_REPAIR_MAX` and for the same
/// reason — it is the point past which the bounded work stops being cheaper
/// than the thing it replaces.
const PRECISION_MAX_DELTA: usize = 4_096;

/// Aborts caused by §7's PREDICATE validation rather than by the read set — a
/// phantom that read-set validation could not have seen. Counted separately
/// because it is the whole measurable effect of precision locking: the two arms
/// must differ here and nowhere else.
pub static PHANTOM_CONFLICTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Write-write conflicts EXEMPTED because both sides were puts to a key class
/// that declares put-vs-put harmless (the adjacency guard row — see the
/// exemption in `commit_reporting`). Monotonic; the evidence that the
/// exemption is doing anything, and the number to watch if a dangling-edge
/// bug is ever suspected.
pub static PUT_PUT_EXEMPTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Re-exported so the server can print one counter surface without a direct
/// engram-log dependency.
pub use engram_log::FSYNCS;

/// What a conflicted [`Transaction::commit_reporting`] knew at the abort:
/// the first key whose committed version moved past the snapshot, and the
/// transaction's own write-set keys. The escalation path uses these to
/// decide whether a lock can help (only a write-write conflict can be
/// queued behind) and which keys to queue on.
#[derive(Debug)]
pub struct ConflictInfo {
    /// The key(s) validation found moved. Currently the first found.
    pub conflicting: Vec<LogicalKey>,
    /// Every key this transaction intended to write.
    pub write_set: Vec<LogicalKey>,
}

/// Store errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A plaintext put to a protected KIND. There is no override.
    ProtectedKindPlaintext {
        /// The KIND that was refused.
        kind_byte: u8,
    },
    /// A KIND byte that may not appear in a key at all.
    InvalidKind(u8),
    /// The write reached the log but the log could not be made durable.
    ///
    /// Returned rather than panicked: a full disk is an operational condition a
    /// server should refuse a write over and keep serving reads through, not a
    /// reason to take the process down. The write-set is unwound before this is
    /// returned, so the caller never sees data it was told was not durable.
    Durability(String),
    /// The CAS guard did not match the current committed value.
    ///
    /// Carries what WAS current, so the caller can retry against reality
    /// instead of re-reading — and so a conflict is distinguishable from a
    /// race it lost twice.
    CasMismatch {
        /// The committed value the guard was compared against.
        current: Option<Vec<u8>>,
    },
    /// An optimistic [`Transaction`] lost a conflict at commit: a key in its
    /// read-set or write-set was committed by another transaction AFTER this
    /// one's snapshot (first-committer-wins). The caller retries from a fresh
    /// snapshot — the transaction published nothing.
    Conflict,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::ProtectedKindPlaintext { kind_byte } => write!(
                f,
                "REFUSED: plaintext put to protected KIND 0x{kind_byte:02x} — the keyspace is \
                 the gate, and there is no override"
            ),
            StoreError::InvalidKind(b) => write!(f, "KIND 0x{b:02x} is not valid in a key"),
            StoreError::Durability(e) => write!(
                f,
                "write NOT made durable, so it was not acknowledged and has been unwound: {e}"
            ),
            StoreError::CasMismatch { .. } => {
                write!(f, "CAS guard did not match the current value")
            }
            StoreError::Conflict => {
                write!(
                    f,
                    "transaction commit lost a conflict — retry from a fresh snapshot"
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// Why a recovery refused the log it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverError {
    /// The hash chain breaks at `seq`. Replaying past a break would silently
    /// restore a prefix and report it as the database.
    BrokenChain {
        /// First entry whose hash fails.
        seq: u64,
    },
    /// Entries are missing or reordered.
    SequenceGap {
        /// Expected sequence number.
        expected: u64,
        /// Found sequence number.
        found: u64,
    },
    /// An entry's payload does not decode as `body_len | body | value`.
    MalformedPayload {
        /// The offending entry.
        seq: u64,
    },
}

impl std::fmt::Display for RecoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoverError::BrokenChain { seq } => write!(
                f,
                "log chain broken at seq {seq} — refusing a partial replay"
            ),
            RecoverError::SequenceGap { expected, found } => {
                write!(f, "log sequence gap: expected {expected}, found {found}")
            }
            RecoverError::MalformedPayload { seq } => {
                write!(f, "malformed log payload at seq {seq}")
            }
        }
    }
}

impl std::error::Error for RecoverError {}

/// Opening a store from a durable WAL fails either at the disk (`Io`) or in the
/// replay of the bytes it read back (`Recover`). Kept distinct so a caller can
/// tell "the file would not open" from "the file opened but its history is
/// broken" — the second is a corruption signal, the first an environment one.
#[derive(Debug)]
pub enum OpenWalError {
    /// The WAL file could not be opened, read, or truncated.
    Io(std::io::Error),
    /// The path is not a WAL, or is a format this build cannot read. Distinct
    /// from `Io` because the file is INTACT and deliberately untouched: the
    /// operator pointed at the wrong path, and the right answer is to say so,
    /// not to overwrite it.
    Format(engram_log::WalError),
    /// The bytes replayed, but the recovered chain is broken/gapped/malformed.
    Recover(RecoverError),
}

impl From<engram_log::WalError> for OpenWalError {
    fn from(e: engram_log::WalError) -> Self {
        match e {
            engram_log::WalError::Io(io) => OpenWalError::Io(io),
            other => OpenWalError::Format(other),
        }
    }
}

impl std::fmt::Display for OpenWalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenWalError::Io(e) => write!(f, "WAL i/o error: {e}"),
            OpenWalError::Format(e) => write!(f, "{e}"),
            OpenWalError::Recover(e) => write!(f, "WAL replay failed: {e}"),
        }
    }
}

impl std::error::Error for OpenWalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenWalError::Io(e) => Some(e),
            OpenWalError::Format(e) => Some(e),
            OpenWalError::Recover(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for OpenWalError {
    fn from(e: std::io::Error) -> Self {
        OpenWalError::Io(e)
    }
}

impl From<RecoverError> for OpenWalError {
    fn from(e: RecoverError) -> Self {
        OpenWalError::Recover(e)
    }
}

// ─── MVCC state ─────────────────────────────────────────────────────────────

/// A logical key: everything except the commit timestamp.
pub(crate) type LogicalKey = Vec<u8>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) commit_ts: u64,
    /// `None` is a tombstone. A deletion is a WRITE — readers below the
    /// tombstone's timestamp still see the old value, which is what makes
    /// deletion safe under concurrent readers.
    ///
    /// Behind an `Arc` so that carrying a version around is a refcount bump
    /// rather than a copy of the record's bytes. Those bytes are produced once
    /// by `Record::encode` and were then copied twice more: into the log
    /// payload, and again into the tail. This removes the third.
    ///
    /// It also makes `Segment::get_at`'s clone free, which is a READ-path win
    /// on every sealed-segment hit, and makes the compactor's `cloned_entries`
    /// share bytes instead of duplicating a segment's worth of them.
    ///
    /// The ON-DISK bytes are unchanged: `sst::encode_entry` writes the slice
    /// either way, which is what the round-trip byte-identity test pins.
    pub(crate) value: Option<std::sync::Arc<[u8]>>,
    pub(crate) sealed: bool,
}

#[derive(Default)]
struct Locks {
    /// Held locks, by logical key.
    held: BTreeMap<LogicalKey, ()>,
    /// FIFO waiters per key, identified — FAIRNESS IS CORRECTNESS here, not
    /// politeness: with a racy wake order the same contender can win repeatedly
    /// and the others starve, which in the incumbent's lease pattern showed up
    /// as a worker that never acquired in 25 rounds.
    ///
    /// Entries carry a waiter id, because a waiter must be able to find and
    /// REFRESH its own entry on re-poll. The first version stored bare wakers
    /// and set a `queued` flag once — so a waiter woken and then beaten to the
    /// lock (a fresh contender can BARGE between the wake and the re-poll)
    /// would see the lock held, decline to re-register, and never be woken
    /// again. A LOST WAITER: its task blocks forever and the run stalls. The
    /// canary that exposed it was "wake every waiter", where losing the
    /// re-poll race is the common case rather than the barging corner.
    waiters: BTreeMap<LogicalKey, VecDeque<(u64, Waker)>>,
    next_waiter_id: u64,
}

/// The commit clock's VISIBILITY side: which allocated timestamps have been
/// published to the tail, in order.
///
/// Timestamps are allocated under the log latch (so log order is commit
/// order) but PUBLISHED into the sharded tail by each writer on its own,
/// after the latch is released — so ts 101 can land in its shard before ts
/// 100 lands in its. A reader must not snapshot at 101 until 100 is there
/// too, or its snapshot would be judged current while missing a version it
/// covers (the derived structures stamp themselves with exactly this clock).
/// `visible_ts` is the highest ts such that EVERY ts at or below it is in the
/// tail; `pending` holds the ones published out of order, waiting for the
/// gap below them to close.
struct Visibility {
    /// One slot per in-flight timestamp, indexed `ts % VISIBILITY_RING`:
    /// holds `ts` once that timestamp's writer has published, 0 otherwise.
    /// A writer stores its own slot (one atomic write, no latch), then
    /// advances the visible clock over every consecutive published slot with
    /// a CAS — several writers may advance at once; the CAS keeps the clock
    /// monotone and the slot values keep each advance honest. The ring must
    /// be larger than the number of timestamps that can be allocated but not
    /// yet published, i.e. the number of concurrent writers; a writer that
    /// laps the ring waits for the clock to catch up first.
    ring: Vec<std::sync::atomic::AtomicU64>,
}

/// In-flight timestamps the visibility ring can hold — far more than any
/// worker pool, so a writer never waits on it in practice.
const VISIBILITY_RING: usize = 4096;

/// A commit timestamp between allocation and publish. Dropping it publishes
/// the timestamp to the visible clock — on the success path after the tail
/// insert, and on every other path too (an error return, a panic's unwind),
/// so an allocated timestamp can never hold the clock back for ever.
struct CommitSlot<'a> {
    store: &'a Store,
    ts: u64,
}

impl CommitSlot<'_> {
    /// The success path: publish, then WAIT until the visible clock has
    /// passed this timestamp — so "the write returned" means "every reader
    /// on every worker can see it", the read-your-writes every acknowledged
    /// write promises. The wait is for earlier, slower allocators only (the
    /// clock advances in order) and is normally zero iterations.
    fn finish(self) {
        use std::sync::atomic::Ordering;
        let (store, ts) = (self.store, self.ts);
        drop(self); // publishes
        let mut waited = 0u32;
        while store.inner.visible_ts.load(Ordering::Acquire) < ts {
            // SELF-HELP, not just a wait — every waiter re-drives the clock.
            // Publishers alone cannot be trusted to: publisher A can store
            // its slot, see the gap below it and return; publisher B fills
            // that gap, advances to its own stamp, and its load of A's slot
            // races A's store — misses it, returns. Both are done, the clock
            // is stuck one below A, and A's wait here would spin for ever
            // (measured: a permanent whole-server write stall). A's own slot
            // is visible to A by program order, so one advance from here
            // resolves it.
            store.advance_visible();
            backoff(&mut waited);
            stall_report(&mut waited, || {
                format!(
                    "CommitSlot::finish waiting for visible {} to reach ts {} (next_ts {})",
                    store.inner.visible_ts.load(Ordering::Relaxed),
                    ts,
                    store.inner.next_ts.load(Ordering::Relaxed),
                )
            });
        }
    }
}

/// One step of a bounded spin-then-yield wait. The waits in this file are
/// for other writers' microseconds, but a writer can be blocked behind a
/// long merge holding every shard's read latch; spinning through that would
/// burn a core for milliseconds, so past a few dozen spins the waiter yields.
fn backoff(waited: &mut u32) {
    if *waited < 64 {
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
    *waited = waited.saturating_add(1);
}

/// A spin wait that has gone on for millions of iterations is a wedge, not
/// a wait — report it ONCE (per call site instance) with enough state to
/// name the stuck clock, instead of burning a core silently for ever. The
/// threshold is far above any legitimate wait (a merge holds latches for
/// milliseconds; this fires after ~tens of seconds of spinning).
fn stall_report(waited: &mut u32, msg: impl FnOnce() -> String) {
    const REPORT_AT: u32 = 30_000_000;
    if *waited == REPORT_AT {
        eprintln!("[engram-store] STALL: {}", msg());
    }
}

impl Drop for CommitSlot<'_> {
    fn drop(&mut self) {
        self.store.publish_ts(self.ts);
    }
}

/// The commit log and the state that must move with it.
struct LogState {
    /// The WAL rule is LOG THEN PUBLISH: an entry is appended before the
    /// version becomes visible, so after a crash the log is always a superset
    /// of the memtable and recovery is a pure REDO. The other order loses
    /// acknowledged writes — the crash tests kill at both boundaries.
    log: CommitLog,
    /// RECENT COMMITS: `(ts, key, is_put)` for the newest writes, oldest first.
    ///
    /// OCC validation asks exactly one question per key — did anyone commit to
    /// it after my snapshot — and answered it with a point lookup per key,
    /// under the global commit latch. That makes commit cost O(read set) at the
    /// one serialisation point that cannot be parallelised, which is why a
    /// statement whose MATCH materialised many rows got slower as workers were
    /// added rather than faster.
    ///
    /// Every write allocates its ts under THIS latch (`Store::allocate` returns
    /// the guard and the slot together), so a ring appended at allocation is
    /// ts-monotone and complete by construction. Validation can then answer
    /// from the window in O(window) instead of O(read set) — and the window is
    /// the number of commits since the reader's snapshot, which for a short
    /// transaction is small however much it read.
    window: std::collections::VecDeque<(u64, LogicalKey, bool)>,
    /// Entries the window may hold. Past this the oldest are dropped and
    /// `window_low` rises, which is what makes the fallback necessary.
    window_cap: usize,
    /// The oldest ts the window still describes. A transaction whose snapshot
    /// is older than this cannot be answered from the window — the entries it
    /// would need have been dropped — and falls back to the point loop.
    window_low: u64,
}

impl LogState {
    /// Record a committed write in the recent-commit window.
    ///
    /// Called with the log latch HELD, at the moment the ts is allocated, so
    /// the window is ts-monotone and complete: no write can allocate between a
    /// record and the next.
    fn note_commit(&mut self, ts: u64, key: LogicalKey, is_put: bool) {
        if self.window_cap == 0 {
            return;
        }
        self.window.push_back((ts, key, is_put));
        while self.window.len() > self.window_cap {
            if let Some((dropped_ts, _, _)) = self.window.pop_front() {
                // The window no longer describes anything at or below this, so
                // a reader whose snapshot is older must take the point loop.
                self.window_low = self.window_low.max(dropped_ts + 1);
            }
        }
    }
}

/// Bytes copied into a log payload — the concatenation `log_payload` performs.
///
/// Process-wide and `Relaxed`: it is an instrument for a sizing decision, not a
/// correctness signal. See `log_payload` for what it is deciding.
pub static LOG_BYTES_COPIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Recent commits the validator may answer from before falling back to a point
/// lookup per key.
///
/// Sized so a short transaction's window is covered while the memory stays
/// trivial: an entry is a ts, a key and a bool, and 64k of them is a few MB.
/// A long-running transaction falls back, which is correct and is exactly the
/// case where the point loop is the cheaper answer anyway.
const COMMIT_WINDOW_CAP: usize = 65_536;

/// Rows a span read copies out of the tail before declining (see
/// `ShardedTail::range_copied`).
///
/// Sized against what the copy COSTS, not what it would like. A `LogicalKey`
/// plus its chain is on the order of 100 B, so 64K rows is a few megabytes per
/// concurrent span read — affordable per worker. The spans this is for are
/// small: one node's adjacency row, one label's membership slice. The walks
/// that are not small are the full-span rebuilds, and those are exactly the
/// ones that must keep the borrow path rather than try to copy 17.26M rows.
const TAIL_COPYOUT_CAP: usize = 65_536;

/// Bytes a row-form column read may hold in its override map before it
/// declines to the caller's per-member gather (`resolve_rows_only`). The row
/// budget is `factor × |members|`, and for a wide label that is a lot of
/// bytes: the production NewsArticle enrichment count walked up to 1.2M rows
/// of ~2 KB records into the map — 2 to 3.8 GB per execution, the transient
/// that OOM-killed the 12Gi pod — and then declined anyway. A quarter of a
/// gigabyte is far above what any label-sized read holds and far below the
/// envelope's headroom.
const COLUMN_SCAN_BYTE_BUDGET: usize = 256 << 20;

/// The IMMUTABLE sealed segments (oldest first; reads walk them NEWEST first
/// after the tail). Swapped ATOMICALLY on seal/compact via `arc_swap`, so a
/// reader loads the whole set with no lock — the read-scaling half of the D2
/// revision. Once published, a `Sealed` is never mutated; seal/compact build a
/// fresh one and swap it in.
#[derive(Default)]
struct Sealed {
    /// `Arc<SealedSegment>` so seal/compact rebuild the set by cloning POINTERS,
    /// not the (large) segment bodies, and so a reader's loaded guard keeps its
    /// segments alive even as a concurrent seal swaps a new set in. Each element
    /// is `Resident` (in RAM, the default) or `Paged` (on disk, cache-backed) —
    /// the read sites are agnostic.
    segments: Vec<std::sync::Arc<SealedSegment>>,
}

/// The shared store body — fine-grained latches, one per concern.
///
/// The first revision put the tail, the log, the pins and the entity locks
/// behind ONE `RwLock<State>`, held by every write across timestamp
/// allocation, the chain-hashed log append and the memtable insert, and taken
/// by every read of a non-empty tail. Measured on disjoint keys with no disk:
/// eight threads did half the work of one. Now each concern has the
/// narrowest latch that keeps its own invariant, and nothing holds two of
/// them at once except where the field docs say so.
struct StoreInner {
    sealed: arc_swap::ArcSwap<Sealed>,
    /// The commit log. Timestamps are allocated INSIDE this latch, so the log
    /// order is the commit order and the chain hash serialises exactly what it
    /// must — an allocation plus a hash plus a buffer append, microseconds.
    log: std::sync::Mutex<LogState>,
    /// The mutable tail, sharded — see [`tail`].
    tail: tail::ShardedTail,
    /// The commit clock's ALLOCATION side — a monotonic counter, NOT wall time.
    /// Advanced by `fetch_add` under the log latch; never read by a query.
    next_ts: std::sync::atomic::AtomicU64,
    /// Whether OCC validation may answer from the recent-commit window instead
    /// of a point lookup per key. See `Store::set_commit_window_validation`.
    commit_window: std::sync::atomic::AtomicBool,
    /// Whether the commit-window delta is built from the ts-monotone SUFFIX
    /// above the reader's snapshot rather than by scanning the whole ring.
    /// Default ON; see `Store::set_window_suffix_scan`.
    window_suffix_scan: std::sync::atomic::AtomicBool,
    /// Whether a span read COPIES the tail's rows out one shard at a time
    /// instead of holding all `TAIL_SHARDS` read latches for the whole merge.
    /// See `ShardedTail::range_copied`. Default ON.
    tail_span_copyout: std::sync::atomic::AtomicBool,
    /// Rows a span read will copy before it declines and keeps the borrow
    /// path. See `Store::set_tail_copyout_cap`.
    tail_copyout_cap: std::sync::atomic::AtomicUsize,
    /// Bytes a row-form column read may HOLD in its override map before it
    /// declines to the caller's per-member gather — see `resolve_rows_only`.
    column_scan_byte_budget: std::sync::atomic::AtomicUsize,
    /// The commit clock's VISIBILITY side — what `now_ts` returns and what
    /// every query and every derived structure snapshots at. Lock-free to
    /// read; advanced in timestamp order through `visibility`.
    visible_ts: std::sync::atomic::AtomicU64,
    visibility: Visibility,
    /// Seal sequence allocator.
    next_segment_seq: std::sync::atomic::AtomicU64,
    /// Serialises the two writers of `sealed` (seal and compaction) against
    /// each other, so a load-then-store of the sealed set never loses the
    /// other's publish.
    sealed_swap: std::sync::Mutex<()>,
    /// Pinned snapshot timestamps → pin count. The GC watermark is the
    /// smallest key; compaction may not retire anything a pinned reader can
    /// still ask for.
    pins: std::sync::Mutex<BTreeMap<u64, u32>>,
    /// The cooperative per-entity locks behind `cas`.
    locks: std::sync::Mutex<Locks>,
    /// Writes that bypassed the log (bulk load) — see `put_unlogged`.
    unlogged: std::sync::atomic::AtomicU64,
    /// The number of versions currently in the TAIL. Reads load it (Acquire);
    /// when it is 0 the tail is empty and a read SKIPS the hot latch entirely,
    /// resolving from the lock-free `sealed` segments alone — which is what lets
    /// concurrent reads scale. Correct because a tail version a reader could
    /// miss has `commit_ts` newer than that reader's snapshot anyway, and seal
    /// publishes the new `sealed` BEFORE zeroing this (Release), so a reader that
    /// sees 0 (Acquire) also sees the segment the drained versions moved into.
    tail_nonempty: std::sync::atomic::AtomicUsize,
    /// Property indexes loaded from disk at open (index-at-seal): `prop token →
    /// index`. A reader consults these before rebuilding, so a store opened from
    /// disk does not pay the first-query index build. Empty for a store that was
    /// not opened with sidecar indexes.
    persisted_indexes: arc_swap::ArcSwap<BTreeMap<u32, Arc<crate::index::RangeIndex>>>,
    /// Cross-worker group commit: the durable watermark and the file handle
    /// the fsync is performed on — OUTSIDE the hot lock. See
    /// [`Store::sync_pending`].
    group: std::sync::Mutex<GroupSync>,
    /// Serialises compactions among themselves. The hot lock is NOT held for
    /// the merge — see [`Store::compact`] — so two compactions could otherwise
    /// both rebuild the same sealed set.
    compacting: std::sync::Mutex<()>,
}

/// The state one fsync is shared through.
///
/// Held for the duration of the fsync — deliberately. A worker that arrives
/// while a sync is running blocks on this mutex, and when it gets in it finds
/// either that the sync covered its records (`synced_seq >= need`, done) or
/// that it must run the next one. That is the whole group-commit protocol,
/// with the mutex doing the work a condvar and a `syncing` flag would; the hot
/// lock is NOT held here, so appends from every other worker proceed while the
/// disk works.
struct GroupSync {
    /// Log sequence up to which records are on stable storage.
    synced_seq: u64,
    /// A clone of the WAL's file handle, or `None` for an in-memory store.
    file: Option<std::fs::File>,
}

/// The in-memory store one shard owns. A cheap `Arc` handle — clone shares.
#[derive(Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

// The D2-revision canary. The store WAS `!Sync` "on purpose"; the concurrent-
// write program requires it to cross threads, so `Send + Sync` is now a load-
// bearing property and this fails the BUILD if a future field reintroduces an
// `Rc`/`RefCell`/`!Send` member. (A comment claiming the property is not the
// property — this is.)
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
};

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// What a span merge hands its visitor: the body, and the value when
/// values were asked for (a block row's record is assembled only then).
/// Returning `false` stops the walk.
pub type SpanVisitor<'a> = dyn FnMut(&[u8], Option<&[u8]>) -> bool + 'a;

/// A resolved span — the visible value per key (`None` for a tombstone) —
/// together with the sealed set it was resolved against, so a caller that
/// continues into the segments reads the same set the tail was merged with.
type ResolvedSpan = (
    BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    arc_swap::Guard<std::sync::Arc<Sealed>>,
);

impl Store {
    /// The log latch. Every latch in this store is poison-TOLERANT on purpose:
    /// a panic under a latch is a crash, and this store's crash-consistency
    /// is the WAL's guarantee (log THEN publish, so the memtable is never left
    /// half-published), not the latch's. std's poisoning would convert a
    /// recoverable crash into an unrecoverable one — exactly what
    /// `tests/recovery.rs` asserts must NOT happen: after a crash injected
    /// mid-write, the surviving store still answers reads and recovery
    /// replays the log.
    fn log_mut(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.inner.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pins_mut(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, u32>> {
        self.inner.pins.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn locks_mut(&self) -> std::sync::MutexGuard<'_, Locks> {
        self.inner.locks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark `ts` published to the tail and advance the visible clock over
    /// every consecutive published timestamp. Called by the [`CommitSlot`]
    /// guard, on every path out of a write — including a panic's unwind, so
    /// a writer that dies between allocation and publish cannot hold the
    /// clock back for ever (its version is in the log; recovery replays it).
    /// Lock-free: one store into the ring, then a CAS loop shared with every
    /// other publisher.
    fn publish_ts(&self, ts: u64) {
        use std::sync::atomic::Ordering;
        let ring = &self.inner.visibility.ring;
        let n = ring.len() as u64;
        // A slot is reused every `n` timestamps. Before claiming ours, the
        // clock must have passed the stamp that last used it — otherwise a
        // slow writer's unpublished stamp would be overwritten and skipped.
        let mut waited = 0u32;
        while self.inner.visible_ts.load(Ordering::Acquire) + n <= ts {
            self.advance_visible(); // waiters drive the clock — see `finish`
            backoff(&mut waited);
            stall_report(&mut waited, || {
                format!(
                    "publish_ts ring-lap wait: visible {} + {n} <= ts {ts} (next_ts {})",
                    self.inner.visible_ts.load(Ordering::Relaxed),
                    self.inner.next_ts.load(Ordering::Relaxed),
                )
            });
        }
        ring[(ts % n) as usize].store(ts, Ordering::Release);
        self.advance_visible();
    }

    /// Advance the visible clock over every consecutive published slot.
    fn advance_visible(&self) {
        use std::sync::atomic::Ordering;
        let ring = &self.inner.visibility.ring;
        let n = ring.len() as u64;
        loop {
            let cur = self.inner.visible_ts.load(Ordering::Acquire);
            let next = cur + 1;
            if ring[(next % n) as usize].load(Ordering::Acquire) != next {
                return; // the gap below is still in flight
            }
            // Another publisher may advance past us meanwhile; a lost CAS
            // just means the clock already moved, and the loop re-reads it.
            let _ = self.inner.visible_ts.compare_exchange(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    /// Force the visible clock to at least `ts` — the replay paths, which
    /// publish in log order themselves and may carry gaps (a bulk-loaded
    /// primary allocated timestamps its log never saw). Single-writer by
    /// construction (recovery, replication, open), so a plain max suffices.
    fn advance_visible_to(&self, ts: u64) {
        self.inner
            .visible_ts
            .fetch_max(ts, std::sync::atomic::Ordering::AcqRel);
        self.advance_visible();
    }

    /// Allocate a commit timestamp under the log latch and hand back the
    /// guard that publishes it. The caller appends to the log through the
    /// same guard, then releases the latch BEFORE it touches the tail.
    fn allocate(&self) -> (std::sync::MutexGuard<'_, LogState>, CommitSlot<'_>) {
        let lg = self.log_mut();
        let ts = self.bump_ts();
        (lg, CommitSlot { store: self, ts })
    }

    /// The immutable sealed segments — loaded WITHOUT a lock. Hold the returned
    /// guard only as long as the read needs; a seal/compact swaps a fresh set in.
    fn sealed(&self) -> arc_swap::Guard<std::sync::Arc<Sealed>> {
        self.inner.sealed.load()
    }

    /// Whether the tail currently holds any versions. When false, a read may
    /// resolve from `sealed` alone and never touch a shard latch. `Acquire`
    /// pairs with the `Release` in the write/seal paths.
    fn tail_has_versions(&self) -> bool {
        self.inner
            .tail_nonempty
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Versions in the tail — for the adapter that decides when to seal.
    /// Summed over the shards' own counters (each maintained under its shard
    /// latch), so the hot write path touches no shared counter.
    pub fn tail_versions(&self) -> usize {
        self.inner.tail.versions()
    }

    /// Record that the tail gained versions. A FLAG, not a count, and set
    /// only on the empty→non-empty transition: a store to a line every core
    /// reads invalidates it for all of them, so an unconditional store here
    /// was one more shared write per write. The per-shard counts are what
    /// `tail_versions` sums.
    fn tail_added(&self) {
        use std::sync::atomic::Ordering;
        if self.inner.tail_nonempty.load(Ordering::Relaxed) == 0 {
            self.inner.tail_nonempty.store(1, Ordering::Release);
        }
    }

    /// Publish that the tail is empty (seal drained it). `Release` so a reader
    /// seeing 0 also sees the freshly-swapped `sealed`.
    fn tail_cleared(&self) {
        self.inner
            .tail_nonempty
            .store(0, std::sync::atomic::Ordering::Release);
    }

    /// Reserve the next commit ts, returning it and advancing. Called under the
    /// log latch, so the stamps are handed out in log order.
    fn bump_ts(&self) -> u64 {
        self.inner
            .next_ts
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
    }

    /// Advance the allocation clock to at least `ts` — recovery replays
    /// stamped entries and must not later re-mint a ts the log already used.
    fn advance_ts_to(&self, ts: u64) {
        self.inner
            .next_ts
            .fetch_max(ts, std::sync::atomic::Ordering::AcqRel);
    }

    /// An empty store. `next_ts` starts at 1 so ts 0 can mean "before
    /// everything" in reads.
    pub fn new() -> Self {
        Store {
            inner: Arc::new(StoreInner {
                sealed: arc_swap::ArcSwap::from_pointee(Sealed::default()),
                log: std::sync::Mutex::new(LogState {
                    log: CommitLog::new(),
                    window: std::collections::VecDeque::new(),
                    window_cap: COMMIT_WINDOW_CAP,
                    window_low: 0,
                }),
                tail: tail::ShardedTail::new(),
                next_ts: std::sync::atomic::AtomicU64::new(1),
                commit_window: std::sync::atomic::AtomicBool::new(true),
                window_suffix_scan: std::sync::atomic::AtomicBool::new(true),
                tail_span_copyout: std::sync::atomic::AtomicBool::new(true),
                tail_copyout_cap: std::sync::atomic::AtomicUsize::new(TAIL_COPYOUT_CAP),
                column_scan_byte_budget: std::sync::atomic::AtomicUsize::new(
                    COLUMN_SCAN_BYTE_BUDGET,
                ),
                visible_ts: std::sync::atomic::AtomicU64::new(0),
                visibility: Visibility {
                    ring: (0..VISIBILITY_RING)
                        .map(|_| std::sync::atomic::AtomicU64::new(0))
                        .collect(),
                },
                next_segment_seq: std::sync::atomic::AtomicU64::new(0),
                sealed_swap: std::sync::Mutex::new(()),
                pins: std::sync::Mutex::new(BTreeMap::new()),
                locks: std::sync::Mutex::new(Locks::default()),
                unlogged: std::sync::atomic::AtomicU64::new(0),
                tail_nonempty: std::sync::atomic::AtomicUsize::new(0),
                persisted_indexes: arc_swap::ArcSwap::from_pointee(BTreeMap::new()),
                group: std::sync::Mutex::new(GroupSync {
                    synced_seq: 0,
                    file: None,
                }),
                compacting: std::sync::Mutex::new(()),
            }),
        }
    }

    fn logical_key(prefix: &KeyPrefix, body: &[u8]) -> LogicalKey {
        // The full key minus the timestamp: encode with ts 0 and strip it.
        let mut k = encode_key(prefix, body, 0);
        k.truncate(k.len() - engram_key::COMMIT_TS_LEN);
        k
    }

    fn check_kind(prefix: &KeyPrefix, value: &StoredValue) -> Result<bool, StoreError> {
        if !prefix.kind.is_valid() {
            return Err(StoreError::InvalidKind(prefix.kind.byte()));
        }
        let sealed = matches!(value, StoredValue::Sealed(_));
        if prefix.kind.is_protected() && !sealed {
            // The one unconditional refusal in the store. A silent downgrade
            // from encrypted to plaintext is the dominant defect class in its
            // worst form; this is the physical impossibility the plan asks for.
            return Err(StoreError::ProtectedKindPlaintext {
                kind_byte: prefix.kind.byte(),
            });
        }
        Ok(sealed)
    }

    /// Encode a log payload: `body_len:u32 BE | body | value` (value absent
    /// for a delete). The BODY rides in the payload, not the routing header —
    /// the header is the plaintext side of the split and stays the closed
    /// structural set; under L8 the payload (body included) is ciphertext.
    fn log_payload(body: &[u8], value: Option<&[u8]>) -> Vec<u8> {
        // The third copy of every record's bytes: `rec.encode()` produced them,
        // this concatenates them with the body into a fresh allocation, and the
        // tail then copies them again.
        //
        // COUNTED before deciding whether to remove it. Two changes already
        // shrink this term — volatile guard rows keep two of six rows per edge
        // out of the log entirely, and the log is released at a seal rather
        // than retained for the process lifetime — so the honest order is to
        // measure what is left, not to assume. Removing the concatenation means
        // hashing the parts incrementally and writing from an `IoSlice`, which
        // is cleverness in the WAL path, and that is the last place to put it
        // on an assumption.
        LOG_BYTES_COPIED.fetch_add(
            (4 + body.len() + value.map_or(0, <[u8]>::len)) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut out = Vec::with_capacity(4 + body.len() + value.map_or(0, <[u8]>::len));
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if let Some(v) = value {
            out.extend_from_slice(v);
        }
        out
    }

    fn decode_log_payload(payload: &[u8]) -> Option<(&[u8], &[u8])> {
        let len = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?) as usize;
        let body = payload.get(4..4 + len)?;
        let value = payload.get(4 + len..)?;
        Some((body, value))
    }

    /// Write a value. Assigns the commit timestamp at publish.
    pub fn put(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<u64, StoreError> {
        let sealed = Self::check_kind(prefix, &value)?;
        let key = Self::logical_key(prefix, body);
        // The payload and its digest are built OUTSIDE the log latch — the
        // hash over the record bytes is the bulk of an append's work.
        let payload = Self::log_payload(body, Some(value.bytes()));
        let digest = engram_log::payload_digest(&payload);
        let slot = {
            let (mut lg, slot) = self.allocate();
            crash_point("store.before_log_append");
            lg.log.append_prehashed(
                RoutingHeader {
                    realm: prefix.realm,
                    namespace: prefix.namespace,
                    kind: prefix.kind,
                    partition: prefix.partition,
                    op: Op::Put,
                    commit_ts: slot.ts,
                },
                payload,
                &digest,
            );
            // Durability point: the record must be on stable storage before
            // this write is acknowledged. A no-op for the default in-memory
            // log; an `fsync` for a WAL-backed store (`open_wal`) — or, under
            // group commit, a mark the adapter pays off per batch. A failure
            // here is a durability failure — panic rather than ack a write the
            // disk never took, the same contract as the log append above.
            lg.log
                .sync()
                .unwrap_or_else(|e| panic!("WAL fsync failed (durability): {e}"));
            lg.note_commit(slot.ts, key.clone(), true);
            slot
        };
        // The boundary the WAL rule is about. A crash HERE leaves the entry in
        // the log and nothing in the memtable — recovery REDOES it. Publishing
        // first would invert that into a write the memtable acknowledged and
        // the log never heard of, which no recovery can get back.
        crash_point("store.between_log_and_publish");

        // APPEND — the chain is stored oldest-first and readers walk it from
        // the back. The first shape ever measured against the full production
        // load found the previous `insert(0)` the hard way: `next_id` writes
        // one counter row per created node, so that single hot key's chain
        // grew by one per node and every insert memmoved the whole chain —
        // O(n²) across the load, ~2.5 ms per 116-byte node by 200k nodes.
        // The log latch is NOT held here: one shard's latch, one insert.
        let ts = slot.ts;
        self.inner.tail.push(
            key,
            Version {
                commit_ts: ts,
                value: Some(value.bytes().into()),
                sealed,
            },
        );
        self.tail_added();
        slot.finish(); // publishes `ts` and waits until it is visible
        counted!("store.puts");
        Ok(ts)
    }

    /// Write a value WITHOUT a log entry — the bulk-load contract.
    ///
    /// The WAL rule ("log then publish") exists so recovery is a pure redo;
    /// this put deliberately opts out: the row exists in the memtable (and
    /// in segments after a seal) but the log never hears of it, so a
    /// replay-based recovery will NOT restore it. That is the same contract
    /// every serious bulk importer offers — durability by re-ingest, not by
    /// log — and it removes the per-row chain hash and log allocation that
    /// dominated the measured ingest cost (28M log entries hashed and then
    /// truncated unread on the full production port). The store counts
    /// every unlogged write so nothing about the trade is silent.
    pub fn put_unlogged(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<u64, StoreError> {
        let sealed = Self::check_kind(prefix, &value)?;
        let key = Self::logical_key(prefix, body);
        // No log record, but the allocation still happens UNDER the log latch:
        // the seal and the commit path rely on "no stamp can be allocated
        // while I hold it" to know that waiting for the visible clock drains
        // every in-flight writer.
        let slot = {
            let (mut lg, slot) = self.allocate();
            // An unlogged write is still a COMMITTED write: it reaches the tail
            // and `current_commit_ts` sees it. If the window did not, the two
            // validators would disagree — the window arm would miss a conflict
            // the point loop catches, which is a lost update.
            lg.note_commit(slot.ts, key.clone(), true);
            slot
        };
        let ts = slot.ts;
        self.inner.tail.push(
            key,
            Version {
                commit_ts: ts,
                value: Some(value.bytes().into()),
                sealed,
            },
        );
        self.tail_added();
        slot.finish();
        self.inner
            .unlogged
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counted!("store.unlogged puts");
        sometimes!("store.bulk put skipped the log", true);
        Ok(ts)
    }

    /// Whether OCC validation answers from the recent-commit window instead of
    /// a point lookup per key, under the global commit latch.
    ///
    /// Off restores the point loop and is the differential arm. The two must
    /// return the SAME verdict and name the same conflicting key — the window
    /// is a cheaper way to compute the identical predicate, not a different
    /// one.
    pub fn set_commit_window_validation(&self, on: bool) {
        self.inner
            .commit_window
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a span read COPIES the tail's rows out one shard at a time
    /// (default ON) or holds every shard's read latch for the whole merge.
    ///
    /// OFF restores the borrow path exactly and is the differential arm. The
    /// two must visit BYTE-IDENTICAL rows in the same order — the copy changes
    /// who waits, never what is answered.
    ///
    /// The differential's bar is an ASYMMETRY, not a speedup: `read-only` and
    /// `write-only` must move by less than noise, because the tail is empty in
    /// the first and no span read is issued in the second, while `balanced` and
    /// `write-heavy` move materially. A uniform improvement would mean
    /// something other than writer-exclusion was measured.
    pub fn set_tail_span_copyout(&self, on: bool) {
        self.inner
            .tail_span_copyout
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Rows a span read will copy out before it declines and keeps the
    /// borrow-everything path.
    ///
    /// The cap exists because copying is O(rows) in memory and a full-span walk
    /// over SF1's adjacency is 17.26M rows — the one allocation a
    /// bigger-than-RAM store cannot make. Declining costs writers their wait;
    /// truncating would answer short, which is why the copy returns `None`
    /// rather than a prefix.
    pub fn set_tail_copyout_cap(&self, n: usize) {
        self.inner
            .tail_copyout_cap
            .store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Entries the recent-commit window may hold. `0` disables it. Test-facing:
    /// shrinking it is how the FALLBACK path gets exercised deliberately.
    /// Whether the commit-window delta is built from the ts-monotone SUFFIX
    /// above the reader's snapshot (default ON) or by scanning the whole ring.
    ///
    /// OFF restores the scan exactly and is the differential arm. The two must
    /// produce an IDENTICAL map — same keys, same `(ts, is_put)` — because the
    /// entries below the snapshot were filtered out one by one and contributed
    /// nothing. This is not a relaxation of any rule; it is the same predicate
    /// computed without touching the entries that cannot affect it.
    pub fn set_window_suffix_scan(&self, on: bool) {
        self.inner
            .window_suffix_scan
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Entries the recent-commit window may hold. `0` disables it. Test-facing:
    /// shrinking it is how the FALLBACK path gets exercised deliberately.
    pub fn set_commit_window_capacity(&self, n: usize) {
        let mut lg = self.log_mut();
        lg.window_cap = n;
        while lg.window.len() > n {
            if let Some((dropped_ts, _, _)) = lg.window.pop_front() {
                lg.window_low = lg.window_low.max(dropped_ts + 1);
            }
        }
    }

    /// How many writes bypassed the log — nonzero means log replay is NOT
    /// a complete recovery of this store.
    pub fn unlogged_count(&self) -> u64 {
        self.inner
            .unlogged
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Delete: writes a tombstone version. Readers below it still see the old
    /// value; a reader at or above it sees absence.
    pub fn delete(&self, prefix: &KeyPrefix, body: &[u8]) -> u64 {
        let key = Self::logical_key(prefix, body);
        let payload = Self::log_payload(body, None);
        let digest = engram_log::payload_digest(&payload);
        let slot = {
            let (mut lg, slot) = self.allocate();
            crash_point("store.before_log_append");
            lg.log.append_prehashed(
                RoutingHeader {
                    realm: prefix.realm,
                    namespace: prefix.namespace,
                    kind: prefix.kind,
                    partition: prefix.partition,
                    op: Op::Delete,
                    commit_ts: slot.ts,
                },
                payload,
                &digest,
            );
            // Durability point — see `put`. A no-op for the in-memory log; an
            // `fsync` for a WAL-backed store, before the delete is acknowledged.
            lg.log
                .sync()
                .unwrap_or_else(|e| panic!("WAL fsync failed (durability): {e}"));
            lg.note_commit(slot.ts, key.clone(), false);
            slot
        };
        crash_point("store.between_log_and_publish");

        let ts = slot.ts;
        self.inner.tail.push(
            key,
            Version {
                commit_ts: ts,
                value: None,
                sealed: false,
            },
        );
        self.tail_added();
        slot.finish();
        counted!("store.deletes");
        ts
    }

    /// READ COMMITTED: the newest committed value, whatever it is right now —
    /// where "committed" is the VISIBLE clock: a transaction publishing its
    /// write-set one shard at a time is not committed until its stamp is
    /// visible, and a reader here sees all of it or none of it.
    pub fn get(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<Vec<u8>> {
        self.get_at(prefix, body, self.now_ts())
    }

    /// [`Store::get`] WITHOUT the copy: `f` sees the newest visible value's
    /// bytes as a borrow of the version the store holds (a cached block's row,
    /// a tail entry) and returns what it needs from them. A gather over a
    /// label of wide records — email bodies, embeddings — paid a memcpy of
    /// every record to read one property from each; this reads the property
    /// off the borrowed bytes and copies only that. Resolution is exactly
    /// `get`'s: tail, then segments newest first, at the visible clock.
    pub fn get_with<R>(&self, prefix: &KeyPrefix, body: &[u8], f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let ts = self.now_ts();
        let key = Self::logical_key(prefix, body);
        counted!("store.gets");
        if self.tail_has_versions() {
            if let Some(v) = self.inner.tail.visible_at(&key, ts) {
                return v.value.as_ref().map(|b| f(b));
            }
        }
        let sealed = self.sealed();
        for seg in sealed.segments.iter().rev() {
            if let Some(v) = seg.get_at(&key, ts) {
                sometimes!("store.read served from a sealed segment", true);
                return v.value.as_ref().map(|b| f(b));
            }
        }
        None
    }

    /// [`Store::get`] PROJECTED to `props`: a row-form record comes back
    /// as its bytes; a block row comes back as only the requested
    /// columns, read from the column bytes — the record is never
    /// assembled. A projection over a row with a multi-kilobyte property
    /// the caller did not ask for costs nothing for it. Resolution is
    /// exactly `get`'s: tail, then segments newest first.
    pub fn get_projected(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        props: &[u32],
    ) -> Option<Projected> {
        let key = Self::logical_key(prefix, body);
        counted!("store.gets");
        counted!("store.projected gets");
        let now = self.now_ts();
        if self.tail_has_versions() {
            if let Some(v) = self.inner.tail.visible_at(&key, now) {
                return v.value.map(|b| Projected::Record(b.to_vec()));
            }
        }
        let sealed = self.sealed();
        for seg in sealed.segments.iter().rev() {
            if let Some(v) = seg.get_projected_at(&key, now, props) {
                sometimes!("store.read served from a sealed segment", true);
                return v;
            }
        }
        None
    }

    /// Whether the CURRENT version of a key is sealed ciphertext.
    ///
    /// `None` when the key is absent or tombstoned. A reader that must route
    /// through decryption asks this rather than sniffing the bytes — ciphertext
    /// has no reliable magic, and a sniffer that guesses wrong feeds ciphertext
    /// to a decoder, which reports it as corruption somewhere else entirely.
    pub fn is_sealed(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<bool> {
        let key = Self::logical_key(prefix, body);
        let now = self.now_ts();
        let tail_head = if self.tail_has_versions() {
            self.inner.tail.visible_at(&key, now)
        } else {
            None
        };
        let sealed = self.sealed();
        let head = tail_head.or_else(|| {
            sealed
                .segments
                .iter()
                .rev()
                .find_map(|s| s.get_at(&key, now))
        })?;
        head.value.as_ref()?;
        Some(head.sealed)
    }

    /// Snapshot read, opt-in: the newest version at or below `ts`.
    ///
    /// The opt-in direction matters. Snapshot semantics as the DEFAULT is the
    /// bug L3 exists to prevent; as an explicit request it is a feature.
    ///
    /// Consults the tail first, then sealed segments NEWEST first, and stops at
    /// the first version that satisfies `ts`. The stop is correct because the
    /// tail is strictly newer than every segment and segment N+1 strictly newer
    /// than N — a seal is a fence, so version chains never straddle two places
    /// with interleaved timestamps.
    pub fn get_at(&self, prefix: &KeyPrefix, body: &[u8], ts: u64) -> Option<Vec<u8>> {
        // Clamped to the visible clock: a stamp above it is a write-set
        // still being published, which no reader may see half of.
        let ts = ts.min(self.now_ts());
        let key = Self::logical_key(prefix, body);
        counted!("store.gets");
        if self.tail_has_versions() {
            if let Some(v) = self.inner.tail.visible_at(&key, ts) {
                return v.value.map(|b| b.to_vec());
            }
        }
        let sealed = self.sealed();
        for seg in sealed.segments.iter().rev() {
            if let Some(v) = seg.get_at(&key, ts) {
                sometimes!("store.read served from a sealed segment", true);
                return v.value.map(|b| b.to_vec());
            }
        }
        None
    }

    /// Drop retained log entries below `seq` — the SHIPPED boundary.
    ///
    /// A retention statement (see [`engram_log::CommitLog::truncate_below`]):
    /// the caller asserts everything below `seq` is replicated or archived,
    /// so this process stops holding those payload bytes. Recovery below the
    /// boundary becomes the archive's job, and `Store::recover` over this
    /// store's own suffix correctly REFUSES. Returns entries dropped.
    pub fn truncate_log_below(&self, seq: u64) -> u64 {
        self.log_mut().log.truncate_below(seq)
    }

    /// The current commit clock — the ts a snapshot read taken now would use.
    pub fn now_ts(&self) -> u64 {
        // The VISIBLE clock, not the allocation clock: every version stamped
        // at or below this has reached the tail. A reader that snapshots here
        // and scans sees everything it covers, which is what lets a derived
        // structure stamp itself with this and be judged current by it.
        self.inner
            .visible_ts
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Wait until every ALLOCATED timestamp is visible — for a caller that
    /// holds the log latch (so no new allocation can happen) and needs the
    /// tail to reflect every write that allocated before it. Bounded by
    /// construction: every allocated stamp is published on every path out of
    /// its writer, including a panic's unwind.
    fn wait_for_allocated_to_publish(&self) {
        use std::sync::atomic::Ordering;
        let mut waited = 0u32;
        loop {
            let allocated = self.inner.next_ts.load(Ordering::Acquire) - 1;
            if self.inner.visible_ts.load(Ordering::Acquire) >= allocated {
                return;
            }
            self.advance_visible(); // waiters drive the clock — see `finish`
            backoff(&mut waited);
            stall_report(&mut waited, || {
                let v = self.inner.visible_ts.load(Ordering::Relaxed);
                let ring = &self.inner.visibility.ring;
                let gap = v + 1;
                format!(
                    "wait_for_allocated_to_publish: visible {v} < allocated {allocated}; \
                     ring[{}] holds {} (want {gap})",
                    gap % ring.len() as u64,
                    ring[(gap % ring.len() as u64) as usize]
                        .load(Ordering::Relaxed),
                )
            });
        }
    }

    /// Seal the tail into an immutable segment. Returns the segment seq, or
    /// `None` when the tail is empty — sealing nothing would mint a segment
    /// that exists only to be walked past on every read.
    ///
    /// The WHOLE tail moves: a partial seal would split a version chain across
    /// tail and segment, two places for one truth. Values are untouched — a
    /// seal is a memory event, and version retirement belongs to compaction,
    /// which knows the oldest live reader. Sealing does not.
    pub fn seal(&self) -> Option<u64> {
        // Close allocation FIRST (the log latch), then wait for every stamp
        // already allocated to reach its shard. Without this, a writer that
        // allocated an OLDER stamp before the seal and pushed after it would
        // land in the fresh tail below a NEWER version of its key already in
        // the segment — the "tail is strictly newer than every segment" fence
        // readers stop on would be false, and compaction would then retire
        // the newer version. Lock order: log → shard latches → sealed_swap.
        let _lg = self.log_mut();
        self.wait_for_allocated_to_publish();
        // Every shard's write latch, held across the publish. While they are
        // held no version enters or leaves the tail, and a reader blocked on
        // a shard latch finds, when it gets in, either the tail as it was or
        // the segment the drained versions moved into — never neither.
        let mut all = self.inner.tail.write_all();
        if all.is_empty() {
            return None;
        }
        let entries = all.drain();
        // Build a fresh Sealed (current segments + the new one) and PUBLISH it,
        // then clear the tail counter (Release) — so a lock-free reader that sees
        // the empty tail also sees the segment the drained versions moved into.
        // `sealed_swap` keeps compaction's load-then-store from racing this one,
        // and the seq is taken INSIDE it so segment order and seq order agree.
        let swap = self
            .inner
            .sealed_swap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let seq = self
            .inner
            .next_segment_seq
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let mut segments = self.sealed().segments.clone();
        segments.push(std::sync::Arc::new(SealedSegment::Resident(Segment::new(
            seq, entries,
        ))));
        self.inner
            .sealed
            .store(std::sync::Arc::new(Sealed { segments }));
        self.tail_cleared();
        drop(swap);
        drop(all);
        counted!("store.seals");
        sometimes!("store.sealed a segment", true);
        Some(seq)
    }

    /// Total block count across the PAGED sealed segments — the number of
    /// chunk boundaries `compact_paged_observed` walks.
    ///
    /// Exposed for tests that must show a fixture actually spans more than one
    /// chunk. Without that check a compaction test passes on a single-chunk
    /// corpus and proves nothing about the chunk loop, which is exactly how the
    /// `boundaries[cut - 1]` re-cover survived the existing suite.
    pub fn paged_block_count_for_test(&self) -> usize {
        self.sealed()
            .segments
            .iter()
            .filter_map(|s| match s.as_ref() {
                SealedSegment::Paged(p) => Some(p.block_first_keys().len()),
                SealedSegment::Resident(_) => None,
            })
            .sum()
    }

    /// Sealed segment count.
    pub fn segment_count(&self) -> usize {
        self.sealed().segments.len()
    }

    /// Sealed segments that are still RESIDENT — sealed but not yet spilled
    /// to a paged file, so a crash loses them exactly as it loses the
    /// unsealed tail. Zero, together with a zero tail, is what "everything
    /// this store holds is on disk" means for a paged store; a checkpoint
    /// reports it rather than assuming its own spill left nothing behind.
    pub fn resident_segment_count(&self) -> usize {
        self.sealed()
            .segments
            .iter()
            .filter(|s| matches!(s.as_ref(), SealedSegment::Resident(_)))
            .count()
    }

    /// The IDENTITY of the current sealed set: a hash over every segment's
    /// `(seq, max_commit_ts)`, in the set's own order.
    ///
    /// This exists because a derived structure persisted to disk needs a
    /// vintage, and **a timestamp is not one**. A stamp says "these rows were
    /// current at T"; it cannot distinguish that set of segments from one with
    /// a segment added, removed, or re-merged underneath it — all of which
    /// leave the clock exactly where it was and all of which make a persisted
    /// CSR describe rows the store no longer holds. That failure is silent and
    /// checksums clean, which is why the vintage is the set and not the clock.
    ///
    /// `max_commit_ts` is included as well as `seq` so that a segment REWRITTEN
    /// under the same seq — which compaction does not do today, but which a
    /// future tiering could — still changes the identity.
    pub fn sealed_set_id(&self) -> u64 {
        let sealed = self.sealed();
        let mut h = blake3::Hasher::new();
        h.update(&(sealed.segments.len() as u64).to_le_bytes());
        for seg in sealed.segments.iter() {
            h.update(&seg.seq().to_le_bytes());
            h.update(&seg.max_commit_ts().to_le_bytes());
        }
        u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8"))
    }

    /// The fraction of versions across the RESIDENT sealed segments that are
    /// tombstones, and the total version count they were measured over.
    ///
    /// Compaction was scheduled purely by segment COUNT, which cannot tell a
    /// segment of live rows from one that is mostly deletions waiting to be
    /// reclaimed. Under a create/delete churn the tombstones accumulate and
    /// every scan, every prefix walk and every `merge_span` keeps paying for
    /// them until the count threshold happens to fire.
    ///
    /// Resident segments only: a paged segment's tombstone count lives in its
    /// footer, which is a FORMAT change, and until it is written a paged
    /// segment simply does not contribute. The ratio is therefore a FLOOR over
    /// what the store holds, never an overstatement — a trigger built on it
    /// fires late rather than spuriously.
    pub fn tombstone_ratio(&self) -> (f64, u64) {
        let sealed = self.sealed();
        let mut dead = 0u64;
        let mut total = 0u64;
        for seg in sealed.segments.iter() {
            match seg.as_ref() {
                SealedSegment::Resident(r) => {
                    dead += r.tombstones();
                    total += r.versions();
                }
                // A v3 file carries the counts in its footer, so a paged
                // segment contributes without being opened. A v2 file says
                // nothing and is skipped — which keeps the ratio a FLOOR over
                // a mixed store rather than a guess.
                SealedSegment::Paged(p) => {
                    if let Some((t, v)) = p.tombstone_counts() {
                        dead += t;
                        total += v;
                    }
                }
            }
        }
        if total == 0 {
            return (0.0, 0);
        }
        (dead as f64 / total as f64, total)
    }

    /// Convert this store to **paged** storage: seal the tail, write every
    /// resident sealed segment to a file under `dir`, and swap the sealed set to
    /// `Paged` backings that read those files block-by-block through a shared
    /// [`BlockCache`](crate::paged::BlockCache) of `cache_bytes`. After this the
    /// resident segment bodies are dropped (their `Arc`s released once no reader
    /// holds them) and steady-state memory is bounded by the cache + the small
    /// per-segment anchors — the point of `paged` mode. Reads answer identically
    /// (Track B M1 differential); only block resolution changes. Idempotent for
    /// already-paged segments (kept as-is). The returned `Arc` is the live cache
    /// (share it, inspect its budget, keep it alive).
    pub fn into_paged(
        &self,
        dir: &std::path::Path,
        cache_bytes: usize,
    ) -> std::io::Result<std::sync::Arc<crate::paged::BlockCache>> {
        self.seal(); // drain any tail into a segment first (None if already empty)
        let cache = crate::paged::BlockCache::new(cache_bytes);
        // Under the swap latch: a seal completing between this load and the
        // store below would otherwise be dropped from the set.
        let _swap = self
            .inner
            .sealed_swap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sealed = self.sealed();
        let mut new_segs: Vec<std::sync::Arc<SealedSegment>> =
            Vec::with_capacity(sealed.segments.len());
        for seg in &sealed.segments {
            match seg.as_resident() {
                Some(resident) => {
                    let path = dir.join(format!("seg-{:020}.seg", resident.seq));
                    crate::sst::write_segment_file(resident, &path)?;
                    let paged = SealedSegment::open_paged(&path, std::sync::Arc::clone(&cache))
                        .map_err(|e| std::io::Error::other(format!("{e}")))?;
                    new_segs.push(std::sync::Arc::new(paged));
                }
                // Already paged (a re-invocation): keep the existing backing.
                None => new_segs.push(std::sync::Arc::clone(seg)),
            }
        }
        drop(sealed);
        self.inner
            .sealed
            .store(std::sync::Arc::new(Sealed { segments: new_segs }));
        counted!("store.converted to paged");
        Ok(cache)
    }

    /// Write every RESIDENT sealed segment to a file under `dir` and swap it to
    /// a `Paged` backing reading through the CALLER's `cache` — the repeatable
    /// spill behind bigger-than-RAM serving. Unlike [`Store::into_paged`] this
    /// does not seal (seal policy belongs to the caller) and does not mint a
    /// cache per call: every spill over a server's lifetime must share ONE
    /// budget, or N spills would carry N budgets and the bound the cache exists
    /// for would grow with uptime. Returns the number of segments converted —
    /// 0 when everything sealed is already paged.
    pub fn spill_sealed_into(
        &self,
        dir: &std::path::Path,
        cache: &std::sync::Arc<crate::paged::BlockCache>,
    ) -> std::io::Result<usize> {
        // Under the swap latch: a seal completing between this load and the
        // store below would otherwise be dropped from the set.
        let _swap = self
            .inner
            .sealed_swap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sealed = self.sealed();
        let mut new_segs: Vec<std::sync::Arc<SealedSegment>> =
            Vec::with_capacity(sealed.segments.len());
        let mut converted = 0usize;
        for seg in &sealed.segments {
            match seg.as_resident() {
                Some(resident) => {
                    let path = dir.join(format!("seg-{:020}.seg", resident.seq));
                    crate::sst::write_segment_file(resident, &path)?;
                    let paged = SealedSegment::open_paged(&path, std::sync::Arc::clone(cache))
                        .map_err(|e| std::io::Error::other(format!("{e}")))?;
                    new_segs.push(std::sync::Arc::new(paged));
                    converted += 1;
                }
                // Already paged (an earlier spill): keep the existing backing.
                None => new_segs.push(std::sync::Arc::clone(seg)),
            }
        }
        drop(sealed);
        // Nothing converted means the set on disk IS the live set — swapping an
        // identical clone in would only churn readers' guards.
        if converted > 0 {
            self.inner
                .sealed
                .store(std::sync::Arc::new(Sealed { segments: new_segs }));
            counted!("store.spilled sealed to paged");
        }
        Ok(converted)
    }

    /// Open a store whose sealed segments are read PAGED from the `seg-<seq>.seg`
    /// files in `dir` (as [`Store::into_paged`] writes them) — the durable open
    /// path. Unlike `into_paged`, this **never loads the graph resident**: each
    /// segment's open `pread`s only its footer + index anchors, and block bytes
    /// fault in on demand through the shared cache. So opening a graph larger
    /// than RAM costs only the anchors + the cache budget — the bigger-than-RAM
    /// property. The store starts with an EMPTY tail (all data is in the sealed
    /// segments); a caller may resume writes on top, and `next_segment_seq`
    /// continues past the highest seq on disk.
    pub fn open_paged_dir(
        dir: &std::path::Path,
        cache_bytes: usize,
    ) -> std::io::Result<(Store, std::sync::Arc<crate::paged::BlockCache>)> {
        let store = Store::new();
        let cache = crate::paged::BlockCache::new(cache_bytes);
        // Collect the segment files, ordered by seq (oldest first, as sealed).
        let mut files: Vec<(u64, std::path::PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let Some(seq) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("seg-"))
                .and_then(|n| n.strip_suffix(".seg"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue; // not a segment file — ignore
            };
            files.push((seq, path));
        }
        files.sort();
        let mut segs: Vec<std::sync::Arc<SealedSegment>> = Vec::with_capacity(files.len());
        let mut next_seq = 0u64;
        let mut max_commit_ts = 0u64;
        for (seq, path) in files {
            let seg = SealedSegment::open_paged(&path, std::sync::Arc::clone(&cache))
                .map_err(|e| std::io::Error::other(format!("{e}")))?;
            next_seq = next_seq.max(seq + 1);
            max_commit_ts = max_commit_ts.max(seg.max_commit_ts());
            segs.push(std::sync::Arc::new(seg));
        }
        store
            .inner
            .sealed
            .store(std::sync::Arc::new(Sealed { segments: segs }));
        store
            .inner
            .next_segment_seq
            .store(next_seq, std::sync::atomic::Ordering::Release);
        // Advance the commit clock PAST the segments' newest version, so every
        // snapshot read at `now_ts` sees the on-disk data (a fresh store starts
        // its clock at 0, below every real commit — without this, traversals and
        // index builds read an empty graph).
        store.advance_ts_to(max_commit_ts + 1);
        store.advance_visible_to(max_commit_ts);
        // Load any persisted property indexes (`idx-<token>.idx`) so the first
        // query need not rebuild them (index-at-seal). A corrupt/foreign file is
        // skipped — the store rebuilds that index on demand as usual.
        let mut loaded: BTreeMap<u32, Arc<crate::index::RangeIndex>> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Some(token) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("idx-"))
                    .and_then(|n| n.strip_suffix(".idx"))
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                if let Ok(bytes) = std::fs::read(&path) {
                    let def = crate::index::IndexDef::new(token, crate::record::PropertyId(token));
                    if let Some(idx) = crate::index::RangeIndex::from_bytes(&bytes, def) {
                        loaded.insert(token, Arc::new(idx));
                    }
                }
            }
        }
        if !loaded.is_empty() {
            counted!("store.loaded persisted indexes");
            store.inner.persisted_indexes.store(Arc::new(loaded));
        }
        counted!("store.opened paged from disk");
        Ok((store, cache))
    }

    /// A property index persisted at open (index-at-seal), if any — a reader
    /// consults this before rebuilding. `None` once any write advances the clock
    /// past the index's vintage (the caller checks `as_of`).
    pub fn persisted_index(&self, token: u32) -> Option<Arc<crate::index::RangeIndex>> {
        self.inner.persisted_indexes.load().get(&token).cloned()
    }

    /// Write a property index to `dir/idx-<token>.idx` (atomic tmp+rename), so a
    /// later [`Store::open_paged_dir`] loads it instead of rebuilding. The token
    /// names the file; the index carries its own vintage + BLAKE3.
    pub fn write_index_sidecar(
        dir: &std::path::Path,
        token: u32,
        idx: &crate::index::RangeIndex,
    ) -> std::io::Result<()> {
        let path = dir.join(format!("idx-{token}.idx"));
        let tmp = path.with_extension("idxtmp");
        std::fs::write(&tmp, idx.to_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// The sealed segments, for the on-disk-format round-trip test (M0). Not a
    /// public read path — segments are an implementation detail of the LSM.
    #[cfg(test)]
    pub(crate) fn sealed_segments_for_test(&self) -> Vec<std::sync::Arc<SealedSegment>> {
        self.sealed().segments.clone()
    }

    /// Rewrite every sealed segment to a file in `dir` and swap the sealed set
    /// to PAGED backings reading those files through `cache` — the M1.1
    /// differential harness. After this, the SAME public read API serves from
    /// disk-via-cache; results must equal the resident answers.
    #[cfg(test)]
    pub(crate) fn rebind_sealed_paged(
        &self,
        dir: &std::path::Path,
        cache: std::sync::Arc<crate::paged::BlockCache>,
    ) {
        let sealed = self.sealed();
        let mut new_segs: Vec<std::sync::Arc<SealedSegment>> = Vec::new();
        for seg in &sealed.segments {
            let resident = seg.as_resident().expect("rebind expects resident segments");
            let path = dir.join(format!("seg-{}.seg", resident.seq));
            crate::sst::write_segment_file(resident, &path).expect("write segment to disk");
            new_segs.push(std::sync::Arc::new(
                SealedSegment::open_paged(&path, std::sync::Arc::clone(&cache))
                    .expect("open paged"),
            ));
        }
        drop(sealed);
        self.inner
            .sealed
            .store(std::sync::Arc::new(Sealed { segments: new_segs }));
    }

    /// Pin the current commit timestamp for a long-lived snapshot reader.
    ///
    /// While the guard lives, compaction may not retire any version the reader
    /// could ask for at this timestamp. This is the plan's named contention
    /// scenario — "a long-running reader pinning the GC watermark" — made a
    /// first-class object instead of an accident: the pin is explicit, counted,
    /// and released by Drop, so a leaked reader is a visible pin rather than an
    /// invisible correctness dependency.
    pub fn pin_snapshot(&self) -> SnapshotPin {
        // The clock is read UNDER the pins latch. Read before it, a commit
        // and a compaction could both land in between: the compaction's
        // watermark (taken under the same latch, with no pin yet) would sit
        // ABOVE this pin's timestamp, and retire the very versions the pinned
        // reader is about to ask for.
        let mut pins = self.pins_mut();
        let ts = self.now_ts();
        *pins.entry(ts).or_insert(0) += 1;
        drop(pins);
        SnapshotPin {
            store: self.clone(),
            ts,
        }
    }

    /// The GC watermark: the oldest pinned timestamp, or the current clock
    /// when nothing is pinned. Everything strictly reachable only by readers
    /// BELOW the watermark is retirable.
    pub fn gc_watermark(&self) -> u64 {
        self.pins_mut()
            .keys()
            .next()
            .copied()
            .unwrap_or_else(|| self.now_ts())
    }

    /// What survives compaction for ONE key, in ascending-ts order.
    ///
    /// Extracted so the resident compactor and the streaming paged one cannot
    /// diverge: the rule that decides what a reader can still reach is the one
    /// place where a difference between them would be a silent wrong answer
    /// rather than a performance difference.
    ///
    /// `shadowing_base` says whether an UNTOUCHED older segment still holds
    /// this key. It is always false for a FULL compaction (nothing is left
    /// untouched), which is why the streaming path can pass `false`.
    fn retain_chain(
        chain: Vec<Version>,
        watermark: u64,
        shadowing_base: bool,
        retired: &mut u64,
    ) -> Vec<Version> {
        let mut kept: Vec<Version> = Vec::new();
        let mut newest_at_or_below: Option<Version> = None;
        // Walk newest -> oldest (the chain is stored ascending).
        for v in chain.into_iter().rev() {
            if v.commit_ts > watermark {
                kept.push(v);
            } else if newest_at_or_below.is_none() {
                newest_at_or_below = Some(v);
            } else {
                *retired += 1;
                sometimes!("store.compaction retired a version", true);
            }
        }
        if let Some(v) = newest_at_or_below {
            if v.value.is_none() {
                if shadowing_base {
                    sometimes!("store.compaction kept a tombstone over the base", true);
                    kept.push(v);
                } else {
                    *retired += 1;
                    sometimes!("store.compaction purged a tombstone", true);
                }
            } else {
                kept.push(v);
            }
        }
        kept.reverse(); // collected newest -> oldest; stored ascending
        kept
    }

    /// Merge every sealed segment into one, retiring versions no reader can
    /// reach. The tail and the log are untouched: compaction is a SEGMENT
    /// LAYOUT event — the log stays the full durable history, which is what
    /// keeps PITR able to reach states compaction has retired from the live
    /// tree.
    ///
    /// Retention per key, against the watermark W:
    ///
    ///  - every version with `ts > W` is kept — a current reader may ask;
    ///  - the NEWEST version with `ts <= W` is kept, because a reader pinned
    ///    exactly at W still resolves to it;
    ///  - …unless that version is a TOMBSTONE: with every older version
    ///    already retired, absence-by-missing and absence-by-tombstone answer
    ///    identically for every reachable timestamp, so the tombstone itself
    ///    retires — whether or not newer versions survive above it. (The first
    ///    version additionally required "nothing newer survives"; a canary
    ///    showed that condition distinguishes NOTHING a reader can observe,
    ///    and an unobservable branch is dead logic wearing a safety look.)
    ///    This is the only place a tombstone dies, and it is gated on the same
    ///    watermark as everything else.
    ///
    /// Returns (versions retired, keys dropped entirely).
    ///
    /// # Online
    ///
    /// The merge runs WITHOUT the hot lock. Segments are immutable `Arc`s, so
    /// reading them needs no latch; writes meanwhile land in the tail, which
    /// this never touches; and a seal meanwhile only APPENDS a segment, which
    /// is carried over at the swap. The lock is held twice, briefly: to take
    /// the watermark, the segment seq and the sealed set at the start, and to
    /// swap the compacted set in at the end. The first version held it for
    /// the whole merge — every reader and writer stalled for the duration of
    /// an O(corpus) rewrite, which is why nothing called it in production.
    ///
    /// The watermark taken at the start stays safe for the whole merge: a
    /// reader pins at the clock of the moment it pins, which is at or past the
    /// watermark, and the newest version at or below the watermark is kept.
    /// The seq taken at the start keeps segments ascending: everything merged
    /// is older than any segment sealed during the merge.
    pub fn compact(&self) -> (u64, u64) {
        let _one = self
            .inner
            .compacting
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (watermark, seq, old) = {
            let watermark = self.gc_watermark();
            // The sealed set and the seq are taken together under the swap
            // latch: a seal landing between them would take a LOWER seq than
            // the merged run that ends up before it, and a durable reopen
            // (which orders segments by seq) would then let the older merged
            // run shadow the newer seal.
            let _swap = self
                .inner
                .sealed_swap
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let old = self.inner.sealed.load_full();
            if old.segments.is_empty() {
                return (0, 0);
            }
            let seq = self
                .inner
                .next_segment_seq
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            (watermark, seq, old)
        };

        // ── Tiering: merge the YOUNG run, keep a dominant base ──────────────
        //
        // The first version merged every segment every time: O(corpus) per
        // compaction, and under a write load a seal landed every few seconds,
        // so the merge was re-triggered before it finished — 22 s rewrites,
        // back to back, of a corpus that had changed by 1%. Walking from the
        // newest segment, the young run grows until the next-older segment is
        // more than TWICE everything younger combined; that segment and
        // everything below it is the base and is left alone. The base is merged
        // only when the young run reaches half its size — amortised O(n log n)
        // over a store's life, the classic size-tiered shape at ratio 2. A
        // paged (on-disk) segment counts as infinitely large, so it is always
        // base.
        let sizes: Vec<usize> = old
            .segments
            .iter()
            .map(|s| s.as_resident().map_or(usize::MAX, Segment::len))
            .collect();
        let mut young_from = old.segments.len();
        let mut young_total = 0usize;
        while young_from > 0 {
            let j = young_from - 1;
            if young_from < old.segments.len() && sizes[j] > young_total.saturating_mul(2) {
                break;
            }
            young_total = young_total.saturating_add(sizes[j]);
            young_from = j;
        }
        let base: Vec<std::sync::Arc<SealedSegment>> = old.segments[..young_from].to_vec();
        if !base.is_empty() {
            sometimes!("store.compaction kept a base tier", true);
        }

        // Merge the young segments OLDEST first so chains concatenate in the
        // ascending-ts order the store keeps everywhere (segment N+1 is
        // strictly newer than N). Clone (not consume): a reader may still hold
        // these `Arc<Segment>`s.
        let mut merged: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
        for seg in &old.segments[young_from..] {
            for (key, chain) in seg.cloned_entries() {
                merged.entry(key).or_default().extend(chain);
            }
        }

        let mut retired = 0u64;
        let mut dropped_keys = 0u64;
        let mut out: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
        for (key, chain) in merged {
            // Absence-by-missing == absence-by-tombstone for every reachable ts
            // — PROVIDED nothing older survives anywhere. Within the young run
            // every older version is retired below; the base is untouched, so a
            // version there would resurface the moment the tombstone went.
            let shadowing_base = base
                .iter()
                .rev()
                .any(|b| b.get_at(&key, u64::MAX).is_some());
            let kept = Self::retain_chain(chain, watermark, shadowing_base, &mut retired);
            if kept.is_empty() {
                dropped_keys += 1;
            } else {
                out.insert(key, kept);
            }
        }

        // The base stays in front, untouched; the merged young run follows it
        // (its seq was taken after every young segment's, so order holds).
        let mut new_segments: Vec<std::sync::Arc<SealedSegment>> = base;
        if !out.is_empty() {
            // The census's head/tail split, applied where the LSM rewrites
            // for reads anyway: signature groups big enough to matter become
            // column blocks; the tail of rare signatures, multi-version
            // chains and sealed values stays in row form.
            let blocks = columnar::build_blocks(&mut out, columnar::COLUMNAR_MIN_ROWS);
            new_segments.push(std::sync::Arc::new(SealedSegment::Resident(
                Segment::with_blocks(seq, out, blocks),
            )));
        }
        // Swap: the compacted segment replaces exactly the segments merged,
        // and any segment sealed DURING the merge — appended after them, and
        // newer than all of them — is carried over behind it. The old
        // `Arc<Segment>`s stay alive for any reader still holding a guard.
        let _swap = self
            .inner
            .sealed_swap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = self.inner.sealed.load_full();
        let merged_n = old.segments.len();
        let prefix_intact = current.segments.len() >= merged_n
            && current.segments[..merged_n]
                .iter()
                .zip(&old.segments)
                .all(|(a, b)| std::sync::Arc::ptr_eq(a, b));
        if !prefix_intact {
            // Cannot happen — seals only append and compactions are serialised
            // — but a wrong assumption here would drop data, so it refuses
            // rather than trusts.
            counted!("store.compaction abandoned: sealed set changed underneath");
            return (0, 0);
        }
        if current.segments.len() > merged_n {
            sometimes!("store.compaction carried over a segment sealed meanwhile", true);
            new_segments.extend(current.segments[merged_n..].iter().cloned());
        }
        self.inner.sealed.store(std::sync::Arc::new(Sealed {
            segments: new_segments,
        }));
        counted!("store.compactions");
        if self.pins_mut().keys().next().is_some() {
            sometimes!("store.compaction ran under a pinned reader", true);
        }
        (retired, dropped_keys)
    }

    /// Committed log entries — a CDC consumer's read, from a cursor.
    pub fn log_tail(&self, from_seq: u64) -> Vec<Entry> {
        self.log_mut().log.tail(from_seq).to_vec()
    }

    /// Log length.
    pub fn log_len(&self) -> u64 {
        self.log_mut().log.len()
    }

    /// The log's chain head — the value the root beacon publishes.
    pub fn log_head(&self) -> [u8; 32] {
        self.log_mut().log.head()
    }

    /// Verify the log's hash chain.
    pub fn verify_log(&self) -> engram_log::ChainVerify {
        self.log_mut().log.verify()
    }

    /// Scan every live row under a full prefix, at READ COMMITTED.
    pub fn scan(&self, prefix: &KeyPrefix) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_at(prefix, u64::MAX)
    }

    /// Scan every live row under a full prefix at or below `ts`, bodies
    /// ascending — the contiguous range scan the key layout was designed for.
    ///
    /// The range is the encoded prefix as a BYTE PREFIX of the logical key:
    /// partition sits before the body in the tuple, so one partition's rows
    /// are one contiguous run and the scan never visits another partition's
    /// keys, another kind's, or another tenant's. That containment is the
    /// L2 property, exercised through the read path rather than asserted
    /// about the encoding in isolation.
    ///
    /// Tombstones are resolved per key BEFORE the row is emitted: a key whose
    /// newest visible version is a delete is absent from the result, not
    /// present-with-empty-value.
    pub fn scan_at(&self, prefix: &KeyPrefix, ts: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_span(prefix, &[], ts)
    }

    /// Scan live rows whose BODY starts with `body_prefix` — the O(matches)
    /// read the adjacency and membership row shapes were designed for. The
    /// bench harness found the difference the hard way: a per-call full-
    /// partition scan made every 1-hop step O(total rows), and 20k steps
    /// over a 10k-node graph visited billions of rows. Bodies come back
    /// FULL (prefix included).
    pub fn scan_body_prefix(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_span(prefix, body_prefix, u64::MAX)
    }

    fn scan_span(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        ts: u64,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + body_prefix.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        lo.extend_from_slice(body_prefix);
        // The exclusive upper bound: the prefix with its last byte run
        // incremented, carrying into earlier bytes. All-0xFF (impossible for
        // a 13-byte encoded prefix ending in a partition, but handled anyway)
        // means "to the end of the keyspace".
        let hi = {
            let mut h = lo.clone();
            let mut i = h.len();
            loop {
                if i == 0 {
                    break None;
                }
                i -= 1;
                if h[i] != 0xFF {
                    h[i] += 1;
                    h.truncate(i + 1);
                    break Some(h);
                }
            }
        };

        // Tail-gate: only touch the shard latches when the tail actually holds
        // versions. After a seal (the entire read benchmark) the tail is empty,
        // so span scans go lock-free over the immutable segments — the same gate
        // the point reads use, extended to the scan path (adjacency expansion
        // and column scans), whose ungated lock was the concurrent complex-join
        // collapse. The sealed set is loaded AFTER the tail is read (inside
        // `resolve_span`): loaded before, a seal landing in between would
        // drain the tail into a segment this scan does not hold.
        let ts = ts.min(self.now_ts());
        let tail = self.tail_has_versions().then_some(&self.inner.tail);
        let (out, _sealed) = self.resolve_span(tail, &lo, hi.as_deref(), ts);

        counted!("store.scans");
        out.into_iter()
            .filter_map(|(k, v)| v.map(|value| (k[strip..].to_vec(), value)))
            .collect()
    }

    /// Resolve every key in `[lo, hi)` to its newest version at or below
    /// `ts` — tail first, then segments newest first, rows and blocks alike.
    /// Tombstones come back as `None` values: the CALLER decides whether a
    /// suppressed key matters (a plain scan drops it; the column scan needs
    /// it to keep a deleted key from resurrecting out of a block).
    fn resolve_span(
        &self,
        tail: Option<&tail::ShardedTail>,
        lo: &[u8],
        hi: Option<&[u8]>,
        ts: u64,
    ) -> ResolvedSpan {
        let mut out: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let visible = |versions: &[Version]| -> Option<Option<Vec<u8>>> {
            versions
                .iter()
                .rev()
                .find(|v| v.commit_ts <= ts)
                // One copy at the PUBLIC boundary: `ResolvedSpan` hands owned
                // bytes to callers outside this crate. Internally the version
                // is now carried by refcount.
                .map(|v| v.value.as_ref().map(|b| b.to_vec()))
        };

        // Tail first: the newest generation wins, so a key resolved here is
        // final and segment hits for it are skipped below. Skipped entirely
        // when the tail is empty — the caller passes None and never touched
        // a shard latch, so the segments below are the whole answer. The
        // shards arrive in shard order; `out` is a map, so key order holds.
        if let Some(t) = tail {
            t.for_each_in_range(lo, hi, |k, versions| {
                debug_assert!(k.starts_with(lo));
                if let Some(v) = visible(versions) {
                    out.insert(k.clone(), v);
                }
            });
        }
        // The sealed set, loaded AFTER the tail: a seal that landed while the
        // tail was read moved versions into a segment this load holds.
        let sealed = self.sealed();

        // Segments, newest first; first resolution per key wins. Within one
        // segment, rows and blocks are disjoint, so their relative order is
        // a convention rather than a correctness point.
        for seg in sealed.segments.iter().rev() {
            seg.range_for_each(lo, hi, |k, versions| {
                if out.contains_key(k) {
                    return;
                }
                if let Some(v) = visible(versions) {
                    out.insert(k.clone(), v);
                }
            });
            for b in seg.blocks() {
                for row in b.rows_in_range(lo, hi) {
                    let k = &b.keys[row];
                    if out.contains_key(k) || b.commit_ts[row] > ts {
                        continue;
                    }
                    sometimes!("store.columnar scan served a block row", true);
                    out.insert(k.clone(), Some(b.value_at(row)));
                }
            }
        }
        (out, sealed)
    }

    /// Project ONE property across live rows whose body starts with
    /// `body_prefix` — the no-transpose read the head/tail layout exists
    /// for. Rows sitting in a column block are served from that column's
    /// bytes alone (no record decode, counted); rows still in row form fall
    /// back to a record decode (also counted). Rows lacking the property are
    /// absent from the result, exactly as a `n.prop` projection would treat
    /// them. Returns `(full body, tagged value bytes)` pairs in key order.
    pub fn scan_column_at(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        prop: u32,
        ts: u64,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + body_prefix.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        lo.extend_from_slice(body_prefix);
        let hi = key_successor(&lo);
        self.column_scan_in(&lo, hi.as_deref(), strip, prop, ts, usize::MAX)
            .expect("an unbounded budget never aborts")
    }

    /// [`Store::scan_column_at`] restricted to bodies in `[lo_body, hi_body)`
    /// (`None` = to the end of the partition) and ABORTED past `budget`
    /// entries: `None` means the column holds more rows in that range than
    /// the caller was willing to read. A columnar aggregate over a label
    /// reads each demanded property as a column; a property shared by the
    /// whole graph (`id`, `name`, `status`) is 1.79M entries however small
    /// the label — measured as nine statements going from ~0 ms to 0.4–3.2 s
    /// on the production port. The range bounds the read to the label's id
    /// span (tight for bulk-loaded labels), and the budget bounds the waste
    /// when the span is wide: the caller declines to its per-id path.
    pub fn scan_column_range_at(
        &self,
        prefix: &KeyPrefix,
        lo_body: &[u8],
        hi_body: Option<&[u8]>,
        prop: u32,
        ts: u64,
        budget: usize,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + lo_body.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        let hi = match hi_body {
            Some(h) => {
                let mut k = lo.clone();
                k.extend_from_slice(h);
                Some(k)
            }
            None => key_successor(&lo),
        };
        lo.extend_from_slice(lo_body);
        self.column_scan_in(&lo, hi.as_deref(), strip, prop, ts, budget)
    }

    /// [`Store::scan_column_range_at`] for PRESENCE only: the bodies that
    /// carry the property in `[lo_body, hi_body)`, no value copied, no
    /// value decoded — `WHERE n.content IS NOT NULL` over 1.79M nodes
    /// copied and decoded 1.79M strings to test whether each existed.
    /// `None` past `budget` entries, as for the value scan.
    pub fn scan_column_presence_at(
        &self,
        prefix: &KeyPrefix,
        lo_body: &[u8],
        hi_body: Option<&[u8]>,
        prop: u32,
        ts: u64,
        budget: usize,
    ) -> Option<Vec<Vec<u8>>> {
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + lo_body.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        let hi = match hi_body {
            Some(h) => {
                let mut k = lo.clone();
                k.extend_from_slice(h);
                Some(k)
            }
            None => key_successor(&lo),
        };
        lo.extend_from_slice(lo_body);
        let ts = ts.min(self.now_ts());
        let tail = self.tail_has_versions().then_some(&self.inner.tail);
        counted!("store.column presence scans");
        // The row-form walk is budgeted on rows VISITED (see
        // `resolve_rows_only`): a span wider than the budget declines here,
        // before any presence is tested, exactly as it declines below once
        // the hits exceed it — the caller's per-id path answers either way.
        let Some((overrides, sealed)) = self.resolve_rows_only(tail, &lo, hi.as_deref(), ts, budget)
        else {
            counted!("store.column presence scan declined on rows visited");
            return None;
        };
        let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
        for seg in sealed.segments.iter().rev() {
            for b in seg.blocks() {
                let Some(col) = b.column_of(prop) else {
                    continue;
                };
                let _ = col; // the column's presence is the row's presence
                for row in b.rows_in_range(&lo, hi.as_deref()) {
                    let k = &b.keys[row];
                    if overrides.contains_key(k) || out.contains(k) || b.commit_ts[row] > ts {
                        continue;
                    }
                    sometimes!("store.column presence scan served a block row", true);
                    out.insert(k.clone());
                    if out.len() > budget {
                        sometimes!("store.column presence scan aborted on its budget", true);
                        return None;
                    }
                }
            }
        }
        for (k, v) in overrides {
            let Some(bytes) = v else { continue }; // tombstone — stays gone
            let Ok(rec) = Record::decode(&bytes) else {
                continue;
            };
            if rec.get(PropertyId(prop)).is_some() {
                sometimes!("store.column presence scan fell back to a row", true);
                out.insert(k);
                if out.len() > budget {
                    sometimes!("store.column presence scan aborted on its budget", true);
                    return None;
                }
            }
        }
        Some(out.into_iter().map(|k| k[strip..].to_vec()).collect())
    }

    /// The column read as a VISITOR: every live `(body, tagged value)`
    /// in `[lo_body, hi_body)` carrying `prop` is handed to `f` as
    /// BORROWED slices — block rows straight from the column's bytes,
    /// row-form rows from one record decode — with no map, no key clone
    /// and no value clone per entry. Block rows arrive block by block
    /// (key order within a block; blocks newest segment first), so a key
    /// blocked in two segments is handed over twice, newest first: a
    /// caller wanting one value per key keeps the first it sees. The
    /// row-form rows follow the blocks. `None` past `budget` entries.
    ///
    /// `scan_column_range_at` served the same entries through a
    /// `BTreeMap<Vec<u8>, Vec<u8>>`: a key clone, a value clone, two map
    /// probes and a second key clone on the way out — 3.4 µs an entry,
    /// and 1.67 s of a 1.7 s statement that read three 163k-row string
    /// columns (the pod bisect, rev 69).
    #[allow(clippy::too_many_arguments)]
    pub fn scan_column_range_with(
        &self,
        prefix: &KeyPrefix,
        lo_body: &[u8],
        hi_body: Option<&[u8]>,
        prop: u32,
        ts: u64,
        budget: usize,
        f: &mut dyn FnMut(&[u8], &[u8]),
    ) -> Option<u64> {
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + lo_body.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        let hi = match hi_body {
            Some(h) => {
                let mut k = lo.clone();
                k.extend_from_slice(h);
                Some(k)
            }
            None => key_successor(&lo),
        };
        lo.extend_from_slice(lo_body);
        self.column_visit_in(&lo, hi.as_deref(), strip, prop, ts, budget, f)
    }

    /// The collecting form of [`Store::column_visit_in`]: one entry per
    /// key in key order, the newest generation winning.
    fn column_scan_in(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        strip: usize,
        prop: u32,
        ts: u64,
        budget: usize,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        self.column_visit_in(lo, hi, strip, prop, ts, budget, &mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
        })?;
        // Stable: the visitor hands a key's newest block row first.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|later, first| later.0 == first.0);
        Some(out)
    }

    /// The one pass behind both column reads. Row-form resolutions (tail and
    /// segment entries) OVERRIDE block rows — any later write, tombstones
    /// included, lands there — and both sides ascend within a block's
    /// range, so one cursor over the overrides resolves every block row in
    /// O(1) amortised instead of a map probe each. Rows lacking the
    /// property are absent, exactly as a `n.prop` projection treats them.
    #[allow(clippy::too_many_arguments)]
    fn column_visit_in(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        strip: usize,
        prop: u32,
        ts: u64,
        budget: usize,
        f: &mut dyn FnMut(&[u8], &[u8]),
    ) -> Option<u64> {
        let ts = ts.min(self.now_ts());
        let tail = self.tail_has_versions().then_some(&self.inner.tail);
        counted!("store.column scans");
        // Budgeted on rows VISITED, not only on entries handed over: on a
        // paged store the override set IS the column read (a paged segment
        // has no column blocks), and a sparse population's span can hold
        // millions of rows that carry nothing — see `resolve_rows_only`.
        let Some((overrides, sealed)) = self.resolve_rows_only(tail, lo, hi, ts, budget) else {
            counted!("store.column scan declined on rows visited");
            // The state the sweep must reach: a population sparse in its span
            // declines here, BEFORE any row is decoded. This replaced the
            // row-form budget abort below as the declared state — that abort
            // can no longer trip on its own (a row-form hit is a visited row,
            // and the visits were within budget), only after block rows have
            // used most of the budget first.
            sometimes!("store.column scan declined on its visit budget", true);
            return None;
        };
        let mut visited = 0u64;
        for seg in sealed.segments.iter().rev() {
            for b in seg.blocks() {
                let Some(col) = b.column_of(prop) else {
                    continue;
                };
                let rows = b.rows_in_range(lo, hi);
                if rows.is_empty() {
                    continue;
                }
                let first = b.keys[rows.start].as_slice();
                let mut ov = overrides
                    .range::<[u8], _>((
                        std::ops::Bound::Included(first),
                        std::ops::Bound::Unbounded,
                    ))
                    .peekable();
                for row in rows {
                    let k = b.keys[row].as_slice();
                    while ov.peek().is_some_and(|(ok, _)| ok.as_slice() < k) {
                        ov.next();
                    }
                    if ov.peek().is_some_and(|(ok, _)| ok.as_slice() == k) || b.commit_ts[row] > ts
                    {
                        continue;
                    }
                    sometimes!("store.columnar column scan served a block row", true);
                    visited += 1;
                    if visited as usize > budget {
                        sometimes!(
                            "store.column scan aborted on its budget in block rows",
                            true
                        );
                        return None;
                    }
                    f(&k[strip..], b.columns[col].get(row));
                }
            }
        }
        for (k, v) in &overrides {
            let Some(bytes) = v else { continue }; // tombstone — stays gone
            let Ok(rec) = Record::decode(bytes) else {
                continue;
            };
            if let Some(val) = rec.get(PropertyId(prop)) {
                sometimes!("store.columnar column scan fell back to a row", true);
                visited += 1;
                if visited as usize > budget {
                    // Reachable only when block rows spent most of the budget
                    // first (the row-form visits alone are within it — see
                    // `resolve_rows_only`), so it shares the block arm's
                    // declared name rather than claiming a state of its own.
                    sometimes!("store.column scan aborted on its budget in block rows", true);
                    return None;
                }
                f(&k[strip..], val);
            }
        }
        Some(visited)
    }

    /// Visit every live row in `[prefix + body_prefix …]` in key order,
    /// WITHOUT materialising the result set — the k-way merge the scans
    /// always implied, surfaced as an API. The visitor sees the full body
    /// and the resolved value; returning `false` stops the walk. Row-form
    /// sources hand the visitor borrowed bytes; block rows assemble their
    /// record (columns are their storage — that assembly IS the read).
    ///
    /// Resolution is identical to `scan_at`: tail over segments (newest
    /// first), newest version at or below `ts` per key, tombstones skip.
    pub fn for_each_span(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        ts: u64,
        f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) -> u64 {
        self.merge_span(prefix, body_prefix, ts, true, false, &mut |k, v| {
            f(k, v.expect("values requested"))
        })
    }

    /// [`Store::for_each_span`] for KEYS only: the visitor sees each live
    /// body and no value, and a block row's record is never assembled.
    /// Counting 1.79M blocked nodes through the value visitor assembled
    /// every record — property blobs included — to throw each away; a
    /// fresh Graph over the production export spent most of its 18.5 s
    /// first-count rebuild there.
    pub fn for_each_key_span(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        ts: u64,
        f: &mut dyn FnMut(&[u8]) -> bool,
    ) -> u64 {
        self.merge_span(prefix, body_prefix, ts, false, false, &mut |k, _| f(k))
    }

    /// [`Store::for_each_key_span`] for a BULK WALK of a span — a derived
    /// structure's rebuild, which reads every row once and never again. Same
    /// rows, same order, same resolution; the only difference is what a PAGED
    /// segment's block cache remembers about the touch: a scan neither promotes
    /// the blocks it crosses nor evicts what a reader is using, and admits into
    /// free room only (see [`crate::paged::BlockCache`]). A rebuild of one
    /// adjacency table over a 17M-row span used to re-fault every block and
    /// cycle the working set through the eviction loop; a resident store is
    /// byte-for-byte the plain walk.
    pub fn for_each_key_span_scan(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        ts: u64,
        f: &mut dyn FnMut(&[u8]) -> bool,
    ) -> u64 {
        counted!("store.scan-policy span walks");
        self.merge_span(prefix, body_prefix, ts, false, true, &mut |k, _| f(k))
    }

    /// Live bodies under `body_prefix`, one clone each, no value touched —
    /// for the index rows (membership, adjacency) whose value is empty by
    /// construction and whose READERS only ever wanted the key.
    pub fn scan_bodies_prefix(&self, prefix: &KeyPrefix, body_prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.for_each_key_span(prefix, body_prefix, u64::MAX, &mut |body| {
            // The whole body (the `body_prefix` included), exactly as
            // `scan_body_prefix` returns it, so the call sites' offsets hold.
            out.push(body.to_vec());
            true
        });
        out
    }

    /// The k-way merge behind the span visitors. `values` false hands the
    /// visitor `None` for block rows instead of assembling their records;
    /// `scan` reads paged segments under the block cache's scan policy (see
    /// [`Store::for_each_key_span_scan`]) — it changes no row the visitor sees.
    fn merge_span(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
        ts: u64,
        values: bool,
        scan: bool,
        f: &mut SpanVisitor<'_>,
    ) -> u64 {
        enum Item<'a> {
            Chain(&'a [Version]),
            Block(&'a crate::columnar::ColumnBlock, usize),
        }
        let mut lo = Vec::with_capacity(engram_key::PREFIX_LEN + body_prefix.len());
        prefix.encode_into(&mut lo);
        let strip = lo.len();
        lo.extend_from_slice(body_prefix);
        let hi = {
            let mut h = lo.clone();
            let mut i = h.len();
            loop {
                if i == 0 {
                    break None;
                }
                i -= 1;
                if h[i] != 0xFF {
                    h[i] += 1;
                    h.truncate(i + 1);
                    break Some(h);
                }
            }
        };
        // Tail-gate the shard latches: only taken when the tail holds versions.
        // After a seal the tail is empty and span scans go lock-free over the
        // immutable segments — the ungated lock here bounced its reader-count
        // atomic across cores and serialised concurrent adjacency expansion.
        // A k-way merge needs ONE sorted cursor over the tail, so it holds
        // every shard's read latch for the merge and sorts the borrowed
        // chains across them. The sealed set is loaded AFTER the latches are
        // held: a seal cannot complete while they are, so tail and segments
        // are one consistent picture.
        let ts = ts.min(self.now_ts());
        // COPY-OUT FIRST, when the lever allows and the range is small enough.
        //
        // `read_all()` below takes all 64 shard read latches and the caller
        // holds them for the whole merge — across the paged segments' preads,
        // their BLAKE3 verification, and every visitor callback. A writer needs
        // one of those shards, so that window is one in which no write can
        // enter the tail. On the pod that is worth 0.63x-0.75x against Neo4j on
        // the MIXED profiles while the pure ones run 1.49x-3.98x ahead; see
        // `ShardedTail::range_copied` for the measurement and the control.
        //
        // The copy holds one latch at a time for a range descent. `None` back
        // means the range was over the cap and the borrow path is kept: a
        // bigger-than-RAM store cannot copy a 17.26M-row span, and answering
        // short instead is the one thing that must never happen.
        //
        // THE CONSISTENCY THE BORROW PATH GOT FOR FREE, and the copy must earn.
        //
        // Holding every shard latch blocks a seal, so `read_all()` then
        // `sealed()` is one consistent picture by construction. The copy
        // RELEASES each latch, so a seal can complete between the copy and the
        // sealed-set load — draining rows out of the tail and into a segment
        // that our `sealed` then includes. Those rows are in BOTH sources and
        // the merge counts them twice.
        //
        // That is not hypothetical: it is what
        // `adjacency_repair_differential.rs` caught on the first cut, as a live
        // relationship count of 23,976 against a true 23,975. An off-by-one in
        // a count, from a change whose whole promise is that it changes no
        // answer.
        //
        // So the sealed set is read on BOTH sides of the copy and compared by
        // pointer. Equal means no seal completed across the window and the two
        // are consistent. Unequal means one did, and this call falls back to
        // the borrow path — correct by the original argument, and rare, because
        // it needs a seal to land inside one range descent.
        let mut copied = None;
        let mut sealed = self.sealed();
        if self.inner.tail_span_copyout.load(std::sync::atomic::Ordering::Relaxed)
            && self.tail_has_versions()
        {
            let c = self.inner.tail.range_copied(
                &lo,
                hi.as_deref(),
                self.inner
                    .tail_copyout_cap
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            match c {
                None => counted!("store.span copy-out declined by its row cap"),
                Some(rows) => {
                    let after = self.sealed();
                    if std::sync::Arc::ptr_eq(&sealed, &after) {
                        copied = Some(rows);
                    } else {
                        // A seal landed inside the copy. Discard it and take
                        // the latches, which cannot race by construction.
                        counted!("store.span copy-out discarded: a seal landed inside it");
                        sometimes!("store.a seal landed inside a span copy-out", true);
                        sealed = after;
                    }
                }
            }
        }
        // The latches are taken ONLY when the copy did not happen. This is the
        // whole change: with a copy in hand there is nothing to hold.
        let all = (copied.is_none() && self.tail_has_versions()).then(|| {
            let g = self.inner.tail.read_all();
            // Re-read UNDER the latches, for the original argument: a seal
            // cannot complete while they are held, so this pairing is
            // consistent however the branches above left `sealed`.
            sealed = self.sealed();
            g
        });
        counted!("store.visitor scans");
        // THE WRITER-EXCLUSION PROBE.
        //
        // `read_all()` above holds ALL 64 shard read latches until this
        // function returns — across the k-way merge, the paged segments' preads
        // and BLAKE3 verification, and every visitor callback. A writer needs
        // exactly one of those shards, so for as long as they are held no write
        // can enter the tail.
        //
        // It is taken only when the tail is NON-EMPTY, which is why this costs
        // nothing on a pure-read workload (the tail drains at the seal, the
        // branch is skipped) and nothing on a pure-write one (no span reads are
        // issued) — and why it shows up only in a MIX. That asymmetry is the
        // signature, so the counters are split on exactly it.
        //
        // Rows rather than nanoseconds: `Instant::now` is clippy-banned in the
        // engine crates, and the row count is the honest proxy — the merge's
        // work, and therefore the exclusion window, is proportional to it.
        if all.is_some() {
            SPAN_READS_EXCLUDING_WRITERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            SPAN_READS_LATCH_FREE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // Segments newest first. A PAGED segment's range is materialised into an
        // owned buffer HERE — declared before `sources` so it outlives every
        // cursor that borrows it (a k-way merge holds all cursors at once, and a
        // paged cursor cannot borrow into transient cache blocks). RESIDENT
        // segments return `None` and are borrowed in place below — the resident
        // scan never copies.
        // STREAMING FAST PATH — no tail versions and a SINGLE PAGED sealed segment
        // (the `open_paged_dir` steady state): stream it row-by-row (block-by-block
        // via `range_for_each`, bounded to one block + the cache) instead of
        // materialising the whole range into an owned buffer — the adjacency build's
        // ~290MB `scan_bodies_prefix` transient over 2.24M rows. A paged segment is
        // all row form (no columnar blocks), one source, keys already in order, and
        // the SAME version resolution as the merge's Chain branch below (latest
        // version <= ts, tombstone skipped) — so it is byte-identical to the k-way
        // merge over one segment. Resident segments keep the general path (their data
        // also lives in columnar blocks, which `range_for_each` does not walk).
        // `copied.is_none()` is NOT redundant, and leaving it out is a wrong
        // answer rather than a slow one.
        //
        // This fast path skips the tail entirely, and its guard used to read
        // `all.is_none()` because the latches were taken exactly when the tail
        // had rows — so "no latches" meant "no tail". The copy-out broke that
        // equivalence: it leaves `all` as `None` while holding the tail's rows
        // in `copied`. With only the old guard the stream fired WITH TAIL ROWS
        // PRESENT and silently dropped them.
        //
        // `adjacency_repair_differential.rs` caught it as a live relationship
        // count of 23,976 against a true 23,975 — one row, on the paged arm
        // only, from a change whose whole promise is that it changes no answer.
        // The condition that is actually wanted is "there are no tail rows from
        // EITHER source", and it is now spelled that way.
        if all.is_none()
            && copied.is_none()
            && sealed.segments.len() == 1
            && sealed.segments[0].as_resident().is_none()
        {
            let mut visited = 0u64;
            let mut stop = false;
            let mut visit = |k: &LogicalKey, versions: &[Version]| {
                if stop {
                    return;
                }
                if let Some(v) = versions.iter().rev().find(|v| v.commit_ts <= ts) {
                    if let Some(bytes) = &v.value {
                        visited += 1;
                        let val = if values { Some(&bytes[..]) } else { None };
                        if !f(&k.as_slice()[strip..], val) {
                            stop = true;
                        }
                    }
                }
            };
            if scan {
                sealed.segments[0].range_for_each_scan(&lo, hi.as_deref(), &mut visit);
            } else {
                sealed.segments[0].range_for_each(&lo, hi.as_deref(), &mut visit);
            }
            return visited;
        }

        let segs: Vec<&std::sync::Arc<SealedSegment>> = sealed.segments.iter().rev().collect();
        // One owned buffer per PAGED segment (None for resident), materialised
        // before `sources` so it outlives the cursors that borrow it.
        let paged_owned: Vec<Option<segment::OwnedRange>> = segs
            .iter()
            .map(|s| s.paged_range_owned(&lo, hi.as_deref(), scan))
            .collect();

        // Sources in PRECEDENCE order: tail first, then segments newest
        // first (entries before blocks within one segment — disjoint, so
        // the order between them is convention).
        type Cursor<'a> = std::iter::Peekable<Box<dyn Iterator<Item = (&'a [u8], Item<'a>)> + 'a>>;
        let mut sources: Vec<Cursor<'_>> = Vec::new();
        if let Some(rows) = copied.as_ref() {
            // The tail's contribution, from an owned buffer — exactly the shape
            // the paged segments already use below, and for the same reason.
            counted!("store.span copied the tail out per shard");
            let it: Box<dyn Iterator<Item = (&[u8], Item)>> = Box::new(
                rows.iter()
                    .map(|(k, v)| (k.as_slice(), Item::Chain(v.as_slice()))),
            );
            sources.push(it.peekable());
        } else if let Some(all) = all.as_ref() {
            let rows = all.range_sorted(&lo, hi.as_deref());
            let it: Box<dyn Iterator<Item = (&[u8], Item)>> = Box::new(
                rows.into_iter()
                    .map(|(k, v)| (k.as_slice(), Item::Chain(v.as_slice()))),
            );
            sources.push(it.peekable());
        }
        for (i, seg) in segs.iter().enumerate() {
            let it: Box<dyn Iterator<Item = (&[u8], Item)>> = match seg.as_resident() {
                // Resident: borrow the chains in place, no copy.
                Some(s) => Box::new(
                    s.range(&lo, hi.as_deref())
                        .map(|(k, v)| (k.as_slice(), Item::Chain(v.as_slice()))),
                ),
                // Paged: iterate the owned buffer materialised above.
                None => {
                    let owned = paged_owned[i]
                        .as_ref()
                        .expect("a paged segment has a buffer");
                    Box::new(
                        owned
                            .iter()
                            .map(|(k, v)| (k.as_slice(), Item::Chain(v.as_slice()))),
                    )
                }
            };
            sources.push(it.peekable());
            for b in seg.blocks() {
                let range = b.rows_in_range(&lo, hi.as_deref());
                let it: Box<dyn Iterator<Item = (&[u8], Item)>> =
                    Box::new(range.map(move |row| (b.keys[row].as_slice(), Item::Block(b, row))));
                sources.push(it.peekable());
            }
        }

        let mut visited = 0u64;
        loop {
            // The smallest key across all cursors.
            let mut min_key: Option<&[u8]> = None;
            for src in sources.iter_mut() {
                if let Some((k, _)) = src.peek() {
                    if min_key.is_none_or(|m| *k < m) {
                        min_key = Some(*k);
                    }
                }
            }
            let Some(key) = min_key else { break };
            let key = key.to_vec(); // the cursors advance below
            // First source (highest precedence) holding this key resolves it.
            let mut resolved = false;
            let mut stop = false;
            for src in sources.iter_mut() {
                let holds = matches!(src.peek(), Some((k, _)) if **k == *key);
                if !holds {
                    continue;
                }
                let (_, item) = src.next().expect("peeked");
                if resolved {
                    continue; // shadowed by a newer generation
                }
                match item {
                    Item::Chain(versions) => {
                        if let Some(v) = versions.iter().rev().find(|v| v.commit_ts <= ts) {
                            resolved = true;
                            if let Some(bytes) = &v.value {
                                visited += 1;
                                if !f(&key[strip..], Some(bytes)) {
                                    stop = true;
                                }
                            }
                        }
                    }
                    Item::Block(b, row) => {
                        if b.commit_ts[row] <= ts {
                            resolved = true;
                            visited += 1;
                            let keep = if values {
                                counted!("store.block rows assembled");
                                let bytes = b.value_at(row);
                                f(&key[strip..], Some(&bytes))
                            } else {
                                f(&key[strip..], None)
                            };
                            if !keep {
                                stop = true;
                            }
                        }
                    }
                }
            }
            if stop {
                break;
            }
        }
        // Charged only when the latches were actually held: the rows merged
        // under them are the size of the window every writer waited on.
        if all.is_some() {
            SPAN_ROWS_UNDER_LATCHES
                .fetch_add(visited, std::sync::atomic::Ordering::Relaxed);
        }
        visited
    }

    /// Count live rows whose body starts with `body_prefix` — the read a
    /// bare `count()` needs, WITHOUT cloning a single value. The general
    /// scan clones every record it returns; counting 1.8M nodes through it
    /// materialises the database to throw it away (the port benchmark paid
    /// that in full). MVCC and tombstones resolve exactly as in scans.
    pub fn count_at(&self, prefix: &KeyPrefix, body_prefix: &[u8], ts: u64) -> u64 {
        counted!("store.count scans");
        self.for_each_key_span(prefix, body_prefix, ts, &mut |_| true)
    }

    /// Row-form half of [`Store::resolve_span`]: tail + segment entries,
    /// NO blocks. The column scan uses it as its override set.
    ///
    /// `visit_budget` bounds the ROWS this walk may visit (distinct keys,
    /// carrying the property or not); past it the walk STOPS and answers
    /// `None`, and the caller declines to its per-id path. The bound is on
    /// visits rather than on hits because on a paged store — where every
    /// segment is row form and this map IS the column read — the visit is the
    /// cost: each row is a block fetched, verified and decoded and a value
    /// cloned. The production mirror's labels interleave in id space, so the
    /// `[lo, hi)` of a 15-node label spanned the whole node partition and one
    /// property read walked ~5M rows (6 s, gigabytes of transient) to find 15
    /// that carried it — and a budget that counted only hits never fired.
    fn resolve_rows_only(
        &self,
        tail: Option<&tail::ShardedTail>,
        lo: &[u8],
        hi: Option<&[u8]>,
        ts: u64,
        visit_budget: usize,
    ) -> Option<ResolvedSpan> {
        let mut out: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let visible = |versions: &[Version]| -> Option<Option<Vec<u8>>> {
            versions
                .iter()
                .rev()
                .find(|v| v.commit_ts <= ts)
                // One copy at the PUBLIC boundary: `ResolvedSpan` hands owned
                // bytes to callers outside this crate. Internally the version
                // is now carried by refcount.
                .map(|v| v.value.as_ref().map(|b| b.to_vec()))
        };
        let mut visited = 0usize;
        // BYTES held, beside rows visited. The row budget is `factor ×
        // |members|`, and for a wide label that is a lot of rows: the
        // production NewsArticle enrichment count (150k members, ~2 KB
        // records) walked up to 1.2M rows of the interleaved span into this
        // map — 2 to 3.8 GB of resident set per execution, reported by the
        // Bolt layer, and the transient that OOM-killed the 12Gi pod — and
        // then declined anyway. Past the byte budget the walk stops and the
        // caller's per-member gather answers, exactly as past the row budget.
        //
        // ONLY for a BUDGETED read. An unbounded read (`visit_budget ==
        // usize::MAX`: `scan_column_at`, the index build) has no decline
        // path — its caller `expect`s completion — and v88 applied the byte
        // budget to it anyway: eight production statements panicked a worker
        // ("an unbounded budget never aborts") and had their connections
        // closed. A read that cannot decline is not bounded here; the bytes
        // it holds are its caller's contract to keep small.
        let byte_budget = if visit_budget == usize::MAX {
            usize::MAX
        } else {
            self.inner
                .column_scan_byte_budget
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        let mut held = 0usize;
        let mut stopped_on_bytes = false;
        // Skipped when the tail is empty (None) — see resolve_span.
        if let Some(t) = tail {
            let complete = t.for_each_in_range_until(lo, hi, |k, versions| {
                visited += 1;
                if visited > visit_budget {
                    return false;
                }
                if let Some(v) = visible(versions) {
                    held += k.len() + v.as_ref().map_or(0, |b| b.len());
                    out.insert(k.clone(), v);
                    if held > byte_budget {
                        stopped_on_bytes = true;
                        return false;
                    }
                }
                true
            });
            if !complete {
                if stopped_on_bytes {
                    counted!("store.row-form span walk stopped on its byte budget");
                } else {
                    counted!("store.row-form span walk stopped on its visit budget");
                }
                return None;
            }
        }
        // Loaded AFTER the tail — see resolve_span. Returned so the caller's
        // block walk uses the same set the overrides were resolved against.
        let sealed = self.sealed();
        for seg in sealed.segments.iter().rev() {
            let complete = seg.range_for_each_until(lo, hi, |k, versions| {
                if out.contains_key(k) {
                    return true;
                }
                visited += 1;
                if visited > visit_budget {
                    return false;
                }
                if let Some(v) = visible(versions) {
                    held += k.len() + v.as_ref().map_or(0, |b| b.len());
                    out.insert(k.clone(), v);
                    if held > byte_budget {
                        stopped_on_bytes = true;
                        return false;
                    }
                }
                true
            });
            if !complete {
                if stopped_on_bytes {
                    counted!("store.row-form span walk stopped on its byte budget");
                } else {
                    counted!("store.row-form span walk stopped on its visit budget");
                }
                return None;
            }
        }
        Some((out, sealed))
    }

    /// The bytes a row-form column read may hold before it declines
    /// (default 256 MB). A test lowers it to reach the byte decline with a
    /// small store; production keeps the default, which bounds the transient
    /// of a wide-label read to a quarter of a gigabyte where it used to be
    /// the label's whole row budget in record bytes.
    pub fn set_column_scan_byte_budget(&self, bytes: usize) {
        self.inner
            .column_scan_byte_budget
            .store(bytes.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// `(blocks, rows held in blocks)` across every sealed segment — the
    /// head/tail split, observable.
    pub fn columnar_stats(&self) -> (usize, usize) {
        let sealed = self.sealed();
        let mut blocks = 0usize;
        let mut rows = 0usize;
        for seg in &sealed.segments {
            for b in seg.blocks() {
                blocks += 1;
                rows += b.rows();
            }
        }
        (blocks, rows)
    }

    /// Rebuild a store from a log — recovery and PITR in one function.
    ///
    /// The chain is VERIFIED first and a broken one is refused outright:
    /// replaying entries up to a break would silently restore a prefix and
    /// report it as the database. A partial recovery that says so beats a
    /// complete-looking one that is not.
    ///
    /// The rebuilt store re-appends every entry through its own write path, so
    /// its log carries the same entries — recover-from-recovered works, and the
    /// chain heads match if and only if the content does.
    /// Apply ONE replicated entry, preserving the primary's commit timestamp
    /// and re-appending to this store's own log (whose chain, being
    /// deterministic, reproduces the primary's hash byte for byte). The
    /// REPLICA layer owns sequence and chain verification; this is the
    /// publish half only.
    pub fn apply_replicated(&self, e: &Entry) -> Result<(), RecoverError> {
        let (body, value) = Self::decode_log_payload(&e.payload)
            .ok_or(RecoverError::MalformedPayload { seq: e.seq })?;
        let prefix = KeyPrefix {
            realm: e.header.realm,
            namespace: e.header.namespace,
            kind: e.header.kind,
            partition: e.header.partition,
        };
        let key = Self::logical_key(&prefix, body);
        let ts = e.header.commit_ts;
        {
            let mut lg = self.log_mut();
            self.advance_ts_to(ts + 1);
            crash_point("replica.between_verify_and_apply");
            lg.log.append(e.header, e.payload.clone());
        }
        let version = match e.header.op {
            Op::Put => Version {
                commit_ts: ts,
                value: Some(value.into()),
                sealed: prefix.kind.is_protected(),
            },
            Op::Delete => Version {
                commit_ts: ts,
                value: None,
                sealed: false,
            },
        };
        // The chain is stored OLDEST-FIRST (see `put`: "the chain is stored
        // oldest-first and readers walk it from the back"), so the insertion
        // point is the first version NEWER than this one — `insert_ordered`.
        //
        // The predicate behind it was once `v.commit_ts > ts`, which is the
        // reverse. On an ascending chain that test is false at the head, so
        // `partition_point` returned 0 for every entry and each replicated
        // version was inserted at the OLDEST position regardless of its
        // timestamp — a reader walking from the back would then find an older
        // version first and answer with it. Wrong answers, not a crash.
        self.inner.tail.insert_ordered(key, version);
        self.tail_added();
        self.advance_visible_to(ts);
        Ok(())
    }

    /// Rebuild a store from a verified entry stream — recovery is pure REDO
    /// under the WAL rule, and the primary's commit timestamps are preserved.
    pub fn recover(entries: &[Entry]) -> Result<Store, RecoverError> {
        match CommitLog::verify_entries(entries) {
            engram_log::ChainVerify::Intact { .. } => {}
            engram_log::ChainVerify::Broken { seq } => {
                return Err(RecoverError::BrokenChain { seq });
            }
            engram_log::ChainVerify::SequenceGap { expected, found } => {
                return Err(RecoverError::SequenceGap { expected, found });
            }
        }
        let store = Store::new();
        {
            let mut lg = store.log_mut();
            let mut newest = 0u64;
            for e in entries {
                let (body, value) = Self::decode_log_payload(&e.payload)
                    .ok_or(RecoverError::MalformedPayload { seq: e.seq })?;
                let prefix = KeyPrefix {
                    realm: e.header.realm,
                    namespace: e.header.namespace,
                    kind: e.header.kind,
                    partition: e.header.partition,
                };
                let key = Self::logical_key(&prefix, body);
                // Timestamps come FROM the log — reassigning them would give
                // the replica a different history than the primary, and every
                // snapshot read would disagree across the pair.
                let ts = e.header.commit_ts;
                store.advance_ts_to(ts + 1);
                let version = match e.header.op {
                    Op::Put => Version {
                        commit_ts: ts,
                        value: Some(value.into()),
                        sealed: prefix.kind.is_protected(),
                    },
                    Op::Delete => Version {
                        commit_ts: ts,
                        value: None,
                        sealed: false,
                    },
                };
                lg.log.append(e.header, e.payload.clone());
                // Ascending chain order — the log replays in seq order, so
                // this is an append in practice; `insert_ordered` keeps it
                // correct even for an out-of-order caller.
                store.inner.tail.insert_ordered(key, version);
                store.tail_added();
                newest = newest.max(ts);
            }
            store.advance_visible_to(newest);
        }
        Ok(store)
    }

    /// Open a store whose commit log is DURABLE — backed by the WAL file at
    /// `path`, created if absent.
    ///
    /// Recovery is the same pure REDO as [`Store::recover`], fed from the
    /// COMPLETE, chain-valid records on disk; a torn tail (a commit that was
    /// buffered but never `fsync`'d before a crash) is discarded by
    /// [`Wal::open`], which is correct — that commit was never acknowledged.
    /// The returned store appends every subsequent logged write to the same
    /// file and `fsync`s it before acknowledging, so a committed autocommit
    /// write survives a crash. (Multi-write [`Transaction`] atomic durability
    /// rides on the commit-framing added with the transaction bridge; the
    /// autocommit `put`/`delete` path is single-record and fully durable here.)
    pub fn open_wal(path: &std::path::Path) -> Result<Store, OpenWalError> {
        let (entries, wal) = Wal::open(path)?;
        // `recover` builds a fresh, SINK-LESS log and replays `entries` into it,
        // so replay does NOT write the already-on-disk records back to the file.
        let store = Store::recover(&entries)?;
        // Seal the replayed history. `recover` replays the WHOLE log into the
        // tail, and every read of a store with a non-empty tail takes the hot
        // latch the writers hold — so a recovered server served its entire
        // corpus from behind the write lock, ~1,000 latch acquisitions per
        // statement under a balanced load. Sealed, the corpus is read
        // lock-free; only what is written after this lands in the tail, and
        // the adapter seals that on a threshold.
        store.seal();
        // The sink is positioned by `Wal::open` at end-of-file, after the last
        // good record — new appends land after the recovered history.
        store.log_mut().log.attach_sink(wal);
        // A second handle to the same file, for the group-commit fsync that
        // runs OUTSIDE the log latch. Taken here, once, so `sync_pending` never
        // has to reach into the log — and so an in-memory store, which has no
        // sink, keeps `None` and never touches a disk.
        let handle = store.log_mut().log.sync_handle();
        if let Some(handle) = handle {
            let file = handle.map_err(|e| OpenWalError::Format(engram_log::WalError::Io(e)))?;
            store
                .inner
                .group
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .file = Some(file);
        }
        Ok(store)
    }

    // ── Group commit ────────────────────────────────────────────────────────
    //
    // Every logged write (`put`, `delete`, `Transaction::commit`) calls
    // `st.log.sync()` — an fsync — WHILE HOLDING THE STORE LOCK. So a durable
    // write costs one serialised fsync, and write throughput cannot rise with
    // the number of writers: measured flat at 375 → 380 ops/s from 1 to 8
    // clients, against an incumbent that scaled 517 → 2,671 on the same corpus
    // and hardware by fsyncing once per batch of concurrent transactions.
    //
    // The three methods below let an adapter that processes requests in
    // batches (the server's engine thread) defer the fsync, append every write
    // in the batch, pay ONE fsync, and only then acknowledge. The durability
    // promise — nothing is acknowledged before it is on disk — moves from
    // inside each write to the end of the batch, and is otherwise unchanged.

    /// Defer every write's fsync until [`Store::sync_pending`]. Off by default.
    ///
    /// A caller that turns this on has taken responsibility for calling
    /// `sync_pending` before acknowledging any write it made — a `put` that
    /// returned `Ok` under this mode is NOT yet durable.
    pub fn set_group_commit(&self, on: bool) {
        self.log_mut().log.set_deferred_sync(on);
    }

    /// Make everything appended so far durable, sharing the fsync with every
    /// other worker that flushed before it. Returns whether THIS call performed
    /// an fsync (`Ok(false)`: already covered, or nothing owed).
    ///
    /// # The protocol — one syncer flushes for everyone, fsyncs outside the hot lock
    ///
    /// 1. Read `need`: the log's tail, at or past this caller's last append.
    /// 2. Take the group mutex. If `synced_seq >= need`, another worker's
    ///    fsync already covered these records — return, no disk work.
    /// 3. Otherwise this worker is the syncer: under the hot lock, briefly,
    ///    flush EVERY buffered record to the OS (`write`, microseconds) and
    ///    take the sequence that flush reached; release the hot lock; `fsync`
    ///    once; publish that sequence as `synced_seq`.
    ///
    /// An fsync covers every write issued to the file before it was called,
    /// from any thread. Because the syncer flushes everyone's records before
    /// its fsync, every worker that appended before that flush is covered, and
    /// finds so when it takes the group mutex next. Six workers pay one fsync
    /// between them instead of six in a row. A worker whose append landed
    /// after the flush finds `synced_seq < need` and becomes the next syncer.
    ///
    /// The hot lock is NOT held during the fsync — appends from every other
    /// worker proceed while the disk works. The first version held it, which
    /// serialised every worker's appends behind every other worker's disk
    /// wait: `--workers 6` collapsed to 326 write ops/s against 4,009 at one
    /// worker. Same convoy as the per-write fsync it replaced, one level up.
    ///
    /// The coverage sequence is taken at the flush, BEFORE the fsync, never
    /// after: records flushed while the fsync runs are not guaranteed by it,
    /// and claiming them would be the one lie this function must not tell.
    pub fn sync_pending(&self) -> std::io::Result<bool> {
        use std::sync::atomic::Ordering;
        // What THIS caller needs durable: the log's tail right now, which is
        // at or past its own last append. A read only — no flush yet.
        let need = self.log_mut().log.len();
        let mut g = self
            .inner
            .group
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.synced_seq >= need {
            counted!("store.group commit covered by another worker");
            return Ok(false);
        }
        let Some(file) = g.file.as_ref() else {
            // In-memory store: nothing to fsync. Record the watermark so the
            // comparison above stays cheap and truthful.
            g.synced_seq = need;
            return Ok(false);
        };
        // We are the syncer. Flush EVERYTHING appended so far — every worker's
        // records, not only ours — and take the sequence that flush reached.
        // That is the sequence the fsync below will cover, stated before the
        // fsync, never after.
        //
        // The first cut of this protocol had each worker flush its own records
        // BEFORE taking the group mutex, and the syncer read the watermark on
        // acquiring it. The other workers' flushes then landed in the gap
        // between that read and the fsync — physically covered, but unclaimed
        // — so every one of them fsynced again: 329 fsyncs for 320 statements
        // across six workers, no sharing at all. Flushing on behalf of everyone
        // from inside the mutex closes that gap by construction: anything
        // appended before this flush is claimed, anything after it is the next
        // syncer's.
        let covers = self.log_mut().log.flush_to_os()?;
        engram_log::FSYNCS.fetch_add(1, Ordering::Relaxed);
        file.sync_all()?;
        g.synced_seq = g.synced_seq.max(covers);
        counted!("store.group commits");
        Ok(true)
    }

    /// Whether a deferred fsync is owed.
    pub fn has_unsynced(&self) -> bool {
        self.log_mut().log.is_dirty()
    }

    /// Acquire the entity write lock for a key, waiting FIFO if held.
    pub fn lock(&self, prefix: &KeyPrefix, body: &[u8]) -> LockFuture {
        LockFuture {
            store: self.clone(),
            key: Self::logical_key(prefix, body),
            waiter_id: None,
        }
    }

    /// Acquire the entity write lock for a LOGICAL key, synchronously — the
    /// escalation lane of the Bolt retry loop (W2.2). Polls the FIFO future
    /// with a noop waker and a yield between polls: FAIRNESS comes from the
    /// waiter queue (the anti-barging rule in `LockFuture::poll`), not from
    /// wakes — a wake aimed at the noop waker is harmless because this
    /// poller re-polls on its own schedule. Sorted-order acquisition across
    /// multiple keys is the CALLER's obligation.
    pub fn lock_key_sync(&self, key: LogicalKey) -> LockGuard {
        use std::task::{Context, Poll, Waker};
        let mut fut = LockFuture {
            store: self.clone(),
            key,
            waiter_id: None,
        };
        let mut cx = Context::from_waker(Waker::noop());
        let mut waited = 0u32;
        loop {
            match std::pin::Pin::new(&mut fut).poll(&mut cx) {
                Poll::Ready(g) => return g,
                Poll::Pending => {
                    backoff(&mut waited);
                    stall_report(&mut waited, || {
                        "lock_key_sync waiting on the FIFO entity lock".to_string()
                    });
                }
            }
        }
    }

    fn unlock(&self, key: &LogicalKey) {
        let mut st = self.locks_mut();
        st.held.remove(key);
        // Wake exactly ONE waiter — the eldest — and wake it WITHOUT removing
        // its entry. The entry is the waiter's claim to its place in line; it
        // is removed by the waiter itself when it acquires. Popping here would
        // reopen the lost-waiter hole: woken, barged, entry gone, never woken
        // again. (Waking all waiters would be semantically equivalent under
        // this executor's FIFO ready queue, but the winner would then be
        // decided by poll order — a scheduler property — rather than by this
        // queue, and the fairness gate wants the queue to be the authority.)
        if let Some(q) = st.waiters.get(key) {
            if let Some((_, w)) = q.front() {
                w.wake_by_ref();
            }
        }
    }

    /// Native compare-and-set: the supported replacement for the incumbent's
    /// guarded-write workaround.
    ///
    /// The body is the L3 ordering, in order:
    ///
    ///  1. **lock** the entity (FIFO, awaits if held);
    ///  2. **read the CURRENT committed value** — never a snapshot;
    ///  3. compare with `expect` (`None` = "must not exist");
    ///  4. write and publish, or return [`StoreError::CasMismatch`] carrying
    ///     what was actually current.
    ///
    /// The lock is released on every path, including the error paths — a CAS
    /// that leaks its lock on mismatch converts every retry loop into a
    /// deadlock that presents as a hang, the worst failure a harness has.
    pub async fn cas(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        expect: Option<&[u8]>,
        value: StoredValue,
    ) -> Result<u64, StoreError> {
        // The kind check happens BEFORE the lock: a refusal that cannot
        // succeed must not queue behind writers, and must not wake one.
        let _ = Self::check_kind(prefix, &value)?;

        let guard = self.lock(prefix, body).await;
        // 2. Current committed, read UNDER the lock. This read is the whole
        // point: after lock acquisition, no other writer can commit between
        // this read and our write, so the compare is against reality.
        let current = self.get(prefix, body);

        let matched = match (expect, &current) {
            (None, None) => true,
            (Some(e), Some(c)) => e == c.as_slice(),
            _ => false,
        };

        if !matched {
            sometimes!("store.cas lost the race", true);
            drop(guard);
            return Err(StoreError::CasMismatch { current });
        }

        // ── The durability suspension point ─────────────────────────────
        //
        // This is where the WAL append will await. It exists NOW, before any
        // WAL does, because without an await inside the critical section the
        // lock can never be held across a suspension — one poll acquires,
        // reads, writes and releases, so no interleaving can contend and the
        // entire lock mechanism is an unfired guard. Its own test caught
        // exactly that: eight contenders, and `store.lock contended` never
        // fired. Landing the suspension point first means the lock semantics
        // are exercised under real interleaving from day one, and swapping
        // this yield for the WAL append changes durability, not semantics.
        SuspensionPoint { polled: false }.await;

        let ts = self.put(prefix, body, value)?;
        counted!("store.cas commits");
        drop(guard);
        Ok(ts)
    }
}

/// One deliberate yield — the placeholder for the WAL append inside `cas`'s
/// critical section. See the comment at its await site.
struct SuspensionPoint {
    polled: bool,
}

impl Future for SuspensionPoint {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.polled {
            return Poll::Ready(());
        }
        this.polled = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A pinned snapshot: while it lives, the GC watermark cannot pass its ts.
pub struct SnapshotPin {
    store: Store,
    ts: u64,
}

impl SnapshotPin {
    /// The pinned timestamp — pass it to [`Store::get_at`].
    pub fn ts(&self) -> u64 {
        self.ts
    }
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        let mut pins = self.store.pins_mut();
        if let Some(n) = pins.get_mut(&self.ts) {
            *n -= 1;
            if *n == 0 {
                pins.remove(&self.ts);
            }
        }
    }
}

// ─── The lock future ────────────────────────────────────────────────────────

/// Pending acquisition of an entity write lock.
pub struct LockFuture {
    store: Store,
    key: LogicalKey,
    /// This future's place in line, once queued.
    waiter_id: Option<u64>,
}

impl Future for LockFuture {
    type Output = LockGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut st = this.store.locks_mut();

        let eldest = st
            .waiters
            .get(&this.key)
            .and_then(|q| q.front())
            .map(|(id, _)| *id);
        let free = !st.held.contains_key(&this.key);
        // Acquire only when the lock is free AND we are the eldest waiter (or
        // nobody waits). The second half is the anti-barging rule: a fresh
        // contender arriving while others queue must not slip past them just
        // because it polled at the right moment.
        let ours = eldest.is_none() || eldest == this.waiter_id;
        if free && ours {
            if let Some(id) = this.waiter_id.take() {
                let q = st
                    .waiters
                    .get_mut(&this.key)
                    .expect("our entry exists");
                let front = q.pop_front();
                debug_assert_eq!(front.map(|(i, _)| i), Some(id));
                if q.is_empty() {
                    st.waiters.remove(&this.key);
                }
            }
            st.held.insert(this.key.clone(), ());
            drop(st);
            return Poll::Ready(LockGuard {
                store: this.store.clone(),
                key: this.key.clone(),
            });
        }

        sometimes!("store.lock contended", true);
        match this.waiter_id {
            None => {
                let id = st.next_waiter_id;
                st.next_waiter_id += 1;
                st.waiters
                    .entry(this.key.clone())
                    .or_default()
                    .push_back((id, cx.waker().clone()));
                this.waiter_id = Some(id);
            }
            Some(id) => {
                // Re-poll while still waiting: REFRESH the stored waker. A
                // stale waker wakes the wrong incarnation of this task, which
                // is indistinguishable from not being woken at all.
                if let Some(q) = st.waiters.get_mut(&this.key) {
                    if let Some(slot) = q.iter_mut().find(|(i, _)| *i == id) {
                        slot.1 = cx.waker().clone();
                    }
                }
            }
        }
        Poll::Pending
    }
}

impl Drop for LockFuture {
    fn drop(&mut self) {
        // A cancelled waiter must leave the line, and if it was the eldest the
        // NEXT waiter must be woken — otherwise the wake that was aimed at us
        // dies with us and everyone behind waits forever.
        if let Some(id) = self.waiter_id {
            let mut st = self.store.locks_mut();
            if let Some(q) = st.waiters.get_mut(&self.key) {
                let was_front = q.front().map(|(i, _)| *i) == Some(id);
                q.retain(|(i, _)| *i != id);
                if was_front {
                    if let Some((_, w)) = q.front() {
                        w.wake_by_ref();
                    }
                }
                if q.is_empty() {
                    st.waiters.remove(&self.key);
                }
            }
        }
    }
}

/// Holds the entity write lock; releases on drop.
///
/// Drop-based release so no code path — early return, `?`, panic-unwind under
/// a test — can exit while still holding the lock.
pub struct LockGuard {
    store: Store,
    key: LogicalKey,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.store.unlock(&self.key);
    }
}

// ─── D3 registration ────────────────────────────────────────────────────────

impl Subsystem for Store {
    const NAME: &'static str = "store";

    fn register() -> Registration {
        Registration::new()
            .crash_point("store.before_log_append")
            .crash_point("store.between_log_and_publish")
            .sometimes("store.cas lost the race")
            .sometimes("store.lock contended")
            .counter("store.puts")
            .counter("store.gets")
            .counter("store.deletes")
            .counter("store.cas commits")
            .counter("store.seals")
            .counter("store.scans")
            .counter("store.compactions")
            .sometimes("store.sealed a segment")
            .sometimes("store.read served from a sealed segment")
            .sometimes("store.compaction retired a version")
            .sometimes("store.compaction purged a tombstone")
            .sometimes("store.compaction ran under a pinned reader")
            .sometimes("overlay.undeclared namespace refused")
            .sometimes("overlay.foreign overlay refused")
            .sometimes("overlay.tenant write to system refused")
            .sometimes("overlay.second system-cap mint refused")
            .crash_point("adjacency.between_out_and_in")
            .sometimes("adjacency.chunk rolled over")
            .sometimes("adjacency.cas retried")
            .sometimes("adjacency.half edge found")
            .sometimes("index.row not orderable by this index")
            .counter("index.builds")
            .counter("index.range queries")
            .counter("index.restricted to a label")
            .counter("index.overlay removal buckets merged")
            .sometimes("vector.row not indexable")
            .counter("vector.builds")
            .counter("vector.searches")
            .counter("store.columnar blocks built")
            .counter("store.columnar rows blocked")
            .counter("store.column scans")
            .counter("store.column presence scans")
            .counter("store.count scans")
            .counter("store.visitor scans")
            .counter("store.unlogged puts")
            .sometimes("store.bulk put skipped the log")
            .sometimes("store.columnar built a head block")
            .sometimes("store.columnar kept a non-canonical row out of a block")
            .sometimes("store.columnar read served from a block")
            .sometimes("store.columnar scan served a block row")
            .sometimes("store.columnar column scan served a block row")
            .sometimes("store.column scan aborted on its budget in block rows")
            .sometimes("store.projected get served from a block")
            .counter("store.block probes")
            .counter("store.projected gets")
            .counter("store.block rows assembled")
            .sometimes("store.column scan declined on its visit budget")
            .counter("store.column scan declined on rows visited")
            .counter("store.row-form span walk stopped on its byte budget")
            .counter("store.column presence scan declined on rows visited")
            .counter("store.row-form span walk stopped on its visit budget")
            .sometimes("store.column presence scan served a block row")
            .sometimes("store.column presence scan fell back to a row")
            .sometimes("store.column presence scan aborted on its budget")
            .sometimes("store.columnar column scan fell back to a row")
            .gate(
                Gate::new(
                    "a row's bytes survive the column round-trip IDENTICALLY",
                    Canary::new(
                        "perturb one column byte at reconstruction and assert reads change",
                    ),
                )
                .and_canary(Canary::new(
                    "block non-canonical records anyway and assert original bytes come back",
                )),
            )
            .gate(
                Gate::new(
                    "a later write or delete always shadows a block row",
                    Canary::new(
                        "serve the block row despite a tail tombstone and assert resurrection",
                    ),
                ))
            .gate(
                Gate::new(
                    "a plaintext put to a protected KIND is refused unconditionally",
                    Canary::new("put StoredValue::Plain under KIND 0x80 and assert it lands"),
                )
                .and_canary(Canary::new("put Plain under an UNKNOWN protected-block kind 0x9A")),
            )
            .gate(
                Gate::new(
                    "cas reads the CURRENT committed value, never a snapshot",
                    Canary::new("read before lock acquisition and assert two contenders both pass their guards"),
                )
                .and_canary(Canary::new("release the lock before the write and assert a lost update")),
            )
            .gate(Gate::new(
                "exactly one contender wins a CAS race",
                Canary::new("wake ALL waiters on unlock and assert multiple winners under 8 contenders"),
            ))
            .gate(
                Gate::new(
                    "a seal changes WHERE data lives, never WHAT a reader sees",
                    Canary::new("collapse version chains to the newest on seal and assert get_at still sees the past"),
                )
                .and_canary(Canary::new("drop tombstones on seal and assert a deleted key stays deleted")),
            )
            .gate(
                Gate::new(
                    "compaction never retires what a pinned reader can reach",
                    Canary::new("compute the watermark ignoring pins and assert a pinned get_at survives compact()"),
                )
                .and_canary(Canary::new("purge a tombstone newer than the watermark and assert the delete holds")),
            )
    }
}

/// The first key above every key that starts with `k` — `None` when `k`
/// is all 0xFF (nothing is above it).
fn key_successor(k: &[u8]) -> Option<Vec<u8>> {
    let mut h = k.to_vec();
    let mut i = h.len();
    loop {
        if i == 0 {
            break None;
        }
        i -= 1;
        if h[i] != 0xFF {
            h[i] += 1;
            h.truncate(i + 1);
            break Some(h);
        }
    }
}

// ─── Optimistic transactions (M3: MVCC-OCC concurrent writes) ────────────────

/// The commit_ts of the CURRENT latest version of `key` — the value a fresh
/// `get` would resolve — or `None` if absent/never-written. Mirrors `get_at`'s
/// resolution: the tail is newest for any key (seal moves OLD versions into a
/// segment and new writes append to a fresh tail), so a tail version wins; only
/// a key absent from the tail consults segments, newest-first. This is the
/// primitive the OCC validator asks: "was this key committed after my snapshot?"
/// Has `key` been written since `snapshot_ts`? Returns the newest commit
/// timestamp when there is one that could possibly exceed the snapshot, and
/// `None` when the key provably has not moved.
///
/// # Why the snapshot bound is not an optimisation detail
///
/// OCC validation asks one question — did anyone commit to this key AFTER my
/// snapshot — and a sealed segment answers it before it is opened. A segment's
/// footer records `max_commit_ts`, the greatest commit timestamp any version
/// in it carries, so a segment whose maximum is at or below the snapshot
/// cannot hold a conflicting version. Segments are sealed in timestamp order
/// and iterated newest-first, so the first one that fails the test ends the
/// walk: every remaining segment is older still.
///
/// Without the bound this walked EVERY sealed segment for every read-set and
/// write-set key, under the global commit latch — and a newly minted node id
/// is never rejected early by `covering_block` (it sorts above every existing
/// NODE key but below the segment's EDGE region), so each segment cost a
/// block-cache shard acquisition and possibly a `pread` + BLAKE3. Measured on
/// official SF1: the latch was held 34.8% of wall on `rel-hub` at 16 clients
/// and 61.5% on `write-only`, at ~46 us of validation per commit, against a
/// segment set that GROWS without bound on the paged path (spill converts
/// 1:1 and never compacts). That is RC2 of `docs/write-concurrency-ceiling.md`.
///
/// The bound is exact rather than heuristic: it never skips a segment that
/// could hold a conflict, so validation refuses exactly the transactions it
/// refused before.
fn current_commit_ts(
    tail: &tail::ShardedTail,
    sealed: &Sealed,
    key: &LogicalKey,
    snapshot_ts: u64,
) -> Option<(u64, bool)> {
    if let Some((ts, is_put)) = tail.newest_ts_and_kind(key) {
        return Some((ts, is_put));
    }
    for seg in sealed.segments.iter().rev() {
        if seg.max_commit_ts() <= snapshot_ts {
            // This segment and every older one were sealed at or before the
            // snapshot; none can carry a version newer than it.
            counted!("store.validate skipped the sealed prefix");
            break;
        }
        if let Some(v) = seg.get_at(key, u64::MAX) {
            return Some((v.commit_ts, v.value.is_some()));
        }
    }
    None
}

/// A buffered write held by a [`Transaction`] until commit.
struct WriteIntent {
    prefix: KeyPrefix,
    body: Vec<u8>,
    /// `Some` = put, `None` = delete (tombstone).
    value: Option<StoredValue>,
    /// The sealed-ciphertext flag, computed by `check_kind` at buffer time so
    /// commit need not re-derive it — and so a protected-kind refusal happens
    /// at `put`, not deep inside `commit`.
    sealed: bool,
    /// VOLATILE: publish to the tail and validate exactly as a put, but do not
    /// append to the commit log.
    ///
    /// For a row whose only purpose is to make a CONFLICT happen and whose
    /// content is never read — the adjacency guard row is the whole of that
    /// class. It needs VISIBILITY (a concurrent validator must see it) but not
    /// DURABILITY: after recovery there are no in-flight transactions, and an
    /// absent guard is indistinguishable from a present one.
    ///
    /// See `Transaction::put_volatile` for the argument in full, including the
    /// line that must not be crossed: a node delete's guard TOMBSTONE stays
    /// logged, because RC1's soundness depends on the second committer seeing a
    /// non-put.
    volatile: bool,
}

/// A predicate over a full logical key: `true` when two PUTs to it are not a
/// real conflict. See [`Transaction::set_exempt_put_put`].
pub type ExemptPutPut = std::sync::Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// §7 — PRECISION LOCKING. The predicate half of validation.
///
/// Read-set validation asks "did anyone touch a row I MATERIALISED?". That
/// cannot see a PHANTOM: a row committed after our snapshot that our MATCH
/// would have returned had it existed. We never read it, so it is not in the
/// read set, so nothing aborts — and `docs/concurrency-direction.md` records
/// that as a known limitation.
///
/// Neumann/Muhlbauer/Kemper (SIGMOD 2015) invert the question: iterate the rows
/// CHANGED since the snapshot and test each against the reader's PREDICATES.
/// The cost becomes O(delta x predicates), independent of how much was read —
/// which is also the last O(read set) term in the commit path.
///
/// The store deliberately cannot evaluate a predicate: it does not know a label
/// from a property. So this is the seam, and `engram-graph` supplies the
/// implementation, exactly as it supplies `ExemptPutPut` and `MergeObserver`.
///
/// # The contract
///
/// `conflicts` is called under the COMMIT LATCH, once per key committed
/// strictly after our snapshot. It may read the store (`get_at` takes no log
/// latch — it is the same class of read `current_commit_ts` already performs
/// here) but must not write, must not begin a transaction, and must be cheap:
/// it runs at the one serialisation point that cannot be parallelised.
///
/// Returning `true` ABORTS. A guard that cannot represent the reader's
/// predicate must therefore return `false` and let read-set validation stand —
/// declining is always sound, because it can only ever admit the anomaly the
/// engine already admits today.
pub trait ChangeGuard: Send + Sync {
    /// A key committed strictly after our snapshot, and whether that commit was
    /// a PUT rather than a tombstone. `true` aborts.
    fn conflicts(&self, key: &[u8], is_put: bool) -> bool;
}

/// An optimistic (OCC) transaction over a consistent MVCC snapshot.
///
/// The BODY runs lock-free: reads resolve as of the snapshot (with
/// read-your-writes) and record their key; writes buffer locally. Only
/// [`Transaction::commit`] takes the store latch, and only briefly — it
/// VALIDATES (any read- or write-set key committed after the snapshot loses,
/// first-committer-wins) then PUBLISHES the whole write-set under ONE commit ts,
/// atomically. So N transactions run their bodies concurrently and serialize
/// only at the short commit critical section — the group-commit shape that turns
/// the single-writer chokepoint into concurrent write throughput.
///
/// This is the M3 core. The commit latch is the store's coarse write lock for
/// now; a later step refines it to a dedicated commit latch + fine-grained index
/// latches so validation of disjoint key-sets need not serialize at all.
pub struct Transaction {
    store: Store,
    snapshot_ts: u64,
    /// Holds the GC watermark at/below the snapshot for the transaction's life.
    _pin: SnapshotPin,
    reads: BTreeSet<LogicalKey>,
    writes: BTreeMap<LogicalKey, WriteIntent>,
    /// Key classes where two PUTs do not conflict with each other, while
    /// anything involving a DELETE still does. Set by the layer that knows
    /// what a key MEANS — the store deliberately does not. See the exemption
    /// in [`Transaction::commit_reporting`] for the soundness argument.
    exempt_put_put: Option<ExemptPutPut>,
    /// §7's predicate validator, when the layer above could represent this
    /// transaction's predicates. `None` leaves read-set validation as the only
    /// rule — today's behaviour, and the fallback for every predicate the
    /// extractor cannot express.
    change_guard: Option<std::sync::Arc<dyn ChangeGuard>>,
}

impl Store {
    /// Begin an optimistic transaction over a consistent snapshot of the store
    /// as of now. See [`Transaction`].
    pub fn begin(&self) -> Transaction {
        let pin = self.pin_snapshot();
        Transaction {
            store: self.clone(),
            snapshot_ts: pin.ts(),
            _pin: pin,
            reads: BTreeSet::new(),
            writes: BTreeMap::new(),
            exempt_put_put: None,
            change_guard: None,
        }
    }
}

impl Transaction {
    /// The snapshot this transaction reads as of.
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot_ts
    }

    /// Read as of the snapshot, with READ-YOUR-WRITES (a buffered put/delete in
    /// this transaction shadows the snapshot). Records the key in the read-set,
    /// so a value read here that another transaction later overwrites aborts
    /// this one at commit rather than letting it commit on stale data.
    pub fn get(&mut self, prefix: &KeyPrefix, body: &[u8]) -> Option<Vec<u8>> {
        let key = Store::logical_key(prefix, body);
        if let Some(w) = self.writes.get(&key) {
            // read-your-writes: a buffered delete reads as absent.
            return w.value.as_ref().map(|v| v.bytes().to_vec());
        }
        self.reads.insert(key);
        self.store.get_at(prefix, body, self.snapshot_ts)
    }

    /// Peek this transaction's OWN buffered write for a key WITHOUT recording a
    /// read: `None` = not written in this transaction (the caller should read
    /// the committed store), `Some(Some(bytes))` = a buffered put,
    /// `Some(None)` = a buffered delete (tombstone). Used by the general read
    /// accessors so a just-created entity materialises inside its own
    /// transaction; pair it with [`Transaction::note_read`] when the read may
    /// feed a write.
    pub fn peek(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<Option<Vec<u8>>> {
        let key = Store::logical_key(prefix, body);
        self.writes
            .get(&key)
            .map(|w| w.value.as_ref().map(|v| v.bytes().to_vec()))
    }

    /// Declare a key class whose PUT-vs-PUT write-write conflicts are not
    /// real. The predicate receives the full logical key. A conflict is
    /// exempted only when the committed version AND this transaction's intent
    /// are both puts and the key is not in the read set — see the argument at
    /// the exemption site.
    pub fn set_exempt_put_put(
        &mut self,
        pred: ExemptPutPut,
    ) {
        self.exempt_put_put = Some(pred);
    }

    /// Install §7's predicate validator for this transaction.
    ///
    /// Additive to read-set validation, never a replacement: the read-set loop
    /// runs first and unchanged, and this runs after it on the same delta. So
    /// the guard can only ever abort MORE, which is an isolation improvement
    /// (it closes phantoms) and never a weakening.
    ///
    /// A guard covers a SUBSET of the transaction's predicates, and that is
    /// deliberate. Coverage is incremental: a predicate the layer above can
    /// represent is checked here, and one it cannot is simply absent from the
    /// guard and keeps today's read-set-only rule. Absent coverage can only
    /// ever admit the anomaly the engine already admits, so partial coverage is
    /// sound — it is never a claim that the uncovered predicates were checked.
    pub fn set_change_guard(&mut self, guard: std::sync::Arc<dyn ChangeGuard>) {
        self.change_guard = Some(guard);
    }

    /// Record that this transaction READ a key it resolved elsewhere (a
    /// `peek` miss served by the committed store). Validation at commit then
    /// covers it: if it moves before this transaction commits a WRITE, the
    /// commit aborts. A transaction that never writes never validates, so
    /// recording every read costs a read-only statement nothing.
    pub fn note_read(&mut self, prefix: &KeyPrefix, body: &[u8]) {
        self.reads.insert(Store::logical_key(prefix, body));
    }

    /// Whether this transaction has buffered any write.
    pub fn has_writes(&self) -> bool {
        !self.writes.is_empty()
    }

    /// This transaction's buffered writes under `prefix` whose body starts
    /// with `body_prefix`, in key order: `(full body, value)` with `None` for
    /// a buffered delete. The overlay a reader applies over the committed
    /// store to see its own writes through a scan.
    pub fn pending_body_prefix(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let lo = Store::logical_key(prefix, body_prefix);
        let strip = engram_key::PREFIX_LEN;
        self.writes
            .range(lo.clone()..)
            .take_while(|(k, _)| k.starts_with(&lo))
            .map(|(k, w)| {
                (
                    k[strip..].to_vec(),
                    w.value.as_ref().map(|v| v.bytes().to_vec()),
                )
            })
            .collect()
    }

    /// [`Self::pending_body_prefix`] without the values: `(body, put?)`.
    /// The overlays that only add or remove KEYS (memberships, adjacency
    /// rows, counts, index candidates) go through this — cloning every
    /// buffered VALUE per overlay read made an UNWIND of k writes cost
    /// O(k²) bytes across its own probes.
    pub fn pending_body_prefix_present(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
    ) -> Vec<(Vec<u8>, bool)> {
        let lo = Store::logical_key(prefix, body_prefix);
        let strip = engram_key::PREFIX_LEN;
        self.writes
            .range(lo.clone()..)
            .take_while(|(k, _)| k.starts_with(&lo))
            .map(|(k, w)| (k[strip..].to_vec(), w.value.is_some()))
            .collect()
    }

    /// Buffer a put until commit — visible to this transaction via
    /// read-your-writes, to no one else until it commits. A protected-kind
    /// plaintext put is refused HERE, exactly as [`Store::put`].
    pub fn put(
        &mut self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<(), StoreError> {
        let sealed = Store::check_kind(prefix, &value)?;
        let key = Store::logical_key(prefix, body);
        self.writes.insert(
            key,
            WriteIntent {
                prefix: *prefix,
                body: body.to_vec(),
                value: Some(value),
                sealed,
                volatile: false,
            },
        );
        Ok(())
    }

    /// Buffer a put that is VISIBLE but not DURABLE: it publishes to the tail
    /// and validates exactly as [`Transaction::put`], but is not appended to
    /// the commit log.
    ///
    /// # What this is for, and why it is sound
    ///
    /// A row whose only purpose is to make a CONFLICT happen and whose content
    /// is never read. The adjacency guard row (`'G' | node id`) is the whole of
    /// that class: it exists so that a relationship write and a node DELETE are
    /// a write-write conflict in either commit order.
    ///
    /// Such a row needs VISIBILITY — a concurrent validator must see it, which
    /// means it must reach the tail — but not DURABILITY. After recovery there
    /// are no in-flight transactions to conflict with, and an absent guard is
    /// indistinguishable from a present one: the next `create_rel` puts it, and
    /// the next `delete_node` tombstones a key that may not exist, which is
    /// harmless.
    ///
    /// # The line that must not be crossed
    ///
    /// A node delete's guard write must stay a REAL, LOGGED tombstone. RC1's
    /// put-vs-put exemption is sound only because the second committer sees a
    /// non-put and aborts; make that volatile too and the dangling-edge
    /// guarantee is traded for a number.
    ///
    /// Volatile writes are counted separately from `unlogged_count`, which
    /// means "log replay is NOT a complete recovery of this store". Folding
    /// them together would turn a durability alarm into noise.
    pub fn put_volatile(
        &mut self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<(), StoreError> {
        let sealed = Store::check_kind(prefix, &value)?;
        let key = Store::logical_key(prefix, body);
        self.writes.insert(
            key,
            WriteIntent {
                prefix: *prefix,
                body: body.to_vec(),
                value: Some(value),
                sealed,
                volatile: true,
            },
        );
        Ok(())
    }

    /// Buffer a delete (tombstone) until commit.
    pub fn delete(&mut self, prefix: &KeyPrefix, body: &[u8]) {
        let key = Store::logical_key(prefix, body);
        self.writes.insert(
            key,
            WriteIntent {
                prefix: *prefix,
                body: body.to_vec(),
                value: None,
                sealed: false,
                // A tombstone is NEVER volatile: RC1's exemption is sound only
                // because a delete is durable and visible as a non-put.
                volatile: false,
            },
        );
    }

    /// Validate and publish. Returns the commit ts on success, or
    /// [`StoreError::Conflict`] when a read- or write-set key was committed by
    /// another transaction after this one's snapshot — in which case NOTHING is
    /// published and the caller retries from a fresh [`Store::begin`].
    ///
    /// The WAL rule holds per write exactly as in [`Store::put`] (log then
    /// publish), and the whole write-set shares ONE commit ts, so a reader sees
    /// either all of the transaction or none of it.
    pub fn commit(self) -> Result<u64, StoreError> {
        self.commit_reporting().map_err(|(e, _)| e)
    }

    /// As [`Transaction::commit`], but a conflict also reports WHAT
    /// conflicted (see [`ConflictInfo`]) — the success path is unchanged and
    /// pays nothing for the reporting.
    pub fn commit_reporting(self) -> Result<u64, (StoreError, Option<ConflictInfo>)> {
        let Transaction {
            store,
            snapshot_ts,
            _pin,
            reads,
            writes,
            exempt_put_put,
            change_guard,
        } = self;
        if writes.is_empty() {
            // A read-only transaction is serialisable at its own snapshot —
            // every read it made saw one consistent instant — so it commits
            // without validating, and NEVER aborts. Validation exists to
            // protect a write from a stale read; with nothing written there
            // is nothing to protect. This is what lets every read record
            // itself for free.
            counted!("store.txn readonly commits");
            return Ok(snapshot_ts);
        }
        // Payloads and their digests are built OUTSIDE the latch: the hash
        // over every record's bytes is the bulk of the commit's work.
        // A VOLATILE write is skipped here, which is where its whole saving
        // lives: no `log_payload` concatenation and no BLAKE3 over it. It still
        // validates (it is in `writes`) and it still publishes to the tail
        // below — only the durable record is elided.
        let prepared: Vec<(&LogicalKey, &WriteIntent, Vec<u8>, [u8; 32])> = writes
            .iter()
            .filter(|(_, w)| !w.volatile)
            .map(|(key, w)| {
                let payload = Store::log_payload(&w.body, w.value.as_ref().map(|v| v.bytes()));
                let digest = engram_log::payload_digest(&payload);
                (key, w, payload, digest)
            })
            .collect();
        // The commit critical section is the LOG latch — the allocation
        // latch — so no other commit and no autocommit write can allocate a
        // stamp between this validation and this publish.
        let mut lg = store.log_mut();
        // A write that allocated BEFORE this latch was taken may not have
        // reached its shard yet; validating against a tail that is missing it
        // would miss the conflict. Allocation is closed while the latch is
        // held, so waiting for the visible clock to catch the allocation clock
        // drains in microseconds — and cannot wait for ever, because every
        // allocated stamp is published on every path out of its writer.
        store.wait_for_allocated_to_publish();
        let sealed = store.sealed();

        // ── VALIDATE ─────────────────────────────────────────────────────
        // Every key we READ or WROTE must not have moved since our snapshot.
        // Validating the read-set too — not only the writes — is what lifts
        // this above bare snapshot isolation toward serializability: a stale
        // READ that fed a decision aborts rather than commits.
        // ── The COMMIT WINDOW, when it reaches back to our snapshot ──────
        //
        // Validation asks one question per key: did anyone commit to it after
        // my snapshot? It answered with a point lookup per key, under this
        // latch, making commit O(read set) at the one serialisation point that
        // cannot be parallelised.
        //
        // Every write allocates its ts under this same latch, so the window is
        // ts-monotone and complete. Iterating the suffix above `snapshot_ts`
        // and keeping the LAST entry per key gives exactly what the point loop
        // would find — the newest committed version — because every older one
        // has a lower ts and is overwritten in the map.
        //
        // The window is bounded, so a transaction older than `window_low`
        // cannot be answered from it and falls back. That is also the case
        // where the point loop is the better answer: a long-running
        // transaction's window would be enormous.
        let window_view: Option<BTreeMap<LogicalKey, (u64, bool)>> =
            if store.inner.commit_window.load(std::sync::atomic::Ordering::Relaxed)
                && snapshot_ts >= lg.window_low
            {
                // THE SUFFIX, by binary search — not a scan of the whole ring.
                //
                // The window is ts-MONOTONE by construction: every entry is
                // appended by `note_commit` with the log latch held, at the
                // moment the ts is allocated, and `bump_ts` has no other caller.
                // So the entries at or below `snapshot_ts` form a prefix, and
                // the ones this validator wants are exactly the suffix after it.
                //
                // The first cut iterated the ENTIRE deque and filtered — up to
                // `COMMIT_WINDOW_CAP` (65,536) entries, roughly 2.6 MB touched
                // per commit, INSIDE the one latch that cannot be parallelised,
                // to build a map from the handful of entries a short statement
                // actually needs. The prose describing this path said "suffix"
                // (see the comment above and docs/write-path-phase0.md); the
                // code said `.iter()`. This makes them agree.
                //
                // Equal-ts runs — a multi-key transaction commits every write at
                // one ts — are excluded by `<=`, exactly as the `>` filter
                // excluded them, so the map is IDENTICAL and not merely
                // equivalent.
                let mut m: BTreeMap<LogicalKey, (u64, bool)> = BTreeMap::new();
                let start = if store
                    .inner
                    .window_suffix_scan
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    lg.window.partition_point(|(wts, _, _)| *wts <= snapshot_ts)
                } else {
                    0
                };
                for (wts, k, is_put) in lg.window.range(start..) {
                    debug_assert!(
                        *wts > snapshot_ts || start == 0,
                        "the suffix must start above the snapshot"
                    );
                    if *wts > snapshot_ts {
                        m.insert(k.clone(), (*wts, *is_put));
                    }
                }
                WINDOW_ENTRIES_SCANNED
                    .fetch_add((lg.window.len() - start) as u64, std::sync::atomic::Ordering::Relaxed);
                counted!("store.validate answered from the commit window");
                Some(m)
            } else {
                counted!("store.validate fell back to the point loop");
                sometimes!("store.commit window did not reach the snapshot", true);
                None
            };

        for key in reads.iter().chain(writes.keys()) {
            let found = match &window_view {
                Some(m) => m.get(key).copied(),
                None => current_commit_ts(&store.inner.tail, &sealed, key, snapshot_ts),
            };
            if let Some((ts, committed_is_put)) = found {
                if ts > snapshot_ts {
                    // ── THE PUT-VS-PUT EXEMPTION (RC1 / O3) ──────────────
                    //
                    // A key class may declare that two PUTs to it do not
                    // conflict with each other, while anything involving a
                    // DELETE still does. The graph declares exactly one such
                    // class: the adjacency GUARD row, whose whole purpose is
                    // to make a relationship write and a node delete a
                    // write-write conflict in either commit order. Two
                    // relationship writes touching one node both PUT that
                    // guard and so abort each other — measured as ~48% of the
                    // re-runs on the `rel-hub` shape (O0's counter), and
                    // semantically nothing: both creates are valid.
                    //
                    // SOUNDNESS. The exemption applies only when the
                    // committed version is a put AND our own intent is a put.
                    // A node delete writes a TOMBSTONE, so whichever side
                    // commits second sees a value that fails one of the two
                    // tests and aborts — in either order, which is the
                    // property the guard exists for. It is never applied to a
                    // READ-set key: a read that moved is stale whatever wrote
                    // it, and a guard's content is never read.
                    let ours_is_put = writes.get(key).is_some_and(|w| w.value.is_some());
                    let exempt = committed_is_put
                        && ours_is_put
                        && !reads.contains(key)
                        && exempt_put_put
                            .as_ref()
                            .is_some_and(|p| p(key));
                    if exempt {
                        counted!("store.txn put-vs-put conflict exempted");
                        PUT_PUT_EXEMPTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    counted!("store.txn conflicts");
                    TXN_CONFLICTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let info = ConflictInfo {
                        conflicting: vec![key.clone()],
                        write_set: writes.keys().cloned().collect(),
                    };
                    return Err((StoreError::Conflict, Some(info)));
                }
            }
        }

        // ── PRECISION LOCKING (§7) ───────────────────────────────────────
        //
        // The read-set loop above has passed: nothing we MATERIALISED moved.
        // That is silent about a PHANTOM — a row committed after our snapshot
        // that our MATCH would have returned had it existed. We never read it,
        // so it never entered the read set, so nothing above can see it.
        //
        // So ask the other question: of the rows that CHANGED since our
        // snapshot, does any satisfy a predicate this transaction depended on?
        // That is O(delta × predicates) and independent of how much was read —
        // the last O(read set) term in the commit path, and a guarantee upgrade
        // rather than an optimisation.
        //
        // THE DELTA IS `window_view`. §6's ring was built for the read-set loop
        // and is literally the first half of this validator; nothing new is
        // gathered, and the latch is held no longer than the guard's own tests.
        //
        // WITHOUT the window there is no delta to iterate — the point loop
        // answers per key and cannot enumerate what changed. A commit in that
        // state validates by read set alone, which is exactly today's rule: the
        // fallback loses COVERAGE, never soundness. It is counted, because a
        // guarantee that silently stops applying is worse than one never made.
        if let Some(guard) = change_guard.as_ref() {
            match &window_view {
                // BOUNDED, and the bound is not decoration. The window holds up
                // to `COMMIT_WINDOW_CAP` entries, so a long-running transaction's
                // delta can be tens of thousands of keys — and each one the
                // guard accepts costs a store read and a record decode UNDER
                // THIS LATCH. Unbounded, that is a convoy at the one
                // serialisation point this whole programme exists to clear, and
                // it would arrive as a latency collapse rather than a wrong
                // answer, which is the harder kind to attribute.
                //
                // Past the cap the pass is SKIPPED, not truncated. Truncating
                // would check some predicates and silently not others while
                // still reporting a commit as validated; skipping loses the
                // coverage honestly and leaves read-set validation, which is
                // today's rule. Coverage is the thing that is allowed to
                // degrade here — never soundness.
                Some(delta) if delta.len() > PRECISION_MAX_DELTA => {
                    counted!("store.precision locking skipped: delta over the cap");
                    sometimes!("store.precision locking skipped for an oversized delta", true);
                }
                Some(delta) => {
                    for (key, (_ts, is_put)) in delta.iter() {
                        // Our own writes are not phantoms to us: a transaction
                        // that CREATEs a matching row must not abort on the row
                        // it just wrote.
                        if writes.contains_key(key) {
                            continue;
                        }
                        if guard.conflicts(key, *is_put) {
                            counted!("store.txn phantom conflicts");
                            PHANTOM_CONFLICTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            TXN_CONFLICTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            sometimes!(
                                "store.a predicate matched a row committed after the snapshot",
                                true
                            );
                            let info = ConflictInfo {
                                conflicting: vec![key.clone()],
                                write_set: writes.keys().cloned().collect(),
                            };
                            return Err((StoreError::Conflict, Some(info)));
                        }
                    }
                    counted!("store.validate ran a predicate over the delta");
                }
                None => {
                    counted!("store.precision locking skipped: no commit window");
                    sometimes!("store.precision locking skipped for want of a window", true);
                }
            }
        }

        // ── LOG ──────────────────────────────────────────────────────────
        // One commit ts for the entire write-set: the atomic visibility point.
        let slot = CommitSlot {
            store: &store,
            ts: store.bump_ts(),
        };
        let ts = slot.ts;
        // Record the WHOLE write-set in the window, volatile writes included:
        // they publish to the tail and `current_commit_ts` sees them, so the
        // window must too or the two validators disagree.
        for (key, w) in writes.iter() {
            lg.note_commit(ts, key.clone(), w.value.is_some());
        }
        for (_, w, payload, digest) in prepared {
            let op = if w.value.is_some() {
                Op::Put
            } else {
                Op::Delete
            };
            crash_point("store.before_log_append");
            lg.log.append_prehashed(
                RoutingHeader {
                    realm: w.prefix.realm,
                    namespace: w.prefix.namespace,
                    kind: w.prefix.kind,
                    partition: w.prefix.partition,
                    op,
                    commit_ts: ts,
                },
                payload,
                &digest,
            );
        }
        // ── DURABILITY POINT ─────────────────────────────────────────────
        //
        // ONE fsync for the whole write-set, after every record is appended:
        // group commit by construction. It was once missing entirely — a
        // multi-write transaction could be acknowledged, be visible, and be
        // lost on power failure — and it now sits BEFORE the publish, so a
        // failed fsync has nothing to unwind: the write-set never reached
        // the tail, and a caller that handles the error sees no trace of it.
        // (The log may carry the records; a later recovery replaying them is
        // the WAL rule's superset, the same as a crash between log and
        // publish.) A no-op for the default in-memory log; an `fsync` for a
        // WAL-backed store, or a mark the adapter pays off under group commit.
        if let Err(e) = lg.log.sync() {
            counted!("store.txn durability failures");
            return Err((StoreError::Durability(e.to_string()), None));
        }
        drop(lg);

        // ── PUBLISH ──────────────────────────────────────────────────────
        // Outside the log latch: one shard latch per write, and the visible
        // clock advances past `ts` only when `slot` drops, after the last of
        // them — so a reader sees the whole write-set or none of it.
        crash_point("store.between_log_and_publish");
        for (key, w) in writes {
            store.inner.tail.push(
                key,
                Version {
                    commit_ts: ts,
                    value: w.value.as_ref().map(|v| v.bytes().into()),
                    sealed: w.sealed,
                },
            );
        }
        store.tail_added();
        slot.finish();
        counted!("store.txn commits");
        Ok(ts)
    }

    /// TEST-ONLY canary primitive: commit validating ONLY the WRITE-set, dropping
    /// the read-set check — i.e. snapshot isolation, NOT serializability. This is
    /// the deliberately-weakened path the serializability gate proves the real
    /// [`Transaction::commit`] is stronger than: under it, WRITE SKEW (two txns
    /// each reading a shared invariant, then writing DISJOINT keys) commits and
    /// breaks the invariant, where the read-validated commit aborts one of them.
    #[cfg(test)]
    fn commit_snapshot_isolation(mut self) -> Result<u64, StoreError> {
        self.reads.clear();
        self.commit()
    }
}

#[cfg(test)]
mod serializability {
    //! Track C / D2 integrity gate: the MVCC-OCC commit is SERIALIZABLE, not just
    //! snapshot-isolated — validating the READ-set (not only the write-set) aborts
    //! a transaction whose read fed a decision another transaction has since
    //! invalidated. Proven DETERMINISTICALLY (a controlled two-transaction
    //! interleave, no thread races) against the classic write-skew anomaly, WITH a
    //! violating canary: the same interleave under `commit_snapshot_isolation`
    //! (read-set dropped) DOES break the invariant, so the gate is not vacuous.

    use super::*;
    use engram_key::{Kind, Namespace, Partition, Realm};

    fn pfx() -> KeyPrefix {
        KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            kind: Kind::KV,
            partition: Partition(1),
        }
    }

    fn set_i64(store: &Store, key: &[u8], v: i64) {
        store
            .put(&pfx(), key, StoredValue::Plain(v.to_le_bytes().to_vec()))
            .expect("seed");
    }

    fn get_i64(txn: &mut Transaction, key: &[u8]) -> i64 {
        let bytes = txn.get(&pfx(), key).expect("present");
        i64::from_le_bytes(bytes.as_slice().try_into().unwrap())
    }

    fn current_i64(store: &Store, key: &[u8]) -> i64 {
        let bytes = store.get(&pfx(), key).expect("present");
        i64::from_le_bytes(bytes.as_slice().try_into().unwrap())
    }

    /// The write-skew set-up shared by the gate and its canary: two accounts A and
    /// B, invariant `A > 0 OR B > 0`. Two transactions snapshot together, each
    /// reads BOTH accounts (sees the invariant satisfied), then each zeroes a
    /// DIFFERENT account — DISJOINT write-sets, so write-set validation alone
    /// (snapshot isolation) cannot catch the conflict. `commit2` selects the real
    /// (read-validated) or weakened (SI) commit for T2. Returns
    /// `(t2_result, final_A, final_B)` read back from the store after.
    fn write_skew(
        commit2: impl FnOnce(Transaction) -> Result<u64, StoreError>,
    ) -> (Result<u64, StoreError>, i64, i64) {
        let store = Store::new();
        set_i64(&store, b"A", 1);
        set_i64(&store, b"B", 1);

        let mut t1 = store.begin();
        let mut t2 = store.begin();
        let (_a1, _b1) = (get_i64(&mut t1, b"A"), get_i64(&mut t1, b"B"));
        let (_a2, _b2) = (get_i64(&mut t2, b"A"), get_i64(&mut t2, b"B"));
        t1.put(
            &pfx(),
            b"A",
            StoredValue::Plain(0i64.to_le_bytes().to_vec()),
        )
        .unwrap();
        t2.put(
            &pfx(),
            b"B",
            StoredValue::Plain(0i64.to_le_bytes().to_vec()),
        )
        .unwrap();
        t1.commit().expect("first committer wins");
        let r2 = commit2(t2);

        (r2, current_i64(&store, b"A"), current_i64(&store, b"B"))
    }

    #[test]
    fn read_validated_commit_prevents_write_skew() {
        // The REAL commit: T1 committed A=0; T2 read A at its snapshot, and A has
        // since moved, so read-set validation ABORTS T2 — the invariant survives.
        let (r2, a, b) = write_skew(Transaction::commit);
        assert_eq!(
            r2,
            Err(StoreError::Conflict),
            "serializable commit must abort the write-skew transaction"
        );
        assert!(
            a > 0 || b > 0,
            "invariant A>0 OR B>0 must hold (got A={a}, B={b})"
        );
    }

    #[test]
    fn snapshot_isolation_canary_admits_write_skew() {
        // The CANARY: dropping the read-set check (snapshot isolation) lets BOTH
        // commit — disjoint writes, no write-write conflict — so A=0 AND B=0 and
        // the `A>0 OR B>0` invariant BREAKS. This proves the gate above is not
        // vacuous: the read-set validation, specifically, is what saves it.
        let (r2, a, b) = write_skew(Transaction::commit_snapshot_isolation);
        assert!(r2.is_ok(), "snapshot isolation admits the disjoint write");
        assert_eq!(
            (a, b),
            (0, 0),
            "the canary MUST exhibit write skew (both zeroed) — else the gate is vacuous"
        );
    }
}

#[cfg(test)]
mod paged_store_differential {
    //! Track B M1.1 gate: the SAME store, read through its public API, must
    //! answer identically whether its sealed segments are RESIDENT or PAGED
    //! (block-by-block from disk under a cache smaller than the data). Proves
    //! the segment-backing dispatch preserves results across point gets,
    //! snapshot reads, and the k-way merge scan.

    use super::*;
    use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};

    fn pfx(part: u32) -> KeyPrefix {
        KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            kind: Kind::NODE,
            partition: Partition(part),
        }
    }

    /// Point-get results (HEAD + early ts) and per-partition scan results.
    type Capture = (Vec<Option<Vec<u8>>>, Vec<Vec<(Vec<u8>, Vec<u8>)>>);

    /// Capture the public read surface: point gets at HEAD and at an early ts,
    /// plus a full ordered scan per partition.
    fn capture(s: &Store, keys: &[(u32, Vec<u8>)]) -> Capture {
        let mut gets = Vec::new();
        for (part, k) in keys {
            gets.push(s.get(&pfx(*part), k));
            gets.push(s.get_at(&pfx(*part), k, 1));
        }
        let scans = [1u32, 2].iter().map(|p| s.scan(&pfx(*p))).collect();
        (gets, scans)
    }

    #[test]
    fn paged_store_answers_identically_to_resident() {
        let s = Store::new();
        let mut keys: Vec<(u32, Vec<u8>)> = Vec::new();
        // Two partitions, values big enough that a partition spans >1 block.
        for part in [1u32, 2] {
            for i in 0..400u32 {
                let k = format!("body-{i:05}").into_bytes();
                s.put(&pfx(part), &k, StoredValue::Plain(vec![(i % 97) as u8; 64]))
                    .expect("put");
                keys.push((part, k));
            }
        }
        s.seal().expect("seal 1");
        // A second segment: overwrites (multi-version) and deletes (tombstones)
        // of first-batch keys, so newest-segment-wins and tombstone visibility
        // are exercised across the paged/resident boundary.
        for part in [1u32, 2] {
            for i in (0..400u32).step_by(5) {
                let k = format!("body-{i:05}").into_bytes();
                s.put(&pfx(part), &k, StoredValue::Plain(vec![0xEE; 6]))
                    .expect("overwrite");
            }
            for i in (0..400u32).step_by(11) {
                let k = format!("body-{i:05}").into_bytes();
                s.delete(&pfx(part), &k);
            }
        }
        s.seal().expect("seal 2");

        let before = capture(&s, &keys);
        // Not vacuous: real values and non-empty scans.
        assert!(before.0.iter().any(Option::is_some), "vacuous get capture");
        assert!(
            before.1.iter().any(|v| !v.is_empty()),
            "vacuous scan capture"
        );

        // Rebind to paged with a cache FAR smaller than the data (~51 KiB/seg),
        // forcing fault-in, eviction and re-fetch during the reads.
        let dir = std::env::temp_dir().join("engram_m11_paged_store");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cache = crate::paged::BlockCache::new(4 * 1024);
        let (_, trace) = engram_observe::with_trace(|| {
            s.rebind_sealed_paged(&dir, cache);
            let after = capture(&s, &keys);
            assert_eq!(before.0, after.0, "paged get/get_at diverged from resident");
            assert_eq!(before.1, after.1, "paged scan diverged from resident");
        });
        // The paged path was genuinely exercised (blocks faulted in, evicted).
        let c = trace.counters();
        assert!(
            c.get("paged.block cache miss").copied().unwrap_or(0) > 0,
            "no fault-ins"
        );
        assert!(
            c.get("paged.block evicted").copied().unwrap_or(0) > 0,
            "cache never evicted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan-policy walk is the plain walk: same bodies in the same order,
    /// over a multi-segment paged store WITH a tail (the k-way merge) and over
    /// a single paged segment with no tail (the streaming fast path), under a
    /// cache far smaller than the data.
    #[test]
    fn scan_policy_span_walk_is_byte_identical_to_the_plain_walk() {
        let s = Store::new();
        for part in [1u32, 2] {
            for i in 0..400u32 {
                let k = format!("body-{i:05}").into_bytes();
                s.put(&pfx(part), &k, StoredValue::Plain(vec![(i % 97) as u8; 64]))
                    .expect("put");
            }
        }
        s.seal().expect("seal 1");
        for part in [1u32, 2] {
            for i in (0..400u32).step_by(7) {
                let k = format!("body-{i:05}").into_bytes();
                s.put(&pfx(part), &k, StoredValue::Plain(vec![0xEE; 6]))
                    .expect("overwrite");
            }
            for i in (0..400u32).step_by(13) {
                let k = format!("body-{i:05}").into_bytes();
                s.delete(&pfx(part), &k);
            }
        }
        s.seal().expect("seal 2");
        let dir = std::env::temp_dir().join("engram_scan_policy_walk");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cache = crate::paged::BlockCache::new(4 * 1024);
        s.rebind_sealed_paged(&dir, cache);
        // A tail too, so the k-way merge (not the streaming path) runs.
        s.put(&pfx(1), b"body-00003", StoredValue::Plain(vec![1; 2]))
            .expect("tail put");
        fn walk(s: &Store, scan: bool, part: u32, body_prefix: &[u8]) -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            let mut f = |b: &[u8]| {
                out.push(b.to_vec());
                true
            };
            if scan {
                s.for_each_key_span_scan(&pfx(part), body_prefix, u64::MAX, &mut f);
            } else {
                s.for_each_key_span(&pfx(part), body_prefix, u64::MAX, &mut f);
            }
            out
        }
        for part in [1u32, 2] {
            for prefix in [&b""[..], b"body-001", b"body-0039"] {
                let plain = walk(&s, false, part, prefix);
                let (scanned, trace) =
                    engram_observe::with_trace(|| walk(&s, true, part, prefix));
                assert!(!plain.is_empty(), "vacuous walk");
                assert_eq!(plain, scanned, "scan policy changed the rows (part {part})");
                assert!(
                    trace.counters().get("store.scan-policy span walks").copied().unwrap_or(0) > 0
                );
            }
        }
        // Single paged segment, empty tail: the streaming fast path.
        let s = Store::new();
        for part in [1u32, 2] {
            for i in 0..400u32 {
                let k = format!("body-{i:05}").into_bytes();
                s.put(&pfx(part), &k, StoredValue::Plain(vec![(i % 89) as u8; 64]))
                    .expect("put");
            }
        }
        let dir2 = std::env::temp_dir().join("engram_scan_policy_walk_stream");
        std::fs::create_dir_all(&dir2).expect("mkdir");
        let _ = s.into_paged(&dir2, 4 * 1024).expect("into_paged");
        assert_eq!(s.segment_count(), 1);
        assert_eq!(s.tail_versions(), 0);
        for part in [1u32, 2] {
            let plain = walk(&s, false, part, b"");
            let (scanned, trace) = engram_observe::with_trace(|| walk(&s, true, part, b""));
            assert_eq!(plain.len(), 400);
            assert_eq!(plain, scanned, "streaming scan policy changed the rows");
            let c = trace.counters();
            assert!(
                c.get("paged.block cache scan hit").copied().unwrap_or(0)
                    + c.get("paged.block cache scan miss").copied().unwrap_or(0)
                    > 0,
                "the streaming path did not take the scan policy: {c:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn into_paged_is_the_public_path_and_preserves_reads() {
        let s = Store::new();
        let mut keys: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..300u32 {
            let k = format!("k-{i:05}").into_bytes();
            s.put(&pfx(1), &k, StoredValue::Plain(vec![(i % 251) as u8; 50]))
                .expect("put");
            keys.push((1, k));
        }
        s.seal().expect("seal");
        // A tail present at conversion time proves `into_paged` seals first.
        s.put(&pfx(1), b"z-tail", StoredValue::Plain(vec![7; 3]))
            .expect("tail put");
        keys.push((1, b"z-tail".to_vec()));
        let before = capture(&s, &keys);
        let segs_before = s.segment_count();

        let dir = std::env::temp_dir().join("engram_m12_into_paged");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let _cache = s.into_paged(&dir, 4 * 1024).expect("into_paged");

        let after = capture(&s, &keys);
        assert_eq!(before.0, after.0, "into_paged changed point-read results");
        assert_eq!(before.1, after.1, "into_paged changed scan results");
        assert_eq!(
            s.segment_count(),
            segs_before + 1,
            "the tail should have sealed into a segment"
        );
        assert!(
            s.sealed_segments_for_test()
                .iter()
                .all(|seg| seg.as_resident().is_none()),
            "into_paged left a resident segment"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_paged_dir_reopens_from_disk_with_identical_reads() {
        let s = Store::new();
        let mut keys: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..300u32 {
            let k = format!("k-{i:05}").into_bytes();
            s.put(&pfx(1), &k, StoredValue::Plain(vec![(i % 251) as u8; 50]))
                .expect("put");
            keys.push((1, k));
        }
        s.seal().expect("seal 1");
        // A second segment so the seq-ordered multi-file open is exercised.
        for i in (0..300u32).step_by(4) {
            let k = format!("k-{i:05}").into_bytes();
            s.put(&pfx(1), &k, StoredValue::Plain(vec![0xEE; 5]))
                .expect("overwrite");
        }
        s.seal().expect("seal 2");

        let want = capture(&s, &keys);
        let want_segs = s.segment_count();

        let dir = std::env::temp_dir().join("engram_m3_open_paged_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Write the seg-<seq>.seg files (into_paged writes them as a side effect).
        let _ = s
            .into_paged(&dir, 1024 * 1024)
            .expect("write segment files");
        drop(s); // release EVERYTHING resident — the reopen must stand on disk alone

        // Durable open: a fresh store over the files, never loaded resident.
        let (reopened, _cache) = Store::open_paged_dir(&dir, 4 * 1024).expect("open_paged_dir");
        assert_eq!(
            reopened.segment_count(),
            want_segs,
            "reopened a different number of segments"
        );
        // The commit clock must have advanced PAST the segments' data, or a
        // snapshot read at `now_ts` would see an empty graph (the durable-open
        // empty-results bug). `now_ts` starts at 0 on a fresh store.
        assert!(
            reopened.now_ts() > 0,
            "open_paged_dir must advance the clock past the persisted commits"
        );
        let got = capture(&reopened, &keys);
        assert_eq!(
            want.0, got.0,
            "durable-open point reads differ from the original"
        );
        assert_eq!(want.1, got.1, "durable-open scans differ from the original");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_sealed_into_is_idempotent_and_reads_through_the_callers_cache() {
        let s = Store::new();
        let mut keys: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..300u32 {
            let k = format!("k-{i:05}").into_bytes();
            s.put(&pfx(1), &k, StoredValue::Plain(vec![(i % 251) as u8; 50]))
                .expect("put");
            keys.push((1, k));
        }
        s.seal().expect("seal 1");
        // A second segment so a multi-segment spill is exercised.
        for i in (0..300u32).step_by(4) {
            let k = format!("k-{i:05}").into_bytes();
            s.put(&pfx(1), &k, StoredValue::Plain(vec![0xEE; 5]))
                .expect("overwrite");
        }
        s.seal().expect("seal 2");
        let want = capture(&s, &keys);
        let want_segs = s.segment_count();

        let dir = std::env::temp_dir().join("engram_spill_sealed_into");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // ONE caller-held cache, far smaller than the data, shared by both
        // spills — the shared-budget property this method exists for (unlike
        // `into_paged`, which mints a cache per call).
        let cache = crate::paged::BlockCache::new(4 * 1024);
        assert_eq!(
            s.spill_sealed_into(&dir, &cache).expect("spill"),
            want_segs,
            "every resident segment spills"
        );
        assert_eq!(
            s.spill_sealed_into(&dir, &cache).expect("re-spill"),
            0,
            "a repeated spill must find nothing resident (idempotent)"
        );
        assert!(
            s.sealed_segments_for_test()
                .iter()
                .all(|seg| seg.as_resident().is_none()),
            "spill left a resident segment"
        );
        // The cache handle is not observable through a segment, so sharing is
        // proven behaviourally: reads answer identically AND fault in through
        // the tiny caller budget — nothing resident is left to answer from.
        let (_, trace) = engram_observe::with_trace(|| {
            let after = capture(&s, &keys);
            assert_eq!(want.0, after.0, "spill changed point-read results");
            assert_eq!(want.1, after.1, "spill changed scan results");
        });
        assert!(
            trace
                .counters()
                .get("paged.block cache miss")
                .copied()
                .unwrap_or(0)
                > 0,
            "no fault-ins — reads did not go through the caller's cache"
        );

        // The files stand alone: a COPY opens durably with identical reads.
        let copy = std::env::temp_dir().join("engram_spill_sealed_into_copy");
        let _ = std::fs::remove_dir_all(&copy);
        std::fs::create_dir_all(&copy).expect("mkdir copy");
        for entry in std::fs::read_dir(&dir).expect("read spill dir") {
            let p = entry.expect("entry").path();
            std::fs::copy(&p, copy.join(p.file_name().expect("name"))).expect("copy seg");
        }
        let (reopened, _cache) = Store::open_paged_dir(&copy, 4 * 1024).expect("open copy");
        assert_eq!(
            reopened.segment_count(),
            want_segs,
            "the copy reopened a different number of segments"
        );
        let got = capture(&reopened, &keys);
        assert_eq!(want.0, got.0, "copied-dir point reads differ");
        assert_eq!(want.1, got.1, "copied-dir scans differ");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&copy);
    }
}
