//! A BM25 term index — the one that stops full-text search being a scan.
//!
//! # What it replaces
//!
//! `db.index.fulltext.queryNodes` walked every node of every covered label and
//! re-tokenised its text ON EVERY QUERY, scoring by raw summed term frequency.
//! That is O(label × properties × text) per statement, and the scoring has no
//! IDF and no length normalisation — so a term appearing in every document
//! counts as much as one appearing in three, and a long document outranks a
//! short one merely for being long.
//!
//! # Scored per FIELD, not per document
//!
//! Postings are keyed `(field, term)` and lengths are kept per field. That is
//! what makes the write hook cheap: `SET n.title = …` replaces field `title`'s
//! postings and its length and nothing else, and the new title is already in
//! the property change log — for a string property the logged value IS the
//! text — so a catch-up performs **no record reads at all**.
//!
//! A document-level BM25 over the concatenated properties would make `dl` a
//! function of every field, so a write to any one property would force a whole
//! record re-read and re-tokenise. That is the write-hook cost problem, and
//! decomposing by field dissolves it rather than mitigating it. It is also what
//! Lucene does for a multi-field query, so it is closer to the incumbent rather
//! than further from it.
//!
//! # A non-string field is zero terms, not a poisoned index
//!
//! This is the one place the discipline diverges from
//! [`crate::trigram::TrigramIndex`], deliberately. That index is a CANDIDATE
//! GENERATOR: the caller only ever re-verifies what it was handed, so a row it
//! silently omitted is a row lost, and any unindexable value therefore disables
//! it. This index is an ANSWER PRODUCER that must agree with the fallback scan
//! — and the scan's own rule is `if let Some(Value::Str(text))`, which ignores
//! a non-string property. Agreeing with the definition is not a floor, so a
//! non-string field contributes nothing, is counted, and changes nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use engram_key::KeyPrefix;
use engram_key::value::Tag;
use engram_observe::{counted, sometimes};

use crate::Store;
use crate::record::{PropertyId, get_property};
use crate::text;

/// A document's store key, SHARED between all of its term entries.
///
/// The same change, for the same reason, as `crate::trigram::BodyKey`: a
/// document contributes one entry per distinct TERM, all naming one key, so
/// the owned form stored a copy per term and the fold — which rebuilds the
/// base by cloning every surviving entry — paid a heap allocation for each.
/// `Arc<[u8]>` orders and compares by content, so nothing about the index's
/// behaviour changes.
type BodyKey = Arc<[u8]>;

/// How many overlay entries accumulate before the overlay is folded into a new
/// base.
///
/// # A constant, and not a fraction of the base
///
/// This index has the same shape as the trigram one — a document contributes
/// one overlay entry per distinct TERM, not one per write — so it invites the
/// same criticism of the constant, and the same answer applies. Folding at a
/// fraction of the base was tried there, measured, and reverted: a catch-up
/// deep-copies the overlay on every stale read, so a larger threshold charges
/// every reader to spare a rare fold, and a threshold large enough to hide the
/// fold only postpones it. See `crate::trigram::FOLD_AT` for the table.
///
/// Not re-measured here, and that is the honest status: the argument carries
/// over, the numbers do not. The fold spike is a real defect in both indexes,
/// and the fix for it is structural — fold off the reader's thread, or make
/// the overlay copy cheap — not a different number here.
const FOLD_AT: usize = 4_096;

/// How many removals are kept in the small sorted bucket before merging.
const RECENT_CAP: usize = 256;

/// Lucene's BM25 saturation parameter.
///
/// **NOT TUNABLE.** A knob nobody has measured is a lever without a
/// measurement, and a per-index float would have to round-trip through the
/// catalogue's JSON exactly or two builds would score the same corpus
/// differently. The rejected alternative is `OPTIONS { k1: …, b: … }`.
pub const K1: f64 = 1.2;

/// Lucene's BM25 length-normalisation parameter. See [`K1`].
pub const B: f64 = 0.75;

