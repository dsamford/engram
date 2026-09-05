//! The production [`Runtime`], on a tokio current-thread scheduler.
//!
//! `new_current_thread`, not `new_multi_thread`, and that is D2 rather than a
//! configuration preference: one shard runs on one thread, and parallelism
//! comes from running N shards. A multi-thread scheduler here would put the
//! interleaving back in the hands of the OS, where no seed can name it — the
//! production build would then be exercising a concurrency model the simulation
//! lane never tests, which is worse than having no simulation lane, because the
//! green result would still be reported.
//!
//! Time still comes through [`Runtime::now`] and randomness through
//! [`Runtime::rand_u64`]. A production build that reached for `Instant::now()`
//! directly would be a different code path from the simulated one, and the
//! difference would live exactly where the bugs are.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{Rng, Runtime, SimInstant};

/// A [`Runtime`] over a tokio current-thread scheduler.
#[derive(Clone)]
pub struct TokioRuntime {
    start: Instant,
    rng: Rc<RefCell<Rng>>,
}

impl TokioRuntime {
    /// Build a runtime, seeding the generator.
    ///
    /// Seeded even in production, and the seed is expected to be LOGGED: a
    /// production incident that can be replayed from its seed is a different
    /// class of problem from one that cannot.
    pub fn new(seed: u64) -> Self {
        Self {
            start: Instant::now(),
            rng: Rc::new(RefCell::new(Rng::new(seed))),
        }
    }
}

impl Runtime for TokioRuntime {
    fn now(&self) -> SimInstant {
        SimInstant(self.start.elapsed())
    }

    fn rand_u64(&self) -> u64 {
        self.rng.borrow_mut().next_u64()
    }

    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        tokio::time::sleep(d)
    }

    fn spawn(&self, fut: impl Future<Output = ()> + 'static) {
        tokio::task::spawn_local(fut);
    }

    fn yield_now(&self) -> impl Future<Output = ()> {
        tokio::task::yield_now()
    }
}
