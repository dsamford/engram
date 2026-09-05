//! SSC1 v2 — chunked sealing with seek, and the AAD that makes truncation loud.
//!
//! No content-defined chunking (CDC yields ~1.0× on compressed media and would
//! destroy the O(1) arithmetic): chunk `k` of a `total_len` blob at chunk size
//! `C` is exactly `plain[k*C .. min((k+1)*C, total_len)]`, so
//! `chunk = floor(offset / C)` is the whole seek computation.
//!
//! **The v2 property:** every chunk's ASSOCIATED DATA binds
//! `header ‖ k ‖ n` — the blob's parameters, the chunk's position, and the
//! TOTAL COUNT — at zero ciphertext expansion. So a chunk spliced into another
//! blob fails (header differs), a chunk moved within a blob fails (k differs),
//! and a truncated object fails on ANY chunk rather than only the last
//! (n differs). The realm is already in the AAD underneath, via the sealer.

use engram_crypto::{Dek, Sealer, Secret};
use engram_observe::{counted, sometimes};

/// Envelope overhead per chunk: version + nonce + GCM tag.
const CHUNK_OVERHEAD: usize = 1 + 12 + 16;

/// Header format v2: `version:u8 | chunk_size:u32 | total_len:u64 | n:u32`.
const HEADER_LEN: usize = 1 + 4 + 8 + 4;
const VERSION_V2: u8 = 2;

/// A sealed, chunked blob — header plus per-chunk envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedBlob {
    header: [u8; HEADER_LEN],
    chunks: Vec<Vec<u8>>,
}

/// Why a chunked operation refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkError {
    /// A zero chunk size, or a range outside the blob.
    BadRequest(String),
    /// The blob's shape disagrees with its header — truncated, padded, or a
    /// chunk of the wrong length. Refused before any key is consulted.
    Malformed(String),
    /// A chunk failed authentication — tampered, moved, spliced, or the
    /// wrong key. One refusal, as everywhere in the crypto seam.
    Refused {
        /// Which chunk refused.
        chunk: u32,
    },
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::BadRequest(d) => write!(f, "bad request: {d}"),
            ChunkError::Malformed(d) => write!(f, "malformed blob: {d}"),
            ChunkError::Refused { chunk } => write!(f, "chunk {chunk} did not authenticate"),
        }
    }
}

impl std::error::Error for ChunkError {}

fn header_bytes(chunk_size: u32, total_len: u64, n: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0] = VERSION_V2;
    h[1..5].copy_from_slice(&chunk_size.to_be_bytes());
    h[5..13].copy_from_slice(&total_len.to_be_bytes());
    h[13..17].copy_from_slice(&n.to_be_bytes());
    h
}

fn binding(header: &[u8; HEADER_LEN], k: u32, n: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER_LEN + 8);
    b.extend_from_slice(header);
    b.extend_from_slice(&k.to_be_bytes());
    b.extend_from_slice(&n.to_be_bytes());
    b
}

impl SealedBlob {
    /// The number of chunks.
    pub fn chunk_count(&self) -> u32 {
        u32::from_be_bytes(self.header[13..17].try_into().expect("4 bytes"))
    }

    /// The plaintext length.
    pub fn total_len(&self) -> u64 {
        u64::from_be_bytes(self.header[5..13].try_into().expect("8 bytes"))
    }

    /// The chunk size.
    pub fn chunk_size(&self) -> u32 {
        u32::from_be_bytes(self.header[1..5].try_into().expect("4 bytes"))
    }

    /// Serialize: header, then chunks in order. Chunk boundaries are
    /// DERIVABLE (every chunk is `chunk_size + overhead` except the last), so
    /// nothing else is stored.
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEADER_LEN + self.chunks.iter().map(Vec::len).sum::<usize>());
        out.extend_from_slice(&self.header);
        for c in &self.chunks {
            out.extend_from_slice(c);
        }
        out
    }

    /// Decode, refusing any shape disagreement with the header. This is a
    /// STRUCTURE check, not an integrity check — a well-shaped tampered blob
    /// passes here and refuses at [`open_range`].
    pub fn decode(bytes: &[u8]) -> Result<SealedBlob, ChunkError> {
        if bytes.len() < HEADER_LEN || bytes[0] != VERSION_V2 {
            return Err(ChunkError::Malformed(
                "short or wrong-version header".into(),
            ));
        }
        let header: [u8; HEADER_LEN] = bytes[..HEADER_LEN].try_into().expect("checked");
        let chunk_size = u32::from_be_bytes(header[1..5].try_into().expect("4 bytes")) as u64;
        let total_len = u64::from_be_bytes(header[5..13].try_into().expect("8 bytes"));
        let n = u32::from_be_bytes(header[13..17].try_into().expect("4 bytes"));
        if chunk_size == 0 {
            return Err(ChunkError::Malformed("zero chunk size".into()));
        }
        let expect_n = total_len.div_ceil(chunk_size).max(1);
        if u64::from(n) != expect_n {
            return Err(ChunkError::Malformed(format!(
                "header says {n} chunks; {total_len} bytes at {chunk_size} needs {expect_n}"
            )));
        }
        let mut chunks = Vec::with_capacity(n as usize);
        let mut at = HEADER_LEN;
        for k in 0..u64::from(n) {
            let plain_len = plain_chunk_len(total_len, chunk_size, k);
            let ct_len = plain_len as usize + CHUNK_OVERHEAD;
            let Some(chunk) = bytes.get(at..at + ct_len) else {
                // The shape the v2 AAD also catches — but catching it here,
                // before any key, is what lets an UNKEYED scrubber count
                // truncations.
                return Err(ChunkError::Malformed(format!("truncated at chunk {k}")));
            };
            chunks.push(chunk.to_vec());
            at += ct_len;
        }
        if at != bytes.len() {
            return Err(ChunkError::Malformed(format!(
                "{} trailing bytes",
                bytes.len() - at
            )));
        }
        Ok(SealedBlob { header, chunks })
    }

    /// Test access: replace one chunk's envelope (returns the old one).
    /// Exists so the splice/swap properties are testable against the REAL
    /// open path rather than a reimplementation.
    pub fn swap_chunk(&mut self, k: u32, envelope: Vec<u8>) -> Vec<u8> {
        std::mem::replace(&mut self.chunks[k as usize], envelope)
    }
}

