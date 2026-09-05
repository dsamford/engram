#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! WHAT DOES A FOLD CLOSE COST, per leaf, on a HOT row?
//!
//! LSQB q2's close — `edge_count_slim(person1, Both, KNOWS, person2)` with
//! `person1` the seed and therefore fixed for ~102 consecutive calls — measured
//! at ~1,100-1,200 ns per leaf on the pod (§20), against ~100 ns for a chain
//! hop over a COLD row. By inspection the close is a memo hit, two
//! `partition_point`s over ~36 entries and a handful of loads, which should be
//! well under 100 ns. Three hypotheses about that gap (table resolution, the
//! rel-uniqueness filter, CSR locality) have each been excluded by measurement
//! or by reading the code.
//!
//! This reproduces q2's SHAPE in miniature so the number can be taken on a
//! laptop in seconds instead of on the pod in minutes: a seed row that stays
//! hot, a chain of expands beneath it, and a close back onto the seed at the
//! leaf. The delta between the chain WITH the close and WITHOUT it is the
//! per-leaf close cost, and `edge_count_slim` is also timed in isolation so the
//! two can be told apart: if the isolated probe is cheap and the in-fold delta
//! is not, the cost is in the fold's plumbing, not in the probe.
//!
//! Timing-only; `#[ignore]` so the gate never runs it. Run with
//!   cargo test -p engram-graph --release --test close_probe_cost -- --ignored --nocapture

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Dir, Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn count(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one Int from `{src}`, got {other:?}"),
    }
}

/// N persons; each KNOWS the next `OUT` (out-degree OUT, undirected degree
/// 2*OUT — LDBC SF1's KNOWS averages ~18 out / ~36 undirected).
const N: i64 = 4_000;
const OUT: i64 = 18;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    // Bulk-ish creation: one CREATE per node, then edges in batches by MATCH.
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    for p in 0..N {
        for d in 1..=OUT {
            let q = (p + d) % N;
            stmt(
                &g,
                &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:KNOWS]->(b)"),
            );
        }
    }
    g.shared_store().seal();
    g
}

fn ns_per(t: Instant, n: u64) -> f64 {
    t.elapsed().as_nanos() as f64 / n as f64
}

