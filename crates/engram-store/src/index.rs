//! L7's first index: a derived, rebuildable range index with MVCC visibility.
//!
//! # Why the index is a SEGMENT and not keyspace rows
//!
//! The keyspace hygiene rule is absolute: a user property value never appears
//! in a sort-ordered key position, because an LSM sorts by key and a plaintext
//! value in a key IS order-preserving encryption. A range index sorts by the
//! value — so it cannot live in the primary keyspace at all. It lives here, as
//! a derived structure the primary data can always rebuild, which is also what
//! makes "drop and rebuild" the whole repair story. FC-11 lands as a
//! consequence: the entry's value payload exists only inside the derived
//! segment, and nothing here can migrate it into a key.
//!
//! # MVCC visibility
//!
//! An index is built AT a timestamp, from `scan_at` — it sees exactly what a
//! snapshot reader at that ts sees: committed values, no tombstones, no
//! future. Every query answer carries `as_of`, because an index that does not
//! say its vintage gets read as current — the staleness equivalent of a
//! percentile without its sample size.
//!
//! # Ordering is TYPED, not byte-wise
//!
//! Values in the store are little-endian tagged payloads; comparing those
//! bytes misorders every negative integer and every float. The comparator
//! decodes and compares by type. Cross-type entries in one property (legal —
//! schema is not enforced per row) order by tag byte first, then value:
//! arbitrary but STABLE, and documented as such rather than implied natural.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use engram_key::KeyPrefix;
use engram_key::value::Tag;
use engram_observe::{counted, sometimes};

use crate::Store;
use crate::record::{PropertyId, Record, get_property};

/// An index definition — FC-9's open-ended record.
///
/// Stored AS a [`Record`], which is what makes it open-ended for real: a
/// newer build's extra definition fields ride through an older build's
/// read-modify-write untouched, because the record layer already preserves
/// what it cannot decode. The known fields are properties of the record, not
/// a fixed struct layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// The definition record. Known fields below; unknown fields preserved.
    record: Record,
}

/// Known definition fields, as property ids inside the definition record.
pub mod def_fields {
    use crate::record::PropertyId;
    /// u32 index id (INT64-tagged).
    pub const INDEX_ID: PropertyId = PropertyId(1);
    /// The indexed property (INT64-tagged id).
    pub const PROPERTY: PropertyId = PropertyId(2);
}

impl IndexDef {
    /// Define an index over `property`.
    pub fn new(index_id: u32, property: PropertyId) -> Self {
        let mut record = Record::new();
        record.set(def_fields::INDEX_ID, int64_value(i64::from(index_id)));
        record.set(def_fields::PROPERTY, int64_value(i64::from(property.0)));
        IndexDef { record }
    }

    /// The definition as its record — for persistence under `Kind::INDEX_DEF`.
    pub fn as_record(&self) -> &Record {
        &self.record
    }

    /// Rehydrate from a stored record, keeping every unknown field.
    pub fn from_record(record: Record) -> Option<Self> {
        // The known fields must decode; everything else rides along.
        let _ = decode_int64(record.get(def_fields::INDEX_ID)?)?;
        let _ = decode_int64(record.get(def_fields::PROPERTY)?)?;
        Some(IndexDef { record })
    }

    /// The index id.
    pub fn index_id(&self) -> u32 {
        decode_int64(self.record.get(def_fields::INDEX_ID).expect("validated")).expect("validated")
            as u32
    }

    /// The indexed property.
    pub fn property(&self) -> PropertyId {
        PropertyId(
            decode_int64(self.record.get(def_fields::PROPERTY).expect("validated"))
                .expect("validated") as u32,
        )
    }
}

fn int64_value(v: i64) -> Vec<u8> {
    let mut out = vec![Tag::INT64.byte()];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

fn decode_int64(tagged: &[u8]) -> Option<i64> {
    if tagged.first() != Some(&Tag::INT64.byte()) || tagged.len() != 9 {
        return None;
    }
    Some(i64::from_le_bytes(tagged[1..9].try_into().ok()?))
}

/// A key an index entry sorts by: the TYPED interpretation of a tagged value.
///
/// The derive order (tag class first, then the typed value) is the documented
/// cross-type order. Byte-comparing the little-endian payloads instead would
/// misorder every negative integer — the canary for exactly that mistake is
/// in the test suite.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexKey {
    /// INT64.
    Int(i64),
    /// FLOAT64, ordered by `total_cmp` so NaN has a stable place instead of
    /// poisoning the sort.
    Float(f64),
    /// STRING, ordered bytewise over UTF-8 (code-point order).
    Str(Vec<u8>),
}

