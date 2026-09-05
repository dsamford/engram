//! The on-disk segment format — Track B (`paged` storage), milestone M0.
//!
//! A sealed [`Segment`] is immutable, so it can be
//! written to disk once and read back forever. This module is that format: a
//! RocksDB-`BlockBasedTable`-shaped layout of **sorted `(key, version-chain)`
//! data blocks**, a **sparse per-block index** (first key → offset), and a
//! **fixed footer**, with a **per-block BLAKE3 trailer** extending the commit
//! log's hash-chain integrity story down to the block — the exact unit M1's
//! block cache will fetch and verify.
//!
//! # Row form on disk, blocks rebuilt in memory
//!
//! The in-memory columnar [`ColumnBlock`](crate::columnar) layout is an
//! orthogonal, *rebuildable* read optimisation (compaction re-derives it from
//! chains; columnar.rs deliberately does not freeze its bytes). So this format
//! serialises the segment's **dissolved row form** — every block row written
//! as its canonical record bytes via
//! [`Segment::cloned_entries`](crate::segment::Segment) — which build time
//! already verified equals the original bytes. A segment written and read back
//! answers `get_at`/`range` identically; only the in-memory row/column split,
//! which compaction rebuilds, differs. That is the M0 gate: **seal → disk →
//! reopen is result-byte-identical**, `resident` untouched.
//!
//! # Integrity, and how corruption surfaces
//!
//! Each data block carries `BLAKE3(payload)`; the footer carries
//! `BLAKE3(footer fields)`. A flipped byte fails verification at the exact
//! block that reads it (or at open, for the footer/index) — never a silent
//! wrong answer. This is the disk end of the same hash discipline the log
//! keeps in memory.

use std::collections::BTreeMap;

use crate::segment::Segment;
use crate::{LogicalKey, Version};

/// Format identifier in the footer. Bump on any incompatible layout change.
const MAGIC: [u8; 8] = *b"ENGRSEG1";
/// Current on-disk format version.
///
/// v2 added `max_commit_ts`. v3 adds `tombstones` + `versions`, so a delete-
/// aware compaction trigger can read a PAGED segment's tombstone density
/// without opening it — the resident half of that trigger had to under-report
/// until this existed.
///
/// **v3 is ADDITIVE and v2 files are still read.** The two fields are appended
/// before `format_version`, and version/magic sit at fixed offsets from the
/// END, so a reader identifies the version before it decides how much footer to
/// parse. Existing paged stores keep serving and existing measurement baselines
/// stay re-runnable — which is the whole reason for doing it this way rather
/// than rewriting the layout.
const FORMAT_VERSION: u32 = 3;
/// v2 footer: index_offset(8) + index_len(8) + seq(8) + entry_count(8) +
/// max_commit_ts(8) + format_version(4) + magic(8) + BLAKE3(32).
pub(crate) const FOOTER_LEN_V2: usize = 8 + 8 + 8 + 8 + 8 + 4 + 8 + 32;
/// v3 footer: v2 plus tombstones(8) + versions(8).
pub(crate) const FOOTER_LEN_V3: usize = FOOTER_LEN_V2 + 8 + 8;
/// What a `paged` open `pread`s from the file's tail — the LARGEST footer any
/// supported version uses, so one bounded read covers every version.
pub(crate) const FOOTER_LEN: usize = FOOTER_LEN_V3;
/// Bytes from the END of the footer at which `format_version` starts:
/// magic(8) + BLAKE3(32) follow it. Version-independent BY CONSTRUCTION, which
/// is what lets a reader identify the version before parsing the rest.
const VERSION_FROM_END: usize = 4 + 8 + 32;
/// The bytes of a v2 footer the hash covers.
const FOOTER_HASHED_V2: usize = FOOTER_LEN_V2 - 32;
/// The bytes of a v3 footer the hash covers.
const FOOTER_HASHED_V3: usize = FOOTER_LEN_V3 - 32;
/// Retained for callers that only need the current version's hashed span.
const FOOTER_HASHED: usize = FOOTER_HASHED_V3;
/// A data block is closed once its payload reaches this many bytes. A block is
/// the cache/read/verify unit; 16 KiB is the design's mid tier (4 KiB is the
/// point-lookup sweet spot, 64 KiB the cold-scan tier — tuned later, per M1).
const TARGET_BLOCK_BYTES: usize = 16 * 1024;
/// The 32-byte BLAKE3 trailer that follows every data block's payload.
pub(crate) const HASH_LEN: usize = 32;

