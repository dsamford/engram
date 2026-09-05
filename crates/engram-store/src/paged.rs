//! Track B milestone M1 — the block cache and the `paged` segment reader.
//!
//! A [`PagedSegment`] keeps only the small resident anchors of an on-disk
//! segment — its footer and its sparse first-key index — and resolves each
//! data block **on demand**: a point get locates the one covering block by a
//! binary search over first keys, fetches it (cache hit, or `pread` + BLAKE3
//! verify + decode on a miss), and reads the key out of it. A bounded
//! [`BlockCache`] keyed `(seq, block_offset)` holds decoded blocks under a byte
//! budget, so a graph larger than the cache answers every query with bounded
//! memory — the property `paged` mode exists for.
//!
//! # What it preserves
//!
//! `PagedSegment::get_at` returns the SAME `Version` a resident
//! [`Segment`](crate::segment) would (M1's differential gate). The block is the
//! read/verify unit: a corrupt block fails BLAKE3 at the exact `pread` that
//! needs it — never a silent wrong answer. Eviction is drop-the-frame (sealed
//! blocks are clean, never dirty), so there is no writeback stall.
//!
//! The cache uses `Arc`-pinned file handles (design fork (a)) and **S3-FIFO-lite
//! scan-resistant admission** (M2): a small probation queue in front of a main
//! queue, so a one-shot scan cannot evict the hot working set. Async readahead
//! and the CSR/Gorder adjacency reorder at seal are the remaining M2 items.

// M1.0: the block cache + paged reader are complete and gated by this module's
// differential tests, but not yet CALLED from non-test `Store` code — that
// wiring (a `StorageMode::Paged` that resolves sealed segments through here) is
// M1.1. Remove this allow when the Store dispatches to `PagedSegment`.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use engram_observe::counted;

use crate::sst::{self, BlockHandle, SegmentFooter, SstError};
use crate::{LogicalKey, Version};

/// A decoded data block: its entries in key order, shared out of the cache.
type Block = Arc<Vec<(LogicalKey, Vec<Version>)>>;

/// Opening a `paged` segment failed — the OS read, or the format/verification.
#[derive(Debug)]
pub enum OpenError {
    /// The segment file could not be opened or read.
    Io(std::io::Error),
    /// The footer or index did not parse / verify.
    Format(SstError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "paged segment I/O: {e}"),
            OpenError::Format(e) => write!(f, "paged segment format: {e:?}"),
        }
    }
}
impl std::error::Error for OpenError {}

// ── the block cache ─────────────────────────────────────────────────────────

struct CacheSlot {
    block: Block,
    size: usize,
    /// Touched by a `get` since it was (re)admitted — the second-chance bit.
    hit: bool,
    /// In the MAIN queue (true) or the small probation queue (false).
    in_main: bool,
}

struct Shard {
    /// `BTreeMap` (not `HashMap`) to satisfy the store's determinism lint — the
    /// cache is never iterated in a trace-affecting way, but the type ban is
    /// blanket; ordered lookup over a bounded map is cheap.
    map: BTreeMap<(u64, u64), CacheSlot>,
    /// The SMALL probation queue: every newly-fetched block enters here. A block
    /// evicted from `small` WITHOUT a hit is dropped (a scan block dies here); a
    /// block that WAS hit is promoted to `main`. Kept to ~10% of the budget.
    small: VecDeque<(u64, u64)>,
    /// The MAIN queue: the durable working set. Evicted with a second chance
    /// (a hit block rotates to the back; an un-hit block is dropped).
    main: VecDeque<(u64, u64)>,
    small_bytes: usize,
    main_bytes: usize,
}

/// A bounded, sharded block cache keyed `(segment seq, block offset)`.
/// **S3-FIFO-lite** admission (Yang et al., SOSP'23): a small probation FIFO in
/// front of a main FIFO makes it SCAN-RESISTANT — a one-shot scan fills only the
/// small queue and its blocks are evicted from there without ever displacing the
/// hot working set in `main`. Blocks re-referenced while in the small queue are
/// promoted; the main queue evicts with a CLOCK-style second chance.
pub struct BlockCache {
    shards: Vec<Mutex<Shard>>,
    /// Total per-shard byte budget.
    per_shard_budget: usize,
    /// The probation queue's share (~10% of the budget) — the scan sink.
    small_budget: usize,
}

impl BlockCache {
    /// A cache holding at most `budget_bytes` of decoded blocks (spread across
    /// a fixed shard count). A budget smaller than the graph is the point.
    pub fn new(budget_bytes: usize) -> Arc<BlockCache> {
        Self::with_shards(budget_bytes, 8)
    }

