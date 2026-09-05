#![allow(non_snake_case)]
//! Streaming compaction for the PAGED path answers exactly what the resident
//! compactor answers.
//!
//! `Store::compact` materialises its merged output as a RESIDENT segment — the
//! one allocation a bigger-than-RAM store cannot make. So the server never
//! called compaction on the paged path at all, and paged is the mode the large
//! corpora serve in. Segments and their TOMBSTONES accumulated for the life of
//! the process, and every O(segments) path got monotonically slower with
//! uptime.
//!
//! The tiering loop could not even refuse safely: a paged segment is sized
//! `usize::MAX`, but `young_total.saturating_mul(2)` saturates too, so on an
//! all-paged store `sizes[j] > young_total * 2` is false for every segment and
//! the loop selects ALL of them — then tries to build one resident segment out
//! of the entire corpus.
//!
//! The bar here is not "it compacts". It is that a store compacted this way is
//! INDISTINGUISHABLE by reads from one compacted the old way.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-cpaged-{}-{}-{}",
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

/// A deterministic corpus with interleaved seals, overwrites and deletes — the
/// shapes retention has to decide about.
fn build(s: &Store) {
    for i in 0..600u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1, (i % 251) as u8]))
            .expect("put");
        if i % 50 == 0 {
            s.seal();
        }
    }
    // Overwrite a third: multi-version chains.
    for i in (0..600u32).step_by(3) {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![2, (i % 251) as u8]))
            .expect("overwrite");
    }
    s.seal();
    // Delete a quarter: tombstones, some over multi-version chains.
    for i in (0..600u32).step_by(4) {
        s.delete(&pfx(), &i.to_be_bytes());
    }
    s.seal();
}

fn full_scan(s: &Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut v = s.scan(&pfx());
    v.sort();
    v
}

/// THE differential: the same corpus, compacted two ways, must read identically.
#[test]
fn streaming_paged_compaction_answers_exactly_like_the_resident_compactor() {
    // Resident arm.
    let resident = Store::new();
    build(&resident);
    let before = full_scan(&resident);
    resident.compact();
    let resident_after = full_scan(&resident);
    assert_eq!(
        before, resident_after,
        "compaction may retire unreachable versions, never change an answer"
    );

    // Paged arm: same corpus, paged out, then compacted by the streaming path.
    let dir = TmpDir::new("differential");
    let paged = Store::new();
    build(&paged);
    let cache = paged.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let paged_before = full_scan(&paged);
    assert_eq!(
        before, paged_before,
        "paging alone must not change an answer either"
    );

    let (retired, dropped) = paged
        .compact_paged_to_dir(dir.path(), &cache)
        .expect("streaming compaction");
    let paged_after = full_scan(&paged);

    assert_eq!(
        resident_after, paged_after,
        "the streaming compactor must answer EXACTLY what the resident one \
         answers — same keys, same values, same order"
    );
    assert!(
        retired > 0 || dropped > 0,
        "the fixture must actually give compaction something to reclaim, or \
         this differential compares two no-ops"
    );
    assert_eq!(
        paged.segment_count(),
        1,
        "a full compaction leaves exactly one segment"
    );
}

/// Tombstones are actually RECLAIMED — the reason the paged path needed this at
/// all. Without it the ratio the trigger reads never falls and it fires for ever.
#[test]
fn streaming_compaction_reclaims_tombstones_on_the_paged_path() {
    let dir = TmpDir::new("reclaim");
    let s = Store::new();
    build(&s);
    let cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let (before_ratio, before_versions) = s.tombstone_ratio();
    assert!(
        before_ratio > 0.0 && before_versions > 0,
        "the fixture must hold tombstones on the paged path"
    );

    s.compact_paged_to_dir(dir.path(), &cache)
        .expect("compaction");
    let (after_ratio, after_versions) = s.tombstone_ratio();
    eprintln!(
        "[paged compaction] tombstone ratio {before_ratio:.3} ({before_versions} versions) \
         -> {after_ratio:.3} ({after_versions} versions)"
    );
    assert!(
        after_ratio < before_ratio,
        "compaction must lower the density the trigger fires on: \
         {before_ratio} -> {after_ratio}"
    );
    assert!(
        after_versions < before_versions,
        "and it must actually retire versions: {before_versions} -> {after_versions}"
    );
}

