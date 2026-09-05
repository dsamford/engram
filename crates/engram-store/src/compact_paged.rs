//! Streaming compaction for the PAGED path.
//!
//! # Why this exists
//!
//! [`Store::compact`] materialises its merged output as a RESIDENT segment.
//! That is the one allocation a bigger-than-RAM store cannot make, so the
//! server never called compaction on the paged path at all — and the paged
//! path is the mode the large corpora actually serve in.
//!
//! The consequence was not merely "no compaction". Segments and their
//! TOMBSTONES accumulated for the lifetime of the process, and every
//! O(segments) path — `merge_span`, every prefix walk, every adjacency scan —
//! got monotonically slower with uptime. A create/delete workload therefore
//! decayed with nothing in the schedule able to stop it.
//!
//! Worse, the tiering loop could not even refuse safely: a paged segment is
//! sized `usize::MAX`, but `young_total.saturating_mul(2)` saturates too, so
//! on an all-paged store the comparison `sizes[j] > young_total * 2` is false
//! for every segment and the loop selects ALL of them — then tries to build one
//! resident segment out of the entire corpus.
//!
//! # How it streams
//!
//! Segments are sorted, so the merge is done in KEY-RANGE CHUNKS. Chunk
//! boundaries come from the segments' own sparse indexes (their block first
//! keys), so a chunk is a bounded number of blocks per segment — peak memory is
//! O(segments x chunk blocks), not O(corpus). Each chunk is merged, passed
//! through the SAME retention rule the resident compactor uses
//! (`Store::retain_chain`), and streamed straight into a [`SegmentWriter`].
//!
//! # Full compactions only, deliberately
//!
//! This merges EVERY segment. That keeps the retention argument simple — with
//! nothing left untouched, `shadowing_base` is always false, so a tombstone at
//! or below the watermark is always safe to purge — and it is the shape a later
//! CSR-emitting compaction needs anyway, since a partial merge could only ever
//! emit a partial derived structure.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::segment::SealedSegment;
use crate::sst::SegmentWriter;
use crate::{LogicalKey, Sealed, Store, Version};

/// Something that wants to see the merge's OUTPUT as it is produced.
///
/// # Why the store offers this at all
///
/// The graph maintains derived structures — an adjacency CSR and label
/// membership — by SCANNING the spans they derive from: `build_adj_table`
/// walks the whole adjacency span, ~32 tables, 17.26M rows at official SF1.
/// That rebuild is what the derived-refresh pass costs, and it is why every
/// headline write number so far was measured with that pass disabled.
///
/// Compaction already walks every key in the merged run, in key order. And the
/// adjacency span's key order — `tag | node | type | peer | rel` — IS a CSR:
/// `build_adj_table` does nothing but walk it in order and pack `offsets` and
/// `entries`. So the structure can be a BY-PRODUCT of work that must happen
/// anyway, at O(merged run) rather than O(corpus).
///
/// # The store stays semantics-free
///
/// This trait carries no notion of adjacency, membership or labels. The
/// observer declares which byte prefixes it wants offered and interprets them
/// itself; the store only knows how to compare bytes. That is the same line
/// `Store` holds everywhere else, and it is why the CSR logic lives in
/// `engram-graph` where the key layout is defined.
pub trait MergeObserver: Send {
    /// Key prefixes this observer wants offered. A key is offered if it starts
    /// with any of them; an empty list means every key.
    fn key_prefixes(&self) -> Vec<Vec<u8>>;

    /// One key the merge KEPT, in ascending key order, and whether its newest
    /// surviving version is LIVE (a put) rather than a tombstone.
    ///
    /// Deliberately narrow. A derived structure over a key-encoded span — the
    /// adjacency CSR, label membership — needs the KEY (which carries
    /// `node | type | peer | rel`, or `label | node`) and whether the row is
    /// still there. It does not need the record bytes, and handing them over
    /// would both leak `Version` out of this crate and invite an observer to
    /// depend on a representation the store is free to change.
    ///
    /// Only kept keys are offered: a key whose every version was retired is not
    /// part of the compacted store, so a structure built from this sees exactly
    /// what a reader of the new segment would.
    fn visit(&mut self, key: &LogicalKey, live: bool);

