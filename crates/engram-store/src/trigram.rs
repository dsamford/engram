//! A trigram index — the one that makes `=~`, `CONTAINS` and `ENDS WITH`
//! seekable.
//!
//! # What it is for
//!
//! A range index sorts by value, so it answers equality, ranges and prefixes.
//! It cannot answer "contains", "ends with", or a regular expression, because
//! none of those is a contiguous span of any sort order. Those queries have
//! therefore been full scans, which is what makes code-shaped search — find
//! every value containing `->foo`, or matching `fn\s+parse_\w+` — cost the
//! whole label every time.
//!
//! This index inverts the problem, as Google Code Search does: every value
//! contributes the set of three-character windows it contains, and a query is
//! turned into a boolean condition over those windows. `foo.*bar` requires the
//! trigrams of `foo` AND those of `bar`; whatever satisfies that condition is
//! a **candidate**, and the real matcher then verifies each one.
//!
//! # THE INVARIANT
//!
//! **THE ANSWER IS A SUPERSET, NEVER AN ORACLE.** Every id this index returns
//! is re-verified by running the actual predicate. That asymmetry is the whole
//! design: an answer that is too WIDE costs time, and an answer that is too
//! NARROW loses rows silently. Every decline path here therefore returns
//! "cannot serve — scan instead" rather than a partial answer, and the query
//! analysis that produces the condition lives beside the pattern parser in
//! `engram-cypher` so that it and the matcher cannot disagree.
//!
//! # Why a sidecar and not keyspace rows
//!
//! The same absolute rule that puts the range index here: a user value in a
//! sort-ordered key position is order-preserving encryption. **A TRIGRAM IS
//! USER DATA** — three characters of it, verbatim — so it is at least as
//! sensitive as the value it came from, and it lives in a derived segment the
//! primary data can always rebuild.
//!
//! # Characters, not bytes
//!
//! A trigram is three `char`s, not three bytes. A byte window can begin or end
//! in the middle of a UTF-8 sequence, and the query analysis would then have to
//! reason about the *encodings* of character classes to keep the superset
//! argument sound — which is exactly where a narrowing bug would live. The key
//! is four times wider and the argument is provable; a later format version can
//! delta-encode without disturbing any of it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use engram_key::KeyPrefix;
use engram_key::value::Tag;
use engram_observe::{counted, sometimes};

use crate::Store;
use crate::index::IndexDef;
use crate::record::get_property;

/// The character that stands before the start of an indexed value.
///
/// The sentinels are what let an anchored query — and `=~` is always anchored,
/// being a full match — become an ordinary trigram requirement, and what make
/// `ENDS WITH` indexable down to a single character. Four extra trigrams per
/// value buys both.
pub const START: char = '\u{2}';

/// The character that stands after the end of an indexed value. See [`START`].
pub const END: char = '\u{3}';

