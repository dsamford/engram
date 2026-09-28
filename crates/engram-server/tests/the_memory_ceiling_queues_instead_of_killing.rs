#![allow(non_snake_case)]
// Real threads and a real clock: a queued statement WAITS, and the wait is the behaviour under test.
#![allow(clippy::disallowed_methods)]
//! A memory ceiling the process governs itself against, replacing a killer.
//!
//! The bench harness carried an EXTERNAL sampler that killed the server at a
//! threshold. That is not a memory policy, it is a crash with better manners:
//! at SF10 the `contention` profile took the process to 122 GiB, the sampler
//! killed it, and the eleven profiles queued behind it each recorded 0.00 ops/s
//! against millions of transport errors. One real finding became twelve blanks,
//! and the store was left holding a lock nobody owned.
//!
//! What replaces it has three properties, and each is tested here:
//!
//!   * the ceiling is CONFIGURABLE, defaulting to the container's own limit, so
//!     the process uses the machine it was given rather than a guessed share;
//!   * pressure QUEUES rather than refuses, so a peak that drains costs latency
//!     instead of an error;
//!   * the queue is BOUNDED, because pressure that is not statements (caches, a
//!     corpus that does not fit) never drains, and an unbounded queue would
//!     turn that into a hang with no error — strictly worse than a refusal.
//!
//! These drive `run_stmt`, NOT `run_query`. That is the entry the Bolt server
//! calls (`engram-bolt/src/server.rs:520`, `:528`) and the one that carries the
//! in-flight accounting the row budget divides by, so it is where admission
//! belongs. Written first against `run_query`, these tests passed the gate
//! without ever reaching it — a governor tested below the layer it governs.

use std::sync::atomic::Ordering::Relaxed;

use engram_server::{memory_governor_step, resolve_memory_max};

const GIB: u64 = 1024 * 1024 * 1024;

/// The queue tests drive PROCESS-WIDE atomics, and cargo runs the tests in one
/// binary in parallel. Without this they raced: one test's release of
/// `MEMORY_CEILING_REACHED` admitted another test's statement, so the
/// never-clears case was handed a result instead of a refusal. The pure
/// hysteresis tests above take no lock because they touch no shared state.
static GOVERNOR: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the governor, and leave the globals as they were found whatever the
/// test does — a panicking test must not strand every later one behind a
/// ceiling that is never released.
fn governed<T>(f: impl FnOnce() -> T) -> T {
    use engram_graph::interp::{MEMORY_CEILING_REACHED, MEMORY_QUEUE_MAX_WAIT_MS};
    let _g = GOVERNOR.lock().unwrap_or_else(|e| e.into_inner());
    let out = f();
    MEMORY_CEILING_REACHED.store(false, Relaxed);
    MEMORY_QUEUE_MAX_WAIT_MS.store(0, Relaxed);
    out
}

// ── The ceiling ─────────────────────────────────────────────────────────────

#[test]
fn nothing_named_uses_the_containers_own_limit() {
    let (ceiling, why) = resolve_memory_max(None);
    let c = ceiling.expect("a default ceiling must exist, not be unlimited");
    assert!(
        c > 0,
        "a zero ceiling would make every statement queue forever: {why}"
    );
    assert!(
        why.contains("MiB"),
        "the announcement must name the number installed, so a refusal hours \
         later is traceable to this line: {why}"
    );
}

#[test]
fn an_explicit_ceiling_is_taken_as_given() {
    let (ceiling, why) = resolve_memory_max(Some(4096));
    assert_eq!(
        ceiling,
        Some(4 * GIB),
        "an explicit ceiling is the operator's decision, not a suggestion: {why}"
    );
}

#[test]
fn zero_means_unlimited_and_says_so() {
    // A real choice on a dedicated box: the operator would rather have the OOM
    // killer than a queue. It must be reachable, and it must be loud.
    let (ceiling, why) = resolve_memory_max(Some(0));
    assert_eq!(ceiling, None, "0 disables the ceiling: {why}");
    assert!(
        why.contains("unlimited"),
        "disabling the only memory guard must announce itself: {why}"
    );
}

// ── The hysteresis ──────────────────────────────────────────────────────────

#[test]
fn above_the_high_water_mark_the_governor_engages() {
    assert!(
        memory_governor_step(95 * GIB, 100 * GIB, false),
        "95% of the ceiling must engage the governor"
    );
}

#[test]
fn below_the_low_water_mark_it_releases() {
    assert!(
        !memory_governor_step(50 * GIB, 100 * GIB, true),
        "50% of the ceiling must release it, even from a pressured state"
    );
}

#[test]
fn between_the_marks_the_state_is_held() {
    // THE POINT OF TWO MARKS. With a single threshold, RSS sitting near it
    // flips the flag on every sample and statements are admitted on one and
    // queued on the next — neither backpressure nor throughput.
    assert!(
        memory_governor_step(85 * GIB, 100 * GIB, true),
        "85% while already pressured must STAY pressured"
    );
    assert!(
        !memory_governor_step(85 * GIB, 100 * GIB, false),
        "the same 85% while not pressured must stay unpressured"
    );
}

#[test]
fn a_zero_ceiling_never_engages() {
    // Guards the division. A zero ceiling reaching the percentage arithmetic
    // would divide by zero; reaching the governor at all would queue every
    // statement forever.
    assert!(
        !memory_governor_step(100 * GIB, 0, true),
        "an unlimited ceiling must never report pressure"
    );
}

// ── The queue ───────────────────────────────────────────────────────────────

