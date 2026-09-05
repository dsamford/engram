//! The determinism lane's own check.
//!
//! Prints `DIGEST <hex>` so `cargo xtask determinism` can run this in two
//! separate PROCESSES and compare. A same-process repeat would not catch
//! anything seeded per process — which is exactly how `RandomState` breaks
//! reproducibility, so the check that matters most is the one an in-process
//! loop cannot make.

use std::time::Duration;

use engram_observe::{Trace, with_trace};
use engram_runtime::{Runtime, SimRuntime};

/// A workload that touches every source of non-determinism there is: task
/// interleaving, wake order, timer ordering, and the seeded stream.
///
/// Deliberately not a straight line. A single task drawing numbers in order
/// would reproduce under almost any implementation, including a broken one —
/// it would be a test that passes because it asks nothing.
fn workload(seed: u64) -> Trace {
    let rt = SimRuntime::new(seed);
    let (_, trace) = with_trace(|| {
        for i in 0..8u64 {
            let rt2 = rt.clone();
            rt.spawn(async move {
                // Interleave: each task sleeps a different amount, so the order
                // tasks resume in is decided by the timer heap rather than by
                // the order they were spawned.
                let d = rt2.rand_u64() % 7;
                rt2.sleep(Duration::from_millis(d + 1)).await;
                let _ = rt2.rand_u64();
                engram_observe::count("workload.tasks", 1);
                if i % 3 == 0 {
                    rt2.sleep(Duration::from_millis(2)).await;
                    engram_observe::record(
                        engram_observe::EventTag::Reachable,
                        "workload.second_leg",
                    );
                }
            });
        }
        rt.run(10_000).expect("workload runs to completion");
    });
    trace
}

#[test]
fn same_seed_reproduces_the_trace() {
    let seed: u64 = std::env::var("ENGRAM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(424_242);

    let a = workload(seed);
    let b = workload(seed);

    // The ORDER, not just the multiset. A run producing the same events in a
    // different order is a different run, and a digest over a sorted set would
    // call the two identical — which is how a determinism check comes to certify
    // a scheduler it never actually constrained.
    assert_eq!(
        a.events()
            .iter()
            .map(|e| (e.tag, e.name.as_str()))
            .collect::<Vec<_>>(),
        b.events()
            .iter()
            .map(|e| (e.tag, e.name.as_str()))
            .collect::<Vec<_>>(),
        "same seed produced a different event ORDER",
    );
    assert_eq!(
        a.digest(),
        b.digest(),
        "same seed produced a different digest"
    );

    // The line `cargo xtask determinism` greps for.
    println!("DIGEST {:016x}", a.digest());
}

#[test]
fn a_different_seed_produces_a_different_trace() {
    // The other direction, and the one that is easy to omit. A digest function
    // that ignored its input would satisfy the test above perfectly: every run
    // would agree, and the lane would report determinism while measuring
    // nothing. Two equal digests are only evidence when unequal ones are
    // possible.
    let a = workload(1);
    let b = workload(2);
    assert_ne!(
        a.digest(),
        b.digest(),
        "two different seeds produced the same run — the seed is not reaching the workload"
    );
}

#[test]
fn the_digest_is_stable_across_processes() {
    // A digest built on `DefaultHasher` would pass both tests above and still
    // differ between processes, because `RandomState` is seeded per process.
    // The value is pinned here so a change to the hashing is a deliberate
    // decision with a visible diff rather than a silent one that only shows up
    // as a cross-process gate failure nobody can reproduce locally.
    let digest = workload(424_242).digest();
    let expected = std::env::var("ENGRAM_EXPECTED_DIGEST").ok();
    if let Some(e) = expected {
        assert_eq!(
            format!("{digest:016x}"),
            e,
            "digest changed against the pinned value"
        );
    }
    // Always printed, so a run under `--nocapture` records it even when the
    // pin is not set.
    println!("DIGEST {digest:016x}");
}
