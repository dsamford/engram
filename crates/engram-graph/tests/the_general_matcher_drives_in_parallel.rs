//! The general matcher drives its input rows in parallel, and answers exactly
//! as it did serially.
//!
//! The morsel executor was consulted in `pipeline.rs` and nowhere else, so
//! every query the vectorised pipeline DECLINES ran on one core however wide
//! `ENGRAM_QUERY_PARALLELISM` was set. Measured at SF3 on SNB BI bi11's
//! expansion — same query, same store, same binary, three widths:
//!
//! ```text
//! width 44   271 s        width 8   268 s        width 1   273 s
//! ```
//!
//! identical, 4,472,653 rows each, on a 48-CPU node. `pipeline.rs` records the
//! same pathology being found once before: "the entire benchmark ran on one
//! core of 44 while `query parallelism ON: width 44` sat in the log above it".
//!
//! # What is and is NOT parallel
//!
//! The DRIVE is. The COLLECTOR is not: `StreamProjector` carries top-k heaps,
//! late projection, aggregation sites, group indices and DISTINCT state, and
//! merging two of those correctly in every mode is its own feature — one whose
//! failure mode is a silently different answer. Each morsel drives into a local
//! buffer and the buffers reach the one collector IN MORSEL ORDER, so it sees
//! the rows it would have seen, in the order it would have seen them.
//!
//! Every test here is therefore a DIFFERENTIAL: the same query, the same
//! graph, parallel against serial, required to agree exactly.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

#[derive(Debug)]
struct TestExec {
    width: usize,
}

impl ScopedExec for TestExec {
    fn width(&self) -> usize {
        self.width
    }
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.width.min(n).max(1);
        if threads <= 1 {
            for i in 0..n {
                f(i);
            }
            return;
        }
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// A graph with enough driving rows to clear `parallel_min_rows`.
fn corpus(people: usize, deg: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::with_capacity(people);
    for i in 0..people {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i as i64));
        m.insert("name".to_string(), Value::Str(format!("p{i}")));
        ids.push(g.create_node(&["P".into()], &m).expect("node"));
    }
    for i in 0..people {
        for k in 1..=deg {
            g.create_rel(ids[i], "K", ids[(i + k) % people], &BTreeMap::new())
                .expect("rel");
        }
    }
    let _ = g.warm();
    g
}

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// The same query serially and at width 4 — required to agree EXACTLY,
/// including row order.
fn differential(src: &str) -> Vec<Vec<Value>> {
    differential_engaging(src, true)
}

/// `engages` says whether the parallel arm is EXPECTED to take the parallel
/// path. Asserting it is what stops the differential being vacuous — two
/// serial runs agree trivially, and that is exactly how a parallel path that
/// never fires would pass every test in this file.
fn differential_engaging(src: &str, engages: bool) -> Vec<Vec<Value>> {
    let g = corpus(200, 8);
    // `parallel_min_rows` exists to be lowered here — its own doc says it is
    // "settable so the differential tests exercise the parallel machinery on
    // small corpora". At the default (256) six of these tests ran BOTH arms
    // serially and agreed trivially, which is exactly how a parallel path that
    // never fires passes every differential written for it.
    g.set_parallel_min_rows(2);
    g.set_exec(None);
    let serial = run(&g, src);
    g.set_exec(Some(std::sync::Arc::new(TestExec { width: 4 })));
    let (parallel, t) = engram_observe::with_trace(|| run(&g, src));
    g.set_exec(None);
    assert_eq!(parallel, serial, "parallel and serial disagree on `{src}`");
    let fired = t
        .counters()
        .contains_key("interp.stage drove its input in parallel");
    assert_eq!(
        fired,
        engages,
        "parallel engagement on `{src}` was {fired}, expected {engages}: {:?}",
        t.counters()
    );
    serial
}

#[test]
fn a_two_clause_expansion_agrees() {
    // bi11's shape: a DISTINCT breaker, then a second MATCH expanding from the
    // carried variable — the shape the pipeline declines.
    let r = differential(
        "MATCH (a:P)-[:K]-(b:P) WHERE a.id < b.id WITH DISTINCT a, b \
         MATCH (b)-[:K]-(c:P) WHERE b.id < c.id RETURN count(*) AS n",
    );
    assert!(matches!(r[0][0], Value::Int(n) if n > 0), "{r:?}");
}

#[test]
fn a_distinct_breaker_agrees() {
    // NOT engaged: `count(DISTINCT …)` over one hop is a shape the VECTORISED
    // PIPELINE takes, so it never reaches the general matcher's driving loop
    // at all. Recorded because "it did not engage" and "it is not parallel"
    // are different claims, and only the first is true here.
    differential_engaging(
        "MATCH (a:P)-[:K]-(b:P) WITH DISTINCT b \
         MATCH (b)-[:K]-(c:P) RETURN count(DISTINCT c) AS n",
        false,
    );
}

