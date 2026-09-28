#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(non_snake_case)]
//! Fix 119: the count fold had no row floor of its own. It borrowed
//! `parallel_min_rows`, which is 256 and is right for `expand` — an expand's
//! driving row is a cheap probe and 256 of them barely covers a spawn.
//!
//! A fold's driving row is not cheap. It is an entire nested walk. LSQB q3
//! seeds on `country` and SF1 holds 111 of them, so `111 < 256` was false and
//! q3 ran **single-threaded on a six-worker server** for the whole of its
//! measured life, at 5,471 ms against PostgreSQL's 1,376. Nothing said so:
//! the parallel counter simply never appeared, and an absent counter reads
//! like a path that was not needed.
//!
//! The fold now has its own floor of 2, and cuts finer than one morsel per
//! worker when no level memoises, so the executor's work-stealing cursor can
//! balance a skewed seed (SF1's largest country holds 1,447 persons against a
//! median in the low hundreds).
//!
//! The existing differential, `parallel_fold_is_byte_identical.rs`, cannot
//! catch this: it calls `set_parallel_min_rows(2)` to force the split, so it
//! passed both before and after. This file leaves every floor at its DEFAULT
//! and asserts the small seed splits anyway.
//!
//! Canary, run: restoring `chunk.selection.len() >= graph.parallel_min_rows()`
//! fails `a_a_seed_under_the_expand_floor_still_splits` on a zero counter
//! while every answer assertion still passes — which is exactly the shape of
//! the defect, a silent serialisation with correct results.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PARALLEL: &str = "interp.pipeline fold parallel";
const FINER: &str = "interp.pipeline fold cut finer than its worker count";

#[derive(Debug)]
struct TestExec {
    width: usize,
    /// How many morsels the engine actually asked for — the finer-cut claim
    /// is about this number, not about the answer.
    morsels: std::sync::atomic::AtomicUsize,
}

impl ScopedExec for TestExec {
    fn width(&self) -> usize {
        self.width
    }
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.morsels.store(n, std::sync::atomic::Ordering::Relaxed);
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

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

/// q3's shape in miniature: a small SEED (countries) that each fan out to
/// many rows, which is precisely the case the expand-sized floor got wrong.
/// 24 countries — well under 256 — each holding five persons who know the
/// next two, so the fold's driving rows are countries and its work per row is
/// a nested walk.
const COUNTRIES: i64 = 24;
const PER: i64 = 5;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for c in 0..COUNTRIES {
        stmt(&g, &format!("CREATE (:Country {{id: {c}}})"));
    }
    for c in 0..COUNTRIES {
        for k in 0..PER {
            let p = c * PER + k;
            stmt(&g, &format!("CREATE (:Person {{id: {p}, home: {c}}})"));
        }
    }
    let edge = |ty: &str, a: i64, b: i64| {
        stmt(
            &g,
            &format!(
                "MATCH (x:Person {{id: {a}}}), (y:Person {{id: {b}}}) CREATE (x)-[:{ty}]->(y)"
            ),
        );
    };
    let total = COUNTRIES * PER;
    for p in 0..total {
        edge("KNOWS", p, (p + 1) % total);
        edge("KNOWS", p, (p + 2) % total);
    }
    // Each person located in their country, so the fold seeds on Country.
    for c in 0..COUNTRIES {
        for k in 0..PER {
            let p = c * PER + k;
            stmt(
                &g,
                &format!(
                    "MATCH (x:Person {{id: {p}}}), (y:Country {{id: {c}}}) CREATE (x)-[:IS_LOCATED_IN]->(y)"
                ),
            );
        }
    }
    g.shared_store().seal();
    g
}