/// How many overlay entries accumulate before the overlay is folded into a new
/// base.
///
/// # A constant, and NOT a fraction of the base — measured, having tried it
///
/// The obvious criticism of this number is that its unit did not survive being
/// copied from the range index: a range write contributes ONE overlay entry,
/// so 4,096 means 4,096 writes, while a trigram write contributes one entry
/// per distinct trigram — about a hundred for a hundred-character body — so
/// the same constant arrives in roughly forty writes. Each arrival rebuilds
/// the base, on the thread of whichever reader found the index stale.
///
/// The conclusion drawn from that — fold at a fraction of the base, so
/// O(base) work buys O(base) insertions — was **wrong, and was reverted after
/// being measured**. `trigram`'s interleaved stage, 5,000 write/read pairs:
///
/// ```text
///   threshold        median      p90       p99        max      wall
///   4,096 (this)    0.900 ms  1.268 ms  1.681 ms  238.8 ms   8.15 s
///   16,384          1.628 ms  2.283 ms  2.811 ms  256.4 ms  10.30 s
///   65,536          5.157 ms  6.144 ms  7.688 ms   15.8 ms  23.16 s
///   base/8          4.943 ms  5.707 ms  6.884 ms   11.3 ms  21.55 s
/// ```
///
/// Two things a 500-pair run did not show and that one does. A catch-up
/// begins `self.clone()`, so the OVERLAY IS COPIED ON EVERY STALE READ, and a
/// bigger threshold buys a bigger copy on every read to save a rare one. And
/// the large thresholds do not make the fold cheap; they mean **no fold
/// happens within the run at all**, which is why their maxima collapse. The
/// spike is postponed, not amortised, and lands larger.
///
/// # What DID fix it: making the fold cheap, not rare
///
/// The amortisation argument was reaching for the right thing by the wrong
/// route. Two changes to [`TrigramIndex::fold`] itself, neither touching this
/// constant:
///
/// - **merge instead of sort.** Both inputs are already ordered, so
///   re-sorting their concatenation pays O(n log n) to rebuild an order it
///   was handed.
/// - **share the document key** ([`BodyKey`]). A clone per surviving entry
///   was a heap allocation per entry; behind an `Arc` it is a refcount bump,
///   and the base holds one key per document rather than one per trigram.
///
/// ```text
///                     median      p99        max      wall
///   before           1.35-1.56  2.8-7.6  233-240 ms  10.3-12.6 s
///   after            1.03-1.42  2.4-3.7   60-84  ms   7.4-9.5  s
/// ```
///
/// Better on every axis — median, tail AND throughput — which is the
/// signature of removing work rather than moving it, and the contrast with
/// the threshold change above is the reason both are recorded here. A
/// remaining ~60 ms maximum is the O(base) rebuild itself, which is inherent;
/// taking it off the reader's thread entirely would need a maintenance pass
/// and is the next move if it ever matters.
const FOLD_AT: usize = 4_096;

/// How many removals are kept in the small sorted bucket before merging.
const RECENT_CAP: usize = 256;

/// A document's store key, SHARED between all of its trigram entries.
///
/// An `Arc<[u8]>` and not a `Vec<u8>`, which is the difference between a fold
/// that allocates once per ENTRY and one that allocates once per DOCUMENT. A
/// hundred-character body contributes about a hundred entries, all naming the
/// same key, so the owned form stored a hundred copies of it and the fold —
/// which rebuilds the base by cloning every surviving entry — paid a heap
/// allocation for each.
///
/// That allocation was the fold's cost, and the fold runs on the thread of
/// whichever READER found the index stale. Measured on a 20,000-row corpus:
/// 239 ms against a 0.9 ms median. Replacing the sort with a merge (both
/// inputs are already ordered) took it to ~140 ms; sharing the key takes it
/// the rest of the way, because a clone becomes a refcount bump.
///
/// `Arc<[u8]>` orders and compares BY CONTENT, so every `sort`,
/// `partition_point` and `BTreeSet` here keeps exactly the semantics it had
/// with `Vec<u8>`. That is what makes this a representation change and not a
/// behaviour change, and why the equals-a-rebuild contract still holds.
type BodyKey = Arc<[u8]>;

/// Three consecutive characters of a value, after case folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Trigram(pub char, pub char, pub char);

/// Fold one character for indexing.
///
/// **SINGLE SCALAR, AND DUPLICATED ON PURPOSE.** `engram-store` sits below
/// `engram-cypher` in the crate graph and may not reach up to it, so the query
/// side has its own copy of this function. The two must agree for every
/// character in existence, or a query asks for trigrams that were never
/// stored — a silent narrowing. That agreement is not assumed: it is asserted
/// over the whole Basic Multilingual Plane by a test in `engram-graph`, which
/// is the one crate that can see both.
#[must_use]
pub fn fold_scalar(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    c.to_lowercase().next().unwrap_or(c)
}

/// The trigrams of a value, padded with the sentinels.
#[must_use]
pub fn trigrams_of_value(s: &str) -> BTreeSet<Trigram> {
    let mut chars = Vec::with_capacity(s.chars().count() + 4);
    chars.push(START);
    chars.push(START);
    chars.extend(s.chars().map(fold_scalar));
    chars.push(END);
    chars.push(END);
    let mut out = BTreeSet::new();
    for w in chars.windows(3) {
        out.insert(Trigram(w[0], w[1], w[2]));
    }
    out
}

/// A boolean condition over trigrams, as the query side derived it.
///
/// Structurally identical to the type in `engram-cypher`, and separate for the
/// same layering reason `fold_scalar` is. The graph layer converts between
/// them at the one call site that has both in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrigramQuery {
    /// Every value is a candidate — the index cannot help.
    All,
    /// No value can match.
    None,
    /// The value must contain this trigram.
    Lit(Trigram),
    /// Every branch must hold.
    And(Vec<TrigramQuery>),
    /// At least one branch must hold.
    Or(Vec<TrigramQuery>),
}

