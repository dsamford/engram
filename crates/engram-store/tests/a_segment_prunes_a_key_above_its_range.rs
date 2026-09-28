//! A paged segment refuses a key ABOVE its range without reading a block.
//!
//! Pruning ran from below only: `covering_block` returned `None` for a key
//! before the first block's first key, and otherwise picked the LAST block
//! whose first key was `<= key` — which, for a key past everything the file
//! holds, is its final block. That block then faulted in from disk, had its
//! BLAKE3 verified and was searched, to find nothing. A store keeps many
//! segments and at most one can hold a given key, so every point lookup paid
//! that for each segment that cannot hold it.
//!
//! The bound was already computed and thrown away: `SegmentWriter::last_key`
//! existed as a `debug_assert` guard. v4 writes the MAXIMUM key at the tail of
//! the sparse-index region — the region a reader already `pread`s in one go —
//! so the upper bound costs no extra read and no footer field.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-maxkey-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

fn key(i: u32) -> Vec<u8> {
    format!("node-{i:05}").into_bytes()
}

/// `n` keys sealed into segment files under `dir`, reopened paged so every
/// read goes through `PagedSegment`.
fn paged(dir: &std::path::Path, n: u32) -> Store {
    let s = Store::new();
    for i in 0..n {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![(i % 251) as u8; 64]))
            .expect("put");
    }
    s.seal();
    s.into_paged(dir, 8 << 20).expect("into_paged");
    let (reopened, _cache) = Store::open_paged_dir(dir, 8 << 20).expect("open paged");
    reopened
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

#[test]
fn a_key_above_the_segment_reads_no_block() {
    let dir = tmp("above");
    let s = paged(&dir, 400);
    let (got, t) = engram_observe::with_trace(|| s.get(&pfx(), b"zzzz-past-the-end"));
    assert!(got.is_none(), "{got:?}");
    assert!(
        counter(&t, "paged.segment pruned above its max key") >= 1,
        "the bound was used: {:?}",
        t.counters()
    );
    assert_eq!(
        counter(&t, "paged.pread"),
        0,
        "no block faulted in for a key the segment cannot hold: {:?}",
        t.counters()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_key_the_segment_holds_is_still_found() {
    // THE CONTROL. A bound that prunes a key the file HOLDS is a wrong answer,
    // and it would look exactly like a fast one.
    let dir = tmp("holds");
    let s = paged(&dir, 400);
    for i in 0..400u32 {
        assert!(s.get(&pfx(), &key(i)).is_some(), "key {i} went missing");
    }
    // the boundary itself, and one byte past it
    assert!(s.get(&pfx(), &key(399)).is_some(), "the max key");
    assert!(
        s.get(&pfx(), b"node-00399\x00").is_none(),
        "one byte past the max key is absent, not present"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_key_below_the_segment_still_reads_no_block() {
    // The pre-existing lower bound keeps working: the upper bound is additive.
    let dir = tmp("below");
    let s = paged(&dir, 200);
    let (got, t) = engram_observe::with_trace(|| s.get(&pfx(), b"aaaa-before-the-start"));
    assert!(got.is_none(), "{got:?}");
    assert_eq!(counter(&t, "paged.pread"), 0, "{:?}", t.counters());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn randomised_lookups_agree_with_what_was_written() {
    let dir = tmp("random");
    let n = 300u32;
    let s = paged(&dir, n);
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..400 {
        let (k, present) = match next() % 4 {
            0 => (key((next() % u64::from(n)) as u32), true),
            1 => (key(n + (next() % 1000) as u32), false),
            2 => (format!("aaa-{:05}", next() % 1000).into_bytes(), false),
            _ => (format!("zzz-{:05}", next() % 1000).into_bytes(), false),
        };
        assert_eq!(
            s.get(&pfx(), &k).is_some(),
            present,
            "key {:?}",
            String::from_utf8_lossy(&k)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A v3 file — one with no recorded maximum — must still open and read, and
/// must simply not prune from above.
///
/// Synthesised by rewriting a v4 footer's version field back to 3 and
/// re-hashing: the index region then carries a max key the reader never looks
/// for, which is exactly what a v3 reader did with a v4 file's extra bytes.
#[test]
fn a_v3_file_opens_and_prunes_only_from_below() {
    let dir = tmp("v3compat");
    let s = Store::new();
    for i in 0..100u32 {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![7])).expect("put");
    }
    s.seal();
    s.into_paged(&dir, 8 << 20).expect("into_paged");

    let mut rewritten = 0;
    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("seg") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read");
        // footer tail: [.. hashed fields .., version(4), magic(8), hash(32)]
        let n = bytes.len();
        let hashed_len = 8 * 7; // v3/v4 hashed fields
        let tail = hashed_len + 4 + 8 + 32;
        assert!(n > tail, "a full footer");
        let mut out = bytes[..n - 44].to_vec(); // drop version+magic+hash
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&bytes[n - 40..n - 32]); // magic
        let start = out.len() - (hashed_len + 4 + 8);
        let h = blake3::hash(&out[start..]);
        out.extend_from_slice(h.as_bytes());
        std::fs::write(&path, &out).expect("write");
        rewritten += 1;
    }
    assert!(rewritten > 0, "the fixture must have rewritten a segment");

    let (reopened, _cache) = Store::open_paged_dir(&dir, 8 << 20).expect("a v3 file must open");
    assert_eq!(reopened.get(&pfx(), &key(60)), Some(vec![7]), "it still reads");
    let (got, t) = engram_observe::with_trace(|| reopened.get(&pfx(), b"zzzz-past-the-end"));
    assert!(got.is_none());
    assert_eq!(
        counter(&t, "paged.segment pruned above its max key"),
        0,
        "a file that cannot say must not be pruned on a bound it never wrote"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
