//! Sealed segments — the LSM's immutable half.
//!
//! A segment is the memtable, frozen: every logical key's FULL version chain,
//! in memcomparable order, immutable from the moment it is sealed. Reads merge
//! the mutable tail with the sealed stack newest-segment-first, and the frozen
//! key encoding is what makes that merge a comparison of bytes.
//!
//! # Why versions seal WITH the data
//!
//! A segment keeps each key's versions rather than collapsing to the newest.
//! Two reasons, both load-bearing:
//!
//!  - **A snapshot read (`get_at`) must keep working after a flush.** Collapse
//!    on seal and every reader positioned before the flush sees the future —
//!    the exact semantics violation L3 exists to prevent, introduced by the
//!    storage layer underneath a correct MVCC.
//!  - **Version retirement is COMPACTION's decision, not sealing's.** Sealing
//!    is a memory-pressure event; what is safe to discard depends on the
//!    oldest live reader, which sealing has no business knowing.
//!
//! # The seal is a fence, not a copy of convenience
//!
//! [`crate::Store::seal`] moves the ENTIRE tail — there is no partial seal. A partial
//! seal would have to choose a boundary mid-version-chain, and a chain split
//! across tail and segment is two places for one truth.
//!
//! # Rows and blocks — two forms, one truth per key
//!
//! A compacted segment may carry part of its keys as signature-homogeneous
//! COLUMN BLOCKS (see [`crate::columnar`]) beside the row-form `entries`. The
//! two are disjoint by construction — `build_blocks` removes every blocked
//! key from the row map — so a key resolves in exactly one place within a
//! segment, and the read paths consult rows first, blocks second, purely as
//! an ordering convention. Seal-created segments never carry blocks; only
//! compaction builds them, because only compaction knows which chains have
//! collapsed to a single live version.

use std::collections::BTreeMap;

use std::path::Path;
use std::sync::Arc;

use crate::columnar::ColumnBlock;
use crate::paged::{BlockCache, OpenError, PagedSegment};
use crate::{LogicalKey, Version};

/// One sealed, immutable, sorted segment.
#[derive(Debug)]
pub struct Segment {
    /// Seal order: 0 for the first seal, rising. Newer segments shadow older.
    pub seq: u64,
    /// Every logical key's version chain at seal time, oldest first (the
    /// tail's append order — readers walk from the back).
    entries: BTreeMap<LogicalKey, Vec<Version>>,
    /// Signature-homogeneous column blocks — compaction-built, disjoint from
    /// `entries`. Empty for seal-created segments.
    blocks: Vec<ColumnBlock>,
    /// `(key hash, block)` for every blocked key, sorted by hash: a point
    /// get binary-searches this once and probes the block(s) it names,
    /// instead of probing EVERY block. Measured on the production port
    /// (278 blocks): a get walked the whole block list — a `starts_with`
    /// and a binary search each — before it found its row, ~60 µs a get,
    /// and one statement paid 9,312 of them. ~10 bytes a row.
    block_index: Vec<(u64, u16)>,
    /// The greatest commit timestamp any version here carries, computed ONCE
    /// at construction (see [`max_ts_of`]).
    ///
    /// Cached because OCC validation asks for it per validated key, per
    /// segment, INSIDE the global commit latch — and the validator's early
    /// `break` is evaluated *after* the call, so the newest segment is
    /// interrogated unconditionally on every commit. Recomputing it walked
    /// every version of every entry plus every block: at the default
    /// `seal_after_versions = 65_536` that is ~590k iterations per commit,
    /// under the one latch that cannot be parallelised.
    ///
    /// Sound to cache because a `Segment` is immutable: `entries` and `blocks`
    /// are private, written only by the two constructors, and no `&mut self`
    /// method exists.
    max_commit_ts: u64,
    /// How many versions here are TOMBSTONES, and how many versions there are
    /// in total — computed once, for the same reason and by the same argument
    /// as `max_commit_ts`.
    ///
    /// Compaction was scheduled purely by SEGMENT COUNT, which cannot tell a
    /// segment full of live rows from one that is mostly deletions waiting to
    /// be reclaimed. A delete-heavy workload therefore accumulated tombstones
    /// that every scan, every prefix walk and every `merge_span` kept paying
    /// for, until the count threshold happened to fire.
    tombstones: u64,
    /// Total versions across row chains and column blocks — the denominator.
    versions: u64,
    /// The commit log's next sequence at the SEAL that made this segment:
    /// every logged record below it is in this segment or an older one. A
    /// spill that puts the segment on disk may checkpoint the WAL below it
    /// (`Store::checkpoint_wal`). Zero for a compaction-built segment, which
    /// never bounds a checkpoint on its own.
    log_upto: u64,
}