/// A field's position in the index's declared property list.
///
/// Declaration order is part of the index's identity — `ON EACH [n.title,
/// n.body]` and `ON EACH [n.body, n.title]` are different indexes — and it is
/// also the order every score is accumulated in, which is what makes the
/// summation reproducible.
pub type FieldId = u16;

/// One `(field, term)` posting key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldTerm {
    /// Which declared property.
    pub field: FieldId,
    /// The analysed term.
    pub term: String,
}

/// One document's per-field token counts.
///
/// A zero length means the field is absent, empty, or not a string — three
/// states the fallback scan also cannot tell apart, which is the point.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DocLen {
    body: Vec<u8>,
    len: Vec<u32>,
}

/// Corpus totals, per field.
///
/// Every member is a sum or a count, never an approximation, which is what
/// lets a catch-up keep them EXACTLY equal to a rebuild's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Corpus {
    /// Documents in the population.
    pub docs: u64,
    /// Per field: documents whose value analysed to at least one token.
    pub with_field: Vec<u64>,
    /// Per field: the sum of `dl` over those documents.
    pub sum_len: Vec<u64>,
}

impl Corpus {
    /// The mean length of field `f`, over the documents that carry it.
    ///
    /// Zero when nothing carries it — which makes the length factor degenerate
    /// rather than divide by zero, and is handled at the one site that reads
    /// it.
    #[must_use]
    pub fn avgdl(&self, f: FieldId) -> f64 {
        let i = f as usize;
        let n = self.with_field.get(i).copied().unwrap_or(0);
        if n == 0 {
            return 0.0;
        }
        self.sum_len.get(i).copied().unwrap_or(0) as f64 / n as f64
    }
}

/// The derived term index.
#[derive(Debug, Clone)]
pub struct TermIndex {
    fields: Arc<Vec<PropertyId>>,
    /// Sorted `(key, body, tf)` — the immutable BASE, shared behind an `Arc` so
    /// carrying the index over a write shares it rather than copying it.
    entries: Arc<Vec<(FieldTerm, BodyKey, u32)>>,
    /// Entries written since the base was last folded. Bounded by [`FOLD_AT`].
    added: Vec<(FieldTerm, BodyKey, u32)>,
    /// Bodies whose BASE entries no longer apply.
    ///
    /// **APPLIES TO THE BASE ONLY, NEVER TO THE OVERLAY.** A document being
    /// re-indexed is marked here so its old entries stop being returned while
    /// its new ones sit in `added`; filtering `added` as well would subtract
    /// away the very entries a catch-up just wrote and leave the document
    /// findable by nothing. That was a real bug in the trigram index, caught by
    /// its equals-a-rebuild test, and it is repeated here because the rule is
    /// not obvious from either structure alone.
    removed: Arc<BTreeSet<Vec<u8>>>,
    /// Removals not yet merged into `removed`, kept sorted and small.
    removed_recent: Vec<Vec<u8>>,
    /// Per-document field lengths, body-ascending.
    docs: Arc<Vec<DocLen>>,
    corpus: Corpus,
    as_of: u64,
    /// Fields that carried a value which was not a string. Counted, never
    /// fatal — see the module documentation.
    non_string_fields: u64,
}

/// What changed about one document.
#[derive(Clone, Debug, Default)]
pub struct DocChange {
    /// Per-field replacements. `None` means the field now analyses to nothing.
    pub fields: Vec<(FieldId, Option<FieldContent>)>,
    /// `fields` is the document's COMPLETE content rather than a patch — a node
    /// that JOINED the population. Without this, a joining document with an
    /// empty title would keep whatever the index last held for that field.
    pub whole: bool,
    /// The document left the population. Overrides everything else.
    pub gone: bool,
}

/// One field's analysed content.
#[derive(Clone, Debug)]
pub struct FieldContent {
    /// Distinct terms with their counts, ascending.
    pub freqs: Vec<(String, u32)>,
    /// The field's length in tokens, with multiplicity — BM25's `dl`.
    pub len: u32,
}

impl FieldContent {
    /// Analyse `text` into content.
    #[must_use]
    pub fn of(text: &str) -> FieldContent {
        let (freqs, len) = text::term_freqs(text);
        FieldContent { freqs, len }
    }
}

