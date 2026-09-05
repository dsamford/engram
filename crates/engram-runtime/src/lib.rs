//! D1 and D2 — the two decisions that cannot be retrofitted.
//!
//! # D1: everything ambient goes through [`Runtime`]
//!
//! Time, randomness, task spawning and I/O are *injected*, never reached for.
//! The plan is blunt about the cost of deferring this: adding it after 20k
//! lines of `tokio::spawn` and `Instant::now()` is a rewrite, and it is what
//! "decides whether the simulation lane is real or theatre."
//!
//! The word theatre is the important one. A simulation lane built over direct
//! clock reads and a real thread pool still *runs*, still goes green, and still
//! produces a coverage report. It just is not reproducing anything — and a
//! green run from a harness that explores one interleaving looks exactly like a
//! green run from one that explored thousands.
//!
//! # D2: single-threaded-per-shard
//!
//! Concurrency here is cooperative tasks on one thread. Parallelism comes from
//! N shards, not from shared mutable state across threads. This is not a
//! performance preference; a shared thread-pool-plus-locks design **is not
//! simulable**, because the interleavings belong to the OS scheduler and no
//! seed can name one.
//!
//! [`SimRuntime`] is therefore a real executor — a ready queue, a timer heap
//! and a virtual clock — and not a thin wrapper that defers to a runtime it
//! does not control.
//!
//! # What "deterministic" is required to mean here
//!
//! Two runs of one seed produce the same trace, byte for byte, including the
//! ORDER of every event. That is checked by `cargo xtask determinism` rather
//! than asserted, because determinism rots quietly: a `HashMap` iterated once,
//! a float reduction reordered, a dependency that spawns a thread.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use engram_observe::{Canary, Gate, Registration, Subsystem, counted, reachable, sometimes};

pub mod sim;

#[cfg(feature = "tokio-runtime")]
pub mod tokio_rt;

pub use sim::SimRuntime;

/// Virtual time since the start of a run.
///
/// Deliberately NOT `std::time::Instant`: an `Instant` can only be produced by
/// reading the real clock, so a type alias would leave the door open that D1
/// exists to close. Under [`SimRuntime`] this advances only when every task is
/// blocked, which is what makes a one-hour timeout cost microseconds and lets
/// the clock jump backward on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SimInstant(pub Duration);

impl SimInstant {
    /// Time elapsed since the start of the run.
    pub fn since_start(self) -> Duration {
        self.0
    }
}

/// D1 — the seam every ambient capability passes through.
///
/// Not `dyn`-safe, on purpose. The engine is *generic* over this so the
/// simulated implementation is the same code path as the production one rather
/// than a branch inside it; a `dyn Runtime` would push the choice to runtime
/// and invite `if cfg!(test)` to creep in beside it.
pub trait Runtime: Clone {
    /// The current virtual time.
    fn now(&self) -> SimInstant;

    /// A pseudo-random `u64` from the run's seeded stream.
    ///
    /// `&self`, not `&mut self`: randomness is drawn from the runtime the whole
    /// shard shares, so the DRAW ORDER is part of the run and is reproduced
    /// with it. A per-caller generator would make two runs diverge whenever
    /// scheduling shifted which task drew first — which is precisely the
    /// divergence the determinism gate exists to catch.
    fn rand_u64(&self) -> u64;

    /// Sleep for `d` of virtual time.
    fn sleep(&self, d: Duration) -> impl Future<Output = ()>;

    /// Spawn a cooperative task on this shard.
    ///
    /// No `Send` bound: D2 means the task never leaves this thread.
    fn spawn(&self, fut: impl Future<Output = ()> + 'static);

    /// Hand control back to the scheduler.
    ///
    /// On the trait because D2 makes it a correctness primitive rather than an
    /// optimisation: a task that never yields holds the shard, and nothing in
    /// the runtime can take it back.
    fn yield_now(&self) -> impl Future<Output = ()>;
}

// ─── The seeded generator ───────────────────────────────────────────────────

/// A small, explicit PRNG (SplitMix64).
///
/// Hand-rolled for the same reason [`engram_observe::Trace::digest`] is: the
/// simulation lane must reproduce a run from a seed, and that guarantee cannot
/// be delegated to a dependency whose algorithm may change across a semver
/// bump. Not cryptographic and never to be used as though it were.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seed the generator.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next value in the stream.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`. Returns 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

// ─── The cooperative executor ───────────────────────────────────────────────

type TaskId = usize;

struct TaskWaker {
    id: TaskId,
    ready: Arc<Mutex<VecDeque<TaskId>>>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // FIFO, so wake order is a property of the run rather than of the
        // allocator or the OS. `Arc<Mutex<_>>` satisfies `Waker`'s Send + Sync
        // contract; under D2 it is never actually contended.
        let mut q = self.ready.lock().expect("ready queue poisoned");
        if !q.contains(&self.id) {
            q.push_back(self.id);
        }
    }
}

