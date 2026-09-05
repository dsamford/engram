//! The mutable tail, sharded — the write side of fine-grained latching.
//!
//! The tail held every not-yet-sealed version in one `BTreeMap` behind one
//! `RwLock`, and every write held that lock across timestamp allocation, the
//! chain-hashed log append and the map insert, while every read of a
//! non-empty tail took the same lock. Measured on disjoint keys with no disk
//! in the picture: eight threads did **half** the work of one
//! (`tests/write_scaling_probe.rs`).
//!
//! Here the tail is `TAIL_SHARDS` maps, each behind its own latch, a key's
//! shard chosen by a hash of its prefix and the first bytes of its body. A
//! write takes one shard's write latch for one map insert — microseconds
//! shorter than the section it replaces, and disjoint from every other
//! shard's. A point read takes one shard's read latch; a range read takes
//! each shard's in turn (or all at once, when it needs a sorted cursor).
//!
//! What the sharding does NOT change, and the store still guarantees around
//! it: timestamps are allocated in log order under the log's own latch;
//! visibility is a separate watermark advanced in timestamp order
//! ([`Store::now_ts`](crate::Store::now_ts) reads it), so a reader never
//! snapshots at a timestamp whose version has not reached its shard yet; and
//! a seal holds every shard's write latch across the segment publish, so a
//! chain is never absent from both the tail and the sealed set.
//!
//! # Why the first bytes of the body pick the shard
//!
//! A key is `<13-byte encoded prefix><body>`. Hashing only the prefix plus
//! the first [`SHARD_BODY_BYTES`] of the body keeps a NARROW range scan on
//! one shard: an adjacency row is `<tag><node id: 8><…>`, so every row of
//! one node's one side shares those bytes and a hop scan touches one latch;
//! node records are `<id: 8>`, so writes to different nodes spread. A WIDE
//! range (a whole label, a whole partition) spans every shard and is
//! merged — the same cost class as merging the tail with the segments.
//!
//! Single-threaded the result of every operation is identical to the one
//! map's (ranges are re-sorted across shards), so the determinism trace is
//! unchanged.

use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{LogicalKey, Version};

/// Shards in the tail. Power of two; more than the worker count by a margin
/// so that spread writes rarely meet on a latch.
pub(crate) const TAIL_SHARDS: usize = 64;

/// Body bytes (after the 13-byte prefix) that take part in the shard choice.
pub(crate) const SHARD_BODY_BYTES: usize = 9;

/// One shard: a slice of the tail's key space, oldest-first chains per key.
pub(crate) type Shard = BTreeMap<LogicalKey, Vec<Version>>;

pub(crate) struct ShardedTail {
    shards: Vec<RwLock<Shard>>,
    /// Versions per shard, maintained under that shard's write latch — so
    /// the write path touches no counter shared across shards. Summed (a
    /// read of each) by the adapter's seal threshold, once per batch.
    counts: Vec<std::sync::atomic::AtomicUsize>,
}

impl ShardedTail {
    pub(crate) fn new() -> Self {
        ShardedTail {
            shards: (0..TAIL_SHARDS).map(|_| RwLock::new(Shard::new())).collect(),
            counts: (0..TAIL_SHARDS)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        }
    }

    /// Versions in the tail, summed over the shards.
    pub(crate) fn versions(&self) -> usize {
        self.counts
            .iter()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    }