/// One scored hit.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The document's entity body.
    pub body: Vec<u8>,
    /// Its BM25 score.
    pub score: f64,
}

impl TermIndex {
    /// Build over the given entity bodies at snapshot `ts`.
    pub fn build_over(
        store: &Store,
        group: &KeyPrefix,
        fields: Vec<PropertyId>,
        ts: u64,
        bodies: impl IntoIterator<Item = Vec<u8>>,
    ) -> TermIndex {
        let nf = fields.len();
        let mut entries: Vec<(FieldTerm, BodyKey, u32)> = Vec::new();
        let mut docs: Vec<DocLen> = Vec::new();
        let mut corpus = Corpus {
            docs: 0,
            with_field: vec![0; nf],
            sum_len: vec![0; nf],
        };
        let mut non_string_fields = 0u64;

        for body in bodies {
            let Some(record) = store.get_at(group, &body, ts) else {
                continue; // not visible at this snapshot
            };
            corpus.docs += 1;
            let mut len = vec![0u32; nf];
            for (f, prop) in fields.iter().enumerate() {
                let Some(tagged) = get_property(&record, *prop) else {
                    continue;
                };
                let Some(t) = string_of_tagged(&tagged) else {
                    non_string_fields += 1;
                    sometimes!("fulltext.document field was not a string", true);
                    continue;
                };
                let key: BodyKey = Arc::from(body.as_slice());
                let (freqs, dl) = text::term_freqs(&t);
                if dl == 0 {
                    continue;
                }
                len[f] = dl;
                corpus.with_field[f] += 1;
                corpus.sum_len[f] += u64::from(dl);
                for (term, tf) in freqs {
                    entries.push((
                        FieldTerm {
                            field: f as FieldId,
                            term,
                        },
                        Arc::clone(&key),
                        tf,
                    ));
                }
            }
            docs.push(DocLen {
                body: body.clone(),
                len,
            });
        }
        entries.sort();
        docs.sort_by(|a, b| a.body.cmp(&b.body));
        counted!("store.term index built");
        TermIndex {
            fields: Arc::new(fields),
            entries: Arc::new(entries),
            added: Vec::new(),
            removed: Arc::new(BTreeSet::new()),
            removed_recent: Vec::new(),
            docs: Arc::new(docs),
            corpus,
            as_of: ts,
            non_string_fields,
        }
    }

    /// The snapshot this index describes.
    #[must_use]
    pub fn as_of(&self) -> u64 {
        self.as_of
    }

    /// The properties this index covers, in declaration order.
    #[must_use]
    pub fn fields(&self) -> &[PropertyId] {
        &self.fields
    }

    /// The corpus totals.
    #[must_use]
    pub fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    /// How many documents the index covers.
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    /// How many field values were present but not strings.
    #[must_use]
    pub fn non_string_fields(&self) -> u64 {
        self.non_string_fields
    }

    fn base_is_stale(&self, body: &[u8]) -> bool {
        self.removed.contains(body)
            || self
                .removed_recent
                .binary_search_by(|b| b[..].cmp(body))
                .is_ok()
    }