#[derive(Default)]
struct Executor {
    tasks: BTreeMap<TaskId, Pin<Box<dyn Future<Output = ()>>>>,
    next_id: TaskId,
    /// Tasks ready to poll, in wake order.
    ready: Arc<Mutex<VecDeque<TaskId>>>,
    /// Sleepers, ordered by wake time then by id so ties break deterministically.
    timers: BinaryHeap<Reverse<(Duration, TaskId)>>,
    now: Duration,
}

// ─── The shared state one shard owns ────────────────────────────────────────

struct SimState {
    exec: Executor,
    rng: Rng,
}

thread_local! {
    /// Set while a timer future is being polled, so `sleep` can register.
    static POLLING: RefCell<Option<TaskId>> = const { RefCell::new(None) };
}

impl Executor {
    fn spawn(&mut self, fut: Pin<Box<dyn Future<Output = ()>>>) -> TaskId {
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.insert(id, fut);
        self.ready
            .lock()
            .expect("ready queue poisoned")
            .push_back(id);
        id
    }
}

/// Errors a run can end in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// Every task is blocked and no timer is pending.
    ///
    /// R14 names this one specifically: "a deadlock reads as a clean run"
    /// unless liveness is checked. The executor returns it rather than
    /// returning quietly, so the absence of progress is a result.
    Stalled {
        /// How many tasks were still alive when nothing could progress.
        blocked_tasks: usize,
    },
    /// The step budget was exhausted — a livelock, or a budget set too low.
    ///
    /// Distinguished from [`RunError::Stalled`] because the fixes differ:
    /// a stall is a missing wake, a budget exhaustion is a spin.
    BudgetExhausted {
        /// The budget that was hit.
        steps: u64,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Stalled { blocked_tasks } => write!(
                f,
                "STALLED: {blocked_tasks} task(s) alive, none ready, no timer pending — \
                 a missed wake, not a finished run"
            ),
            RunError::BudgetExhausted { steps } => {
                write!(
                    f,
                    "BUDGET EXHAUSTED after {steps} steps — livelock, or a budget set too low"
                )
            }
        }
    }
}

impl std::error::Error for RunError {}

/// D2 — one shard, one thread, virtual time.
#[derive(Clone)]
pub struct Shard {
    state: Rc<RefCell<SimState>>,
}

impl Shard {
    /// A shard with a seeded generator and a clock at zero.
    pub fn new(seed: u64) -> Self {
        Self {
            state: Rc::new(RefCell::new(SimState {
                exec: Executor::default(),
                rng: Rng::new(seed),
            })),
        }
    }

    /// Virtual time.
    pub fn now(&self) -> SimInstant {
        SimInstant(self.state.borrow().exec.now)
    }

    /// Move the clock. Accepts a BACKWARD jump on purpose.
    ///
    /// R14 requires proving the key encoding survives a clock that goes
    /// backwards rather than asserting it, and a clock that cannot be moved
    /// backwards cannot run that test. Timers already scheduled keep their
    /// absolute deadlines, so a backward jump delays them — which is the real
    /// behaviour a monotonic-counter design has to be correct against.
    pub fn set_now(&self, t: Duration) {
        self.state.borrow_mut().exec.now = t;
    }

    /// Draw from the seeded stream.
    pub fn rand_u64(&self) -> u64 {
        self.state.borrow_mut().rng.next_u64()
    }

    /// Spawn a cooperative task.
    pub fn spawn(&self, fut: impl Future<Output = ()> + 'static) {
        self.state.borrow_mut().exec.spawn(Box::pin(fut));
    }

    /// Run until every task completes, or until the budget or a stall ends it.
    ///
    /// `max_steps` bounds the run so a livelock fails in finite time rather
    /// than hanging the suite. A hang is the worst failure mode a test harness
    /// has: it produces no verdict at all, and whatever kills it usually kills
    /// the reporting with it.
    pub fn run_until_idle(&self, max_steps: u64) -> Result<u64, RunError> {
        let mut steps = 0u64;
        loop {
            if steps >= max_steps {
                return Err(RunError::BudgetExhausted { steps });
            }

            let next = {
                let mut st = self.state.borrow_mut();
                let ready = st
                    .exec
                    .ready
                    .lock()
                    .expect("ready queue poisoned")
                    .pop_front();
                match ready {
                    Some(id) => Some(id),
                    None => {
                        // Nothing ready: advance the clock to the earliest timer.
                        match st.exec.timers.pop() {
                            Some(Reverse((deadline, id))) => {
                                // `max`, never a bare assignment: a backward
                                // set_now must not make the clock go backwards
                                // AGAIN here and fire timers out of order.
                                let before = st.exec.now;
                                st.exec.now = st.exec.now.max(deadline);
                                // Declared in `Shard::register`. Fires only when
                                // a run actually idles into a timer, which is
                                // the interleaving a purely CPU-bound test never
                                // reaches — exactly the coverage a floor exists
                                // to notice the absence of.
                                sometimes!(
                                    "executor.clock advanced to timer",
                                    st.exec.now > before
                                );
                                Some(id)
                            }
                            None => None,
                        }
                    }
                }
            };

            let Some(id) = next else {
                let alive = self.state.borrow().exec.tasks.len();
                if alive == 0 {
                    reachable!("executor.drained");
                    return Ok(steps);
                }
                sometimes!("executor.stalled", true);
                return Err(RunError::Stalled {
                    blocked_tasks: alive,
                });
            };

            let Some(mut fut) = self.state.borrow_mut().exec.tasks.remove(&id) else {
                // Woken after completing. Not an error: a waker may outlive the
                // task that owned it.
                sometimes!("executor.woke a completed task", true);
                steps += 1;
                continue;
            };

            let ready_q = self.state.borrow().exec.ready.clone();
            let waker = Waker::from(Arc::new(TaskWaker { id, ready: ready_q }));
            let mut cx = Context::from_waker(&waker);

            let prev = POLLING.with(|p| p.borrow_mut().replace(id));
            let poll = fut.as_mut().poll(&mut cx);
            POLLING.with(|p| *p.borrow_mut() = prev);

            counted!("executor.polls");
            if poll == Poll::Pending {
                self.state.borrow_mut().exec.tasks.insert(id, fut);
            } else {
                counted!("executor.tasks completed");
            }
            steps += 1;
        }
    }

