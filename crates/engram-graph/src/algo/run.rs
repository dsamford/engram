//! Running one algorithm in one mode.
//!
//! # The architectural rule, in code
//!
//! **The procedure never writes; the statement does.** [`Graph::algo_run`]
//! computes against a read snapshot and returns values. It opens no
//! transaction and touches no keyspace row. `write` is a SEPARATE step,
//! [`Graph::algo_write_back`], which performs an ordinary bulk property set
//! through the normal write path in short transactions AFTER the computation
//! has returned.
//!
//! That separation is what keeps an entity lock from being held across a
//! fixpoint iteration — which is the failure a naive "write mode" produces,
//! and which no amount of care inside the procedure could avoid if the write
//! happened during the compute.

use engram_cypher::Value;
use engram_observe::{counted, sometimes};

use super::Refusal;
use super::kernels;
use super::modes::{AlgoConfig, AlgoResult, AlgoValues, Mode};
use crate::scoped_exec::{ScopedExec, SerialExec};
use crate::{Dir, Graph, GraphError};

/// Which algorithm to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// PageRank.
    PageRank,
    /// Weakly connected components.
    Wcc,
    /// Degree centrality.
    Degree,
    /// Unweighted breadth-first distances from a source.
    Bfs,
    /// Weighted shortest distances from a source.
    Sssp,
    /// Triangle count per node.
    TriangleCount,
    /// Local clustering coefficient.
    LocalClustering,
    /// Label propagation.
    LabelPropagation,
    /// Louvain modularity.
    Louvain,
    /// Betweenness centrality (exact, Brandes).
    Betweenness,
    /// Strongly connected components.
    Scc,
    /// Closeness centrality (Wasserman-Faust).
    Closeness,
}

impl Algorithm {
    /// The name a procedure uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::PageRank => "pagerank",
            Algorithm::Wcc => "wcc",
            Algorithm::Degree => "degree",
            Algorithm::Bfs => "bfs",
            Algorithm::Sssp => "sssp",
            Algorithm::TriangleCount => "trianglecount",
            Algorithm::LocalClustering => "localclusteringcoefficient",
            Algorithm::LabelPropagation => "labelpropagation",
            Algorithm::Louvain => "louvain",
            Algorithm::Betweenness => "betweenness",
            Algorithm::Scc => "scc",
            Algorithm::Closeness => "closeness",
        }
    }

    /// What the streamed value column is called.
    #[must_use]
    pub fn value_column(self) -> &'static str {
        match self {
            Algorithm::PageRank => "score",
            Algorithm::Wcc => "componentId",
            Algorithm::Degree => "degree",
            Algorithm::Bfs => "depth",
            Algorithm::Sssp => "distance",
            Algorithm::TriangleCount => "triangleCount",
            Algorithm::LocalClustering => "coefficient",
            Algorithm::LabelPropagation | Algorithm::Louvain => "communityId",
            Algorithm::Betweenness | Algorithm::Closeness => "score",
            Algorithm::Scc => "componentId",
        }
    }

    /// How many bytes of per-node state the algorithm needs, for the pre-flight
    /// cost estimate.
    fn state_bytes(self) -> u64 {
        match self {
            // Two f64 buffers, current and next.
            Algorithm::PageRank => 16,
            // Union-find parent and size, plus the label.
            Algorithm::Wcc | Algorithm::LabelPropagation | Algorithm::Louvain => 24,
            // Two component vectors plus the finish-order stack.
            Algorithm::Scc => 20,
            // One distance vector and the score.
            Algorithm::Closeness => 16,
            // sigma, dist, delta, the score, and a predecessor list header per
            // vertex. The predecessor LISTS themselves are bounded by the edge
            // count, which the edge ceiling already prices.
            Algorithm::Betweenness => 60,
            _ => 8,
        }
    }

    /// Whether the algorithm's cost is `O(V x E)` rather than `O(V + E)`.
    ///
    /// Only all-pairs algorithms answer yes, and they are priced against
    /// [`WORK_CEILING`] as well as the node and edge ceilings — because a
    /// projection well inside both of those is still days of work when every
    /// vertex is a source. A ceiling that cannot fire for the algorithm most
    /// able to run away is not a ceiling.
    fn is_all_pairs(self) -> bool {
        matches!(self, Algorithm::Betweenness | Algorithm::Closeness)
    }
}

