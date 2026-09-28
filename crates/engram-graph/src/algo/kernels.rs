//! The algorithms themselves.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use engram_observe::{counted, sometimes};

use super::fixpoint::{FixpointReport, VertexProgram, run};
use super::graph::AlgoGraph;
use crate::scoped_exec::ScopedExec;

// ─── PageRank ──────────────────────────────────────────────────────────────

/// PageRank, in its pull form.
///
/// `pr[v] = (1-d)/n + d(dangling/n + Σ_{u ∈ in(v)} pr[u]/outdeg(u))`
struct PageRank {
    damping: f64,
    tolerance: f64,
    n: f64,
    /// `1/outdeg`, precomputed.
    ///
    /// **Also a determinism choice, not only a speed one.** Dividing per edge
    /// and multiplying per edge are not the same operation in floating point,
    /// so computing this once fixes which of the two every iteration performs.
    inv_out: Vec<f64>,
    /// Mass held by vertices with no out-edges, redistributed evenly.
    dangling: f64,
}

impl VertexProgram for PageRank {
    type State = f64;

    fn init(&self, _v: u32, g: &AlgoGraph) -> f64 {
        if g.is_empty() {
            0.0
        } else {
            1.0 / g.len() as f64
        }
    }

    fn pull(&self, v: u32, prev: &[f64], _g: &AlgoGraph, rev: &AlgoGraph) -> f64 {
        let mut acc = 0.0;
        for &u in rev.neighbours(v) {
            acc += prev[u as usize] * self.inv_out[u as usize];
        }
        (1.0 - self.damping) / self.n + self.damping * (self.dangling / self.n + acc)
    }

    fn delta(&self, old: f64, new: f64) -> f64 {
        (old - new).abs()
    }

    fn tolerance(&self) -> f64 {
        self.tolerance
    }

    fn after_iteration(&mut self, values: &[f64], g: &AlgoGraph) {
        // SERIAL, ascending, over the whole array — see `fixpoint::run`.
        let mut mass = 0.0;
        for (v, val) in values.iter().enumerate() {
            if g.degree(v as u32) == 0 {
                mass += *val;
            }
        }
        self.dangling = mass;
    }
}

/// Run PageRank over `g`.
pub(crate) fn pagerank(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    exec: &dyn ScopedExec,
    damping: f64,
    tolerance: f64,
    max_iterations: u32,
    graphalytics: bool,
) -> (Vec<f64>, FixpointReport) {
    // GRAPHALYTICS HAS NO CONVERGENCE TEST: the specification says exactly
    // `numberOfIterations` synchronous rounds, and the expected scores ARE the
    // state after that many. `fixpoint::run` stops early when
    // `delta <= tolerance`, so a graph that settles sooner than the cap would
    // return a different vector -- correct as an engine answer, wrong as
    // conformance.
    //
    // `delta` is a sum of absolute differences and therefore never negative,
    // so a negative tolerance makes the test unsatisfiable and the loop runs
    // the full cap. That is a smaller and more obviously correct change than
    // threading a flag through the shared fixpoint driver, which every other
    // iterative kernel also uses.
    //
    // It does not change the SF10 conformance result -- the break was measured
    // not to fire at 14 iterations on LDBC's own graph -- so this closes a
    // RISK rather than a demonstrated failure: a graph converging sooner would
    // have diverged silently.
    let tolerance = if graphalytics { -1.0 } else { tolerance };
    let n = g.len();
    let inv_out: Vec<f64> = (0..n as u32)
        .map(|v| {
            let d = g.degree(v);
            if d == 0 { 0.0 } else { 1.0 / d as f64 }
        })
        .collect();
    let mut p = PageRank {
        damping,
        tolerance,
        n: if n == 0 { 1.0 } else { n as f64 },
        inv_out,
        dangling: 0.0,
    };
    counted!("algo.pagerank runs");
    run(&mut p, g, rev, exec, max_iterations)
}

// ─── Weakly connected components ───────────────────────────────────────────

/// Weakly connected components, by union-find.
///
/// **Serial, deliberately.** A lock-free union-find is order-dependent in its
/// STRUCTURE even when its answer is not, and an answer that is right for
/// reasons that vary run to run is not one the simulation lane can replay.
///
/// Components are labelled by their smallest NODE ID rather than by whichever
/// root the union happened to pick, so a label is stable across runs, across
/// executor widths, and across an unrelated change to the numbering.
pub(crate) fn wcc(g: &AlgoGraph, rev: &AlgoGraph) -> Vec<u64> {
    let n = g.len();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    let mut size: Vec<u32> = vec![1; n];

    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            // Path halving: point at the grandparent as we go.
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }

    // Edges are unioned in a FIXED order — ascending source, then the row's
    // own (already sorted) order — so the tree's shape is reproducible even
    // though its shape does not change the answer.
    for v in 0..n as u32 {
        for &u in g.neighbours(v).iter().chain(rev.neighbours(v)) {
            let (a, b) = (find(&mut parent, v), find(&mut parent, u));
            if a == b {
                continue;
            }
            // Union by size, ties broken by the LOWER dense offset, so the
            // representative is a pure function of the input.
            let (big, small) = if size[a as usize] > size[b as usize]
                || (size[a as usize] == size[b as usize] && a < b)
            {
                (a, b)
            } else {
                (b, a)
            };
            parent[small as usize] = big;
            size[big as usize] += size[small as usize];
        }
    }

    // Canonicalise to the component's smallest node id.
    let mut min_id: BTreeMap<u32, u64> = BTreeMap::new();
    for v in 0..n as u32 {
        let r = find(&mut parent, v);
        let id = g.id_of(v);
        min_id
            .entry(r)
            .and_modify(|m| *m = (*m).min(id))
            .or_insert(id);
    }
    counted!("algo.wcc runs");
    (0..n as u32)
        .map(|v| {
            let r = find(&mut parent, v);
            min_id[&r]
        })
        .collect()
}

// ─── Degree ────────────────────────────────────────────────────────────────

/// Degree centrality, weighted or not.
pub(crate) fn degree(g: &AlgoGraph) -> Vec<f64> {
    counted!("algo.degree runs");
    (0..g.len() as u32)
        .map(|v| match g.weights_of(v) {
            Some(w) => w.iter().sum(),
            None => g.degree(v) as f64,
        })
        .collect()
}

// ─── BFS and SSSP ──────────────────────────────────────────────────────────