    /// Every live `(body, tf)` for one key, ascending by body.
    fn posting(&self, k: &FieldTerm) -> Vec<(Vec<u8>, u32)> {
        let lo = self.entries.partition_point(|(key, _, _)| key < k);
        let hi = self.entries.partition_point(|(key, _, _)| key <= k);
        let mut out: Vec<(Vec<u8>, u32)> = Vec::with_capacity(hi - lo);
        for (_, body, tf) in &self.entries[lo..hi] {
            if !self.base_is_stale(body) {
                out.push((body.to_vec(), *tf));
            }
        }
        for (key, body, tf) in &self.added {
            if key == k {
                out.push((body.to_vec(), *tf));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }

    /// How many documents carry `k` — the document frequency.
    ///
    /// **RECOUNTED FROM THE LIVE POSTING, NEVER CACHED.** A stale `df` is not a
    /// loose bound, it is a WRONG idf: every score in the answer shifts, and
    /// nothing about the result looks unusual. The walk is the price of that.
    #[must_use]
    pub fn doc_freq(&self, k: &FieldTerm) -> u64 {
        self.posting(k).len() as u64
    }

    /// One document's field lengths, if it is in the population.
    fn doc_len(&self, body: &[u8]) -> Option<&[u32]> {
        self.docs
            .binary_search_by(|d| d.body[..].cmp(body))
            .ok()
            .map(|i| &self.docs[i].len[..])
    }

    /// Score `query` over the whole index, best first.
    ///
    /// The answer is the ANSWER — not a candidate set — so it is ordered by the
    /// total order `(score DESC, body ASC)`. The tie-break is explicit because
    /// the scan it replaces broke ties by label-scan order, which was an
    /// accident of iteration that nothing declared and nobody could rely on.
    #[must_use]
    pub fn query(&self, query: &str) -> Vec<Hit> {
        let tokens = text::analyse(query);
        if tokens.is_empty() {
            return Vec::new();
        }
        let nf = self.fields.len();
        let plan = QueryPlan::new(nf, tokens, &self.corpus, &mut |f, t| {
            self.doc_freq(&FieldTerm {
                field: f,
                term: t.to_string(),
            })
        });

        // Gather every document any query token touches, with its per-field
        // term frequencies. A BTreeMap and not a HashMap: iteration order
        // reaches the result through the sort's tie-break, so it must not vary.
        let mut acc: BTreeMap<Vec<u8>, Vec<Vec<u32>>> = BTreeMap::new();
        for f in 0..nf {
            for (i, token) in plan.tokens.iter().enumerate() {
                let k = FieldTerm {
                    field: f as FieldId,
                    term: token.clone(),
                };
                for (body, tf) in self.posting(&k) {
                    let e = acc
                        .entry(body)
                        .or_insert_with(|| vec![vec![0u32; plan.tokens.len()]; nf]);
                    e[f][i] = tf;
                }
            }
        }

        let mut out: Vec<Hit> = Vec::with_capacity(acc.len());
        for (body, tfs) in acc {
            let Some(dl) = self.doc_len(&body) else {
                continue;
            };
            let score = plan.score(&tfs, dl);
            if score > 0.0 {
                out.push(Hit { body, score });
            }
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.body.cmp(&b.body))
        });
        counted!("store.term index queries");
        out
    }

