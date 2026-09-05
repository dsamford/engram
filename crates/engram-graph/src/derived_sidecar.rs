//! §5.4 — the derived bases, persisted, so a restart does not re-earn them.
//!
//! # What this closes
//!
//! §5.2 makes the CSR a by-product of compaction, so a healthy server stops
//! rebuilding it *during* a process's life. It does nothing for the START of
//! one: at official LDBC SF1 a cold server pays a ~43.2 s warm walking 17.26M
//! adjacency rows across ~32 buckets, plus the membership bases. Without this
//! item Phase 5 is a process-lifetime cache rather than a property of the
//! store.
//!
//! # Why a SIDECAR and not a segment field
//!
//! Three reasons, and the second is the one that decides it.
//!
//! 1. **No segment-format change.** The programme's format register stays at
//!    exactly one additive touch (the v3 footer).
//! 2. **A stale sidecar is DISCARDABLE, not fatal.** This is the genuinely
//!    dangerous item in the whole plan: a stale persisted CSR is a *wrong
//!    answer with a valid checksum* — it answers a traversal with a subset,
//!    silently. In a segment there is nowhere to put that failure except a
//!    panic or a lie. A sidecar that fails its vintage check is simply deleted
//!    and rebuilt, so the worst case degrades to today's behaviour.
//! 3. **The discipline is proven in-tree.** `ensure_range_index_scoped`
//!    already loads a persisted index only while its vintage holds and falls
//!    through to a build otherwise. This is that mechanism, with a stronger
//!    vintage.
//!
//! # The vintage, and why it is not a timestamp
//!
//! `(compaction stamp, sealed-set id)`. The stamp alone cannot detect a segment
//! added, removed or re-merged underneath the sidecar — every one of those
//! leaves the clock where it was and every one makes the sidecar describe rows
//! the store no longer holds. So the sealed set's own identity
//! (`Store::sealed_set_id`) is what must match, and the stamp rides along as
//! the value the base is published AT.
//!
//! # v2: one RECORD at a time, and the SPARSE directory (2026-09-04)
//!
//! v1 was one body, one hash, read whole: `std::fs::read` of the file, BLAKE3
//! over all of it, then every table decoded into memory before the first was
//! published — and each table's row directory was the DENSE form, one `u32`
//! per node id. On the production mirror that file was ~4.9 GB (318 tables
//! over ~3.4M ids), and adoption held the file bytes AND the decoded vectors
//! at once: the pod was OOMKilled three times within a minute of start at the
//! 12Gi Neo4j-parity limit, each time right after "adopted 319 derived
//! structure(s)", with no client connected. Steady state was a third of that.
//!
//! v2 stores each table as its own hashed RECORD with the sparse directory
//! ([`crate::RowIndex::parts`]: ids/8 bytes of bits + 4 bytes per non-empty
//! node), a table of contents at the end, and a fixed footer. The reader
//! verifies the footer and the TOC hash, then reads, verifies and decodes ONE
//! record at a time; the writer appends one record at a time. Memory at
//! adoption is one table, not the file. The safety contract is unchanged in
//! kind: every length read from disk is checked against bytes that exist, a
//! record's hash is verified before any of its lengths are trusted, the TOC
//! hash covers the vintage fields, and any mismatch REFUSES — a corrupt record
//! stops adoption at that record (what was adopted before it was verified on
//! its own), and the rest is built on first use.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use engram_observe::{counted, sometimes};

use crate::{RowIndex, SlimAdj};

