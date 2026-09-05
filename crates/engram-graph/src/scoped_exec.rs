//! The morsel-parallelism seam (W3 of `docs/scale-and-integrity-plan.md`).
//!
//! The ENGINE never spawns a thread — that is a house rule with a lint
//! behind it, and the deterministic simulation depends on it. Instead, an
//! operator that can split its work into MORSELS asks the graph's installed
//! [`ScopedExec`] to run them. Who implements it decides everything:
//!
//! - **absent** (the default, and always in the sim lane): operators take
//!   their serial paths untouched — byte-identical behaviour, no new code
//!   on the hot path beyond one lever check;
//! - [`SerialExec`]: width 1, `f` invoked inline on the caller's thread —
//!   the parallel MACHINERY (morsel split, slot collection, ordered merge)
//!   exercised deterministically, which is how the differential suite and
//!   the sim can cover it without threads;
//! - the server's thread-scope pool (`engram-server`): real OS threads, the
//!   only production implementor, installed behind `ENGRAM_QUERY_PARALLELISM`.
//!
//! The merge discipline is the operator's obligation, not this trait's:
//! partials are collected per-morsel and concatenated IN MORSEL ORDER, so a
//! parallel run reproduces the serial output byte-identically (proven per
//! operator by an A/B differential with a fired-counter canary).
//!
//! Read semantics under concurrency, stated rather than assumed: a morsel
//! worker reads exactly as the serial loop it replaces does — read-committed
//! per row against the visible clock. A commit that lands mid-statement can
//! be seen by later rows and not earlier ones IN EITHER MODE; parallelism
//! changes which rows are "later" but not the anomaly class. Statements
//! inside a transaction never parallelise at all (the overlays and the OCC
//! read-set are thread-local), which the dispatch gates enforce.

/// A scoped executor: runs `f(0)..f(n-1)`, possibly concurrently, and
/// returns only when ALL have run. `f` observes only `Sync` state.
pub trait ScopedExec: Send + Sync {
    /// How many workers `for_each` may actually use — the operator sizes
    /// its morsels by this. 1 means "inline, serial".
    fn width(&self) -> usize;

    /// Run every index in `[0, n)`. The implementor chooses concurrency and
    /// scheduling; the caller guarantees `f` is safe from any thread and
    /// collects results itself (slot per morsel).
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync));
}

/// Width-1 inline executor: the deterministic implementor. Installing it
/// routes operators through the SAME morsel/slot/merge machinery as the
/// parallel pool, one morsel at a time on the calling thread.
pub struct SerialExec;

impl ScopedExec for SerialExec {
    fn width(&self) -> usize {
        1
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        for i in 0..n {
            f(i);
        }
    }
}
