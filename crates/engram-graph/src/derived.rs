//! Derived structures — the one rule, written once.
//!
//! A graph keeps many structures DERIVED from its store: label memberships,
//! range indexes, adjacency tables, degree tables, BFS memos. Each is a cache
//! of some SOURCE (a label's membership rows, a property's values, one
//! relationship type's adjacency rows) and is correct exactly as long as that
//! source has not changed since the structure was built.
//!
//! # The defect this module exists to end
//!
//! Six times in one day the same defect was found in six different caches:
//!
//! 1. **Validity keyed on the wrong clock.** A cache compared its build clock
//!    to the store's GLOBAL commit clock, which every write advances — so a
//!    write to an unrelated structure invalidated it. A `SET n.hits` rebuilt
//!    the label membership; a `CREATE (:Message)` reset the adjacency probe
//!    gate so every traversal, over relationship types the write never
//!    touched, bypassed a current table for its first 1,024 hops.
//! 2. **Catch-up by copy.** Applying a delta of five ids copied the whole
//!    label. O(label) per read after a write.
//! 3. **Deltas consumed by the first reader.** The first stale reader took the
//!    delta; a concurrent reader on another worker found none and fell to a
//!    full rebuild under the store's read lock, stalling every writer. Worse,
//!    a rebuild at an older epoch could be published over a newer catch-up
//!    and lose a row — a correctness fault, reachable.
//!
//! Every one was fixed as a special case, and the next cache had it again.
//! This module is the general case. A derived structure is:
//!
//! - **a [`ChangeLog`]** of its source — append-only, stamped with the commit
//!   timestamp of each change, carrying the source's own epoch. Readers apply
//!   entries newer than their snapshot and **never consume**; two concurrent
//!   readers apply the same entries and publish the same result. Entries are
//!   pruned only behind a published snapshot, so nothing a live snapshot needs
//!   is ever dropped. A change that cannot be expressed as an entry (an
//!   overflow, a write inside a transaction) is a `touch`, which raises the
//!   floor and forces a rebuild for anything older — the conservative
//!   direction, and the only one.
//! - **a [`Slot`]** holding the current snapshot, published **monotonically**:
//!   an older-epoch build can never overwrite a newer one.
//! - **a [`SingleFlight`]** guard around the BUILD path only, ONE PER SLOT,
//!   so N workers that miss at once on the same structure do one rebuild,
//!   not N, and a build of one structure never holds up a builder of another.
//! - a snapshot that applies a delta in **O(delta)**, not O(base) — a shared
//!   immutable base plus a small overlay, folded on a threshold
//!   ([`MembersView`] is the reference implementation for id sets).
//!
//! # The reader protocol
//!
//! ```text
//! snap = slot.load()
//! if snap.at >= log.epoch            -> current, use it            (no lock)
//! else if log.covers(snap.at)        -> apply log.since(snap.at),
//!                                       publish at fenced(log.epoch)  (log lock; see the fence)
//! else                               -> reload slot (someone published); if still uncovered,
//!                                       enter SingleFlight and re-check:
//!                                         snap.at >= epoch      -> the winner's, use it
//!                                         log.covers(snap.at)   -> the winner's, published
//!                                                                  FENCED below the epoch:
//!                                                                  catch up from the log,
//!                                                                  never rescan
//!                                         else                  -> build: at = now_ts()
//!                                                                  BEFORE the scan,
//!                                                                  publish at fenced(at)
//!                                                                  read AFTER it
//! ```
//!
//! The epoch a catch-up is stamped with is the log's epoch READ UNDER THE
//! LOG'S LOCK, in the same critical section as the entries, then clamped
//! below every in-flight writer (`Graph::fenced`, next section) in that same
//! section. A build's stamp is `now_ts()` read BEFORE the scan (every change
//! at or below it has rows the scan sees), clamped AFTER the scan. Reading
//! the clock separately from the entries is the stale-stamp hazard: a reader
//! that took `now_ts()` after a write's rows committed but before the write
//! logged them would stamp a snapshot as newer than a change it does not
//! contain, and then be judged current for ever.
//!
//! The loser's re-check behind `SingleFlight` has THREE arms, not two, because
//! of the clamp: the winner's publish is below the epoch whenever any writer
//! was in flight when it published, so `snap.at >= epoch` alone would judge
//! a table walked a moment ago stale and walk it again — N workers missing
//! at once did N builds, serially. The entries above the winner's stamp are
//! exactly the in-flight writers' rows (the winner pruned only below its
//! stamp), so the loser repairs from them, and only a snapshot the log no
//! longer reaches (or one the repair gate declines) is rebuilt.
//!
//! # The write fence — why a publish stamp is CLAMPED, not just read
//!
//! Reading the epoch under the lock is necessary and was not sufficient. A
//! writer commits its rows at commit ts `t` and only THEN records its log
//! entry; between the two, another writer can commit at `t' > t` and record
//! first, moving the epoch to `t'`. A catch-up that runs in that window
//! publishes at `t'`, prunes behind it — and the late entry, stamped `t ≤ t'`
//! (or folded up to `t'`), is below `since(t')` for every reader after. The
//! row is in the store and in no snapshot, for ever: `Slot::publish` is
//! monotone, so nothing older can displace the snapshot that missed it.
//! Measured: 2 writers + the maintenance refresh, 4,847 adjacency and
//! 431,164 membership bounds violations and a settled `:Message` membership
//! 24 short of the acknowledged count while a direct store walk had every
//! row (`tests/derived_refresh_concurrent.rs`).
//!
//! The fix is a LOW-WATER MARK the graph keeps (`Graph::inflight`): every
//! direct writer and every committing transaction registers the visible
//! clock `r` it observed BEFORE its first row write and unregisters AFTER
//! its entries are recorded; its rows and entries are stamped `t > r`. A
//! publisher reads the lowest registered `r` in the same critical section
//! as the log's epoch and publishes at `min(epoch, r)`. Every entry a still
//! in-flight writer will record is then stamped above the publish stamp and
//! inside `since(at)` of the next catch-up. Scan-built snapshots take the
//! same clamp (the registry read AFTER the walk, so a writer registering
//! later has `r` at or past the walk's clock): with it, no entry is ever
//! recorded at or below a stamp a published snapshot has pruned behind, and
//! [`ChangeLog::record`] treats one that is as an invariant violation — the
//! log is POISONED (entries dropped, floor raised, counted) and the graph
//! retracts the affected snapshots so the next reader rebuilds from the
//! store rather than trusting a repair. Fail closed.
//!
//! # House rules honoured
//!
//! No `unsafe`; no clock or thread; `VecDeque`/`BTreeMap` only (no `HashMap`
//! — iteration order is part of the determinism trace); every path counted.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use engram_observe::counted;