/// `"EGDS"` — engram graph derived sidecar.
const MAGIC: [u8; 4] = *b"EGDS";
/// Bumping this refuses every older sidecar, which is always safe: the reader
/// rebuilds. There is deliberately no compatibility shim — the file is a cache
/// whose only cost of loss is a rebuild, so an old one is never worth reading.
const VERSION: u32 = 2;
/// stamp u64 | sealed_id u64 | toc_offset u64 | toc_len u64 | toc_hash 32 | MAGIC 4 | VERSION 4
const FOOTER_LEN: usize = 8 + 8 + 8 + 8 + 32 + 4 + 4;
/// A record's kind byte in the TOC.
const KIND_ADJ: u8 = 1;
const KIND_MEMBERS: u8 = 2;
/// The most bytes one record may claim — a corrupt TOC entry must not reserve
/// a gigabyte before its bytes are read; a real record is far under this.
const MAX_RECORD_LEN: u64 = 4 << 30;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn u32(&mut self) -> Option<u32> {
        let e = self.at.checked_add(4)?;
        let v = u32::from_le_bytes(self.b.get(self.at..e)?.try_into().ok()?);
        self.at = e;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let e = self.at.checked_add(8)?;
        let v = u64::from_le_bytes(self.b.get(self.at..e)?.try_into().ok()?);
        self.at = e;
        Some(v)
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        let e = self.at.checked_add(n)?;
        let s = self.b.get(self.at..e)?;
        self.at = e;
        Some(s)
    }
    /// A length read from the file is ATTACKER-SHAPED in the same sense a
    /// corrupt block is: it must never be used to reserve memory before the
    /// bytes behind it are known to exist. `with_capacity(n)` on a corrupt
    /// length is an OOM, not a refusal.
    fn len_within(&self, n: usize, unit: usize) -> bool {
        n.checked_mul(unit)
            .is_some_and(|bytes| self.b.len().saturating_sub(self.at) >= bytes)
    }
    fn done(&self) -> bool {
        self.at == self.b.len()
    }
}

/// One table-of-contents entry: where a record is, how long, and its hash.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TocEntry {
    kind: u8,
    offset: u64,
    len: u64,
    hash: [u8; 32],
}

/// The file this graph's sidecar lives in. One file per graph, named by the
/// realm/namespace prefix so two graphs in one data directory cannot
/// collide — the mistake that would make a sidecar describe another
/// tenant's rows and pass every checksum.
pub(crate) fn sidecar_path(dir: &Path, prefix: &[u8]) -> PathBuf {
    let mut name = String::from("derived-");
    for b in prefix {
        name.push_str(&format!("{b:02x}"));
    }
    name.push_str(".dsc");
    dir.join(name)
}

fn encode_adj(key: &(u8, Vec<u32>), index: &RowIndex, entries: &[SlimAdj], sorted: bool) -> Vec<u8> {
    let (nodes, bits, starts) = index.parts();
    let mut body = Vec::with_capacity(32 + bits.len() * 8 + starts.len() * 4 + entries.len() * 20);
    body.push(key.0);
    put_u32(&mut body, key.1.len() as u32);
    for t in &key.1 {
        put_u32(&mut body, *t);
    }
    body.push(u8::from(sorted));
    put_u32(&mut body, nodes as u32);
    put_u32(&mut body, bits.len() as u32);
    for w in bits {
        put_u64(&mut body, *w);
    }
    put_u32(&mut body, starts.len() as u32);
    for s in starts {
        put_u32(&mut body, *s);
    }
    put_u32(&mut body, entries.len() as u32);
    for e in entries {
        put_u64(&mut body, e.rel);
        put_u32(&mut body, e.type_token);
        put_u64(&mut body, e.peer);
    }
    body
}

fn encode_members(token: u32, ids: &[u64]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + ids.len() * 8);
    put_u32(&mut body, token);
    put_u32(&mut body, ids.len() as u32);
    for id in ids {
        put_u64(&mut body, *id);
    }
    body
}

/// One persisted derived base, decoded.
pub(crate) enum Record {
    Adj {
        key: (u8, Vec<u32>),
        index: RowIndex,
        entries: Vec<SlimAdj>,
        sorted: bool,
    },
    Members {
        token: u32,
        ids: Vec<u64>,
    },
}