    /// As [`BlockCache::new`] but with an explicit shard count — tests use a
    /// single shard so eviction is deterministic rather than spread.
    pub(crate) fn with_shards(budget_bytes: usize, shard_count: usize) -> Arc<BlockCache> {
        let shard_count = shard_count.max(1);
        let per_shard_budget = (budget_bytes / shard_count).max(1);
        let small_budget = (per_shard_budget / 10).max(1);
        let shards = (0..shard_count)
            .map(|_| {
                Mutex::new(Shard {
                    map: BTreeMap::new(),
                    small: VecDeque::new(),
                    main: VecDeque::new(),
                    small_bytes: 0,
                    main_bytes: 0,
                })
            })
            .collect();
        Arc::new(BlockCache {
            shards,
            per_shard_budget,
            small_budget,
        })
    }

    /// Bytes of decoded blocks the cache holds right now, across every shard —
    /// the attribution a memory report needs beside the budget. Takes each
    /// shard's lock briefly; called from a reporting thread, not a query.
    pub fn resident_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                let g = s.lock().unwrap_or_else(|e| e.into_inner());
                g.small_bytes + g.main_bytes
            })
            .sum()
    }

    /// The cache's total budget in bytes (the per-shard budgets summed).
    pub fn budget_bytes(&self) -> usize {
        self.per_shard_budget * self.shards.len()
    }

    fn shard_for(&self, seq: u64, offset: u64) -> &Mutex<Shard> {
        // A cheap mix so consecutive block offsets of one segment spread across
        // shards rather than all landing in one.
        let mixed = seq
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(offset.wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        &self.shards[(mixed as usize) % self.shards.len()]
    }

    /// The cached block, or `None`. A hit sets the block's second-chance bit —
    /// which both keeps it in `main` and PROMOTES it out of the probation queue
    /// on that queue's next eviction (the re-reference signal S3-FIFO admits on).
    fn get(&self, seq: u64, offset: u64) -> Option<Block> {
        let mut shard = self.shard_for(seq, offset).lock().expect("cache shard");
        match shard.map.get_mut(&(seq, offset)) {
            Some(slot) => {
                slot.hit = true;
                counted!("paged.block cache hit");
                Some(Arc::clone(&slot.block))
            }
            None => {
                counted!("paged.block cache miss");
                None
            }
        }
    }

    /// [`BlockCache::get`] for a SPAN SCAN: the cached block if present, WITHOUT
    /// setting its second-chance bit. A scan touches every block exactly once,
    /// so its touch is not a re-reference signal — treating it as one would
    /// promote every probation block the scan crossed and renew the second
    /// chance of every main block, which is how a full-table rebuild came to
    /// look like the working set to the eviction loop.
    fn get_scan(&self, seq: u64, offset: u64) -> Option<Block> {
        let shard = self.shard_for(seq, offset).lock().expect("cache shard");
        match shard.map.get(&(seq, offset)) {
            Some(slot) => {
                counted!("paged.block cache scan hit");
                Some(Arc::clone(&slot.block))
            }
            None => {
                counted!("paged.block cache scan miss");
                None
            }
        }
    }

    /// Admit a block a SPAN SCAN faulted in — only into FREE ROOM. A scan of a
    /// corpus smaller than the cache leaves it resident (the 2 GB-cache case,
    /// where every block used to re-fault on the next scan); a scan of a corpus
    /// larger than the cache READS THROUGH and admits nothing, so the working
    /// set in `main` and `small` is never displaced by rows read once. Nothing
    /// is evicted on this path, by construction.
    fn insert_scan(&self, seq: u64, offset: u64, block: Block, size: usize) {
        let mut shard = self.shard_for(seq, offset).lock().expect("cache shard");
        let key = (seq, offset);
        if shard.map.contains_key(&key) {
            return; // a concurrent fetch admitted it; the block is immutable
        }
        if shard.small_bytes + shard.main_bytes + size > self.per_shard_budget {
            counted!("paged.block scan bypassed the cache");
            return;
        }
        counted!("paged.block scan admitted into free room");
        shard.map.insert(
            key,
            CacheSlot {
                block,
                size,
                hit: false,
                in_main: false,
            },
        );
        shard.small.push_back(key);
        shard.small_bytes += size;
    }

    /// Admit a freshly-fetched block into the probation queue, then evict to the
    /// budget. A concurrent fetch of the same block overwrites with an identical
    /// value (blocks are immutable) — its queue membership is left as-is.
    fn insert(&self, seq: u64, offset: u64, block: Block, size: usize) {
        let mut shard = self.shard_for(seq, offset).lock().expect("cache shard");
        let key = (seq, offset);
        match shard.map.get_mut(&key) {
            Some(old) => {
                // Already present — refresh bytes, keep its queue; mark hit.
                let delta = size as isize - old.size as isize;
                if old.in_main {
                    shard.main_bytes = (shard.main_bytes as isize + delta).max(0) as usize;
                } else {
                    shard.small_bytes = (shard.small_bytes as isize + delta).max(0) as usize;
                }
                let old = shard.map.get_mut(&key).expect("present");
                old.block = block;
                old.size = size;
                old.hit = true;
                return;
            }
            None => {
                shard.map.insert(
                    key,
                    CacheSlot {
                        block,
                        size,
                        hit: false,
                        in_main: false,
                    },
                );
                shard.small.push_back(key);
                shard.small_bytes += size;
            }
        }
        self.evict_to_budget(&mut shard, key);
    }

    /// Evict until within budget, never evicting the just-admitted `keep`.
    /// Drains the probation queue first (where scan blocks die), promoting any
    /// re-referenced block to `main`; then the main queue with a second chance.
    fn evict_to_budget(&self, shard: &mut Shard, keep: (u64, u64)) {
        // Nothing is evicted while the shard has ROOM. The probation share used
        // to be enforced unconditionally, so a one-touch block was dropped at
        // 10% of the budget even with the other 90% empty: a warm-up scan of a
        // 200 MB corpus under a 2 GB cache left ~10% resident and the next
        // scan re-faulted every block. Eviction is a response to being full,
        // not a policy applied to free memory; the probation share below is
        // what decides WHICH blocks go once there is something to decide.
        if shard.small_bytes + shard.main_bytes <= self.per_shard_budget {
            return;
        }
        // Keep the probation queue near its share: over-cap probation blocks are
        // either promoted (if hit) or dropped (if not) — this is the scan sink.
        let mut guard = shard.small.len() + shard.main.len() + 2;
        while shard.small_bytes > self.small_budget && guard > 0 {
            guard -= 1;
            let Some(v) = shard.small.pop_front() else {
                break;
            };
            if v == keep {
                shard.small.push_back(v);
                continue;
            }
            let hit = shard.map.get(&v).map(|s| s.hit).unwrap_or(false);
            let size = shard.map.get(&v).map(|s| s.size).unwrap_or(0);
            shard.small_bytes -= size;
            if hit {
                // Re-referenced in probation → promote to the main queue.
                if let Some(s) = shard.map.get_mut(&v) {
                    s.in_main = true;
                    s.hit = false;
                }
                shard.main.push_back(v);
                shard.main_bytes += size;
            } else {
                shard.map.remove(&v);
                counted!("paged.block evicted");
            }
        }
        // Total over budget → evict from main with a CLOCK second chance.
        let mut guard = shard.main.len() * 2 + 2;
        while shard.small_bytes + shard.main_bytes > self.per_shard_budget && guard > 0 {
            guard -= 1;
            let Some(v) = shard.main.pop_front() else {
                break;
            };
            if v == keep {
                shard.main.push_back(v);
                continue;
            }
            let hit = shard.map.get(&v).map(|s| s.hit).unwrap_or(false);
            if hit {
                if let Some(s) = shard.map.get_mut(&v) {
                    s.hit = false; // spend the second chance
                }
                shard.main.push_back(v);
            } else {
                if let Some(s) = shard.map.remove(&v) {
                    shard.main_bytes -= s.size;
                    counted!("paged.block evicted");
                }
            }
        }
    }
}

