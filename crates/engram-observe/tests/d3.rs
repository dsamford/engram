#![allow(non_snake_case)]
//! D3 — registration, the gate/canary pairing, and the coverage floor.
//!
//! The floor is the reason this crate exists: **a `sometimes!` that never fires
//! is a build failure, not neutral.** Most of what follows is about keeping
//! "never observed" distinguishable from "does not exist", because collapsing
//! them is what lets a simulation that never injects a fault pass everything.

use engram_observe::{
    Canary, EventTag, Gate, Kind, Registration, SometimesEvent, Subsystem, Trace, coverage,
    register, with_trace,
};

// ─── The gate/canary pairing ────────────────────────────────────────────────

#[test]
fn a_gate_carries_at_least_one_canary() {
    let g = Gate::new(
        "no unindexed tail is skipped",
        Canary::new("drop the tail and assert the count still matches"),
    );
    assert_eq!(g.canaries().len(), 1);

    let g = g.and_canary(Canary::new(
        "return partial results without the incomplete flag",
    ));
    assert_eq!(g.canaries().len(), 2);
}

// A gate with NO canary is not tested here because it cannot be written: the
// only constructor takes one by value. That is the point — R14 asks for "a gate
// without a canary structurally impossible to add", and a test asserting the
// impossible state is rejected would be weaker than a type that cannot express
// it. If `Gate::new(name)` is ever added, this comment is the thing that should
// stop it.

// ─── Registration completeness ──────────────────────────────────────────────

struct Complete;
impl Subsystem for Complete {
    const NAME: &'static str = "complete";
    fn register() -> Registration {
        Registration::new()
            .crash_point("before_commit")
            .sometimes("compaction hit ENOSPC")
            .counter("commits")
            .gate(Gate::new(
                "commits are durable",
                Canary::new("skip the fsync"),
            ))
    }
}

struct MissingTwo;
impl Subsystem for MissingTwo {
    const NAME: &'static str = "missing-two";
    fn register() -> Registration {
        Registration::new().crash_point("x").counter("y")
    }
}

#[test]
fn a_complete_registration_is_accepted() {
    let reg = register::<Complete>();
    assert!(reg.missing().is_empty());
    assert_eq!(reg.gates()[0].canaries().len(), 1);
}

#[test]
fn missing_returns_EVERY_gap_not_just_the_first() {
    // One at a time turns a single fix-and-rerun into four, and the second
    // omission then reads as something the first fix introduced.
    let missing = MissingTwo::register().missing();
    assert_eq!(missing, vec![Kind::SometimesEvents, Kind::Gates]);
}

#[test]
#[should_panic(expected = "sometimes! events")]
fn an_incomplete_registration_refuses_to_construct() {
    // A panic rather than a Result, because the alternative is a subsystem that
    // starts, runs, and is discovered later to have been unobservable the whole
    // time — by which point its silence has already been read as health.
    let _ = register::<MissingTwo>();
}

#[test]
fn the_panic_names_every_missing_category() {
    let err = std::panic::catch_unwind(register::<MissingTwo>).unwrap_err();
    let msg = err
        .downcast_ref::<String>()
        .expect("panic carries a message");
    assert!(
        msg.contains("sometimes! events"),
        "message did not name the first gap: {msg}"
    );
    assert!(
        msg.contains("gates"),
        "message did not name the second gap: {msg}"
    );
    assert!(
        msg.contains("missing-two"),
        "message did not name the subsystem: {msg}"
    );
}

// ─── The coverage floor ─────────────────────────────────────────────────────

#[test]
fn a_declared_event_that_never_fires_is_a_finding() {
    let declared = [SometimesEvent("a"), SometimesEvent("b")];
    let (_, trace) = with_trace(|| {
        engram_observe::sometimes!("a", true);
    });

    let cov = coverage(&declared, [&trace]);
    assert_eq!(cov.hit, vec!["a"]);
    assert_eq!(cov.never_fired, vec!["b"]);
    assert!(
        !cov.is_covered(),
        "a never-fired event must not read as covered"
    );
}

#[test]
fn REACHED_BUT_FALSE_is_not_the_same_as_never_reached() {
    // Two different findings with two different fixes: a condition that is
    // never true means the fault is not being injected; a branch that is never
    // reached means the code path is not being exercised. A coverage report
    // that cannot tell them apart sends the reader to the wrong place.
    let declared = [SometimesEvent("enospc")];
    let (_, trace) = with_trace(|| {
        engram_observe::sometimes!("enospc", false);
    });

    let cov = coverage(&declared, [&trace]);
    assert_eq!(
        cov.never_fired,
        vec!["enospc"],
        "a false condition must not count as coverage"
    );
    // ...and the trace still records that the site was REACHED, so the two
    // cases are distinguishable to anyone who looks.
    assert!(
        trace
            .events()
            .iter()
            .any(|e| e.tag == EventTag::SometimesMissed && e.name == "enospc")
    );
}