/// The slot for `key` in a map of slots, inserted if absent.
///
/// Insertion is a whole-map rcu (rare: once per key ever); every later access
/// is a lock-free load and a per-key publish. This is what replaces the old
/// `ArcSwap<BTreeMap<key, (epoch, Arc<T>)>>` maps, whose EVERY publish cloned
/// the whole map — and whose publishes were not monotone. Past `max` keys the
/// map is cleared, the same bound the old maps carried, so a workload that
/// mints unbounded key sets (type-set fan-out) cannot grow it without limit.
pub(crate) fn slot_in<K: Ord + Clone, T>(
    map: &arc_swap::ArcSwap<BTreeMap<K, Arc<Slot<T>>>>,
    key: &K,
    max: usize,
) -> Arc<Slot<T>> {
    if let Some(s) = map.load().get(key) {
        return Arc::clone(s);
    }
    let fresh = Arc::new(Slot::default());
    let mut out = Arc::clone(&fresh);
    map.rcu(|old| {
        if let Some(s) = old.get(key) {
            // Lost the insert race: use theirs, publish nothing.
            out = Arc::clone(s);
            return Arc::clone(old);
        }
        let mut new = (**old).clone();
        if new.len() >= max {
            counted!("derived.slot map cleared at its cap");
            new.clear();
        }
        new.insert(key.clone(), Arc::clone(&fresh));
        out = Arc::clone(&fresh);
        Arc::new(new)
    });
    out
}

// ── AdjChangeFilter ─────────────────────────────────────────────────────────

/// Stamp slots in [`AdjChangeFilter`]. 2 MiB, allocated once per `Graph`.
///
/// Sized against the number of DISTINCT nodes written between two publishes of
/// a table, not against the corpus: a slot whose stamp is older than the
/// reader's table passes, so the filter cleans itself by comparison and never
/// needs a reset. A few thousand writes into 262,144 slots leaves the false
/// "maybe" rate around 1%, and a false "maybe" only costs the slower check it
/// was avoiding.
pub(crate) const ADJ_CHANGE_FILTER_SLOTS: usize = 1 << 18;

/// The last stamp at which SOME node hashing to each slot had its adjacency
/// row moved — a lock-free approximation of "has this node changed since?".
///
/// # Why this exists
///
/// A single-node reader whose adjacency table is stale as a whole can still be
/// served from it when the change set does not touch ITS node. Answering that
/// from [`ChangeLog`] means taking the log's lock — the same lock every write
/// takes exclusively — and scanning the delta. Measured on `balattr` at 8
/// clients 50/50, that path left writes running at 24% of their solo rate
/// while a disjoint-type control (readers whose table no writer invalidates)
/// showed only 2.7% interference: all of it was here, and none of it was in
/// the store or the commit path.
///
/// A read must therefore not touch the writers' structure at all. This is one
/// atomic load against one atomic `fetch_max` per changed row.
///
/// # Why the approximation is SOUND
///
/// The filter is keyed on `(tag, node)` and NOT on relationship type, and
/// slots collide. Both make it answer "maybe changed" for nodes that did not
/// change — which costs only the slower check it was in front of. It can never
/// answer "unchanged" for a node that did: a slot's stamp is the maximum over
/// everything hashing to it, so it is at least the node's own last change.
///
/// The freshness argument is the ordering one. `note` runs inside the same
/// write critical section as [`ChangeLog::record`] and BEFORE it, so the filter
/// is never staler than the log a reader would otherwise have consulted. A
/// reader that sees an unchanged slot would have seen an empty delta.
/// # Why the table is allocated lazily
///
/// A `Graph` that never writes a relationship never needs it, and the test
/// suite builds `Graph`s in the thousands. Two megabytes zeroed in
/// `Graph::new` is a cost every one of them would pay for a structure only a
/// mixed read/write load uses. `OnceLock` puts it on the first adjacency write
/// instead, and makes an unallocated filter answer "maybe" — the conservative
/// direction, and exactly what a graph with no writes should say.
#[derive(Debug, Default)]
pub(crate) struct AdjChangeFilter {
    slots: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
}

impl AdjChangeFilter {
    /// The slot `(tag, node)` hashes to. A splitmix64 finaliser, so ids that
    /// are dense and sequential — which every id this engine mints is — spread
    /// across the whole table instead of clustering into one run of slots.
    fn index(tag: u8, node: u64) -> usize {
        let mut x = node
            .wrapping_add((tag as u64) << 56)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        (x as usize) & (ADJ_CHANGE_FILTER_SLOTS - 1)
    }

    /// Record that `node`'s `tag`-side adjacency row moved at `ts`.
    ///
    /// `Release`, paired with `unchanged_since`'s `Acquire`: a reader that
    /// sees the stamp must also see the row the stamp describes.
    pub(crate) fn note(&self, tag: u8, node: u64, ts: u64) {
        let slots = self.slots.get_or_init(|| {
            (0..ADJ_CHANGE_FILTER_SLOTS)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect()
        });
        slots[Self::index(tag, node)].fetch_max(ts, std::sync::atomic::Ordering::Release);
    }

    /// Whether nothing hashing to `node`'s slot has changed since `at`.
    ///
    /// `true` is a guarantee about `node`; `false` is only "ask something more
    /// precise" — which is also what an unallocated table answers, because
    /// nothing has written through it and it can vouch for nothing.
    pub(crate) fn unchanged_since(&self, tag: u8, node: u64, at: u64) -> bool {
        match self.slots.get() {
            Some(s) => s[Self::index(tag, node)].load(std::sync::atomic::Ordering::Acquire) <= at,
            None => false,
        }
    }
}

