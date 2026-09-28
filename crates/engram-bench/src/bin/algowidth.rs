#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does splitting a fixpoint across workers start to pay?
//!
//! `Graph::algo_min_vertices` is the count below which a fixpoint runs serially
//! however wide the installed executor is. That number was a judgement — a
//! morsel costs one mutex, one slot vector and a merge, against a few
//! multiply-adds per vertex — and a judgement written into a constant with a
//! comment claiming it was "sized from the merge, not guessed" is a claim that
//! has to be paid for. This bin pays for it, and the bill was larger than
//! expected.
//!
//! It measures ONE thing the correctness tests deliberately cannot: cost. They
//! assert the answers are bit-identical across widths, which is exactly why
//! they can say nothing about whether the split was worth making.
//!
//! | stage | what it times |
//! |---|---|
//! | `w1` | PageRank with the algorithm parallelism lever off — the shipped serial lane |
//! | `w2` / `w4` / `w8` | the same run at each executor width |
//!
//! THE PREDICTION, recorded before the run. The per-iteration work is
//! `O(V + E)` multiply-adds over a materialised CSR with no store access and
//! no allocation inside the morsel, so the split should scale close to
//! linearly once the vertex count is large enough to dwarf the merge — and the
//! merge is `width` allocations plus one `Vec::extend` of the whole array. I
//! expect break-even somewhere in the low thousands of vertices, a real but
//! sub-linear gain by 100k (memory-bound, not compute-bound), and width 8 to
//! beat width 4 by noticeably less than 2x.
//!
//! WHAT IT MEASURED, and the prediction was wrong in the direction that
//! matters. Width 4 against the serial lane, medians across two runs:
//!
//! ```text
//!   vertices   1,024   4,096   16,384   65,536   200,000   500,000
//!   speedup     0.36x   0.82x    1.05x    1.09x     1.03x     0.97x
//!
//!   nodes=500000    w1 2207 ms   w2 2218 ms (1.00x)   w4 2271 ms (0.97x)   w8 1756 ms (1.26x)
//! ```
//!
//! **The split does not reliably pay at any size measured.** Worse for the
//! prediction than a small gain would have been: the width column at 500,000
//! is not monotonic, and the same 200,000-vertex run at width 4 came out at
//! 711 ms and 843 ms on two invocations — a spread wider than the effect. The
//! honest summary is that the numbers above are noise around 1.0x, not a
//! speed-up curve.
//!
//! Why, in one line: PageRank pulling over a materialised CSR is
//! MEMORY-BANDWIDTH-bound, and threads do not add bandwidth. The `O(V+E)`
//! shape that drove the prediction describes the instruction count, not the
//! bottleneck. What is left is then capped by Amdahl against the two O(V)
//! serial passes each iteration must keep — the morsel-order merge and the
//! width-independent convergence fold — neither of which may be parallelised
//! without giving up bit-identity, which is not for sale.
//!
//! Two things follow, and both are already done. The floor is set at 65,536,
//! where the LOSS stops, rather than at a size where a gain starts — there
//! isn't one. And the sub-floor rows are the reason the floor swaps the
//! executor out rather than asking it for a single morsel: 0.36x at 1,024
//! vertices was a thread spawn per iteration to run one closure, a slowdown
//! produced entirely by the guard against slowdowns.
//!
//! ```text
//! algowidth [nodes=...] [iters=...]
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The bench-lane threaded executor: the same shape as the server's, which is
/// the only production implementor. The engine never spawns; a bin may.
struct PoolExec(usize);

impl ScopedExec for PoolExec {
    fn width(&self) -> usize {
        self.0
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.0.min(n).max(1);
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(i);
                    }
                });
            }
        });
    }
}

const Q: &str = "CALL engram.algo.pagerank.stream({nodeLabels: ['N'], relationshipTypes: ['R']}) \
                 YIELD nodeId, score RETURN count(score)";

fn main() {
    let mut args = std::env::args().skip(1);
    let nodes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200_000);
    let iters: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);

    println!("algowidth [nodes={nodes}] [iters={iters}]");
    println!();
    println!("  A sweep ACROSS the floor first, then the widths at full size.");
    println!();

    // The floor sweep: sizes either side of the default say whether it is in
    // the right place, which is the only question a constant like that can be
    // wrong about.
    println!("  vertices        w1         w4     speedup");
    println!("  ---------------------------------------------");
    for v in [1_024usize, 4_096, 16_384, 65_536] {
        let g = build(v);
        // The floor is lowered so this sweep measures the SPLIT at each size
        // rather than the floor's own decision — the point is to find out
        // where the split starts paying, which is what sets the floor.
        g.set_algo_min_vertices(1);
        let w1 = time(&g, 1, iters);
        let w4 = time(&g, 4, iters);
        let speedup = if w4 > 0.0 { w1 / w4 } else { f64::NAN };
        println!("  {v:8}  {w1:8.2} ms {w4:8.2} ms   {speedup:6.2}x");
    }
    println!();

    // The widths at full size. "Unambiguously worth making" was the phrase
    // here before the first run; it is not, and the table says so.
    let g = build(nodes);
    g.set_algo_min_vertices(1);
    println!("  nodes={nodes}");
    println!("  width          time     speedup vs w1");
    println!("  ---------------------------------------------");
    let base = time(&g, 1, iters);
    println!("  1 (lever off) {base:8.2} ms        1.00x");
    for w in [2usize, 4, 8] {
        let t = time(&g, w, iters);
        let speedup = if t > 0.0 { base / t } else { f64::NAN };
        println!("  {w:<13} {t:8.2} ms   {speedup:10.2}x");
    }
    println!();
    println!("  The answers are bit-identical at every width — that is asserted by");
    println!("  `pagerank_is_bit_identical_at_every_scoped_exec_width`, not by this bin.");
    println!("  This bin LOWERS the floor so it measures the split rather than the floor's");
    println!("  decision. In a real run the executor is not entered below 65,536 vertices.");
}

/// A scale-free-ish corpus: degrees vary, so the score vector is not uniform
/// and the per-vertex work is unevenly distributed across the morsels — which
/// is the case a contiguous output split is worst at, and therefore the
/// honest one to measure.
fn build(nodes: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ids: Vec<u64> = (0..nodes)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("i".to_string(), Value::Int(i as i64));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for (i, src) in ids.iter().enumerate() {
        for k in 0..(i % 6) + 1 {
            g.create_rel(
                *src,
                "R",
                ids[(i * 7 + k * 13 + 1) % nodes],
                &BTreeMap::new(),
            )
            .expect("rel");
        }
    }
    g
}

/// Median of `iters` runs at `width`, with the projection cache already warm
/// so the CSR build is not counted — it is serial in both arms and would
/// simply dilute the ratio.
fn time(g: &Graph, width: usize, iters: usize) -> f64 {
    if width <= 1 {
        g.set_algo_parallel(false);
        g.set_exec(None);
    } else {
        g.set_algo_parallel(true);
        g.set_exec(Some(Arc::new(PoolExec(width))));
    }
    let stmt = parse_statement(Q).expect("parses");
    let _ = run_query(g, &stmt, Default::default()).expect("warm");

    let mut ms: Vec<f64> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            let _ = run_query(g, &stmt, Default::default()).expect("run");
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}