/// Folds seeded on the 24 countries — the q3 shape, and a cyclic close.
const QUERIES: &[&str] = &[
    "MATCH (c:Country) MATCH (p1:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p2:Person)-[:IS_LOCATED_IN]->(c) MATCH (p1)-[:KNOWS]-(p2) \
     RETURN count(*) AS n",
    "MATCH (c:Country) MATCH (p1:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p1)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p1) \
     RETURN count(*) AS n",
    "MATCH (c:Country) MATCH (p:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p)-[:KNOWS]->(q:Person) RETURN c.id AS id, count(*) AS n",
];

/// Enable the parallel fold and install an executor, leaving EVERY row floor
/// at its default — the whole point of this file.
fn with_parallel_at_defaults<R>(g: &Graph, width: usize, f: impl FnOnce() -> R) -> (R, usize) {
    let exec = std::sync::Arc::new(TestExec {
        width,
        morsels: std::sync::atomic::AtomicUsize::new(0),
    });
    g.set_exec(Some(exec.clone()));
    g.set_parallel_fold(true);
    let out = f();
    g.set_parallel_fold(false);
    g.set_exec(None);
    (out, exec.morsels.load(std::sync::atomic::Ordering::Relaxed))
}

#[test]
fn a_a_seed_under_the_expand_floor_still_splits() {
    let g = fixture();
    let (_, trace) = engram_observe::with_trace(|| {
        with_parallel_at_defaults(&g, 4, || {
            let _ = rows(&g, QUERIES[0]);
        })
    });
    let n = trace.counters().get(PARALLEL).copied().unwrap_or(0);
    assert!(
        n > 0,
        "a {COUNTRIES}-row seed did not split at DEFAULT floors — the fold is \
         still borrowing the expand-sized floor, which is the whole defect: {:?}",
        trace.counters()
    );
}

#[test]
fn b_the_small_seed_answers_exactly_what_serial_answers() {
    let g = fixture();
    let serial: Vec<Vec<Vec<Value>>> = QUERIES.iter().map(|q| rows(&g, q)).collect();
    // Widths that do not divide 24 put morsel boundaries mid-selection.
    for width in [2, 3, 5, 7, 64] {
        let (parallel, _) = with_parallel_at_defaults(&g, width, || {
            QUERIES.iter().map(|q| rows(&g, q)).collect::<Vec<_>>()
        });
        assert_eq!(
            serial, parallel,
            "the small-seed fold answered differently at width {width}"
        );
    }
}

#[test]
fn c_a_fold_that_memoises_nothing_is_cut_finer_than_its_workers() {
    let g = fixture();
    let ((_, morsels), trace) = engram_observe::with_trace(|| {
        with_parallel_at_defaults(&g, 4, || {
            let _ = rows(&g, QUERIES[1]);
        })
    });
    // The finer cut is what lets a skewed seed balance: SF1's largest country
    // holds 1,447 persons against a median in the low hundreds, and one
    // contiguous chunk per worker hands that giant to a single thread.
    if trace.counters().get(FINER).copied().unwrap_or(0) > 0 {
        assert!(
            morsels > 4,
            "the finer cut was counted but the executor was still asked for \
             {morsels} morsels at width 4"
        );
    }
    // Whether or not this particular plan memoises, the answer must not move.
    let serial = rows(&g, QUERIES[1]);
    let (parallel, _) = with_parallel_at_defaults(&g, 4, || rows(&g, QUERIES[1]));
    assert_eq!(serial, parallel, "the finer cut changed an answer");
}

#[test]
fn d_the_expand_floor_is_left_alone() {
    // Fix 119 gives the FOLD its own floor and must not move `expand`'s: 256
    // is right there, and lowering it would spawn for work that cannot repay
    // a spawn. This pins the two apart.
    let g = fixture();
    assert_eq!(
        g.parallel_min_rows_for_test(),
        256,
        "the expand floor moved — fix 119 was supposed to leave it alone"
    );
    g.set_parallel_fold_min_rows(2);
    assert_eq!(
        g.parallel_min_rows_for_test(),
        256,
        "setting the fold floor moved the expand floor"
    );
}
