//! Graph algorithms — PageRank, components, paths, communities.
//!
//! # Where they run, and why it matters
//!
//! Every kernel here reads a [`AlgoGraph`]: a dense, pinned, compressed
//! adjacency view of ONE projection at ONE snapshot. It is built once, before
//! any iteration begins, and no kernel touches the store afterwards.
//!
//! That is not tidiness. It is what makes **the procedure never write; the
//! statement does** enforceable rather than aspirational. Compute runs against
//! a read snapshot outside any write transaction; `mutate` publishes into a
//! cache; `write` performs an ORDINARY bulk property set through the normal
//! write path, in a separate short transaction, AFTER the fixpoint has
//! returned. Nothing here ever holds an entity lock across an iteration,
//! because nothing here can take one.
//!
//! It also means a result is stale by construction — computed at snapshot `S`,
//! landing at commit `C` — so both stamps are reported. GDS returns one write
//! summary and never tells you the scores describe a graph that no longer
//! exists.
//!
//! # Determinism, in four links
//!
//! Every link has a mechanism, not an intention:
//!
//! 1. **The dense numbering is a pure function of the ascending member id
//!    set.** Vertices are numbered by their position in the sorted id vector,
//!    and the vector is checked to be strictly ascending rather than assumed.
//! 2. **Every adjacency row is sorted** by dense id. The store yields a row in
//!    its own order, which depends on the relationship type layout and on
//!    whichever cached table served — neither of which is a property of the
//!    graph.
//! 3. **Kernels PULL.** `next[v]` is a pure function of `(v, previous)`, so no
//!    floating-point sum is ever owned by two vertices, and no two morsels
//!    ever accumulate into the same cell.
//! 4. **Morsels partition the OUTPUT range** and merge in morsel order, so a
//!    parallel run is BIT-IDENTICAL to a serial one rather than merely
//!    equivalent — which is what the engine's two-lane rule demands.
//!
//! The global scalars — PageRank's dangling mass, the convergence delta,
//! modularity — are folded **serially, in ascending vertex order, over the
//! finished array**, never from per-morsel partials. A partial-based reduction
//! regroups float addition with the executor's width, and that regrouping
//! feeds the next iteration, so the answer would depend on how many threads
//! happened to be available. The extra pass is the price of width-independence
//! and it is deliberate; a profiler will make it look like waste.
//!
//! # It refuses rather than tries
//!
//! `try_shortest_path_bfs` declines to a slower correct path. **An algorithm
//! has no slower path** — there is no other way to compute PageRank — so its
//! refusal is a typed error naming the numbers and the lever that changes
//! them, raised BEFORE the projection is built. The rejected alternative is
//! "build it and let the allocator decide", which is what GDS does and what an
//! LDBC IC13 out-of-memory taught this engine costs.

pub(crate) mod cache;
pub(crate) mod fixpoint;
pub(crate) mod graph;
pub(crate) mod kernels;
pub(crate) mod modes;
pub(crate) mod proc;
pub(crate) mod run;

pub use graph::{AlgoGraph, ProjectionKey};
pub use modes::{AlgoConfig, AlgoResult, AlgoValues, Mode};
pub use run::{AlgoError, Algorithm};

/// How many vertices a projection may hold before it is refused.
pub const NODE_CEILING: u64 = 20_000_000;

/// How many edges a projection may hold before it is refused.
pub const EDGE_CEILING: u64 = 200_000_000;

/// How many bytes of working set a projection may need before it is refused.
pub const BYTE_CEILING: u64 = 2 * 1024 * 1024 * 1024;

/// The most iterations any fixpoint will run.
pub const MAX_ITERATIONS: u32 = 100;

/// How much `V x E` work an all-pairs algorithm may cost before it is refused.
///
/// Betweenness centrality runs a shortest-path search FROM EVERY VERTEX, so
/// its cost is `O(V x E)` and not `O(V + E)` like everything else here. A
/// projection that PageRank finishes in a second — a million nodes, four
/// million edges — is four times ten to the twelve units of work for Brandes,
/// which is days. `NODE_CEILING` and `EDGE_CEILING` both pass it comfortably,
/// so without a ceiling of its own the refusal that exists to prevent a
/// runaway would never fire for the one algorithm that needs it most.
///
/// Ten to the tenth is roughly a minute of single-threaded work — MEASURED,
/// not estimated, by `algocost`: betweenness costs 4.87 ns per unit, so 1e10
/// units is 48.7 s. Closeness is cheaper at 2.60 ns, or 25.9 s.
///
/// The figure that justifies pricing a PRODUCT rather than a sum is that the
/// per-unit cost holds CONSTANT across a sixteenfold change in `V x E`
/// (betweenness 4.959 then 4.869 ns at 7.5e5 and 1.2e7 units). A ceiling on a
/// product whose per-unit cost drifted with size would refuse correctly at one
/// scale and wrongly at every other, and one measurement could not have shown
/// that — which is why `algocost` measures two sizes rather than one.
///
/// A minute is the most a synchronous procedure call should ever take without
/// the caller having asked for it explicitly.
/// The alternative — compute it and let the operator notice — is what GDS
/// does, and it is how a single `CALL` takes a production instance down.
pub const WORK_CEILING: u64 = 10_000_000_000;

/// Why an algorithm refused to run.
///
/// A typed refusal rather than a message, because the numbers are the useful
/// part: an operator needs to know what was too big and by how much before
/// deciding whether to narrow the projection or raise the ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The algorithm that refused.
    pub algorithm: String,
    /// What was measured.
    pub what: &'static str,
    /// The measurement.
    pub measured: u64,
    /// The ceiling it passed.
    pub ceiling: u64,
    /// The lever that changes the ceiling.
    pub lever: &'static str,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: this projection has {} {}, over the ceiling of {}. Narrow it with \
             `nodeLabels`/`relationshipTypes`, or raise {}. Nothing was computed.",
            self.algorithm, self.measured, self.what, self.ceiling, self.lever
        )
    }
}