#[test]
fn an_ordered_projection_agrees_row_for_row() {
    // ONE input row (the empty seed), so input-row splitting cannot engage —
    // this stage's work is a label scan, not a per-row drive. The differential
    // still has to hold.
    // ORDER BY makes row ORDER observable, so a morsel merged out of sequence
    // shows up here rather than as a count that happens to match.
    differential_engaging(
        "MATCH (a:P)-[:K]-(b:P) WHERE a.id < 20 \
         RETURN a.id AS a, b.id AS b ORDER BY a, b",
        false,
    );
}

#[test]
fn an_unordered_projection_agrees_row_for_row() {
    // WITHOUT an ORDER BY the order is unspecified by Cypher — but it must
    // still be DETERMINISTIC here, because the collector sees morsels in
    // input order. A parallel path that interleaved would fail this.
    differential_engaging(
        "MATCH (a:P)-[:K]-(b:P) WHERE a.id < 30 RETURN a.id AS a, b.id AS b",
        false,
    );
}

#[test]
fn an_aggregation_agrees() {
    // the SECOND stage is driven by the grouped rows, so it engages
    differential_engaging(
        "MATCH (a:P)-[:K]-(b:P) WITH a, count(b) AS c RETURN sum(c) AS total, max(c) AS worst",
        true,
    );
}

#[test]
fn an_optional_match_agrees() {
    // OPTIONAL's null row is per input row; a morsel boundary must not drop or
    // duplicate one. One input row here, so the drive stays serial — the
    // differential still has to hold.
    differential_engaging(
        "MATCH (a:P) OPTIONAL MATCH (a)-[:K]->(b:P) WHERE b.id > 190 \
         RETURN count(a) AS seen, count(b) AS matched",
        false,
    );
}

#[test]
fn a_limit_keeps_the_serial_path_and_agrees() {
    // A LIMIT raises `Saturated` mid-drive and a worker cannot see that
    // another morsel already filled it, so these stay serial — and must still
    // answer identically.
    differential_engaging(
        "MATCH (a:P)-[:K]-(b:P) RETURN a.id AS a, b.id AS b ORDER BY a, b LIMIT 25",
        false,
    );
}

#[test]
fn a_var_length_hop_agrees() {
    differential(
        "MATCH (a:P) WHERE a.id < 10 WITH DISTINCT a \
         MATCH (a)-[:K*1..2]->(c:P) RETURN count(DISTINCT c) AS n",
    );
}

#[test]
fn a_query_error_still_surfaces_from_a_worker() {
    // An error raised inside a morsel must reach the caller, not be swallowed
    // with the morsel's buffer.
    let g = corpus(200, 8);
    g.set_exec(Some(std::sync::Arc::new(TestExec { width: 4 })));
    let q = parse_statement("MATCH (a:P)-[:K]-(b:P) RETURN a.id / 0 AS boom").expect("parses");
    let got = run_query(&g, &q, BTreeMap::new());
    g.set_exec(None);
    assert!(got.is_err(), "division by zero must surface: {got:?}");
}

#[test]
fn every_eligible_stage_parallelises_not_just_the_first() {
    // `stream_stage` has TWO driving loops — one for a stage ending in
    // `RETURN`, one for a stage ending in a `WITH` breaker — and the first
    // version of this change touched only the first. bi11's stages are mostly
    // `WITH`-terminated, so exactly ONE stage per query went parallel and the
    // rest stayed serial. The engagement COUNT is what shows that; the rows
    // are identical either way, so no answer assertion could have caught it.
    let g = corpus(200, 8);
    g.set_parallel_min_rows(2);
    g.set_exec(Some(std::sync::Arc::new(TestExec { width: 4 })));
    let src = "MATCH (a:P)-[:K]-(b:P) WHERE a.id < b.id WITH DISTINCT a, b \
               MATCH (b)-[:K]-(c:P) WHERE b.id < c.id WITH DISTINCT a, b, c \
               RETURN count(*) AS n";
    let (r, t) = engram_observe::with_trace(|| run(&g, src));
    g.set_exec(None);
    let fired = t
        .counters()
        .get("interp.stage drove its input in parallel")
        .copied()
        .unwrap_or(0);
    assert!(
        fired >= 2,
        "both the WITH-terminated and the RETURN-terminated stage must go \
         parallel, got {fired}: {:?}",
        t.counters()
    );
    assert!(matches!(r[0][0], Value::Int(n) if n > 0), "{r:?}");
}