/// Unweighted single-source shortest paths, level by level.
///
/// The frontier is expanded in ASCENDING dense order, so a vertex reachable by
/// several equal-length paths takes the lowest-numbered parent — a tie-break
/// that is total, and therefore reproducible.
pub(crate) fn bfs(g: &AlgoGraph, source: u32, max_depth: Option<u32>) -> Vec<Option<u32>> {
    let n = g.len();
    let mut dist: Vec<Option<u32>> = vec![None; n];
    if (source as usize) >= n {
        return dist;
    }
    dist[source as usize] = Some(0);
    let mut frontier = vec![source];
    let mut depth = 0u32;
    while !frontier.is_empty() {
        if max_depth.is_some_and(|m| depth >= m) {
            break;
        }
        depth += 1;
        let mut next = Vec::new();
        for &v in &frontier {
            for &u in g.neighbours(v) {
                if dist[u as usize].is_none() {
                    dist[u as usize] = Some(depth);
                    next.push(u);
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        frontier = next;
    }
    counted!("algo.bfs runs");
    dist
}

/// Weighted single-source shortest paths, by Dijkstra.
///
/// The heap is keyed `(distance, dense offset)`, so the settle order is TOTAL
/// — which is what makes the answer reproducible when several paths tie.
///
/// **A negative weight is refused, not mis-answered.** Dijkstra settles a
/// vertex permanently on first pop, which is only sound while no later edge
/// can shorten it; running it over negative weights returns a plausible wrong
/// answer rather than an obviously wrong one.
pub(crate) fn sssp(g: &AlgoGraph, source: u32) -> Result<Vec<Option<f64>>, String> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let n = g.len();
    let mut dist: Vec<Option<f64>> = vec![None; n];
    if (source as usize) >= n {
        return Ok(dist);
    }
    for v in 0..n as u32 {
        if let Some(ws) = g.weights_of(v) {
            for (k, w) in ws.iter().enumerate() {
                if *w < 0.0 {
                    counted!("algo.sssp refused a negative weight");
                    return Err(format!(
                        "engram.algo.sssp: the edge from node {} to node {} has weight {w}; \
                         Dijkstra requires non-negative weights",
                        g.id_of(v),
                        g.id_of(g.neighbours(v)[k])
                    ));
                }
            }
        }
    }

    // f64 is not Ord, so the key is its bit pattern for non-negative values,
    // which orders identically and is total.
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
    dist[source as usize] = Some(0.0);
    heap.push(Reverse((0u64, source)));
    while let Some(Reverse((bits, v))) = heap.pop() {
        let d = f64::from_bits(bits);
        if dist[v as usize].is_some_and(|cur| d > cur) {
            continue;
        }
        let ws = g.weights_of(v);
        for (k, &u) in g.neighbours(v).iter().enumerate() {
            let w = ws.map_or(1.0, |ws| ws[k]);
            let nd = d + w;
            if dist[u as usize].is_none_or(|cur| nd < cur) {
                dist[u as usize] = Some(nd);
                heap.push(Reverse((nd.to_bits(), u)));
            }
        }
    }
    counted!("algo.sssp runs");
    Ok(dist)
}

// ─── Triangles and local clustering ────────────────────────────────────────

/// The undirected neighbour sets, deduplicated and without self-loops, in CSR.
///
/// # Why CSR and not `Vec<Vec<u32>>`
///
/// [`AlgoGraph`] is ALREADY CSR — three flat arrays for the whole graph. This
/// used to convert it into one heap allocation PER VERTEX, which at the SF10
/// SNB corpus (V = 32,653,609, E = 180,619,897) is:
///
/// ```text
///   Vec<Vec<u32>>   784 MB of Vec headers
///                 + 522 MB of allocator metadata, across 32.6M allocations
///                 +1445 MB of neighbour data      = ~2751 MB
///   CSR             261 MB of offsets
///                 +1445 MB of neighbour data      = ~1706 MB, 2 allocations
/// ```
///
/// `triangles` holds TWO such structures at once, so LCC paid ~5.5 GB before
/// counting a single triangle. The arithmetic understates it: mimalloc rounds
/// small allocations to size classes, so a three-neighbour vertex occupied 16
/// or 32 bytes for 12 bytes of data.
///
/// Three kernels pay this — LCC, CDLP and Louvain. The pattern was already
/// known here: `betweenness` reuses its scratch across sources because "at `V`
/// sources this is the difference between `V` allocations and four". This was
/// the outlier.
///
/// It does NOT touch the triangle enumeration's cost. `triangles` became the
/// forward algorithm at rev56 and the directed (Graphalytics) LCC at rev61;
/// until then it merged each vertex's whole neighbour set against every
/// neighbour's row, O(sum d^2).
struct Undirected {
    /// Row starts, length `n + 1`.
    offsets: Vec<u32>,
    /// Neighbours, each row sorted and deduplicated.
    targets: Vec<u32>,
}

impl Undirected {
    fn row(&self, v: u32) -> &[u32] {
        let lo = self.offsets[v as usize] as usize;
        let hi = self.offsets[v as usize + 1] as usize;
        &self.targets[lo..hi]
    }
    fn degree(&self, v: u32) -> usize {
        self.row(v).len()
    }
}

/// The neighbour MULTISET: out- and in-neighbours chained, self-loops removed,
/// and NOT deduplicated.
///
/// LDBC Graphalytics' CDLP counts in- and out-neighbours separately, so a
/// reciprocal edge contributes its label TWICE. `undirected` dedups, which
/// changes which label wins a tie — and on LDBC's own 8-vertex validation
/// graph the effect is not marginal: the published answer is three communities
/// ([1,2,3], [4], [5,6,7,8]) and the deduplicated walk returns ONE of eight.
///
/// Sorted but not deduped, so `label_propagation`'s tally sees the repeats.
fn undirected_multiset(g: &AlgoGraph, rev: &AlgoGraph, exec: &dyn ScopedExec) -> Undirected {
    let (offsets, targets) = par_rows(g.len(), exec, &|v, row: &mut Vec<u32>| {
        row.extend(
            g.neighbours(v)
                .iter()
                .chain(rev.neighbours(v))
                .copied()
                .filter(|u| *u != v),
        );
        row.sort_unstable();
    });
    Undirected { offsets, targets }
}

fn undirected(g: &AlgoGraph, rev: &AlgoGraph, exec: &dyn ScopedExec) -> Undirected {
    let (offsets, targets) = par_rows(g.len(), exec, &|v, row: &mut Vec<u32>| {
        row.extend(
            g.neighbours(v)
                .iter()
                .chain(rev.neighbours(v))
                .copied()
                .filter(|u| *u != v),
        );
        row.sort_unstable();
        row.dedup();
    });
    Undirected { offsets, targets }
}

/// A CSR of rows `0..n`, each one written by `row` from its own index alone,
/// built across the executor (rev63). Each morsel of vertices fills buffers of
/// its own, and they are concatenated in morsel order, so the CSR is the
/// serial loop's byte for byte whatever the width.
///
/// The symmetrised adjacency was built by one thread: of datagen-7_5-fb's LCC
/// (4.31 s on 40 cores) it was 3.16 s, the orientation 0.38 s, and the
/// triangle enumeration, already split, 0.74 s. Every row reads the
/// projection and nothing else, so nothing but habit kept it serial.
fn par_rows<T: Copy + Send>(
    n: usize,
    exec: &dyn ScopedExec,
    row: &(dyn Fn(u32, &mut Vec<T>) + Sync),
) -> (Vec<u32>, Vec<T>) {
    let morsels = vertex_morsels(n, exec);
    type Part<T> = (Vec<u32>, Vec<T>);
    let slots: Vec<Mutex<Option<Part<T>>>> = morsels.iter().map(|_| Mutex::new(None)).collect();
    exec.for_each(morsels.len(), &|m| {
        let mut lens: Vec<u32> = Vec::with_capacity(morsels[m].len());
        let mut out: Vec<T> = Vec::new();
        let mut scratch: Vec<T> = Vec::new();
        for v in morsels[m].clone() {
            scratch.clear();
            row(v as u32, &mut scratch);
            lens.push(u32::try_from(scratch.len()).unwrap_or(u32::MAX));
            out.extend_from_slice(&scratch);
        }
        *slots[m].lock().unwrap_or_else(|e| e.into_inner()) = Some((lens, out));
    });
    let parts: Vec<Part<T>> = slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every morsel ran — `for_each` returns only when all have")
        })
        .collect();
    let total: usize = parts.iter().map(|(_, t)| t.len()).sum();
    let mut offsets: Vec<u32> = Vec::with_capacity(n + 1);
    offsets.push(0);
    let mut entries: Vec<T> = Vec::with_capacity(total);
    let mut at = 0u32;
    for (lens, part) in parts {
        for len in lens {
            at += len;
            offsets.push(at);
        }
        entries.extend_from_slice(&part);
    }
    (offsets, entries)
}

