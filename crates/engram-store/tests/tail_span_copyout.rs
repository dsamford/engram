#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! COPY-OUT SPAN SNAPSHOT: the fix for engram's one remaining deficit against
//! Neo4j, and the differential that says it changed nothing else.
//!
//! # The mechanism it removes
//!
//! `merge_span` took all 64 tail shard read latches (`Tail::read_all`) and held
//! them until it returned — across the k-way merge, the paged segments' `pread`s
//! and BLAKE3 verification, and every visitor callback. A writer needs exactly
//! one of those shards, so **every span read mutually excluded every writer for
//! its whole duration**, and only when the tail was non-empty — that is, only
//! in a mixed workload.
//!
//! Measured on the bench pod at official LDBC SF1, with the two PURE profiles
//! as the control:
//!
//! | profile | span reads excluding writers | rows merged under the latches |
//! |---|---|---|
//! | read-only | 0 | 0 |
//! | write-only | 0 | 0 |
//! | balanced | 11,681 | 6,024,535 |
//! | write-heavy | 35,723 | 18,092,001 |
//!
//! Zero on both pure profiles, tens of thousands on both mixed ones — which is
//! exactly where engram trailed Neo4j 5.26 (0.63x-0.75x) while leading it
//! everywhere else (1.49x-3.98x).
//!
//! # What this file has to prove
//!
//! The copy changes WHO WAITS. It must not change WHAT IS ANSWERED. A span
//! visitor sees rows in key order with a precedence rule between the tail and
//! the segments, and a copy that reordered them, dropped a shadowed row, or
//! resolved a version differently would be a wrong answer that no throughput
//! number would reveal. So the bar is byte-identical visitor output on both
//! arms, over a corpus built to make precedence matter.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{
    SPAN_READS_EXCLUDING_WRITERS, SPAN_READS_LATCH_FREE, Store, StoredValue,
};
use std::sync::atomic::Ordering;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
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
    let mut b = b"S".to_vec();
    b.extend_from_slice(&i.to_be_bytes());
    b
}

/// A corpus where PRECEDENCE matters: rows sealed into segments, some of them
/// overwritten in the tail, some deleted in the tail, and some live only in the
/// tail. If the copy resolved precedence differently from the borrow, this is
/// the shape that shows it.
fn build(s: &Store) {
    for i in 0..300u32 {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![1, (i % 251) as u8]))
            .expect("put");
    }
    s.seal();
    // Overwrite a third IN THE TAIL: the tail must win over the segment.
    for i in (0..300u32).step_by(3) {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![2, (i % 251) as u8]))
            .expect("overwrite");
    }
    // Delete a quarter IN THE TAIL: a tombstone must shadow the segment's row.
    for i in (0..300u32).step_by(4) {
        s.delete(&pfx(), &key(i));
    }
    // And rows that exist ONLY in the tail.
    for i in 300..360u32 {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![3]))
            .expect("tail only");
    }
}

/// Everything a span visitor sees, in the order it sees it.
fn walk(s: &Store) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    s.for_each_key_span(&pfx(), b"S", u64::MAX, &mut |body| {
        out.push(body.to_vec());
        true
    });
    out
}

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-copyout-{}-{}-{}",
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

/// THE PAGED DIFFERENTIAL — the arm that caught the real defect.
///
/// The first cut of this file tested a RESIDENT store only and passed while the
/// change was wrong. `engram-graph`'s `adjacency_repair_differential.rs` caught
/// it instead, on its paged arm, as a live relationship count of 23,976 against
/// a true 23,975.
///
/// The cause was a guard reading `all.is_none()` to mean "the tail is empty" —
/// true while the latches were taken exactly when the tail had rows, and false
/// once the copy could leave `all` as `None` with rows in hand. The streaming
/// fast path then fired WITH TAIL ROWS PRESENT and dropped them. It is
/// reachable only on a paged store with a single sealed segment, which is why
/// a resident fixture could never see it.
///
/// A differential that does not cover the storage mode the change touches is a
/// differential that passes for the wrong reason.
#[test]
fn the_copy_and_the_borrow_answer_identically_on_a_paged_store() {
    let _serial = serial();
    let dir = TmpDir::new("paged");
    let s = Store::new();
    // EXACTLY ONE SEALED SEGMENT, and it must be PAGED. Both are conditions of
    // the fast path (`sealed.segments.len() == 1` and
    // `segments[0].as_resident().is_none()`), so a fixture with two segments —
    // which `build()` produces, since it seals in the middle — never reaches
    // the branch under test and passes for the wrong reason. The first cut of
    // this test did exactly that.
    for i in 0..300u32 {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![1, (i % 251) as u8]))
            .expect("put");
    }
    s.seal();
    let _cache = s.into_paged(dir.path(), 8 << 20).expect("into_paged");
    assert_eq!(
        s.segment_count(),
        1,
        "the fast path needs exactly one sealed segment, or this fixture does          not exercise it"
    );
    for i in 400..460u32 {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![7]))
            .expect("tail after paging");
    }
    for i in (0..300u32).step_by(7) {
        s.put(&pfx(), &key(i), StoredValue::Plain(vec![8]))
            .expect("overwrite a paged row from the tail");
    }

    s.set_tail_span_copyout(false);
    let borrowed = walk(&s);
    s.set_tail_span_copyout(true);
    let copied = walk(&s);

    assert!(
        borrowed.len() > 300,
        "the paged fixture must produce rows from BOTH the segment and the          tail, or the guard under test is never exercised: {}",
        borrowed.len()
    );
    assert_eq!(
        copied, borrowed,
        "on a PAGED store the copy must answer exactly what the borrow          answers — this is the arm where the streaming fast path is reachable          and where dropping the tail silently costs rows"
    );
}