    /// The merge finished, and produced a segment covering everything at or
    /// below `stamp`.
    ///
    /// `stamp` is the greatest commit timestamp in the merged run. By the seal
    /// fence (see `Store::get_at`) the tail is strictly newer than every
    /// segment, and segment N+1 strictly newer than N — so everything at or
    /// below `stamp` is inside this merge, and anything above it is in a later
    /// segment or the tail, both of which post-date the sealed set this merge
    /// loaded.
    fn finish(&mut self, stamp: u64);
}

/// How many key-range chunks' worth of block boundaries to take at a time.
///
/// One boundary per chunk would be maximally streaming and maximally slow (a
/// pass per block); the whole key space in one chunk is `Store::compact` again.
/// 64 keeps peak memory at roughly `segments x 64 blocks` while amortising the
/// per-chunk setup.
const BOUNDARIES_PER_CHUNK: usize = 64;

impl Store {
    /// Compact every sealed segment into ONE new segment FILE under `dir`,
    /// streaming, and swap the sealed set to read it through `cache`.
    ///
    /// Returns `(retired_versions, dropped_keys)` — the same accounting
    /// [`Store::compact`] returns, so the two are comparable.
    ///
    /// Online, on the same terms as the resident compactor: segments are
    /// immutable `Arc`s so the merge needs no latch; writes land in the tail,
    /// which this never touches; and a seal during the merge only APPENDS a
    /// segment, which is carried over at the swap.
    pub fn compact_paged_to_dir(
        &self,
        dir: &std::path::Path,
        cache: &Arc<crate::paged::BlockCache>,
    ) -> std::io::Result<(u64, u64)> {
        self.compact_paged_observed(dir, cache, None)
    }