    /// Carry the index forward over a set of changes, or decline.
    ///
    /// **The result must be identical to a fresh build at the same `ts`.** That
    /// equality is what makes an incremental cache a cache rather than a second
    /// implementation with its own bugs, and it is asserted by a test rather
    /// than argued for here.
    #[must_use]
    pub fn with_changes(&self, changes: &BTreeMap<Vec<u8>, DocChange>, ts: u64) -> Option<TermIndex> {
        if changes.len() > FOLD_AT {
            return None;
        }
        let nf = self.fields.len();
        if changes
            .values()
            .any(|c| c.fields.iter().any(|(f, _)| (*f as usize) >= nf))
        {
            // A field outside the declaration is a definition change, not a
            // data change. Rebuild rather than guess at the new shape.
            return None;
        }
        let mut next = self.clone();
        let changed: BTreeSet<Vec<u8>> = changes.keys().cloned().collect();

        // The overlay holds only CURRENT entries, so a changed document's
        // previous overlay entries go before its new ones are written.
        next.added.retain(|(_, body, _)| !changed.contains(&body[..]));

        let mut docs: Vec<DocLen> = (*next.docs).clone();
        for (body, change) in changes {
            let old = docs
                .binary_search_by(|d| d.body[..].cmp(body))
                .ok()
                .map(|i| docs[i].len.clone());

            if change.gone {
                if let Some(old) = &old {
                    for (f, dl) in old.iter().enumerate() {
                        if *dl > 0 {
                            next.corpus.with_field[f] -= 1;
                            next.corpus.sum_len[f] -= u64::from(*dl);
                        }
                    }
                    next.corpus.docs -= 1;
                    docs.retain(|d| &d.body != body);
                }
                continue;
            }

            let mut len = old.clone().unwrap_or_else(|| vec![0u32; nf]);
            if old.is_none() {
                next.corpus.docs += 1;
            }
            if change.whole {
                // A joining document's listed fields are ALL of its content, so
                // an unlisted field is empty rather than unchanged.
                for (f, dl) in len.iter_mut().enumerate() {
                    if *dl > 0 {
                        next.corpus.with_field[f] -= 1;
                        next.corpus.sum_len[f] -= u64::from(*dl);
                    }
                    let _ = f;
                    *dl = 0;
                }
            }
            for (f, content) in &change.fields {
                let fi = *f as usize;
                if len[fi] > 0 {
                    next.corpus.with_field[fi] -= 1;
                    next.corpus.sum_len[fi] -= u64::from(len[fi]);
                }
                len[fi] = 0;
                let Some(c) = content else { continue };
                if c.len == 0 {
                    continue;
                }
                len[fi] = c.len;
                let key: BodyKey = Arc::from(body.as_slice());
                next.corpus.with_field[fi] += 1;
                next.corpus.sum_len[fi] += u64::from(c.len);
                for (term, tf) in &c.freqs {
                    next.added.push((
                        FieldTerm {
                            field: *f,
                            term: term.clone(),
                        },
                        Arc::clone(&key),
                        *tf,
                    ));
                }
            }
            match docs.binary_search_by(|d| d.body[..].cmp(body)) {
                Ok(i) => docs[i].len = len,
                Err(i) => docs.insert(
                    i,
                    DocLen {
                        body: body.clone(),
                        len,
                    },
                ),
            }
        }
        next.docs = Arc::new(docs);

        next.removed_recent.extend(changed.iter().cloned());
        next.removed_recent.sort();
        next.removed_recent.dedup();
        if next.removed_recent.len() > RECENT_CAP {
            let mut rm = (*next.removed).clone();
            rm.extend(next.removed_recent.drain(..));
            next.removed = Arc::new(rm);
        }
        next.added.sort();
        next.as_of = ts;
        if next.added.len() > FOLD_AT {
            next.fold();
        }
        counted!("store.term index caught up");
        Some(next)
    }

    /// Merge the overlay into a fresh base.
    fn fold(&mut self) {
        // COUNTED, NOT `sometimes!`. A fold needs thousands of changes to
        // reach, so declaring it as a reachable state would fail the sim's
        // coverage floor for ever; its reachability is proven instead by
        // `a_catch_up_over_the_fold_threshold_declines_to_a_rebuild` and by
        // the equals-a-rebuild contract.
        counted!("fulltext.overlay folded");

        // A MERGE, NOT A SORT — both inputs are already ordered, the base
        // because every fold leaves it so and the overlay because
        // `with_changes` sorts it before testing the threshold. See
        // `crate::trigram`'s fold for the measurement that motivated this;
        // the two structures have the same shape and the same cost.
        let base = Arc::clone(&self.entries);
        let added = std::mem::take(&mut self.added);
        let mut entries: Vec<(FieldTerm, BodyKey, u32)> =
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
        entries.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        self.entries = Arc::new(entries);
        self.removed = Arc::new(BTreeSet::new());
        self.removed_recent.clear();
    }
}

/// The Lucene inverse document frequency, `ln(1 + (N - df + 0.5)/(df + 0.5))`.
///
/// **NOT the classic `ln((N - df + 0.5)/(df + 0.5))`**, which goes NEGATIVE
/// once a term appears in more than half the corpus. Two things break there. A
/// negative contribution PENALISES a document for containing a query term, so a
/// document matching more of the query can rank below one matching less — which
/// is not a ranking anyone can explain. And it makes an upper-bound-based
/// top-k silently wrong rather than merely slow, because a bound is only a
/// bound while no later term can subtract from it.
///
/// Clamping the classic form at zero was also rejected: it makes the score
/// non-injective in `df` and manufactures ties that then reorder on the
/// tie-break rather than on relevance.
///
/// The `1 +` puts the argument at or above 1 for every `0 <= df <= N`, so this
/// is non-negative by construction and still strictly decreasing in `df`.
#[must_use]
pub fn idf(n_docs: u64, df: u64) -> f64 {
    let n = n_docs as f64;
    let d = df as f64;
    (1.0 + (n - d + 0.5) / (d + 0.5)).ln()
}