impl Eq for IndexKey {}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use IndexKey::*;
        match (self, other) {
            (Int(a), Int(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.total_cmp(b),
            (Str(a), Str(b)) => a.cmp(b),
            // Cross-type: tag-class order, stable and documented as arbitrary.
            (Int(_), _) => std::cmp::Ordering::Less,
            (_, Int(_)) => std::cmp::Ordering::Greater,
            (Float(_), _) => std::cmp::Ordering::Less,
            (_, Float(_)) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl IndexKey {
    /// Interpret a tagged value as an index key. `None` for types this index
    /// does not order (which the build COUNTS rather than skips silently).
    pub fn from_tagged(tagged: &[u8]) -> Option<IndexKey> {
        let tag = Tag::from_byte(*tagged.first()?);
        match tag {
            t if t == Tag::INT64 => Some(IndexKey::Int(i64::from_le_bytes(
                tagged.get(1..9)?.try_into().ok()?,
            ))),
            t if t == Tag::FLOAT64 => Some(IndexKey::Float(f64::from_le_bytes(
                tagged.get(1..9)?.try_into().ok()?,
            ))),
            t if t == Tag::STRING => {
                let len = u32::from_le_bytes(tagged.get(1..5)?.try_into().ok()?) as usize;
                Some(IndexKey::Str(tagged.get(5..5 + len)?.to_vec()))
            }
            _ => None,
        }
    }
}

/// The derived range index.
#[derive(Debug, Clone)]
pub struct RangeIndex {
    def: IndexDef,
    /// Sorted `(key, entity body)` pairs — the immutable BASE. The value
    /// payload lives HERE, in the derived segment — FC-11's "never migrates
    /// into the key" is structural, because there is no key for it to migrate
    /// into.
    ///
    /// Behind an `Arc` so that carrying the index forward over a write shares
    /// the base instead of copying it. That is the difference between a write
    /// costing O(rows-in-the-index) and O(rows-that-changed): copying a 100k
    /// entry base per write measured 3.5x more per write against a 4x larger
    /// corpus — linear, and the reason a first version of incremental
    /// maintenance still could not hold up a mixed workload.
    entries: Arc<Vec<(IndexKey, Vec<u8>)>>,
    /// Sorted `(key, body)` pairs added since the base was last folded. Small
    /// by construction — [`RangeIndex::FOLD_AT`] bounds it.
    added: Vec<(IndexKey, Vec<u8>)>,
    /// Bodies whose BASE entry no longer applies: deleted, or moved to a new
    /// key (in which case the new pair is in `added`). Every read subtracts
    /// this from the base.
    ///
    /// Behind an `Arc` for the same reason `entries` is, and it took a second
    /// workload to notice: `with_changes` used to CLONE this set on every
    /// catch-up. Bounded by `FOLD_AT` that is up to 4,096 `Vec<u8>` clones per
    /// call, so the per-write cost sawtoothed from ~0 up to 4,096 and back —
    /// a cost that GROWS with ops executed, which is exactly the decay shape
    /// the multi-key seek was added to remove. Under a create/delete churn the
    /// index is probed every operation, so that path went from once-a-level to
    /// once-an-op and the sawtooth became the profile.
    removed: Arc<BTreeSet<Vec<u8>>>,
    /// Removals not yet merged into `removed`, kept SORTED and small.
    ///
    /// A catch-up appends here and clones only this (<= `RECENT_CAP`), paying
    /// the O(|removed|) merge once per `RECENT_CAP` changes instead of once per
    /// change. Reads test both, so the answer is unchanged.
    removed_recent: Vec<Vec<u8>>,
    /// The snapshot this index describes.
    as_of: u64,
    /// Rows whose indexed property carried a tag the index cannot order.
    /// Counted, never silently skipped: an index that quietly ignores a type
    /// reports "no matches" in the same words as one that indexed it.
    unindexable: u64,
}

/// A range answer, carrying its vintage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeAnswer {
    /// Entity bodies whose indexed value falls in the range, in key order.
    pub bodies: Vec<Vec<u8>>,
    /// The snapshot the answer describes. An answer without its vintage gets
    /// read as current.
    pub as_of: u64,
    /// Rows the index could not order at build time. Non-zero means the
    /// answer is a FLOOR over typed rows, not a census of the group.
    pub unindexable: u64,
}

impl RangeIndex {
    /// Build the index from the group at snapshot `ts`.
    ///
    /// Reads through `scan_at`, so the index sees exactly what a snapshot
    /// reader sees: committed values only, tombstones excluded, nothing newer
    /// than `ts`. Rebuilding at the same ts over the same store yields an
    /// identical index — the property that makes "drop and rebuild" a repair
    /// rather than a gamble, pinned by a determinism test.
    pub fn build(store: &Store, group: &KeyPrefix, def: IndexDef, ts: u64) -> RangeIndex {
        let mut entries = Vec::new();
        let mut unindexable = 0u64;
        // STREAMED, not materialised: `scan_at` resolves the whole partition
        // into an owned map before the first row is examined — on the paged
        // production mirror that is every node record (gigabytes) per build,
        // and a first-use build of a partition-wide index for an undeclared
        // key put a 40Gi pod at 25 GiB under shadow reads. The k-way merge
        // hands each row over once, in key order, and keeps nothing.
        store.for_each_span(group, &[], ts, &mut |body, record_bytes| {
            if let Some(tagged) = get_property(record_bytes, def.property()) {
                match IndexKey::from_tagged(&tagged) {
                    Some(key) => entries.push((key, body.to_vec())),
                    None => {
                        unindexable += 1;
                        sometimes!("index.row not orderable by this index", true);
                    }
                }
            }
            true
        });
        entries.sort();
        counted!("index.builds");
        crate::INDEX_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        RangeIndex {
            def,
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            as_of: ts,
            unindexable,
        }
    }

    /// Build the index over ONLY the given entity bodies — a LABEL-SCOPED
    /// index, where `build` above indexes the whole partition.
    ///
    /// `CREATE INDEX ... FOR (n:Churn) ON (n.id)` names a LABEL, and Cypher
    /// means it: the index covers that label's nodes. `build` ignores the label
    /// and indexes every node in the partition carrying the property, so on a
    /// corpus where `id` is a shared property — official SF1 has 3.18M nodes
    /// with one — an index the operator scoped to a few hundred nodes cost
    /// millions of entries to build, to fold, and to hold.
    ///
    /// `bodies` must be the label's members, ascending. Rows without the
    /// property are simply absent, exactly as in `build`.
    pub fn build_over(
        store: &Store,
        group: &KeyPrefix,
        def: IndexDef,
        ts: u64,
        bodies: impl IntoIterator<Item = Vec<u8>>,
    ) -> RangeIndex {
        let mut entries = Vec::new();
        let mut unindexable = 0u64;
        for body in bodies {
            let Some(record_bytes) = store.get_at(group, &body, ts) else {
                continue; // not visible at this snapshot
            };
            let Some(tagged) = get_property(&record_bytes, def.property()) else {
                continue;
            };
            match IndexKey::from_tagged(&tagged) {
                Some(key) => entries.push((key, body)),
                None => {
                    unindexable += 1;
                    sometimes!("index.row not orderable by this index", true);
                }
            }
        }
        entries.sort();
        counted!("index.builds");
        crate::INDEX_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counted!("index.label-scoped builds");
        RangeIndex {
            def,
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            as_of: ts,
            unindexable,
        }
    }

    /// This index over ONLY the bodies `keep` admits — the LABEL-SCOPED view
    /// of a partition-wide index, derived WITHOUT a store read: the live
    /// entries (overlay resolved) filtered by membership, in the same order,
    /// at the same vintage. A persisted index-at-seal covers the whole
    /// partition (`build`), and a scoped slot that took it as-is answered a
    /// common value with every label's rows: `{status: "pending"}` on a
    /// 701-node label walked the partition's thousands of `pending` entries
    /// per probe (4.8 ms against a 0.4 ms probe for a value nobody carries).
    /// `unindexable` is carried as the partition's count — a floor stays a
    /// floor.
    pub fn restricted_to(&self, keep: &mut dyn FnMut(&[u8]) -> bool) -> RangeIndex {
        let mut entries: Vec<(IndexKey, Vec<u8>)> = Vec::new();
        for (k, b) in self.entries.iter() {
            if !self.is_removed(b) && keep(b) {
                entries.push((k.clone(), b.clone()));
            }
        }
        for (k, b) in &self.added {
            if keep(b) {
                entries.push((k.clone(), b.clone()));
            }
        }
        entries.sort();
        counted!("index.restricted to a label");
        RangeIndex {
            def: self.def.clone(),
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            as_of: self.as_of,
            unindexable: self.unindexable,
        }
    }

    /// Carry this index forward to `ts` by applying the entity bodies whose
    /// indexed property CHANGED, instead of rebuilding it from the store.
    ///
    /// `changes` maps an entity body to its new index key — `None` when the row
    /// no longer belongs in the index (the property was removed, the entity was
    /// deleted, or the new value is a type this index cannot order).
    ///
    /// # Why this exists
    ///
    /// [`build`](RangeIndex::build) re-scans the whole partition and re-sorts.
    /// `ensure_range_index` called it whenever the indexed property had been
    /// written since the cached index was built — which is to say, after every
    /// write. Against a 100k-node LDBC SNB corpus over Bolt, adding **5%**
    /// writes to a read workload cost 14x the throughput (620 -> 43 ops/s), and
    /// the per-shape latency was the giveaway: an indexed point lookup held a
    /// p50 of 0.15 ms and grew a p95 of 107 ms. The reads that followed a write
    /// were each paying a full corpus rescan.
    ///
    /// An index that must be rebuilt after every write is not an index.
    ///
    /// # The result is identical to a rebuild
    ///
    /// That is the contract, and it is what makes this a safe substitution
    /// rather than a faster approximation: the entries stay sorted by
    /// `(key, body)` exactly as `build`'s `entries.sort()` leaves them, and
    /// `crates/engram-store/tests/index_incremental.rs` asserts equality
    /// against a fresh `build` at the same `ts` over the same store.
    ///
    /// # When it declines
    ///
    /// `None` — meaning "rebuild instead" — when any changed row carried a
    /// value this index cannot order. Such a row contributes to `unindexable`,
    /// a count this method cannot maintain without knowing what the row held
    /// BEFORE the change, and a wrong `unindexable` silently converts a floor
    /// into a census. Rare, and cheaper to decline than to track.
    pub fn with_changes(
        &self,
        changes: &BTreeMap<Vec<u8>, Option<IndexKey>>,
        ts: u64,
    ) -> Option<RangeIndex> {
        if self.unindexable > 0 {
            // The count cannot be maintained across a change to an unorderable
            // row, and we cannot tell whether one of these bodies is such a row.
            return None;
        }
        // The changed bodies leave the base (and any earlier overlay entry).
        //
        // The shared set is carried by REFCOUNT and the new removals land in a
        // small sorted bucket beside it; the O(|removed|) merge happens once
        // per `RECENT_CAP` changes rather than once per change. See the field
        // docs for the sawtooth this replaced.
        let mut removed = Arc::clone(&self.removed);
        let mut recent = self.removed_recent.clone();
        let mut added: Vec<(IndexKey, Vec<u8>)> = self
            .added
            .iter()
            .filter(|(_, body)| !changes.contains_key(body))
            .cloned()
            .collect();
        for (body, key) in changes {
            if !removed.contains(body) {
                if let Err(pos) = recent.binary_search(body) {
                    recent.insert(pos, body.clone());
                }
            }
            if let Some(k) = key {
                added.push((k.clone(), body.clone()));
            }
        }
        if recent.len() > Self::RECENT_CAP {
            // Amortised: fold the bucket into the shared set. This is the only
            // place the O(|removed|) copy happens.
            let mut merged = (*removed).clone();
            merged.extend(recent.drain(..));
            removed = Arc::new(merged);
            counted!("index.overlay removal buckets merged");
        }
        // Sorted on the `(key, body)` TUPLE, the order `build` leaves the base
        // in, so the merge at read time reproduces a rebuild exactly.
        added.sort();
        counted!("index.incremental updates");
        crate::INDEX_CATCHUPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let next = RangeIndex {
            def: self.def.clone(),
            entries: Arc::clone(&self.entries),
            added,
            removed,
            removed_recent: recent,
            as_of: ts,
            unindexable: 0,
        };
        // FOLD when the overlay has grown enough to start costing reads. Every
        // read merges two runs and tests each base entry against `removed`, so
        // an unbounded overlay would trade the write cost straight back into
        // read latency. Folding is the O(base) pass this method exists to
        // avoid doing per write — amortised over `FOLD_AT` writes, it is
        // O(base / FOLD_AT) each, which is what makes a write O(1) in practice.
        if next.added.len() + next.removed_len() > Self::FOLD_AT {
            return Some(next.folded());
        }
        Some(next)
    }

    /// Past this much pending overlay, collapse it into a fresh base.
    const FOLD_AT: usize = 4_096;

    /// Removals held in the small sorted bucket before being merged into the
    /// shared set. Small enough that cloning it per catch-up is noise; large
    /// enough that the O(|removed|) merge is rare.
    const RECENT_CAP: usize = 256;

    /// Whether this body's BASE entry has been withdrawn — by either half of
    /// the removal overlay. The one place the split is resolved, so a reader
    /// cannot consult one half and miss the other.
    fn is_removed(&self, body: &[u8]) -> bool {
        self.removed.contains(body)
            || self
                .removed_recent
                .binary_search_by(|x| x.as_slice().cmp(body))
                .is_ok()
    }

    /// Total withdrawn bodies across both halves.
    fn removed_len(&self) -> usize {
        self.removed.len() + self.removed_recent.len()
    }

    /// Whether anything is withdrawn at all.
    fn removed_is_empty(&self) -> bool {
        self.removed.is_empty() && self.removed_recent.is_empty()
    }

    /// Collapse the overlay into a new base. The result answers identically —
    /// it is the same set of live entries in the same order, with nothing
    /// pending.
    fn folded(&self) -> RangeIndex {
        let mut entries: Vec<(IndexKey, Vec<u8>)> =
            Vec::with_capacity(self.entries.len() + self.added.len());
        // Merge the two sorted runs once, base filtered by `removed`.
        let mut base = self
            .entries
            .iter()
            .filter(|(_, b)| !self.is_removed(b))
            .peekable();
        let mut add = self.added.iter().peekable();
        loop {
            let take_add = match (base.peek(), add.peek()) {
                (Some(b), Some(a)) => a < b,
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) => break,
            };
            let e = if take_add {
                add.next().expect("peeked")
            } else {
                base.next().expect("peeked")
            };
            entries.push(e.clone());
        }
        counted!("index.overlay folds");
        crate::INDEX_FOLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        RangeIndex {
            def: self.def.clone(),
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            as_of: self.as_of,
            unindexable: self.unindexable,
        }
    }

    /// The definition this index was built from.
    pub fn def(&self) -> &IndexDef {
        &self.def
    }

    /// The snapshot this index describes.
    pub fn as_of(&self) -> u64 {
        self.as_of
    }

    /// Whether any pending change is layered over the base. When false every
    /// read below takes the original single-run path exactly as before.
    fn has_overlay(&self) -> bool {
        !self.added.is_empty() || !self.removed_is_empty()
    }

    /// The live `(key, body)` pairs in `[lo, hi)`, in key order — base minus
    /// `removed`, merged with `added`. The one place the overlay is resolved;
    /// every public read is expressed in terms of it, so a read path cannot
    /// forget the overlay and quietly answer from the base alone.
    fn live_range<'a>(
        &'a self,
        lo: &IndexKey,
        hi: &IndexKey,
    ) -> impl Iterator<Item = (&'a IndexKey, &'a [u8])> {
        let bs = self.entries.partition_point(|(k, _)| k < lo);
        let be = self.entries.partition_point(|(k, _)| k < hi);
        let as_ = self.added.partition_point(|(k, _)| k < lo);
        let ae = self.added.partition_point(|(k, _)| k < hi);
        let mut base = self.entries[bs..be]
            .iter()
            .filter(|(_, b)| !self.is_removed(b))
            .peekable();
        let mut add = self.added[as_..ae].iter().peekable();
        // Both runs are sorted on the `(key, body)` TUPLE — the same order
        // `build` leaves the base in — so merging on the tuple reproduces a
        // rebuild's order exactly.
        std::iter::from_fn(move || {
            let take_add = match (base.peek(), add.peek()) {
                (Some(b), Some(a)) => a < b,
                (None, Some(_)) => true,
                _ => false,
            };
            let e = if take_add { add.next()? } else { base.next()? };
            Some((&e.0, e.1.as_slice()))
        })
    }

    /// Indexed entries.
    ///
    /// O(base) when an overlay is pending, because a removed body cannot be
    /// located in a key-sorted base without scanning. No hot caller counts
    /// entries, so this is left honest rather than made fast with a maintained
    /// counter that could drift out of agreement with the entries themselves.
    pub fn len(&self) -> usize {
        if !self.has_overlay() {
            return self.entries.len();
        }
        self.entries
            .iter()
            .filter(|(_, b)| !self.is_removed(b))
            .count()
            + self.added.len()
    }

    /// Whether the index holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many entries fall in `[lo, hi)` - two binary searches, no bodies
    /// cloned. A seek uses this to bail before materialising a match set too
    /// large to beat the column scan.
    pub fn range_count(&self, lo: &IndexKey, hi: &IndexKey) -> usize {
        if !self.has_overlay() {
            let start = self.entries.partition_point(|(k, _)| k < lo);
            let end = self.entries.partition_point(|(k, _)| k < hi);
            return end.saturating_sub(start);
        }
        self.live_range(lo, hi).count()
    }

    /// All bodies with `lo <= key < hi`, in key order, with the vintage.
    pub fn range(&self, lo: &IndexKey, hi: &IndexKey) -> RangeAnswer {
        counted!("index.range queries");
        // With nothing pending this is the original two-binary-searches-and-copy
        // path, byte for byte. Kept separate rather than folded into
        // `live_range` because this is the hot read — a point seek goes through
        // it — and the merged form pays for a peekable pair and a set lookup
        // per entry to handle an overlay that, most of the time, is empty.
        if !self.has_overlay() {
            let start = self.entries.partition_point(|(k, _)| k < lo);
            let end = self.entries.partition_point(|(k, _)| k < hi);
            return RangeAnswer {
                bodies: self.entries[start..end]
                    .iter()
                    .map(|(_, b)| b.clone())
                    .collect(),
                as_of: self.as_of,
                unindexable: self.unindexable,
            };
        }
        RangeAnswer {
            bodies: self.live_range(lo, hi).map(|(_, b)| b.to_vec()).collect(),
            as_of: self.as_of,
            unindexable: self.unindexable,
        }
    }

    /// Entry `(key, body)` pairs with key **< `hi`**, in DESCENDING key order,
    /// yielded LAZILY. The driver of an **index-ordered top-k**: scan
    /// newest-first and stop as soon as enough entries pass a downstream filter,
    /// never materialising the whole `< hi` set — that is what beats a full
    /// expand-then-sort when the filter is non-selective. `key` is yielded (not
    /// just the body) so the caller can resolve ORDER-BY ties at the top-k
    /// boundary. Bodies are BORROWED, not cloned. `unindexable` rows (a value
    /// the index could not order) are absent, as in every other query — the
    /// caller that needs a total census must account for `self.unindexable`.
    pub fn iter_desc_below<'a>(
        &'a self,
        hi: &IndexKey,
    ) -> impl Iterator<Item = (&'a IndexKey, &'a [u8])> {
        let end = self.entries.partition_point(|(k, _)| k < hi);
        let aend = self.added.partition_point(|(k, _)| k < hi);
        counted!("index.desc scans");
        // Merged DESCENDING, and still lazy — the top-k driver stops as soon as
        // enough entries pass its filter, so materialising the merge here would
        // give up the property that makes this beat expand-then-sort.
        let mut base = self.entries[..end]
            .iter()
            .rev()
            .filter(|(_, b)| !self.is_removed(b))
            .peekable();
        let mut add = self.added[..aend].iter().rev().peekable();
        std::iter::from_fn(move || {
            let take_add = match (base.peek(), add.peek()) {
                (Some(b), Some(a)) => a > b, // descending: the greater first
                (None, Some(_)) => true,
                _ => false,
            };
            let e = if take_add { add.next()? } else { base.next()? };
            Some((&e.0, e.1.as_slice()))
        })
    }

    /// The greatest key the index holds that is `< hi`, or `None` if none —
    /// the first key `iter_desc_below` would yield.
    pub fn max_key_below(&self, hi: &IndexKey) -> Option<&IndexKey> {
        if !self.has_overlay() {
            let end = self.entries.partition_point(|(k, _)| k < hi);
            return end.checked_sub(1).map(|i| &self.entries[i].0);
        }
        // Defined as the first key the descending walk yields, so it is derived
        // from that walk rather than restated — the two cannot disagree.
        self.iter_desc_below(hi).next().map(|(k, _)| k)
    }

    // ── persistence — the derived index, serialised (index-at-seal) ──────────
    //
    // A `RangeIndex` is a rebuildable artifact, so a persisted copy is a cache:
    // written at seal/compaction, loaded on open so the first query need not
    // rebuild it. Same BLAKE3 discipline as the segment format — a corrupt index
    // file fails to load rather than answering wrong.

    /// Serialise this index to bytes: `as_of`, `unindexable`, then the sorted
    /// `(key, body)` entries, with a trailing BLAKE3 over all of it. The `def`
    /// is NOT stored — the caller supplies it on load (the file is named by its
    /// property), keeping the index a pure `(key -> body)` artifact.
    pub fn to_bytes(&self) -> Vec<u8> {
        // FOLD FIRST. The serialised form is a flat sorted run with nowhere to
        // put an overlay, so writing `self.entries` directly would persist the
        // base and silently drop every pending change — an index file that
        // loads cleanly, verifies its BLAKE3, and is missing the most recent
        // writes. Folding is a no-op when nothing is pending.
        if self.has_overlay() {
            return self.folded().to_bytes();
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"ENGRIDX1");
        out.extend_from_slice(&self.as_of.to_le_bytes());
        out.extend_from_slice(&self.unindexable.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (key, body) in self.entries.iter() {
            match key {
                IndexKey::Int(i) => {
                    out.push(0);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                IndexKey::Float(f) => {
                    out.push(1);
                    out.extend_from_slice(&f.to_le_bytes());
                }
                IndexKey::Str(s) => {
                    out.push(2);
                    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                    out.extend_from_slice(s);
                }
            }
            out.extend_from_slice(&(body.len() as u64).to_le_bytes());
            out.extend_from_slice(body);
        }
        let hash = blake3::hash(&out);
        out.extend_from_slice(hash.as_bytes());
        out
    }

    /// Reconstruct an index from [`RangeIndex::to_bytes`], verifying the BLAKE3
    /// and re-attaching `def`. `None` on any structural or integrity failure —
    /// a bad index file is DISCARDED (the store rebuilds it), never trusted.
    pub fn from_bytes(bytes: &[u8], def: IndexDef) -> Option<RangeIndex> {
        if bytes.len() < 32 + 8 + 8 + 8 + 8 {
            return None;
        }
        let (body, stored_hash) = bytes.split_at(bytes.len() - 32);
        if blake3::hash(body).as_bytes() != stored_hash {
            return None;
        }
        let mut p = 0usize;
        let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
            let s = body.get(*p..*p + n)?;
            *p += n;
            Some(s)
        };
        if take(&mut p, 8)? != b"ENGRIDX1" {
            return None;
        }
        let as_of = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?);
        let unindexable = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?);
        let n = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?) as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let key = match take(&mut p, 1)?[0] {
                0 => IndexKey::Int(i64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?)),
                1 => IndexKey::Float(f64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?)),
                2 => {
                    let len = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?) as usize;
                    IndexKey::Str(take(&mut p, len)?.to_vec())
                }
                _ => return None,
            };
            let blen = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?) as usize;
            let bbody = take(&mut p, blen)?.to_vec();
            entries.push((key, bbody));
        }
        // The entries were written in sorted order (the index maintains it); a
        // tampered-but-hash-valid file can't occur, so trust the order.
        Some(RangeIndex {
            def,
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            as_of,
            unindexable,
        })
    }
}