/// Why reading an on-disk segment failed. Every variant is a HARD error at the
/// point of use — an unreadable segment is never a silently empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SstError {
    /// The buffer is shorter than the fixed footer, or a declared region runs
    /// past the end of the buffer.
    Truncated {
        /// What the reader was trying to read when it ran out of bytes.
        what: &'static str,
    },
    /// The footer magic does not match — not an Engram segment, or a newer
    /// incompatible format.
    BadMagic,
    /// The footer declares a `format_version` this build does not understand.
    UnsupportedVersion(u32),
    /// A BLAKE3 check failed: the named region's bytes do not match the stored
    /// hash. The block offset (or `u64::MAX` for the footer) locates it.
    HashMismatch {
        /// Byte offset of the corrupt block's payload, or `u64::MAX` for the
        /// footer.
        at: u64,
    },
    /// A varint or structural field was malformed (e.g. a length that cannot
    /// be represented, or trailing/short block bytes).
    Corrupt {
        /// A short description of the structural fault.
        why: &'static str,
    },
}

// ── unsigned LEB128 varint ──────────────────────────────────────────────────

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Read a varint at `*pos`, advancing it. Fails on truncation or a value that
/// would need more than 10 bytes (an overlong / malformed encoding).
fn get_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64, SstError> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *buf
            .get(*pos)
            .ok_or(SstError::Truncated { what: "varint" })?;
        *pos += 1;
        if shift >= 64 {
            return Err(SstError::Corrupt {
                why: "overlong varint",
            });
        }
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Read exactly `n` bytes at `*pos`, advancing it.
fn take<'a>(
    buf: &'a [u8],
    pos: &mut usize,
    n: usize,
    what: &'static str,
) -> Result<&'a [u8], SstError> {
    let end = pos.checked_add(n).ok_or(SstError::Corrupt {
        why: "length overflow",
    })?;
    let slice = buf.get(*pos..end).ok_or(SstError::Truncated { what })?;
    *pos = end;
    Ok(slice)
}