    /// The shard a key lives in. FNV-1a over the prefix and the first
    /// [`SHARD_BODY_BYTES`] of the body — a stable, cheap hash whose choice is
    /// invisible to every caller (results are merged in key order).
    fn shard_of(key: &[u8]) -> usize {
        let n = key.len().min(engram_key::PREFIX_LEN + SHARD_BODY_BYTES);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in &key[..n] {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        (h as usize) % TAIL_SHARDS
    }

    fn read(&self, i: usize) -> RwLockReadGuard<'_, Shard> {
        self.shards[i].read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, i: usize) -> RwLockWriteGuard<'_, Shard> {
        self.shards[i].write().unwrap_or_else(|e| e.into_inner())
    }

    /// Add `v` to `key`'s chain at its timestamp's position. Usually an
    /// append — but NOT by construction: a writer allocates its stamp under
    /// the log latch and publishes here after releasing it, so a second
    /// writer to the same key with a later stamp can arrive first. A plain
    /// append would then leave the chain non-monotone and every reader
    /// answering the older version for ever; the ordered insert costs one
    /// binary search over a chain that is almost always short.
    pub(crate) fn push(&self, key: LogicalKey, v: Version) {
        self.insert_ordered(key, v);
    }

    /// Insert `v` into `key`'s chain at its timestamp's position — for the
    /// replay paths, whose caller may hand versions out of order.
    pub(crate) fn insert_ordered(&self, key: LogicalKey, v: Version) {
        let i = Self::shard_of(&key);
        let mut g = self.write(i);
        let chain = g.entry(key).or_default();
        let pos = chain.partition_point(|x| x.commit_ts <= v.commit_ts);
        chain.insert(pos, v);
        self.counts[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The newest version of `key` at or below `ts` — `Some` even for a
    /// tombstone (value `None`), so a caller can stop resolving.
    pub(crate) fn visible_at(&self, key: &LogicalKey, ts: u64) -> Option<Version> {
        let i = Self::shard_of(key);
        self.read(i)
            .get(key)
            .and_then(|c| c.iter().rev().find(|v| v.commit_ts <= ts))
            .cloned()
    }

    /// The commit timestamp of `key`'s newest version, if any.
    /// The newest version's commit timestamp, whatever kind it is. Used by
    /// this crate's tests; validation wants [`ShardedTail::newest_ts_and_kind`].
    #[cfg(test)]
    pub(crate) fn newest_ts(&self, key: &LogicalKey) -> Option<u64> {
        let i = Self::shard_of(key);
        self.read(i)
            .get(key)
            .and_then(|c| c.last())
            .map(|v| v.commit_ts)
    }

    /// The newest version's timestamp AND whether it is a put (`true`) rather
    /// than a tombstone. OCC validation needs the second half to tell a
    /// relationship touch from a node delete on a guard row.
    pub(crate) fn newest_ts_and_kind(&self, key: &LogicalKey) -> Option<(u64, bool)> {
        let i = Self::shard_of(key);
        self.read(i)
            .get(key)
            .and_then(|c| c.last())
            .map(|v| (v.commit_ts, v.value.is_some()))
    }

    /// Visit every key in `[lo, hi)` with its chain, shard by shard — NOT in
    /// key order across shards. Callers that need order collect and sort.
    pub(crate) fn for_each_in_range(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        mut f: impl FnMut(&LogicalKey, &[Version]),
    ) {
        self.for_each_in_range_until(lo, hi, |k, chain| {
            f(k, chain);
            true
        });
    }

    /// [`ShardedTail::for_each_in_range`] with an EARLY STOP: `f` answers
    /// whether to continue, and the call answers whether the visit ran to
    /// completion (`false` = stopped by the visitor). A budgeted reader uses
    /// this so that a span wider than its budget costs the budget, not the
    /// span.
    pub(crate) fn for_each_in_range_until(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        mut f: impl FnMut(&LogicalKey, &[Version]) -> bool,
    ) -> bool {
        for i in 0..TAIL_SHARDS {
            let g = self.read(i);
            let iter: Box<dyn Iterator<Item = (&LogicalKey, &Vec<Version>)>> = match hi {
                Some(h) => Box::new(g.range(lo.to_vec()..h.to_vec())),
                None => Box::new(g.range(lo.to_vec()..)),
            };
            for (k, chain) in iter {
                if !f(k, chain) {
                    return false;
                }
            }
        }
        true
    }

    /// Every shard's read latch, in index order — for a merge that needs a
    /// SORTED cursor over borrowed chains. Held for the merge's duration;
    /// writers to any shard wait, as they did on the one latch, but only for
    /// the merge and only if they land on a shard it holds.
    pub(crate) fn read_all(&self) -> AllRead<'_> {
        AllRead {
            guards: (0..TAIL_SHARDS).map(|i| self.read(i)).collect(),
        }
    }

    /// Every shard's write latch, in index order — the seal's barrier. While
    /// this lives no version can enter or leave the tail.
    pub(crate) fn write_all(&self) -> AllWrite<'_> {
        AllWrite {
            guards: (0..TAIL_SHARDS).map(|i| self.write(i)).collect(),
            counts: &self.counts,
        }
    }
}