// ── ChangeLog ───────────────────────────────────────────────────────────────

/// An append-only, epoch-stamped log of changes to one source.
///
/// `E` is one change (an id joining or leaving a label, a row's new index key,
/// a node whose adjacency row moved). Entries are kept in commit-timestamp
/// order, which is the order they are recorded in — a writer stamps its change
/// with the commit ts of the row it describes (a transaction with its commit
/// ts), never with a clock read before the row was written: a stamp below the
/// row's ts would put the entry inside a scan-built snapshot's `since(at)`
/// while the row itself was past the scan.
#[derive(Debug)]
pub(crate) struct ChangeLog<E> {
    entries: VecDeque<(u64, E)>,
    /// Entries stamped at or below this are gone — pruned behind a published
    /// snapshot, dropped on overflow, or never logged (`touch`). A snapshot at
    /// `at < floor` cannot be caught up from this log.
    floor: u64,
    /// The stamp of the newest change to the source, logged or not. A
    /// snapshot at `at >= epoch` is current.
    epoch: u64,
    cap: usize,
    /// The highest stamp a PUBLISHED snapshot has pruned behind
    /// (`prune_below`) — distinct from `floor`, which overflow and `touch`
    /// also raise without any snapshot existing at it. An entry recorded at
    /// or below THIS stamp describes a row a published snapshot may lack and
    /// nothing will ever apply: the write fence (see the module doc) makes
    /// that impossible, so `record` treats it as an invariant violation.
    pruned_to: u64,
}

impl<E> ChangeLog<E> {
    pub(crate) fn new(cap: usize) -> Self {
        ChangeLog {
            entries: VecDeque::new(),
            floor: 0,
            epoch: 0,
            cap,
            pruned_to: 0,
        }
    }

    /// The source's epoch: the stamp of its newest change.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Record one change stamped `ts`. Returns `true` when the log was
    /// POISONED by it — see below — so the caller can retract the snapshots
    /// the entry would otherwise have been lost to.
    ///
    /// Past `cap` pending entries the log gives up on catch-up for anything
    /// older than now — bounded memory — and says so with the floor rather
    /// than by silently dropping the oldest entries, which would turn a
    /// catch-up into a snapshot missing rows.
    ///
    /// A stamp at or below `pruned_to` is a change that a published snapshot
    /// was stamped past without holding: under the write fence this cannot
    /// happen (every publish is clamped below every in-flight writer's
    /// stamp), so when it does the log FAILS CLOSED rather than folding the
    /// entry forward silently — the entries are dropped and the floor raised
    /// to the epoch, so no snapshot older than the epoch is repaired from
    /// this log again; the event is counted; and the caller retracts the
    /// snapshot at the epoch itself, which the floor cannot reach.
    pub(crate) fn record(&mut self, ts: u64, e: E) -> bool {
        let poisoned = ts <= self.pruned_to;
        if ts < self.epoch {
            // Stamps must not go backwards: a reader relies on `since(at)`
            // being a suffix. A writer that observed the clock before a
            // concurrent writer's stamp is folded up to the current epoch —
            // its change is still applied by every reader older than that.
            self.entries.push_back((self.epoch, e));
        } else {
            self.epoch = ts;
            self.entries.push_back((ts, e));
        }
        if poisoned {
            counted!("derived.change log poisoned by a stamp below a published snapshot");
            self.entries.clear();
            self.floor = self.floor.max(self.epoch);
            return true;
        }
        if self.entries.len() > self.cap {
            counted!("derived.change log overflowed");
            self.entries.clear();
            self.floor = self.epoch;
        }
        false
    }

    /// Record that the source changed at `ts` in a way this log does not
    /// express. Everything older than `ts` must rebuild.
    ///
    /// No production writer uses this any more: a transaction used to
    /// `touch` every source it changed at commit, and once every statement
    /// was a transaction that made every insert's readers rebuild —
    /// 130 ms per insert on the pod. Commits now replay their entries
    /// (`record`) like the direct path. Kept for the unit test of the
    /// overflow semantics it shares with `record`.
    #[cfg(test)]
    pub(crate) fn touch(&mut self, ts: u64) {
        self.epoch = self.epoch.max(ts);
        self.entries.clear();
        self.floor = self.floor.max(self.epoch);
        counted!("derived.change log touched");
    }

    /// Whether a snapshot taken at `at` can be caught up from this log.
    pub(crate) fn covers(&self, at: u64) -> bool {
        at >= self.floor
    }

    /// The entries newer than `at`, in stamp order.
    pub(crate) fn since(&self, at: u64) -> impl Iterator<Item = &(u64, E)> {
        let start = self.entries.partition_point(|(ts, _)| *ts <= at);
        self.entries.range(start..)
    }

    /// Drop entries at or below `at` — legal only once a snapshot stamped
    /// `>= at` has been PUBLISHED, because after this no older snapshot can
    /// catch up from here (it reloads the published one instead).
    pub(crate) fn prune_below(&mut self, at: u64) {
        let n = self.entries.partition_point(|(ts, _)| *ts <= at);
        if n > 0 {
            self.entries.drain(..n);
            counted!("derived.change log pruned");
        }
        self.floor = self.floor.max(at);
        self.pruned_to = self.pruned_to.max(at);
    }