#[test]
#[ignore = "timing-only; run explicitly in release"]
fn close_cost_isolated_and_in_fold() {
    let g = fixture();
    let knows = g.type_token_peek("KNOWS").expect("KNOWS minted");
    let tokens = Some(vec![knows]);

    // Warm: a two-hop count builds the O and I KNOWS tables.
    let two = "MATCH (a:P)-[:KNOWS]->(b:P)-[:KNOWS]->(c:P) RETURN count(*) AS c";
    let closed = "MATCH (a:P)-[:KNOWS]->(b:P)-[:KNOWS]->(c:P)-[:KNOWS]-(a) RETURN count(*) AS c";
    let n2 = count(&g, two);
    let nc = count(&g, closed);
    assert_eq!(n2, N * OUT * OUT, "two-hop leaf count");
    println!("leaves per fold: {n2}; closed answer: {nc}");

    // Need a node id to probe. Node ids are opaque; take them from the table
    // by asking the engine for a person's neighbours through Cypher would be
    // slow — instead probe by id sweep: the first N ids created are the P nodes
    // in creation order (the store allocates densely from a fresh seed).
    // Verify that assumption rather than trust it.
    let hot: u64 = 1_000;
    let mut deg = 0u64;
    g.adjacent_slim_for_each(hot, Dir::Both, &tokens, |_| deg += 1);
    assert_eq!(
        deg,
        (2 * OUT) as u64,
        "node {hot} should have undirected KNOWS degree {} — if not, ids are not \
         dense from 0 and this probe is not reading a person",
        2 * OUT
    );

    // 1. edge_count_slim in isolation, hot row, peers cycling through a window
    //    that mixes hits (the 36 neighbours) and misses.
    let reps = 2_000_000u64;
    let mut sink = 0u64;
    let t = Instant::now();
    for i in 0..reps {
        let peer = (hot as i64 - 40 + (i % 80) as i64).rem_euclid(N) as u64;
        sink += g.edge_count_slim(hot, Dir::Both, &tokens, peer);
    }
    let a = ns_per(t, reps);
    println!("edge_count_slim(hot, Both, KNOWS, peer)   {a:8.1} ns/call   (sink {sink})");

    // 2. The same, DIRECTED (one side).
    let mut sink = 0u64;
    let t = Instant::now();
    for i in 0..reps {
        let peer = (hot as i64 - 40 + (i % 80) as i64).rem_euclid(N) as u64;
        sink += g.edge_count_slim(hot, Dir::Out, &tokens, peer);
    }
    let b = ns_per(t, reps);
    println!("edge_count_slim(hot, Out,  KNOWS, peer)   {b:8.1} ns/call   (sink {sink})");

    // 3. A raw row visit for scale — what one adjacency read costs here.
    let mut sink = 0u64;
    let t = Instant::now();
    for _ in 0..reps {
        g.adjacent_slim_for_each(hot, Dir::Out, &tokens, |e| sink += e.peer & 1);
    }
    let c = ns_per(t, reps);
    println!("adjacent_slim_for_each(hot, Out)         {c:8.1} ns/call   (sink {sink})");

    // 4. In the fold: chain vs chain+close, two ways of writing the close.
    //
    //    `closed`   ONE path, the cycle closes at its end — q3's shape, and
    //               the `Q2-inl` variant that ran at 1,150 ms on the pod.
    //    `twopath`  the close as a SEPARATE single-hop path between two vars
    //               the main path binds — LSQB q2 as written, 2,100 ms on the
    //               pod for the identical answer.
    //
    //    The pod put the difference at ~1,000 ns per leaf. If it reproduces
    //    here the mechanism can be found with a trace instead of guessed at.
    let twopath = "MATCH (a:P)-[:KNOWS]-(c:P), (a)-[:KNOWS]->(b:P)-[:KNOWS]->(c) RETURN count(*) AS c";
    let nt = count(&g, twopath);
    assert_eq!(nt, nc, "the two spellings of the close must agree");
    // The hypothesis for the pod: the two-path form MATERIALISES `c` (the fold
    // declines the close) and the close runs as a semijoin over a columnar
    // expand — which allocates a `used_rels` Vec per row. Force that shape here
    // by reading `c` outside the fold, and see whether ~1,000 ns/leaf appears.
    let materialised =
        "MATCH (a:P)-[:KNOWS]-(c:P), (a)-[:KNOWS]->(b:P)-[:KNOWS]->(c) WHERE c.id >= 0 RETURN count(*) AS c";
    assert_eq!(count(&g, materialised), nc, "the materialised spelling must agree too");

    let rounds = 5;
    let mut best_two = f64::MAX;
    let mut best_closed = f64::MAX;
    let mut best_twopath = f64::MAX;
    let mut best_mat = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        let _ = count(&g, two);
        best_two = best_two.min(t.elapsed().as_secs_f64());
        let t = Instant::now();
        let _ = count(&g, closed);
        best_closed = best_closed.min(t.elapsed().as_secs_f64());
        let t = Instant::now();
        let _ = count(&g, twopath);
        best_twopath = best_twopath.min(t.elapsed().as_secs_f64());
        let t = Instant::now();
        let _ = count(&g, materialised);
        best_mat = best_mat.min(t.elapsed().as_secs_f64());
    }
    let leaves = n2 as f64;
    let per_leaf_two = best_two * 1e9 / leaves;
    let per_leaf_closed = best_closed * 1e9 / leaves;
    let per_leaf_twopath = best_twopath * 1e9 / leaves;
    println!(
        "fold: MATERIALISED c + semijoin {:7.1} ms  = {:7.1} ns/leaf   (+{:.1} ns/leaf)",
        best_mat * 1e3,
        best_mat * 1e9 / leaves,
        best_mat * 1e9 / leaves - per_leaf_two
    );
    println!(
        "fold: chain only            {:7.1} ms  = {per_leaf_two:7.1} ns/leaf",
        best_two * 1e3
    );
    println!(
        "fold: chain + inline close  {:7.1} ms  = {per_leaf_closed:7.1} ns/leaf   (+{:.1} ns/leaf)",
        best_closed * 1e3,
        per_leaf_closed - per_leaf_two
    );
    println!(
        "fold: chain + SECOND-PATH   {:7.1} ms  = {per_leaf_twopath:7.1} ns/leaf   (+{:.1} ns/leaf)",
        best_twopath * 1e3,
        per_leaf_twopath - per_leaf_two
    );
    println!("isolated probe: {a:.1} ns");

    // 5. WHAT fires per leaf on the second-path form — the counters name the
    //    path taken, which is what the pod's timings could not.
    let (_, tr_inline) = engram_observe::with_trace(|| count(&g, closed));
    let (_, tr_two) = engram_observe::with_trace(|| count(&g, twopath));
    let mut keys: Vec<&String> = tr_two.counters().keys().collect();
    keys.extend(tr_inline.counters().keys());
    keys.sort();
    keys.dedup();
    println!("\n{:>12} {:>12}  counter", "inline", "two-path");
    for k in keys {
        let a = tr_inline.counters().get(k).copied().unwrap_or(0);
        let b = tr_two.counters().get(k).copied().unwrap_or(0);
        if a != b || b >= leaves as u64 / 4 {
            println!("{a:>12} {b:>12}  {k}");
        }
    }

    // 6. EVENTS, not counters: a `sometimes!`/`always!` records an event and
    //    no counter, so a site firing per leaf is invisible above. The pod's
    //    two-path q2 took 35 s TRACED against 3 s for the inline spelling with
    //    the same counted operations, which is the fingerprint of exactly that.
    let sites = |t: &engram_observe::Trace| {
        let mut m: BTreeMap<(String, String), u64> = BTreeMap::new();
        for e in t.events() {
            *m.entry((format!("{:?}", e.tag), e.name.clone())).or_insert(0) += 1;
        }
        m
    };
    let (si, st) = (sites(&tr_inline), sites(&tr_two));
    println!(
        "\nevents: inline {}  two-path {}",
        tr_inline.events().len(),
        tr_two.events().len()
    );
    let mut all: Vec<&(String, String)> = si.keys().chain(st.keys()).collect();
    all.sort();
    all.dedup();
    println!("{:>12} {:>12}  tag / site", "inline", "two-path");
    for k in all {
        let a = si.get(k).copied().unwrap_or(0);
        let b = st.get(k).copied().unwrap_or(0);
        if !k.0.starts_with("Count") && (a != b || b >= leaves as u64 / 4) {
            println!("{a:>12} {b:>12}  {} / {}", k.0, k.1);
        }
    }
}