fn read_u64(buf: &[u8], pos: &mut usize, what: &'static str) -> Result<u64, SstError> {
    let b = take(buf, pos, 8, what)?;
    Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

fn read_u32(buf: &[u8], pos: &mut usize, what: &'static str) -> Result<u32, SstError> {
    let b = take(buf, pos, 4, what)?;
    Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

// ── entry (one key's version chain) encode/decode ───────────────────────────

fn encode_entry(out: &mut Vec<u8>, key: &LogicalKey, versions: &[Version]) {
    put_uvarint(out, key.len() as u64);
    out.extend_from_slice(key);
    put_uvarint(out, versions.len() as u64);
    for v in versions {
        out.extend_from_slice(&v.commit_ts.to_le_bytes());
        // flags: bit0 = has value (else tombstone), bit1 = sealed.
        let mut flags = 0u8;
        if v.value.is_some() {
            flags |= 0b01;
        }
        if v.sealed {
            flags |= 0b10;
        }
        out.push(flags);
        if let Some(val) = &v.value {
            put_uvarint(out, val.len() as u64);
            out.extend_from_slice(val);
        }
    }
}

fn decode_entry(buf: &[u8], pos: &mut usize) -> Result<(LogicalKey, Vec<Version>), SstError> {
    let klen = get_uvarint(buf, pos)? as usize;
    let key = take(buf, pos, klen, "entry key")?.to_vec();
    let nver = get_uvarint(buf, pos)? as usize;
    let mut versions = Vec::with_capacity(nver);
    for _ in 0..nver {
        let commit_ts = read_u64(buf, pos, "version ts")?;
        let flags = *take(buf, pos, 1, "version flags")?.first().expect("1 byte");
        let value = if flags & 0b01 != 0 {
            let vlen = get_uvarint(buf, pos)? as usize;
            Some(take(buf, pos, vlen, "version value")?.to_vec())
        } else {
            None
        };
        let sealed = flags & 0b10 != 0;
        versions.push(Version {
            commit_ts,
            value: value.map(std::sync::Arc::from),
            sealed,
        });
    }
    Ok((key, versions))
}

// ── write ───────────────────────────────────────────────────────────────────

/// Builds a segment file INCREMENTALLY from keys fed in ascending order.
///
/// Why it exists: `write_segment` takes a `&Segment`, so producing a MERGED
/// segment file meant materialising the whole merge in RAM first. That is the
/// one allocation a bigger-than-RAM store cannot make, and it is why compaction
/// was never called on the paged path at all — leaving tombstones there
/// unreclaimable for the life of the process, however dense they became.
///
/// `write_segment` below is this writer plus a loop, deliberately: the
/// streaming compactor and the whole-segment writer then cannot drift into
/// producing different bytes, because there is only one implementation.
pub(crate) struct SegmentWriter {
    out: Vec<u8>,
    /// `(first_key, payload_offset, payload_len)` per closed data block.
    index: Vec<(LogicalKey, u64, u64)>,
    block: Vec<u8>,
    block_first: Option<LogicalKey>,
    entries: u64,
    max_commit_ts: u64,
    tombstones: u64,
    versions: u64,
    /// Guards the ascending-key contract. Feeding keys out of order produces a
    /// file whose sparse index LIES, and every read through it is then wrong in
    /// a way no hash check can catch — the block hashes are all correct.
    last_key: Option<LogicalKey>,
}

impl SegmentWriter {
    pub(crate) fn new() -> SegmentWriter {
        SegmentWriter {
            out: Vec::new(),
            index: Vec::new(),
            block: Vec::new(),
            block_first: None,
            entries: 0,
            max_commit_ts: 0,
            tombstones: 0,
            versions: 0,
            last_key: None,
        }
    }

    fn flush_block(&mut self) {
        if self.block.is_empty() {
            return;
        }
        let offset = self.out.len() as u64;
        let len = self.block.len() as u64;
        let hash = blake3::hash(&self.block);
        self.out.extend_from_slice(&self.block);
        self.out.extend_from_slice(hash.as_bytes());
        self.index.push((
            self.block_first
                .take()
                .expect("a non-empty block has a first key"),
            offset,
            len,
        ));
        self.block.clear();
    }

    /// Append one key's version chain. Keys MUST arrive in ascending order.
    pub(crate) fn push(&mut self, key: &LogicalKey, versions: &[Version]) {
        debug_assert!(
            self.last_key.as_ref().is_none_or(|k| k < key),
            "SegmentWriter requires ascending keys"
        );
        self.last_key = Some(key.clone());
        if self.block_first.is_none() {
            self.block_first = Some(key.clone());
        }
        for v in versions {
            self.versions += 1;
            if v.value.is_none() {
                self.tombstones += 1;
            }
            self.max_commit_ts = self.max_commit_ts.max(v.commit_ts);
        }
        self.entries += 1;
        encode_entry(&mut self.block, key, versions);
        if self.block.len() >= TARGET_BLOCK_BYTES {
            self.flush_block();
        }
    }

    /// Close the file: flush the last block, write the sparse index, stamp the
    /// footer (including the v3 tombstone counts accumulated above).
    pub(crate) fn finish(mut self, seq: u64) -> Vec<u8> {
        self.flush_block();
        // Sparse index section: n_blocks, then (first_key, offset, len)*.
        let index_offset = self.out.len() as u64;
        put_uvarint(&mut self.out, self.index.len() as u64);
        for (first, offset, len) in &self.index {
            put_uvarint(&mut self.out, first.len() as u64);
            self.out.extend_from_slice(first);
            self.out.extend_from_slice(&offset.to_le_bytes());
            self.out.extend_from_slice(&len.to_le_bytes());
        }
        let index_len = self.out.len() as u64 - index_offset;

        // Fixed footer, ending in its own hash.
        let footer_start = self.out.len();
        self.out.extend_from_slice(&index_offset.to_le_bytes());
        self.out.extend_from_slice(&index_len.to_le_bytes());
        self.out.extend_from_slice(&seq.to_le_bytes());
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&self.max_commit_ts.to_le_bytes());
        self.out.extend_from_slice(&self.tombstones.to_le_bytes());
        self.out.extend_from_slice(&self.versions.to_le_bytes());
        self.out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        self.out.extend_from_slice(&MAGIC);
        let footer_hash = blake3::hash(&self.out[footer_start..footer_start + FOOTER_HASHED]);
        self.out.extend_from_slice(footer_hash.as_bytes());
        self.out
    }
}

/// Serialise a sealed segment to the on-disk format. Row form: every block row
/// is written as its canonical record bytes (via `cloned_entries`), so the
/// bytes are self-contained and the reader needs no columnar machinery.
///
/// This is `SegmentWriter` plus a loop — so the streaming compactor and this
/// path cannot produce different bytes for the same content.
pub fn write_segment(seg: &Segment) -> Vec<u8> {
    let entries = seg.cloned_entries();
    let mut w = SegmentWriter::new();
    for (key, versions) in &entries {
        w.push(key, versions);
    }
    w.finish(seg.seq)
}

// ── read ────────────────────────────────────────────────────────────────────

/// The parsed fixed footer — the resident anchor from which every block is
/// located. Small and cheap to keep in RAM even in `paged` mode.
pub(crate) struct SegmentFooter {
    pub(crate) index_offset: u64,
    pub(crate) index_len: u64,
    pub(crate) seq: u64,
    pub(crate) entry_count: u64,
    /// The greatest commit timestamp any version in the segment carries — the
    /// clock a store opened from this file must advance past.
    pub(crate) max_commit_ts: u64,
    /// Tombstoned versions (v3+; `0` for a v2 file, which did not record it).
    pub(crate) tombstones: u64,
    /// Total versions (v3+; `0` for a v2 file). Zero means "this file cannot
    /// say", NOT "this file holds nothing" — a reader must treat it as absent
    /// rather than as a zero ratio.
    pub(crate) versions: u64,
}

/// One data block's placement: the first key it holds (the sparse-index search
/// key) and its payload `(offset, len)` in the segment bytes/file. The 32-byte
/// BLAKE3 trailer immediately follows the payload.
#[derive(Clone)]
pub(crate) struct BlockHandle {
    pub(crate) first_key: LogicalKey,
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

/// Read and verify the fixed footer. Its hash is checked before any field is
/// trusted, so a corrupt footer fails here rather than misdirecting a read.
pub(crate) fn read_footer(buf: &[u8]) -> Result<SegmentFooter, SstError> {
    // ORDER MATTERS, and getting it wrong changes what a failure is CALLED.
    //
    //   1. Too short for even the smallest supported footer -> Truncated.
    //   2. Wrong magic -> BadMagic. "Is this an engram segment at all?" must be
    //      answered before "which version is it?", or a random file reports as
    //      an unsupported VERSION, which sends a reader looking for an upgrade
    //      instead of for the wrong file.
    //   3. Only then the version, which decides how much footer to parse.
    //
    // Both magic and version sit at fixed offsets from the END (magic + hash
    // follow the version), which is exactly what lets one reader serve v2 and
    // v3 — and is why the version was placed there rather than at the front.
    if buf.len() < FOOTER_LEN_V2 {
        return Err(SstError::Truncated { what: "footer" });
    }
    let magic_at = buf.len() - 8 - 32;
    if buf[magic_at..magic_at + 8] != MAGIC {
        return Err(SstError::BadMagic);
    }
    let vpos = buf.len() - VERSION_FROM_END;
    let format_version =
        u32::from_le_bytes(buf[vpos..vpos + 4].try_into().expect("4 bytes"));
    let (flen, hashed) = match format_version {
        2 => (FOOTER_LEN_V2, FOOTER_HASHED_V2),
        3 => (FOOTER_LEN_V3, FOOTER_HASHED_V3),
        v => return Err(SstError::UnsupportedVersion(v)),
    };
    if buf.len() < flen {
        return Err(SstError::Truncated { what: "footer" });
    }
    let footer = &buf[buf.len() - flen..];
    let stored = &footer[hashed..];
    if blake3::hash(&footer[..hashed]).as_bytes() != stored {
        return Err(SstError::HashMismatch { at: u64::MAX });
    }
    let mut fp = 0usize;
    let index_offset = read_u64(footer, &mut fp, "footer index_offset")?;
    let index_len = read_u64(footer, &mut fp, "footer index_len")?;
    let seq = read_u64(footer, &mut fp, "footer seq")?;
    let entry_count = read_u64(footer, &mut fp, "footer entry_count")?;
    let max_commit_ts = read_u64(footer, &mut fp, "footer max_commit_ts")?;
    // v2 simply did not record these; 0/0 means "cannot say", and every reader
    // must treat that as ABSENT rather than as a zero ratio.
    let (tombstones, versions) = if format_version >= 3 {
        let t = read_u64(footer, &mut fp, "footer tombstones")?;
        let v = read_u64(footer, &mut fp, "footer versions")?;
        (t, v)
    } else {
        (0, 0)
    };
    let _ = read_u32(footer, &mut fp, "footer format_version")?;
    let magic = take(footer, &mut fp, 8, "footer magic")?;
    if magic != MAGIC {
        return Err(SstError::BadMagic);
    }
    Ok(SegmentFooter {
        index_offset,
        index_len,
        seq,
        entry_count,
        max_commit_ts,
        tombstones,
        versions,
    })
}

/// Read the sparse index — one [`BlockHandle`] per data block, in key order.
/// `index_region` is the `[index_offset, index_offset+index_len)` slice.
pub(crate) fn read_index(index_region: &[u8]) -> Result<Vec<BlockHandle>, SstError> {
    let mut ip = 0usize;
    let nblocks = get_uvarint(index_region, &mut ip)?;
    let mut handles = Vec::with_capacity(nblocks as usize);
    for _ in 0..nblocks {
        let klen = get_uvarint(index_region, &mut ip)? as usize;
        let first_key = take(index_region, &mut ip, klen, "index first key")?.to_vec();
        let offset = read_u64(index_region, &mut ip, "index block offset")?;
        let len = read_u64(index_region, &mut ip, "index block len")?;
        handles.push(BlockHandle {
            first_key,
            offset,
            len,
        });
    }
    Ok(handles)
}

/// The `[index_offset, index_offset+index_len)` slice of `buf`, bounds-checked.
pub(crate) fn index_region<'a>(buf: &'a [u8], f: &SegmentFooter) -> Result<&'a [u8], SstError> {
    let io = f.index_offset as usize;
    let il = f.index_len as usize;
    let end = io.checked_add(il).ok_or(SstError::Corrupt {
        why: "index region overflow",
    })?;
    buf.get(io..end).ok_or(SstError::Truncated {
        what: "index region",
    })
}