    /// [`Store::compact_paged_to_dir`], offering the merge's output to an
    /// observer so a derived structure can be built as a BY-PRODUCT.
    pub fn compact_paged_observed(
        &self,
        dir: &std::path::Path,
        cache: &Arc<crate::paged::BlockCache>,
        mut observer: Option<&mut dyn MergeObserver>,
    ) -> std::io::Result<(u64, u64)> {
        let _one = self
            .inner
            .compacting
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let (watermark, seq, old) = {
            let watermark = self.gc_watermark();
            // Taken together under the swap latch, for the reason the resident
            // compactor documents: a seal landing between them would take a
            // LOWER seq than the merged run that ends up before it, and a
            // durable reopen (which orders by seq) would let the older merged
            // run shadow the newer seal.
            let _swap = self
                .inner
                .sealed_swap
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let old = self.inner.sealed.load_full();
            if old.segments.len() < 2 {
                // Nothing to merge. Not an error — a store with one segment is
                // already compacted, and saying so by doing nothing is cheaper
                // than rewriting it to itself.
                return Ok((0, 0));
            }
            let seq = self
                .inner
                .next_segment_seq
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            (watermark, seq, old)
        };

        // ── Resident segments, dissolved ONCE ──────────────────────────────
        //
        // A paged store's resident set is bounded by construction: the spill
        // runs on every storage ask, so what is resident is at most the
        // just-sealed tail. Dissolving it up front (oldest first, so chains
        // concatenate ascending) costs that bound in memory and buys a simple
        // range query per chunk, instead of re-walking a segment's row map and
        // its column blocks separately for every chunk.
        let mut resident: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
        for seg in old.segments.iter() {
            if let SealedSegment::Resident(r) = seg.as_ref() {
                for (key, chain) in r.cloned_entries() {
                    resident.entry(key).or_default().extend(chain);
                }
            }
        }

        // ── Chunk boundaries, from the paged segments' sparse indexes ──────
        //
        // Only PAGED segments contribute boundaries: theirs are free (already
        // in the index) and they are the segments whose size makes chunking
        // necessary. The resident map is already in memory and is range-queried
        // per chunk, so it needs no boundaries of its own.
        let mut boundaries: Vec<LogicalKey> = Vec::new();
        for seg in old.segments.iter() {
            if let SealedSegment::Paged(p) = seg.as_ref() {
                boundaries.extend(p.block_first_keys());
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        // The stamp the merged segment covers: the greatest commit timestamp
        // across everything being merged.
        //
        // Sound by the SEAL FENCE (`Store::get_at`'s doc): the tail is strictly
        // newer than every segment and segment N+1 strictly newer than N. So
        // every version at or below this is inside the merge, and anything
        // above it lives in a later segment or the tail — both of which
        // post-date the sealed set loaded above, and both of which a reader
        // catches up from its change log.
        let stamp = old
            .segments
            .iter()
            .map(|seg| match seg.as_ref() {
                SealedSegment::Resident(r) => r.max_commit_ts(),
                SealedSegment::Paged(p) => p.max_commit_ts(),
            })
            .max()
            .unwrap_or(0);

        let prefixes: Vec<Vec<u8>> = observer
            .as_deref_mut()
            .map(|o| o.key_prefixes())
            .unwrap_or_default();

        let mut writer = SegmentWriter::new();
        let mut retired = 0u64;
        let mut dropped_keys = 0u64;

        // Walk the key space in chunks of `BOUNDARIES_PER_CHUNK` boundaries.
        // `None` for the final `hi` means "to the end".
        let mut cut = 0usize;
        loop {
            // `boundaries[cut]`, NOT `boundaries[cut - 1]`.
            //
            // The previous chunk's `hi` was `boundaries[cut]` and `hi` is
            // EXCLUSIVE, so the range that continues seamlessly from it starts
            // at `boundaries[cut]`. Taking `cut - 1` re-covers
            // `[b[cut-1], b[cut])` — a range the previous chunk already merged
            // and pushed — so every key in it is pushed to the `SegmentWriter`
            // a second time.
            //
            // That violates the writer's stated invariant ("Keys MUST arrive in
            // ascending order", sst.rs:283): in a debug build its
            // `debug_assert!` fires, and in the release builds the bench pod
            // runs it silently writes duplicate keys into a segment that is
            // supposed to be sorted and unique, inflating `entry_count`,
            // `versions` and `tombstones` — the last two being what the
            // delete-aware compaction trigger reads.
            //
            // It went unseen because a chunk is 64 block boundaries and a block
            // closes at 16 KiB, so reaching a SECOND chunk takes about a
            // megabyte of segment. The existing suite's 600 two-byte values
            // close a handful of blocks and never advance `cut`.
            // `compact_paged_chunk_boundary.rs` is the fixture that does.
            let lo: Option<LogicalKey> = if cut == 0 {
                None
            } else {
                Some(boundaries[cut].clone())
            };
            let next = (cut + BOUNDARIES_PER_CHUNK).min(boundaries.len());
            let hi: Option<LogicalKey> = if next >= boundaries.len() {
                None
            } else {
                Some(boundaries[next].clone())
            };

            let mut merged: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
            // OLDEST segment first, so chains concatenate in ascending-ts order
            // — the same order the resident compactor builds them in, and the
            // order every reader assumes.
            let lo_b: &[u8] = lo.as_deref().unwrap_or(&[]);
            let hi_b: Option<&[u8]> = hi.as_deref();
            for seg in old.segments.iter() {
                if let SealedSegment::Paged(p) = seg.as_ref() {
                    // The SCAN cache policy: a full-corpus walk must not evict
                    // what live readers are using.
                    p.range_for_each_scan(lo_b, hi_b, |k, versions| {
                        merged
                            .entry(k.clone())
                            .or_default()
                            .extend_from_slice(versions);
                    })
                    .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
                }
            }
            // The resident set is NEWER than every paged segment (it was sealed
            // after them), so its versions concatenate last — the ascending-ts
            // order every reader assumes.
            let upper: Option<&[u8]> = hi_b;
            for (k, chain) in resident.range::<[u8], _>((
                std::ops::Bound::Included(lo_b),
                match upper {
                    Some(h) => std::ops::Bound::Excluded(h),
                    None => std::ops::Bound::Unbounded,
                },
            )) {
                merged.entry(k.clone()).or_default().extend(chain.clone());
            }

            for (key, chain) in merged {
                // FULL compaction: nothing is left untouched, so no older
                // segment can resurface a key once its tombstone is purged.
                let kept = Store::retain_chain(chain, watermark, false, &mut retired);
                if kept.is_empty() {
                    dropped_keys += 1;
                } else {
                    if let Some(o) = observer.as_deref_mut() {
                        if prefixes.is_empty()
                            || prefixes.iter().any(|p| key.starts_with(p))
                        {
                            // The NEWEST surviving version decides liveness —
                            // chains are stored ascending, so that is the last.
                            let live = kept.last().is_some_and(|v| v.value.is_some());
                            o.visit(&key, live);
                        }
                    }
                    writer.push(&key, &kept);
                }
            }

            if next >= boundaries.len() {
                break;
            }
            cut = next;
        }

        // ── Publish ────────────────────────────────────────────────────────
        let bytes = writer.finish(seq);
        let path = dir.join(format!("seg-{seq:020}.seg"));
        let tmp = path.with_extension("segtmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        let paged = SealedSegment::open_paged(&path, Arc::clone(cache))
            .map_err(|e| std::io::Error::other(format!("{e}")))?;

        let _swap = self
            .inner
            .sealed_swap
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = self.inner.sealed.load_full();
        // Carry over anything sealed DURING the merge. Its seq is above ours,
        // so it belongs after the merged run and the ordering invariant holds.
        let carried: Vec<Arc<SealedSegment>> = now
            .segments
            .iter()
            .filter(|s| s.seq() > old.segments.last().map_or(0, |l| l.seq()))
            .cloned()
            .collect();
        let mut new_segments: Vec<Arc<SealedSegment>> = vec![Arc::new(paged)];
        new_segments.extend(carried);
        self.inner
            .sealed
            .store(Arc::new(Sealed {
                segments: new_segments,
            }));
        // The observer is told LAST, after the new segment is published. A
        // derived structure stamped with this must not become visible before
        // the segment it describes — the reverse order would let a reader adopt
        // a structure covering rows the store had not yet swapped in.
        if let Some(o) = observer {
            o.finish(stamp);
        }
        // ── Retire the inputs' FILES ────────────────────────────────────────
        // The swap dropped the merged segments from the sealed set; their files
        // stayed on disk, so every compaction left a whole generation behind —
        // 1,010 files for 50 live segments forty minutes into an SF3 load, the
        // SF1 bulk store at 7x its live size, and SF3 disk-unbounded (the load
        // was hours from ENOSPC when this was found). Only the INPUTS go: the
        // merged output is `path`, and anything sealed during the merge was
        // carried over above, not merged. A reader still holding an input keeps
        // its inode (unlink semantics); a platform that refuses to unlink an
        // open file leaves the file, which the next compaction cannot see and
        // is the one leak this does not close — best effort, counted either way.
        // A segment sealed but not yet spilled has no file and is skipped.
        for s in old.segments.iter() {
            if !matches!(**s, SealedSegment::Paged(_)) {
                continue;
            }
            // Two naming forms exist for a spilled segment (zero-padded from
            // the spill and the compactor, bare from the rebind path).
            for name in [
                format!("seg-{:020}.seg", s.seq()),
                format!("seg-{}.seg", s.seq()),
            ] {
                let p = dir.join(name);
                if p == path || !p.exists() {
                    continue;
                }
                if std::fs::remove_file(&p).is_ok() {
                    engram_observe::counted!("store.paged compaction unlinked an input segment");
                } else {
                    engram_observe::counted!("store.paged compaction could not unlink an input segment");
                }
            }
        }
        engram_observe::counted!("store.paged compactions");
        engram_observe::sometimes!("store.compacted the paged set into one file", true);
        Ok((retired, dropped_keys))
    }
}