/// The decoded-size estimate a block contributes to the cache budget: key
/// bytes + per-version overhead + value bytes. Approximate but monotone, which
/// is all the budget needs.
/// What a decoded block COSTS IN MEMORY — the number the cache budget is
/// enforced on, so it must be the heap the block actually holds, not the
/// bytes it encodes.
///
/// It used to charge `key.len() + 16` per entry and `16 + value.len()` per
/// version, which is roughly the ON-DISK size. The resident form is
/// `Vec<(Vec<u8>, Vec<Version>)>` with `Arc<[u8]>` values: 48 bytes of tuple
/// per entry, a key allocation, 32 bytes per `Version`, and an `Arc` header
/// plus the bytes per value — about 2× the old charge for the small
/// adjacency and index entries that dominate the platform mirror. With a
/// 4 GiB budget the pod held 4,084 "MB" of cache and 11.85 GB of anonymous
/// RSS against 5.4 GB attributed; the gap was mostly this undercount. A
/// budget that is not enforced on real bytes is a limit met by the thing it
/// limits.
///
/// Allocations are rounded the way a size-class allocator rounds them
/// (16-byte granularity plus ~12 % class slack), so the charge tracks RSS
/// rather than the sum of lengths.
fn block_size(entries: &[(LogicalKey, Vec<Version>)]) -> usize {
    #[inline]
    fn alloc(n: usize) -> usize {
        let rounded = n.div_ceil(16) * 16;
        rounded + rounded / 8
    }
    const TUPLE: usize = std::mem::size_of::<(LogicalKey, Vec<Version>)>();
    const VERSION: usize = std::mem::size_of::<Version>();
    // Arc<[u8]>: two words of refcounts before the bytes.
    const ARC_HEADER: usize = 2 * std::mem::size_of::<usize>();
    // The outer vector is shrunk to fit at decode, so its length is its
    // capacity.
    let mut n = alloc(entries.len() * TUPLE);
    for (k, versions) in entries {
        n += alloc(k.capacity());
        n += alloc(versions.capacity() * VERSION);
        for v in versions {
            if let Some(b) = v.value.as_ref() {
                n += alloc(ARC_HEADER + b.len());
            }
        }
    }
    n
}

// ── the paged segment ───────────────────────────────────────────────────────