/// The saturating frequency factor, `tf(k1+1) / (tf + k1(1 - b + b·dl/avgdl))`.
#[must_use]
pub fn tf_factor(tf: u32, dl: u32, avgdl: f64, k1: f64, b: f64) -> f64 {
    if tf == 0 {
        return 0.0;
    }
    let tf = f64::from(tf);
    // An avgdl of zero means nothing carries this field, in which case no
    // document can have a non-zero tf either — but the guard is here rather
    // than assumed, because a division by zero would become a NaN that
    // propagates silently through the sort.
    let norm = if avgdl > 0.0 {
        1.0 - b + b * (f64::from(dl) / avgdl)
    } else {
        1.0 - b
    };
    tf * (k1 + 1.0) / (tf + k1 * norm)
}

/// A query, planned once.
///
/// The tokens are kept IN ORDER AND WITH MULTIPLICITY, so `'rust rust'` scores
/// twice — as both the term-frequency path it replaces and a Lucene boolean
/// query do.
pub struct QueryPlan {
    /// The analysed query tokens.
    pub tokens: Vec<String>,
    /// `[field][token position]`.
    idf: Vec<Vec<f64>>,
    /// `[field]`.
    avgdl: Vec<f64>,
    k1: f64,
    b: f64,
}

impl QueryPlan {
    /// Plan `tokens` against a corpus and a document-frequency lookup.
    ///
    /// The lookup is a callback so that the index arm and the fallback scan
    /// build the plan from the SAME function with different sources — which is
    /// what lets a differential test compare their scores bit for bit.
    pub fn new(
        fields: usize,
        tokens: Vec<String>,
        corpus: &Corpus,
        df_of: &mut dyn FnMut(FieldId, &str) -> u64,
    ) -> QueryPlan {
        let mut idfs = Vec::with_capacity(fields);
        let mut avgdl = Vec::with_capacity(fields);
        for f in 0..fields {
            let fid = f as FieldId;
            avgdl.push(corpus.avgdl(fid));
            idfs.push(
                tokens
                    .iter()
                    .map(|t| idf(corpus.docs, df_of(fid, t)))
                    .collect(),
            );
        }
        QueryPlan {
            tokens,
            idf: idfs,
            avgdl,
            k1: K1,
            b: B,
        }
    }

    /// **THE ONE SUMMATION.** `tfs[field][token]`, `dl[field]`.
    ///
    /// Fields in declaration order, then tokens in query position. Both arms
    /// call this, so both execute the identical sequence of `f64` additions and
    /// a differential test can compare exactly rather than with an epsilon —
    /// an epsilon large enough to absorb a reordering is large enough to stop
    /// catching a real scoring change.
    #[must_use]
    pub fn score(&self, tfs: &[Vec<u32>], dl: &[u32]) -> f64 {
        let mut acc = 0.0;
        for f in 0..self.avgdl.len() {
            for i in 0..self.tokens.len() {
                let tf = tfs.get(f).and_then(|r| r.get(i)).copied().unwrap_or(0);
                if tf == 0 {
                    continue;
                }
                let d = dl.get(f).copied().unwrap_or(0);
                acc += self.idf[f][i] * tf_factor(tf, d, self.avgdl[f], self.k1, self.b);
            }
        }
        acc
    }
}

/// Decode a tagged property value as a string, if it is one.
fn string_of_tagged(tagged: &[u8]) -> Option<String> {
    let tag = Tag::from_byte(*tagged.first()?);
    if tag != Tag::STRING {
        return None;
    }
    let len = u32::from_le_bytes(tagged.get(1..5)?.try_into().ok()?) as usize;
    String::from_utf8(tagged.get(5..5 + len)?.to_vec()).ok()
}
