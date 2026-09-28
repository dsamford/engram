#![allow(non_snake_case)]
//! A span visitor that stops must stop the WALK, not just its own callback.
//!
//! `merge_span`'s single-paged-segment fast path used a NON-stopping walk with
//! a local `stop` flag: once the visitor said stop, the closure returned early
//! per row while the walk kept fetching, verifying and decoding every remaining
//! block. A reader that wanted one row paid for the whole span — measured by an
//! adversarial review as 142 paged reads to return a single row.
//!
//! It used the non-stopping walk only because `range_for_each_until` could not
//! carry the block cache's SCAN policy. It can now, so the fast path uses it.
//!
//! The fix is invisible to ANSWERS by construction — the same rows come back in
//! the same order — so these tests assert on the two things that do change:
//! the rows the visitor is offered, and `SPAN_STOPPED_EARLY`.

use engram_key::{Kind, KeyPrefix, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue, SPAN_STOPPED_EARLY};
use std::sync::atomic::Ordering::Relaxed;

/// A temp directory that cleans up after itself.
struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "engram-span-stop-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
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
    KeyPrefix { realm: Realm(1), namespace: Namespace(1), kind: Kind::KV, partition: Partition(1) }
}
fn key(i: usize) -> Vec<u8> {
    format!("k{i:08}").into_bytes()
}

/// A store with `n` rows in ONE PAGED segment — the shape the fast path serves,
/// and the only one where the fix is reachable. A resident segment is walked
/// in memory and was never the problem.
fn paged_store(n: usize, dir: &TmpDir) -> Store {
    let store = Store::new();
    for i in 0..n {
        store
            .put(&pfx(), &key(i), StoredValue::Plain(vec![7u8; 32]))
            .expect("seed");
    }
    store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    store
}

#[test]
fn a_visitor_that_stops_is_not_offered_the_rest_of_the_span() {
    const N: usize = 8_192;
    let dir = TmpDir::new("offered");
    let store = paged_store(N, &dir);

    // Stop after the first row. Before the fix the walk continued and the
    // closure was still ENTERED for every remaining key (it returned early
    // inside), so counting entries distinguishes the two behaviours without
    // needing a block-read counter.
    let mut offered = 0usize;
    store.for_each_span(&pfx(), b"k", store.now_ts(), &mut |_body, _val| {
        offered += 1;
        false
    });
    assert_eq!(
        offered, 1,
        "a visitor that stops after one row must be offered exactly one row, not {offered} of {N}"
    );
}

#[test]
fn stopping_is_counted_and_running_to_completion_is_not() {
    const N: usize = 512;
    let dir = TmpDir::new("counted");
    let store = paged_store(N, &dir);

    let before = SPAN_STOPPED_EARLY.load(Relaxed);
    let mut seen = 0usize;
    store.for_each_span(&pfx(), b"k", store.now_ts(), &mut |_b, _v| {
        seen += 1;
        true // never stops
    });
    assert_eq!(seen, N, "a visitor that never stops must see every row");
    assert_eq!(
        SPAN_STOPPED_EARLY.load(Relaxed) - before,
        0,
        "a completed walk must NOT count as an early stop"
    );

    let before = SPAN_STOPPED_EARLY.load(Relaxed);
    let mut seen = 0usize;
    store.for_each_span(&pfx(), b"k", store.now_ts(), &mut |_b, _v| {
        seen += 1;
        seen < 10
    });
    assert_eq!(seen, 10, "the visitor stops itself at ten rows");
    assert_eq!(
        SPAN_STOPPED_EARLY.load(Relaxed) - before,
        1,
        "an early stop must be counted exactly once — a 0 here means termination \
         stopped propagating and the fix has silently regressed"
    );
}

#[test]
fn the_rows_are_unchanged_when_nothing_stops() {
    // The invariant the fix must not break: a full walk still answers exactly
    // what it answered before, in order.
    const N: usize = 1_000;
    let dir = TmpDir::new("unchanged");
    let store = paged_store(N, &dir);
    let mut got: Vec<Vec<u8>> = Vec::new();
    store.for_each_span(&pfx(), b"k", store.now_ts(), &mut |body, _val| {
        got.push(body.to_vec());
        true
    });
    let want: Vec<Vec<u8>> = (0..N).map(key).collect();
    assert_eq!(got.len(), N);
    assert_eq!(got, want, "a full span walk must be unchanged, in order");
}