/// An on-disk segment read block-by-block through a shared [`BlockCache`].
/// Holds only the resident anchors (footer fields + sparse index + file
/// handle); block bytes live on disk and transit the cache.
pub struct PagedSegment {
    seq: u64,
    max_commit_ts: u64,
    /// Tombstone density from the footer (v3+). `(0, 0)` for a v2 file, which
    /// did not record it — "cannot say", not "holds nothing".
    tombstones: u64,
    versions: u64,
    file: Arc<File>,
    index: Vec<BlockHandle>,
    cache: Arc<BlockCache>,
}

impl PagedSegment {
    /// Open a segment file for paged reads against `cache`. **Bounded open**:
    /// `pread`s only the fixed footer (from the file tail) and then the sparse
    /// index extent it names — never the whole file — so opening a graph larger
    /// than RAM costs only its per-segment anchors, not a resident load. Data
    /// blocks fault in later, on demand. Footer and index BLAKE3 are verified.
    pub fn open(path: &Path, cache: Arc<BlockCache>) -> Result<PagedSegment, OpenError> {
        let file = File::open(path).map_err(OpenError::Io)?;
        let len = file.metadata().map_err(OpenError::Io)?.len();
        if len < sst::FOOTER_LEN as u64 {
            return Err(OpenError::Format(SstError::Truncated { what: "footer" }));
        }
        // pread the fixed footer from the tail; `read_footer` accepts a buffer
        // whose last FOOTER_LEN bytes are the footer — here that is all of it.
        let mut footer_buf = vec![0u8; sst::FOOTER_LEN];
        pread_exact(&file, len - sst::FOOTER_LEN as u64, &mut footer_buf).map_err(OpenError::Io)?;
        let footer: SegmentFooter = sst::read_footer(&footer_buf).map_err(OpenError::Format)?;
        // pread exactly the index region and parse it.
        let mut index_buf = vec![0u8; footer.index_len as usize];
        pread_exact(&file, footer.index_offset, &mut index_buf).map_err(OpenError::Io)?;
        let index = sst::read_index(&index_buf).map_err(OpenError::Format)?;
        Ok(PagedSegment {
            seq: footer.seq,
            max_commit_ts: footer.max_commit_ts,
            tombstones: footer.tombstones,
            versions: footer.versions,
            file: Arc::new(file),
            index,
            cache,
        })
    }

    /// The segment's seal sequence.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The greatest commit timestamp any version in this segment carries — a
    /// store opened from disk advances its clock past it.
    pub(crate) fn max_commit_ts(&self) -> u64 {
        self.max_commit_ts
    }

    /// The first key of every data block — the sparse index's keys.
    ///
    /// These are natural CHUNK BOUNDARIES for a streaming merge: splitting on
    /// them means a chunk is a bounded number of whole blocks per segment, so
    /// peak memory is O(segments x blocks-per-chunk) rather than O(corpus).
    pub(crate) fn block_first_keys(&self) -> Vec<LogicalKey> {
        self.index.iter().map(|h| h.first_key.clone()).collect()
    }

    /// `(tombstones, versions)` from the footer, or `None` for a v2 file that
    /// did not record them.
    ///
    /// `None` and `Some((0, n))` are DIFFERENT: the first means this segment
    /// cannot say, the second means it says there are no tombstones. Folding
    /// them together would let an old file read as a clean one.
    pub(crate) fn tombstone_counts(&self) -> Option<(u64, u64)> {
        if self.versions == 0 {
            return None;
        }
        Some((self.tombstones, self.versions))
    }

    /// The index of the one block whose key range covers `key`: the last block
    /// whose first key is `<= key`. `None` when `key` sorts before every block
    /// (so this segment cannot hold it).
    fn covering_block(&self, key: &[u8]) -> Option<usize> {
        if self.index.is_empty() || key < self.index[0].first_key.as_slice() {
            return None;
        }
        // partition_point → count of handles with first_key <= key; the last
        // such is that minus one. Blocks are contiguous key ranges, so exactly
        // one block can hold the key.
        let after = self
            .index
            .partition_point(|h| h.first_key.as_slice() <= key);
        Some(after - 1)
    }

    /// Fetch block `bi` — cache hit, or `pread` its frame, verify BLAKE3,
    /// decode, and admit.
    fn block(&self, bi: usize) -> Result<Block, SstError> {
        self.block_in(bi, false)
    }

