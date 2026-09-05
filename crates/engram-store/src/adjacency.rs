//! L5 — adjacency as chunked posting lists.
//!
//! The plan's warning is the design constraint: *"Do NOT ship 'one KV per edge
//! for now.' The executor's cost model is defined by the adjacency encoding;
//! changing it later invalidates the operators and the planner's constants."*
//!
//! So: one row per `(node, direction, edge type, chunk)`, holding a SORTED
//! packed list of peers. Reading a neighbourhood is a short range of key-
//! adjacent rows; a supernode (the corpus holds one of degree 245,340) is many
//! chunks rather than one multi-megabyte value that every edge-add rewrites.
//! Peers carry their namespace, so a cross-namespace edge (M2's overlay edge)
//! is an ordinary entry.
//!
//! # Concurrency: the CAS loop is the write path
//!
//! Two writers adding edges to the same node race on the same chunk row. Each
//! goes through `cas(expect current, write updated)` and RETRIES on mismatch —
//! the lost-update shape L3's CAS exists for, exercised here on real data. The
//! retry is bounded and its exhaustion is an ERROR, not a silent drop.
//!
//! # The half-edge seam, named
//!
//! An edge is TWO rows — `Out` on the source, `In` on the destination — and
//! v0 has no multi-row transaction, so a crash between them leaves a half
//! edge. That window is not hidden: a crash point sits inside it,
//! [`find_half_edges`] detects the damage from the outside, and the crash test
//! proves the pair (window exists → checker finds it). When multi-row commit
//! arrives, the checker becomes the proof that the window CLOSED — the same
//! instrument, flipped from documenting a flaw to guarding a fix.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::{crash_point, sometimes};

use crate::{Store, StoreError, StoredValue};

/// Edge direction, from the perspective of the node whose row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDir {
    /// The node is the source.
    Out,
    /// The node is the destination.
    In,
}

impl EdgeDir {
    fn byte(self) -> u8 {
        match self {
            EdgeDir::Out => 0,
            EdgeDir::In => 1,
        }
    }
}

/// An edge type id. Interned elsewhere; the adjacency layer treats it as opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeType(pub u32);

/// A peer endpoint: namespace + entity id. Structural, like every key part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerRef {
    /// The peer's namespace — cross-namespace edges are ordinary entries.
    pub ns: u32,
    /// The peer's entity id.
    pub id: u64,
}

impl PeerRef {
    const LEN: usize = 12;

    fn encode_into(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ns.to_be_bytes());
        out.extend_from_slice(&self.id.to_be_bytes());
    }

    fn decode(buf: &[u8]) -> Option<PeerRef> {
        Some(PeerRef {
            ns: u32::from_be_bytes(buf.get(0..4)?.try_into().ok()?),
            id: u64::from_be_bytes(buf.get(4..12)?.try_into().ok()?),
        })
    }
}

/// Peers per chunk. Small enough that a chunk rewrite is cheap, large enough
/// that a neighbourhood read touches few rows. A supernode of degree 245,340
/// (the measured corpus maximum) is ~240 chunks at this size.
pub const CHUNK_CAPACITY: usize = 1024;

/// Bounded CAS retries. Exhaustion is an error the caller sees — a silent drop
/// here is a lost edge that every traversal afterwards simply never visits.
const MAX_CAS_RETRIES: u32 = 32;

/// Adjacency errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdjacencyError {
    /// The CAS loop exhausted its retries under contention.
    ContentionExhausted,
    /// A chunk's bytes do not decode as packed peers.
    CorruptChunk {
        /// The chunk index.
        chunk: u32,
    },
    /// The underlying store refused.
    Store(StoreError),
}

impl std::fmt::Display for AdjacencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdjacencyError::ContentionExhausted => write!(f, "adjacency CAS exhausted its retries"),
            AdjacencyError::CorruptChunk { chunk } => {
                write!(f, "adjacency chunk {chunk} is corrupt")
            }
            AdjacencyError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AdjacencyError {}

/// Where a node's adjacency lives.
#[derive(Debug, Clone, Copy)]
pub struct NodeAt {
    /// The node's realm.
    pub realm: Realm,
    /// The node's namespace.
    pub ns: Namespace,
    /// The node's partition — adjacency rows share it, which is what makes a
    /// node group partition-homogeneous and its scan contiguous.
    pub partition: Partition,
    /// The node id.
    pub node: u64,
}

/// The key body of one adjacency chunk — public because the DST and any
/// future fsck need to address chunks directly (a corrupt-chunk test writes
/// garbage at exactly this key).
pub fn chunk_key_body(node: u64, dir: EdgeDir, etype: EdgeType, chunk: u32) -> Vec<u8> {
    chunk_body(node, dir, etype, chunk)
}