/// Triangle count per vertex, and the local clustering coefficient.
///
/// # The ordering trick, and what it costs
///
/// Each undirected edge is oriented from the LOWER-degree endpoint to the
/// higher, ties broken by the lower dense offset. That bounds any vertex's
/// oriented out-degree by `O(√E)` — which is the whole point: a hub with a
/// million neighbours contributes `O(√E)` work rather than `O(deg²)`.
///
/// The tie-break is what makes it deterministic, and the sortedness of each
/// oriented list is ASSERTED rather than assumed, because the membership test
/// below is a binary search.
/// LCC under LDBC Graphalytics' DIRECTED definition.
///
/// The spec symmetrises the neighbour SET and keeps DIRECTION in the edge test:
/// for each `v`, `N(v)` is its in- and out-neighbours, and the numerator counts
/// ORDERED pairs `(u, w)` drawn from `N(v)` with a directed edge `u -> w`. The
/// denominator is `d(d - 1)`.
///
/// `triangles` symmetrises BOTH sides — it tests membership in the same
/// undirected set it enumerates from — so it answers a different question on a
/// directed graph. Verified against LDBC's `lcc/dir-output` before this was
/// written: the rule above reproduces all ten published coefficients, and
/// vertex 1 is the readable case (N = {3,5,8}, four of the six ordered pairs
/// carry an edge, 4/6 = 0.667).
///
/// Returned as `(triangle-ish counts, coefficients)` to match `triangles`'
/// shape; the first element is the numerator, which is an ordered-pair count
/// rather than a triangle count and is not comparable to the other kernel's.
///
/// # Counted per triangle, not per neighbour pair (rev61)
///
/// A pair `(u, w)` counts toward `v` only if `u` and `w` are both neighbours of
/// `v` AND an edge joins them, so `{v, u, w}` is a triangle of the symmetrised
/// graph. `v`'s numerator is therefore a sum over its triangles: for each one,
/// the number of directed edges between the other two corners, `[u -> w] +
/// [w -> u]`. So each triangle is found ONCE, by the forward algorithm over the
/// degree-oriented CSR that `triangles` uses, and each corner is credited the
/// edges between the other two. Each adjacency entry carries a two-bit record
/// of which directions the edge exists in (`undirected_with_directions`), so a
/// credit is a popcount rather than a search.
///
/// The first version merged `N(v)` against the out-row of every neighbour
/// `u`: `O(d(v)²)` per vertex, `O(Σ d²)` in all. On dota-league (61,170
/// vertices, 50,870,313 edges, mean degree ~1,663) one executor thread was
/// still merging a morsel of hubs after 50 minutes, every other thread idle,
/// and the warm-up passed its four ceilings. The forward algorithm bounds any
/// vertex's oriented degree by `O(√E)`.
///
/// Self-loops never count (`undirected` drops them from `N(v)`, and a triangle
/// has three distinct corners), and a multi-edge is one edge in either
/// direction, exactly as before. `the_directed_lcc_matches_its_definition`
/// checks the numerators against a count that shares no code with this.
pub(crate) fn triangles_directed(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    exec: &dyn ScopedExec,
) -> (Vec<u64>, Vec<f64>) {
    let n = g.len();
    // `ENGRAM_ALGO_TIMING=1` reports the three phases, each split across the
    // executor (the first two since rev63).
    let timing = super::run::phase_timing_on();
    let t = super::run::phase_clock(timing);
    let (off, ent) = undirected_with_directions(g, rev, exec);
    let deg: Vec<usize> = (0..n).map(|v| (off[v + 1] - off[v]) as usize).collect();
    if let Some(t) = t {
        eprintln!("[algo-timing] lcc symmetrise {:?} ({} entries)", t.elapsed(), ent.len());
    }
    let t = super::run::phase_clock(timing);

    // The oriented CSR, as `triangles` builds it, each entry carrying its
    // edge's direction bits.
    let before = |a: u32, b: u32| (deg[a as usize], a) < (deg[b as usize], b);
    let (ooff, oent) = par_rows(n, exec, &|v, row: &mut Vec<(u32, u8)>| {
        row.extend(
            ent[off[v as usize] as usize..off[v as usize + 1] as usize]
                .iter()
                .filter(|&&(u, _)| before(v, u)),
        );
    });
    let otgt: Vec<u32> = oent.iter().map(|&(u, _)| u).collect();
    let odir: Vec<u8> = oent.iter().map(|&(_, d)| d).collect();
    drop(oent);
    let span = |v: u32| ooff[v as usize] as usize..ooff[v as usize + 1] as usize;
    if let Some(t) = t {
        eprintln!("[algo-timing] lcc orient {:?} ({} oriented)", t.elapsed(), otgt.len());
    }
    let t = super::run::phase_clock(timing);

    // Triangle x < y < z in the order found once, at v = x and a = y, as the
    // common z of out(x) and out(y). x is credited the edges between y and z,
    // y those between x and z, z those between x and y. Integer counts added
    // atomically, so the answer is the serial loop's whatever the width.
    //
    // z's credit is gathered per vertex before it is added (rev63). Every z
    // found at v lies in v's own oriented row, so the credits sum into a
    // scratch array aligned with that row and go to `num` once per (v, z)
    // pair, not once per triangle. z is the highest-degree corner, so on a
    // dense graph the per-triangle adds all landed on hubs: dota-league's
    // enumeration took 28 s at width 40, about 11x its serial time.
    let num: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
    let morsels = vertex_morsels(n, exec);
    exec.for_each(morsels.len(), &|m| {
        let mut cred: Vec<u64> = Vec::new();
        for v in morsels[m].clone() {
            let (ov, dv) = (&otgt[span(v as u32)], &odir[span(v as u32)]);
            cred.clear();
            cred.resize(ov.len(), 0);
            let mut own = 0u64;
            for (p, &a) in ov.iter().enumerate() {
                let (oa, da) = (&otgt[span(a)], &odir[span(a)]);
                let v_a = u64::from(dv[p].count_ones());
                let (mut i, mut j, mut to_a) = (0usize, 0usize, 0u64);
                while i < ov.len() && j < oa.len() {
                    match ov[i].cmp(&oa[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            own += u64::from(da[j].count_ones());
                            to_a += u64::from(dv[i].count_ones());
                            cred[i] += v_a;
                            i += 1;
                            j += 1;
                        }
                    }
                }
                if to_a > 0 {
                    num[a as usize].fetch_add(to_a, Ordering::Relaxed);
                }
            }
            for (&z, &c) in ov.iter().zip(&cred) {
                if c > 0 {
                    num[z as usize].fetch_add(c, Ordering::Relaxed);
                }
            }
            if own > 0 {
                num[v].fetch_add(own, Ordering::Relaxed);
            }
        }
    });
    let num: Vec<u64> = num.into_iter().map(AtomicU64::into_inner).collect();
    if let Some(t) = t {
        eprintln!(
            "[algo-timing] lcc enumerate {:?} ({} morsels, width {})",
            t.elapsed(),
            morsels.len(),
            exec.width()
        );
    }
    let lcc = (0..n)
        .map(|v| {
            let d = deg[v] as f64;
            if d < 2.0 {
                0.0
            } else {
                num[v] as f64 / (d * (d - 1.0))
            }
        })
        .collect();
    counted!("algo.lcc directed runs");
    (num, lcc)
}