/// Decode one verified record. `None` = malformed: refuse it (the hash passed,
/// so this is a writer bug rather than corruption — refused all the same).
fn decode_record(kind: u8, b: &[u8]) -> Option<Record> {
    let mut c = Cursor { b, at: 0 };
    match kind {
        KIND_ADJ => {
            let tag = c.u8()?;
            let n_types = c.u32()? as usize;
            if !c.len_within(n_types, 4) {
                return None;
            }
            let mut types = Vec::with_capacity(n_types);
            for _ in 0..n_types {
                types.push(c.u32()?);
            }
            let sorted = c.u8()? != 0;
            let nodes = c.u32()? as usize;
            let n_words = c.u32()? as usize;
            if !c.len_within(n_words, 8) {
                return None;
            }
            let mut bits = Vec::with_capacity(n_words);
            for _ in 0..n_words {
                bits.push(c.u64()?);
            }
            let n_starts = c.u32()? as usize;
            if !c.len_within(n_starts, 4) {
                return None;
            }
            let mut starts = Vec::with_capacity(n_starts);
            for _ in 0..n_starts {
                starts.push(c.u32()?);
            }
            let n_ent = c.u32()? as usize;
            if !c.len_within(n_ent, 20) {
                return None;
            }
            let mut entries = Vec::with_capacity(n_ent);
            for _ in 0..n_ent {
                let rel = c.u64()?;
                let type_token = c.u32()?;
                let peer = c.u64()?;
                entries.push(SlimAdj {
                    rel,
                    type_token,
                    peer,
                });
            }
            if !c.done() {
                return None;
            }
            let index = RowIndex::from_parts(nodes, bits, starts)?;
            // The directory must not name entries the record does not carry.
            if index.to_dense().last().copied().unwrap_or(0) as usize != entries.len() {
                return None;
            }
            Some(Record::Adj {
                key: (tag, types),
                index,
                entries,
                sorted,
            })
        }
        KIND_MEMBERS => {
            let token = c.u32()?;
            let n_ids = c.u32()? as usize;
            if !c.len_within(n_ids, 8) {
                return None;
            }
            let mut ids = Vec::with_capacity(n_ids);
            for _ in 0..n_ids {
                ids.push(c.u64()?);
            }
            if !c.done() {
                return None;
            }
            Some(Record::Members { token, ids })
        }
        _ => None,
    }
}

/// Writes a sidecar ONE RECORD AT A TIME, publishing it by rename at the end.
/// A torn sidecar read as valid is the failure this whole file exists to
/// prevent, so nothing is at the final path until `finish` renamed it there.
pub(crate) struct SidecarWriter {
    tmp: PathBuf,
    path: PathBuf,
    out: BufWriter<File>,
    at: u64,
    toc: Vec<TocEntry>,
}

impl SidecarWriter {
    pub(crate) fn create(dir: &Path, prefix: &[u8]) -> io::Result<SidecarWriter> {
        let path = sidecar_path(dir, prefix);
        let tmp = path.with_extension("dsctmp");
        let out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        Ok(SidecarWriter {
            tmp,
            path,
            out,
            at: 0,
            toc: Vec::new(),
        })
    }

    fn append(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        let hash = *blake3::hash(body).as_bytes();
        self.out.write_all(body)?;
        self.toc.push(TocEntry {
            kind,
            offset: self.at,
            len: body.len() as u64,
            hash,
        });
        self.at += body.len() as u64;
        Ok(())
    }

    /// One adjacency CSR: its cache key, its sparse row directory, its packed
    /// rows and the `sorted_by_peer` claim established when it was built.
    pub(crate) fn add_adj(
        &mut self,
        key: &(u8, Vec<u32>),
        index: &RowIndex,
        entries: &[SlimAdj],
        sorted: bool,
    ) -> io::Result<()> {
        let body = encode_adj(key, index, entries, sorted);
        self.append(KIND_ADJ, &body)
    }

    /// One label's membership id-set.
    pub(crate) fn add_members(&mut self, token: u32, ids: &[u64]) -> io::Result<()> {
        let body = encode_members(token, ids);
        self.append(KIND_MEMBERS, &body)
    }

    /// Records written so far.
    pub(crate) fn len(&self) -> usize {
        self.toc.len()
    }

