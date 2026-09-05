#![allow(non_snake_case)]
//! The lock's edge cases, polled BY HAND.
//!
//! The sim executor cannot reach these interleavings: its ready queue is FIFO
//! and a waker resolves to a task id, so the woken head is always the next
//! relevant poll and every re-poll carries an equivalent waker. Three canaries
//! against the lock's hard cases came back NOT DETECTED for exactly that
//! reason — the properties are unreachable there, not untested here.
//!
//! Production is not that executor. Under tokio, wake order is not coupled to
//! poll order and a future re-polled from a different context carries a
//! DIFFERENT waker. So these tests drive `LockFuture` directly with hand-built
//! wakers, constructing the schedules the executor cannot: a fresh contender
//! polled between a release and the woken head's re-poll (barging), a woken
//! head dropped before it re-polls (the wake must be passed on), and a re-poll
//! under a new waker (the stored one must be refreshed).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::Store;

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

/// A waker that counts its wakes, so a test can ask "was THIS one woken".
struct CountingWaker(AtomicU32);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker() -> (Arc<CountingWaker>, Waker) {
    let cw = Arc::new(CountingWaker(AtomicU32::new(0)));
    let w = Waker::from(cw.clone());
    (cw, w)
}

#[test]
fn a_fresh_contender_cannot_BARGE_past_the_queue_head() {
    // Holder acquires; B queues; holder releases (wake aimed at B, in place);
    // then C — never queued — polls while the lock is FREE. Without the
    // queue-head check C acquires here, B's eventual acquisition order breaks,
    // and under a scheduler that keeps timing C right, B starves. That is the
    // incumbent's measured lease pathology: a worker that never acquired in 25
    // rounds.
    let store = Store::new();
    let (_, w) = counting_waker();

    let holder = {
        let mut f = Box::pin(store.lock(&prefix(), b"k"));
        match f.as_mut().poll(&mut Context::from_waker(&w)) {
            Poll::Ready(g) => g,
            Poll::Pending => panic!("a free lock must acquire on first poll"),
        }
    };

    let (_b_cw, b_w) = counting_waker();
    let mut b = Box::pin(store.lock(&prefix(), b"k"));
    assert!(
        b.as_mut().poll(&mut Context::from_waker(&b_w)).is_pending(),
        "B queues"
    );

    drop(holder); // wake aimed at B, entry left in place

    // C polls fresh, lock free, B still at the head.
    let (_c_cw, c_w) = counting_waker();
    let mut c = Box::pin(store.lock(&prefix(), b"k"));
    assert!(
        c.as_mut().poll(&mut Context::from_waker(&c_w)).is_pending(),
        "C BARGED: acquired a free lock past the queue head",
    );

    // And B, re-polling, acquires — its place was preserved.
    assert!(
        b.as_mut().poll(&mut Context::from_waker(&b_w)).is_ready(),
        "B must acquire"
    );
}

#[test]
fn a_woken_head_dropped_before_repolling_PASSES_THE_WAKE_ON() {
    // Holder releases → wake delivered to B. B's future is then dropped
    // without ever re-polling (a timeout fired, the request was cancelled).
    // The wake died with B — unless B's Drop re-aims it at C. Without that, C
    // waits forever on a FREE lock, which presents as a hang with no owner.
    let store = Store::new();
    let (_, w) = counting_waker();

    let holder = {
        let mut f = Box::pin(store.lock(&prefix(), b"k"));
        let Poll::Ready(g) = f.as_mut().poll(&mut Context::from_waker(&w)) else {
            panic!("acquires")
        };
        g
    };

    let (b_cw, b_w) = counting_waker();
    let mut b = Box::pin(store.lock(&prefix(), b"k"));
    assert!(b.as_mut().poll(&mut Context::from_waker(&b_w)).is_pending());

    let (c_cw, c_w) = counting_waker();
    let mut c = Box::pin(store.lock(&prefix(), b"k"));
    assert!(c.as_mut().poll(&mut Context::from_waker(&c_w)).is_pending());

    drop(holder);
    assert_eq!(
        b_cw.0.load(Ordering::SeqCst),
        1,
        "the wake went to the head, B"
    );
    assert_eq!(c_cw.0.load(Ordering::SeqCst), 0, "C not woken yet");

    drop(b); // B abandons WITHOUT re-polling — the wake it received dies here…

    assert_eq!(
        c_cw.0.load(Ordering::SeqCst),
        1,
        "…unless B's Drop passes it on. C was never woken: the queue is wedged on a free lock",
    );
    assert!(
        c.as_mut().poll(&mut Context::from_waker(&c_w)).is_ready(),
        "C acquires"
    );
}

