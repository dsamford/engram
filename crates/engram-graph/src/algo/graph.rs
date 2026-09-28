//! The projection an algorithm iterates: a dense CSR at one snapshot.

use std::collections::BTreeMap;

use engram_observe::{counted, sometimes};

use super::Refusal;
use crate::{Dir, Graph, GraphError};

/// Which slice of the graph an algorithm runs over.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectionKey {
    /// The labels whose members are vertices. Empty means every node.
    pub labels: Vec<String>,
    /// The relationship types that are edges. Empty means every type.
    pub types: Vec<String>,
    /// Which direction an edge is followed in.
    pub dir: Dir,
    /// The property carrying an edge weight, if the algorithm is weighted.
    pub weight: Option<String>,
}

/// A pinned, dense, compressed view of one projection.
///
/// # The numbering is the determinism argument
///
/// Vertices are numbered by their position in the ASCENDING member id vector,
/// so the numbering is a pure function of the member set — two builds at the
/// same snapshot produce byte-identical arrays, and therefore an identical
/// order of floating-point additions. Everything downstream rests on that, so
/// the ascent is CHECKED rather than assumed.
#[derive(Debug, Clone)]
pub struct AlgoGraph {
    /// The snapshot this describes. Carried to every result row.
    pub as_of: u64,
    /// Dense offset to node id, ascending.
    pub ids: Vec<u64>,
    /// Row starts into [`AlgoGraph::targets`], length `n + 1`.
    pub offsets: Vec<u64>,
    /// Neighbours as dense offsets, each row sorted.
    pub targets: Vec<u32>,
    /// Per-edge weight, parallel to `targets`.
    ///
    /// Absent means unweighted. **Never defaulted to 1.0 silently**: a
    /// weighted algorithm over a projection with no weight property should say
    /// so rather than quietly compute an unweighted answer.
    pub weights: Option<Vec<f64>>,
    /// Edges whose peer fell outside the projection. Reported, never dropped
    /// silently.
    pub outside: u64,
}

impl AlgoGraph {
    /// How many vertices.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the projection is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// One vertex's neighbours, as dense offsets.
    #[must_use]
    pub fn neighbours(&self, v: u32) -> &[u32] {
        let lo = self.offsets[v as usize] as usize;
        let hi = self.offsets[v as usize + 1] as usize;
        &self.targets[lo..hi]
    }

    /// One vertex's edge weights, if the projection is weighted.
    #[must_use]
    pub fn weights_of(&self, v: u32) -> Option<&[f64]> {
        let w = self.weights.as_ref()?;
        let lo = self.offsets[v as usize] as usize;
        let hi = self.offsets[v as usize + 1] as usize;
        Some(&w[lo..hi])
    }

    /// The out-degree of `v`.
    #[must_use]
    pub fn degree(&self, v: u32) -> usize {
        (self.offsets[v as usize + 1] - self.offsets[v as usize]) as usize
    }

    /// How many edges the projection holds.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.targets.len()
    }

    /// The node id at a dense offset.
    #[must_use]
    pub fn id_of(&self, v: u32) -> u64 {
        self.ids[v as usize]
    }

    /// The dense offset of a node id, if it is in the projection.
    #[must_use]
    pub fn offset_of(&self, id: u64) -> Option<u32> {
        self.ids.binary_search(&id).ok().map(|i| i as u32)
    }

    /// The reverse projection: every edge turned around.
    ///
    /// A PULL kernel reads its IN-neighbours, and the store gives out-edges, so
    /// this is built once per projection rather than per iteration.
    #[must_use]
    pub fn reversed(&self) -> AlgoGraph {
        let n = self.len();
        let mut counts = vec![0u64; n + 1];
        for &t in &self.targets {
            counts[t as usize + 1] += 1;
        }
        for i in 0..n {
            counts[i + 1] += counts[i];
        }
        let mut cursor = counts.clone();
        let mut targets = vec![0u32; self.targets.len()];
        let mut weights = self.weights.as_ref().map(|_| vec![0.0; self.targets.len()]);
        for v in 0..n as u32 {
            let lo = self.offsets[v as usize] as usize;
            for (k, &t) in self.neighbours(v).iter().enumerate() {
                let at = cursor[t as usize] as usize;
                targets[at] = v;
                if let (Some(w), Some(src)) = (weights.as_mut(), self.weights.as_ref()) {
                    w[at] = src[lo + k];
                }
                cursor[t as usize] += 1;
            }
        }
        // Each reversed row must be sorted too — the counting sort above
        // emits them in source order, which is not dense order.
        for v in 0..n {
            let (lo, hi) = (counts[v] as usize, counts[v + 1] as usize);
            if let Some(w) = weights.as_mut() {
                let mut pairs: Vec<(u32, f64)> = targets[lo..hi]
                    .iter()
                    .copied()
                    .zip(w[lo..hi].iter().copied())
                    .collect();
                pairs.sort_by_key(|(t, _)| *t);
                for (k, (t, wt)) in pairs.into_iter().enumerate() {
                    targets[lo + k] = t;
                    w[lo + k] = wt;
                }
            } else {
                targets[lo..hi].sort_unstable();
            }
        }
        AlgoGraph {
            as_of: self.as_of,
            ids: self.ids.clone(),
            offsets: counts,
            targets,
            weights,
            outside: self.outside,
        }
    }
}

