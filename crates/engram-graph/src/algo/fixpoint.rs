//! The bulk-synchronous fixpoint driver.
//!
//! # Why the parallel run is BIT-identical and not merely equivalent
//!
//! A kernel here is a PULL: `next[v]` is computed from `v`'s in-neighbours'
//! previous values. Two things follow, and both are load-bearing.
//!
//! Every floating-point sum for vertex `v` is owned by exactly one vertex, so
//! no two morsels ever accumulate into the same cell — which is what a PUSH
//! formulation would do, and what would make the answer depend on the order
//! threads happened to arrive. And because morsels partition the OUTPUT range,
//! the set of additions performed for any given `v` is the same whatever the
//! width: the same neighbours, in the same slice order, into the same
//! accumulator.
//!
//! Push was rejected for exactly this. Atomic float addition is
//! order-dependent, therefore width-dependent, therefore unreplayable — and
//! the simulation lane's whole claim is that a seed reproduces a run.
//!
//! # Where the parallelism actually is
//!
//! In `run`'s one [`ScopedExec::for_each`] call, and nowhere else. That is
//! worth stating because the driver went one revision with the morsel split
//! computed from `exec.width()` and then evaluated by a plain iterator chain:
//! the arithmetic was width-dependent, the execution was not, and a
//! bit-identity test over several widths passed without ever crossing a
//! thread. A test that varies a width the work never reaches proves the
//! partitioning is deterministic and nothing more. `for_each` is the seam; if
//! it is not called, there is no claim to make.
//!
//! # Why no counter fires inside a morsel
//!
//! `engram-observe`'s counters are thread-local and flushed by the thread
//! that owns the statement, so a `counted!` reached from inside `pull` would
//! record onto a worker and be dropped. Every counter here is therefore on
//! the calling thread, outside the closure — including the morsel-count
//! `sometimes!`, which reads `morsels.len()` rather than being fired from
//! within a morsel. `pull` must stay counter-free for the same reason it must
//! stay pure.

use engram_observe::{counted, sometimes};

use super::MAX_ITERATIONS;

use super::graph::AlgoGraph;
use crate::scoped_exec::ScopedExec;

/// What one iteration of an algorithm does to one vertex.
pub(crate) trait VertexProgram {
    /// The per-vertex state.
    type State: Copy + Send + Sync + Default;

    /// The value `v` starts at.
    fn init(&self, v: u32, g: &AlgoGraph) -> Self::State;

    /// `v`'s next value, from its in-neighbours' previous values.
    ///
    /// **A pure function of `(v, prev, g)`.** It may not read anything else and
    /// may not write anything: that is what makes the morsel split safe.
    fn pull(&self, v: u32, prev: &[Self::State], g: &AlgoGraph, rev: &AlgoGraph) -> Self::State;

    /// One vertex's contribution to the convergence measure.
    fn delta(&self, old: Self::State, new: Self::State) -> f64;

    /// The convergence threshold.
    fn tolerance(&self) -> f64;

    /// Called after each iteration with the finished array, for a program that
    /// needs a global scalar (PageRank's dangling mass).
    ///
    /// Runs SERIALLY over the whole array in ascending vertex order — see the
    /// note on `run`.
    fn after_iteration(&mut self, _values: &[Self::State], _g: &AlgoGraph) {}
}

/// What a fixpoint did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FixpointReport {
    /// How many iterations ran.
    pub iterations: u32,
    /// Whether it converged, or hit the cap.
    ///
    /// **Reported in every mode's summary, never silently returned as if it
    /// had converged.** A PageRank that ran out of iterations is a different
    /// answer from one that settled, and only the caller can decide whether
    /// that matters.
    pub converged: bool,
    /// The final convergence measure.
    pub delta: f64,
}

