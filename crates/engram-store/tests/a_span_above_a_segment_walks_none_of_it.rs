//! A span that starts ABOVE a paged segment reads none of it.
//!
//! `covering_block` learned to answer `None` for a key above the segment's
//! max key (`a_segment_prunes_a_key_above_its_range.rs`), and that is right
//! for a point lookup: `None` means "not here". The range walk read the same
//! `None` as "`lo` sorts before every block" and started at block 0 — and
//! since every block's first key is below `hi` too, nothing stopped it. A
//! span above the segment walked EVERY block of it and kept no row.
//!
//! SNB BI bi11's first leg on the SF3 store (a 13 GB segment and a 56 MB one)
//! made 7,226 adjacency scans and touched 24.7M cached blocks: 3,430 per scan,
//! which is the small segment, whole — 16 KB blocks, 56 MB. Every scan whose
//! prefix sorted above that segment's last key paid for all of it.

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
    p.push(format!("engram-span-above-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// Every block a read touched, whether the cache had it or not.
fn blocks_touched(t: &engram_observe::Trace) -> u64 {
    counter(t, "paged.pread") + counter(t, "paged.block cache hit")
}

/// One sealed segment per batch, in order, reopened paged so every read goes
/// through `PagedSegment`.
fn paged(dir: &std::path::Path, batches: &[&[Vec<u8>]]) -> Store {
    let s = Store::new();
    for batch in batches {
        for k in *batch {
            s.put(&pfx(), k, StoredValue::Plain(vec![0x5a; 64]))
                .expect("put");
        }
        s.seal();
    }
    s.into_paged(dir, 8 << 20).expect("into_paged");
    let segs = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().and_then(|x| x.to_str()) == Some("seg"))
        })
        .count();
    assert_eq!(segs, batches.len(), "the fixture needs one segment per batch");
    let (reopened, _cache) = Store::open_paged_dir(dir, 8 << 20).expect("open paged");
    reopened
}

fn keys(stem: &str, n: u32) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("{stem}-{i:05}").into_bytes()).collect()
}

#[test]
fn a_span_above_the_segment_reads_no_block() {
    let dir = tmp("one");
    let node = keys("node", 4_000);
    let s = paged(&dir, &[&node]);
    let (got, t) = engram_observe::with_trace(|| s.scan_bodies_prefix(&pfx(), b"zzzz"));
    assert!(got.is_empty(), "{} rows above every key", got.len());
    assert_eq!(
        blocks_touched(&t),
        0,
        "a span above the segment walked it: {:?}",
        t.counters()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_small_segment_below_the_span_costs_the_merge_nothing() {
    // bi11's shape: the span lies inside the BIG segment and above the small
    // one. The small one is 4,000 rows (~25 blocks); the span is 100 rows of
    // the big one (one or two blocks).
    let dir = tmp("two");
    let node = keys("node", 400);
    let low = keys("aaa", 4_000);
    let s = paged(&dir, &[&node, &low]);
    let (got, t) = engram_observe::with_trace(|| s.scan_bodies_prefix(&pfx(), b"node-003"));
    let want: Vec<Vec<u8>> = (300..400u32).map(|i| format!("node-{i:05}").into_bytes()).collect();
    assert_eq!(got, want, "the span's rows, in order");
    assert!(
        blocks_touched(&t) <= 3,
        "{} blocks for a 100-row span: the segment below it was walked: {:?}",
        blocks_touched(&t),
        t.counters()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spans_at_every_edge_answer_as_the_rows_written() {
    // THE CONTROL. A start that skips a block holding part of the span is a
    // wrong answer that looks exactly like a fast one, so every edge a
    // segment has — below it, at its first key, inside, at its last key, one
    // byte past it, above it — is checked against the rows written, over two
    // segments whose ranges interleave.
    let dir = tmp("edges");
    let evens: Vec<Vec<u8>> = (0..2_000u32).map(|i| format!("k-{:05}", 2 * i).into_bytes()).collect();
    let odds: Vec<Vec<u8>> = (0..300u32).map(|i| format!("k-{:05}", 2 * i + 1).into_bytes()).collect();
    let s = paged(&dir, &[&evens, &odds]);
    let mut all: Vec<Vec<u8>> = evens.iter().chain(odds.iter()).cloned().collect();
    all.sort();
    for p in [
        &b"a"[..],
        b"k-",
        b"k-0",
        b"k-000",
        b"k-0059",
        b"k-00599",
        b"k-006",
        b"k-01",
        b"k-0399",
        b"k-03998",
        b"k-03999",
        b"k-04",
        b"k-1",
        b"l",
        b"zz",
    ] {
        let got = s.scan_bodies_prefix(&pfx(), p);
        let want: Vec<Vec<u8>> = all.iter().filter(|k| k.starts_with(p)).cloned().collect();
        assert_eq!(got, want, "prefix {:?}", String::from_utf8_lossy(p));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
