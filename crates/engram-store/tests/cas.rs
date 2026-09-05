#![allow(non_snake_case)]
//! The N-contender CAS race — the test L3 exists for.
//!
//! The incumbent measured this shape twice: its guarded write produced
//! **2 winners in 23 of 25 rounds** before the fix (a MATCH+WHERE+SET is not a
//! CAS), and its lease-create produced exactly 1 winner in 25/25 × 8 contenders
//! after. The property is *exactly one winner*, and the failure mode being
//! guarded is silent — every contender believes it won, nothing errors, and the
//! loser's write is simply gone.
//!
//! Run on the simulated shard: eight cooperative tasks on ONE thread,
//! interleaved through the lock's await point. Determinism means a failure here
//! is a seed, not a flake.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::with_trace;
use engram_runtime::{Runtime, SimRuntime};
use engram_store::{Store, StoreError, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

#[test]
fn EXACTLY_ONE_of_eight_contenders_wins_a_create_race() {
    // The lease-create shape: everyone expects ABSENT, everyone writes its own
    // id. One winner; seven precise losers who can see who beat them.
    let rt = SimRuntime::new(42);
    let store = Store::new();
    let outcomes = Rc::new(RefCell::new(Vec::<Result<u64, StoreError>>::new()));

    for contender in 0..8u8 {
        let rt2 = rt.clone();
        let store = store.clone();
        let outcomes = outcomes.clone();
        rt.spawn(async move {
            // Stagger arrivals so the interleaving is decided by the timer heap
            // and the lock queue, not by spawn order alone.
            rt2.sleep(Duration::from_millis(u64::from(contender % 3)))
                .await;
            let r = store
                .cas(
                    &prefix(),
                    b"lease",
                    None,
                    StoredValue::Plain(vec![contender]),
                )
                .await;
            outcomes.borrow_mut().push(r);
        });
    }
    rt.run(10_000).expect("all contenders complete");

    let outcomes = outcomes.borrow();
    assert_eq!(outcomes.len(), 8, "every contender must reach a verdict");
    let winners = outcomes.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        winners, 1,
        "a CAS race must have EXACTLY one winner, got {winners}"
    );

    // Every loser saw the winner's value as current — a mismatch against
    // reality, not against a stale snapshot.
    let winner_value = store
        .get(&prefix(), b"lease")
        .expect("the winner's write landed");
    for r in outcomes.iter().filter(|r| r.is_err()) {
        match r {
            Err(StoreError::CasMismatch { current }) => {
                assert_eq!(current.as_deref(), Some(winner_value.as_slice()));
            }
            other => panic!("a loser must lose by CasMismatch, got {other:?}"),
        }
    }
}

#[test]
fn the_race_is_deterministic_per_seed() {
    // A failure in the test above prints a seed. This is what makes that seed
    // worth printing: the same seed reproduces the same winner.
    let winner_for = |seed: u64| -> u8 {
        let rt = SimRuntime::new(seed);
        let store = Store::new();
        for contender in 0..8u8 {
            let rt2 = rt.clone();
            let store = store.clone();
            rt.spawn(async move {
                rt2.sleep(Duration::from_millis(u64::from(contender % 3)))
                    .await;
                let _ = store
                    .cas(
                        &prefix(),
                        b"lease",
                        None,
                        StoredValue::Plain(vec![contender]),
                    )
                    .await;
            });
        }
        rt.run(10_000).expect("completes");
        store.get(&prefix(), b"lease").expect("someone won")[0]
    };

    assert_eq!(
        winner_for(7),
        winner_for(7),
        "one seed produced two different winners"
    );
}