#[test]
fn pressure_queues_a_statement_and_admits_it_when_memory_returns() {
    governed(|| {
        use engram_graph::interp::{
            MEMORY_CEILING_REACHED, MEMORY_QUEUE_ADMITTED, MEMORY_QUEUE_MAX_WAIT_MS, MEMORY_QUEUED,
        };
        use std::collections::BTreeMap;
        use engram_cypher::parse_any;
        use engram_graph::{Graph, run_stmt};
        use engram_key::{Namespace, Realm};
        use engram_store::Store;

        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let q = parse_any("RETURN 1 AS n").expect("parse");
        run_stmt(&g, &q, BTreeMap::new()).expect("a baseline statement must answer");

        MEMORY_QUEUE_MAX_WAIT_MS.store(5_000, Relaxed);
        let before_q = MEMORY_QUEUED.load(Relaxed);
        let before_a = MEMORY_QUEUE_ADMITTED.load(Relaxed);
        MEMORY_CEILING_REACHED.store(true, Relaxed);

        // Memory "comes back" shortly, exactly as draining statements would do.
        let releaser = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            MEMORY_CEILING_REACHED.store(false, Relaxed);
        });

        let t = std::time::Instant::now();
        let r = run_stmt(&g, &q, BTreeMap::new());
        let waited = t.elapsed();
        releaser.join().expect("releaser");
        MEMORY_QUEUE_MAX_WAIT_MS.store(0, Relaxed);
        MEMORY_CEILING_REACHED.store(false, Relaxed);

        assert!(
            r.is_ok(),
            "the statement must be ADMITTED after waiting, not refused: {:?}",
            r.err()
        );
        assert!(
            waited >= std::time::Duration::from_millis(100),
            "it must actually have waited; {waited:?} suggests the gate was skipped"
        );
        assert!(
            MEMORY_QUEUED.load(Relaxed) > before_q,
            "the queue counter must move — a governor that never fires and one that \
             is not wired up read identically without it"
        );
        assert!(
            MEMORY_QUEUE_ADMITTED.load(Relaxed) > before_a,
            "and it must record that queueing WORKED, not merely that it happened"
        );
    });
}

#[test]
fn pressure_that_never_clears_refuses_rather_than_hanging() {
    governed(|| {
        use engram_graph::interp::{MEMORY_CEILING_REACHED, MEMORY_QUEUE_MAX_WAIT_MS, MEMORY_REFUSALS};
        use std::collections::BTreeMap;
        use engram_cypher::parse_any;
        use engram_graph::{Graph, run_stmt};
        use engram_key::{Namespace, Realm};
        use engram_store::Store;

        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let q = parse_any("RETURN 1 AS n").expect("parse");

        MEMORY_QUEUE_MAX_WAIT_MS.store(120, Relaxed);
        MEMORY_CEILING_REACHED.store(true, Relaxed);
        let before = MEMORY_REFUSALS.load(Relaxed);
        let r = run_stmt(&g, &q, BTreeMap::new());
        MEMORY_CEILING_REACHED.store(false, Relaxed);
        MEMORY_QUEUE_MAX_WAIT_MS.store(0, Relaxed);

        let e = r.expect_err("memory that never returns must end in a refusal, not a hang");
        let msg = format!("{e:?}");
        assert!(
            msg.contains("memory ceiling"),
            "the error must name the ceiling as the cause: {msg}"
        );
        assert!(
            msg.contains("waited"),
            "and must say it QUEUED first, so an operator can tell backpressure \
             from an instant refusal: {msg}"
        );
        assert!(
            MEMORY_REFUSALS.load(Relaxed) > before,
            "the refusal counter must move"
        );
    });
}

#[test]
fn a_zero_deadline_refuses_without_queueing() {
    governed(|| {
        // The opt-out: an operator who wants a fast failure rather than latency.
        use engram_graph::interp::{MEMORY_CEILING_REACHED, MEMORY_QUEUE_MAX_WAIT_MS};
        use std::collections::BTreeMap;
        use engram_cypher::parse_any;
        use engram_graph::{Graph, run_stmt};
        use engram_key::{Namespace, Realm};
        use engram_store::Store;

        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let q = parse_any("RETURN 1 AS n").expect("parse");
        MEMORY_QUEUE_MAX_WAIT_MS.store(0, Relaxed);
        MEMORY_CEILING_REACHED.store(true, Relaxed);
        let t = std::time::Instant::now();
        let r = run_stmt(&g, &q, BTreeMap::new());
        let took = t.elapsed();
        MEMORY_CEILING_REACHED.store(false, Relaxed);

        assert!(r.is_err(), "a zero deadline refuses immediately");
        assert!(
            took < std::time::Duration::from_millis(100),
            "it must not have queued: {took:?}"
        );
    });
}

#[test]
fn an_unpressured_server_never_touches_the_gate() {
    governed(|| {
        // The common case must cost nothing: no sleep, no atomic contention beyond
        // one relaxed load. A regression here would tax every statement on every
        // server, including those with no ceiling configured at all.
        use engram_graph::interp::MEMORY_CEILING_REACHED;
        use std::collections::BTreeMap;
        use engram_cypher::parse_any;
        use engram_graph::{Graph, run_stmt};
        use engram_key::{Namespace, Realm};
        use engram_store::Store;

        MEMORY_CEILING_REACHED.store(false, Relaxed);
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let q = parse_any("RETURN 1 AS n").expect("parse");
        let t = std::time::Instant::now();
        for _ in 0..200 {
            run_stmt(&g, &q, BTreeMap::new()).expect("answer");
        }
        let took = t.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "200 trivial statements took {took:?}; the admission gate must be free \
             when there is no pressure"
        );
    });
}