/// `(tombstones, versions)` across row chains and column blocks.
///
/// A column block holds only live rows by construction (`build_blocks`
/// qualifies on `v.value` being present), so every block row counts toward the
/// denominator and none toward the numerator.
pub(crate) fn tombstones_of(
    entries: &BTreeMap<LogicalKey, Vec<Version>>,
    blocks: &[ColumnBlock],
) -> (u64, u64) {
    let mut dead = 0u64;
    let mut total = 0u64;
    for chain in entries.values() {
        for v in chain {
            total += 1;
            if v.value.is_none() {
                dead += 1;
            }
        }
    }
    for b in blocks {
        total += b.commit_ts.len() as u64;
    }
    (dead, total)
}

/// The greatest commit timestamp across row chains and column blocks — the
/// single expression of that rule.
///
/// [`Segment`] caches it at construction and `sst::write_segment` stamps the
/// footer from the cached field, so the in-memory value and the on-disk one
/// cannot drift apart. They used to be two independent expressions of the same
/// rule, which is exactly the shape a silent divergence hides in.
pub(crate) fn max_ts_of(
    entries: &BTreeMap<LogicalKey, Vec<Version>>,
    blocks: &[ColumnBlock],
) -> u64 {
    let rows = entries
        .values()
        .flat_map(|vs| vs.iter().map(|v| v.commit_ts))
        .max()
        .unwrap_or(0);
    let cols = blocks
        .iter()
        .flat_map(|b| b.commit_ts.iter().copied())
        .max()
        .unwrap_or(0);
    rows.max(cols)
}

