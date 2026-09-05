#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The morsel-parallel COUNT FOLD must be invisible in every answer.
//!
//! # What this guards
//!
//! P-2 splits `fold_tail`'s driving rows across the installed [`ScopedExec`],
//! one `FoldState` per worker, partials concatenated in morsel order. The
//! claim is BYTE-IDENTITY with the serial loop — same kept rows, same folded
//! weights, therefore same counts — which holds only while:
//!
//! * the merge preserves ROW ORDER across morsel boundaries;
//! * per-worker `FoldState`s are self-consistent (memo-eligible levels are
//!   pure functions of node id, so duplicated memos cost work, never answers);
//! * the per-row isomorphism base (`used_rels`) travels with its row.
//!
//! Every count below runs the SAME query serial and parallel and demands
//! equality, across the fold shapes that exercise different close arms:
//! a plain chain, a tracked cyclic close, an untracked two-path close, and a
//! WHERE edge predicate. Widths that do not divide the row count are the
//! merge-boundary case.
//!
//! # Proven to bite
//!
//! Checked against a merge that reverses morsel order (`slots` iterated
//! backwards): `every_fold_shape_agrees_with_serial_at_every_width` fails at
//! width 2 on the grouped NO-ORDER-BY query — group output order is first-seen
//! order, which is selection order, the one place a scrambled merge shows in
//! an answer. The count-only shapes pass against that break (sums cannot see
//! order), which is why the grouped query exists and why it must never gain
//! an ORDER BY.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A REAL multi-thread executor for the test — the engine never spawns, so the
/// production implementor lives in the server; tests bring their own.
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

const N: i64 = 60;

/// A ring with KNOWS(+1), LIKES(+1,+2), and mutual FOLLOWS on the first ten
/// pairs — enough asymmetry that a wrong merge or a wrong close changes an
/// answer rather than reshuffling equals.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    let edge = |ty: &str, a: i64, b: i64| {
        stmt(
            &g,
            &format!("MATCH (x:P {{id: {a}}}), (y:P {{id: {b}}}) CREATE (x)-[:{ty}]->(y)"),
        );
    };
    for p in 0..N {
        edge("KNOWS", p, (p + 1) % N);
        edge("LIKES", p, (p + 1) % N);
        edge("LIKES", p, (p + 2) % N);
        edge("FOLLOWS", p, (p + 1) % N);
    }
    for p in 0..10 {
        edge("FOLLOWS", p + 1, p);
    }
    g.shared_store().seal();
    g
}

const QUERIES: &[&str] = &[
    // A plain folded chain.
    "MATCH (a:P)-[:KNOWS]->(b:P)-[:LIKES]->(c:P) RETURN count(*) AS c",
    // A TRACKED cyclic close (edges_to_peer_slim arm).
    "MATCH (a:P)-[:FOLLOWS]->(b:P)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    // An UNTRACKED two-path close (count_edges arm).
    "MATCH (a:P)-[:FOLLOWS]->(b:P), (b)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    // A WHERE edge predicate (pred_holds arm).
    "MATCH (a:P)-[:FOLLOWS]->(b:P) WHERE (b)-[:FOLLOWS]->(a) RETURN count(*) AS c",
    // Grouped WITHOUT an ORDER BY: group output order is first-seen order,
    // which is SELECTION order — the one place a merge that scrambles morsel
    // order is visible in an answer. An ORDER BY here would re-sort and hide
    // exactly the defect this query exists to catch.
    "MATCH (a:P)-[:LIKES]->(b:P)-[:KNOWS]->(c:P) RETURN a.id AS id, count(*) AS c",
];

fn with_parallel<R>(g: &Graph, width: usize, f: impl FnOnce() -> R) -> R {
    g.set_exec(Some(std::sync::Arc::new(TestExec { width })));
    g.set_parallel_fold(true);
    g.set_parallel_min_rows(2);
    let out = f();
    g.set_parallel_fold(false);
    g.set_exec(None);
    out
}

#[test]
fn every_fold_shape_agrees_with_serial_at_every_width() {
    let g = fixture();
    let serial: Vec<Vec<Vec<Value>>> = QUERIES.iter().map(|q| rows(&g, q)).collect();
    // Widths chosen so morsel boundaries land mid-selection (60 rows / 7) and
    // past it (width > rows is the degenerate clamp).
    for width in [2, 3, 7, 128] {
        let parallel: Vec<Vec<Vec<Value>>> =
            with_parallel(&g, width, || QUERIES.iter().map(|q| rows(&g, q)).collect());
        assert_eq!(
            serial, parallel,
            "a morsel-parallel fold answered differently at width {width}"
        );
    }
}

#[test]
fn the_parallel_path_actually_ran() {
    let g = fixture();
    let (_, on) = engram_observe::with_trace(|| {
        with_parallel(&g, 3, || {
            let _ = rows(&g, QUERIES[0]);
        })
    });
    assert!(
        on.counters().get("interp.pipeline fold parallel").copied().unwrap_or(0) > 0,
        "the ON arm never took the parallel path — this file proved nothing"
    );
    let (_, off) = engram_observe::with_trace(|| {
        let _ = rows(&g, QUERIES[0]);
    });
    assert_eq!(
        off.counters().get("interp.pipeline fold parallel").copied().unwrap_or(0),
        0,
        "the lever is off and the parallel path still ran"
    );
}

#[test]
fn a_transaction_stays_serial() {
    // A worker thread cannot see this thread's buffered writes, so the gate
    // must refuse while a transaction is active — and the answer must include
    // the buffered edge (read-your-writes), which only the serial path can.
    let g = fixture();
    with_parallel(&g, 3, || {
        g.begin_txn().expect("begin");
        stmt(
            &g,
            "MATCH (a:P {id: 0}), (b:P {id: 30}) CREATE (a)-[:KNOWS]->(b)",
        );
        let (r, trace) = engram_observe::with_trace(|| {
            rows(&g, "MATCH (a:P)-[:KNOWS]->(b:P)-[:LIKES]->(c:P) RETURN count(*) AS c")
        });
        assert_eq!(
            trace.counters().get("interp.pipeline fold parallel").copied().unwrap_or(0),
            0,
            "an active transaction must keep the fold serial"
        );
        // The buffered KNOWS 0->30 adds b=30's two LIKES continuations.
        assert_eq!(r, vec![vec![Value::Int(N * 2 + 2)]]);
        g.rollback_txn();
    });
}