#[test]
fn a_repoll_under_a_NEW_waker_refreshes_the_stored_one() {
    // A future re-polled from a different context carries a different waker —
    // tokio does this whenever a future moves between combinators. If the
    // queue keeps the FIRST waker, the release wakes a context that no longer
    // drives this future, which is indistinguishable from not being woken.
    let store = Store::new();
    let (_, w) = counting_waker();

    let holder = {
        let mut f = Box::pin(store.lock(&prefix(), b"k"));
        let Poll::Ready(g) = f.as_mut().poll(&mut Context::from_waker(&w)) else {
            panic!("acquires")
        };
        g
    };

    let (old_cw, old_w) = counting_waker();
    let mut b = Box::pin(store.lock(&prefix(), b"k"));
    assert!(
        b.as_mut()
            .poll(&mut Context::from_waker(&old_w))
            .is_pending(),
        "queued under OLD"
    );

    let (new_cw, new_w) = counting_waker();
    assert!(
        b.as_mut()
            .poll(&mut Context::from_waker(&new_w))
            .is_pending(),
        "re-polled under NEW"
    );

    drop(holder);

    assert_eq!(
        old_cw.0.load(Ordering::SeqCst),
        0,
        "the STALE waker was woken — wrong context"
    );
    assert_eq!(
        new_cw.0.load(Ordering::SeqCst),
        1,
        "the current waker must receive the wake"
    );
    assert!(b.as_mut().poll(&mut Context::from_waker(&new_w)).is_ready());
}

#[test]
fn wake_then_lose_the_race_then_STILL_acquire_eventually() {
    // The full lost-waiter scenario, by hand: B is woken, but before B
    // re-polls, C barges a poll in (refused), and ANOTHER writer D takes and
    // releases the lock through the proper channel... except D cannot — the
    // head is B. So this pins the composite: whatever interleaving happens
    // between B's wake and B's re-poll, B's claim survives it.
    let store = Store::new();
    let (_, w) = counting_waker();

    let holder = {
        let mut f = Box::pin(store.lock(&prefix(), b"k"));
        let Poll::Ready(g) = f.as_mut().poll(&mut Context::from_waker(&w)) else {
            panic!("acquires")
        };
        g
    };

    let (_b_cw, b_w) = counting_waker();
    let mut b = Box::pin(store.lock(&prefix(), b"k"));
    assert!(b.as_mut().poll(&mut Context::from_waker(&b_w)).is_pending());

    drop(holder);

    // Two different fresh contenders poll in the window. Both must queue.
    let (_c_cw, c_w) = counting_waker();
    let mut c = Box::pin(store.lock(&prefix(), b"k"));
    assert!(c.as_mut().poll(&mut Context::from_waker(&c_w)).is_pending());
    let (_d_cw, d_w) = counting_waker();
    let mut d = Box::pin(store.lock(&prefix(), b"k"));
    assert!(d.as_mut().poll(&mut Context::from_waker(&d_w)).is_pending());

    // B re-polls last of the three and STILL acquires: order is arrival, not
    // poll timing.
    assert!(b.as_mut().poll(&mut Context::from_waker(&b_w)).is_ready());
    drop(b);
    // …and the line advances in order behind it.
    assert!(c.as_mut().poll(&mut Context::from_waker(&c_w)).is_ready());
}
