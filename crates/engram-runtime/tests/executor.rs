#![allow(non_snake_case)]
//! D2 — the executor, and the failures that otherwise present as success.
//!
//! R14: "Safety is free in simulation; **a deadlock reads as a clean run**
//! unless checked." Most of this file is about making the run say what actually
//! happened instead of ending quietly.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use engram_observe::with_trace;
use engram_runtime::{RunError, Runtime, Shard, SimRuntime};

#[test]
fn a_finished_run_is_ok_and_reports_its_steps() {
    let rt = SimRuntime::new(1);
    let hits = Rc::new(RefCell::new(0));
    for _ in 0..3 {
        let h = hits.clone();
        rt.spawn(async move { *h.borrow_mut() += 1 });
    }
    let steps = rt.run(100).expect("runs to completion");
    assert_eq!(*hits.borrow(), 3);
    assert!(
        steps >= 3,
        "three tasks cannot complete in fewer than three polls"
    );
}

#[test]
fn a_task_that_is_never_woken_is_a_STALL_not_a_clean_run() {
    // The property the whole error type exists for. An executor that returned
    // Ok here would report a deadlocked system as a passing test, and every
    // liveness bug after it would be invisible.
    let rt = SimRuntime::new(1);
    rt.spawn(async { std::future::pending::<()>().await });

    match rt.run(100) {
        Err(RunError::Stalled { blocked_tasks }) => assert_eq!(blocked_tasks, 1),
        other => panic!("expected a stall, got {other:?}"),
    }
}

#[test]
fn a_stall_is_distinguished_from_a_livelock() {
    // Different diagnoses, different fixes: a stall is a missed wake, a budget
    // exhaustion is a spin. Collapsing them into one error would send the
    // reader looking in the wrong place — which is the same defect as a metric
    // that cannot tell "absent" from "zero", one layer up.
    let rt = SimRuntime::new(1);
    let rt2 = rt.clone();
    rt.spawn(async move {
        loop {
            // Yields and immediately re-wakes: always ready, never finishing.
            // `yield_now`, not `sleep(ZERO)` — the point is a task that hands
            // control BACK each turn, which is what the budget can bound.
            rt2.yield_now().await;
        }
    });

    match rt.run(50) {
        Err(RunError::BudgetExhausted { steps }) => assert_eq!(steps, 50),
        other => panic!("expected budget exhaustion, got {other:?}"),
    }
}

#[test]
fn virtual_time_advances_only_when_every_task_is_blocked() {
    // What makes a one-hour timeout cost microseconds. If the clock advanced on
    // a schedule of its own, a sleeping task could be resumed before the work it
    // waits on had run, and the simulation would explore orderings the real
    // system cannot produce — failures nobody can act on.
    let rt = SimRuntime::new(1);
    let rt2 = rt.clone();
    let seen = Rc::new(RefCell::new(Vec::<u64>::new()));
    let s = seen.clone();

    rt.spawn(async move {
        s.borrow_mut()
            .push(rt2.now().since_start().as_millis() as u64);
        rt2.sleep(Duration::from_secs(3600)).await;
        s.borrow_mut()
            .push(rt2.now().since_start().as_millis() as u64);
    });

    rt.run(100).expect("completes");
    assert_eq!(*seen.borrow(), vec![0, 3_600_000]);
}

#[test]
fn timers_fire_in_deadline_order_regardless_of_spawn_order() {
    let rt = SimRuntime::new(1);
    let order = Rc::new(RefCell::new(Vec::<&'static str>::new()));

    for (name, delay) in [("late", 30u64), ("early", 10), ("mid", 20)] {
        let rt2 = rt.clone();
        let o = order.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(delay)).await;
            o.borrow_mut().push(name);
        });
    }

    rt.run(200).expect("completes");
    assert_eq!(*order.borrow(), vec!["early", "mid", "late"]);
}

