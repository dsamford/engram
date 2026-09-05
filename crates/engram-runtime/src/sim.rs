//! The simulated [`Runtime`] — one shard, virtual time, a seeded stream.

use std::future::Future;
use std::time::Duration;

use crate::{Runtime, Shard, SimInstant};

/// A [`Runtime`] backed by [`Shard`]'s cooperative executor.
///
/// This is the implementation the tests run against, and it is a real executor
/// rather than a shim over someone else's. That matters more than it sounds:
/// if the simulated runtime delegated scheduling to a runtime it did not
/// control, a seed would not determine the interleaving, and the whole
/// simulation lane would be exploring one arbitrary schedule while reporting as
/// though it had explored many.
#[derive(Clone)]
pub struct SimRuntime {
    shard: Shard,
}

impl SimRuntime {
    /// Build a runtime over a seeded shard.
    pub fn new(seed: u64) -> Self {
        Self {
            shard: Shard::new(seed),
        }
    }

    /// The shard, for driving the run and for tests that need to move the clock.
    pub fn shard(&self) -> &Shard {
        &self.shard
    }

    /// Run to completion, or to a stall or the step budget.
    pub fn run(&self, max_steps: u64) -> Result<u64, crate::RunError> {
        self.shard.run_until_idle(max_steps)
    }
}

impl Runtime for SimRuntime {
    fn now(&self) -> SimInstant {
        self.shard.now()
    }

    fn rand_u64(&self) -> u64 {
        self.shard.rand_u64()
    }

    fn sleep(&self, d: Duration) -> impl Future<Output = ()> {
        self.shard.sleep(d)
    }

    fn spawn(&self, fut: impl Future<Output = ()> + 'static) {
        self.shard.spawn(fut);
    }

    fn yield_now(&self) -> impl Future<Output = ()> {
        self.shard.yield_now()
    }
}