/// [`undirected`]'s rows, each entry carrying which directions its edge exists
/// in: bit 0 when `v -> u`, bit 1 when `u -> v` (so a reciprocal pair holds 3).
/// Rows sorted and deduplicated, self-loops dropped, exactly as `undirected`'s
/// — the same offsets and neighbours, plus one byte an entry. Returned as the
/// offsets and the `(neighbour, bits)` entries.
fn undirected_with_directions(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    exec: &dyn ScopedExec,
) -> (Vec<u32>, Vec<(u32, u8)>) {
    par_rows(g.len(), exec, &|v, row: &mut Vec<(u32, u8)>| {
        row.extend(
            g.neighbours(v)
                .iter()
                .map(|&u| (u, 1u8))
                .chain(rev.neighbours(v).iter().map(|&u| (u, 2u8)))
                .filter(|&(u, _)| u != v),
        );
        row.sort_unstable();
        // fold the repeats of one neighbour into one entry, OR-ing its bits
        let mut w = 0usize;
        for r in 0..row.len() {
            if w > 0 && row[w - 1].0 == row[r].0 {
                row[w - 1].1 |= row[r].1;
            } else {
                row[w] = row[r];
                w += 1;
            }
        }
        row.truncate(w);
    })
}

/// `0..n` in morsels for `exec`: sixteen a worker, so a hub's vertex does not
/// hold the rest of a run up, and never fewer than 64 vertices to a morsel.
fn vertex_morsels(n: usize, exec: &dyn ScopedExec) -> Vec<std::ops::Range<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let per = n.div_ceil(exec.width().max(1) * 16).max(64);
    (0..n).step_by(per).map(|lo| lo..(lo + per).min(n)).collect()
}

/// The per-morsel results, in morsel order.
fn concat_slots<T>(slots: Vec<Mutex<Option<Vec<T>>>>, n: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(n);
    for slot in slots {
        out.extend(
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every morsel ran — `for_each` returns only when all have"),
        );
    }
    out
}

pub(crate) fn triangles(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    exec: &dyn ScopedExec,
) -> (Vec<u64>, Vec<f64>) {
    let n = g.len();
    let adj = undirected(g, rev, exec);
    let deg: Vec<usize> = (0..n as u32).map(|v| adj.degree(v)).collect();

    // The oriented CSR: each undirected edge kept once, from the lower
    // (degree, id) end. `adj`'s rows are ascending and deduplicated, and a
    // filter keeps both, so no row needs sorting. Built across the executor
    // (rev63), row by row as the symmetrised adjacency is.
    let before = |a: u32, b: u32| (deg[a as usize], a) < (deg[b as usize], b);
    let (ooff, otgt) = par_rows(n, exec, &|v, row: &mut Vec<u32>| {
        row.extend(adj.row(v).iter().filter(|&&u| before(v, u)));
    });
    let out = |v: u32| &otgt[ooff[v as usize] as usize..ooff[v as usize + 1] as usize];

    // The forward algorithm: a triangle x < y < z (in that order) is found
    // exactly once, at v = x and a = y, as the common z of out(x) and out(y) --
    // a merge of two sorted rows. The vertices are split across the executor;
    // the counts are integers added atomically, so they are the serial loop's
    // exactly whatever the width or the order. v's and a's shares are summed
    // locally first and added once; z's are gathered in a scratch array
    // aligned with v's row and added once per (v, z) pair (rev63, as in
    // `triangles_directed`), not once per triangle on the hubs.
    let tri: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
    let morsels = vertex_morsels(n, exec);
    exec.for_each(morsels.len(), &|m| {
        let mut cred: Vec<u64> = Vec::new();
        for v in morsels[m].clone() {
            let ov = out(v as u32);
            cred.clear();
            cred.resize(ov.len(), 0);
            let mut own = 0u64;
            for &a in ov {
                let oa = out(a);
                let (mut i, mut j, mut with_a) = (0usize, 0usize, 0u64);
                while i < ov.len() && j < oa.len() {
                    match ov[i].cmp(&oa[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            cred[i] += 1;
                            with_a += 1;
                            i += 1;
                            j += 1;
                        }
                    }
                }
                if with_a > 0 {
                    tri[a as usize].fetch_add(with_a, Ordering::Relaxed);
                    own += with_a;
                }
            }
            for (&z, &c) in ov.iter().zip(&cred) {
                if c > 0 {
                    tri[z as usize].fetch_add(c, Ordering::Relaxed);
                }
            }
            if own > 0 {
                tri[v].fetch_add(own, Ordering::Relaxed);
            }
        }
    });
    let tri: Vec<u64> = tri.into_iter().map(AtomicU64::into_inner).collect();

    let lcc = (0..n)
        .map(|v| {
            let d = deg[v] as f64;
            // A vertex with fewer than two neighbours has no possible triangle,
            // so its coefficient is defined as zero rather than left as a NaN.
            if d < 2.0 {
                0.0
            } else {
                2.0 * tri[v] as f64 / (d * (d - 1.0))
            }
        })
        .collect();
    counted!("algo.triangle runs");
    (tri, lcc)
}