#[test]
fn cas_expecting_a_VALUE_updates_exactly_once_per_generation() {
    // The counter-increment shape: read v, cas(expect v, write v+1). Eight
    // tasks each try to advance the counter once from whatever they first saw;
    // only those whose expectation still holds succeed.
    let rt = SimRuntime::new(3);
    let store = Store::new();
    store
        .put(&prefix(), b"ctr", StoredValue::Plain(vec![0]))
        .expect("seed");
    let successes = Rc::new(RefCell::new(0u32));

    for contender in 0..8u8 {
        let rt2 = rt.clone();
        let store = store.clone();
        let successes = successes.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(u64::from(contender))).await;
            // Everyone read the value BEFORE contending (a stale read on
            // purpose — that is the client shape being modelled).
            let seen = vec![0u8];
            let r = store
                .cas(&prefix(), b"ctr", Some(&seen), StoredValue::Plain(vec![1]))
                .await;
            if r.is_ok() {
                *successes.borrow_mut() += 1;
            }
        });
    }
    rt.run(10_000).expect("completes");

    // Exactly one 0→1 transition. Seven contenders held a guard that no longer
    // described reality, and every one of them was told so.
    assert_eq!(*successes.borrow(), 1);
    assert_eq!(store.get(&prefix(), b"ctr"), Some(vec![1]));
}

#[test]
fn the_lock_is_released_on_the_MISMATCH_path() {
    // A CAS that leaks its lock on failure converts every retry loop into a
    // deadlock that presents as a hang — no verdict at all. The second call
    // completing IS the assertion.
    let rt = SimRuntime::new(1);
    let store = Store::new();
    store
        .put(&prefix(), b"k", StoredValue::Plain(vec![1]))
        .expect("seed");
    let done = Rc::new(RefCell::new(0u32));

    for _ in 0..2 {
        let store = store.clone();
        let done = done.clone();
        rt.spawn(async move {
            // Both expect a value that is not current, so both MISMATCH.
            let _ = store
                .cas(&prefix(), b"k", Some(&[9]), StoredValue::Plain(vec![2]))
                .await;
            *done.borrow_mut() += 1;
        });
    }
    rt.run(10_000)
        .expect("no stall — the first loser released the lock");
    assert_eq!(*done.borrow(), 2);
    assert_eq!(
        store.get(&prefix(), b"k"),
        Some(vec![1]),
        "no mismatched write landed"
    );
}

#[test]
fn the_gate_refusal_happens_BEFORE_the_lock() {
    // A plaintext put to a protected KIND must fail without queueing: a refusal
    // that cannot succeed must not delay writers who can, and must not consume
    // a wake.
    let rt = SimRuntime::new(1);
    let store = Store::new();
    let p = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::PROTECTED_PROPERTY,
        partition: Partition(1),
    };
    let refused = Rc::new(RefCell::new(false));

    {
        let store = store.clone();
        let refused = refused.clone();
        rt.spawn(async move {
            let r = store.cas(&p, b"k", None, StoredValue::Plain(vec![1])).await;
            *refused.borrow_mut() = matches!(
                r,
                Err(StoreError::ProtectedKindPlaintext { kind_byte: 0x80 })
            );
        });
    }
    rt.run(1_000).expect("completes");
    assert!(*refused.borrow());
}

#[test]
fn contention_and_lost_races_are_OBSERVED_not_just_survived() {
    // D3's link to the trace: the declared sometimes! events must actually
    // fire under the workload that exercises them, or the coverage floor is
    // reporting on declarations rather than behaviour.
    let rt = SimRuntime::new(42);
    let store = Store::new();
    let (_, trace) = with_trace(|| {
        for contender in 0..8u8 {
            let rt2 = rt.clone();
            let store = store.clone();
            rt.spawn(async move {
                rt2.sleep(Duration::from_millis(u64::from(contender % 3)))
                    .await;
                let _ = store
                    .cas(
                        &prefix(),
                        b"lease",
                        None,
                        StoredValue::Plain(vec![contender]),
                    )
                    .await;
            });
        }
        rt.run(10_000).expect("completes");
    });

    let hit = trace.sometimes_hit();
    assert!(
        hit.contains("store.cas lost the race"),
        "7 losers and the event never fired: {hit:?}"
    );
    assert!(
        hit.contains("store.lock contended"),
        "8 contenders and no contention observed: {hit:?}"
    );
}