/// THE DIFFERENTIAL. Both arms must visit byte-identical rows in the same
/// order.
#[test]
fn the_copy_and_the_borrow_answer_identically() {
    let _serial = serial();
    let s = Store::new();
    build(&s);

    s.set_tail_span_copyout(false);
    let borrowed = walk(&s);
    s.set_tail_span_copyout(true);
    let copied = walk(&s);

    assert!(
        !borrowed.is_empty(),
        "the fixture must produce rows, or agreement is vacuous"
    );
    assert_eq!(
        copied, borrowed,
        "the copy changes WHO WAITS, never what is answered — same rows, same \
         order, including the tail's precedence over the segments and its \
         tombstones' shadowing of them"
    );
}

/// THE COUNTER HALF: the copy actually happens, and it actually stops the
/// exclusion.
///
/// Without this the differential above could be comparing the borrow path
/// against itself — the lever unread, the copy never taken — which is precisely
/// how a differential passes while measuring nothing.
#[test]
fn the_copy_removes_the_writer_exclusion() {
    let _serial = serial();
    let s = Store::new();
    build(&s);

    let excl = |on: bool| -> (u64, u64) {
        s.set_tail_span_copyout(on);
        let e0 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed);
        let f0 = SPAN_READS_LATCH_FREE.load(Ordering::Relaxed);
        walk(&s);
        (
            SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e0,
            SPAN_READS_LATCH_FREE.load(Ordering::Relaxed) - f0,
        )
    };
    let (off_excl, _) = excl(false);
    let (on_excl, on_free) = excl(true);

    eprintln!(
        "[tail copy-out] span reads excluding writers: {off_excl} borrowing, \
         {on_excl} copying"
    );
    assert_eq!(
        off_excl, 1,
        "the OFF arm must take the latches, or the ON arm's zero proves nothing"
    );
    assert_eq!(
        on_excl, 0,
        "with the copy in hand there is nothing to hold: no span read may \
         exclude a writer"
    );
    assert_eq!(
        on_free, 1,
        "and the read still happened — a zero exclusion count with a zero read \
         count would be a lever that disabled the scan rather than the latch"
    );
}

/// THE CAP DECLINES rather than truncating.
///
/// Copying is O(rows), and a full-span walk over SF1's adjacency is 17.26M rows
/// — the one allocation a bigger-than-RAM store cannot make. Past the cap the
/// borrow path is kept. Answering SHORT instead would be a wrong answer that no
/// throughput number reveals, so the decline is asserted to still answer
/// everything.
#[test]
fn past_its_cap_the_copy_declines_and_still_answers_everything() {
    let _serial = serial();
    let s = Store::new();
    build(&s);
    s.set_tail_span_copyout(true);

    let full = walk(&s);
    // A cap below the tail's row count in range: the copy must give up.
    s.set_tail_copyout_cap(4);
    let e0 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed);
    let capped = walk(&s);
    let excl = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e0;

    assert_eq!(
        capped, full,
        "a declined copy must fall back to the borrow path and answer EVERY \
         row — truncating at the cap would answer short, silently"
    );
    assert_eq!(
        excl, 1,
        "and the fallback is the borrow path, which does take the latches: a \
         zero here would mean the copy truncated instead of declining"
    );
    s.set_tail_copyout_cap(65_536);
}

/// An EMPTY tail takes neither path — the case that makes a pure-read workload
/// free, and the reason the pod's control arms read zero.
#[test]
fn an_empty_tail_neither_copies_nor_latches() {
    let _serial = serial();
    let s = Store::new();
    build(&s);
    s.seal();
    s.set_tail_span_copyout(true);

    let e0 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed);
    let f0 = SPAN_READS_LATCH_FREE.load(Ordering::Relaxed);
    let rows = walk(&s);
    assert!(!rows.is_empty(), "the sealed corpus must still answer");
    assert_eq!(
        SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e0,
        0,
        "an empty tail excludes nobody"
    );
    assert_eq!(
        SPAN_READS_LATCH_FREE.load(Ordering::Relaxed) - f0,
        1,
        "and the read is counted on the latch-free arm"
    );
}