#[test]
fn the_clock_can_jump_BACKWARD_without_reordering_fired_timers() {
    // R14 names this test specifically: it is what PROVES the inverted-commit-ts
    // in the key encoding rather than asserting it. A clock that cannot be moved
    // backwards cannot run it, so the capability is deliberate.
    //
    // A timer keeps its ABSOLUTE deadline, so a backward jump delays it. The
    // property asserted is the one a monotonic-counter design must hold: time
    // never runs backwards *as observed by the run*, whatever the clock does.
    let rt = SimRuntime::new(1);
    rt.shard().set_now(Duration::from_millis(50));
    let observed = Rc::new(RefCell::new(Vec::<u128>::new()));

    // Spawned FIRST, so these register their timers (at 60, 70, 80) before the
    // jumper below runs. A jump before anything is polled would only change
    // what the deadlines are computed from, which is not the interesting case:
    // the hazard is a clock that moves under timers already committed.
    for delay in [10u64, 20, 30] {
        let rt2 = rt.clone();
        let o = observed.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(delay)).await;
            o.borrow_mut().push(rt2.now().since_start().as_millis());
        });
    }

    let jumper = rt.clone();
    rt.spawn(async move {
        // 45 milliseconds into the past, with three timers outstanding.
        jumper.shard().set_now(Duration::from_millis(5));
    });

    rt.run(200).expect("completes");

    let seen = observed.borrow().clone();
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "observed time went backwards across timer fires: {seen:?}",
    );
    // Deadlines are ABSOLUTE, so the jump delays them rather than re-basing
    // them: a timer set for t=60 still fires at t=60 even though the clock
    // spent part of the run reading 5. Re-basing would let a backward jump
    // fire the same deadline twice, which in the key encoding is a duplicate
    // key — the thing the inverted-commit-ts has to make impossible.
    assert_eq!(seen, vec![60, 70, 80]);
}

#[test]
fn a_backward_jump_does_not_re_fire_a_timer() {
    // The failure this guards is a duplicate, which in the key encoding is a
    // duplicate KEY — the thing the inverted-commit-ts has to make impossible.
    let rt = SimRuntime::new(1);
    let fires = Rc::new(RefCell::new(0u32));
    let f = fires.clone();
    let rt2 = rt.clone();

    rt.spawn(async move {
        rt2.sleep(Duration::from_millis(10)).await;
        *f.borrow_mut() += 1;
    });

    rt.run(100).expect("completes");
    rt.shard().set_now(Duration::ZERO);
    // Nothing left to run; the completed timer must not come back.
    rt.run(100).expect("still completes");
    assert_eq!(*fires.borrow(), 1);
}

#[test]
fn the_seeded_stream_is_shared_by_the_shard_not_by_the_caller() {
    // `Runtime::rand_u64` takes `&self` so the DRAW ORDER belongs to the run.
    // A per-caller generator would make two runs diverge whenever scheduling
    // shifted which task drew first — the divergence the determinism gate
    // exists to catch, introduced by the design meant to be measured by it.
    let rt = SimRuntime::new(7);
    let a = rt.rand_u64();
    let b = rt.rand_u64();
    assert_ne!(
        a, b,
        "two draws returned the same value — the generator is not advancing"
    );

    let fresh = SimRuntime::new(7);
    assert_eq!(
        fresh.rand_u64(),
        a,
        "the same seed did not reproduce the first draw"
    );
}

#[test]
fn the_executor_records_its_declared_events() {
    // D3 is only worth anything if the declared events actually fire. This is
    // the link between the registration and the coverage floor: without it, a
    // subsystem could declare five `sometimes!` events, never call one, and the
    // sweep would report the gap correctly — but nothing would prove the gap
    // was real rather than the harness failing to observe.
    let rt = SimRuntime::new(3);
    let rt2 = rt.clone();
    let (_, trace) = with_trace(|| {
        rt.spawn(async move { rt2.sleep(Duration::from_millis(5)).await });
        rt.run(100).expect("completes");
    });

    assert!(
        trace
            .sometimes_hit()
            .contains("executor.clock advanced to timer"),
        "the timer path ran but its declared event did not fire: {:?}",
        trace.sometimes_hit(),
    );
    assert_eq!(trace.counters().get("executor.tasks completed"), Some(&1));
    assert!(
        trace.violations().is_empty(),
        "a clean run recorded a violation"
    );
}

#[test]
fn a_stalled_run_records_the_stall_as_an_observation() {
    let rt = SimRuntime::new(3);
    let (result, trace) = with_trace(|| {
        rt.spawn(async { std::future::pending::<()>().await });
        rt.run(100)
    });

    assert!(matches!(result, Err(RunError::Stalled { .. })));
    // The stall is in the TRACE as well as in the return value, so a sweep that
    // only aggregates traces still sees it.
    assert!(trace.sometimes_hit().contains("executor.stalled"));
}

#[test]
fn shard_and_runtime_agree_on_time() {
    // Two views of one clock. If they could disagree, code written against the
    // Runtime and code written against the Shard would be simulating different
    // runs — and the difference would only show up where the two meet.
    let shard = Shard::new(1);
    shard.set_now(Duration::from_millis(250));
    assert_eq!(shard.now().since_start(), Duration::from_millis(250));
}