/// An answer, carrying its vintage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrigramAnswer {
    /// Candidate entity bodies, ascending. **A SUPERSET** — every one must be
    /// re-verified against the real predicate.
    pub bodies: Vec<Vec<u8>>,
    /// The snapshot the answer describes. An answer without its vintage gets
    /// read as current.
    pub as_of: u64,
}

/// The derived trigram index.
#[derive(Debug, Clone)]
pub struct TrigramIndex {
    def: IndexDef,
    /// Sorted `(trigram, body)` pairs — the immutable BASE, shared behind an
    /// `Arc` so that carrying the index over a write shares it rather than
    /// copying it. The range index learned that lesson at 3.5x per write; this
    /// structure is larger per row, not smaller.
    entries: Arc<Vec<(Trigram, BodyKey)>>,
    /// Pairs added since the base was last folded. Bounded by [`FOLD_AT`].
    added: Vec<(Trigram, BodyKey)>,
    /// Bodies whose base entries no longer apply — deleted, or re-indexed
    /// under different trigrams (in which case the new pairs are in `added`).
    removed: Arc<BTreeSet<Vec<u8>>>,
    /// Removals not yet merged into `removed`, kept sorted and small.
    removed_recent: Vec<Vec<u8>>,
    /// Every body the index covers, ascending — needed to price a probe
    /// against the alternative of scanning, and to answer `All` honestly.
    docs: Arc<Vec<Vec<u8>>>,
    /// The snapshot this index describes.
    as_of: u64,
    /// Rows whose indexed property was present but was not a string.
    ///
    /// **A NON-ZERO COUNT DISABLES THE INDEX ENTIRELY**, which is the one
    /// place this diverges from the range index. There, an unindexable row
    /// makes the answer an honest FLOOR over typed rows and the caller is told
    /// so. Here the answer is a CANDIDATE SET, and a candidate set that is a
    /// floor is simply a wrong answer: the row it omitted might have matched.
    unindexable: u64,
}

impl TrigramIndex {
    /// Build over the given entity bodies at snapshot `ts`.
    ///
    /// Label-scoped by construction — `CREATE TRIGRAM INDEX … FOR (n:File)`
    /// names a label and means it. There is deliberately no partition-wide
    /// variant: the range index has one and it cost a 40 GiB pod, because a
    /// property shared across millions of nodes indexes all of them.
    pub fn build_over(
        store: &Store,
        group: &KeyPrefix,
        def: IndexDef,
        ts: u64,
        bodies: impl IntoIterator<Item = Vec<u8>>,
    ) -> TrigramIndex {
        let mut entries: Vec<(Trigram, BodyKey)> = Vec::new();
        let mut docs: Vec<Vec<u8>> = Vec::new();
        let mut unindexable = 0u64;
        for body in bodies {
            let Some(record_bytes) = store.get_at(group, &body, ts) else {
                continue; // not visible at this snapshot
            };
            let Some(tagged) = get_property(&record_bytes, def.property()) else {
                continue;
            };
            match string_of_tagged(&tagged) {
                Some(text) => {
                    docs.push(body.clone());
                    // ONE allocation for the key, shared by every entry this
                    // document contributes. See [`BodyKey`].
                    let key: BodyKey = Arc::from(body.as_slice());
                    for t in trigrams_of_value(&text) {
                        entries.push((t, Arc::clone(&key)));
                    }
                }
                None => {
                    unindexable += 1;
                    sometimes!("trigram.row was not a string", true);
                }
            }
        }
        entries.sort();
        docs.sort();
        counted!("store.trigram index built");
        TrigramIndex {
            def,
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            docs: Arc::new(docs),
            as_of: ts,
            unindexable,
        }
    }

    /// The definition this index was built from.
    #[must_use]
    pub fn def(&self) -> &IndexDef {
        &self.def
    }

    /// The snapshot this index describes.
    #[must_use]
    pub fn as_of(&self) -> u64 {
        self.as_of
    }

    /// How many rows carried the property but were not strings.
    #[must_use]
    pub fn unindexable(&self) -> u64 {
        self.unindexable
    }

