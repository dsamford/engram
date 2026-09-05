#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The paged compactor's chunk loop RE-COVERS one boundary range per chunk.
//!
//! # The defect
//!
//! `Store::compact_paged_observed` walks the key space in chunks of
//! `BOUNDARIES_PER_CHUNK` (64) block boundaries
//! (crates/engram-store/src/compact_paged.rs:228-243):
//!
//! ```text
//! let lo   = if cut == 0 { None } else { Some(boundaries[cut - 1].clone()) };
//! let next = (cut + BOUNDARIES_PER_CHUNK).min(boundaries.len());
//! let hi   = if next >= boundaries.len() { None } else { Some(boundaries[next].clone()) };
//! ...
//! cut = next;
//! ```
//!
//! Chunk 1 covers `[-inf, b[64])`. `cut` becomes 64, so chunk 2's `lo` is
//! `b[cut - 1]` = `b[63]` — and `[b[63], b[64])` is merged and pushed a SECOND
//! time. The `lo` that continues seamlessly from the previous chunk's exclusive
//! `hi` is `b[cut]`, not `b[cut - 1]`.
//!
//! # Why no existing test sees it
//!
//! Boundaries are the paged segments' block first-keys, and a block closes at
//! `TARGET_BLOCK_BYTES` = 16 KiB. Reaching a SECOND chunk therefore needs more
//! than 64 blocks — about a megabyte of segment. `compact_paged.rs`'s fixture
//! writes 600 keys of two-byte values, which is a handful of blocks and exactly
//! one chunk, so the loop never advances and the bug cannot fire.
//!
//! At official LDBC SF1 the store is 4.6 GB — on the order of 280,000 blocks
//! and 4,400 chunks, so it fires ~4,399 times per full compaction.
//!
//! # What it costs
//!
//! `SegmentWriter::push` documents "Keys MUST arrive in ascending order" and
//! guards it with a `debug_assert!` (sst.rs:284-288). Pushing a key twice
//! violates that invariant: in a debug build the assertion fires, and in the
//! release builds the bench pod runs it silently writes duplicate keys into a
//! segment that is supposed to be sorted and unique — inflating `entry_count`,
//! `versions` and `tombstones`, which are what the delete-aware compaction
//! trigger reads.
//!
//! This test builds a corpus large enough to reach the second chunk. It is the
//! regression test for the fix.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-chunkbound-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        TmpDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

fn key(i: u32) -> Vec<u8> {
    let mut b = b"K".to_vec();
    b.extend_from_slice(&i.to_be_bytes());
    b
}

/// Enough rows to close well over 64 blocks, so the chunk loop advances at
/// least once and the second chunk's `lo` is exercised.
///
/// A block closes at 16 KiB of payload. 1 KiB values give ~16 entries per
/// block, so 2,000 entries is ~125 blocks — comfortably two chunks, and small
/// enough to stay fast in a debug build.
const ENTRIES: u32 = 2_000;
const VALUE_BYTES: usize = 1024;

fn build(s: &Store) {
    for i in 0..ENTRIES {
        let v = vec![(i % 251) as u8; VALUE_BYTES];
        s.put(&pfx(), &key(i), StoredValue::Plain(v)).expect("put");
        if i % 500 == 499 {
            s.seal();
        }
    }
    s.seal();
}

fn scan_sorted(s: &Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut v = s.scan(&pfx());
    v.sort();
    v
}

/// A compaction spanning MORE THAN ONE CHUNK must answer exactly what it
/// answered before, and must not push a key twice.
///
/// In a debug build the second half is enforced by `SegmentWriter::push`'s own
/// `debug_assert!` — a duplicate key arrives out of ascending order and the
/// assertion fires. That makes the engine's own stated invariant the detector,
/// which is stronger than anything this file could assert from outside.
#[test]
fn a_multi_chunk_compaction_does_not_re_cover_a_boundary_range() {
    let dir = TmpDir::new("multichunk");
    let s = Store::new();
    build(&s);
    let cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");

    let before = scan_sorted(&s);
    assert_eq!(
        before.len(),
        ENTRIES as usize,
        "the fixture must hold every key before compaction"
    );

    let (retired, dropped) = s
        .compact_paged_to_dir(dir.path(), &cache)
        .expect("streaming compaction");
    let after = scan_sorted(&s);

    assert_eq!(
        after, before,
        "a compaction spanning multiple chunks must answer exactly what it \
         answered before — same keys, same values, same order"
    );
    // The compaction retires nothing here (single versions, no deletes), so the
    // counts are reported rather than asserted; what matters is that the walk
    // completed without pushing a key twice.
    eprintln!("[chunk boundary] {retired} version(s) retired, {dropped} key(s) dropped");
    assert_eq!(
        s.segment_count(),
        1,
        "a full compaction leaves exactly one segment"
    );
}

/// The corpus really does span more than one chunk.
///
/// Without this the test above passes on a single-chunk fixture and proves
/// nothing about the loop — which is precisely why the defect survived the
/// existing `compact_paged.rs` suite, whose 600 two-byte values close a handful
/// of blocks and never advance `cut`.
#[test]
fn the_fixture_actually_spans_more_than_one_chunk() {
    let dir = TmpDir::new("spans");
    let s = Store::new();
    build(&s);
    let _cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");

    // One paged segment per seal; their block counts sum to the boundary count
    // the compactor chunks over. `BOUNDARIES_PER_CHUNK` is 64.
    let blocks = s.paged_block_count_for_test();
    eprintln!("[chunk boundary] the fixture's paged segments hold {blocks} block(s)");
    assert!(
        blocks > 64,
        "the fixture must exceed BOUNDARIES_PER_CHUNK (64) blocks or the chunk \
         loop never advances and the test above is vacuous: {blocks}"
    );
}