impl Graph {
    /// Run `alg` under `cfg`, against a read snapshot.
    ///
    /// **Computes only.** Nothing here writes, and nothing here can: the
    /// values come back and the caller decides what to do with them.
    pub fn algo_run(
        &self,
        alg: Algorithm,
        cfg: &AlgoConfig,
        exec: &dyn ScopedExec,
    ) -> Result<AlgoResult, AlgoError> {
        let full = format!("engram.algo.{}", alg.name());
        // PHASE TIMING, behind `ENGRAM_ALGO_TIMING=1`.
        //
        // The first algorithm call on a served SF3 store takes ~35 s and five
        // separate hypotheses about where it goes were each falsified by
        // measurement: the kernel (O(V+E) over 1.13M edges), the projection's
        // edge reads (a materialising traversal of the same edges is 0 s on
        // the same cold server), the stream's per-vertex record reads, the
        // pricing pass, and execution width. Counters could not settle it
        // because the expensive phase bumps none of its own. So the phases
        // report their own elapsed time.
        let timing = phase_timing_on();
        let t0 = phase_clock(timing);
        let named = match &cfg.named {
            Some(h) => Some(super::graph::named_projection(h).map_err(AlgoError::Semantic)?),
            None => None,
        };
        let (nodes, edges) = match &named {
            // priced when it was built
            Some(g) => (g.len() as u64, g.edge_count() as u64),
            None => self
                .algo_price(
                    &full,
                    &cfg.projection,
                    alg.state_bytes(),
                    alg.is_all_pairs(),
                )
                .map_err(AlgoError::Refused)?,
        };
        if let Some(t0) = t0 {
            eprintln!("[algo-timing] price {:?}", t0.elapsed());
        }
        if alg.is_all_pairs() {
            // `V x E`, saturating: the product overflows a u64 long before it
            // becomes a plausible amount of work, and a wrapped product would
            // compare as SMALL and admit exactly the runaway this refuses.
            let work = nodes.saturating_mul(edges);
            if work > self.algo_work_ceiling() {
                counted!("algo.refused for all-pairs work");
                return Err(AlgoError::Refused(Refusal {
                    algorithm: full,
                    what: "node x edge work",
                    measured: work,
                    ceiling: self.algo_work_ceiling(),
                    lever: "ENGRAM_ALGO_WORK_CEILING",
                }));
            }
        }

        let t1 = phase_clock(timing);
        let g = match named {
            Some(g) => super::graph::AlgoProjection::new(g),
            None => self.algo_graph(&cfg.projection).map_err(AlgoError::Graph)?,
        };
        if let Some(t1) = t1 {
            eprintln!(
                "[algo-timing] build {:?} ({} vertices)",
                t1.elapsed(),
                g.len()
            );
        }
        // The reverse is the PULL kernels' alone, built when the first of them
        // asks and kept with the projection (`AlgoProjection::reversed`).
        let rev = || g.reversed();
        let t2 = phase_clock(timing);
        let ids = g.ids.clone();
        if let Some(t2) = t2 {
            eprintln!("[algo-timing] ids {:?}", t2.elapsed());
        }

        // THE SIZE FLOOR, applied here because this is the first point that
        // knows BOTH the projection's vertex count and the graph's settings.
        //
        // It swaps the executor rather than narrowing the split, and that
        // distinction is the whole fix: a floor implemented inside the driver
        // still called `for_each(1, ..)`, which a thread-pool implementor
        // honours by spawning a scope to run one closure. `algowidth` measured
        // a 1,024-vertex run at 0.56x — a slowdown caused entirely by the
        // guard against slowdowns. Below the floor the installed executor is
        // now not touched at all.
        //
        // The floor counts EDGES too (rev62). It was set by `algowidth`'s
        // PageRank on a graph of 3.5 edges a vertex, where 65,536 vertices is
        // where the split stops losing. A kernel's work is the vertices AND
        // the edges, though, and Graphalytics' dota-league has 61,170 vertices
        // over 50,870,313 edges, ~830 a vertex. Under the vertex floor every
        // one of its kernels ran on one thread; its LCC held one core of 40
        // for 50 minutes. So a projection also goes to the executor once its
        // edge count reaches 16x the vertex floor (1,048,576 at the default):
        // an algowidth-shaped graph reaches that only above 300,000 vertices,
        // past the vertex floor already, so no decision the measurement made
        // changes.
        let floor = SerialExec;
        let min = self.algo_min_vertices();
        let exec: &dyn ScopedExec = if g.len() >= min || g.edge_count() >= min.saturating_mul(16) {
            exec
        } else {
            counted!("algo.fixpoint below the parallel floor");
            &floor
        };
        let mut extra: Vec<(String, Value)> = Vec::new();
        let mut iterations = 0u32;
        let mut converged = true;

        let t3 = phase_clock(timing);
        let values = match alg {
            Algorithm::PageRank => {
                let (v, r) = kernels::pagerank(
                    &g,
                    rev(),
                    exec,
                    cfg.damping,
                    cfg.tolerance,
                    cfg.max_iterations,
                    cfg.graphalytics,
                );
                iterations = r.iterations;
                converged = r.converged;
                AlgoValues::Float(v)
            }
            Algorithm::Wcc => {
                let v = kernels::wcc(&g, rev());
                extra.push((
                    "componentCount".into(),
                    Value::Int(kernels::community_count(&v) as i64),
                ));
                AlgoValues::Id(v)
            }
            Algorithm::Degree => AlgoValues::Float(kernels::degree(&g)),
            Algorithm::Bfs => {
                let src = self.algo_source(&g, cfg)?;
                AlgoValues::Depth(kernels::bfs(&g, src, None))
            }
            Algorithm::Sssp => {
                let src = self.algo_source(&g, cfg)?;
                let v = kernels::sssp(&g, src).map_err(AlgoError::Semantic)?;
                AlgoValues::Distance(v)
            }
            Algorithm::TriangleCount => {
                let (t, _) = kernels::triangles(&g, rev(), exec);
                AlgoValues::Count(t)
            }
            Algorithm::LocalClustering => {
                // Graphalytics keeps DIRECTION in the triangle test; the
                // default symmetrises both sides. See `triangles_directed`.
                let (_, c) = if cfg.graphalytics {
                    kernels::triangles_directed(&g, rev(), exec)
                } else {
                    kernels::triangles(&g, rev(), exec)
                };
                AlgoValues::Float(c)
            }
            Algorithm::LabelPropagation => {
                let (v, r, oscillated) =
                    kernels::label_propagation(&g, rev(), cfg.max_iterations, cfg.graphalytics, exec);
                iterations = r.iterations;
                converged = r.converged;
                extra.push(("oscillated".into(), Value::Bool(oscillated)));
                extra.push((
                    "communityCount".into(),
                    Value::Int(kernels::community_count(&v) as i64),
                ));
                AlgoValues::Id(v)
            }
            Algorithm::Louvain => {
                let (v, q, passes) = kernels::louvain(&g, rev(), cfg.max_iterations, cfg.resolution);
                iterations = passes;
                extra.push(("modularity".into(), Value::Float(q)));
                extra.push((
                    "communityCount".into(),
                    Value::Int(kernels::community_count(&v) as i64),
                ));
                AlgoValues::Id(v)
            }
            Algorithm::Betweenness => {
                // UNDIRECTED means the projection was asked for undirected,
                // which is `Dir::Both`. The kernel then walks the reverse
                // adjacency alongside the forward one and halves at the end,
                // because an unordered pair is otherwise counted from both
                // ends — the discrepancy a caller comparing against GDS would
                // see as every score doubled.
                // `Dir::Both` means the CSR is ALREADY symmetric — the store
                // yields both directions for it — so this flag halves the
                // result and does not change the traversal. See the kernel.
                let undirected = cfg.projection.dir == Dir::Both;
                AlgoValues::Float(kernels::betweenness(&g, undirected))
            }
            Algorithm::Scc => {
                let v = kernels::scc(&g, rev());
                extra.push((
                    "componentCount".into(),
                    Value::Int(kernels::community_count(&v) as i64),
                ));
                AlgoValues::Id(v)
            }
            Algorithm::Closeness => AlgoValues::Float(kernels::closeness(&g)),
        };
        if let Some(t3) = t3 {
            eprintln!(
                "[algo-timing] kernel {:?} ({}, width {})",
                t3.elapsed(),
                alg.name(),
                exec.width()
            );
        }

        counted!("algo.runs");
        Ok(AlgoResult {
            algorithm: full,
            ids,
            values,
            as_of: g.as_of,
            iterations,
            graphalytics: cfg.graphalytics,
            converged,
            outside: g.outside,
            extra,
        })
    }