    /// [`PagedSegment::block`] with the cache policy chosen by the caller:
    /// `scan` reads through the cache without promoting a hit or displacing a
    /// resident block on a miss (see [`BlockCache::get_scan`] /
    /// [`BlockCache::insert_scan`]). The bytes returned are identical either
    /// way — only what the cache remembers about the touch differs.
    fn block_in(&self, bi: usize, scan: bool) -> Result<Block, SstError> {
        let h = &self.index[bi];
        let cached = if scan {
            self.cache.get_scan(self.seq, h.offset)
        } else {
            self.cache.get(self.seq, h.offset)
        };
        if let Some(b) = cached {
            return Ok(b);
        }
        // Miss: read the payload + its 32-byte hash trailer in one positioned
        // read, verify, decode.
        let frame_len = (h.len as usize)
            .checked_add(sst::HASH_LEN)
            .ok_or(SstError::Corrupt {
                why: "block frame overflow",
            })?;
        let mut frame = vec![0u8; frame_len];
        counted!("paged.pread");
        pread_exact(&self.file, h.offset, &mut frame).map_err(|_| SstError::Truncated {
            what: "data block pread",
        })?;
        let entries = sst::verify_and_decode_block(&frame, h.offset)?;
        let size = block_size(&entries);
        let block: Block = Arc::new(entries);
        if scan {
            self.cache
                .insert_scan(self.seq, h.offset, Arc::clone(&block), size);
        } else {
            self.cache
                .insert(self.seq, h.offset, Arc::clone(&block), size);
        }
        Ok(block)
    }

    /// The newest version at or below `ts` for `key`, if this segment holds one
    /// — read through the block cache, byte-identical to a resident
    /// [`Segment::get_at`](crate::segment::Segment). Crate-internal until M1.1
    /// wires `paged` into the `Store`'s public read API (which returns the
    /// public value types, not the internal `Version`).
    pub(crate) fn get_at(&self, key: &LogicalKey, ts: u64) -> Result<Option<Version>, SstError> {
        let Some(bi) = self.covering_block(key) else {
            return Ok(None);
        };
        let block = self.block(bi)?;
        let Ok(row) = block.binary_search_by(|(k, _)| k.as_slice().cmp(key)) else {
            return Ok(None); // the covering block is the only candidate
        };
        let versions = &block[row].1;
        Ok(versions.iter().rev().find(|v| v.commit_ts <= ts).cloned())
    }

    /// The projected form a resident `get_projected_at` would return: `None` if
    /// this segment holds no visible version, `Some(None)` for a tombstone,
    /// `Some(Some(Record))` for a value. A paged segment is row form, so a
    /// present value is always a `Projected::Record` (never `Columns`). `props`
    /// is unused — the whole record is returned, as the resident row path does.
    pub(crate) fn get_projected_at(
        &self,
        key: &LogicalKey,
        ts: u64,
        _props: &[u32],
    ) -> Result<Option<Option<crate::segment::Projected>>, SstError> {
        Ok(self
            .get_at(key, ts)?
            .map(|v| {
                v.value
                    .map(|b| crate::segment::Projected::Record(b.to_vec()))
            }))
    }

    /// Call `f(key, versions)` for every key in `[lo, hi)`, in key order —
    /// paged equivalent of `Segment::range`. Fetches only the blocks that
    /// overlap the range (they are contiguous key ranges), through the cache.
    pub(crate) fn range_for_each(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        mut f: impl FnMut(&LogicalKey, &[Version]),
    ) -> Result<(), SstError> {
        self.range_for_each_until_in(lo, hi, false, |k, v| {
            f(k, v);
            true
        })
        .map(|_| ())
    }

    /// [`PagedSegment::range_for_each`] for a SPAN SCAN — the same keys in the
    /// same order, fetched through the cache's scan policy so that a walk of
    /// the whole segment neither promotes what it crosses nor evicts what a
    /// reader is using (see [`BlockCache::get_scan`]).
    pub(crate) fn range_for_each_scan(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        mut f: impl FnMut(&LogicalKey, &[Version]),
    ) -> Result<(), SstError> {
        self.range_for_each_until_in(lo, hi, true, |k, v| {
            f(k, v);
            true
        })
        .map(|_| ())
    }

    /// [`PagedSegment::range_for_each`] with an EARLY STOP: `f` answers
    /// whether to continue, and the call answers whether the walk ran to
    /// completion. The point of the variant is what it does NOT do: once the
    /// visitor stops, no further block is fetched, verified or decoded — a
    /// budgeted reader over a span of thousands of blocks pays for the blocks
    /// up to its budget, not for the span.
    pub(crate) fn range_for_each_until(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        f: impl FnMut(&LogicalKey, &[Version]) -> bool,
    ) -> Result<bool, SstError> {
        self.range_for_each_until_in(lo, hi, false, f)
    }