/// A deterministic 64-bit hash of a key — FNV-1a. Not a secret and not
/// a guard: a collision costs one extra probe, which `row_of` resolves.
pub(crate) fn key_hash(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One key's record, as a point get returns it: the row's bytes, or —
/// for a block row — only the REQUESTED columns, straight from the
/// column bytes, with nothing else copied.
#[derive(Debug, Clone, PartialEq)]
pub enum Projected {
    /// A row-form record: the full encoded bytes (decode and pick).
    Record(Vec<u8>),
    /// A block row: `(property id, tagged value)` for each requested
    /// property the row carries, ascending by id.
    Columns(Vec<(u32, Vec<u8>)>),
}

impl Segment {
    pub(crate) fn new(seq: u64, entries: BTreeMap<LogicalKey, Vec<Version>>) -> Self {
        let max_commit_ts = max_ts_of(&entries, &[]);
        let (tombstones, versions) = tombstones_of(&entries, &[]);
        Segment {
            seq,
            entries,
            blocks: Vec::new(),
            block_index: Vec::new(),
            max_commit_ts,
            tombstones,
            versions,
            log_upto: 0,
        }
    }

    /// Record the commit-log boundary of the seal that made this segment.
    pub(crate) fn with_log_upto(mut self, log_upto: u64) -> Self {
        self.log_upto = log_upto;
        self
    }

    /// The commit-log boundary recorded at seal (0 when unknown).
    pub(crate) fn log_upto(&self) -> u64 {
        self.log_upto
    }

    pub(crate) fn with_blocks(
        seq: u64,
        entries: BTreeMap<LogicalKey, Vec<Version>>,
        blocks: Vec<ColumnBlock>,
    ) -> Self {
        assert!(blocks.len() <= usize::from(u16::MAX), "block index width");
        let mut block_index: Vec<(u64, u16)> =
            Vec::with_capacity(blocks.iter().map(ColumnBlock::rows).sum());
        for (bi, b) in blocks.iter().enumerate() {
            let bi = bi as u16;
            block_index.extend(b.keys.iter().map(|k| (key_hash(k), bi)));
        }
        block_index.sort_unstable();
        let max_commit_ts = max_ts_of(&entries, &blocks);
        let (tombstones, versions) = tombstones_of(&entries, &blocks);
        Segment {
            seq,
            entries,
            blocks,
            block_index,
            max_commit_ts,
            tombstones,
            versions,
            log_upto: 0,
        }
    }

    /// The blocks that may hold `key`, by the index: usually one, more
    /// only on a hash collision. Every block named is then probed by
    /// `row_of`, which is exact.
    fn candidate_blocks(&self, key: &[u8]) -> impl Iterator<Item = usize> + '_ {
        let h = key_hash(key);
        let start = self.block_index.partition_point(|(x, _)| *x < h);
        self.block_index[start..]
            .iter()
            .take_while(move |(x, _)| *x == h)
            .map(|(_, b)| usize::from(*b))
    }

    /// The newest version at or below `ts`, PROJECTED: row-form rows as
    /// their bytes, block rows as only the requested columns. `None` when
    /// the segment holds nothing for the key; `Some(None)` for a tombstone
    /// (this segment's answer is "gone").
    pub(crate) fn get_projected_at(
        &self,
        key: &LogicalKey,
        ts: u64,
        props: &[u32],
    ) -> Option<Option<Projected>> {
        if let Some(v) = self
            .entries
            .get(key)
            .and_then(|vs| vs.iter().rev().find(|v| v.commit_ts <= ts))
        {
            return Some(v.value.clone().map(|b| Projected::Record(b.to_vec())));
        }
        for bi in self.candidate_blocks(key) {
            let b = &self.blocks[bi];
            if !key.starts_with(&b.prefix) {
                continue; // a hash collision across partitions
            }
            engram_observe::counted!("store.block probes");
            if let Some(row) = b.row_of(key) {
                if b.commit_ts[row] > ts {
                    return None; // the block row IS this segment's only version
                }
                engram_observe::sometimes!("store.projected get served from a block", true);
                let mut out = Vec::with_capacity(props.len());
                for &pid in props {
                    if let Some(col) = b.column_of(pid) {
                        out.push((pid, b.columns[col].get(row).to_vec()));
                    }
                }
                out.sort_unstable_by_key(|(pid, _)| *pid);
                return Some(Some(Projected::Columns(out)));
            }
        }
        None
    }

    /// The newest version at or below `ts` for a key, if this segment holds
    /// one — from the row chains or reconstructed from a column block.
    pub(crate) fn get_at(&self, key: &LogicalKey, ts: u64) -> Option<Version> {
        if let Some(v) = self
            .entries
            .get(key)
            .and_then(|vs| vs.iter().rev().find(|v| v.commit_ts <= ts))
        {
            return Some(v.clone());
        }
        for bi in self.candidate_blocks(key) {
            let b = &self.blocks[bi];
            if !key.starts_with(&b.prefix) {
                continue; // a hash collision across partitions
            }
            engram_observe::counted!("store.block probes");
            if let Some(row) = b.row_of(key) {
                if b.commit_ts[row] <= ts {
                    engram_observe::sometimes!("store.columnar read served from a block", true);
                    return Some(b.version_at(row));
                }
                return None; // the block row IS this segment's only version
            }
        }
        None
    }

    /// Logical keys in this segment — row form plus blocked.
    pub fn len(&self) -> usize {
        self.entries.len() + self.blocks.iter().map(ColumnBlock::rows).sum::<usize>()
    }

    /// Whether the segment holds no keys. Sealing an empty tail is refused
    /// upstream, so this is false for every segment that exists — kept for
    /// symmetry and for the compactor, which can empty one.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.blocks.is_empty()
    }

    /// The segment's chains — the compactor's input. Blocks dissolve back into
    /// single-version chains here, so compaction's retention logic sees plain
    /// rows and re-decides the split afresh. Non-consuming (segments live behind
    /// `Arc` a reader may still hold), at the cost of cloning the row-form
    /// chains — blocks are rebuilt from their columns, not cloned.
    pub(crate) fn cloned_entries(&self) -> BTreeMap<LogicalKey, Vec<Version>> {
        let mut entries = self.entries.clone();
        for b in &self.blocks {
            for row in 0..b.rows() {
                entries.insert(b.keys[row].clone(), vec![b.version_at(row)]);
            }
        }
        entries
    }

    /// Range over `[lo, hi)` of ROW-FORM logical keys — the scan's
    /// per-segment leg. Blocked keys are served by [`Segment::blocks`].
    pub(crate) fn range(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
    ) -> impl Iterator<Item = (&LogicalKey, &Vec<Version>)> {
        let lo = lo.to_vec();
        match hi {
            Some(h) => self.entries.range(lo..h.to_vec()),
            None => self.entries.range(lo..),
        }
    }

    /// The segment's column blocks.
    pub(crate) fn blocks(&self) -> &[ColumnBlock] {
        &self.blocks
    }

    /// The greatest commit timestamp any version in this segment carries — row
    /// chains and column blocks alike.
    ///
    /// O(1): the value is computed once at construction. This is called per
    /// validated key per segment inside the global commit latch, so it must
    /// stay O(1) — see the field's doc for what it cost when it was not.
    pub(crate) fn max_commit_ts(&self) -> u64 {
        self.max_commit_ts
    }

    /// Tombstoned versions in this segment.
    pub(crate) fn tombstones(&self) -> u64 {
        self.tombstones
    }

    /// Total versions in this segment — the denominator for a tombstone ratio.
    pub(crate) fn versions(&self) -> u64 {
        self.versions
    }

    /// Recompute from scratch — the tests' oracle for the cached field, so a
    /// constructor that forgets to stamp it is caught rather than inferred.
    #[cfg(test)]
    pub(crate) fn recomputed_max_commit_ts(&self) -> u64 {
        max_ts_of(&self.entries, &self.blocks)
    }

    /// The ROW-FORM version chains, in memcomparable key order. Used by the
    /// on-disk-format round-trip test to assert byte-identity against a real
    /// sealed segment (the writer itself takes `cloned_entries`, which also
    /// dissolves any column blocks into their canonical row form).
    #[cfg(test)]
    pub(crate) fn row_entries(&self) -> &BTreeMap<LogicalKey, Vec<Version>> {
        &self.entries
    }
}