    /// Write the table of contents and the footer, sync, and PUBLISH by rename.
    pub(crate) fn finish(mut self, stamp: u64, sealed_id: u64) -> io::Result<()> {
        let mut toc = Vec::with_capacity(4 + self.toc.len() * 49);
        put_u32(&mut toc, self.toc.len() as u32);
        for e in &self.toc {
            toc.push(e.kind);
            put_u64(&mut toc, e.offset);
            put_u64(&mut toc, e.len);
            toc.extend_from_slice(&e.hash);
        }
        let toc_offset = self.at;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&stamp.to_le_bytes());
        hasher.update(&sealed_id.to_le_bytes());
        hasher.update(&toc);
        let toc_hash = *hasher.finalize().as_bytes();
        let mut footer = Vec::with_capacity(FOOTER_LEN);
        put_u64(&mut footer, stamp);
        put_u64(&mut footer, sealed_id);
        put_u64(&mut footer, toc_offset);
        put_u64(&mut footer, toc.len() as u64);
        footer.extend_from_slice(&toc_hash);
        footer.extend_from_slice(&MAGIC);
        put_u32(&mut footer, VERSION);
        debug_assert_eq!(footer.len(), FOOTER_LEN);
        self.out.write_all(&toc)?;
        self.out.write_all(&footer)?;
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        drop(self.out);
        // Windows refuses a rename onto an existing file; on the platforms the
        // server runs on the rename replaces atomically.
        if cfg!(windows) && self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
        std::fs::rename(&self.tmp, &self.path)?;
        counted!("graph.derived sidecars written");
        Ok(())
    }

    /// Discard what was written: nothing reaches the final path.
    pub(crate) fn abandon(self) {
        drop(self.out);
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// Reads a sidecar ONE RECORD AT A TIME. `open` verifies the footer, the TOC
/// hash and the vintage; each `read_record` verifies that record's own hash
/// before a single length in it is trusted.
pub(crate) struct SidecarReader {
    file: File,
    stamp: u64,
    toc: Vec<TocEntry>,
    toc_offset: u64,
}

/// Why `SidecarReader::open` declined a file. Every arm means "build it"; the
/// arms exist so the reason can be SAID. A sidecar refused silently looks
/// exactly like one never written — the bench deployment's first v2 start
/// found a v1 file, refused it without a word, warmed for 74 s, and the only
/// evidence was a warm time that had not improved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarRefusal {
    /// No file — the ordinary first start; nothing to report.
    Absent,
    /// Present but not a sidecar this reader understands: another version, a
    /// truncated or misplaced footer, a TOC that does not verify or that a
    /// writer could not have produced.
    Unreadable(&'static str),
    /// Intact and of this version, but describing another sealed set.
    Vintage { file: u64, store: u64 },
}

impl SidecarReader {
    /// Open the sidecar for `prefix`, but only if its vintage matches
    /// `sealed_id`. A file that is absent, unreadable, corrupt in its footer or
    /// TOC, of another version, or of another vintage all give `None` — one
    /// outcome, one meaning: build it. `open_reporting` says which — and is
    /// what the graph calls; this form is the tests' shorthand.
    #[cfg(test)]
    pub(crate) fn open(dir: &Path, prefix: &[u8], sealed_id: u64) -> Option<SidecarReader> {
        Self::open_reporting(dir, prefix, sealed_id).ok()
    }

    /// `open`, with the refusal named.
    pub(crate) fn open_reporting(
        dir: &Path,
        prefix: &[u8],
        sealed_id: u64,
    ) -> Result<SidecarReader, SidecarRefusal> {
        use SidecarRefusal::*;
        let mut file = File::open(sidecar_path(dir, prefix)).map_err(|_| Absent)?;
        let len = file
            .metadata()
            .map_err(|_| Unreadable("metadata"))?
            .len();
        if len < FOOTER_LEN as u64 {
            return Err(Unreadable("shorter than a footer"));
        }
        let mut footer = vec![0u8; FOOTER_LEN];
        file.seek(SeekFrom::Start(len - FOOTER_LEN as u64))
            .map_err(|_| Unreadable("seek to the footer"))?;
        file.read_exact(&mut footer)
            .map_err(|_| Unreadable("read the footer"))?;
        let mut c = Cursor { b: &footer, at: 0 };
        let stamp = c.u64().ok_or(Unreadable("footer"))?;
        let file_sealed_id = c.u64().ok_or(Unreadable("footer"))?;
        let toc_offset = c.u64().ok_or(Unreadable("footer"))?;
        let toc_len = c.u64().ok_or(Unreadable("footer"))?;
        let toc_hash: [u8; 32] = c
            .bytes(32)
            .and_then(|b| b.try_into().ok())
            .ok_or(Unreadable("footer"))?;
        if c.bytes(4).ok_or(Unreadable("footer"))? != MAGIC {
            return Err(Unreadable("not a derived sidecar (magic)"));
        }
        if c.u32().ok_or(Unreadable("footer"))? != VERSION {
            return Err(Unreadable("another sidecar version"));
        }
        // The TOC must sit exactly between the records and the footer.
        if toc_offset
            .checked_add(toc_len)
            .ok_or(Unreadable("toc placement"))?
            != len - FOOTER_LEN as u64
        {
            return Err(Unreadable("toc placement"));
        }
        let mut toc = vec![0u8; toc_len as usize];
        file.seek(SeekFrom::Start(toc_offset))
            .map_err(|_| Unreadable("seek to the toc"))?;
        file.read_exact(&mut toc)
            .map_err(|_| Unreadable("read the toc"))?;
        // The TOC hash BEFORE any length in it is trusted, and it covers the
        // vintage fields too, so a flipped stamp or sealed id is a refusal
        // rather than a wrong vintage that happens to match.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&stamp.to_le_bytes());
        hasher.update(&file_sealed_id.to_le_bytes());
        hasher.update(&toc);
        if hasher.finalize().as_bytes() != &toc_hash {
            counted!("graph.derived sidecar refused: toc hash");
            return Err(Unreadable("toc hash"));
        }
        let mut c = Cursor { b: &toc, at: 0 };
        let n = c.u32().ok_or(Unreadable("toc"))? as usize;
        if !c.len_within(n, 1 + 8 + 8 + 32) {
            return Err(Unreadable("toc length"));
        }
        let mut entries = Vec::with_capacity(n);
        let mut expect = 0u64;
        for _ in 0..n {
            let kind = c.u8().ok_or(Unreadable("toc"))?;
            let offset = c.u64().ok_or(Unreadable("toc"))?;
            let rlen = c.u64().ok_or(Unreadable("toc"))?;
            let hash: [u8; 32] = c
                .bytes(32)
                .and_then(|b| b.try_into().ok())
                .ok_or(Unreadable("toc"))?;
            // Records are contiguous and in order; a TOC saying otherwise is
            // not one this writer produced.
            if offset != expect || rlen > MAX_RECORD_LEN {
                return Err(Unreadable("toc record layout"));
            }
            expect = offset
                .checked_add(rlen)
                .ok_or(Unreadable("toc record layout"))?;
            entries.push(TocEntry {
                kind,
                offset,
                len: rlen,
                hash,
            });
        }
        if !c.done() || expect != toc_offset {
            return Err(Unreadable("toc record layout"));
        }
        if file_sealed_id != sealed_id {
            // REPORTED, not merely counted. A silently refused sidecar looks
            // exactly like one that was never written — the server warms slowly
            // and nothing says why. That cost a pod run to diagnose.
            eprintln!(
                "[engram-graph] derived sidecar REFUSED: it describes sealed set                  {:#018x}, the store has {:#018x}. A segment was added, removed or                  re-merged since it was written, so it names rows this store does                  not have — the structures are rebuilt.",
                file_sealed_id, sealed_id
            );
            // The sealed set moved: a segment was added, removed or re-merged.
            // The bytes are intact and the hash is good and the content is
            // WRONG, which is exactly the case a checksum cannot see.
            counted!("graph.derived sidecar refused: vintage");
            sometimes!("graph.derived sidecar refused for a moved sealed set", true);
            return Err(Vintage {
                file: file_sealed_id,
                store: sealed_id,
            });
        }
        counted!("graph.derived sidecars loaded");
        Ok(SidecarReader {
            file,
            stamp,
            toc: entries,
            toc_offset,
        })
    }

    /// The stamp the bases are published AT.
    pub(crate) fn stamp(&self) -> u64 {
        self.stamp
    }

    /// Records in the file.
    pub(crate) fn len(&self) -> usize {
        self.toc.len()
    }

    /// Read, verify and decode record `i`. `None` is a refusal of THAT record
    /// (corrupt bytes, or a body the writer could not have produced); the
    /// caller stops adopting there — what came before was verified on its own.
    pub(crate) fn read_record(&mut self, i: usize) -> Option<Record> {
        let e = self.toc.get(i)?.clone();
        if e.offset.checked_add(e.len)? > self.toc_offset {
            return None;
        }
        let mut body = vec![0u8; e.len as usize];
        self.file.seek(SeekFrom::Start(e.offset)).ok()?;
        self.file.read_exact(&mut body).ok()?;
        // BLAKE3 BEFORE any length in the body is trusted. Checking it after
        // would mean allocating from numbers a flipped bit chose.
        if blake3::hash(&body).as_bytes() != &e.hash {
            counted!("graph.derived sidecar refused: record hash");
            return None;
        }
        decode_record(e.kind, &body)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// One adjacency table as the writer takes it: key, directory, rows, sorted.
    type SampleAdj = ((u8, Vec<u32>), RowIndex, Vec<SlimAdj>, bool);

    struct Sample {
        adj: Vec<SampleAdj>,
        members: BTreeMap<u32, Vec<u64>>,
    }

    fn sample() -> Sample {
        Sample {
            adj: vec![
                (
                    (b'O', vec![]),
                    RowIndex::from_dense(&[0, 2, 2, 3]),
                    vec![
                        SlimAdj {
                            rel: 7,
                            type_token: 1,
                            peer: 9,
                        },
                        SlimAdj {
                            rel: 8,
                            type_token: 2,
                            peer: 3,
                        },
                        SlimAdj {
                            rel: 9,
                            type_token: 1,
                            peer: 0,
                        },
                    ],
                    false,
                ),
                ((b'I', vec![3, 5]), RowIndex::from_dense(&[0, 0]), vec![], true),
            ],
            members: BTreeMap::from([(1u32, vec![2u64, 4, 6]), (9u32, vec![])]),
        }
    }

    const STAMP: u64 = 0xDEAD_BEEF;
    const SEALED: u64 = 0x1234_5678_9ABC_DEF0;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("engram-dsc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_sample(dir: &Path) {
        let s = sample();
        let mut w = SidecarWriter::create(dir, b"pfx").expect("create");
        for (key, index, entries, sorted) in &s.adj {
            w.add_adj(key, index, entries, *sorted).expect("adj");
        }
        for (token, ids) in &s.members {
            w.add_members(*token, ids).expect("members");
        }
        w.finish(STAMP, SEALED).expect("finish");
    }

    /// Every record read back, or `None` at the first refusal.
    fn read_all(dir: &Path, sealed: u64) -> Option<Vec<Record>> {
        let mut r = SidecarReader::open(dir, b"pfx", sealed)?;
        let mut out = Vec::new();
        for i in 0..r.len() {
            out.push(r.read_record(i)?);
        }
        Some(out)
    }

    fn matches_sample(records: &[Record]) -> bool {
        let s = sample();
        if records.len() != s.adj.len() + s.members.len() {
            return false;
        }
        for (i, (key, index, entries, sorted)) in s.adj.iter().enumerate() {
            match &records[i] {
                Record::Adj {
                    key: k,
                    index: ix,
                    entries: en,
                    sorted: so,
                } => {
                    if k != key || ix != index || en != entries || so != sorted {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        for (j, (token, ids)) in s.members.iter().enumerate() {
            match &records[s.adj.len() + j] {
                Record::Members { token: t, ids: v } => {
                    if t != token || v != ids {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    #[test]
    fn a_sidecar_round_trips_exactly() {
        let dir = tmpdir("rt");
        write_sample(&dir);
        let r = SidecarReader::open(&dir, b"pfx", SEALED).expect("open");
        assert_eq!(r.stamp(), STAMP);
        let records = read_all(&dir, SEALED).expect("read");
        assert!(matches_sample(&records), "the encoding must be lossless");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// EVERY single-byte flip in the file must be refused — at open (footer,
    /// TOC) or at the record it lands in — and no flipped record may ever be
    /// handed back as a good one. Not a sampled flip: a corrupt CSR that parses
    /// is a wrong answer with a valid-looking file.
    #[test]
    fn every_flipped_byte_is_refused() {
        let dir = tmpdir("flip");
        write_sample(&dir);
        let path = sidecar_path(&dir, b"pfx");
        let good = std::fs::read(&path).expect("read");
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x01;
            std::fs::write(&path, &bad).expect("write");
            match read_all(&dir, SEALED) {
                None => {}
                Some(records) => panic!(
                    "a flip at byte {i} of {} was read as {} good record(s)",
                    good.len(),
                    records.len()
                ),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A TRUNCATED file is refused rather than parsed short. Truncation is the
    /// shape a crash mid-write produces (though the rename means a torn file
    /// never sits at the final path), and it is the one case where a length
    /// prefix promises more than the file holds.
    #[test]
    fn a_truncated_sidecar_is_refused() {
        let dir = tmpdir("trunc");
        write_sample(&dir);
        let path = sidecar_path(&dir, b"pfx");
        let good = std::fs::read(&path).expect("read");
        for cut in 0..good.len() {
            std::fs::write(&path, &good[..cut]).expect("write");
            assert!(
                read_all(&dir, SEALED).is_none(),
                "a file truncated to {cut} bytes was accepted"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wrong vintage is refused even though every byte is intact and every
    /// checksum passes — the failure a checksum cannot see.
    #[test]
    fn a_moved_sealed_set_refuses_an_intact_file() {
        let dir = tmpdir("vintage");
        write_sample(&dir);
        assert!(
            SidecarReader::open(&dir, b"pfx", SEALED).is_some(),
            "the matching vintage must load, or the negative below is vacuous"
        );
        assert!(
            SidecarReader::open(&dir, b"pfx", SEALED ^ 1).is_none(),
            "a sealed set that moved must REFUSE an otherwise perfect file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An abandoned writer leaves nothing at the final path, and a finished
    /// one replaces what was there.
    #[test]
    fn abandon_publishes_nothing_and_finish_replaces() {
        let dir = tmpdir("abandon");
        let w = SidecarWriter::create(&dir, b"pfx").expect("create");
        w.abandon();
        assert!(SidecarReader::open(&dir, b"pfx", SEALED).is_none());
        write_sample(&dir);
        assert!(SidecarReader::open(&dir, b"pfx", SEALED).is_some());
        let mut w = SidecarWriter::create(&dir, b"pfx").expect("create");
        w.add_members(4, &[1, 2]).expect("m");
        w.finish(STAMP + 1, SEALED + 1).expect("finish");
        let r = SidecarReader::open(&dir, b"pfx", SEALED + 1).expect("open");
        assert_eq!((r.stamp(), r.len()), (STAMP + 1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two graphs in one directory get two files. Sharing one would let a
    /// sidecar describe another tenant's rows and pass every check it has.
    #[test]
    fn different_prefixes_do_not_share_a_file() {
        let a = sidecar_path(Path::new("/d"), b"\x01\x02");
        let b = sidecar_path(Path::new("/d"), b"\x01\x03");
        assert_ne!(a, b);
    }
}