    /// Pending entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

// ── Slot ────────────────────────────────────────────────────────────────────

/// A published snapshot and the epoch it describes.
#[derive(Debug)]
pub(crate) struct Snapshot<T> {
    pub(crate) at: u64,
    pub(crate) value: Arc<T>,
}

/// Where a derived structure's current snapshot lives. Lock-free to read;
/// published monotonically; built under its OWN single-flight guard.
#[derive(Debug)]
pub(crate) struct Slot<T> {
    inner: arc_swap::ArcSwapOption<Snapshot<T>>,
    /// The build guard for THIS slot. It was one guard per family (every
    /// adjacency table shared one), so a maintenance refresh rebuilding one
    /// table — 25 s for an untyped table on SF1 paged — held every worker
    /// that needed to build ANY other table, which then still built its own.
    /// Per slot, a build of A never delays a builder of B; N workers missing
    /// the SAME table still do one build.
    build: SingleFlight,
    /// The snapshot stamp whose repair cost has already been priced, plus one
    /// so that `0` means "nothing priced yet", and the verdict that pricing
    /// reached.
    ///
    /// Pricing walks the change set, under the same lock every write takes
    /// exclusively. A published snapshot is IMMUTABLE and its change set only
    /// grows, so the verdict "this is too much work for a reader" cannot
    /// become false while the snapshot stands — which makes it a property of
    /// the snapshot rather than of the read, and lets every reader after the
    /// first have it for two atomic loads.
    ///
    /// Measured on `balattr`: pricing per read left 42,801 declining reads
    /// each walking the change set under the log's lock, and the writers went
    /// from 55,816 to 20,364 ops/s. The work was correct and the frequency was
    /// not.
    priced_at: std::sync::atomic::AtomicU64,
    priced_decline: std::sync::atomic::AtomicBool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            inner: arc_swap::ArcSwapOption::empty(),
            build: SingleFlight::default(),
            priced_at: std::sync::atomic::AtomicU64::new(0),
            priced_decline: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl<T> Slot<T> {
    /// The cached repair-cost verdict for the snapshot at `at`, or `None` if
    /// this snapshot has not been priced yet.
    pub(crate) fn priced(&self, at: u64) -> Option<bool> {
        use std::sync::atomic::Ordering::Acquire;
        (self.priced_at.load(Acquire) == at.wrapping_add(1))
            .then(|| self.priced_decline.load(Acquire))
    }

    /// Record the verdict for `at`. A racing pricer reaches the same verdict
    /// from the same immutable snapshot, so the two cannot disagree and the
    /// pair does not need to be written atomically together — the worst a
    /// torn read costs is one extra pricing.
    pub(crate) fn note_priced(&self, at: u64, decline: bool) {
        use std::sync::atomic::Ordering::Release;
        self.priced_decline.store(decline, Release);
        self.priced_at.store(at.wrapping_add(1), Release);
    }

    /// The current snapshot, owned.
    pub(crate) fn load(&self) -> Option<Arc<Snapshot<T>>> {
        self.inner.load_full()
    }

    /// The current snapshot BORROWED through the arc-swap guard, with no
    /// refcount traffic — for the per-hop hot path, where cloning the `Arc`
    /// is one cache line every worker contends on.
    pub(crate) fn peek(&self) -> arc_swap::Guard<Option<Arc<Snapshot<T>>>> {
        self.inner.load()
    }

    /// Enter this slot's build path — held while ONE worker rebuilds this
    /// structure from the store. Never held on a hit or a catch-up.
    pub(crate) fn enter_build(&self) -> std::sync::MutexGuard<'_, ()> {
        self.build.enter()
    }

    /// Publish `value` as the snapshot at `at` — unless a snapshot at a
    /// LATER epoch is already there, in which case this one is discarded and
    /// `false` is returned.
    ///
    /// Monotone by construction. This is the line that closes the lost-row
    /// hazard: a worker that built at an older epoch, however long it took,
    /// cannot overwrite the catch-up another worker published meanwhile.
    pub(crate) fn publish(&self, at: u64, value: Arc<T>) -> bool {
        self.publish_snapshot(Arc::new(Snapshot { at, value }))
    }

    /// [`Slot::publish`] of a snapshot the caller keeps a handle to — so the
    /// worker that built it can SERVE it whether or not the publish won. A
    /// publish loses to an equal stamp too (a fenced publish can land exactly
    /// on the slot's current stamp while a writer is in flight), and the
    /// loser's own snapshot is at least as current as what it lost to; it
    /// used to reload the slot and serve whatever was there.
    pub(crate) fn publish_snapshot(&self, snap: Arc<Snapshot<T>>) -> bool {
        let mut won = false;
        self.inner.rcu(|cur| match cur {
            Some(c) if c.at >= snap.at => {
                won = false;
                cur.clone()
            }
            _ => {
                won = true;
                Some(Arc::clone(&snap))
            }
        });
        if won {
            counted!("derived.snapshot published");
        } else {
            counted!("derived.snapshot publish lost to a newer one");
        }
        won
    }