#[test]
fn lock_acquisition_order_is_ARRIVAL_order() {
    // Fairness is correctness: the queue, not the scheduler, decides. Each
    // contender arrives 2ms apart and appends its id on winning; the sequence
    // must be arrival order exactly. Under a barging lock the late arrivals
    // that happen to poll at the right moment jump the queue, and under a
    // wake-all lock the order belongs to the poll sequence instead.
    let rt = SimRuntime::new(11);
    let store = Store::new();
    let order = Rc::new(RefCell::new(Vec::<u8>::new()));

    for contender in 0..6u8 {
        let rt2 = rt.clone();
        let store = store.clone();
        let order = order.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(u64::from(contender) * 2))
                .await;
            let guard = store.lock(&prefix(), b"fifo").await;
            // Hold across a suspension so the NEXT contender genuinely queues.
            rt2.sleep(Duration::from_millis(20)).await;
            order.borrow_mut().push(contender);
            drop(guard);
        });
    }
    rt.run(100_000).expect("completes");
    assert_eq!(*order.borrow(), vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn a_woken_waiter_that_gets_BARGED_is_not_lost() {
    // The lost-waiter bug, exercised deliberately. The first version of the
    // lock registered its waker ONCE: a waiter woken and then beaten to the
    // lock by a fresh contender saw the lock held, declined to re-register,
    // and was never woken again — its task blocked forever and the run
    // STALLED. The fix is an identified queue entry the waiter refreshes, and
    // an anti-barging acquire that respects the queue's head.
    //
    // This test's assertion is simply that the run COMPLETES: with the bug it
    // returns RunError::Stalled.
    let rt = SimRuntime::new(5);
    let store = Store::new();
    let done = Rc::new(RefCell::new(0u32));

    // A holds the lock across a long suspension; B queues; C arrives fresh
    // exactly when A releases, trying to barge past woken-B.
    for (delay_ms, hold_ms) in [(0u64, 30u64), (5, 5), (30, 5), (31, 5), (32, 5)] {
        let rt2 = rt.clone();
        let store = store.clone();
        let done = done.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(delay_ms)).await;
            let guard = store.lock(&prefix(), b"barge").await;
            rt2.sleep(Duration::from_millis(hold_ms)).await;
            drop(guard);
            *done.borrow_mut() += 1;
        });
    }
    rt.run(100_000)
        .expect("NO waiter may be lost — a stall here is the bug");
    assert_eq!(*done.borrow(), 5);
}

#[test]
fn a_CANCELLED_waiter_leaves_the_line_and_passes_the_wake_on() {
    // Dropping a LockFuture mid-wait (a timeout, a cancelled request) must not
    // wedge the queue: if the cancelled waiter was the head, the wake that was
    // aimed at it has to be re-aimed at the next in line.
    let rt = SimRuntime::new(9);
    let store = Store::new();
    let acquired = Rc::new(RefCell::new(Vec::<&'static str>::new()));

    {
        let rt2 = rt.clone();
        let store = store.clone();
        let acquired = acquired.clone();
        rt.spawn(async move {
            let g = store.lock(&prefix(), b"c").await;
            acquired.borrow_mut().push("holder");
            rt2.sleep(Duration::from_millis(20)).await;
            drop(g);
        });
    }
    {
        // Queues second, then abandons the wait at 10ms — before the holder
        // releases at 20ms.
        let rt2 = rt.clone();
        let store = store.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(1)).await;
            let lock = store.lock(&prefix(), b"c");
            let mut lock = Box::pin(lock);
            futures_lite_race(&mut lock, rt2.sleep(Duration::from_millis(9))).await;
            // lock dropped here, mid-queue
        });
    }
    {
        let rt2 = rt.clone();
        let store = store.clone();
        let acquired = acquired.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(2)).await;
            let g = store.lock(&prefix(), b"c").await;
            acquired.borrow_mut().push("third");
            drop(g);
        });
    }

    rt.run(100_000)
        .expect("the cancelled waiter must not wedge the queue");
    assert_eq!(*acquired.borrow(), vec!["holder", "third"]);
}

/// Race two futures, completing when either does. Local, dependency-free.
async fn futures_lite_race<A: std::future::Future + Unpin, B: std::future::Future>(
    a: &mut A,
    b: B,
) {
    let mut b = Box::pin(b);
    std::future::poll_fn(|cx| {
        if std::pin::Pin::new(&mut *a).poll(cx).is_ready() {
            return std::task::Poll::Ready(());
        }
        b.as_mut().poll(cx).map(|_| ())
    })
    .await
}