/// Run `p` to a fixpoint over `g`.
pub(crate) fn run<P: VertexProgram + Sync>(
    p: &mut P,
    g: &AlgoGraph,
    rev: &AlgoGraph,
    exec: &dyn ScopedExec,
    cap: u32,
) -> (Vec<P::State>, FixpointReport) {
    let n = g.len();
    let mut cur: Vec<P::State> = (0..n as u32).map(|v| p.init(v, g)).collect();
    if n == 0 {
        return (
            cur,
            FixpointReport {
                iterations: 0,
                converged: true,
                delta: 0.0,
            },
        );
    }
    p.after_iteration(&cur, g);

    let cap = cap.clamp(1, MAX_ITERATIONS);
    // The SIZE FLOOR is applied by the caller (`Graph::algo_run`), which
    // swaps in a serial executor below it — deliberately, and not here.
    //
    // Applying it here would mean calling `for_each(1, ..)` on the installed
    // executor, and a thread-pool implementor honours that literally: it
    // spawns a scope to run one closure. `algowidth` measured a 1,024-vertex
    // PageRank at 0.56x under exactly that, a slowdown produced entirely by
    // the guard meant to prevent one. A floor must skip the seam, not narrow
    // what crosses it.
    let width = exec.width().max(1);
    let mut iterations = 0u32;
    let mut delta = f64::INFINITY;
    let mut converged = false;

    while iterations < cap {
        iterations += 1;
        // Contiguous morsels OF THE OUTPUT RANGE. Each writes only its own
        // owned vector, and they are concatenated in morsel order — no lock,
        // no `unsafe`, and disjointness by construction rather than by
        // convention.
        let per = n.div_ceil(width);
        let morsels: Vec<(usize, usize)> = (0..width)
            .map(|m| ((m * per).min(n), ((m + 1) * per).min(n)))
            .filter(|(lo, hi)| lo < hi)
            .collect();
        if morsels.len() > 1 {
            // A COUNTER AND NOT A `sometimes!`, deliberately.
            //
            // Reaching more than one morsel needs a projection above
            // `Graph::algo_min_vertices` — 65,536 nodes by default — and the
            // simulation sweep runs 48 seeds; building that corpus on each
            // would cost far more than the state is worth, so a declared
            // event here would sit permanently unreached and the coverage
            // floor would be measuring the sweep's budget rather than the
            // engine's states. The state IS tested, on the real procedure
            // surface, by
            // `an_algorithm_procedure_reaches_the_parallel_lane` and
            // `the_fixpoint_actually_hands_its_morsels_to_the_executor`.
            counted!("algo.fixpoint morsels above one");
        }
        let parts: Vec<Vec<P::State>> = {
            // One SLOT per morsel, filled by whichever worker ran it and read
            // back below IN MORSEL ORDER — the idiom `pipeline.rs`'s parallel
            // expand uses, for the same reason: no return value has to cross
            // the `ScopedExec` seam, and no two workers ever touch one slot.
            //
            // `p` is reborrowed IMMUTABLY here. `pull` takes `&self` and is
            // documented as a pure function of `(v, prev, g)`; the `&mut P`
            // is needed only by `after_iteration`, which runs serially after
            // the barrier. That split is what makes the closure `Sync`.
            let cur_ref = &cur;
            let prog: &P = p;
            let slots: Vec<std::sync::Mutex<Option<Vec<P::State>>>> = morsels
                .iter()
                .map(|_| std::sync::Mutex::new(None))
                .collect();
            exec.for_each(morsels.len(), &|i| {
                let (lo, hi) = morsels[i];
                let part: Vec<P::State> = ((lo as u32)..(hi as u32))
                    .map(|v| prog.pull(v, cur_ref, g, rev))
                    .collect();
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(part);
            });
            slots
                .into_iter()
                .map(|m| {
                    m.into_inner()
                        .unwrap_or_else(|e| e.into_inner())
                        .expect("every morsel ran — `for_each` returns only when all have")
                })
                .collect()
        };
        let mut next: Vec<P::State> = Vec::with_capacity(n);
        for part in parts {
            next.extend(part);
        }

        // THE CONVERGENCE FOLD IS SERIAL, over the finished array, in
        // ascending vertex order. Summing per-morsel partials instead would
        // regroup the float addition with the executor's width — and the
        // result feeds the loop condition, so the number of iterations would
        // depend on how many threads were available. One extra pass is the
        // price of width-independence, and it will look like waste to anyone
        // profiling it.
        let mut acc = 0.0;
        for v in 0..n {
            acc += p.delta(cur[v], next[v]);
        }
        delta = acc;
        cur = next;
        p.after_iteration(&cur, g);
        counted!("algo.fixpoint iterations");
        if delta <= p.tolerance() {
            converged = true;
            sometimes!("algo.fixpoint converged before the cap", true);
            break;
        }
    }
    if !converged {
        sometimes!("algo.fixpoint hit the iteration cap", true);
        counted!("algo.fixpoint hit the iteration cap");
    }
    (
        cur,
        FixpointReport {
            iterations,
            converged,
            delta,
        },
    )
}