// ─── Label propagation ─────────────────────────────────────────────────────

/// Label propagation.
///
/// # Synchronous, and what that trades
///
/// Every vertex reads the PREVIOUS iteration's labels, so there is no
/// visit-order dependence at all. Asynchronous label propagation converges
/// faster and to better communities, and is inherently order-dependent — this
/// trades quality for reproducibility, and that is the sentence rather than an
/// omission.
///
/// Ties are broken by the SMALLEST label, which is total because labels are
/// node ids. Synchronous propagation can oscillate between two states on a
/// bipartite-ish graph; that is DETECTED and reported rather than hidden.
pub(crate) fn label_propagation(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    max_iterations: u32,
    graphalytics: bool,
    exec: &dyn ScopedExec,
) -> (Vec<u64>, FixpointReport, bool) {
    let n = g.len();
    // Under Graphalytics semantics a reciprocal edge votes TWICE, and the run
    // takes exactly `max_iterations` rounds -- the spec has no convergence
    // test and no two-cycle escape. See `AlgoConfig::graphalytics`.
    let adj = if graphalytics {
        undirected_multiset(g, rev, exec)
    } else {
        undirected(g, rev, exec)
    };
    let mut labels: Vec<u64> = (0..n as u32).map(|v| g.id_of(v)).collect();
    let mut history: Vec<u64> = Vec::new();
    let mut oscillated = false;
    let mut iterations = 0u32;
    let mut converged = false;

    // A round is a pure function of the PREVIOUS round's labels, vertex by
    // vertex, so the vertices are split across the executor and the new labels
    // concatenated in order: the serial answer at every width.
    let morsels = vertex_morsels(n, exec);
    while iterations < max_iterations.min(super::MAX_ITERATIONS) {
        iterations += 1;
        let slots: Vec<Mutex<Option<Vec<u64>>>> = morsels.iter().map(|_| Mutex::new(None)).collect();
        let current = &labels;
        exec.for_each(morsels.len(), &|m| {
            let mut part = Vec::with_capacity(morsels[m].len());
            let mut seen: Vec<u64> = Vec::new();
            for v in morsels[m].clone() {
                let row = adj.row(v as u32);
                if row.is_empty() {
                    part.push(current[v]);
                    continue;
                }
                // The neighbours' labels, sorted: each run is one label's
                // count. Strictly greater, runs ascending, so the SMALLEST
                // label wins a tie -- as the BTreeMap tally this replaces did.
                seen.clear();
                seen.extend(row.iter().map(|&u| current[u as usize]));
                seen.sort_unstable();
                let (mut best, mut best_n, mut i) = (current[v], 0usize, 0usize);
                while i < seen.len() {
                    let label = seen[i];
                    let mut j = i + 1;
                    while j < seen.len() && seen[j] == label {
                        j += 1;
                    }
                    if j - i > best_n {
                        best_n = j - i;
                        best = label;
                    }
                    i = j;
                }
                part.push(best);
            }
            *slots[m].lock().unwrap_or_else(|e| e.into_inner()) = Some(part);
        });
        let next = concat_slots(slots, n);
        let changed = next != labels;
        labels = next;
        // The digest serves the two-cycle escape alone, and Graphalytics
        // semantics have none, so under them it is not taken (rev64). It was
        // computed every round regardless, serially: on the zf graphs of the
        // Graphalytics S set (13-16M vertices) that was the round's largest
        // single-threaded cost.
        if !graphalytics {
            let digest = digest_of(&labels);
            if history.len() >= 2 && history[history.len() - 2] == digest {
                // The state two iterations ago has returned: a two-cycle.
                oscillated = true;
                sometimes!("algo.label propagation oscillated", true);
                counted!("algo.lpa oscillated");
                break;
            }
            history.push(digest);
        }
        if !changed {
            converged = true;
            break;
        }
    }
    counted!("algo.lpa runs");
    (
        labels,
        FixpointReport {
            iterations,
            converged,
            delta: 0.0,
        },
        oscillated,
    )
}

fn digest_of(labels: &[u64]) -> u64 {
    // The same bytes in the same order as one `update` per label, fed a 64 KiB
    // run at a time: the hash is unchanged, and the per-call cost is paid 8,192
    // labels at a time rather than once per label.
    let mut h = blake3::Hasher::new();
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    for chunk in labels.chunks(8 * 1024) {
        buf.clear();
        for l in chunk {
            buf.extend_from_slice(&l.to_le_bytes());
        }
        h.update(&buf);
    }
    let out = h.finalize();
    u64::from_le_bytes(out.as_bytes()[..8].try_into().unwrap_or([0; 8]))
}

// ─── Louvain ───────────────────────────────────────────────────────────────