impl ShardedTail {
    /// The chains in `[lo, hi)`, COPIED OUT one shard at a time, sorted by key
    /// — or `None` if the range holds more than `cap` rows.
    ///
    /// # Why this exists
    ///
    /// [`Tail::read_all`] takes all `TAIL_SHARDS` read latches at once and the
    /// caller holds them for the whole merge — across the paged segments'
    /// `pread`s, their BLAKE3 verification, and every visitor callback. A
    /// writer needs exactly one of those shards, so for that entire window no
    /// write can enter the tail: **every span read mutually excludes every
    /// writer**.
    ///
    /// Measured on the bench pod at official LDBC SF1, counting span reads that
    /// took the latches, with the two PURE profiles as the control:
    ///
    /// | profile | span reads excluding writers | rows merged under them |
    /// |---|---|---|
    /// | read-only | 0 | 0 |
    /// | write-only | 0 | 0 |
    /// | balanced | 11,681 | 6,024,535 |
    /// | write-heavy | 35,723 | 18,092,001 |
    ///
    /// Zero on both pure profiles and tens of thousands on both mixed ones,
    /// which is exactly where engram trails Neo4j 5.26 (0.63x-0.75x) while
    /// leading it everywhere else (1.49x-3.98x).
    ///
    /// This copies instead: one shard's latch is held for a `BTreeMap` range
    /// descent and released before the next is taken, so a writer waits for one
    /// descent rather than for a whole merge plus its disk I/O.
    ///
    /// # The cap, and why it returns `None` rather than truncating
    ///
    /// Copying is O(rows) in memory, and a full-span walk over SF1's adjacency
    /// is 17.26M rows. Past `cap` this declines and the caller keeps the
    /// borrow-everything path — slower for writers, but bounded in memory,
    /// which is the trade a bigger-than-RAM store has to make. Truncating would
    /// silently answer short.
    pub(crate) fn range_copied(
        &self,
        lo: &[u8],
        hi: Option<&[u8]>,
        cap: usize,
    ) -> Option<Vec<(LogicalKey, Vec<Version>)>> {
        let mut out: Vec<(LogicalKey, Vec<Version>)> = Vec::new();
        for i in 0..TAIL_SHARDS {
            // ONE shard's latch, for a range descent and a clone of what it
            // finds. Dropped at the end of this block, before the next is
            // taken — that release is the whole point of the function.
            let g = self.read(i);
            match hi {
                Some(h) => {
                    for (k, v) in g.range(lo.to_vec()..h.to_vec()) {
                        if out.len() >= cap {
                            return None;
                        }
                        out.push((k.clone(), v.clone()));
                    }
                }
                None => {
                    for (k, v) in g.range(lo.to_vec()..) {
                        if out.len() >= cap {
                            return None;
                        }
                        out.push((k.clone(), v.clone()));
                    }
                }
            }
        }
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Some(out)
    }
}

/// Every shard read-latched at once.
pub(crate) struct AllRead<'a> {
    guards: Vec<RwLockReadGuard<'a, Shard>>,
}