    fn range_for_each_until_in(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        scan: bool,
        mut f: impl FnMut(&LogicalKey, &[Version]) -> bool,
    ) -> Result<bool, SstError> {
        // First block that can hold a key >= lo: the block covering lo, or block
        // 0 when lo sorts before every block.
        let start = self.covering_block(lo).unwrap_or(0);
        for bi in start..self.index.len() {
            // Blocks are sorted by first key; once a block starts at/after hi,
            // no later block can contribute.
            if let Some(h) = hi {
                if self.index[bi].first_key.as_slice() >= h {
                    break;
                }
            }
            let block = self.block_in(bi, scan)?;
            for (k, versions) in block.iter() {
                if k.as_slice() < lo {
                    continue;
                }
                if let Some(h) = hi {
                    if k.as_slice() >= h {
                        return Ok(true); // sorted within and across blocks
                    }
                }
                if !f(k, versions) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Decode every block into a full `(key -> chain)` map — the compactor's
    /// input, the paged analogue of `Segment::cloned_entries`. Loads the whole
    /// segment (compaction is not a steady-state read), verifying each block.
    pub(crate) fn decode_all(&self) -> Result<BTreeMap<LogicalKey, Vec<Version>>, SstError> {
        let mut out = BTreeMap::new();
        for bi in 0..self.index.len() {
            for (k, versions) in self.block(bi)?.iter() {
                out.insert(k.clone(), versions.clone());
            }
        }
        Ok(out)
    }
}

// ── positioned reads (pread / seek_read), thread-safe on a shared handle ─────

#[cfg(unix)]
fn pread_exact(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn pread_exact(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short positioned read",
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, StoredValue};
    use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};

    fn pfx() -> KeyPrefix {
        KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            kind: Kind::NODE,
            partition: Partition(1),
        }
    }

    /// Build a real sealed segment (its keys are FULL logical keys — the store
    /// prefixes realm/ns/kind/partition), write it to a temp file, return the
    /// resident segment plus the path.
    fn sealed_to_disk(name: &str, n: u32) -> (Store, std::path::PathBuf) {
        let s = Store::new();
        for i in 0..n {
            let k = format!("node-{i:05}").into_bytes();
            s.put(&pfx(), &k, StoredValue::Plain(vec![(i % 251) as u8; 64]))
                .expect("put");
        }
        // Multi-version chains on a few keys.
        for i in [3u32, 100, n - 1] {
            let k = format!("node-{i:05}").into_bytes();
            s.put(&pfx(), &k, StoredValue::Plain(vec![0xAA; 8]))
                .expect("overwrite");
        }
        s.seal().expect("seal");
        let path = std::env::temp_dir().join(name);
        let segs = s.sealed_segments_for_test();
        let resident = segs[0].as_resident().expect("resident seal");
        crate::sst::write_segment_file(resident, &path).expect("write");
        (s, path)
    }

    /// The M1 gate at segment granularity: a real sealed segment read
    /// block-by-block through a cache SMALLER than the data answers `get_at`
    /// identically to the resident segment — proving fault-in, eviction,
    /// re-fetch and BLAKE3 verification all preserve results. Queried with the
    /// REAL logical keys (from the resident segment), so it is not vacuous.
    #[test]
    fn paged_get_at_equals_resident_under_a_small_cache() {
        let (s, path) = sealed_to_disk("engram_paged_m1_roundtrip.seg", 500);
        let segs = s.sealed_segments_for_test();
        let resident = segs[0].as_resident().expect("resident");
        let entries: Vec<(LogicalKey, Vec<Version>)> = resident
            .row_entries()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // ~40 KiB of data; an 8 KiB cache forces eviction and re-fetch.
        let cache = BlockCache::new(8 * 1024);
        let (_, trace) = engram_observe::with_trace(|| {
            let paged = PagedSegment::open(&path, cache).expect("open paged");
            assert_eq!(paged.seq(), resident.seq);

            let mut any_some = false;
            for (k, versions) in &entries {
                for v in versions {
                    for probe in [v.commit_ts.saturating_sub(1), v.commit_ts, v.commit_ts + 1] {
                        let got = paged.get_at(k, probe).expect("paged read");
                        assert_eq!(got, resident.get_at(k, probe), "paged vs resident disagree");
                    }
                }
                let hi = paged.get_at(k, u64::MAX).expect("paged read");
                assert_eq!(hi, resident.get_at(k, u64::MAX));
                any_some |= hi.is_some();
            }
            // Not vacuous: the real keys actually resolve to values.
            assert!(any_some, "test is vacuous — no key returned a value");
            // A key sorting before every block, and one after the last: absent.
            assert_eq!(paged.get_at(&b"\x00".to_vec(), u64::MAX).unwrap(), None);
            assert_eq!(paged.get_at(&vec![0xFFu8; 64], u64::MAX).unwrap(), None);
        });

        // The cache was actually exercised: misses (fault-ins), evictions (cache
        // < data), and at least one hit (a block re-read while still resident).
        let c = trace.counters();
        assert!(
            c.get("paged.block cache miss").copied().unwrap_or(0) > 0,
            "no misses"
        );
        assert!(
            c.get("paged.block evicted").copied().unwrap_or(0) > 0,
            "cache never evicted"
        );
        assert!(
            c.get("paged.block cache hit").copied().unwrap_or(0) > 0,
            "no cache hits"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The rebuild-scan pin: a reader's working set is resident in a cache
    /// SMALLER than the segment; a whole-segment walk under the scan policy
    /// (what an adjacency rebuild does) runs through it; afterwards the
    /// working set's reads still hit — `paged.block cache miss` is unchanged
    /// for them — and the walk bypassed the full cache rather than churning it.
    #[test]
    fn a_rebuild_scan_leaves_the_working_set_hitting() {
        let (s, path) = sealed_to_disk("engram_paged_scan_working_set.seg", 3000);
        let segs = s.sealed_segments_for_test();
        let resident = segs[0].as_resident().expect("resident");
        let keys: Vec<LogicalKey> = resident.row_entries().keys().cloned().collect();
        // ~22 blocks of ~16 KiB behind a single-shard cache that holds five:
        // the three-block working set fits, the segment does not.
        let cache = BlockCache::with_shards(96 * 1024, 1);
        let paged = PagedSegment::open(&path, Arc::clone(&cache)).expect("open");
        // The working set: three keys far apart, each read twice (the
        // re-reference that makes them the working set).
        let ws = [&keys[10], &keys[keys.len() / 2], &keys[keys.len() - 10]];
        for _ in 0..2 {
            for k in ws {
                assert!(paged.get_at(k, u64::MAX).expect("read").is_some());
            }
        }
        let (rows, trace) = engram_observe::with_trace(|| {
            let mut rows = 0usize;
            paged
                .range_for_each_scan(&[], None, |_, _| rows += 1)
                .expect("scan");
            rows
        });
        assert_eq!(rows, keys.len(), "the scan must visit every row");
        let c = trace.counters();
        assert!(
            c.get("paged.block scan bypassed the cache").copied().unwrap_or(0) > 0,
            "vacuous: the cache was not smaller than the segment: {c:?}"
        );
        let (_, trace) = engram_observe::with_trace(|| {
            for k in ws {
                assert!(paged.get_at(k, u64::MAX).expect("read").is_some());
            }
        });
        let c = trace.counters();
        assert_eq!(
            c.get("paged.block cache miss").copied().unwrap_or(0),
            0,
            "the rebuild scan evicted a working-set block: {c:?}"
        );
        assert_eq!(c.get("paged.pread").copied().unwrap_or(0), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// The 2 GB-cache pin: a walk of a segment SMALLER than the cache leaves
    /// it resident, so the next walk performs no `pread` at all. Under the old
    /// unconditional probation drain the second walk re-faulted ~90% of it.
    #[test]
    fn a_scan_under_a_cache_with_room_makes_the_next_scan_free() {
        let (_s, path) = sealed_to_disk("engram_paged_scan_room.seg", 3000);
        let cache = BlockCache::with_shards(4 << 20, 1);
        let paged = PagedSegment::open(&path, cache).expect("open");
        let (first, trace) = engram_observe::with_trace(|| {
            let mut rows = 0usize;
            paged.range_for_each_scan(&[], None, |_, _| rows += 1).expect("scan");
            rows
        });
        let preads = trace.counters().get("paged.pread").copied().unwrap_or(0);
        assert!(preads > 1, "vacuous: a one-block segment");
        let (second, trace) = engram_observe::with_trace(|| {
            let mut rows = 0usize;
            paged.range_for_each_scan(&[], None, |_, _| rows += 1).expect("scan");
            rows
        });
        assert_eq!(first, second);
        let c = trace.counters();
        assert_eq!(
            c.get("paged.pread").copied().unwrap_or(0),
            0,
            "the second scan re-faulted blocks the cache had room for: {c:?}"
        );
        assert_eq!(c.get("paged.block cache scan hit").copied().unwrap_or(0), preads);
        let _ = std::fs::remove_file(&path);
    }

    /// A corrupt on-disk block is caught at the `pread` that faults it in — a
    /// hard error, never a silent wrong/empty answer.
    #[test]
    fn a_corrupt_block_is_caught_on_fault_in() {
        let (s, path) = sealed_to_disk("engram_paged_m1_corrupt.seg", 300);
        // The first logical key lives in the FIRST data block (byte 0's block).
        let segs = s.sealed_segments_for_test();
        let first_key = segs[0]
            .as_resident()
            .expect("resident")
            .row_entries()
            .keys()
            .next()
            .expect("a key")
            .clone();

        // Flip a byte inside the first data block (byte 0 of the file).
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[0] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("rewrite");

        let cache = BlockCache::new(1024 * 1024);
        let paged = PagedSegment::open(&path, cache).expect("open: footer/index still valid");
        match paged.get_at(&first_key, u64::MAX) {
            Err(SstError::HashMismatch { at }) => {
                assert_ne!(at, u64::MAX, "a block, not the footer")
            }
            other => panic!("expected a block HashMismatch, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod cache_tests {
    //! The block cache's S3-FIFO admission must be SCAN-RESISTANT: a one-shot
    //! scan of many blocks must not evict a repeatedly-used hot working set.

    use super::*;

    fn block(tag: u8) -> Block {
        Arc::new(vec![(vec![tag], vec![])])
    }

    #[test]
    fn s3fifo_scan_does_not_evict_the_hot_set() {
        // One shard, budget 1000 bytes (~20 × 50), small (probation) ~100 = 2 blocks.
        let cache = BlockCache::with_shards(1000, 1);
        const SZ: usize = 50;

        // Admit a small hot working set and RE-REFERENCE each immediately (the
        // signal S3-FIFO promotes on) so it moves out of probation into main.
        for off in 0..5u64 {
            cache.insert(1, off, block(off as u8), SZ);
            assert!(cache.get(1, off).is_some(), "just-admitted hot block {off}");
        }
        // Touch them again — they are the hot working set.
        for _ in 0..2 {
            for off in 0..5u64 {
                assert!(
                    cache.get(1, off).is_some(),
                    "hot block {off} must be resident"
                );
            }
        }

        // A big one-shot scan: 60 distinct blocks, each fetched once (seq 2).
        for off in 0..60u64 {
            cache.insert(2, 1000 + off, block(0), SZ);
        }

        // The hot set survives (promoted to / kept in main); the scan blocks are
        // gone (they cycled through the small probation queue and were dropped).
        for off in 0..5u64 {
            assert!(
                cache.get(1, off).is_some(),
                "the scan evicted hot block {off} — cache is NOT scan-resistant"
            );
        }
        let scan_resident = (0..60u64)
            .filter(|&off| cache.get(2, 1000 + off).is_some())
            .count();
        assert!(
            scan_resident <= 4,
            "scan should leave only a few probation blocks resident, got {scan_resident}"
        );
    }

    /// A FULL cache holding a reader's working set in `main`; a rebuild scan
    /// through the SCAN policy reads 60 blocks through it. Afterwards every
    /// working-set block still hits — `paged.block cache miss` does not move
    /// for them — and the scan admitted nothing (there was no room).
    #[test]
    fn a_scan_through_a_full_cache_bypasses_it_and_evicts_nothing() {
        let cache = BlockCache::with_shards(1000, 1);
        const SZ: usize = 50;
        // 18 hot blocks, re-referenced (hit); 3 fillers push the shard over
        // budget so the drain PROMOTES the hot set into main and drops the
        // un-hit fillers — the steady state of a served graph.
        for off in 0..18u64 {
            cache.insert(1, off, block(off as u8), SZ);
            assert!(cache.get(1, off).is_some());
        }
        for off in 100..104u64 {
            cache.insert(1, off, block(0), SZ);
        }
        // Full, exactly: the hot set in main (900) + two fillers in probation.
        let (_, trace) = engram_observe::with_trace(|| {
            for off in 0..60u64 {
                if cache.get_scan(2, 1000 + off).is_none() {
                    cache.insert_scan(2, 1000 + off, block(0), SZ);
                }
            }
        });
        let c = trace.counters();
        assert_eq!(
            c.get("paged.block scan bypassed the cache").copied().unwrap_or(0),
            60,
            "every scan block must be read through, not admitted: {c:?}"
        );
        let (_, trace) = engram_observe::with_trace(|| {
            for off in 0..18u64 {
                assert!(
                    cache.get(1, off).is_some(),
                    "the scan evicted working-set block {off}"
                );
            }
        });
        assert_eq!(
            trace.counters().get("paged.block cache miss").copied().unwrap_or(0),
            0,
            "a working-set read missed after the scan"
        );
        assert_eq!(
            (0..60u64).filter(|&o| cache.get_scan(2, 1000 + o).is_some()).count(),
            0,
            "a scan block was admitted into a full cache"
        );
    }

    /// The 2 GB-cache case: a scan of a corpus SMALLER than the cache must
    /// leave it resident, so the next scan faults nothing. Under the old
    /// unconditional probation drain, 90% of it was dropped with the cache
    /// 90% empty.
    #[test]
    fn a_scan_with_room_leaves_the_corpus_resident() {
        let cache = BlockCache::with_shards(10_000, 1);
        for off in 0..60u64 {
            assert!(cache.get_scan(3, off).is_none());
            cache.insert_scan(3, off, block(0), 50);
        }
        let resident = (0..60u64).filter(|&o| cache.get_scan(3, o).is_some()).count();
        assert_eq!(resident, 60, "a scan into free room must retain every block");
        // And the plain path agrees: a one-touch block is not dropped while
        // there is room for it.
        for off in 0..60u64 {
            cache.insert(4, off, block(0), 50);
        }
        let resident = (0..60u64).filter(|&o| cache.get(4, o).is_some()).count();
        assert_eq!(resident, 60, "the plain admission dropped blocks with room to spare");
    }

    /// A scan touch is NOT a re-reference: a probation block a scan crosses is
    /// dropped by the next drain, where the same block touched by a reader is
    /// promoted. This is the promotion pollution the scan policy exists to
    /// prevent.
    #[test]
    fn a_scan_touch_does_not_promote_a_probation_block() {
        let cache = BlockCache::with_shards(1000, 1);
        const SZ: usize = 50;
        cache.insert(1, 1, block(1), SZ); // touched by a reader below
        cache.insert(1, 2, block(2), SZ); // touched only by a scan below
        assert!(cache.get(1, 1).is_some());
        assert!(cache.get_scan(1, 2).is_some());
        // Fill past the budget so the probation queue drains.
        for off in 10..40u64 {
            cache.insert(1, off, block(0), SZ);
        }
        assert!(cache.get(1, 1).is_some(), "the reader-touched block must be promoted");
        assert!(cache.get(1, 2).is_none(), "the scan-touched block must NOT be promoted");
    }
}