    /// How many entries the folded BASE holds.
    ///
    /// Exposed for the fold-threshold test, which is the only caller: the
    /// relationship between this and [`TrigramIndex::overlay_len`] is the
    /// thing `FOLD_AT` governs, and a test that cannot see both can only
    /// assert timings.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// How many entries the unfolded OVERLAY holds.
    ///
    /// This is the quantity DEEP-COPIED by every catch-up (`with_changes`
    /// begins `self.clone()`), which is why it must stay bounded by a small
    /// constant rather than by a fraction of the base. See [`FOLD_AT`].
    #[must_use]
    pub fn overlay_len(&self) -> usize {
        self.added.len()
    }

    /// How many values the index covers.
    ///
    /// Read straight off `docs`, which `with_changes` maintains exactly.
    /// Subtracting the removal set as well would count every deletion twice —
    /// `docs` no longer contains it and `removed` still names it.
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    /// Whether this body's BASE entries are stale.
    ///
    /// **THIS APPLIES TO THE BASE ONLY, NEVER TO THE OVERLAY.** A body that is
    /// re-indexed is marked here so its old entries stop being returned, and
    /// its new entries go into `added` — so a filter that applied this to
    /// `added` as well would subtract away the very entries the catch-up just
    /// wrote, leaving the row findable by nothing at all. That was a real bug,
    /// caught by the equals-a-rebuild test.
    fn base_is_stale(&self, body: &[u8]) -> bool {
        self.removed.contains(body)
            || self
                .removed_recent
                .binary_search_by(|b| b[..].cmp(body))
                .is_ok()
    }