/// Verify a block's BLAKE3 and decode its entries. `frame` is the block's
/// `payload || 32-byte hash`, exactly `handle.len + 32` bytes. Returns the
/// entries in key order (as stored). A hash mismatch names the block offset.
pub(crate) fn verify_and_decode_block(
    frame: &[u8],
    offset: u64,
) -> Result<Vec<(LogicalKey, Vec<Version>)>, SstError> {
    if frame.len() < HASH_LEN {
        return Err(SstError::Truncated { what: "data block" });
    }
    let (payload, stored) = frame.split_at(frame.len() - HASH_LEN);
    if blake3::hash(payload).as_bytes() != stored {
        return Err(SstError::HashMismatch { at: offset });
    }
    let mut out = Vec::new();
    let mut bp = 0usize;
    while bp < payload.len() {
        out.push(decode_entry(payload, &mut bp)?);
    }
    // A block lives in the cache for as long as it is hot; the push-growth
    // slack (up to one extra tuple per entry, 48 bytes) would live there with
    // it. One realloc here is nothing next to the BLAKE3 above.
    out.shrink_to_fit();
    Ok(out)
}

/// Parse an on-disk segment back into an in-memory (row-form) [`Segment`],
/// verifying every block's and the footer's BLAKE3. M0 loads the whole segment
/// resident; the [`crate::paged`] reader resolves blocks individually through
/// the block cache using the SAME footer + index + verification.
pub fn read_segment(buf: &[u8]) -> Result<Segment, SstError> {
    let footer = read_footer(buf)?;
    let handles = read_index(index_region(buf, &footer)?)?;

    let mut entries: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
    for h in handles {
        let off = h.offset as usize;
        let frame_len = (h.len as usize)
            .checked_add(HASH_LEN)
            .ok_or(SstError::Corrupt {
                why: "block frame overflow",
            })?;
        let frame_end = off.checked_add(frame_len).ok_or(SstError::Corrupt {
            why: "block region overflow",
        })?;
        let frame = buf
            .get(off..frame_end)
            .ok_or(SstError::Truncated { what: "data block" })?;
        for (key, versions) in verify_and_decode_block(frame, h.offset)? {
            if entries.insert(key, versions).is_some() {
                return Err(SstError::Corrupt {
                    why: "duplicate key across blocks",
                });
            }
        }
    }

    if entries.len() as u64 != footer.entry_count {
        return Err(SstError::Corrupt {
            why: "entry_count disagrees with decoded entries",
        });
    }

    Ok(Segment::new(footer.seq, entries))
}

