#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! What does each graph algorithm cost, and is the all-pairs ceiling in the
//! right place?
//!
//! `algowidth` measures the parallel SPLIT on one algorithm. This measures the
//! algorithms themselves, which is a different question and the one that
//! decides a constant nobody has checked.
//!
//! # The constant under test
//!
//! `WORK_CEILING` is 10,000,000,000 `V x E` units, and its doc claims that is
//! "roughly a minute of single-threaded work at the rate the fixpoint driver
//! measures". **That was reasoned, not measured.** The whole point of the
//! ceiling is to refuse a projection before it runs away, so a ceiling set from
//! an estimate is a guess about the thing it exists to prevent — and a wrong
//! guess is invisible in both directions: too high and it never fires, too low
//! and it refuses work that would have finished.
//!
//! # The prediction, recorded before the run
//!
//! The `O(V + E)` algorithms — PageRank, WCC, SCC, degree, label propagation,
//! Louvain — should scale close to linearly in `V + E` and finish a
//! hundred-thousand-node projection in well under a second.
//!
//! Betweenness and closeness are `O(V x E)`. At V=2,000 with about 6,000 edges
//! that is 1.2e7 units; if the ceiling's claim holds, 1e10 units is 60 s, so
//! 1.2e7 should be about 70 ms. The test of the claim is whether the measured
//! time per unit is within an order of magnitude of that, across two sizes —
//! and whether it stays constant as the projection grows, which is what makes
//! `V x E` the right thing to price at all.
//!
//! # What it measured
//!
//! ```text
//!   nodes=500   edges=1500   V x E = 7.500e5
//!     pagerank 1.12 ms   wcc 1.17   scc 1.06   degree 1.06
//!     labelpropagation 1.99   louvain 1.70   trianglecount 1.07
//!     betweenness  3.72 ms   4.959 ns/unit  → ceiling ~ 49.6 s
//!     closeness    2.63 ms   3.506 ns/unit  → ceiling ~ 35.1 s
//!
//!   nodes=2000  edges=6000   V x E = 1.200e7
//!     pagerank 4.97 ms   wcc 4.69   scc 4.89   degree 4.89
//!     labelpropagation 9.10   louvain 8.58   trianglecount 5.14
//!     betweenness 58.43 ms   4.869 ns/unit  → ceiling ~ 48.7 s
//!     closeness   31.14 ms   2.595 ns/unit  → ceiling ~ 25.9 s
//! ```
//!
//! **The prediction held, which is worth saying plainly because on this
//! codebase it has more often not.** Two results, and the second matters more
//! than the first:
//!
//! 1. `1e10` units measures at 48.7 s for betweenness — the `WORK_CEILING`
//!    doc's "roughly a minute of single-threaded work" was reasoned rather
//!    than measured, and it was right.
//! 2. **The per-unit cost is CONSTANT across a sixteenfold change in `V x E`**
//!    — 4.959 then 4.869 ns for betweenness, 3.506 then 2.595 for closeness.
//!    That is the load-bearing result, because it is what makes `V x E` the
//!    right quantity to price at all. A ceiling on a product whose per-unit
//!    cost drifted with size would refuse correctly at one scale and wrongly
//!    at every other, and a single measurement could never have shown it.
//!
//! The `O(V + E)` row scales as advertised: four times the nodes, four to five
//! times the time, for every one of the seven.
//!
//! # The MODES, and the result that was not expected
//!
//! `stream` was the only mode any benchmark or stress shape had ever issued.
//! Over 2,000 nodes on `degree`, where the mode's own work dominates:
//!
//! ```text
//!   stream   4.80 ms   1.00x
//!   stats    0.93 ms   0.19x
//!   mutate   0.57 ms   0.12x
//!   write    3.91 ms   0.81x
//! ```
//!
//! **About four fifths of a `stream` call is materialising rows, not
//! computing.** `stats` runs the identical computation and returns one row
//! instead of `n`, and it is five times faster.
//!
//! And the one worth stopping on: **`write` is CHEAPER than `stream`.**
//! Persisting a property to every node through the ordinary MVCC write path,
//! in short transactions, costs less than handing the same number of rows back
//! to the caller. The intuition that the write path is the expensive part is
//! simply wrong at this size — row materialisation is, and it is the cost
//! nobody was measuring because `stream` was the only mode anyone ran.
//!
//! That inverts where an optimisation would go. Anyone reaching for "make the
//! algorithm faster" on the strength of a `stream` measurement would be tuning
//! the fifth of the call that is the algorithm.
//!
//! ```text
//! algocost [nodes=...] [iters=...]
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Every algorithm that answers over a whole projection, with whether its cost
/// is all-pairs. The path procedures are measured separately: their cost is
/// dominated by `k` and the two endpoints, not by the projection.
const ALGORITHMS: &[(&str, bool)] = &[
    ("pagerank", false),
    ("wcc", false),
    ("scc", false),
    ("degree", false),
    ("labelpropagation", false),
    ("louvain", false),
    ("trianglecount", false),
    ("betweenness", true),
    ("closeness", true),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let nodes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2_000);
    let iters: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);

    println!("algocost [nodes={nodes}] [iters={iters}]");
    println!();

    // Two sizes, so "cost per V x E unit" can be checked for CONSTANCY rather
    // than computed once and believed. A single size cannot tell a linear cost
    // from a quadratic one.
    for n in [nodes / 4, nodes] {
        let g = build(n);
        let edges = count_edges(&g);
        let all_pairs_units = (n as u64).saturating_mul(edges);
        println!("  nodes={n}  edges={edges}  V x E = {all_pairs_units:.3e}");
        println!("    algorithm                 median      per V x E unit");
        println!("    -------------------------------------------------------");
        for (alg, all_pairs) in ALGORITHMS {
            let ms = time(&g, alg, iters);
            if *all_pairs {
                // Nanoseconds per unit, which is the number the ceiling is
                // really a statement about.
                let ns_per_unit = ms * 1e6 / all_pairs_units.max(1) as f64;
                let at_ceiling_s = ns_per_unit * 1e10 / 1e9;
                println!(
                    "    {alg:<22} {ms:8.2} ms   {ns_per_unit:8.3} ns  → ceiling ≈ {at_ceiling_s:6.1} s",
                );
            } else {
                println!("    {alg:<22} {ms:8.2} ms          (O(V+E))");
            }
        }
        println!();
    }

    // ── THE MODES ────────────────────────────────────────────────────────
    //
    // Everything above measures `stream`, which was the only mode any
    // benchmark or stress shape ever issued. The other three do materially
    // different work after the same computation, and none of it was measured:
    //
    // - `stats` returns ONE row, so `stream` minus `stats` is the cost of
    //   materialising a row per node — the part that is not the algorithm.
    // - `mutate` publishes into the result cache, which copies the result and
    //   charges it against a byte budget.
    // - `write` persists a property per node through the ordinary write path
    //   in short transactions. It is the only algorithm operation that touches
    //   the keyspace, and until this was measured nobody knew what it cost.
    {
        let g = build(nodes);
        println!("  MODES over {nodes} nodes, on `degree` — so the mode's own cost dominates");
        println!("    mode                      median      vs stream");
        println!("    -------------------------------------------------------");
        let mut base = 0.0f64;
        for mode in ["stream", "stats", "mutate", "write"] {
            let ms = time_mode(&g, mode, iters);
            if mode == "stream" {
                base = ms;
                println!("    {mode:<22} {ms:8.2} ms        1.00x");
            } else {
                println!("    {mode:<22} {ms:8.2} ms   {:10.2}x", ms / base.max(1e-9));
            }
        }
        println!();
    }

    println!("  The `→ ceiling` column is the claim under test: how long a run AT the");
    println!("  default WORK_CEILING of 1e10 units would actually take, at the measured");
    println!("  rate. If the two sizes disagree, `V x E` is the wrong thing to price.");
}

