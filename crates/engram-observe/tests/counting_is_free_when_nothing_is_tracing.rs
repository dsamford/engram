#![allow(non_snake_case)]
//! Fix 123: `count` and `record` check a plain `Cell<bool>` before touching
//! the trace.
//!
//! Both used to reach straight for the `RefCell<Option<Trace>>`, so every
//! `counted!` in the engine paid a `borrow_mut` and an `Option` test whether
//! or not anything was recording. That is not a rounding error at the places
//! it is called from: `adj_snap_memo_serve`'s hit arm fires TWO per probe and
//! LSQB q3 makes 107,386,468 probes, so an untraced production run paid a
//! quarter of a billion `RefCell` borrows to record nothing at all.
//!
//! The flag is written only where the trace is written, so the observable
//! behaviour must not move an inch. That is what this file asserts: the
//! counters recorded under a trace are exactly what they were, suppression
//! still suppresses and still restores, and the untraced path is a no-op that
//! cannot panic even while a borrow would have conflicted.
//!
//! The SPEED claim is not asserted here — a microbenchmark of a thread-local
//! read against a `RefCell` borrow would measure the harness. The engine-level
//! evidence is the determinism digest holding at 5483cf2ea8e8fc46 across the
//! change, plus the q3 measurement on the benchmark pod.

use engram_observe::{count, counted, tracing, with_suppressed_trace, with_trace};

#[test]
fn a_nothing_is_tracing_outside_with_trace() {
    assert!(!tracing(), "a trace was installed before any test ran");
    // The no-op path, exercised. If this ever panics or records, the fast
    // path has diverged from the slow one.
    for _ in 0..1_000 {
        counted!("test.untraced counter");
    }
    count("test.untraced counter", 41);
    assert!(!tracing(), "counting installed a trace");
}

#[test]
fn b_a_trace_records_exactly_what_it_used_to() {
    let (out, trace) = with_trace(|| {
        assert!(tracing(), "the flag is not set INSIDE with_trace");
        counted!("test.one");
        counted!("test.one");
        counted!("test.many", 40);
        7u32
    });
    assert_eq!(out, 7);
    assert_eq!(trace.counters().get("test.one").copied(), Some(2));
    assert_eq!(trace.counters().get("test.many").copied(), Some(40));
    assert!(!tracing(), "the flag survived with_trace");
}

#[test]
fn c_suppression_still_suppresses_and_still_restores() {
    let (_, trace) = with_trace(|| {
        counted!("test.before");
        with_suppressed_trace(|| {
            assert!(
                !tracing(),
                "suppression left the flag set, so the fast path would record"
            );
            counted!("test.during");
        });
        assert!(tracing(), "suppression did not restore the flag");
        counted!("test.after");
    });
    assert_eq!(trace.counters().get("test.before").copied(), Some(1));
    assert_eq!(
        trace.counters().get("test.during").copied(),
        None,
        "a suppressed counter was recorded anyway"
    );
    assert_eq!(trace.counters().get("test.after").copied(), Some(1));
}

#[test]
fn d_suppression_outside_a_trace_is_still_a_no_op() {
    assert!(!tracing());
    with_suppressed_trace(|| {
        counted!("test.nowhere");
    });
    assert!(!tracing(), "suppression outside a trace set the flag");
}

#[test]
fn e_the_flag_agrees_with_the_trace_on_every_path() {
    // The one invariant that makes the fast path sound: `tracing()` is true
    // exactly when a trace would have been found. Asserted across the three
    // transitions rather than assumed.
    assert!(!tracing());
    let (inner_seen, trace) = with_trace(|| {
        let a = tracing();
        let b = with_suppressed_trace(tracing);
        let c = tracing();
        counted!("test.probe");
        (a, b, c)
    });
    assert_eq!(
        inner_seen,
        (true, false, true),
        "the flag disagreed with the trace"
    );
    assert_eq!(trace.counters().get("test.probe").copied(), Some(1));
    assert!(!tracing());
}