    fn register_timer(&self, at: Duration, id: TaskId) {
        self.state.borrow_mut().exec.timers.push(Reverse((at, id)));
    }
}

/// The future returned by [`Shard::sleep`].
pub struct Sleep {
    shard: Shard,
    deadline: Duration,
    registered: bool,
    polled: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        // ALWAYS yield at least once, even for a zero or already-elapsed
        // deadline. Two reasons, and the second is the one that matters:
        //
        //  1. It matches tokio, so a sleep does not behave differently between
        //     the simulated and production runtimes — a difference there would
        //     live exactly where the concurrency bugs are.
        //  2. Returning Ready inline makes `loop { sleep(ZERO).await }` an
        //     infinite loop INSIDE one poll, which no step budget can interrupt:
        //     the shard hangs with no verdict at all. A hang is the worst
        //     failure a harness has, because whatever kills it usually takes the
        //     reporting with it. Yielding turns that same code into a
        //     BudgetExhausted, which names the problem.
        if !this.polled {
            this.polled = true;
            if this.deadline <= this.shard.now().0 {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        } else if this.shard.now().0 >= this.deadline {
            return Poll::Ready(());
        }

        if !this.registered {
            let id = POLLING.with(|p| *p.borrow());
            if let Some(id) = id {
                this.shard.register_timer(this.deadline, id);
                this.registered = true;
            } else {
                // Polled outside the executor (a manual `poll` in a test, say).
                // Wake rather than sleep forever: an un-registerable timer that
                // returns Pending is an unexplained stall.
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }
}

/// The future returned by [`Shard::yield_now`].
pub struct YieldNow {
    done: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(());
        }
        this.done = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Shard {
    /// Sleep for `d` of virtual time.
    pub fn sleep(&self, d: Duration) -> Sleep {
        Sleep {
            shard: self.clone(),
            deadline: self.now().0 + d,
            registered: false,
            polled: false,
        }
    }

    /// Yield to the scheduler, resuming on the next turn.
    ///
    /// The cooperative-scheduling escape hatch, and the reason it is a named
    /// primitive rather than `sleep(ZERO)`: under D2 a task that never yields
    /// owns the shard's thread until it returns, so long CPU-bound work has to
    /// hand control back on purpose. Nothing can preempt it — not the step
    /// budget, not a timer.
    pub fn yield_now(&self) -> YieldNow {
        YieldNow { done: false }
    }
}

// ─── D3 registration ────────────────────────────────────────────────────────

impl Subsystem for Shard {
    const NAME: &'static str = "executor";

    fn register() -> Registration {
        Registration::new()
            // Crash points are NAMED so a minimised repro can say where. An
            // anonymous crash point cannot appear in a seed.
            .crash_point("executor.before_poll")
            .crash_point("executor.after_timer_fire")
            // Declared here so that never observing one is a finding rather
            // than a silence. A sweep that never idles into a timer has not
            // tested the timer path, and without this declaration the coverage
            // report would simply not mention it.
            .sometimes("executor.clock advanced to timer")
            .sometimes("executor.woke a completed task")
            .sometimes("executor.stalled")
            .counter("executor.polls")
            .counter("executor.tasks completed")
            .gate(
                Gate::new(
                    "no run ends quietly with work outstanding",
                    // The deliberate violation. R14: "a deadlock reads as a
                    // clean run" unless liveness is checked — so the canary is
                    // a task that is never woken, and the gate must FAIL on it.
                    Canary::new(
                        "spawn a task that never completes and assert run_until_idle returns Ok",
                    ),
                )
                .and_canary(Canary::new(
                    "swallow RunError::Stalled and return Ok(steps)",
                )),
            )
            .gate(Gate::new(
                "one seed produces one trace",
                Canary::new("draw from a second, unseeded generator inside a task"),
            ))
    }
}
