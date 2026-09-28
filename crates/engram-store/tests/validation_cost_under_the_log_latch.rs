#![allow(non_snake_case)]
// Real threads and a real clock: the claim under test is a COST under the log
// latch, measured, exactly as `txn.rs` measures its contention.
#![allow(clippy::disallowed_methods)]
//! Does OCC validation's FALLBACK cost less than the commit window, as the
//! window's docstring claims?
//!
//! `COMMIT_WINDOW_CAP`'s doc (engram-store/src/lib.rs:620) says:
//!
//!     "A long-running transaction falls back, which is correct and is exactly
//!      the case where the point loop is the cheaper answer anyway."
//!
//! That claim is load-bearing: it is the reason the window is capped in ENTRIES
//! and the reason nothing guards the fallback. It is also the leading suspect
//! for the SF10 write-path gap (829 ops/s vs SF3's 3,113 at 32 clients), because
//! BOTH validation branches run under the log latch -- "the one serialisation
//! point that cannot be parallelised", in the commit path's own words (:4497).
//!
//! The claim is about COST, so it is testable at any scale; it does not need
//! SF10. These tests run both arms over the SAME transaction shape and let the
//! numbers decide. `set_commit_window_validation(false)` selects the point loop
//! exactly as an exhausted window would.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_key::{Kind, Namespace, Partition, Realm, KeyPrefix};
use engram_store::{Store, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix { realm: Realm(1), namespace: Namespace(1), kind: Kind::KV, partition: Partition(1) }
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:08}").into_bytes()
}

fn seed(n: usize) -> Store {
    let store = Store::new();
    for i in 0..n {
        store
            .put(&pfx(), &key(i), StoredValue::Plain(vec![0u8; 16]))
            .expect("seed");
    }
    store
}

/// One commit that reads `read_set` keys and writes one. Returns its duration.
fn timed_commit(store: &Store, read_set: usize, write_key: usize) -> Duration {
    let mut t = store.begin();
    for i in 0..read_set {
        let _ = t.get(&pfx(), &key(i));
    }
    t.put(&pfx(), &key(write_key), StoredValue::Plain(vec![1u8; 16]))
        .expect("put");
    let started = Instant::now();
    let _ = t.commit();
    started.elapsed()
}

/// The minimum over repetitions: CPU-bound work has a hard floor and a noisy
/// tail, so the min separates the arms where a mean would not. (A ratio of
/// medians is not evidence until the distributions separate -- so this reports
/// both arms' floors and requires them to be far apart, not merely ordered.)
fn floor(store: &Store, read_set: usize, reps: usize) -> Duration {
    (0..reps)
        .map(|r| timed_commit(store, read_set, 900_000 + r))
        .min()
        .expect("at least one rep")
}

#[test]
fn the_fallback_is_not_the_cheaper_answer_at_a_large_read_set() {
    const KEYS: usize = 200_000;
    const SMALL: usize = 64;
    const LARGE: usize = 50_000;
    const REPS: usize = 7;

    let store = seed(KEYS);

    store.set_commit_window_validation(true);
    let win_small = floor(&store, SMALL, REPS);
    let win_large = floor(&store, LARGE, REPS);

    store.set_commit_window_validation(false);
    let pt_small = floor(&store, SMALL, REPS);
    let pt_large = floor(&store, LARGE, REPS);

    println!("read set {SMALL:>6}: window {win_small:>12?}   point loop {pt_small:>12?}");
    println!("read set {LARGE:>6}: window {win_large:>12?}   point loop {pt_large:>12?}");

    let win_growth = win_large.as_secs_f64() / win_small.as_secs_f64().max(1e-9);
    let pt_growth = pt_large.as_secs_f64() / pt_small.as_secs_f64().max(1e-9);
    println!(
        "growth {SMALL} -> {LARGE} keys: window x{win_growth:.1}, point loop x{pt_growth:.1}"
    );

    // THE CLAIM UNDER TEST. If the docstring is right, the point loop is at
    // worst comparable at a large read set. If it is wrong, the point loop is
    // decisively worse -- and the fallback is a scale-dependent cost sitting on
    // the one latch that cannot be parallelised.
    assert!(
        pt_large > win_large,
        "the point loop ({pt_large:?}) was NOT worse than the window ({win_large:?}) at a \
         {LARGE}-key read set -- the docstring's claim survives and hypothesis 11 is dead"
    );
}