/// A sealed segment as the store holds it: either fully **resident** in RAM
/// (today's default — direct `Segment` pointers) or **paged** — read
/// block-by-block from disk through the [`BlockCache`](crate::paged). The
/// store's read sites call these delegating methods and stay agnostic to the
/// backing; only how a block is resolved (RAM vs cache miss → `pread`) differs.
///
/// A paged read is fallible (I/O, a BLAKE3 mismatch); a resident one is not.
/// Because a failed read of a sealed segment means the durable store is corrupt
/// or unreadable — it cannot serve correct results — this enum surfaces such a
/// failure as a **fatal panic at the fetch** (the design's "hard error surfaced
/// at the fetch"), never a wrong or silently-empty answer. That keeps the read
/// API infallible so `resident` is byte-for-byte unchanged.
pub enum SealedSegment {
    /// The whole segment is in RAM (the default, today's behaviour).
    Resident(Segment),
    /// The segment lives on disk; blocks fault in through the cache.
    Paged(PagedSegment),
}

/// A segment's `[lo, hi)` range materialised as owned `(key, chain)` pairs in
/// key order — a paged segment's contribution to a merge scan.
pub(crate) type OwnedRange = Vec<(LogicalKey, Vec<Version>)>;

/// A failed paged-segment read is unrecoverable: fail loud, naming the segment
/// and the fault, rather than returning a wrong answer.
fn paged_fatal(seq: u64, e: crate::sst::SstError) -> ! {
    panic!("paged segment seq={seq}: durable read failed (corruption or I/O): {e:?}");
}

impl SealedSegment {
    /// Open a sealed segment from disk in PAGED mode; its blocks fault in
    /// through `cache`. Resident segments are built directly (`Resident(..)`).
    pub fn open_paged(path: &Path, cache: Arc<BlockCache>) -> Result<SealedSegment, OpenError> {
        Ok(SealedSegment::Paged(PagedSegment::open(path, cache)?))
    }

    /// The seal sequence — resident and paged alike.
    pub fn seq(&self) -> u64 {
        match self {
            SealedSegment::Resident(s) => s.seq,
            SealedSegment::Paged(p) => p.seq(),
        }
    }

    /// The greatest commit timestamp any version in this segment carries — a
    /// store opened from disk advances its clock past the max over its segments.
    pub(crate) fn max_commit_ts(&self) -> u64 {
        match self {
            SealedSegment::Paged(p) => p.max_commit_ts(),
            SealedSegment::Resident(s) => s.max_commit_ts(),
        }
    }

    /// The resident segment behind this backing, if any — for the on-disk
    /// writer (which serialises a resident segment) and tests.
    pub(crate) fn as_resident(&self) -> Option<&Segment> {
        match self {
            SealedSegment::Resident(s) => Some(s),
            SealedSegment::Paged(_) => None,
        }
    }

    /// The newest version at or below `ts` for `key`, if this segment holds one.
    pub(crate) fn get_at(&self, key: &LogicalKey, ts: u64) -> Option<Version> {
        match self {
            SealedSegment::Resident(s) => s.get_at(key, ts),
            SealedSegment::Paged(p) => p
                .get_at(key, ts)
                .unwrap_or_else(|e| paged_fatal(p.seq(), e)),
        }
    }