// ── file I/O (SegmentWriter / SegmentReader) ────────────────────────────────

/// Reading a segment file failed — either the OS read, or the format.
#[derive(Debug)]
pub enum ReadFileError {
    /// The file could not be read (missing, permissions, short read).
    Io(std::io::Error),
    /// The bytes were read but do not parse / verify as a segment.
    Format(SstError),
}

impl std::fmt::Display for ReadFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadFileError::Io(e) => write!(f, "segment file I/O: {e}"),
            ReadFileError::Format(e) => write!(f, "segment format: {e:?}"),
        }
    }
}

impl std::error::Error for ReadFileError {}

/// Seal a segment to `path` in the on-disk format. Writes to a sibling
/// `.tmp` and atomically renames, so a reader (or a crash) never observes a
/// half-written, hash-failing segment — the durable analogue of the sealed
/// segment's immutability.
pub fn write_segment_file(seg: &Segment, path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = write_segment(seg);
    let tmp = path.with_extension("tmp");
    // `fsync` the bytes BEFORE the rename and the directory AFTER it: a
    // segment file is the durable home of every row the WAL checkpoint
    // drops behind it, so it must be on the platter — not merely in the
    // page cache — before that checkpoint can run. `fs::write` + `rename`
    // alone survived a process crash but not a power loss.
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    engram_log::sync_parent_dir(path)
}