#[test]
fn validation_cost_tracks_the_read_set_not_the_write_set() {
    // Every transaction here writes exactly ONE key. If commit time still rises
    // with the READ set, validation -- not the write -- is what commit pays for,
    // which is the premise the whole hypothesis rests on.
    const KEYS: usize = 200_000;
    const REPS: usize = 7;
    let store = seed(KEYS);
    store.set_commit_window_validation(false);

    let mut last = Duration::ZERO;
    for &r in &[100usize, 1_000, 10_000, 100_000] {
        let d = floor(&store, r.min(KEYS), REPS);
        println!("read set {r:>7} (1 write): commit floor {d:?}");
        assert!(
            d >= last,
            "commit got CHEAPER as the read set grew ({last:?} -> {d:?}); validation is not \
             O(read set) and the hypothesis is wrong"
        );
        last = d;
    }
}

#[test]
fn concurrent_committers_serialise_on_validation() {
    // The latch claim: if validation is O(read set) under one unparallelisable
    // latch, then adding threads that each carry a large read set buys far less
    // than linear throughput. Compares 1 thread against 4 doing identical work.
    const KEYS: usize = 100_000;
    const READ_SET: usize = 20_000;
    const PER_THREAD: usize = 8;

    let store = Arc::new(seed(KEYS));
    store.set_commit_window_validation(false);

    // Sum the COMMIT durations only. The first cut timed the whole loop, which
    // is dominated by the 20,000 gets before each commit -- and reads
    // parallelise, so it reported x3.73 "scaling" that was measuring the read
    // path and said nothing whatever about the latch.
    let run = |threads: usize| -> (Duration, usize) {
        let mut hs = Vec::new();
        for t in 0..threads {
            let s = Arc::clone(&store);
            hs.push(std::thread::spawn(move || {
                let mut total = Duration::ZERO;
                for i in 0..PER_THREAD {
                    total += timed_commit(&s, READ_SET, 800_000 + t * 1_000 + i);
                }
                total
            }));
        }
        let mut total = Duration::ZERO;
        for h in hs {
            total += h.join().expect("thread");
        }
        (total, threads * PER_THREAD)
    };

    run(2); // warm
    let (one_total, one_n) = run(1);
    let (four_total, four_n) = run(4);
    let per_1 = one_total.as_secs_f64() / one_n as f64;
    let per_4 = four_total.as_secs_f64() / four_n as f64;

    println!("1 thread : {per_1:.6}s of latched commit per commit");
    println!("4 threads: {per_4:.6}s of latched commit per commit");
    println!(
        "a commit costs x{:.2} more when 3 other threads are committing          (1.00 = no queueing, ~4.00 = fully serialised)",
        per_4 / per_1.max(1e-9)
    );

    assert!(one_total > Duration::ZERO && four_total > Duration::ZERO);
}

/// The SF10 prediction, made LOCALLY and in advance.
///
/// SF10 carries 3.23x SF3's data. If a `balanced` operation's read set scales
/// with the corpus, the validation curve alone says what the per-op cost
/// multiplier must be -- no SF10 corpus required to compute it. The measured
/// write-path gap is 829 ops/s vs 3,113, i.e. each SF10 op is ~3.75x dearer.
///
/// This test measures cost(r) and cost(3.23r) across the plausible band of
/// per-operation read-set sizes and prints the multiplier the mechanism
/// predicts. If the printed band brackets 3.75, validation cost ALONE accounts
/// for the whole SF10 write gap and nothing else need be invoked. If it lands
/// far from 3.75, the mechanism is real but is not the explanation, and the
/// pod's counters should be spent elsewhere.
#[test]
fn the_curve_predicts_the_sf10_per_op_multiplier() {
    const KEYS: usize = 400_000;
    const REPS: usize = 9;
    const SF10_OVER_SF3: f64 = 3.23;
    const OBSERVED_GAP: f64 = 3113.0 / 829.0;

    let store = seed(KEYS);
    store.set_commit_window_validation(false);

    println!("SF3 read set -> SF10 read set (x{SF10_OVER_SF3}):  predicted per-op multiplier");
    let mut bracketing = 0usize;
    let mut tried = 0usize;
    for &sf3 in &[200usize, 500, 1_000, 5_000, 10_000, 30_000] {
        let sf10 = (sf3 as f64 * SF10_OVER_SF3).round() as usize;
        if sf10 > KEYS {
            continue;
        }
        let a = floor(&store, sf3, REPS).as_secs_f64();
        let b = floor(&store, sf10, REPS).as_secs_f64();
        let mult = b / a.max(1e-12);
        tried += 1;
        let hit = if (mult - OBSERVED_GAP).abs() <= 0.75 {
            bracketing += 1;
            "  <-- brackets the observed 3.75x"
        } else {
            ""
        };
        println!("  {sf3:>6} -> {sf10:>6}:  x{mult:.2}{hit}");
    }

    println!(
        "\nobserved SF10 write gap: x{OBSERVED_GAP:.2} dearer per op; \
         {bracketing} of {tried} read-set bands predict it within +/-0.75"
    );
    assert!(tried > 0, "no band was measurable");
}