    /// The newest version at or below `ts`, PROJECTED (see [`Segment::get_projected_at`]).
    pub(crate) fn get_projected_at(
        &self,
        key: &LogicalKey,
        ts: u64,
        props: &[u32],
    ) -> Option<Option<Projected>> {
        match self {
            SealedSegment::Resident(s) => s.get_projected_at(key, ts, props),
            SealedSegment::Paged(p) => p
                .get_projected_at(key, ts, props)
                .unwrap_or_else(|e| paged_fatal(p.seq(), e)),
        }
    }

    /// Call `f(key, versions)` for each ROW-FORM key in `[lo, hi)`, in key
    /// order — the delegating form of [`Segment::range`] that both backings
    /// share (a resident range yields borrows from its `BTreeMap`; a paged one
    /// yields borrows into cache blocks it holds for the call).
    pub(crate) fn range_for_each(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        f: impl FnMut(&LogicalKey, &[Version]),
    ) {
        self.range_for_each_in(lo, hi, false, f)
    }

    /// [`SealedSegment::range_for_each`] under the block cache's SCAN policy —
    /// identical keys and versions; a paged backing neither promotes nor
    /// displaces cached blocks for a walk that touches each block once. A
    /// resident backing has no cache and is unchanged.
    pub(crate) fn range_for_each_scan(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        f: impl FnMut(&LogicalKey, &[Version]),
    ) {
        self.range_for_each_in(lo, hi, true, f)
    }

    /// [`SealedSegment::range_for_each`] with an EARLY STOP: `f` answers
    /// whether to continue, and the call answers whether the walk ran to
    /// completion (`false` = the visitor stopped it). A paged backing fetches
    /// no block past the stop — which is the whole point: a budgeted reader
    /// over a wide span must pay for its budget, not for the span (the
    /// production mirror's labels interleave in id space, so a 15-node
    /// label's span was the entire node partition, walked per property read).
    pub(crate) fn range_for_each_until(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        mut f: impl FnMut(&LogicalKey, &[Version]) -> bool,
    ) -> bool {
        match self {
            SealedSegment::Resident(s) => {
                for (k, versions) in s.range(lo, hi) {
                    if !f(k, versions.as_slice()) {
                        return false;
                    }
                }
                true
            }
            SealedSegment::Paged(p) => p
                .range_for_each_until(lo, hi, |k, v| f(k, v))
                .unwrap_or_else(|e| paged_fatal(p.seq(), e)),
        }
    }

    fn range_for_each_in(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        scan: bool,
        mut f: impl FnMut(&LogicalKey, &[Version]),
    ) {
        match self {
            SealedSegment::Resident(s) => {
                for (k, versions) in s.range(lo, hi) {
                    f(k, versions.as_slice());
                }
            }
            SealedSegment::Paged(p) if scan => p
                .range_for_each_scan(lo, hi, |k, v| f(k, v))
                .unwrap_or_else(|e| paged_fatal(p.seq(), e)),
            SealedSegment::Paged(p) => p
                .range_for_each(lo, hi, |k, v| f(k, v))
                .unwrap_or_else(|e| paged_fatal(p.seq(), e)),
        }
    }

    /// For a PAGED segment, its `[lo, hi)` range materialised into an owned
    /// buffer — a k-way merge scan holds every segment's cursor at once, and a
    /// paged cursor cannot yield borrows into cache blocks that outlive the
    /// merge. `None` for a RESIDENT segment, which the caller borrows in place
    /// (so the resident scan never copies — no regression). `scan` selects the
    /// block cache's scan policy (see [`SealedSegment::range_for_each_scan`]).
    pub(crate) fn paged_range_owned(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        scan: bool,
    ) -> Option<OwnedRange> {
        match self {
            SealedSegment::Resident(_) => None,
            SealedSegment::Paged(_) => {
                let mut out = Vec::new();
                self.range_for_each_in(lo, hi, scan, |k, v| out.push((k.clone(), v.to_vec())));
                Some(out)
            }
        }
    }

    /// The segment's column blocks — a paged segment has none (its data is row
    /// form on disk), so its columnar reads simply see an empty slice and the
    /// row-form `range_for_each`/`get_at` serve the data.
    pub(crate) fn blocks(&self) -> &[ColumnBlock] {
        match self {
            SealedSegment::Resident(s) => s.blocks(),
            SealedSegment::Paged(_) => &[],
        }
    }

    /// The segment's chains — the compactor's input (see [`Segment::cloned_entries`]).
    pub(crate) fn cloned_entries(&self) -> BTreeMap<LogicalKey, Vec<Version>> {
        match self {
            SealedSegment::Resident(s) => s.cloned_entries(),
            SealedSegment::Paged(p) => p.decode_all().unwrap_or_else(|e| paged_fatal(p.seq(), e)),
        }
    }
}