/// Read and fully verify a segment written by [`write_segment_file`]. M0
/// loads the whole file resident and reconstructs the in-memory segment; M1
/// will resolve individual blocks via `pread` against the same index + BLAKE3.
pub fn read_segment_file(path: &std::path::Path) -> Result<Segment, ReadFileError> {
    let bytes = std::fs::read(path).map_err(ReadFileError::Io)?;
    read_segment(&bytes).map_err(ReadFileError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(ts: u64, val: Option<&[u8]>, sealed: bool) -> Version {
        Version {
            commit_ts: ts,
            value: val.map(std::sync::Arc::from),
            sealed,
        }
    }

    /// A segment with multi-version chains, a tombstone, sealed flags and one
    /// large value (to force >1 data block) — the shapes the format must carry.
    fn sample_entries(big: usize) -> BTreeMap<LogicalKey, Vec<Version>> {
        let mut e: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
        // multi-version chain, oldest first
        e.insert(
            b"alpha".to_vec(),
            vec![v(1, Some(b"a1"), true), v(5, Some(b"a2"), false)],
        );
        // a tombstone as the live version
        e.insert(
            b"beta".to_vec(),
            vec![v(2, Some(b"b1"), true), v(9, None, false)],
        );
        // a single sealed version
        e.insert(b"gamma".to_vec(), vec![v(3, Some(b"g1"), true)]);
        // a large value to push the running block past TARGET_BLOCK_BYTES
        e.insert(
            b"delta".to_vec(),
            vec![v(4, Some(&vec![0xABu8; big]), false)],
        );
        // an empty (zero-length) value — distinct from a tombstone
        e.insert(b"epsilon".to_vec(), vec![v(6, Some(b""), false)]);
        e
    }

    #[test]
    fn roundtrip_is_byte_identical_and_reads_match() {
        let entries = sample_entries(100);
        let seg = Segment::new(7, entries.clone());
        let bytes = write_segment(&seg);
        let back = read_segment(&bytes).expect("clean read");

        assert_eq!(back.seq, 7, "seq survives");
        assert_eq!(
            back.row_entries(),
            &entries,
            "row chains are byte-identical"
        );
        // Reads agree at representative timestamps, including across the
        // tombstone boundary and below the oldest version.
        for key in [
            b"alpha".as_ref(),
            b"beta",
            b"gamma",
            b"delta",
            b"epsilon",
            b"absent",
        ] {
            for ts in [0u64, 1, 2, 3, 4, 5, 6, 9, 100] {
                assert_eq!(
                    seg.get_at(&key.to_vec(), ts),
                    back.get_at(&key.to_vec(), ts),
                    "get_at disagrees for {key:?} @ {ts}"
                );
            }
        }
    }

    #[test]
    fn a_large_segment_spans_multiple_blocks_and_round_trips() {
        // ~200 keys each with a ~256-byte value ≈ 50 KiB > one 16 KiB block.
        let mut entries: BTreeMap<LogicalKey, Vec<Version>> = BTreeMap::new();
        for i in 0..200u32 {
            let key = format!("key-{i:05}").into_bytes();
            entries.insert(key, vec![v(i as u64 + 1, Some(&vec![i as u8; 256]), false)]);
        }
        let seg = Segment::new(3, entries.clone());
        let bytes = write_segment(&seg);
        let back = read_segment(&bytes).expect("clean read");
        assert_eq!(back.row_entries(), &entries, "multi-block chains survive");

        // Prove it actually used more than one block (else this isn't testing
        // the multi-block path). Re-parse the index and count.
        let footer = &bytes[bytes.len() - FOOTER_LEN..];
        let mut fp = 0usize;
        let index_offset = read_u64(footer, &mut fp, "io").unwrap() as usize;
        let index_len = read_u64(footer, &mut fp, "il").unwrap() as usize;
        let mut ip = 0usize;
        let nblocks = get_uvarint(&bytes[index_offset..index_offset + index_len], &mut ip).unwrap();
        assert!(nblocks > 1, "expected multiple data blocks, got {nblocks}");
    }

    #[test]
    fn a_flipped_data_block_byte_is_caught() {
        let seg = Segment::new(1, sample_entries(10));
        let mut bytes = write_segment(&seg);
        // Byte 0 is inside the first data block's payload.
        bytes[0] ^= 0xFF;
        match read_segment(&bytes) {
            Err(SstError::HashMismatch { at }) => {
                assert_ne!(at, u64::MAX, "a block, not the footer")
            }
            other => panic!("expected a block HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_flipped_footer_byte_is_caught() {
        let seg = Segment::new(1, sample_entries(10));
        let mut bytes = write_segment(&seg);
        // Corrupt the seq field inside the footer (before the footer hash).
        let n = bytes.len();
        bytes[n - FOOTER_LEN + 16] ^= 0xFF;
        match read_segment(&bytes) {
            Err(SstError::HashMismatch { at }) => assert_eq!(at, u64::MAX, "the footer"),
            other => panic!("expected a footer HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_buffer_is_rejected_not_read_empty() {
        let seg = Segment::new(1, sample_entries(10));
        let bytes = write_segment(&seg);
        // Half a footer: must error, never decode to an empty segment.
        assert!(matches!(
            read_segment(&bytes[..FOOTER_LEN / 2]),
            Err(SstError::Truncated { what: "footer" })
        ));
        // A whole footer but a chopped body: the index/blocks it points at are
        // now past the end.
        assert!(read_segment(&bytes[bytes.len() - FOOTER_LEN..]).is_err());
    }

    #[test]
    fn a_real_sealed_segment_round_trips_byte_identically() {
        use crate::{Store, StoredValue};
        use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};

        let pfx = KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            kind: Kind::NODE,
            partition: Partition(1),
        };
        let s = Store::new();
        // Enough real records to fill multiple blocks, with multi-version
        // chains — exactly what `Store::seal` freezes into a `Segment`.
        let mut written: Vec<(Vec<u8>, u64)> = Vec::new();
        for i in 0..300u32 {
            let k = format!("node-{i:05}").into_bytes();
            let ts = s
                .put(&pfx, &k, StoredValue::Plain(vec![i as u8; 40]))
                .expect("put");
            written.push((k, ts));
        }
        // Overwrite a few keys → real multi-version chains in the tail.
        for i in [0u32, 7, 42, 299] {
            let k = format!("node-{i:05}").into_bytes();
            s.put(&pfx, &k, StoredValue::Plain(vec![0xEE; 4]))
                .expect("overwrite");
        }
        s.seal().expect("seals");

        let segs = s.sealed_segments_for_test();
        let seg = segs[0].as_resident().expect("resident seal");
        let back = read_segment(&write_segment(seg)).expect("clean read of real seal output");
        assert_eq!(
            back.row_entries(),
            seg.row_entries(),
            "a real sealed segment must round-trip through disk byte-identically"
        );
        // And reads agree at the exact commit timestamps that were handed out.
        for (k, ts) in &written {
            for probe in [ts.saturating_sub(1), *ts, ts + 1_000] {
                assert_eq!(
                    seg.get_at(k, probe),
                    back.get_at(k, probe),
                    "get_at disagrees for a real key"
                );
            }
        }
    }

    #[test]
    fn seal_to_a_file_and_reopen_is_byte_identical() {
        let seg = Segment::new(11, sample_entries(500));
        let dir = std::env::temp_dir();
        let path = dir.join("engram_sst_m0_roundtrip.seg");
        write_segment_file(&seg, &path).expect("write to disk");
        let back = read_segment_file(&path).expect("reopen from disk");
        assert_eq!(back.seq, 11);
        assert_eq!(
            back.row_entries(),
            seg.row_entries(),
            "seal → file → reopen must be byte-identical"
        );
        // The atomic write leaves no .tmp behind.
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file not cleaned up by rename"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn not_a_segment_is_bad_magic() {
        let mut bytes = vec![0u8; FOOTER_LEN * 2];
        // Give it a valid footer hash over garbage fields but the wrong magic,
        // so we reach the magic check rather than failing the hash first.
        let start = bytes.len() - FOOTER_LEN;
        let h = blake3::hash(&bytes[start..start + FOOTER_HASHED]);
        bytes[start + FOOTER_HASHED..].copy_from_slice(h.as_bytes());
        assert!(matches!(read_segment(&bytes), Err(SstError::BadMagic)));
    }

    // ── the cached commit bound ──────────────────────────────────────────
    //
    // `Segment::max_commit_ts` is cached at construction because OCC
    // validation asks for it per validated key per segment inside the global
    // commit latch. A cache is only as good as the guarantee that it equals
    // what it replaced, so these assert it against a from-scratch recompute —
    // the oracle — rather than against a hand-written expectation, which would
    // only restate whatever the constructor happened to do.

    #[test]
    fn cached_bound_equals_a_from_scratch_recompute() {
        for big in [0usize, 100, 5_000] {
            let seg = Segment::new(1, sample_entries(big));
            assert_eq!(
                seg.max_commit_ts(),
                seg.recomputed_max_commit_ts(),
                "big {big}: the cached bound must equal the walk it replaced"
            );
            // sample_entries' greatest commit_ts is 9 (beta's tombstone).
            assert_eq!(seg.max_commit_ts(), 9, "big {big}: and it is the real max");
        }
    }

    #[test]
    fn an_empty_segment_reports_a_zero_bound() {
        // 0 is the identity the validator's `<= snapshot_ts` test expects for
        // "nothing here"; a constructor that skipped the stamp would also
        // answer 0, which is why the fat-segment canary in
        // `tests/validate_sealed_prefix.rs` exists alongside this.
        let seg = Segment::new(0, BTreeMap::new());
        assert_eq!(seg.max_commit_ts(), 0);
        assert_eq!(seg.max_commit_ts(), seg.recomputed_max_commit_ts());
    }

    /// A sealed-segment read SHARES the record's bytes rather than copying them.
    ///
    /// This is the point of putting `Version.value` behind an `Arc`, and it is
    /// invisible from the answers — `get_at` returned the identical bytes
    /// before and after. So it is asserted on POINTER IDENTITY: two reads of
    /// one key must hand back the same allocation, and a read must share with
    /// the segment rather than duplicate it.
    ///
    /// Without this the change is untested: every other test in this file
    /// passes just as happily against a `Vec<u8>` that is deep-copied on every
    /// sealed-segment hit.
    #[test]
    fn a_sealed_read_shares_the_bytes_rather_than_copying_them() {
        // A big value, so a copy would be unmistakable if one happened.
        let seg = Segment::new(1, sample_entries(64 * 1024));
        let key = b"delta".to_vec();
        let a = seg.get_at(&key, u64::MAX).expect("present");
        let b = seg.get_at(&key, u64::MAX).expect("present");
        let (av, bv) = (a.value.expect("live"), b.value.expect("live"));
        assert!(
            std::sync::Arc::ptr_eq(&av, &bv),
            "two reads of one key must hand back the SAME allocation — if they              do not, every sealed-segment hit is still copying the record"
        );
        assert_eq!(av.len(), 64 * 1024, "and it is the whole value");
        // The segment still holds it too: the reads shared, they did not move.
        let c = seg.get_at(&key, u64::MAX).expect("present");
        assert!(std::sync::Arc::ptr_eq(&av, &c.value.expect("live")));
    }

    /// `cloned_entries` — the compactor's input — shares too. It used to
    /// duplicate a whole segment's bytes to hand them to the merge.
    #[test]
    fn cloned_entries_shares_the_bytes_with_the_segment() {
        let seg = Segment::new(1, sample_entries(16 * 1024));
        let key = b"delta".to_vec();
        let from_seg = seg.get_at(&key, u64::MAX).expect("present").value.expect("live");
        let cloned = seg.cloned_entries();
        let from_clone = cloned
            .get(&key)
            .and_then(|vs| vs.last())
            .and_then(|v| v.value.clone())
            .expect("live");
        assert!(
            std::sync::Arc::ptr_eq(&from_seg, &from_clone),
            "the compactor's input must share the segment's bytes, not copy a              segment's worth of them"
        );
    }

    #[test]
    fn the_footer_stamps_the_segments_cached_bound() {
        // These used to be two independent expressions of one rule: the footer
        // recomputed over `cloned_entries` while the segment walked
        // `entries` + `blocks`. Identical in fact, but nothing forced it. Now
        // the footer reads the cached field, and this pins that it does.
        let seg = Segment::new(7, sample_entries(100));
        let bytes = write_segment(&seg);
        let footer = read_footer(&bytes).expect("clean footer");
        assert_eq!(
            footer.max_commit_ts,
            seg.max_commit_ts(),
            "the footer must stamp the segment's own bound, not a second opinion"
        );
        // And it survives the round trip into a rebuilt segment.
        let back = read_segment(&bytes).expect("clean read");
        assert_eq!(back.max_commit_ts(), seg.max_commit_ts());
        assert_eq!(back.max_commit_ts(), back.recomputed_max_commit_ts());
    }
}
