//! The signature-homogeneous columnar layout — the census's head/tail split.
//!
//! Production measured 935 distinct property-set signatures over 1.78M nodes
//! with the top 50 covering 93.4% of rows (the columnar gate, 2026-08-19).
//! That distribution is what this module exploits: within a compacted
//! segment, rows sharing one property-set signature store as COLUMN BLOCKS —
//! one contiguous byte run per property — while the long tail of rare
//! signatures stays in row form. Reading one property across a block touches
//! that column's bytes and nothing else (the no-transpose property), and the
//! split is decided per segment by observed row counts, never by a schema.
//!
//! # What may enter a block, and why the rules are strict
//!
//! A row qualifies only when ALL of these hold:
//!
//!  - **its chain is a single live version** — multi-version chains carry
//!    MVCC history that a flat column cannot express; they stay rows until
//!    compaction retires the history;
//!  - **it is plaintext** — sealed ciphertext is opaque bytes under the
//!    crypto layer's rules, and a layout that decoded it would be wrong twice;
//!  - **its bytes are the CANONICAL record encoding** — reconstruction from
//!    columns re-encodes in property-id order, so a non-canonical original
//!    (unsorted ids, however it got there) would silently change bytes on the
//!    way through. Build time VERIFIES decode→encode equals the original and
//!    keeps mismatches in row form, counted. Byte identity is a checked
//!    invariant here, not an assumption.
//!
//! Non-record rows (adjacency, membership, index postings — empty or foreign
//! values) fail record decode and self-exclude. No partition list to fall
//! behind.
//!
//! # Nothing here is a frozen format
//!
//! Blocks are an in-memory layout of a REBUILDABLE artifact (compaction can
//! always re-derive them from chains; the log stays the durable history).
//! The on-disk block format — the irreversible half — waits for the E6/E8
//! experiments the plan gates it on.

use std::collections::BTreeMap;

use engram_observe::{counted, sometimes};

use crate::record::Record;
use crate::{LogicalKey, Version};

/// Rows sharing (partition prefix, signature) below this count stay in row
/// form. The census's tail — ~885 signatures holding 6.6% of rows — sits
/// almost entirely under it, and a three-row column block would cost more
/// structure than it removes.
pub const COLUMNAR_MIN_ROWS: usize = 64;

/// One column: every row's tagged value bytes for a single property.
#[derive(Debug)]
pub(crate) enum Column {
    /// Every value has the same byte length — ints, floats, bools.
    Fixed { width: usize, bytes: Vec<u8> },
    /// Variable-length values behind a prefix-sum offset table.
    Var { offsets: Vec<u32>, bytes: Vec<u8> },
}

impl Column {
    pub(crate) fn get(&self, row: usize) -> &[u8] {
        match self {
            Column::Fixed { width, bytes } => &bytes[row * width..(row + 1) * width],
            Column::Var { offsets, bytes } => {
                &bytes[offsets[row] as usize..offsets[row + 1] as usize]
            }
        }
    }
}

/// Rows of one (partition prefix, property-set signature), stored by column.
#[derive(Debug)]
pub(crate) struct ColumnBlock {
    /// The encoded key prefix (realm │ ns │ kind │ partition) all rows share.
    pub(crate) prefix: Vec<u8>,
    /// Sorted property ids — the signature. Column `j` holds `signature[j]`.
    pub(crate) signature: Vec<u32>,
    /// Full logical keys, ascending. Row `i` is `keys[i]`.
    pub(crate) keys: Vec<LogicalKey>,
    /// Per-row commit timestamps (single live version each).
    pub(crate) commit_ts: Vec<u64>,
    /// One column per signature entry.
    pub(crate) columns: Vec<Column>,
}

impl ColumnBlock {
    /// Row count.
    pub(crate) fn rows(&self) -> usize {
        self.keys.len()
    }

    /// The row index for a key, if this block holds it.
    pub(crate) fn row_of(&self, key: &[u8]) -> Option<usize> {
        self.keys.binary_search_by(|k| k.as_slice().cmp(key)).ok()
    }

    /// Reconstruct row `i`'s record bytes — canonical encoding, which build
    /// time verified equals the original bytes.
    pub(crate) fn value_at(&self, row: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.signature.len() as u32).to_le_bytes());
        for (j, id) in self.signature.iter().enumerate() {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(self.columns[j].get(row));
        }
        out
    }

    /// Reconstruct row `i` as a `Version` (always live plaintext — the entry
    /// rules admit nothing else).
    pub(crate) fn version_at(&self, row: usize) -> Version {
        Version {
            commit_ts: self.commit_ts[row],
            value: Some(self.value_at(row).into()),
            sealed: false,
        }
    }

    /// Index of a property in the signature, if present.
    pub(crate) fn column_of(&self, prop: u32) -> Option<usize> {
        self.signature.binary_search(&prop).ok()
    }

    /// Row indexes whose keys fall in `[lo, hi)`.
    pub(crate) fn rows_in_range(&self, lo: &[u8], hi: Option<&[u8]>) -> std::ops::Range<usize> {
        let start = self.keys.partition_point(|k| k.as_slice() < lo);
        let end = match hi {
            Some(h) => self.keys.partition_point(|k| k.as_slice() < h),
            None => self.keys.len(),
        };
        start..end.max(start)
    }
}