/// A reader PINNED before the compaction must still see what it could see.
///
/// This is the invariant that makes compaction safe to run online at all, and
/// the streaming path must honour it exactly as the resident path does.
#[test]
fn a_pinned_reader_still_sees_what_it_could_before() {
    let dir = TmpDir::new("pinned");
    let s = Store::new();
    build(&s);
    let cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");

    let pin = s.pin_snapshot();
    let at = pin.ts();
    let before: Vec<(Vec<u8>, Vec<u8>)> = {
        let mut v = s.scan_at(&pfx(), at);
        v.sort();
        v
    };
    // Write MORE after the pin, so the watermark cannot simply be "everything".
    for i in 600..700u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![3]))
            .expect("put");
    }
    s.seal();

    s.compact_paged_to_dir(dir.path(), &cache)
        .expect("compaction");
    let after: Vec<(Vec<u8>, Vec<u8>)> = {
        let mut v = s.scan_at(&pfx(), at);
        v.sort();
        v
    };
    assert_eq!(
        before, after,
        "a pinned reader's view must survive a compaction that ran underneath it"
    );
    drop(pin);
}

/// A store with one segment is already compacted; saying so by doing nothing is
/// cheaper than rewriting it to itself.
#[test]
fn a_single_segment_store_is_left_alone() {
    let dir = TmpDir::new("single");
    let s = Store::new();
    for i in 0..50u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1]))
            .expect("put");
    }
    let cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");
    assert_eq!(s.segment_count(), 1);
    let files_before = std::fs::read_dir(dir.path()).expect("read_dir").count();
    let got = s
        .compact_paged_to_dir(dir.path(), &cache)
        .expect("compaction");
    assert_eq!(got, (0, 0), "nothing to do");
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("read_dir").count(),
        files_before,
        "and no file was written"
    );
}

/// The inputs' FILES leave the directory with them. Before this, every
/// compaction left a generation on disk: 1,010 files for 50 live segments
/// forty minutes into an SF3 load, the SF1 bulk store at 7x its live size.
/// After a compaction the directory must hold exactly the live set — the
/// merged output plus anything sealed during the merge — and the store must
/// still answer from it.
#[test]
fn a_compaction_unlinks_the_segments_it_merged() {
    let dir = TmpDir::new("unlink");
    let s = Store::new();
    build(&s);
    let cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let seg_files = || -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("seg-"))
            .collect();
        v.sort();
        v
    };
    let before = seg_files();
    assert!(before.len() > 1, "the fixture must spill several segments: {before:?}");
    let want = full_scan(&s);
    let ((retired, _), trace) = engram_observe::with_trace(|| {
        s.compact_paged_to_dir(dir.path(), &cache).expect("compaction")
    });
    assert!(retired > 0, "the fixture must retire something");
    let after = seg_files();
    let live = s.segment_count();
    assert_eq!(
        after.len(),
        live,
        "the directory must hold exactly the live set after a compaction: \
         {} file(s) for {live} live segment(s) — {after:?}",
        after.len()
    );
    assert!(
        !after.iter().any(|f| before.contains(f)),
        "every input file must be gone: {after:?} vs inputs {before:?}"
    );
    assert_eq!(
        trace
            .counters()
            .get("store.paged compaction unlinked an input segment")
            .copied()
            .unwrap_or(0) as usize,
        before.len(),
        "each input unlinked exactly once"
    );
    assert_eq!(full_scan(&s), want, "and the store still answers from the merged file");
}