fn chunk_body(node: u64, dir: EdgeDir, etype: EdgeType, chunk: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(17);
    b.extend_from_slice(&node.to_be_bytes());
    b.push(dir.byte());
    b.extend_from_slice(&etype.0.to_be_bytes());
    b.extend_from_slice(&chunk.to_be_bytes());
    b
}

fn prefix(at: NodeAt) -> KeyPrefix {
    KeyPrefix {
        realm: at.realm,
        namespace: at.ns,
        kind: Kind::ADJACENCY,
        partition: at.partition,
    }
}

fn decode_chunk(bytes: &[u8], chunk: u32) -> Result<Vec<PeerRef>, AdjacencyError> {
    if bytes.len() % PeerRef::LEN != 0 {
        return Err(AdjacencyError::CorruptChunk { chunk });
    }
    let mut peers = Vec::with_capacity(bytes.len() / PeerRef::LEN);
    for c in bytes.chunks_exact(PeerRef::LEN) {
        peers.push(PeerRef::decode(c).ok_or(AdjacencyError::CorruptChunk { chunk })?);
    }
    Ok(peers)
}

fn encode_chunk(peers: &[PeerRef]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * PeerRef::LEN);
    for p in peers {
        p.encode_into(&mut out);
    }
    out
}

/// Insert `peer` into one direction of one node's posting list.
///
/// The CAS loop: read the last chunk, insert sorted (idempotent — a duplicate
/// peer is a no-op, so retrying a half-applied `add_edge` cannot double an
/// edge), CAS it back, retry on mismatch. A full chunk rolls over to the next
/// index — the supernode path.
async fn add_direction(
    store: &Store,
    at: NodeAt,
    dir: EdgeDir,
    etype: EdgeType,
    peer: PeerRef,
) -> Result<(), AdjacencyError> {
    let p = prefix(at);
    for _ in 0..MAX_CAS_RETRIES {
        // Find the last chunk (the only one that can be non-full).
        let mut chunk = 0u32;
        let mut current: Option<Vec<u8>> = None;
        loop {
            let body = chunk_body(at.node, dir, etype, chunk);
            match store.get(&p, &body) {
                None => break,
                Some(bytes) => {
                    // Duplicate check spans EVERY chunk, not just the last:
                    // sorted insertion keeps a peer in exactly one chunk, but a
                    // peer already present in an earlier, full chunk must be
                    // seen or add_edge stops being idempotent.
                    let peers = decode_chunk(&bytes, chunk)?;
                    if peers.binary_search(&peer).is_ok() {
                        return Ok(());
                    }
                    if peers.len() < CHUNK_CAPACITY {
                        current = Some(bytes);
                        break;
                    }
                    chunk += 1;
                    sometimes!("adjacency.chunk rolled over", true);
                }
            }
        }

        let body = chunk_body(at.node, dir, etype, chunk);
        let mut peers = match &current {
            Some(bytes) => decode_chunk(bytes, chunk)?,
            None => Vec::new(),
        };
        match peers.binary_search(&peer) {
            Ok(_) => return Ok(()),
            Err(pos) => peers.insert(pos, peer),
        }

        match store
            .cas(
                &p,
                &body,
                current.as_deref(),
                StoredValue::Plain(encode_chunk(&peers)),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(StoreError::CasMismatch { .. }) => {
                // Someone else advanced this chunk between our read and our
                // lock. Retry against the new reality — THE lost-update shape,
                // resolved by re-reading rather than by overwriting.
                sometimes!("adjacency.cas retried", true);
                continue;
            }
            Err(e) => return Err(AdjacencyError::Store(e)),
        }
    }
    Err(AdjacencyError::ContentionExhausted)
}

/// Add an edge `src --etype--> dst`, writing BOTH directions.
///
/// The crash point between the two writes is the half-edge seam, deliberately
/// visible — see the module header.
pub async fn add_edge(
    store: &Store,
    src: NodeAt,
    etype: EdgeType,
    dst: NodeAt,
) -> Result<(), AdjacencyError> {
    let dst_peer = PeerRef {
        ns: dst.ns.0,
        id: dst.node,
    };
    let src_peer = PeerRef {
        ns: src.ns.0,
        id: src.node,
    };
    add_direction(store, src, EdgeDir::Out, etype, dst_peer).await?;
    crash_point("adjacency.between_out_and_in");
    add_direction(store, dst, EdgeDir::In, etype, src_peer).await?;
    Ok(())
}

/// Remove an edge, both directions. Removing an absent edge is a no-op.
pub async fn remove_edge(
    store: &Store,
    src: NodeAt,
    etype: EdgeType,
    dst: NodeAt,
) -> Result<(), AdjacencyError> {
    remove_direction(
        store,
        src,
        EdgeDir::Out,
        etype,
        PeerRef {
            ns: dst.ns.0,
            id: dst.node,
        },
    )
    .await?;
    crash_point("adjacency.between_out_and_in");
    remove_direction(
        store,
        dst,
        EdgeDir::In,
        etype,
        PeerRef {
            ns: src.ns.0,
            id: src.node,
        },
    )
    .await?;
    Ok(())
}