/// A built projection as the kernels receive it: shared, never copied, with
/// its REVERSE (the PULL kernels' in-neighbours) built at most once, when a
/// kernel first asks for it. BFS and SSSP never do; they built it anyway, on
/// every call, before the kernel ran.
#[derive(Debug, Clone)]
pub(crate) struct AlgoProjection {
    graph: std::sync::Arc<AlgoGraph>,
    reversed: std::sync::Arc<std::sync::OnceLock<AlgoGraph>>,
}

impl AlgoProjection {
    pub(crate) fn new(graph: std::sync::Arc<AlgoGraph>) -> Self {
        AlgoProjection {
            graph,
            reversed: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Every edge turned around ([`AlgoGraph::reversed`]), built on first use
    /// and kept with the projection.
    pub(crate) fn reversed(&self) -> &AlgoGraph {
        self.reversed.get_or_init(|| {
            counted!("algo.reverse built");
            self.graph.reversed()
        })
    }

    /// The bytes the forward arrays hold (the reverse holds as many again
    /// once built).
    pub(crate) fn bytes(&self) -> u64 {
        let g = &self.graph;
        let arrays = g.ids.len() * 8
            + g.offsets.len() * 8
            + g.targets.len() * 4
            + g.weights.as_ref().map_or(0, |w| w.len() * 8);
        arrays as u64
    }
}

impl std::ops::Deref for AlgoProjection {
    type Target = AlgoGraph;
    fn deref(&self) -> &AlgoGraph {
        &self.graph
    }
}

/// One projection kept between statements: graph `graph_id`'s, built at
/// commit clock `at` for `key` ([`Graph::algo_graph`]).
type KeptProjection = (u64, u64, ProjectionKey, AlgoProjection);

/// How many projections a graph keeps between statements: an unweighted one
/// and a weighted one serve a Graphalytics graph's six kernels.
const KEPT_PROJECTIONS: usize = 2;

/// The projections kept between statements, of every graph in the process.
///
/// PROCESS-WIDE AND KEYED BY `graph_id` (never reused, unlike an address)
/// rather than a field of `Graph`. The first build held them in a field, and
/// the field moved `Graph`'s layout: SNB BI bi2, which reaches nothing of the
/// algorithm layer, ran ~22% slower on that build at SF3 and at SF10 (ab137:
/// 585-611 ms against 461-510 on the builds before and after it; chain117 at
/// SF10 1,997 against 1,633), while every other statement measured the same.
/// A graph's entries go when it does (`Drop for Graph`).
static KEPT: std::sync::Mutex<Vec<KeptProjection>> = std::sync::Mutex::new(Vec::new());
/// How many entries are kept, so a graph that never kept one does not take
/// the lock to forget them when it is dropped.
static KEPT_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Forget graph `graph_id`'s kept projections: its `Drop`.
pub(crate) fn forget_kept_projections(graph_id: u64) {
    if KEPT_LIVE.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|(g, ..)| *g != graph_id);
    KEPT_LIVE.store(kept.len(), std::sync::atomic::Ordering::Release);
}

thread_local! {
    // The projection built for the CURRENT statement, if any.
    //
    // `algo_graph` is a pure function of committed state, and it was rebuilt
    // ON EVERY CALL. LDBC SNB BI **bi19** calls
    // `engram.algo.kshortestpaths.stream` once per (person, person) pair —
    // 52 x 43 = 2,236 times at SF3 — over the SAME 565,247-edge projection,
    // and measured it rebuilt it 2,236 times: 1 call 7 s, 5 calls 29 s, 20
    // calls 105 s, with `algo.graph built` equal to the call count each time.
    // Linear, and essentially all of it the build.
    //
    // The memo is keyed by the STATEMENT generation, not by an epoch. A
    // statement sees one snapshot, so a projection built inside it cannot go
    // stale inside it — which is a far narrower claim than a cross-statement
    // cache would need, and it is the whole win for a query that calls an
    // algorithm in a loop. (The cross-statement keep, `Graph::algo_graph`,
    // makes the wider claim on the commit clock.)
    //
    // A statement that WRITES is excluded: its own buffered writes could
    // change the projection under the next call.
    static STATEMENT_PROJECTION: std::cell::RefCell<Option<(u64, ProjectionKey, AlgoProjection)>> =
        const { std::cell::RefCell::new(None) };
}

/// Projections built from ROWS by `engram.algo.project`, keyed by the
/// creating statement's generation and the caller's name.
///
/// PROCESS-WIDE AND LOCKED, not thread-local: the algorithm that reads one may
/// run on a WORKER thread of the same statement, and a worker's own statement
/// generation is 0. So a reader names a projection by its HANDLE —
/// `name#generation`, which `engram.algo.project` YIELDs — and any thread can
/// resolve it. Generations are unique across the process, so two concurrent
/// statements that both call theirs `q15` never see each other's.
///
/// STATEMENT-SCOPED: the creating thread's outermost `StatementScope` removes
/// its entries when it ends. Nothing outlives its statement, so this is not
/// the graph catalogue `proc.rs` declines to promise by refusing `gds.*`
/// names.
type NamedEntry = (std::sync::Arc<AlgoGraph>, u64);
static NAMED_PROJECTIONS: std::sync::Mutex<BTreeMap<(u64, String), NamedEntry>> =
    std::sync::Mutex::new(BTreeMap::new());
/// How many entries are live, so a statement that built none never takes the
/// lock to release them — this runs at the end of EVERY statement.
static NAMED_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Drop every projection built by statement `generation`; called when that
/// statement's outermost scope ends.
pub(crate) fn release_named_projections(generation: u64) {
    if NAMED_LIVE.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let mut m = NAMED_PROJECTIONS.lock().unwrap_or_else(|p| p.into_inner());
    let before = m.len();
    m.retain(|(g, _), _| *g != generation);
    let dropped = before - m.len();
    if dropped > 0 {
        NAMED_LIVE.fetch_sub(dropped, std::sync::atomic::Ordering::AcqRel);
        counted!("algo.row projection released with its statement");
    }
}

/// Publish a row-built projection under `(generation, name)`, within a byte
/// budget shared by every live one. Returns the handle a reader passes back.
pub(crate) fn register_named_projection(
    generation: u64,
    name: &str,
    graph: AlgoGraph,
    bytes: u64,
    budget: u64,
) -> Result<String, String> {
    let mut m = NAMED_PROJECTIONS.lock().unwrap_or_else(|p| p.into_inner());
    let key = (generation, name.to_string());
    let others: u64 = m
        .iter()
        .filter(|(k, _)| **k != key)
        .map(|(_, (_, b))| *b)
        .sum();
    if others.saturating_add(bytes) > budget {
        return Err(Refusal {
            algorithm: "engram.algo.project".to_string(),
            what: "bytes of live row-built projections",
            measured: others.saturating_add(bytes),
            ceiling: budget,
            lever: "ENGRAM_ALGO_BYTE_CEILING",
        }
        .to_string());
    }
    if m.insert(key, (std::sync::Arc::new(graph), bytes)).is_none() {
        NAMED_LIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    Ok(format!("{name}#{generation}"))
}

/// Resolve a handle (`name#generation`), or a bare name on the thread that
/// built it.
pub(crate) fn named_projection(handle: &str) -> Result<std::sync::Arc<AlgoGraph>, String> {
    let parsed = handle
        .rsplit_once('#')
        .and_then(|(n, g)| g.parse::<u64>().ok().map(|g| (g, n.to_string())));
    let key = match parsed {
        Some(k) => k,
        None => {
            let generation = crate::interp::statement_gen();
            if generation == 0 {
                return Err(format!(
                    "projection `{handle}`: pass the `projection` value engram.algo.project \
                     YIELDed. A bare name resolves only on the thread that built it, and \
                     this call is running on a worker of the statement."
                ));
            }
            (generation, handle.to_string())
        }
    };
    let m = NAMED_PROJECTIONS.lock().unwrap_or_else(|p| p.into_inner());
    m.get(&key).map(|(g, _)| std::sync::Arc::clone(g)).ok_or_else(|| {
        format!(
            "no projection `{handle}` in this statement. A projection built by \
             engram.algo.project lives only until the statement that built it ends."
        )
    })
}

/// The bytes a row-built projection holds: ids and offsets per vertex,
/// targets and weights per edge.
pub(crate) fn row_projection_bytes(nodes: u64, edges: u64) -> u64 {
    nodes
        .saturating_mul(8 + 8)
        .saturating_add(edges.saturating_mul(4 + 8))
}

impl Graph {
    /// Build a projection from an EDGE LIST (`engram.algo.project`).
    ///
    /// The vertices are the members of `labels` — not the edges' endpoints —
    /// so a node with no edge is still in the graph, and a route to it is
    /// "none" rather than "that node is not in this projection". SNB BI bi15
    /// needs exactly that: `-1.0` for a disconnected pair.
    ///
    /// THE NUMBERING IS THE SAME DETERMINISM ARGUMENT AS `algo_graph`'s:
    /// vertices by ascending id, and each row's edges SORTED — by target, then
    /// by weight bits for parallel edges — so the arrays are a pure function
    /// of the edge SET, whatever order the rows arrived in and whichever
    /// worker produced them.
    ///
    /// Priced before built, against the same ceilings as a stored projection.
    pub(crate) fn algo_graph_from_edges(
        &self,
        labels: &[String],
        dir: Dir,
        edges: &[(u64, u64, f64)],
    ) -> Result<AlgoGraph, String> {
        let full = "engram.algo.project".to_string();
        let mut ids: Vec<u64> = Vec::new();
        if labels.is_empty() {
            ids.extend(self.members(None).map_err(|e| format!("{e:?}"))?.iter());
        } else {
            for l in labels {
                ids.extend(self.members(Some(l)).map_err(|e| format!("{e:?}"))?.iter());
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let n = ids.len() as u64;
        if n > self.algo_node_ceiling() {
            return Err(Refusal {
                algorithm: full,
                what: "nodes",
                measured: n,
                ceiling: self.algo_node_ceiling(),
                lever: "ENGRAM_ALGO_NODE_CEILING",
            }
            .to_string());
        }
        let e = (edges.len() as u64).saturating_mul(if dir == Dir::Both { 2 } else { 1 });
        if e > self.algo_edge_ceiling() {
            return Err(Refusal {
                algorithm: full,
                what: "edges",
                measured: e,
                ceiling: self.algo_edge_ceiling(),
                lever: "ENGRAM_ALGO_EDGE_CEILING",
            }
            .to_string());
        }
        let bytes = row_projection_bytes(n, e);
        if bytes > self.algo_byte_ceiling() {
            return Err(Refusal {
                algorithm: full,
                what: "bytes of working set",
                measured: bytes,
                ceiling: self.algo_byte_ceiling(),
                lever: "ENGRAM_ALGO_BYTE_CEILING",
            }
            .to_string());
        }
        let mut rows: Vec<Vec<(u32, f64)>> = vec![Vec::new(); ids.len()];
        let mut outside = 0u64;
        for &(s, t, w) in edges {
            let (Ok(si), Ok(ti)) = (ids.binary_search(&s), ids.binary_search(&t)) else {
                // an endpoint outside `nodeLabels`: counted, never silently kept
                outside += 1;
                continue;
            };
            let (si, ti) = (si as u32, ti as u32);
            match dir {
                Dir::Out => rows[si as usize].push((ti, w)),
                Dir::In => rows[ti as usize].push((si, w)),
                Dir::Both => {
                    rows[si as usize].push((ti, w));
                    rows[ti as usize].push((si, w));
                }
            }
        }
        let mut offsets = Vec::with_capacity(ids.len() + 1);
        let mut targets = Vec::with_capacity(e as usize);
        let mut weights = Vec::with_capacity(e as usize);
        offsets.push(0u64);
        for row in &mut rows {
            row.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_bits().cmp(&b.1.to_bits())));
            for &(t, w) in row.iter() {
                targets.push(t);
                weights.push(w);
            }
            offsets.push(targets.len() as u64);
        }
        if outside > 0 {
            sometimes!("algo.a row projection edge fell outside its nodeLabels", true);
        }
        counted!("algo.graph built from rows");
        Ok(AlgoGraph {
            as_of: self.store.now_ts(),
            ids,
            offsets,
            targets,
            weights: Some(weights),
            outside,
        })
    }

    /// Price a projection, and refuse before building it if it is too large.
    ///
    /// **BEFORE, not after.** An algorithm has no slower path to decline into,
    /// so the refusal has to happen where the cost is known and nothing has
    /// been allocated. "Build it and let the allocator decide" is what an LDBC
    /// IC13 out-of-memory taught this engine costs.
    pub(crate) fn algo_price(
        &self,
        algorithm: &str,
        key: &ProjectionKey,
        state_bytes_per_node: u64,
        // Whether the CALLER will price `nodes x edges` against the all-pairs
        // work ceiling. Only then does an over-estimated edge count risk
        // refusing work the exact figure would admit, so only then must the
        // cheap bound clear that ceiling too.
        all_pairs: bool,
    ) -> Result<(u64, u64), Refusal> {
        let n = if key.labels.is_empty() {
            self.count_all_nodes()
        } else {
            key.labels.iter().map(|l| self.count_label_nodes(l)).sum()
        };
        if n > self.algo_node_ceiling() {
            return Err(Refusal {
                algorithm: algorithm.to_string(),
                what: "nodes",
                measured: n,
                ceiling: self.algo_node_ceiling(),
                lever: "ENGRAM_ALGO_NODE_CEILING",
            });
        }
        // THE REFUSAL MUST BE CHEAPER THAN THE THING IT REFUSES, and the
        // per-node pass below is not: `count_adjacent_memo` on a cold type
        // builds a whole-store degree table, so pricing the SF3 friendship
        // graph (24,328 people, 1.13M friendships) cost 35 s before a build
        // whose own reads are free once the adjacency tables exist. Measured:
        // a materialising traversal of the same edges on the same cold server
        // takes 0 s, and the projection still took 35 s after it.
        //
        // So try an O(1) UPPER BOUND from the maintained counts first. Summing
        // per-node degree over a label subset cannot exceed the type's total
        // edge count (doubled for an undirected view, where each edge is seen
        // from both ends), so a bound that clears every ceiling guarantees the
        // exact figure clears them too — the decision is identical, without
        // the pass. A bound that does NOT clear them proves nothing, and falls
        // through to the exact count, which is what decides as before.
        //
        // Every ceiling is checked against the bound, the all-pairs work one
        // included: returning a bound the caller then multiplies could refuse
        // work the exact figure would admit, which would be a behaviour change
        // rather than an optimisation.
        let type_tokens = self.algo_type_tokens(key);
        let sample: Vec<u64> = if key.labels.is_empty() {
            self.members(None)
                .map(|m| m.iter().collect())
                .unwrap_or_default()
        } else {
            let mut v: Vec<u64> = Vec::new();
            for l in &key.labels {
                if let Ok(m) = self.members(Some(l)) {
                    v.extend(m.iter());
                }
            }
            v.sort_unstable();
            v.dedup();
            v
        };
        // `count_adjacent_memo`, NOT `edge_count_slim`.
        //
        // `edge_count_slim(from, dir, types, TO)` counts the edges between two
        // NAMED nodes; its last parameter is a peer id, not a cap. This priced
        // every projection by asking how many edges ran from each node to node
        // `u64::MAX`, which is none — so `e` was ZERO for every projection ever
        // priced, and with it the edge ceiling, the edge term of the byte
        // ceiling, and the all-pairs work ceiling that multiplies by it. Three
        // guards that could not fire, reading as three guards that never
        // needed to.
        //
        // The guard existed, was checked, and measured the wrong quantity —
        // and a pair probe and a degree differ by one argument's meaning,
        // which is exactly the kind of thing a type does not catch and a test
        // only catches if it sets the ceiling low enough to expect a refusal.
        // ids + offsets + targets, doubled for the reverse view, plus per-node
        // algorithm state — the same formula the exact path uses below.
        let bytes_for = |e: u64| -> u64 {
            n.saturating_mul(8 + 8 + state_bytes_per_node)
                .saturating_add(e.saturating_mul(4 * 2))
                .saturating_add(if key.weight.is_some() {
                    e.saturating_mul(8 * 2)
                } else {
                    0
                })
        };
        let bound = {
            let per_type = self.type_edge_count(&type_tokens);
            match key.dir {
                crate::Dir::Both => per_type.saturating_mul(2),
                _ => per_type,
            }
        };
        // THE WORK CEILING IS THE CALLER'S, AND ONLY FOR AN ALL-PAIRS
        // ALGORITHM. Requiring it here regardless is what made the first
        // version of this fix a no-op: BFS is not all-pairs, but
        // 24,328 x 1,130,494 is 27.5 billion, so the bound never cleared a
        // ceiling that was never going to be applied to it, and every call
        // fell through to the 32.6 s degree pass it was written to skip.
        if bound <= self.algo_edge_ceiling()
            && bytes_for(bound) <= self.algo_byte_ceiling()
            && (!all_pairs || n.saturating_mul(bound) <= self.algo_work_ceiling())
        {
            counted!("algo.priced from the maintained counts, without a degree pass");
            return Ok((n, bound));
        }

        counted!("algo.priced by counting every node's degree");
        let e: u64 = sample
            .iter()
            .map(|id| self.count_adjacent_memo(*id, key.dir, &type_tokens))
            .sum();
        if e > self.algo_edge_ceiling() {
            return Err(Refusal {
                algorithm: algorithm.to_string(),
                what: "edges",
                measured: e,
                ceiling: self.algo_edge_ceiling(),
                lever: "ENGRAM_ALGO_EDGE_CEILING",
            });
        }
        let bytes = bytes_for(e);
        if bytes > self.algo_byte_ceiling() {
            return Err(Refusal {
                algorithm: algorithm.to_string(),
                what: "bytes of working set",
                measured: bytes,
                ceiling: self.algo_byte_ceiling(),
                lever: "ENGRAM_ALGO_BYTE_CEILING",
            });
        }
        Ok((n, e))
    }

    fn algo_type_tokens(&self, key: &ProjectionKey) -> Option<Vec<u32>> {
        if key.types.is_empty() {
            return None;
        }
        let mut out: Vec<u32> = key
            .types
            .iter()
            .filter_map(|t| {
                self.type_tokens_peek(std::slice::from_ref(t))
                    .and_then(|v| v.first().copied())
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Build the projection.
    ///
    /// Vertices come from the labels' membership, ASCENDING and deduplicated;
    /// edges from the adjacency the store already holds. No record is read: an
    /// algorithm needs structure, not properties, unless it is weighted.
    pub(crate) fn algo_graph(&self, key: &ProjectionKey) -> Result<AlgoProjection, GraphError> {
        let stmt_gen = crate::interp::statement_gen();
        let memoable = stmt_gen != 0 && !self.in_txn_with_writes();
        if memoable {
            let hit = STATEMENT_PROJECTION.with(|c| match &*c.borrow() {
                Some((g, k, built)) if *g == stmt_gen && k == key => Some(built.clone()),
                _ => None,
            });
            if let Some(built) = hit {
                counted!("algo.graph reused within the statement");
                return Ok(built);
            }
        }
        // KEPT BETWEEN STATEMENTS WHILE NOTHING COMMITS.
        //
        // The projection is a pure function of committed state, so one built
        // at commit clock `t` is exact for every statement that runs while the
        // clock still reads `t`: the rule the relationship memo keeps its
        // values by (`RelPropMemo`), coarse and needing no change log. It was
        // rebuilt by every statement. On LDBC Graphalytics that put the build
        // in every timed repetition: SSSP on datagen-7_5-fb (34,185,747 edges)
        // read 163-190 s a call, the weights one full relationship record per
        // edge, against a Dijkstra of seconds, and a warm-up call made before
        // the repetitions could not take the build out of them. The clock is
        // read BEFORE the build, so a commit that lands during it leaves the
        // kept projection already stale. A transaction with buffered writes
        // neither reads nor keeps one.
        let clock = self.store.now_ts();
        let keep = !self.in_txn_with_writes();
        let graph_id = self.graph_id;
        if keep && KEPT_LIVE.load(std::sync::atomic::Ordering::Acquire) > 0 {
            let hit = {
                let kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
                kept.iter()
                    .find(|(g, at, k, _)| *g == graph_id && *at == clock && k == key)
                    .map(|(_, _, _, built)| built.clone())
            };
            if let Some(built) = hit {
                counted!("algo.graph reused across statements: nothing committed since its build");
                if memoable {
                    STATEMENT_PROJECTION
                        .with(|c| *c.borrow_mut() = Some((stmt_gen, key.clone(), built.clone())));
                }
                return Ok(built);
            }
        }
        let mut ids: Vec<u64> = Vec::new();
        if key.labels.is_empty() {
            ids.extend(self.members(None)?.iter());
        } else {
            for l in &key.labels {
                ids.extend(self.members(Some(l))?.iter());
            }
        }
        ids.sort_unstable();
        ids.dedup();
        // THE NUMBERING'S PRECONDITION, CHECKED. Everything below rests on the
        // dense id being a pure function of this vector, so a vector that is
        // not strictly ascending would make two builds disagree.
        debug_assert!(ids.windows(2).all(|w| w[0] < w[1]));

        let n = ids.len();
        let type_tokens = self.algo_type_tokens(key);
        let weight_token = key
            .weight
            .as_ref()
            .and_then(|p| self.token_peek("prop:", &self.props, p));

        let mut rows: Vec<Vec<(u32, f64)>> = vec![Vec::new(); n];
        // A WEIGHTED projection keeps each edge's relationship id beside its
        // target, so that each edge is given its own weight.
        let mut rel_rows: Vec<Vec<(u32, u64)>> = if weight_token.is_some() {
            vec![Vec::new(); n]
        } else {
            Vec::new()
        };
        let mut outside = 0u64;
        for (v, id) in ids.iter().enumerate() {
            let row = &mut rows[v];
            let mut rel_row = rel_rows.get_mut(v);
            self.adjacent_slim_for_each(*id, key.dir, &type_tokens, |adj| {
                match ids.binary_search(&adj.peer) {
                    Ok(t) => {
                        row.push((t as u32, 1.0));
                        if let Some(rr) = rel_row.as_mut() {
                            rr.push((t as u32, adj.rel));
                        }
                    }
                    // An edge whose other end is not in the projection is not
                    // an error — a label-scoped projection has a boundary — but
                    // it is COUNTED, because a projection quietly losing most
                    // of its edges is the kind of thing that looks like a bad
                    // algorithm rather than a bad projection.
                    Err(_) => outside += 1,
                }
            });
            // THE ROW ORDER IS THE STORE'S, WHICH IS NOT A PROPERTY OF THE
            // GRAPH: it depends on the relationship type layout and on which
            // cached table served. Sorting makes it one. A weighted row sorts
            // by target and then by relationship id, so parallel edges keep
            // a fixed order too.
            row.sort_by_key(|(t, _)| *t);
            if let Some(rr) = rel_row {
                rr.sort_unstable();
            }
        }

        if let Some(wt) = weight_token {
            self.algo_fill_weights(&mut rows, &rel_rows, key, wt)?;
        }

        let mut offsets = Vec::with_capacity(n + 1);
        let mut targets = Vec::new();
        let mut weights = if key.weight.is_some() {
            Some(Vec::new())
        } else {
            None
        };
        offsets.push(0u64);
        for row in &rows {
            for (t, w) in row {
                targets.push(*t);
                if let Some(ws) = weights.as_mut() {
                    ws.push(*w);
                }
            }
            offsets.push(targets.len() as u64);
        }
        if outside > 0 {
            sometimes!("algo.an edge left the projection", true);
        }
        counted!("algo.graph built");
        let built = AlgoProjection::new(std::sync::Arc::new(AlgoGraph {
            as_of: self.store.now_ts(),
            ids,
            offsets,
            targets,
            weights,
            outside,
        }));
        if memoable {
            STATEMENT_PROJECTION
                .with(|c| *c.borrow_mut() = Some((stmt_gen, key.clone(), built.clone())));
        }
        if keep {
            // A graph keeps at most `KEPT_PROJECTIONS`, within its algorithm
            // byte ceiling twice over (a projection and its reverse); one from
            // an older clock can never be read again. Other graphs' entries
            // are theirs.
            let ceiling = self.algo_byte_ceiling().saturating_mul(2);
            let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
            kept.retain(|(g, at, k, _)| *g != graph_id || (*at == clock && k != key));
            let mut mine: Vec<usize> = (0..kept.len()).filter(|&i| kept[i].0 == graph_id).collect();
            let mut total: u64 = mine.iter().map(|&i| kept[i].3.bytes() * 2).sum();
            while let Some(&oldest) = mine.first() {
                if mine.len() < KEPT_PROJECTIONS && total + built.bytes() * 2 <= ceiling {
                    break;
                }
                let (_, _, _, gone) = kept.remove(oldest);
                total -= gone.bytes() * 2;
                mine = (0..kept.len()).filter(|&i| kept[i].0 == graph_id).collect();
            }
            if built.bytes() * 2 <= ceiling {
                kept.push((graph_id, clock, key.clone(), built.clone()));
                counted!("algo.graph kept for the next statement");
            }
            KEPT_LIVE.store(kept.len(), std::sync::atomic::Ordering::Release);
        }
        Ok(built)
    }

    /// Each edge's weight from its relationship property, as ONE sorted
    /// gather for the whole projection (`column_entries_gather`): the
    /// property is read off each record's bytes through a shared block
    /// cursor, never the record decoded. It read one full relationship record
    /// per edge (`rel`): 34,185,747 of them for Graphalytics SSSP on
    /// datagen-7_5-fb, 163-190 s a call. `rel_rows` holds, per row, the
    /// `(target, relationship)` pairs in the row's own order, so each edge
    /// takes its own weight. The per-target map it replaces gave parallel
    /// edges between one pair the weight of whichever was read last. Inside a
    /// transaction with buffered writes the committed values cannot see them,
    /// so each edge is read by a projected read at the transaction's snapshot.
    fn algo_fill_weights(
        &self,
        rows: &mut [Vec<(u32, f64)>],
        rel_rows: &[Vec<(u32, u64)>],
        key: &ProjectionKey,
        _weight_token: u32,
    ) -> Result<(), GraphError> {
        let prop = key.weight.as_deref().unwrap_or_default();
        let gathered: Option<Vec<(u64, crate::Value)>> = if self.in_txn_with_writes() {
            None
        } else {
            let mut all: Vec<u64> = rel_rows
                .iter()
                .flat_map(|r| r.iter().map(|&(_, rel)| rel))
                .collect();
            all.sort_unstable();
            all.dedup();
            counted!("algo.weights read in one gather", all.len() as u64);
            // Sorted by id, a relationship without the property left out.
            Some(self.column_entries_gather(crate::ColumnFamily::Rels, prop, &all)?)
        };
        let want: std::collections::BTreeSet<String> = std::iter::once(prop.to_string()).collect();
        let mut missing = 0u64;
        for (row, rel_row) in rows.iter_mut().zip(rel_rows) {
            debug_assert_eq!(row.len(), rel_row.len());
            // Both are sorted by target; `rel_row` then by relationship id.
            // Parallel edges share their target, so the slots of one target
            // take its relationships' weights in relationship order.
            for ((t, w), &(rt, rel)) in row.iter_mut().zip(rel_row) {
                debug_assert_eq!(*t, rt);
                let value = match &gathered {
                    Some(col) => col
                        .binary_search_by_key(&rel, |(id, _)| *id)
                        .ok()
                        .map(|at| col[at].1.clone()),
                    None => self.rel_projected(rel, &want)?.and_then(|r| r.props.get(prop).cloned()),
                };
                *w = match value {
                    Some(crate::Value::Float(f)) => f,
                    Some(crate::Value::Int(i)) => i as f64,
                    // A relationship with no weight, or one whose weight is
                    // not a number, contributes 1.0 — and is COUNTED, so a
                    // projection whose weights are mostly absent is visible
                    // rather than looking like a badly-weighted graph.
                    _ => {
                        missing += 1;
                        1.0
                    }
                };
            }
        }
        if missing > 0 {
            sometimes!("algo.a weight property was missing", true);
            counted!("algo.weights defaulted");
        }
        Ok(())
    }
}