/// Louvain modularity-based community detection.
///
/// # STRICTLY SEQUENTIAL, AND THERE WILL NEVER BE A PARALLEL VARIANT
///
/// The local-moving phase is Gauss-Seidel *by definition*: the modularity gain
/// of moving `v` depends on the communities its neighbours are in **right
/// now**, including moves made earlier in this same sweep. So the sweep order
/// is part of the specification rather than an implementation detail, and it
/// is ascending node id.
///
/// A parallel variant added later would not be an optimisation — it would
/// silently change every result already published. That is why this says so
/// here rather than leaving the next reader to discover it by trying.
pub(crate) fn louvain(
    g: &AlgoGraph,
    rev: &AlgoGraph,
    max_iterations: u32,
    resolution: f64,
) -> (Vec<u64>, f64, u32) {
    let n = g.len();
    // Louvain has no executor of its own, so its adjacency is built serially
    // as it always was.
    let adj = undirected(g, rev, &crate::scoped_exec::SerialExec);
    let k: Vec<f64> = (0..n as u32).map(|v| adj.degree(v) as f64).collect();
    let two_m: f64 = k.iter().sum();
    if two_m == 0.0 {
        // No edges: every vertex is its own community and modularity is zero.
        return ((0..n as u32).map(|v| g.id_of(v)).collect(), 0.0, 0);
    }

    // Community of each vertex, as a dense offset; and each community's total
    // degree.
    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut tot: Vec<f64> = k.clone();
    let mut passes = 0u32;

    for _ in 0..max_iterations.min(super::MAX_ITERATIONS) {
        passes += 1;
        let mut moved = false;
        // ASCENDING VERTEX ORDER. This is the specification, not a choice.
        for v in 0..n as u32 {
            let cv = comm[v as usize];
            let kv = k[v as usize];
            // Weight from `v` into each neighbouring community.
            let mut into: BTreeMap<u32, f64> = BTreeMap::new();
            for &u in adj.row(v) {
                *into.entry(comm[u as usize]).or_insert(0.0) += 1.0;
            }
            // Remove `v` from its community before scoring, or it competes
            // with itself.
            tot[cv as usize] -= kv;
            let base =
                into.get(&cv).copied().unwrap_or(0.0) - resolution * tot[cv as usize] * kv / two_m;
            let mut best = cv;
            let mut best_gain = base;
            for (c, w) in &into {
                if *c == cv {
                    continue;
                }
                let gain = *w - resolution * tot[*c as usize] * kv / two_m;
                // Strictly greater, and the map ascends, so the LOWEST
                // community wins a tie — total, and reproducible.
                if gain > best_gain {
                    best_gain = gain;
                    best = *c;
                }
            }
            tot[best as usize] += kv;
            if best != cv {
                comm[v as usize] = best;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    // Modularity of the final partition, folded serially in ascending order.
    let mut inside: BTreeMap<u32, f64> = BTreeMap::new();
    let mut total: BTreeMap<u32, f64> = BTreeMap::new();
    for v in 0..n as u32 {
        *total.entry(comm[v as usize]).or_insert(0.0) += k[v as usize];
        for &u in adj.row(v) {
            if comm[u as usize] == comm[v as usize] {
                *inside.entry(comm[v as usize]).or_insert(0.0) += 1.0;
            }
        }
    }
    let mut q = 0.0;
    for (c, t) in &total {
        let e = inside.get(c).copied().unwrap_or(0.0);
        q += e / two_m - resolution * (t / two_m) * (t / two_m);
    }

    // Canonicalise: a community is named by its smallest node id, so the
    // labels do not depend on which vertex happened to seed it.
    let mut min_id: BTreeMap<u32, u64> = BTreeMap::new();
    for v in 0..n as u32 {
        let id = g.id_of(v);
        min_id
            .entry(comm[v as usize])
            .and_modify(|m| *m = (*m).min(id))
            .or_insert(id);
    }
    counted!("algo.louvain runs");
    (
        (0..n as u32).map(|v| min_id[&comm[v as usize]]).collect(),
        q,
        passes,
    )
}

/// How many distinct communities a labelling holds.
pub(crate) fn community_count(labels: &[u64]) -> usize {
    labels.iter().copied().collect::<BTreeSet<u64>>().len()
}

// ─── Betweenness centrality ────────────────────────────────────────────────

/// Betweenness centrality, by Brandes' algorithm.
///
/// # What it computes
///
/// For each vertex `v`, the sum over all ordered pairs `(s, t)` of the
/// fraction of shortest `s`-`t` paths that pass through `v`. Brandes computes
/// it in `O(V x E)` rather than the `O(V^3)` of the definition, by running one
/// shortest-path search per source and accumulating dependencies backwards
/// along the search's own DAG.
///
/// # Why this is SERIAL, and permanently so
///
/// Brandes parallelises naturally over SOURCES — every source's search is
/// independent — and that is exactly what makes it unusable here. Each
/// vertex's score is a sum over every source's contribution, so a parallel run
/// adds those contributions in whatever order the threads finish, and floating
/// point addition is not associative. The scores would depend on the executor
/// width, which is the one thing this layer promises they never do.
///
/// Summing per-source partial arrays in a fixed order afterwards would restore
/// determinism, and is rejected on memory: a partial array is `O(V)` f64 per
/// source in flight, so eight workers over a million-vertex projection is
/// 64 MB of partials for a factor the measurements say is under 1.1x anyway
/// (see `Graph::algo_min_vertices`). The serial loop is the honest choice, and
/// the module head of `fixpoint.rs` explains why that layer's parallelism is
/// worth so little here in the first place.
///
/// # Determinism
///
/// Sources run in ascending dense order. Each source's BFS visits its frontier
/// in ascending offset order, so the traversal stack is a pure function of the
/// CSR — which is itself a pure function of the ascending member id set. The
/// dependency accumulation then pops that stack in reverse, so every addition
/// into every accumulator happens in the same order on every run.
pub(crate) fn betweenness(g: &AlgoGraph, undirected: bool) -> Vec<f64> {
    let n = g.len();
    let mut score = vec![0.0f64; n];
    if n == 0 {
        counted!("algo.betweenness runs");
        return score;
    }

    // Scratch reused across sources rather than reallocated per source: at
    // `V` sources this is the difference between `V` allocations and four.
    let mut sigma = vec![0.0f64; n];
    let mut dist = vec![-1i64; n];
    let mut delta = vec![0.0f64; n];
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut stack: Vec<u32> = Vec::with_capacity(n);
    let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();

    for s in 0..n as u32 {
        for v in 0..n {
            sigma[v] = 0.0;
            dist[v] = -1;
            delta[v] = 0.0;
            preds[v].clear();
        }
        stack.clear();
        queue.clear();

        sigma[s as usize] = 1.0;
        dist[s as usize] = 0;
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            // An UNDIRECTED projection must see each edge from both ends, and
            // the reverse adjacency is where the other end lives. Chaining
            // rather than symmetrising the CSR keeps one representation and
            // one determinism argument; the order is fixed either way.
            // NO REVERSE ADJACENCY HERE, and that is the correction that
            // matters: an UNDIRECTED projection is `Dir::Both`, and the store
            // already yields both directions for it, so the CSR is symmetric
            // before this kernel sees it. Chaining the reverse would visit
            // every neighbour twice, add `sigma` twice and push the same
            // predecessor twice — wrong shortest-path COUNTS, so wrong
            // scores, in a way that looks like a plausible number rather than
            // an error. `undirected` therefore controls only the halving
            // below.
            //
            // Consecutive duplicates ARE skipped: a pair joined in both
            // directions appears twice in a `Dir::Both` row, and parallel
            // edges appear once each. Betweenness is defined over a simple
            // graph — GDS and networkx both treat parallel edges as one — and
            // counting them separately would multiply `sigma` by the edge
            // multiplicity. The rows are sorted, so duplicates are adjacent
            // and one comparison finds them.
            let mut last: Option<u32> = None;
            for &w in g.neighbours(v) {
                if last == Some(w) {
                    continue;
                }
                last = Some(w);
                if dist[w as usize] < 0 {
                    dist[w as usize] = dist[v as usize] + 1;
                    queue.push_back(w);
                }
                if dist[w as usize] == dist[v as usize] + 1 {
                    sigma[w as usize] += sigma[v as usize];
                    preds[w as usize].push(v);
                }
            }
        }

        // Accumulate backwards. The stack is in non-decreasing distance order,
        // so popping it visits every vertex only after all of its successors.
        while let Some(w) = stack.pop() {
            let coeff = (1.0 + delta[w as usize]) / sigma[w as usize];
            for &v in &preds[w as usize] {
                delta[v as usize] += sigma[v as usize] * coeff;
            }
            if w != s {
                score[w as usize] += delta[w as usize];
            }
        }
    }

    if undirected {
        // Each unordered pair is counted from both ends on an undirected
        // graph, so the conventional score is halved. GDS does the same, and
        // a caller comparing against it would otherwise see every value
        // doubled.
        for v in score.iter_mut() {
            *v /= 2.0;
        }
    }

    counted!("algo.betweenness runs");
    score
}

// ─── K shortest paths (Yen) ────────────────────────────────────────────────

/// One route: its total cost and the dense offsets it visits, in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KPath {
    /// The route's total cost — hop count when the projection is unweighted.
    pub cost: f64,
    /// The dense offsets visited, source first and target last.
    pub nodes: Vec<u32>,
}

/// Dijkstra from `src` to `dst`, with some nodes and some edges withdrawn.
///
/// The withdrawals are what makes this Yen's inner loop rather than plain
/// SSSP: each spur search runs on the graph minus the root path's nodes and
/// minus the edges already used by a shorter route sharing that root, which is
/// how the same shortest path is not found twice.
///
/// Ties are broken by the LOWER dense offset, so the settle order is total and
/// the route returned is a pure function of the projection.
fn dijkstra_avoiding(
    g: &AlgoGraph,
    src: u32,
    dst: u32,
    banned_nodes: &BTreeSet<u32>,
    banned_edges: &BTreeSet<(u32, u32)>,
) -> Option<KPath> {
    let n = g.len();
    if src as usize >= n || dst as usize >= n || banned_nodes.contains(&src) {
        return None;
    }
    let mut dist = vec![f64::INFINITY; n];
    let mut prev = vec![u32::MAX; n];
    let mut done = vec![false; n];
    dist[src as usize] = 0.0;

    // A sorted set as the priority queue rather than a binary heap: the key
    // carries the offset, so equal distances settle in ascending offset and
    // the order is total. A heap would need the same tie-break anyway, and at
    // the sizes an interactive `k`-shortest-paths call is priced for, the
    // log-factor difference is not what decides this.
    let mut queue: BTreeSet<(u64, u32)> = BTreeSet::new();
    queue.insert((0, src));
    while let Some(&(key, v)) = queue.iter().next() {
        queue.remove(&(key, v));
        if done[v as usize] {
            continue;
        }
        done[v as usize] = true;
        if v == dst {
            break;
        }
        let weights = g.weights_of(v);
        for (i, &w) in g.neighbours(v).iter().enumerate() {
            if banned_nodes.contains(&w) || banned_edges.contains(&(v, w)) {
                continue;
            }
            let step = weights.map_or(1.0, |ws| ws[i]);
            if step < 0.0 {
                // The same refusal SSSP makes, for the same reason: Dijkstra's
                // settle-once invariant assumes non-negative steps, and a
                // negative one silently returns a non-shortest path.
                counted!("algo.kshortest refused a negative weight");
                return None;
            }
            let alt = dist[v as usize] + step;
            if alt < dist[w as usize] {
                dist[w as usize] = alt;
                prev[w as usize] = v;
                queue.insert((alt.to_bits(), w));
            }
        }
    }

    if !dist[dst as usize].is_finite() {
        return None;
    }
    let mut nodes = vec![dst];
    let mut at = dst;
    while at != src {
        at = prev[at as usize];
        if at == u32::MAX {
            return None;
        }
        nodes.push(at);
    }
    nodes.reverse();
    Some(KPath {
        cost: dist[dst as usize],
        nodes,
    })
}

/// The `k` shortest loopless routes from `src` to `dst`, by Yen's algorithm.
///
/// # Why loopless, and why that costs a whole algorithm
///
/// The `k` shortest WALKS are easy — repeated relaxation finds them — and are
/// almost never what a caller wants, because walks may revisit a node and the
/// second-best walk is usually the best one with a detour taken twice. Yen's
/// returns simple paths, and pays for it: `k` rounds, each running a Dijkstra
/// per node of the previous route, so `O(k x V x (E + V log V))`.
///
/// That cost is why this is priced against the all-pairs work ceiling like
/// betweenness rather than the ordinary node and edge ones.
///
/// # Determinism
///
/// Candidates are ordered by `(cost, node sequence)` — the sequence breaking
/// cost ties, so the order is TOTAL and two routes of equal cost always come
/// back in the same order. Ordering by cost alone would leave equal-cost
/// candidates in whatever order the spur loop produced them, which is stable
/// today and would stop being so the first time the loop was reordered.
pub(crate) fn yen_k_shortest(g: &AlgoGraph, src: u32, dst: u32, k: usize) -> Vec<KPath> {
    counted!("algo.kshortest runs");
    let mut accepted: Vec<KPath> = Vec::new();
    let Some(first) = dijkstra_avoiding(g, src, dst, &BTreeSet::new(), &BTreeSet::new()) else {
        return accepted;
    };
    accepted.push(first);
    if k <= 1 {
        return accepted;
    }

    // Candidates keyed by `(cost bits, nodes)` so the set is both ordered and
    // deduplicated: the same spur route is reachable from more than one root,
    // and admitting it twice would return a duplicate as if it were the next
    // distinct route.
    let mut candidates: BTreeSet<(u64, Vec<u32>)> = BTreeSet::new();

    while accepted.len() < k {
        let prev = accepted.last().expect("non-empty").clone();
        for i in 0..prev.nodes.len().saturating_sub(1) {
            let spur = prev.nodes[i];
            let root = &prev.nodes[..=i];

            let mut banned_edges: BTreeSet<(u32, u32)> = BTreeSet::new();
            for p in &accepted {
                if p.nodes.len() > i && p.nodes[..=i] == *root {
                    banned_edges.insert((p.nodes[i], p.nodes[i + 1]));
                }
            }
            // Every root node EXCEPT the spur is withdrawn, which is what
            // keeps the result loopless: a spur route that re-entered the root
            // would repeat a node.
            let banned_nodes: BTreeSet<u32> = root[..i].iter().copied().collect();

            let Some(spur_path) = dijkstra_avoiding(g, spur, dst, &banned_nodes, &banned_edges)
            else {
                continue;
            };
            let mut nodes = root[..i].to_vec();
            nodes.extend(spur_path.nodes.iter().copied());
            let root_cost = path_cost(g, root);
            let total = root_cost + spur_path.cost;
            if accepted.iter().any(|p| p.nodes == nodes) {
                continue;
            }
            candidates.insert((total.to_bits(), nodes));
        }
        let Some((bits, nodes)) = candidates.iter().next().cloned() else {
            // No more distinct routes exist. Returning fewer than `k` is the
            // answer, not a failure: a graph may simply not have `k` of them.
            sometimes!("algo.kshortest exhausted before k", true);
            break;
        };
        candidates.remove(&(bits, nodes.clone()));
        accepted.push(KPath {
            cost: f64::from_bits(bits),
            nodes,
        });
    }
    accepted
}

/// The cost of walking `nodes` in order, for the root prefix Yen re-uses.
fn path_cost(g: &AlgoGraph, nodes: &[u32]) -> f64 {
    let mut total = 0.0;
    for pair in nodes.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let weights = g.weights_of(a);
        // The CHEAPEST parallel edge, which is the one a shortest-path search
        // would have taken. Taking the first would price the root differently
        // from the way it was found.
        let mut best = f64::INFINITY;
        for (i, &w) in g.neighbours(a).iter().enumerate() {
            if w == b {
                best = best.min(weights.map_or(1.0, |ws| ws[i]));
            }
        }
        if best.is_finite() {
            total += best;
        }
    }
    total
}

