#![allow(non_snake_case)]
//! The row budget is a SHARED POOL, not a per-statement grant.
//!
//! Before this, `row_budget` was handed in full to every concurrent statement.
//! The server derives it from the cgroup ceiling (`ceiling / 4 / 96 B`), so at
//! the 160Gi bench pod four concurrent statements were entitled to the whole
//! container and 32 to EIGHT TIMES it. That over-commit OOM-killed two bench
//! pods on 2026-09-11, and bounding the budget by hand closed only half the SF10
//! write-path gap (0.27 -> 0.55 of SF3).
//!
//! It cannot be fixed by dividing harder up front: LSQB q7 at SF10 needs
//! 447,392,426 rows, which is EXACTLY what 160Gi derives, so any constant that
//! bounds 8 concurrent statements refuses q7
//! (engram-server/tests/the_row_budget_cannot_bound_concurrency_and_admit_q7.rs
//! proves no divisor in 1..=64 satisfies both). Dividing by the statements
//! ACTUALLY IN FLIGHT satisfies both, and that is what these tests pin.
//!
//! ITS OWN TEST BINARY: `STATEMENTS_IN_FLIGHT` is process-global, so a test
//! running a statement in another binary would perturb these counts.

use engram_graph::interp::{STATEMENTS_IN_FLIGHT, shared_row_budget};
use std::sync::atomic::Ordering::Relaxed;

/// q7 at SF10, live-verified, and exactly what a 160Gi container derives.
const Q7_ROWS: usize = 447_392_426;

/// EVERY CALLER TAKES THIS LOCK.
///
/// `STATEMENTS_IN_FLIGHT` is process-global and this helper STORES to it, so
/// two tests in flight together read each other's value. The file header
/// already noted the cross-binary version of this hazard; the tests inside one
/// binary run in parallel too, which is the half it missed. Observed as
/// `n_concurrent_statements_cannot_exceed_the_pool` failing only inside a full
/// `cargo test --workspace` and passing five times out of five alone.
static IN_FLIGHT: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_in_flight<T>(n: usize, f: impl FnOnce() -> T) -> T {
    let _guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    STATEMENTS_IN_FLIGHT.store(n, Relaxed);
    let out = f();
    STATEMENTS_IN_FLIGHT.store(0, Relaxed);
    out
}

#[test]
fn one_statement_alone_still_gets_the_whole_pool() {
    // REQUIREMENT A. q7 ran with no flag at SF10 and must continue to: alone, a
    // statement's share is the entire configured grant.
    for n in [0usize, 1] {
        let share = with_in_flight(n, || shared_row_budget(Q7_ROWS));
        assert_eq!(
            share, Q7_ROWS,
            "with {n} in flight a statement must keep the whole pool, else q7 regresses"
        );
    }
}

#[test]
fn n_concurrent_statements_cannot_exceed_the_pool() {
    // REQUIREMENT B, the one that was violated. The SUM of every in-flight
    // statement's share must not exceed the configured grant.
    for n in [2usize, 4, 8, 16, 32, 64] {
        let share = with_in_flight(n, || shared_row_budget(Q7_ROWS));
        let committed = share.saturating_mul(n);
        assert!(
            committed <= Q7_ROWS,
            "{n} statements x {share} rows = {committed} exceeds the pool of {Q7_ROWS}"
        );
        println!("{n:>3} in flight: share {share:>12}, committed {committed:>13} of {Q7_ROWS}");
    }
}

#[test]
fn the_share_never_falls_to_zero() {
    // A statement that materialises a handful of rows must not be refused just
    // because the server is busy — the floor keeps trivial work servable.
    let share = with_in_flight(100_000, || shared_row_budget(Q7_ROWS));
    assert!(share > 0, "the share must never reach zero");
    assert!(
        share >= 1_000_000.min(Q7_ROWS),
        "the floor must hold at extreme concurrency (got {share})"
    );
}

#[test]
fn a_tiny_configured_budget_is_never_raised_by_the_floor() {
    // The floor is `min(MIN_SHARED_SHARE, configured)`, so a deliberately tiny
    // budget stays tiny: the pool may never hand out MORE than was configured,
    // which would be the floor quietly overriding an operator's choice.
    let configured = 1_000usize;
    for n in [1usize, 2, 32] {
        let share = with_in_flight(n, || shared_row_budget(configured));
        assert!(
            share <= configured,
            "with {n} in flight the share {share} exceeded the configured {configured}"
        );
    }
}