impl AllRead<'_> {
    /// The chains in `[lo, hi)` across every shard, SORTED by key, borrowed
    /// from the held latches.
    pub(crate) fn range_sorted(&self, lo: &[u8], hi: Option<&[u8]>) -> Vec<(&LogicalKey, &Vec<Version>)> {
        let mut out: Vec<(&LogicalKey, &Vec<Version>)> = Vec::new();
        for g in &self.guards {
            match hi {
                Some(h) => out.extend(g.range(lo.to_vec()..h.to_vec())),
                None => out.extend(g.range(lo.to_vec()..)),
            }
        }
        out.sort_unstable_by(|a, b| a.0.cmp(b.0));
        out
    }
}

/// Every shard write-latched at once.
pub(crate) struct AllWrite<'a> {
    guards: Vec<RwLockWriteGuard<'a, Shard>>,
    counts: &'a [std::sync::atomic::AtomicUsize],
}

impl AllWrite<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.guards.iter().all(|g| g.is_empty())
    }

    /// Take every chain out of the tail, merged into one ordered map.
    pub(crate) fn drain(&mut self) -> Shard {
        let mut all = Shard::new();
        for (i, g) in self.guards.iter_mut().enumerate() {
            let taken = std::mem::take(&mut **g);
            all.extend(taken);
            self.counts[i].store(0, std::sync::atomic::Ordering::Relaxed);
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(prefix_byte: u8, body: &[u8]) -> LogicalKey {
        let mut k = vec![prefix_byte; engram_key::PREFIX_LEN];
        k.extend_from_slice(body);
        k
    }

    fn v(ts: u64) -> Version {
        Version {
            commit_ts: ts,
            value: Some(std::sync::Arc::from(&[ts as u8][..])),
            sealed: false,
        }
    }

    #[test]
    fn a_narrow_range_lands_on_one_shard_and_a_wide_range_is_merged_sorted() {
        let t = ShardedTail::new();
        // One node's adjacency rows: same first 9 body bytes → one shard.
        let node = 7u64.to_be_bytes();
        let mut rows = Vec::new();
        for peer in 0..20u64 {
            let mut body = vec![b'O'];
            body.extend_from_slice(&node);
            body.extend_from_slice(&peer.to_be_bytes());
            rows.push(key(1, &body));
        }
        let shards: std::collections::BTreeSet<usize> =
            rows.iter().map(|k| ShardedTail::shard_of(k)).collect();
        assert_eq!(shards.len(), 1, "one node's rows share a shard");
        for (i, k) in rows.iter().enumerate() {
            t.push(k.clone(), v(i as u64 + 1));
        }
        // Wide range: many distinct nodes → many shards, merged in key order.
        for id in 0..500u64 {
            t.push(key(2, &id.to_be_bytes()), v(1000 + id));
        }
        let all = t.read_all();
        let got = all.range_sorted(&[2u8; engram_key::PREFIX_LEN], None);
        assert_eq!(got.len(), 500);
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0), "sorted across shards");
        drop(all);
        assert_eq!(t.newest_ts(&rows[3]), Some(4));
        assert_eq!(t.visible_at(&rows[3], 3), None, "nothing at or below 3 for a version stamped 4");
    }

    #[test]
    fn insert_ordered_keeps_chains_ascending_and_a_seal_drains_every_shard() {
        let t = ShardedTail::new();
        let k = key(3, b"x");
        t.insert_ordered(k.clone(), v(5));
        t.insert_ordered(k.clone(), v(2));
        t.insert_ordered(k.clone(), v(9));
        let chain: Vec<u64> = {
            let all = t.read_all();
            all.range_sorted(&k, None)[0].1.iter().map(|x| x.commit_ts).collect()
        };
        assert_eq!(chain, vec![2, 5, 9]);
        assert_eq!(t.newest_ts(&k), Some(9));
        assert_eq!(t.visible_at(&k, 6).map(|x| x.commit_ts), Some(5));
        let mut w = t.write_all();
        assert!(!w.is_empty());
        let drained = w.drain();
        assert_eq!(drained.len(), 1);
        assert!(w.is_empty());
    }
}