    /// The bodies carrying `t`, ascending — base plus overlay, minus removals.
    fn posting(&self, t: Trigram) -> Vec<Vec<u8>> {
        let lo = self.entries.partition_point(|(k, _)| *k < t);
        let hi = self.entries.partition_point(|(k, _)| *k <= t);
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(hi - lo);
        for (_, body) in &self.entries[lo..hi] {
            if !self.base_is_stale(body) {
                out.push(body.to_vec());
            }
        }
        // The overlay is NOT filtered — see `base_is_stale`.
        for (k, body) in &self.added {
            if *k == t {
                out.push(body.to_vec());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// How many bodies carry `t` — the cheap cardinality probe.
    ///
    /// The planner compares candidate sets before materialising any of them,
    /// so this must not build one. It counts the base span and the overlay
    /// without cloning a body.
    #[must_use]
    pub fn count(&self, t: Trigram) -> usize {
        // TWO BINARY SEARCHES, NOT A WALK. This is the price the planner pays
        // to DECLINE — it asks how big a candidate set would be before deciding
        // whether to build one — so it must not cost the size of the answer.
        //
        // The first version walked the span. On a 20,000-row corpus an
        // `ENDS WITH` whose trigram covered 3,029 rows spent 0.5 ms pricing a
        // probe it then correctly refused for being over the cap, which turned
        // a seek that declines into a seek that declines SLOWLY: measurably
        // worse than never having consulted the index. The bench caught it.
        let lo = self.entries.partition_point(|(k, _)| *k < t);
        let hi = self.entries.partition_point(|(k, _)| *k <= t);
        let mut n = hi - lo;
        // The stale filter is O(span), so it is paid ONLY when something has
        // actually been removed. On a freshly built index — the common case —
        // both sets are empty and the count is two binary searches.
        if !self.removed.is_empty() || !self.removed_recent.is_empty() {
            n -= self.entries[lo..hi]
                .iter()
                .filter(|(_, body)| self.base_is_stale(body))
                .count();
        }
        n + self.added.iter().filter(|(k, _)| *k == t).count()
    }

    /// An upper bound on how many candidates `q` would produce, WITHOUT
    /// producing them.
    ///
    /// `And` is bounded by its smallest branch, `Or` by the sum of its
    /// branches, `All` by every document. Saturating throughout, because an
    /// estimate that overflows into a small number would make the planner
    /// choose this index precisely when it should not.
    #[must_use]
    pub fn estimate(&self, q: &TrigramQuery) -> usize {
        match q {
            TrigramQuery::All => self.doc_count(),
            TrigramQuery::None => 0,
            TrigramQuery::Lit(t) => self.count(*t),
            TrigramQuery::And(qs) => qs
                .iter()
                .map(|q| self.estimate(q))
                .min()
                .unwrap_or_else(|| self.doc_count()),
            TrigramQuery::Or(qs) => qs
                .iter()
                .map(|q| self.estimate(q))
                .fold(0usize, usize::saturating_add)
                .min(self.doc_count()),
        }
    }

    /// The candidate bodies satisfying `q`, or `None` if the index cannot
    /// serve it.
    ///
    /// `None` means "scan instead" and is returned when the condition
    /// constrains nothing, when the estimate is over `cap`, or when any row
    /// was unindexable — see [`TrigramIndex::unindexable`] for why that last
    /// one is fatal here and merely reported by the range index.
    #[must_use]
    pub fn query(&self, q: &TrigramQuery, cap: Option<usize>) -> Option<TrigramAnswer> {
        if self.unindexable > 0 {
            sometimes!("trigram.probe declined over an unindexable row", true);
            return None;
        }
        if matches!(q, TrigramQuery::All) {
            sometimes!("trigram.query analysis returned match-all", true);
            return None;
        }
        if let Some(cap) = cap {
            if self.estimate(q) > cap {
                sometimes!("trigram.probe declined over the cap", true);
                counted!("interp.trigram probe declined");
                return None;
            }
        }
        let bodies = self.evaluate(q)?;
        counted!("store.trigram postings intersected");
        Some(TrigramAnswer {
            bodies,
            as_of: self.as_of,
        })
    }

    /// Sorted-list evaluation of the condition.
    fn evaluate(&self, q: &TrigramQuery) -> Option<Vec<Vec<u8>>> {
        match q {
            // Reached only through a branch of an `Or`, since `query` refuses
            // a bare `All` — and there it means "this branch admits
            // everything", which makes the whole union everything.
            TrigramQuery::All => None,
            TrigramQuery::None => Some(Vec::new()),
            TrigramQuery::Lit(t) => Some(self.posting(*t)),
            TrigramQuery::And(qs) => {
                let mut acc: Option<Vec<Vec<u8>>> = None;
                // Cheapest branch first: an intersection is bounded by its
                // smallest input, so starting small keeps every later step
                // small too.
                let mut order: Vec<&TrigramQuery> = qs.iter().collect();
                order.sort_by_key(|q| self.estimate(q));
                for q in order {
                    let Some(next) = self.evaluate(q) else {
                        // A branch that constrains nothing simply drops out of
                        // an AND; the others still bound the answer.
                        continue;
                    };
                    acc = Some(match acc {
                        None => next,
                        Some(cur) => intersect(&cur, &next),
                    });
                    if acc.as_ref().is_some_and(std::vec::Vec::is_empty) {
                        break;
                    }
                }
                acc
            }
            TrigramQuery::Or(qs) => {
                let mut acc: Vec<Vec<u8>> = Vec::new();
                for q in qs {
                    // ONE UNCONSTRAINED BRANCH MAKES THE WHOLE UNION
                    // UNCONSTRAINED. Dropping it instead would narrow the
                    // answer, which is the direction that loses rows.
                    let next = self.evaluate(q)?;
                    acc = union(&acc, &next);
                }
                Some(acc)
            }
        }
    }

    /// Carry the index forward over a set of changes, or decline.
    ///
    /// `changes` maps a body to its new trigram set, or `None` where the row
    /// was deleted or lost its property. **The result must be identical to a
    /// fresh build at the same `ts`** — that equality is what makes an
    /// incremental cache a cache rather than a second implementation, and it
    /// is asserted by a test rather than argued for here.
    ///
    /// Returns `None` when the caller should rebuild instead.
    #[must_use]
    pub fn with_changes(
        &self,
        changes: &BTreeMap<Vec<u8>, Option<BTreeSet<Trigram>>>,
        ts: u64,
    ) -> Option<TrigramIndex> {
        if self.unindexable > 0 {
            // The index is already refusing to serve; carrying it forward
            // would only carry the refusal.
            return None;
        }
        if changes.len() > FOLD_AT {
            return None;
        }
        let mut next = self.clone();
        let changed: BTreeSet<Vec<u8>> = changes.keys().cloned().collect();

        // A body that changes loses whatever it previously contributed. Its
        // BASE entries are marked stale for good; its OVERLAY entries are
        // dropped outright, because the overlay holds only current entries and
        // this body is about to write new ones (or none, if it was deleted).
        next.added.retain(|(_, body)| !changed.contains(&body[..]));

        let mut docs_added: Vec<Vec<u8>> = Vec::new();
        let mut docs_removed: BTreeSet<Vec<u8>> = BTreeSet::new();
        for (body, tris) in changes {
            match tris {
                Some(tris) => {
                    docs_added.push(body.clone());
                    let key: BodyKey = Arc::from(body.as_slice());
                    for t in tris {
                        next.added.push((*t, Arc::clone(&key)));
                    }
                }
                None => {
                    docs_removed.insert(body.clone());
                }
            }
        }

        next.removed_recent.extend(changed.iter().cloned());
        next.removed_recent.sort();
        next.removed_recent.dedup();
        if next.removed_recent.len() > RECENT_CAP {
            let mut rm = (*next.removed).clone();
            rm.extend(next.removed_recent.drain(..));
            next.removed = Arc::new(rm);
        }

        let mut docs = (*next.docs).clone();
        docs.retain(|b| !docs_removed.contains(b));
        docs.extend(docs_added);
        docs.sort();
        docs.dedup();
        next.docs = Arc::new(docs);

        next.added.sort();
        next.as_of = ts;
        if next.added.len() > FOLD_AT {
            next.fold();
        }
        counted!("graph.trigram index caught up");
        Some(next)
    }

    /// Merge the overlay into a fresh base.
    fn fold(&mut self) {
        // COUNTED, NOT `sometimes!`. A fold needs thousands of changes to
        // reach, so declaring it as a reachable state would fail the sim's
        // coverage floor for ever; its reachability is proven instead by
        // `a_catch_up_over_the_fold_threshold_declines_to_a_rebuild` and by
        // the equals-a-rebuild contract.
        counted!("trigram.overlay folded");

        // A MERGE, NOT A SORT. Both inputs are already in order — the base
        // because every fold leaves it so, the overlay because `with_changes`
        // sorts it before testing this threshold — so re-sorting their
        // concatenation throws that order away and pays O(n log n) to
        // reconstruct it.
        //
        // This is the whole of the fold's cost, and the fold runs on the
        // thread of whichever READER found the index stale: measured at
        // 239 ms against a 0.9 ms median on a 20,000-row corpus, which is a
        // 265x tail on an operation nobody asked for. The base dominates the
        // overlay by orders of magnitude here (the overlay is bounded by
        // `FOLD_AT`, the base grows with the corpus), so the comparison count
        // falls from n log n to n — about twenty-fold at a million entries.
        //
        // Rejected alternative: fold at a fraction of the base, the textbook
        // amortisation. Measured, and it made things worse for two reasons
        // recorded at [`FOLD_AT`]. Making the fold CHEAP is the fix that the
        // amortisation argument was reaching for.
        let base = Arc::clone(&self.entries);
        let added = std::mem::take(&mut self.added);
        let mut entries: Vec<(Trigram, BodyKey)> =
            Vec::with_capacity(base.len() + added.len());
        let mut ai = added.into_iter().peekable();
        for e in base.iter() {
            if self.base_is_stale(&e.1) {
                continue;
            }
            while ai.peek().is_some_and(|n| n < e) {
                entries.push(ai.next().expect("peeked"));
            }
            entries.push(e.clone());
        }
        entries.extend(ai);
        // Duplicates are adjacent after a merge of two sorted inputs, so this
        // stays the linear pass it was.
        entries.dedup();
        self.entries = Arc::new(entries);
        self.removed = Arc::new(BTreeSet::new());
        self.removed_recent.clear();
    }
}

/// Decode a tagged property value as a string, if it is one.
fn string_of_tagged(tagged: &[u8]) -> Option<String> {
    let tag = Tag::from_byte(*tagged.first()?);
    if tag != Tag::STRING {
        return None;
    }
    let len = u32::from_le_bytes(tagged.get(1..5)?.try_into().ok()?) as usize;
    let bytes = tagged.get(5..5 + len)?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// Sorted-list intersection.
fn intersect(a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i].clone());
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Sorted-list union.
fn union(a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j].clone());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i].clone());
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}
