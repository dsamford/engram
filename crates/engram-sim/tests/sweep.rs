#![allow(non_snake_case)]
//! The sweep, run in CI at a small seed count. The NIGHTLY lane runs the same
//! function at a large one — same code, one env var, so the nightly cannot
//! drift from what CI verifies.

use engram_sim::{SweepFailure, run_seed, sweep};

fn seed_count() -> u64 {
    std::env::var("ENGRAM_SWEEP_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48)
}

#[test]
fn the_sweep_passes_and_the_coverage_floor_holds() {
    match sweep(1, seed_count()) {
        Ok(report) => {
            assert!(report.seeds >= 1);
            // The floor held; say what it covered so the pass is inspectable.
            println!(
                "SWEEP ok: {} seeds, {} events covered",
                report.seeds,
                report.hit.len()
            );
            for u in &report.undeclared {
                println!("  undeclared (outside the floor): {u}");
            }
        }
        Err(SweepFailure::SeedViolations(v)) => {
            for (seed, cfg, violations) in &v {
                eprintln!("SEED={seed} config={cfg:?}");
                for x in violations {
                    eprintln!("  {x}");
                }
            }
            panic!(
                "{} seed(s) violated invariants — each line above is a repro",
                v.len()
            );
        }
        Err(SweepFailure::CoverageFloor { never_fired }) => {
            panic!(
                "COVERAGE FLOOR: {} declared event(s) never fired across the sweep: {:?}. \
                 A state never reached is a claim never tested.",
                never_fired.len(),
                never_fired
            );
        }
    }
}

#[test]
fn one_seed_is_reproducible() {
    // The property that turns a failure into a repro: the same seed yields the
    // same trace digest, twice, including the workload's own randomness.
    let a = run_seed(424_242);
    let b = run_seed(424_242);
    assert!(
        a.violations.is_empty(),
        "seed 424242 must be clean: {:?}",
        a.violations
    );
    assert_eq!(
        a.trace.digest(),
        b.trace.digest(),
        "one seed produced two different runs"
    );
}

#[test]
fn a_violating_store_is_CAUGHT_by_the_invariants() {
    // The harness's own positive control, run every time: a tampered log must
    // fail verification through the same code path the sweep uses. A harness
    // whose invariants cannot fail is a sweep that measures nothing.
    let report = run_seed(7);
    assert!(report.violations.is_empty(), "seed 7 itself must be clean");
    // Tamper directly through the public surface the sweep checks.
    let store = engram_store::Store::new();
    let p = engram_key::KeyPrefix {
        realm: engram_key::Realm(1),
        namespace: engram_key::Namespace(1),
        kind: engram_key::Kind::NODE,
        partition: engram_key::Partition(1),
    };
    store
        .put(&p, b"k", engram_store::StoredValue::Plain(vec![1]))
        .expect("w");
    let mut entries = store.log_tail(0);
    entries[0].payload[0] ^= 1;
    assert!(
        !matches!(
            engram_log::CommitLog::verify_entries(&entries),
            engram_log::ChainVerify::Intact { .. }
        ),
        "the invariant the sweep relies on cannot detect a tampered log",
    );
}

// ─── The sweep's own failure branches, fed unhealthy input on purpose ──────
//
// On a healthy corpus none of these branches execute, so canaries that
// disabled them came back NOT DETECTED. `sweep_runs` exists so these tests can
// hand the decision logic fabricated failures — the unhealthy input its
// enforcement is FOR.

use engram_observe::{SometimesEvent, with_trace};
use engram_sim::{Config, RunReport, sweep_runs};

fn clean_report(seed: u64) -> RunReport {
    let ((), trace) = with_trace(|| {
        engram_observe::sometimes!("fixture.event", true);
    });
    RunReport {
        seed,
        config: Config::derive(seed),
        trace,
        violations: vec![],
    }
}

#[test]
fn a_seed_violation_FAILS_the_sweep() {
    let out = sweep_runs(
        0,
        2,
        |seed| {
            let mut r = clean_report(seed);
            if seed == 1 {
                r.violations.push("fabricated invariant break".into());
            }
            r
        },
        &[SometimesEvent("fixture.event")],
    );
    match out {
        Err(SweepFailure::SeedViolations(v)) => {
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].0, 1, "the failing seed must be named");
        }
        other => panic!("a violating seed must fail the sweep, got {other:?}"),
    }
}

#[test]
fn a_TRACE_violation_fails_the_sweep_like_any_other() {
    // An `unreachable!` reached inside a run is a seed failure even when the
    // runner's own violation list is empty — the merge lives in the sweep so
    // this is testable, and so a runner cannot forget it.
    let out = sweep_runs(
        0,
        1,
        |seed| {
            let ((), trace) = with_trace(|| {
                engram_observe::sometimes!("fixture.event", true);
                engram_observe::unreachable_hit!("fixture.impossible state");
            });
            RunReport {
                seed,
                config: Config::derive(seed),
                trace,
                violations: vec![],
            }
        },
        &[SometimesEvent("fixture.event")],
    );
    match out {
        Err(SweepFailure::SeedViolations(v)) => {
            assert!(
                v[0].2[0].contains("fixture.impossible state"),
                "the violation must be named: {:?}",
                v[0].2
            );
        }
        other => panic!("a trace violation must fail the sweep, got {other:?}"),
    }
}

#[test]
fn a_declared_event_that_never_fires_FAILS_the_sweep() {
    let out = sweep_runs(
        0,
        3,
        clean_report,
        &[
            SometimesEvent("fixture.event"),
            SometimesEvent("fixture.never-fired"),
        ],
    );
    match out {
        Err(SweepFailure::CoverageFloor { never_fired }) => {
            assert_eq!(never_fired, vec!["fixture.never-fired".to_string()]);
        }
        other => panic!("the floor must fail on a gap, got {other:?}"),
    }
}