async fn remove_direction(
    store: &Store,
    at: NodeAt,
    dir: EdgeDir,
    etype: EdgeType,
    peer: PeerRef,
) -> Result<(), AdjacencyError> {
    let p = prefix(at);
    'retry: for _ in 0..MAX_CAS_RETRIES {
        let mut chunk = 0u32;
        loop {
            let body = chunk_body(at.node, dir, etype, chunk);
            let Some(bytes) = store.get(&p, &body) else {
                return Ok(());
            };
            let mut peers = decode_chunk(&bytes, chunk)?;
            if let Ok(pos) = peers.binary_search(&peer) {
                peers.remove(pos);
                match store
                    .cas(
                        &p,
                        &body,
                        Some(&bytes),
                        StoredValue::Plain(encode_chunk(&peers)),
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(StoreError::CasMismatch { .. }) => {
                        sometimes!("adjacency.cas retried", true);
                        continue 'retry;
                    }
                    Err(e) => return Err(AdjacencyError::Store(e)),
                }
            }
            chunk += 1;
        }
    }
    Err(AdjacencyError::ContentionExhausted)
}

/// A node's neighbours in one direction for one edge type, sorted, across all
/// chunks. The executor's scan primitive.
pub fn neighbors(
    store: &Store,
    at: NodeAt,
    dir: EdgeDir,
    etype: EdgeType,
) -> Result<Vec<PeerRef>, AdjacencyError> {
    let p = prefix(at);
    let mut out = Vec::new();
    let mut chunk = 0u32;
    loop {
        let body = chunk_body(at.node, dir, etype, chunk);
        match store.get(&p, &body) {
            None => break,
            Some(bytes) => {
                out.extend(decode_chunk(&bytes, chunk)?);
                chunk += 1;
            }
        }
    }
    // Chunks are individually sorted and rollover preserves order between
    // them EXCEPT under concurrent inserts near a boundary; a final merge
    // keeps the contract absolute rather than schedule-dependent.
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// A node's degree in one direction for one type.
pub fn degree(
    store: &Store,
    at: NodeAt,
    dir: EdgeDir,
    etype: EdgeType,
) -> Result<usize, AdjacencyError> {
    Ok(neighbors(store, at, dir, etype)?.len())
}

/// How many chunk rows a posting list occupies, and the RAW entry count
/// across them — before `neighbors`' dedup.
///
/// `neighbors` deduplicates, which is right for readers and blinding for
/// structure: a duplicate smuggled into a second chunk, or a rollover that
/// stopped rolling, is invisible through every deduped read. Canaries against
/// both defects came back NOT DETECTED for exactly that reason. This is the
/// structural view the tests (and a future fsck) assert on.
pub fn chunk_stats(
    store: &Store,
    at: NodeAt,
    dir: EdgeDir,
    etype: EdgeType,
) -> Result<(u32, usize), AdjacencyError> {
    let p = prefix(at);
    let mut chunks = 0u32;
    let mut raw = 0usize;
    loop {
        let body = chunk_body(at.node, dir, etype, chunks);
        match store.get(&p, &body) {
            None => break,
            Some(bytes) => {
                raw += decode_chunk(&bytes, chunks)?.len();
                chunks += 1;
            }
        }
    }
    Ok((chunks, raw))
}

/// Find half edges: an `Out` entry whose mirror `In` is missing, or the
/// reverse, between two nodes.
///
/// The detector for the crash window `add_edge` documents. Scoped to a node
/// PAIR because v0 has no key iteration; the DST arms the crash and then asks
/// this exact question, which is the shape the full scan will generalise.
pub fn find_half_edges(
    store: &Store,
    a: NodeAt,
    etype: EdgeType,
    b: NodeAt,
) -> Result<Vec<String>, AdjacencyError> {
    let mut findings = Vec::new();
    let a_out = neighbors(store, a, EdgeDir::Out, etype)?;
    let b_in = neighbors(store, b, EdgeDir::In, etype)?;
    let b_peer = PeerRef {
        ns: b.ns.0,
        id: b.node,
    };
    let a_peer = PeerRef {
        ns: a.ns.0,
        id: a.node,
    };
    let forward = a_out.binary_search(&b_peer).is_ok();
    let backward = b_in.binary_search(&a_peer).is_ok();
    if forward != backward {
        sometimes!("adjacency.half edge found", true);
        findings.push(format!(
            "half edge: {}->{} out={} in={}",
            a.node, b.node, forward, backward
        ));
    }
    Ok(findings)
}