// ─── Strongly connected components ─────────────────────────────────────────

/// Strongly connected components, by Kosaraju's two-pass algorithm.
///
/// # Why this exists beside WCC rather than instead of it
///
/// They answer different questions and only one of them is about direction.
/// WCC asks which nodes are joined ignoring arrow direction; SCC asks which
/// nodes can each REACH each other following the arrows. On a directed graph
/// WCC will happily report one component where no node can reach any other,
/// which is a true answer to a question the caller probably did not ask.
///
/// # Why Kosaraju and not Tarjan
///
/// Tarjan is one pass rather than two and is the usual choice. It is also
/// naturally recursive, and a recursive descent over a projection the node
/// ceiling permits — twenty million — overflows the stack long before it
/// finishes. An iterative Tarjan is possible and is genuinely fiddly: the
/// low-link update on return from a child has to be replayed by hand, and
/// getting it subtly wrong produces components that are merely plausible.
/// Kosaraju's two passes are each a plain iterative DFS, the reverse graph is
/// already built for the pull kernels, and the cost is one extra traversal of
/// a structure that is already resident.
///
/// # Determinism
///
/// Both passes start their roots in ascending dense order and walk each
/// vertex's CSR row in its own order, so the finish-order stack and therefore
/// the component assignment are pure functions of the projection. Components
/// are canonicalised to their MINIMUM node id, exactly as WCC does, so the
/// labels are stable across runs and comparable between the two.
pub(crate) fn scc(g: &AlgoGraph, rev: &AlgoGraph) -> Vec<u64> {
    let n = g.len();
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let mut seen = vec![false; n];

    // PASS ONE: push vertices in finish order. Iterative, with an explicit
    // (vertex, next-neighbour-index) stack — the index is what lets a vertex
    // be finished only after every one of its children, which is the whole
    // content of "finish order".
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for root in 0..n as u32 {
        if seen[root as usize] {
            continue;
        }
        seen[root as usize] = true;
        stack.push((root, 0));
        while let Some((v, i)) = stack.pop() {
            let row = g.neighbours(v);
            if i < row.len() {
                stack.push((v, i + 1));
                let w = row[i];
                if !seen[w as usize] {
                    seen[w as usize] = true;
                    stack.push((w, 0));
                }
            } else {
                order.push(v);
            }
        }
    }

    // PASS TWO: pop the finish order and walk the REVERSE graph. Each tree is
    // one strongly connected component.
    let mut comp = vec![u32::MAX; n];
    let mut root_of: Vec<u32> = Vec::new();
    for &start in order.iter().rev() {
        if comp[start as usize] != u32::MAX {
            continue;
        }
        let id = root_of.len() as u32;
        root_of.push(start);
        comp[start as usize] = id;
        let mut work = vec![start];
        while let Some(v) = work.pop() {
            for &w in rev.neighbours(v) {
                if comp[w as usize] == u32::MAX {
                    comp[w as usize] = id;
                    work.push(w);
                }
            }
        }
    }

    // Canonicalise to the component's smallest node id, as WCC does — so the
    // two are comparable, and so a label means the same thing on every run.
    let mut min_id: BTreeMap<u32, u64> = BTreeMap::new();
    for v in 0..n as u32 {
        let c = comp[v as usize];
        let id = g.id_of(v);
        min_id
            .entry(c)
            .and_modify(|m| *m = (*m).min(id))
            .or_insert(id);
    }
    counted!("algo.scc runs");
    (0..n as u32).map(|v| min_id[&comp[v as usize]]).collect()
}