    fn algo_source(&self, g: &super::AlgoGraph, cfg: &AlgoConfig) -> Result<u32, AlgoError> {
        let Some(id) = cfg.source else {
            return Err(AlgoError::Semantic(
                "this algorithm needs a `sourceNode` in its configuration".into(),
            ));
        };
        g.offset_of(id as u64).ok_or_else(|| {
            AlgoError::Semantic(format!(
                "sourceNode {id} is not in this projection; narrow or widen \
                 `nodeLabels` so that it is"
            ))
        })
    }

    /// Persist a result as a node property, through the ORDINARY write path.
    ///
    /// # Why this is a separate function called afterwards
    ///
    /// The computation has already finished and returned. This opens SHORT
    /// transactions, one per batch, and sets a property exactly as any
    /// statement would — index maintenance and the commit log come free
    /// because nothing here is special.
    ///
    /// A write mode that wrote DURING the fixpoint would hold entity locks for
    /// the whole run. That is the failure this shape exists to make
    /// impossible, rather than to be careful about.
    ///
    /// The values describe snapshot `result.as_of` and land at whatever commit
    /// this reaches, so both stamps are reported. GDS returns one summary and
    /// never mentions that the scores describe a graph that no longer exists.
    pub fn algo_write_back(
        &self,
        result: &AlgoResult,
        property: &str,
        batch: usize,
    ) -> Result<(u64, u64), GraphError> {
        if self.in_explicit_txn() {
            return Err(GraphError::SchemaConflict(
                "an algorithm's `write` mode cannot run inside an open transaction: it \
                 computes against a read snapshot and commits separately, so an enclosing \
                 transaction would either see its own uncommitted writes in that snapshot \
                 or hold entity locks across the whole computation. Use `mutate` here, and \
                 `write` outside."
                    .into(),
            ));
        }
        // `batch` BOUNDS THE CHUNK, NOT A TRANSACTION, and the distinction is
        // the whole of what this key means.
        //
        // The catalogue said "in short transactions" for every `.write` entry
        // and no such transaction exists here: the writes go through
        // `set_prop`, and under Bolt the enclosing statement is already one
        // autocommit transaction. That is not a shortcoming to fix — a Bolt
        // statement is ATOMIC, one that fails half-way leaves nothing behind,
        // and a write-back that committed in pieces would break exactly that
        // guarantee, leaving half a graph annotated after a failure.
        //
        // So the key shapes WORK GRANULARITY and never durability. Saying so
        // is the fix; adding transactions would be the bug.
        let batch = batch.max(1);
        let mut written = 0u64;
        let mut batches = 0u64;
        for chunk in result.ids.chunks(batch) {
            batches += 1;
            // COUNTED PER CHUNK. It sat outside this loop, firing once per
            // CALL under the name "batches committed" — a counter that did not
            // measure what it was named, and the reason a test asserting the
            // multi-chunk path saw one.
            counted!("algo.write chunks");
            for (k, id) in chunk.iter().enumerate() {
                let i = batches as usize - 1;
                let value = result.values.at(i * batch + k);
                if matches!(value, Value::Null) {
                    continue;
                }
                self.set_prop(true, *id, property, &value)?;
                written += 1;
            }
            if batches > 1 {
                sometimes!(
                    "algo.a write batched across more than one transaction",
                    true
                );
            }
        }
        Ok((written, self.store.now_ts()))
    }
}