    /// RETRACT the published snapshot, whatever its stamp: the next reader
    /// finds nothing and builds from the store. The fail-closed response to
    /// a poisoned change log (see [`ChangeLog::record`]) — the one snapshot
    /// the raised floor cannot send to a rebuild is the one stamped AT the
    /// epoch, and that is the one that may be missing the row.
    pub(crate) fn retract(&self) {
        if self.inner.swap(None).is_some() {
            counted!("derived.snapshot retracted");
        }
    }
}

// ── SingleFlight ────────────────────────────────────────────────────────────

/// A guard for the BUILD path only.
///
/// Held while one worker rebuilds a structure from the store so that N
/// workers missing at once do one rebuild. It is never held on the hit path
/// or the catch-up path, so single-threaded traces are unchanged and the
/// common case takes no lock. The loser re-checks the slot after acquiring
/// and usually finds the winner's publish. One per [`Slot`].
#[derive(Debug, Default)]
pub(crate) struct SingleFlight(std::sync::Mutex<()>);

impl SingleFlight {
    pub(crate) fn enter(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── MembersView — an id set that catches up in O(delta) ────────────────────

/// Past this many pending ids in the overlay, fold into a new base.
///
/// Every `contains` is two or three binary searches whatever the overlay
/// holds, so this bounds the overlay's MEMORY and the cost of the sorted
/// insert that maintains it, not the read. Chosen equal to the range index's
/// `FOLD_AT` so the two structures age the same way.
pub(crate) const MEMBERS_FOLD_AT: usize = 4_096;

/// Ids a base must hold before a presence bitmap is worth building over it.
///
/// Below this the base fits in a few cache lines and a binary search over it
/// costs ~12 probes that mostly hit; the bitmap would save nothing and cost an
/// allocation.
pub(crate) const MEMBERS_BITS_MIN: usize = 4_096;

/// Bytes of bitmap a base may spend PER ID before it is judged too sparse.
///
/// The bitmap covers the base's id SPAN, so a label of n ids scattered over a
/// span of s costs s/8 bytes. Four bytes per id caps that at half what the
/// base vector itself already costs (8 bytes an id), which is the point past
/// which a denser test stops being obviously worth its memory.
pub(crate) const MEMBERS_BITS_MAX_BYTES_PER_ID: u64 = 4;

/// A dense presence bitmap over a base's id span.
///
/// `MembersView::contains` walks the base with a binary search — ~21 probes
/// over 3.1M `Message` ids at SF1, nearly all cache misses, on EVERY candidate
/// peer a hop's label filter examines. This answers the same question with one
/// probe into a structure small enough to stay resident: `Message`'s span is
/// its own length, so 3.1M ids cost 390 KB.
///
/// Built at most once per BASE and shared by every view that carries it — the
/// overlay changes on catch-up but the base does not, so a bitmap outlives
/// every catch-up between two rebuilds.
#[derive(Debug)]
pub(crate) struct BaseBits {
    lo: u64,
    words: Box<[u64]>,
}

impl BaseBits {
    /// Build over `base`, or decline when it is too small or too sparse to be
    /// worth the memory. `base` must be sorted ascending.
    pub(crate) fn build(base: &[u64]) -> Option<BaseBits> {
        let lo = *base.first()?;
        let hi = *base.last()?;
        if base.len() < MEMBERS_BITS_MIN {
            return None;
        }
        let span = hi - lo + 1;
        let words = span.div_ceil(64);
        if words.saturating_mul(8) > (base.len() as u64).saturating_mul(MEMBERS_BITS_MAX_BYTES_PER_ID) {
            return None;
        }
        let mut w = vec![0u64; usize::try_from(words).ok()?];
        for &id in base {
            let off = id - lo;
            w[usize::try_from(off >> 6).expect("span fits, checked above")] |= 1u64 << (off & 63);
        }
        Some(BaseBits { lo, words: w.into_boxed_slice() })
    }

    /// Is `id` present? One bounds check and one probe.
    pub(crate) fn test(&self, id: u64) -> bool {
        let Some(off) = id.checked_sub(self.lo) else {
            return false;
        };
        let Ok(w) = usize::try_from(off >> 6) else {
            return false;
        };
        self.words.get(w).is_some_and(|x| (x >> (off & 63)) & 1 == 1)
    }
}

/// A sorted set of ids as an immutable base plus a small overlay.
///
/// The membership of a label was an `Arc<Vec<u64>>`, and applying a delta of
/// k ids to it copied the whole vector: O(label) per catch-up. This keeps the
/// base shared and untouched, records adds and removes in two small sorted
/// overlays, and folds into a new base only past `MEMBERS_FOLD_AT`.
///
/// Invariants, maintained by [`MembersView::apply`] and relied on by `len`:
/// `added ∩ base = ∅` and `removed ⊆ base`. They make `len` O(1) and
/// `contains` three binary searches.
#[derive(Debug, Clone)]
pub struct MembersView {
    base: Arc<Vec<u64>>,
    added: Arc<Vec<u64>>,
    removed: Arc<Vec<u64>>,
    /// The materialised sorted vector, for the consumers that need a slice:
    /// computed at most ONCE per snapshot and shared by every clone of it.
    /// Without this a snapshot carrying an overlay was re-materialised by
    /// every read that asked for a slice — O(label) per query for as long as
    /// the overlay lived, which under a read-only load is for ever. Measured
    /// on the pod: the 90k-id `:Message` label copied on every pipeline
    /// statement, ~1.5 ms a read.
    flat: Arc<std::sync::OnceLock<Arc<Vec<u64>>>>,
    /// A presence bitmap over the BASE, built at most once per base and shared
    /// by every view that carries it (catch-up clones the base, so it clones
    /// this too). `None` inside the `OnceLock` records a base judged too small
    /// or too sparse — a decision, taken once, not re-taken per probe.
    base_bits: Arc<std::sync::OnceLock<Option<BaseBits>>>,
    /// Base probes seen so far, so the bitmap is built only once THIS base has
    /// proven busy. A label touched a handful of times must not pay a build.
    base_probes: Arc<std::sync::atomic::AtomicU64>,
}

impl MembersView {
    /// A view over a sorted id vector, with no overlay.
    pub fn from_base(base: Arc<Vec<u64>>) -> Self {
        MembersView {
            base,
            added: Arc::new(Vec::new()),
            removed: Arc::new(Vec::new()),
            flat: Arc::new(std::sync::OnceLock::new()),
            base_bits: Arc::new(std::sync::OnceLock::new()),
            base_probes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// The empty set.
    pub fn empty() -> Self {
        Self::from_base(Arc::new(Vec::new()))
    }

    /// Membership test: three binary searches, no allocation.
    pub fn contains(&self, id: u64) -> bool {
        self.contains_with(id, 0)
    }

    /// [`MembersView::contains`], with the base answered from a presence
    /// BITMAP once this base has been probed `bitmap_after` times.
    ///
    /// The overlays stay binary searches — they are small and cache-resident,
    /// and they are what changes. Only the base is worth a denser form, and
    /// only when it is large, dense and busy: the threshold is a probe count
    /// rather than a size so a label touched a handful of times never pays a
    /// build. `0` never builds, which is what plain `contains` asks for.
    pub fn contains_with(&self, id: u64, bitmap_after: usize) -> bool {
        if self.added.binary_search(&id).is_ok() {
            return true;
        }
        self.base_contains(id, bitmap_after) && self.removed.binary_search(&id).is_err()
    }

    fn base_contains(&self, id: u64, bitmap_after: usize) -> bool {
        if bitmap_after == 0 {
            return self.base.binary_search(&id).is_ok();
        }
        if let Some(decided) = self.base_bits.get() {
            return match decided {
                Some(bits) => bits.test(id),
                None => self.base.binary_search(&id).is_ok(),
            };
        }
        let seen = self
            .base_probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if seen < bitmap_after as u64 {
            return self.base.binary_search(&id).is_ok();
        }
        match self.base_bits.get_or_init(|| {
            let built = BaseBits::build(&self.base);
            if built.is_some() {
                counted!("derived.members base bitmap built");
                crate::counters::MEMBERS_BITMAPS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                counted!("derived.members base declined a bitmap");
            }
            built
        }) {
            Some(bits) => bits.test(id),
            None => self.base.binary_search(&id).is_ok(),
        }
    }

    /// The number of ids — O(1) by the invariants above.
    pub fn len(&self) -> usize {
        self.base.len() - self.removed.len() + self.added.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether any change is layered over the base.
    pub fn has_overlay(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }

    /// The ids in ascending order — a sorted merge of the base (minus removes)
    /// and the adds.
    ///
    /// The removals are applied by a single forward cursor, not a binary
    /// search per base element. `removed ⊆ base` and both are sorted, so one
    /// pass suffices: this is O(base + removed) rather than
    /// O(base · log removed), which over a 3.1M-id `:Message` base is 3.1M
    /// searches saved on every materialisation.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        let mut rem = self.removed.iter().copied().peekable();
        let mut base = self
            .base
            .iter()
            .copied()
            .filter(move |id| {
                while rem.peek().is_some_and(|r| r < id) {
                    rem.next();
                }
                if rem.peek() == Some(id) {
                    rem.next();
                    return false;
                }
                true
            })
            .peekable();
        let mut add = self.added.iter().copied().peekable();
        std::iter::from_fn(move || match (base.peek(), add.peek()) {
            (Some(b), Some(a)) => {
                if a < b {
                    add.next()
                } else {
                    base.next()
                }
            }
            (Some(_), None) => base.next(),
            (None, Some(_)) => add.next(),
            (None, None) => None,
        })
    }

    /// Materialise as a plain sorted vector, for the consumers that genuinely
    /// need a slice. With no overlay this is a clone of the shared base's
    /// `Arc`, not of its contents; with one, the O(n) merge is paid once per
    /// snapshot and every later call (from any clone) shares it.
    pub fn to_arc_vec(&self) -> Arc<Vec<u64>> {
        if !self.has_overlay() {
            return Arc::clone(&self.base);
        }
        Arc::clone(self.flat.get_or_init(|| {
            counted!("derived.members view materialised");
            crate::counters::MEMBERS_MATERIALISED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let out: Vec<u64> = self.iter().collect();
            crate::counters::MEMBERS_FLAT_ROWS
                .fetch_add(out.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Arc::new(out)
        }))
    }

    /// Apply `changes` — `(id, joined)` pairs in stamp order — and return the
    /// caught-up view. O((overlay + k) + k log k) for k changes; folds past
    /// `MEMBERS_FOLD_AT`. This is the BATCHED fold (see
    /// [`MembersView::apply_batched`]); [`MembersView::apply_serial`] is the
    /// fold it replaced, kept as the differential arm.
    pub fn apply(&self, changes: impl IntoIterator<Item = (u64, bool)>) -> MembersView {
        self.apply_batched(changes)
    }

    /// [`MembersView::apply`] with the fold chosen by the caller — the lever
    /// the graph exposes so a test can prove the two arms agree.
    pub fn apply_with(&self, changes: impl IntoIterator<Item = (u64, bool)>, batch: bool) -> MembersView {
        if batch {
            self.apply_batched(changes)
        } else {
            self.apply_serial(changes)
        }
    }

    /// The fold as one sort and one merge pass.
    ///
    /// Only the LAST change to an id decides its state — a join after a leave
    /// is a member, a leave after a join is not — so the k changes collapse to
    /// a sorted map of last-change-per-id (O(k log k)), and that map merges
    /// with the sorted `added`/`removed` overlays in a single two-pointer pass
    /// (O(overlay + k)). Every id in the map REPLACES its old overlay entry;
    /// every id not in the map keeps it. That is exactly the state the serial
    /// fold reaches by applying each change in turn, proven by the
    /// `batched_fold_equals_serial_fold` differential below.
    ///
    /// The serial fold inserted each change into a sorted `Vec` at its
    /// position — O(overlay) per change, O(k²) per catch-up — and a 5k-write
    /// burst with no reader between made the first `:Message` scan after it
    /// 40-50× its steady cost.
    pub fn apply_batched(&self, changes: impl IntoIterator<Item = (u64, bool)>) -> MembersView {
        let mut last: BTreeMap<u64, bool> = BTreeMap::new();
        for (id, joined) in changes {
            last.insert(id, joined); // later stamps overwrite: last change wins
        }
        let mut added: Vec<u64> = Vec::with_capacity(self.added.len() + last.len());
        let mut removed: Vec<u64> = Vec::with_capacity(self.removed.len() + last.len());
        let mut a = self.added.iter().copied().peekable();
        let mut r = self.removed.iter().copied().peekable();
        for (&id, &joined) in &last {
            // Carry the old overlay entries below `id` forward unchanged.
            while a.peek().is_some_and(|&x| x < id) {
                added.push(a.next().expect("peeked"));
            }
            while r.peek().is_some_and(|&x| x < id) {
                removed.push(r.next().expect("peeked"));
            }
            // An old entry AT `id` is superseded by the change.
            if a.peek() == Some(&id) {
                a.next();
            }
            if r.peek() == Some(&id) {
                r.next();
            }
            let in_base = self.base.binary_search(&id).is_ok();
            match (joined, in_base) {
                (true, true) | (false, false) => {} // the base already says so
                (true, false) => added.push(id),
                (false, true) => removed.push(id),
            }
        }
        added.extend(a);
        removed.extend(r);
        self.finish_apply(added, removed)
    }

    /// The fold as it was: each change inserted into the sorted overlay at
    /// its position — O(overlay) per change. Kept for the differential test
    /// and the graph's `members_batch_fold` lever; not the production path.
    pub fn apply_serial(&self, changes: impl IntoIterator<Item = (u64, bool)>) -> MembersView {
        let mut added: Vec<u64> = (*self.added).clone();
        let mut removed: Vec<u64> = (*self.removed).clone();
        for (id, joined) in changes {
            let in_base = self.base.binary_search(&id).is_ok();
            if joined {
                if in_base {
                    // Re-added after a remove: undo the remove.
                    if let Ok(i) = removed.binary_search(&id) {
                        removed.remove(i);
                    }
                } else if let Err(i) = added.binary_search(&id) {
                    added.insert(i, id);
                }
            } else if in_base {
                if let Err(i) = removed.binary_search(&id) {
                    removed.insert(i, id);
                }
            } else if let Ok(i) = added.binary_search(&id) {
                added.remove(i);
            }
        }
        self.finish_apply(added, removed)
    }

    /// The tail both folds share: the new view over the shared base, folded
    /// into a fresh base past [`MEMBERS_FOLD_AT`].
    fn finish_apply(&self, added: Vec<u64>, removed: Vec<u64>) -> MembersView {
        let next = MembersView {
            base: Arc::clone(&self.base),
            added: Arc::new(added),
            removed: Arc::new(removed),
            // A new snapshot, so a fresh materialisation cache — but the SAME
            // base, so the same bitmap and the same probe count. The overlay is
            // what moved; the base the bitmap describes did not.
            flat: Arc::new(std::sync::OnceLock::new()),
            base_bits: Arc::clone(&self.base_bits),
            base_probes: Arc::clone(&self.base_probes),
        };
        if next.added.len() + next.removed.len() > MEMBERS_FOLD_AT {
            counted!("derived.members view folded");
            crate::counters::MEMBERS_FOLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return MembersView::from_base(Arc::new(next.iter().collect()));
        }
        counted!("derived.members view caught up");
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_log_since_is_a_suffix_and_prune_raises_the_floor() {
        let mut l: ChangeLog<u64> = ChangeLog::new(100);
        for ts in 1..=5u64 {
            l.record(ts * 10, ts);
        }
        assert_eq!(l.epoch(), 50);
        let got: Vec<u64> = l.since(20).map(|(_, e)| *e).collect();
        assert_eq!(got, vec![3, 4, 5], "since(20) must be exactly the entries stamped after 20");
        assert!(l.covers(0));
        l.prune_below(30);
        assert!(!l.covers(20), "a snapshot older than the prune point cannot catch up");
        assert!(l.covers(30));
        let got: Vec<u64> = l.since(30).map(|(_, e)| *e).collect();
        assert_eq!(got, vec![4, 5]);
    }

    #[test]
    fn a_backwards_stamp_is_folded_forward_not_reordered() {
        let mut l: ChangeLog<&str> = ChangeLog::new(100);
        l.record(10, "a");
        l.record(5, "b"); // observed the clock before a concurrent writer
        let got: Vec<&str> = l.since(9).map(|(_, e)| *e).collect();
        assert_eq!(got, vec!["a", "b"], "the late entry must still be seen by a reader at 9");
    }

    #[test]
    fn overflow_and_touch_force_a_rebuild_rather_than_a_short_catch_up() {
        let mut l: ChangeLog<u64> = ChangeLog::new(3);
        for ts in 1..=4u64 {
            l.record(ts, ts);
        }
        assert_eq!(l.len(), 0, "past the cap the entries are dropped");
        assert!(!l.covers(0), "and a snapshot from before the overflow must rebuild");
        assert!(l.covers(4));
        let mut t: ChangeLog<u64> = ChangeLog::new(3);
        t.record(1, 1);
        t.touch(7);
        assert_eq!(t.epoch(), 7);
        assert!(!t.covers(6));
    }

    #[test]
    fn slot_publish_is_monotone() {
        let s: Slot<&str> = Slot::default();
        assert!(s.publish(5, Arc::new("five")));
        assert!(!s.publish(3, Arc::new("three")), "an older build must not overwrite");
        assert_eq!(*s.load().expect("snap").value, "five");
        assert!(s.publish(6, Arc::new("six")));
        assert_eq!(*s.load().expect("snap").value, "six");
        s.retract();
        assert!(s.load().is_none(), "a retracted slot holds nothing");
        assert!(s.publish(2, Arc::new("two")), "after a retract any stamp publishes");
    }

    /// A stamp at or below what a PUBLISHED snapshot pruned behind poisons
    /// the log: the entries go, the floor rises to the epoch, the caller is
    /// told. An overflow-raised floor is NOT a published snapshot and a late
    /// stamp below it is folded forward as before — that case is legitimate
    /// (a writer in flight across an overflow) and must not fail closed.
    #[test]
    fn a_stamp_below_a_published_snapshot_poisons_the_log() {
        let mut l: ChangeLog<&str> = ChangeLog::new(100);
        assert!(!l.record(10, "a"));
        l.prune_below(10); // a snapshot at 10 was published
        assert!(!l.record(12, "b"));
        assert!(l.record(9, "late"), "a stamp below the published snapshot must poison");
        assert_eq!(l.len(), 0, "the entries are dropped");
        assert_eq!(l.epoch(), 12);
        assert!(!l.covers(11), "everything below the epoch must rebuild");
        assert!(l.covers(12));
        // Overflow raises the floor but not `pruned_to`.
        let mut o: ChangeLog<u64> = ChangeLog::new(2);
        for ts in 1..=3u64 {
            assert!(!o.record(ts, ts));
        }
        assert!(!o.covers(2), "overflowed");
        assert!(!o.record(2, 9), "a late stamp below an overflow floor is folded, not poison");
    }

    /// Two slots' build guards are independent: holding one does not block
    /// entering the other. (Holding the SAME one does — that is the guard.)
    #[test]
    fn build_guards_are_per_slot() {
        let a: Slot<u8> = Slot::default();
        let b: Slot<u8> = Slot::default();
        let _held = a.enter_build();
        assert!(a.build.0.try_lock().is_err(), "the same slot's guard is held");
        assert!(b.build.0.try_lock().is_ok(), "another slot's guard is free");
    }

    #[test]
    fn members_view_contains_len_iter_agree_with_a_plain_set() {
        let base: Vec<u64> = (0..1000).map(|i| i * 2).collect(); // evens
        let v = MembersView::from_base(Arc::new(base.clone()));
        // add some odds, remove some evens, re-add a removed even
        let v = v.apply(vec![(1, true), (3, true), (4, false), (6, false), (4, true), (1999, true)]);
        let mut expect: std::collections::BTreeSet<u64> = base.into_iter().collect();
        expect.insert(1);
        expect.insert(3);
        expect.remove(&6);
        expect.insert(1999);
        assert_eq!(v.len(), expect.len());
        assert_eq!(v.iter().collect::<Vec<_>>(), expect.iter().copied().collect::<Vec<_>>());
        for id in 0..2001u64 {
            assert_eq!(v.contains(id), expect.contains(&id), "id {id}");
        }
        assert_eq!(*v.to_arc_vec(), expect.iter().copied().collect::<Vec<_>>());
        // Materialised once per snapshot, shared across clones.
        let again = v.clone();
        assert!(
            Arc::ptr_eq(&v.to_arc_vec(), &again.to_arc_vec()),
            "a clone must share the snapshot's materialised vector, not copy again"
        );
    }

    /// The BITMAP arm must answer exactly what the binary-search arm answers,
    /// on the same view, for every id in and around the base's span — and the
    /// build must actually happen, or the differential compares one path with
    /// itself.
    #[test]
    fn the_base_bitmap_answers_exactly_what_the_binary_search_answers() {
        // Dense enough to accept a bitmap and past MEMBERS_BITS_MIN: 8,192 ids
        // over a span of 24,576, which is 3 bytes an id.
        let base: Vec<u64> = (0..8_192u64).map(|i| 1_000 + i * 3).collect();
        let v = MembersView::from_base(Arc::new(base.clone()));
        let v = v.apply(vec![(7, true), (1_003, false), (999_999, true), (1_006, false)]);

        // THE CANARY: prove the bitmap is built and used, not silently declined.
        // One probe past the threshold is enough; `base_bits` records the
        // decision either way, so `Some(Some(_))` is the build having happened.
        assert!(v.contains_with(1_000, 1), "an id in the base");
        assert!(
            matches!(v.base_bits.get(), Some(Some(_))),
            "the bitmap must have been BUILT — a declined base would take the              binary-search path and this test would compare it with itself"
        );

        for id in 0..3_000u64 {
            assert_eq!(v.contains_with(id, 1), v.contains(id), "low id {id}");
        }
        for id in 24_000..26_000u64 {
            assert_eq!(v.contains_with(id, 1), v.contains(id), "span edge id {id}");
        }
        for id in [0, 7, 999, 1_000, 1_003, 1_006, 25_573, 25_574, 999_999, u64::MAX] {
            assert_eq!(v.contains_with(id, 1), v.contains(id), "boundary id {id}");
        }
        // And against a plain set, so neither arm is the oracle for the other.
        let mut expect: std::collections::BTreeSet<u64> = base.into_iter().collect();
        expect.insert(7);
        expect.insert(999_999);
        expect.remove(&1_003);
        expect.remove(&1_006);
        for id in 0..3_000u64 {
            assert_eq!(v.contains_with(id, 1), expect.contains(&id), "vs set, id {id}");
        }

        // A base too sparse for the memory bound DECLINES, and still answers.
        let sparse = MembersView::from_base(Arc::new(
            (0..8_192u64).map(|i| i * 64).collect::<Vec<_>>(),
        ));
        assert!(sparse.contains_with(64, 1));
        assert!(
            matches!(sparse.base_bits.get(), Some(None)),
            "8 bytes an id is past MEMBERS_BITS_MAX_BYTES_PER_ID — the decision              is taken once and recorded, not re-taken on every probe"
        );
        assert!(!sparse.contains_with(65, 1));

        // Below the probe threshold nothing is built at all.
        let cold = MembersView::from_base(Arc::new((0..8_192u64).collect::<Vec<_>>()));
        assert!(cold.contains_with(5, 1_000_000));
        assert!(cold.base_bits.get().is_none(), "a barely-probed base pays no build");
    }

    /// The batched fold must reach exactly the state the serial fold reaches,
    /// over a change stream that exercises every transition: an id joining
    /// and leaving repeatedly, ids in and out of the base, and an existing
    /// overlay to merge with. Byte-identical overlays, not merely the same
    /// membership — the overlays are what later folds build on.
    #[test]
    fn batched_fold_equals_serial_fold() {
        // A deterministic LCG: no `rand` dependency, reproducible failures.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let base: Vec<u64> = (0..5_000u64).map(|i| i * 3).collect();
        let start = MembersView::from_base(Arc::new(base));
        // An existing overlay, itself built serially.
        let seed: Vec<(u64, bool)> = (0..300).map(|_| (next() % 20_000, next() % 2 == 0)).collect();
        let start = start.apply_serial(seed);
        assert!(start.has_overlay());
        for k in [1usize, 2, 17, 1_000, 6_000] {
            let changes: Vec<(u64, bool)> = (0..k).map(|_| (next() % 20_000, next() % 3 != 0)).collect();
            let serial = start.apply_serial(changes.clone());
            let batched = start.apply_batched(changes);
            assert_eq!(*serial.added, *batched.added, "k={k}: added overlays differ");
            assert_eq!(*serial.removed, *batched.removed, "k={k}: removed overlays differ");
            assert_eq!(serial.len(), batched.len(), "k={k}");
            assert_eq!(
                serial.iter().collect::<Vec<_>>(),
                batched.iter().collect::<Vec<_>>(),
                "k={k}: memberships differ"
            );
            assert!(Arc::ptr_eq(&serial.base, &batched.base) || !serial.has_overlay());
        }
    }

    #[test]
    fn members_view_folds_past_the_threshold() {
        let v = MembersView::from_base(Arc::new(vec![0]));
        let big: Vec<(u64, bool)> = (1..=(MEMBERS_FOLD_AT as u64 + 1)).map(|i| (i, true)).collect();
        let v = v.apply(big);
        assert!(!v.has_overlay(), "past the threshold the overlay must fold into a new base");
        assert_eq!(v.len(), MEMBERS_FOLD_AT + 2);
        assert!(v.contains(MEMBERS_FOLD_AT as u64 + 1));
    }
}