// ─── Closeness centrality ──────────────────────────────────────────────────

/// Closeness centrality, in Wasserman and Faust's form.
///
/// The plain definition — the reciprocal of the summed distance to every other
/// node — is undefined on a disconnected graph, where some distance is
/// infinite. The usual repair, and GDS's, is to scale by the fraction of the
/// graph a node can actually reach:
///
/// ```text
///   C(v) = (reachable(v) - 1) / sum_of_distances(v)
///        x (reachable(v) - 1) / (n - 1)
/// ```
///
/// so a node that reaches a small tight cluster does not outrank one that
/// reaches the whole graph slightly less tightly. A node that reaches nothing
/// scores zero rather than dividing by zero.
///
/// All-pairs like betweenness, and priced against the same ceiling. Serial for
/// the same reason: the answer is a sum over sources.
pub(crate) fn closeness(g: &AlgoGraph) -> Vec<f64> {
    let n = g.len();
    let mut out = vec![0.0f64; n];
    if n <= 1 {
        counted!("algo.closeness runs");
        return out;
    }
    let mut dist = vec![-1i64; n];
    let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    for s in 0..n as u32 {
        for d in dist.iter_mut() {
            *d = -1;
        }
        queue.clear();
        dist[s as usize] = 0;
        queue.push_back(s);
        let mut total = 0i64;
        let mut reached = 0i64;
        while let Some(v) = queue.pop_front() {
            let mut last: Option<u32> = None;
            for &w in g.neighbours(v) {
                if last == Some(w) {
                    continue;
                }
                last = Some(w);
                if dist[w as usize] < 0 {
                    dist[w as usize] = dist[v as usize] + 1;
                    total += dist[w as usize];
                    reached += 1;
                    queue.push_back(w);
                }
            }
        }
        if total > 0 {
            out[s as usize] = (reached as f64 / total as f64) * (reached as f64 / (n as f64 - 1.0));
        }
    }
    counted!("algo.closeness runs");
    out
}
