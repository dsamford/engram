//! M4 — the vector index, exactly as X2 measured it.
//!
//! X2 ran against production (19,782 × 1024-d, 200 held-out queries, exact
//! brute-force ground truth): **int8 scan + f32 rescore at oversample 2 gave
//! recall@10 = 1.00000, 200/200 queries perfect, 4× scan saving** — and the
//! binary prefilter was measured as a TRADE (capped at 0.999 recall however
//! much you oversample) and dropped. This module is that design and no more:
//! quantized scan, exact rescore, no prefilter, with the design's own
//! conditions carried on every answer.
//!
//! # Why exact scan and not HNSW yet
//!
//! M0 measured the growth: ANN is a BASE requirement because a partition's
//! size is bounded by one tenant's volume, not by time. But the segment
//! interface is the commitment — `search` takes a query and returns ranked
//! bodies with a report — and an HNSW build slots behind it without changing
//! a caller. At the current corpus (~34k embedded nodes) the exact scan is
//! measured fine; shipping a graph index before the interface exists would be
//! optimizing ahead of the seam.
//!
//! # The conditions travel with the answer
//!
//! X2's own write-up says a recall of 1.000 is uninterpretable without the
//! margin/error ratio beside it. The per-answer equivalent: every answer
//! carries `as_of`, how many vectors were scanned, how many rescored, and how
//! many rows were unindexable — so a perfect-looking result over a half-built
//! index cannot read as a census.

use engram_key::KeyPrefix;
use engram_key::value::Tag;
use engram_observe::{counted, sometimes};

use crate::Store;
use crate::record::{PropertyId, get_property};

/// X2's oversample: the int8 scan keeps `k × OVERSAMPLE` candidates for the
/// f32 rescore. 2 was measured sufficient for recall 1.0 at k=10.
pub const OVERSAMPLE: usize = 2;

/// One indexed vector.
#[derive(Debug, Clone)]
struct Entry {
    body: Vec<u8>,
    /// Unit-normalized f32s — the rescore side. Cosine over unit vectors is a
    /// dot product, so normalization happens ONCE, at build.
    f32s: Vec<f32>,
    /// Per-vector max-abs int8 quantization — the scan side.
    i8s: Vec<i8>,
    /// The entry's quantization scale (max-abs / 127). The scan MUST multiply
    /// by this: raw i8 dots across entries with different scales are
    /// incomparable numbers, and ranking them raw systematically distorts
    /// whichever entries have unusual max components. Found by the
    /// self-calibrating brute-force test — a hardcoded error band had hidden
    /// it, and the measured band could not.
    scale: f32,
}

/// The derived vector index.
#[derive(Debug, Clone)]
pub struct VectorIndex {
    property: PropertyId,
    dim: usize,
    entries: Vec<Entry>,
    as_of: u64,
    /// Rows whose property existed but could not be indexed: wrong tag, wrong
    /// dimension, zero norm, NaN. Counted, carried on every answer.
    unindexable: u64,
}

/// A ranked search answer, with the conditions that make it interpretable.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchAnswer {
    /// `(body, cosine)` best-first, exact f32 scores.
    pub hits: Vec<(Vec<u8>, f32)>,
    /// The snapshot the index describes.
    pub as_of: u64,
    /// Vectors int8-scanned.
    pub scanned: usize,
    /// Candidates exactly rescored.
    pub rescored: usize,
    /// Rows the index could not hold. Non-zero means the ranking is over a
    /// SUBSET of the group's vectors, and the answer says so itself.
    pub unindexable: u64,
}

/// Why a search was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorError {
    /// The query's dimensionality does not match the index's.
    ///
    /// Refused, never truncated or padded: a 512-d query against a 1024-d
    /// index is a DIFFERENT EMBEDDING SPACE, and cosines across spaces are
    /// plausible numbers with no meaning — the incumbent's space-mismatch
    /// lesson, enforced at the seam.
    DimensionMismatch {
        /// The query's dimension.
        query: usize,
        /// The index's dimension.
        index: usize,
    },
    /// The query has zero norm — cosine against it is undefined.
    ZeroQuery,
}

impl std::fmt::Display for VectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VectorError::DimensionMismatch { query, index } => write!(
                f,
                "query dimension {query} vs index dimension {index}: different embedding \
                 spaces — cosines across them are plausible numbers with no meaning"
            ),
            VectorError::ZeroQuery => write!(f, "a zero-norm query has no direction to compare"),
        }
    }
}

impl std::error::Error for VectorError {}

fn decode_f32_vector(tagged: &[u8]) -> Option<Vec<f32>> {
    if tagged.first() != Some(&Tag::VECTOR_F32.byte()) {
        return None;
    }
    // The u32 is the BYTE length — the skip rule's contract. The first
    // version wrote the DIMENSION here, and `skip_value` (which steps by
    // bytes, knowing no element width) mis-walked every record containing a
    // vector: `get_property` then reported the property ABSENT and the index
    // built empty. The vector tests caught it; the dimension is derived.
    let byte_len = u32::from_le_bytes(tagged.get(1..5)?.try_into().ok()?) as usize;
    if byte_len % 4 != 0 {
        return None;
    }
    let payload = tagged.get(5..5 + byte_len)?;
    if tagged.len() != 5 + byte_len {
        return None;
    }
    Some(
        payload
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect(),
    )
}