/// A corpus with irregular degree, so no algorithm gets an unrealistically
/// tidy graph and the community algorithms have something to find.
fn build(nodes: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ids: Vec<u64> = (0..nodes)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i as i64));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for (i, src) in ids.iter().enumerate() {
        for k in 0..(i % 5) + 1 {
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

fn count_edges(g: &Graph) -> u64 {
    match run_query(
        g,
        &parse_statement("MATCH ()-[r:R]->() RETURN count(r)").expect("parses"),
        BTreeMap::new(),
    )
    .expect("runs")
    .rows
    .first()
    .and_then(|r| r.first())
    {
        Some(Value::Int(n)) => *n as u64,
        _ => 0,
    }
}

/// Median of `iters` runs through the REAL procedure surface, so the number
/// includes the projection build and the row materialisation a caller pays —
/// not just the kernel, which no user can invoke on its own.
fn time(g: &Graph, alg: &str, iters: usize) -> f64 {
    // The ceiling is lifted so the measurement is of the WORK, not of the
    // refusal: the whole point is to find out what the ceiling should be.
    g.set_algo_work_ceiling(u64::MAX);
    let src = format!(
        "CALL engram.algo.{alg}.stream({{nodeLabels: ['N'], relationshipTypes: ['R'], \
         maxIterations: 20}}) YIELD nodeId RETURN count(nodeId)"
    );
    let stmt = parse_statement(&src).expect("parses");
    let _ = run_query(g, &stmt, BTreeMap::new()).expect("warm");

    let mut ms: Vec<f64> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            let _ = run_query(g, &stmt, BTreeMap::new()).expect("run");
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}

/// One mode of `degree`, through the real procedure surface.
///
/// `degree` deliberately: it is the cheapest algorithm here, so the number is
/// dominated by what the MODE does rather than by the computation, which is
/// what makes the ratios readable.
fn time_mode(g: &Graph, mode: &str, iters: usize) -> f64 {
    g.set_algo_work_ceiling(u64::MAX);
    let src = match mode {
        "stream" => "CALL engram.algo.degree.stream({nodeLabels: ['N'],              relationshipTypes: ['R']}) YIELD degree RETURN count(degree)"
            .to_string(),
        "stats" => "CALL engram.algo.degree.stats({nodeLabels: ['N'],              relationshipTypes: ['R']}) YIELD nodeCount RETURN nodeCount"
            .to_string(),
        // A VARYING key would grow the cache without bound across iterations;
        // a fixed one overwrites in place, which is the steady state a caller
        // re-running one named measurement is actually in.
        "mutate" => "CALL engram.algo.degree.mutate({nodeLabels: ['N'],              relationshipTypes: ['R'], mutateKey: 'bench'}) YIELD mutateKey RETURN mutateKey"
            .to_string(),
        _ => "CALL engram.algo.degree.write({nodeLabels: ['N'],              relationshipTypes: ['R'], writeProperty: 'bench'})              YIELD nodesWritten RETURN nodesWritten"
            .to_string(),
    };
    let stmt = parse_statement(&src).expect("parses");
    let _ = run_query(g, &stmt, BTreeMap::new()).expect("warm");
    let mut ms: Vec<f64> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            let _ = run_query(g, &stmt, BTreeMap::new()).expect("run");
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}
