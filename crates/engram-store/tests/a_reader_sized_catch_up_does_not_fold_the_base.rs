#![allow(non_snake_case)]
//! `RangeIndex::with_changes` folds the WHOLE base once pending overlay passes
//! `FOLD_AT` (4,096) — and `folded()` walks and clones every entry. The catch-up
//! that calls it runs on a READER's query thread, before the requested key is
//! looked up, so a point seek returning one row can pay O(base) copying. As the
//! base grows the same 4,096-change threshold costs more, which is the SF10
//! shape.
//!
//! These tests pin the cost at the level an adversarial review found it: on the
//! index itself, where the behaviour is unambiguous, rather than through Cypher
//! where intervening catch-ups keep the overlay small and the defect is not
//! reached.

use std::collections::BTreeMap;

use engram_store::{IndexDef, IndexKey, RangeIndex};

fn body(i: u64) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}

/// A base of `n` entries, at ts 1.
fn base(n: u64) -> RangeIndex {
    let def = IndexDef::new(1, engram_store::PropertyId(7));
    let entries: Vec<(IndexKey, Vec<u8>)> =
        (0..n).map(|i| (IndexKey::Int(i as i64), body(i))).collect();
    RangeIndex::from_entries(def, 1, entries, 0)
}

/// `n` fresh inserts as a change set.
fn inserts(from: u64, n: u64) -> BTreeMap<Vec<u8>, Option<IndexKey>> {
    (from..from + n)
        .map(|i| (body(i), Some(IndexKey::Int(i as i64))))
        .collect()
}

#[test]
fn a_small_change_set_does_not_fold_however_large_the_base() {
    // The property that must hold: work is proportional to the CHANGE, not to
    // the base. A handful of inserts against a large base must not clone it.
    let big = base(200_000);
    let (_, t) = engram_observe::with_trace(|| {
        let next = big.with_changes(&inserts(1_000_000, 8), 2).expect("applies");
        std::hint::black_box(next.live_entries().count());
    });
    let folds = t.counters().get("index.overlay folds").copied().unwrap_or(0);
    assert_eq!(
        folds, 0,
        "8 inserts against a 200,000-entry base folded it: {:?}",
        t.counters()
    );
}

#[test]
fn the_fold_threshold_is_reached_by_the_change_count_alone() {
    // FOLD_AT is 4,096 over `added + removed`, and a fresh insert contributes to
    // both — so ~2,049 distinct inserts reach it. This documents the arithmetic
    // that makes a reader-side fold easy to hit under ordinary write load.
    let small = base(1_000);
    let (_, t) = engram_observe::with_trace(|| {
        let next = small
            .with_changes(&inserts(1_000_000, 2_100), 2)
            .expect("applies");
        std::hint::black_box(next.live_entries().count());
    });
    let folds = t.counters().get("index.overlay folds").copied().unwrap_or(0);
    println!("2,100 inserts over a 1,000-entry base -> folds={folds}");
    // Recorded, not asserted as a defect: folding a small base is cheap. The
    // defect is the same threshold against a LARGE base, below.
}

#[test]
fn the_cost_of_crossing_the_threshold_grows_with_the_base() {
    // The scaling claim, made concrete: the SAME change count against bases
    // 20x apart. If crossing the threshold folds, the larger base pays 20x the
    // copying for identical work — which is why SF10 hurts where SF3 does not.
    let n = 2_100u64;
    let small = base(10_000);
    let large = base(200_000);

    let (_, ts) = engram_observe::with_trace(|| {
        std::hint::black_box(
            small
                .with_changes(&inserts(1_000_000, n), 2)
                .expect("applies")
                .live_entries()
                .count(),
        );
    });
    let (_, tl) = engram_observe::with_trace(|| {
        std::hint::black_box(
            large
                .with_changes(&inserts(1_000_000, n), 2)
                .expect("applies")
                .live_entries()
                .count(),
        );
    });
    let fs = ts.counters().get("index.overlay folds").copied().unwrap_or(0);
    let fl = tl.counters().get("index.overlay folds").copied().unwrap_or(0);
    println!("same {n} changes: 10k base folds={fs}, 200k base folds={fl}");
    assert_eq!(
        fs, fl,
        "the fold decision must not depend on base size — it depends only on the \
         change count, which is exactly why a large base is punished"
    );
}