/// Encode a `VECTOR_F32` tagged value — the write-side helper tests and
/// callers share, so the encoding cannot drift between them.
pub fn encode_f32_vector(v: &[f32]) -> Vec<u8> {
    let mut out = vec![Tag::VECTOR_F32.byte()];
    out.extend_from_slice(&((v.len() * 4) as u32).to_le_bytes());
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    // Per-vector max-abs scale. X2 measured Voyage vectors well-conditioned
    // under exactly this (crest factor 3.33, ~38 of 127 levels used). The
    // scale is RETURNED because the scan needs it: i8 dots are only
    // comparable across entries after multiplying each by its own scale.
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return (vec![0; v.len()], 0.0);
    }
    (
        (v.iter()
            .map(|x| ((x / max) * 127.0).round() as i8)
            .collect()),
        max / 127.0,
    )
}

fn dot_i8(a: &[i8], b: &[i8]) -> i64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| i64::from(*x) * i64::from(*y))
        .sum()
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl VectorIndex {
    /// Build from the group at snapshot `ts`, indexing `property`.
    ///
    /// The dimension is fixed by the FIRST valid vector; later rows with a
    /// different dimension are counted unindexable — one index, one space.
    pub fn build(store: &Store, group: &KeyPrefix, property: PropertyId, ts: u64) -> VectorIndex {
        let mut entries: Vec<Entry> = Vec::new();
        let mut unindexable = 0u64;
        let mut dim: Option<usize> = None;

        for (body, record_bytes) in store.scan_at(group, ts) {
            let Some(tagged) = get_property(&record_bytes, property) else {
                continue;
            };
            let Some(raw) = decode_f32_vector(&tagged) else {
                unindexable += 1;
                sometimes!("vector.row not indexable", true);
                continue;
            };
            if let Some(d) = dim {
                if raw.len() != d {
                    unindexable += 1;
                    sometimes!("vector.row not indexable", true);
                    continue;
                }
            }
            let Some(unit) = normalize(&raw) else {
                unindexable += 1;
                sometimes!("vector.row not indexable", true);
                continue;
            };
            dim.get_or_insert(raw.len());
            let (i8s, scale) = quantize(&unit);
            entries.push(Entry {
                body,
                f32s: unit,
                i8s,
                scale,
            });
        }
        counted!("vector.builds");
        VectorIndex {
            property,
            dim: dim.unwrap_or(0),
            entries,
            as_of: ts,
            unindexable,
        }
    }

    /// The indexed property.
    pub fn property(&self) -> PropertyId {
        self.property
    }

    /// The index's dimension (0 when empty).
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Indexed vectors.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The snapshot this index describes.
    pub fn as_of(&self) -> u64 {
        self.as_of
    }

    /// Top-`k` by cosine: int8 scan keeps `k × OVERSAMPLE` candidates, the f32
    /// rescore ranks them exactly.
    ///
    /// The reported scores are ALWAYS the f32 ones. Returning the quantized
    /// scores would leak the scan's approximation into downstream fusion,
    /// where a score is compared against other sources' scores.
    pub fn search(&self, query: &[f32], k: usize) -> Result<SearchAnswer, VectorError> {
        if self.dim != 0 && query.len() != self.dim {
            return Err(VectorError::DimensionMismatch {
                query: query.len(),
                index: self.dim,
            });
        }
        let unit_q = normalize(query).ok_or(VectorError::ZeroQuery)?;
        let (q_i8, _q_scale) = quantize(&unit_q);

        // Scan: int8 dots RESCALED by each entry's own quantization scale.
        // The query's scale is common to every comparison and drops out of
        // the ordering; the ENTRY scales do not, and ranking raw i8 dots
        // across them compares incomparable numbers.
        let mut scored: Vec<(f32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| ((dot_i8(&q_i8, &e.i8s) as f32) * e.scale, i))
            .collect();
        let keep = (k * OVERSAMPLE).min(scored.len());
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(keep);

        // Rescore: exact f32 over the kept candidates only.
        let mut exact: Vec<(f32, usize)> = scored
            .iter()
            .map(|(_, i)| (dot_f32(&unit_q, &self.entries[*i].f32s), *i))
            .collect();
        exact.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        exact.truncate(k);

        counted!("vector.searches");
        Ok(SearchAnswer {
            hits: exact
                .iter()
                .map(|(s, i)| (self.entries[*i].body.clone(), *s))
                .collect(),
            as_of: self.as_of,
            scanned: self.entries.len(),
            rescored: keep,
            unindexable: self.unindexable,
        })
    }
}