/// Build column blocks from compacted chains, REMOVING blocked keys from
/// `entries`. Only groups reaching `min_rows` block; everything else stays.
///
/// Two passes, deliberately: pass 1 groups KEYS by signature and retains
/// nothing else; pass 2 re-decodes one group at a time, freeing each
/// group's row values as its block is built. The first version held every
/// qualifying row's decoded Record across the whole build — on the full
/// production port that was an extra ~3.5 GB at exactly the moment the
/// heap was largest, and the OOM killer explained the design error.
/// Whether record bytes are the CANONICAL encoding — checked WITHOUT
/// re-encoding. Canonical means exactly: the declared count matches, the
/// property ids appear in strictly ascending order, every value obeys the
/// skip rule, and nothing trails. `Record::decode` + `encode` proves the
/// same thing at the cost of a full re-encoding allocation per row; on the
/// production compaction that was 7M throwaway buffers.
fn is_canonical(bytes: &[u8]) -> bool {
    let Some(count) = bytes
        .get(0..4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
    else {
        return false;
    };
    let mut at = 4usize;
    let mut prev: Option<u32> = None;
    for _ in 0..count {
        let Some(id) = bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        else {
            return false;
        };
        if let Some(p) = prev {
            if id <= p {
                return false; // out of order or duplicate
            }
        }
        prev = Some(id);
        at += 4;
        let Some(len) = engram_key::value::skip_value(&bytes[at.min(bytes.len())..]) else {
            return false;
        };
        at += len;
        if at > bytes.len() {
            return false;
        }
    }
    at == bytes.len()
}

pub(crate) fn build_blocks(
    entries: &mut BTreeMap<LogicalKey, Vec<Version>>,
    min_rows: usize,
) -> Vec<ColumnBlock> {
    // ── Pass 1: qualify and group keys. Nothing heavy retained. ────────
    let mut groups: BTreeMap<(Vec<u8>, Vec<u32>), Vec<LogicalKey>> = BTreeMap::new();
    for (key, chain) in entries.iter() {
        if key.len() < engram_key::PREFIX_LEN || chain.len() != 1 {
            continue;
        }
        let v = &chain[0];
        if v.sealed {
            continue;
        }
        let Some(bytes) = &v.value else { continue };
        // Non-records (adjacency, membership, postings) fail decode here and
        // stay rows — no partition list to maintain.
        let Ok(rec) = Record::decode(bytes) else {
            continue;
        };
        if !is_canonical(bytes) {
            // Non-canonical original: reconstruction would change bytes.
            sometimes!(
                "store.columnar kept a non-canonical row out of a block",
                true
            );
            continue;
        }
        let prefix = key[..engram_key::PREFIX_LEN].to_vec();
        let signature: Vec<u32> = rec.iter().map(|(id, _)| id.0).collect();
        groups
            .entry((prefix, signature))
            .or_default()
            .push(key.clone());
    }

    // ── Pass 2: one group at a time — build columns, then free the rows.
    let mut blocks = Vec::new();
    for ((prefix, signature), keys) in groups {
        if keys.len() < min_rows {
            continue; // the tail stays in row form
        }
        let mut commit_ts = Vec::with_capacity(keys.len());
        let mut builders: Vec<ColBuilder> = signature.iter().map(|_| ColBuilder::new()).collect();
        for key in &keys {
            let chain = entries.get(key).expect("grouped key present");
            let v = &chain[0];
            let bytes = v.value.as_ref().expect("qualified as live");
            let rec = Record::decode(bytes).expect("qualified as canonical");
            commit_ts.push(v.commit_ts);
            for (j, id) in signature.iter().enumerate() {
                builders[j].push(rec.get(crate::record::PropertyId(*id)).expect("signature"));
            }
        }
        let columns: Vec<Column> = builders.into_iter().map(ColBuilder::finish).collect();
        counted!("store.columnar blocks built");
        for key in &keys {
            entries.remove(key);
            counted!("store.columnar rows blocked");
        }
        blocks.push(ColumnBlock {
            prefix,
            signature,
            keys,
            commit_ts,
            columns,
        });
        sometimes!("store.columnar built a head block", true);
    }
    blocks
}

/// Streaming column assembly: bytes accumulate once, and the fixed/var
/// decision falls out at the end instead of requiring a second pass.
struct ColBuilder {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
    fixed_width: Option<usize>,
    rows: usize,
}

impl ColBuilder {
    fn new() -> Self {
        ColBuilder {
            offsets: vec![0],
            bytes: Vec::new(),
            fixed_width: Some(0),
            rows: 0,
        }
    }

    fn push(&mut self, v: &[u8]) {
        if self.rows == 0 {
            self.fixed_width = Some(v.len());
        } else if self.fixed_width != Some(v.len()) {
            self.fixed_width = None;
        }
        self.bytes.extend_from_slice(v);
        self.offsets.push(self.bytes.len() as u32);
        self.rows += 1;
    }

    fn finish(self) -> Column {
        match self.fixed_width {
            Some(w) if w > 0 => Column::Fixed {
                width: w,
                bytes: self.bytes,
            },
            _ => Column::Var {
                offsets: self.offsets,
                bytes: self.bytes,
            },
        }
    }
}