fn plain_chunk_len(total_len: u64, chunk_size: u64, k: u64) -> u64 {
    let start = k * chunk_size;
    total_len.saturating_sub(start).min(chunk_size)
}

/// Seal `plain` into chunks of `chunk_size` under `sealer`.
pub fn seal_chunked(
    sealer: &mut Sealer,
    plain: &Secret,
    chunk_size: u32,
) -> Result<SealedBlob, ChunkError> {
    if chunk_size == 0 {
        return Err(ChunkError::BadRequest("chunk size 0".into()));
    }
    let total_len = plain.len() as u64;
    let n_u64 = total_len.div_ceil(u64::from(chunk_size)).max(1);
    let n = u32::try_from(n_u64)
        .map_err(|_| ChunkError::BadRequest("too many chunks for u32".into()))?;
    let header = header_bytes(chunk_size, total_len, n);
    let bytes = plain.expose();
    let mut chunks = Vec::with_capacity(n as usize);
    for k in 0..n {
        let start = k as u64 * u64::from(chunk_size);
        let len = plain_chunk_len(total_len, u64::from(chunk_size), u64::from(k));
        let piece = Secret::new(bytes[start as usize..(start + len) as usize].to_vec());
        chunks.push(sealer.seal_bound(&piece, &binding(&header, k, n)));
        counted!("blob.chunks sealed");
    }
    Ok(SealedBlob { header, chunks })
}

/// What a ranged open did — the seek economy, measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenReport {
    /// Chunks decrypted to serve the range.
    pub chunks_opened: u32,
    /// Total chunks in the blob.
    pub chunks_total: u32,
}

/// Open `[start, end)` of the plaintext, decrypting ONLY the covering chunks.
///
/// Seeking to minute 40 of 90 decrypts the chunks under the range — the
/// report carries the count so "at most ⌈range/C⌉ + 1" is asserted by tests
/// rather than believed.
pub fn open_range(
    dek: &Dek,
    blob: &SealedBlob,
    start: u64,
    end: u64,
) -> Result<(Vec<u8>, OpenReport), ChunkError> {
    let total = blob.total_len();
    if start > end || end > total {
        return Err(ChunkError::BadRequest(format!(
            "[{start}, {end}) outside 0..{total}"
        )));
    }
    let n = blob.chunk_count();
    let c = u64::from(blob.chunk_size());
    if blob.chunks.len() as u64 != u64::from(n) {
        return Err(ChunkError::Malformed(
            "chunk count disagrees with header".into(),
        ));
    }
    if start == end {
        return Ok((
            Vec::new(),
            OpenReport {
                chunks_opened: 0,
                chunks_total: n,
            },
        ));
    }
    let first = start / c;
    let last = (end - 1) / c;
    let mut out = Vec::with_capacity((end - start) as usize);
    for k in first..=last {
        let bind = binding(&blob.header, k as u32, n);
        let piece =
            engram_crypto::open_bound(dek, &blob.chunks[k as usize], &bind).map_err(|_| {
                sometimes!("blob.chunk refused", true);
                ChunkError::Refused { chunk: k as u32 }
            })?;
        let bytes = piece.expose();
        if bytes.len() as u64 != plain_chunk_len(total, c, k) {
            // The AAD authenticated it, so this is our own arithmetic being
            // wrong — refuse rather than mis-slice.
            return Err(ChunkError::Malformed(format!(
                "chunk {k} decrypted to a wrong length"
            )));
        }
        let chunk_start = k * c;
        let lo = start.saturating_sub(chunk_start).min(bytes.len() as u64) as usize;
        let hi = (end - chunk_start).min(bytes.len() as u64) as usize;
        out.extend_from_slice(&bytes[lo..hi]);
        counted!("blob.chunks opened");
    }
    Ok((
        out,
        OpenReport {
            chunks_opened: (last - first + 1) as u32,
            chunks_total: n,
        },
    ))
}