/// Why an algorithm could not answer.
#[derive(Debug)]
pub enum AlgoError {
    /// The projection was too large. See [`Refusal`].
    Refused(Refusal),
    /// The configuration or the request was wrong.
    Semantic(String),
    /// The graph could not be read.
    Graph(GraphError),
}

impl std::fmt::Display for AlgoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlgoError::Refused(r) => write!(f, "{r}"),
            AlgoError::Semantic(m) => write!(f, "{m}"),
            AlgoError::Graph(e) => write!(f, "{e}"),
        }
    }
}

/// A serial executor, for a caller with no morsel seam of its own.
#[must_use]
pub fn serial() -> SerialExec {
    SerialExec
}

/// A clock reading for `ENGRAM_ALGO_TIMING`'s phase report — taken only when
/// the report is on, printed to stderr and never an input to an answer, which
/// is all the determinism lint guards.
#[allow(clippy::disallowed_methods)]
pub(crate) fn phase_clock(on: bool) -> Option<std::time::Instant> {
    on.then(std::time::Instant::now)
}

/// Whether `ENGRAM_ALGO_TIMING=1` asks for the phase report.
pub(crate) fn phase_timing_on() -> bool {
    std::env::var("ENGRAM_ALGO_TIMING").as_deref() == Ok("1")
}

/// One algorithm's mode, parsed from a procedure name's last segment.
#[must_use]
pub fn mode_of(segment: &str) -> Option<Mode> {
    match segment {
        "stream" => Some(Mode::Stream),
        "stats" => Some(Mode::Stats),
        "mutate" => Some(Mode::Mutate),
        "write" => Some(Mode::Write),
        _ => None,
    }
}