#[test]
fn the_floor_is_over_the_SWEEP_not_over_one_run() {
    // What makes swarm configuration meaningful rather than decorative: no
    // single seed is expected to reach every state, so requiring per-run
    // coverage would force the floor down to whatever one run happens to hit.
    let declared = [SometimesEvent("a"), SometimesEvent("b")];
    let (_, run1) = with_trace(|| {
        engram_observe::sometimes!("a", true);
    });
    let (_, run2) = with_trace(|| {
        engram_observe::sometimes!("b", true);
    });

    assert!(!coverage(&declared, [&run1]).is_covered());
    assert!(coverage(&declared, [&run1, &run2]).is_covered());
}

#[test]
fn an_UNDECLARED_event_is_reported_rather_than_ignored() {
    // Not an error — but it sits outside the floor, so it can stop firing
    // without anything noticing. Reporting it is what keeps the declared set
    // from quietly falling behind the code.
    let declared = [SometimesEvent("a")];
    let (_, trace) = with_trace(|| {
        engram_observe::sometimes!("a", true);
        engram_observe::sometimes!("undeclared", true);
    });

    let cov = coverage(&declared, [&trace]);
    assert!(cov.is_covered());
    assert_eq!(cov.undeclared, vec!["undeclared"]);
}

#[test]
fn an_empty_sweep_covers_NOTHING() {
    // The reading this whole crate exists to prevent: zero runs producing zero
    // failures is not a pass. If `coverage` returned "covered" for an empty
    // input, a sweep that crashed before running anything would report green.
    let declared = [SometimesEvent("a")];
    let cov = coverage(&declared, std::iter::empty::<&Trace>());
    assert_eq!(cov.never_fired, vec!["a"]);
    assert!(!cov.is_covered());
}

// ─── The vocabulary ─────────────────────────────────────────────────────────

#[test]
fn always_records_both_outcomes_and_returns_the_condition() {
    let (held, trace) = with_trace(|| {
        let a = engram_observe::always!("index is sorted", true);
        let b = engram_observe::always!("index is sorted", false);
        (a, b)
    });

    assert_eq!(held, (true, false));
    assert_eq!(trace.violations().len(), 1);
    assert_eq!(trace.violations()[0].tag, EventTag::AlwaysViolated);
}

#[test]
fn unreachable_hit_is_always_a_violation() {
    let (_, trace) = with_trace(|| {
        engram_observe::unreachable_hit!("two writers won the same lease");
    });
    assert_eq!(trace.violations().len(), 1);
}

#[test]
fn the_macros_are_a_no_op_outside_a_trace() {
    // Production code carries these calls. It must not pay for, or panic on, a
    // recorder nobody installed.
    engram_observe::always!("no trace installed", true);
    engram_observe::sometimes!("no trace installed", true);
    engram_observe::reachable!("no trace installed");
    engram_observe::counted!("no trace installed");
}

// ─── The digest ─────────────────────────────────────────────────────────────

#[test]
fn the_digest_depends_on_ORDER_not_just_on_content() {
    // A digest over a set would call these identical, and a determinism gate
    // built on it would certify a scheduler it never constrained.
    let (_, a) = with_trace(|| {
        engram_observe::reachable!("x");
        engram_observe::reachable!("y");
    });
    let (_, b) = with_trace(|| {
        engram_observe::reachable!("y");
        engram_observe::reachable!("x");
    });
    assert_ne!(a.digest(), b.digest());
}

#[test]
fn the_digest_depends_on_counter_TOTALS_as_well_as_increments() {
    // An off-by-one that adds 2 where it should add 1 produces exactly the same
    // event sequence, so a digest over events alone would miss it.
    let (_, a) = with_trace(|| engram_observe::counted!("ops", 1));
    let (_, b) = with_trace(|| engram_observe::counted!("ops", 2));
    assert_ne!(a.digest(), b.digest());
}

#[test]
fn an_empty_trace_has_a_stable_nonzero_digest() {
    let (_, a) = with_trace(|| {});
    let (_, b) = with_trace(|| {});
    assert_eq!(a.digest(), b.digest());
    assert_ne!(
        a.digest(),
        0,
        "a zero digest would collide with an uninitialised one"
    );
}
