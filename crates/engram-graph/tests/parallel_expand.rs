#![allow(non_snake_case)]
//! Morsel-parallel `expand` (#5) — the A/B differential proving the parallel
//! path (lever ON) returns byte-identically what the serial path (lever OFF, the
//! default) does, INCLUDING row order: the morsels are concatenated in slice
//! order, so a parallel expansion reproduces the serial one exactly. The lever is
//! off by default, so this suite is the only place the parallel path runs — every
//! other test, the determinism digest, and the benchmarks keep the serial path.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, SerialExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The test-lane threaded executor — the same shape as the server's
/// production implementor (tests may spawn; the engine may not).
struct TestExec(usize);

impl ScopedExec for TestExec {
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

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // 40 People; each KNOWS three others (by id arithmetic), the edge weighted.
    // Enough driving rows that `expand` splits across morsels.
    ddl(&g, "UNWIND range(0, 39) AS i CREATE (:P {id: i})");
    ddl(
        &g,
        "MATCH (a:P), (b:P) \
         WHERE b.id IN [(a.id + 1) % 40, (a.id + 7) % 40, (a.id + 13) % 40] \
         CREATE (a)-[:R {w: a.id * 100 + b.id}]->(b)",
    );
    g
}

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

type Rows = Vec<Vec<Value>>;

/// Run `q` serial (lever off, no exec), then through the THREADED executor,
/// then through the inline `SerialExec` — all three must agree byte-for-byte.
fn all_three(g: &Graph, q: &str) -> (Rows, Rows, Rows) {
    g.set_parallel_expand(false);
    g.set_exec(None);
    let serial = rows(g, q);
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let threaded = rows(g, q);
    g.set_exec(Some(Arc::new(SerialExec)));
    let inline = rows(g, q);
    g.set_parallel_expand(false);
    g.set_exec(None);
    (serial, threaded, inline)
}

fn both(g: &Graph, q: &str) -> (Rows, Rows) {
    let (serial, threaded, inline) = all_three(g, q);
    assert_eq!(
        serial, inline,
        "the inline SerialExec must reproduce the serial path exactly: {q}"
    );
    (serial, threaded)
}

/// Whether the query, with the lever ON, actually drove the parallel expand —
/// so a green differential can't be a false pass over a path that never ran.
fn parallel_fired(g: &Graph, q: &str) -> bool {
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(SerialExec)));
    let (_, trace) = engram_observe::with_trace(|| rows(g, q));
    g.set_parallel_expand(false);
    g.set_exec(None);
    trace
        .counters()
        .get("interp.expand parallel")
        .copied()
        .unwrap_or(0)
        > 0
}

const TWO_HOP: &str = "MATCH (a:P)-[:R]->(b:P)-[:R]->(c:P) RETURN a.id, b.id, c.id";
const REL_VAR: &str = "MATCH (a:P)-[r:R]->(b:P) RETURN a.id, r.w, b.id";
const THREE_HOP: &str =
    "MATCH (a:P)-[:R]->(b:P)-[:R]->(c:P)-[:R]->(d:P) RETURN a.id, b.id, c.id, d.id";
const FILTERED: &str =
    "MATCH (a:P)-[:R]->(b:P)-[:R]->(c:P) WHERE b.id > 10 AND c.id < 30 RETURN a.id, b.id, c.id";
const ORDERED: &str = "MATCH (a:P)-[:R]->(b:P)-[:R]->(c:P) \
     RETURN a.id, b.id, c.id ORDER BY a.id DESC, b.id, c.id LIMIT 25";

#[test]
fn the_parallel_path_actually_fires() {
    // Canary the instrument: if the lever ON did not route through
    // expand_parallel, every equality below is vacuous.
    let g = graph();
    assert!(
        parallel_fired(&g, TWO_HOP),
        "two-hop must drive parallel expand"
    );
    assert!(parallel_fired(&g, REL_VAR), "rel-var hop must drive it too");
    assert!(parallel_fired(&g, THREE_HOP), "three-hop must drive it");
}

#[test]
fn parallel_equals_serial_byte_for_byte_including_order() {
    let g = graph();
    for q in [TWO_HOP, REL_VAR, THREE_HOP, FILTERED, ORDERED] {
        let (serial, parallel) = both(&g, q);
        assert!(!serial.is_empty(), "query produced rows to compare: {q}");
        assert_eq!(
            serial, parallel,
            "parallel expand diverged from serial (order included) on: {q}"
        );
    }
}

#[test]
fn a_rel_bound_expand_is_identical() {
    // The rel-var column is appended in lockstep with the peer column; a morsel
    // boundary must not desynchronise them.
    let g = graph();
    let (serial, parallel) = both(
        &g,
        "MATCH (a:P)-[r:R]->(b:P)-[s:R]->(c:P) RETURN a.id, r.w, b.id, s.w, c.id",
    );
    assert!(!serial.is_empty());
    assert_eq!(serial, parallel, "two rel-bound hops must stay in lockstep");
}

#[test]
fn an_empty_expansion_agrees() {
    // A pattern nobody satisfies: both paths return no rows (parallel must not
    // invent any, and the empty concat is still valid).
    let g = graph();
    let (serial, parallel) = both(&g, "MATCH (a:P)-[:NONE]->(b:P) RETURN a.id, b.id");
    assert!(serial.is_empty());
    assert_eq!(serial, parallel);
}

#[test]
fn no_installed_exec_means_the_serial_path_even_with_the_lever_on() {
    // The engine may not spawn — with nothing installed, the lever alone
    // must route serially and the parallel counter must NOT fire.
    let g = graph();
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(2);
    g.set_exec(None);
    let (r, trace) = engram_observe::with_trace(|| rows(&g, TWO_HOP));
    assert!(!r.is_empty());
    assert_eq!(
        trace.counters().get("interp.expand parallel").copied().unwrap_or(0),
        0,
        "exec-absent must decline: {:?}",
        trace.counters()
    );
    g.set_parallel_expand(false);
}

#[test]
fn a_transaction_declines_parallelism_and_still_sees_its_writes() {
    // Inside a transaction the overlays and the OCC read-set are
    // thread-local: the dispatch must take the serial path (counter silent)
    // and read-your-writes must hold.
    let g = graph();
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let txn = g.open_txn();
    let ((), trace) = engram_observe::with_trace(|| {
        let (txn_back, r) = g.with_txn(txn, || {
            let q = parse_statement(
                "CREATE (:P {id: 100})-[:R {w: 1}]->(:P {id: 101}) \
                 WITH 1 AS one MATCH (a:P)-[:R]->(b:P) RETURN a.id, b.id",
            )
            .expect("parse");
            run_query(&g, &q, BTreeMap::new()).map(|r| r.rows.len())
        });
        let n = r.expect("in-txn expand");
        g.rollback_owned(txn_back);
        // 40 people × 3 edges committed, plus the one buffered edge.
        assert_eq!(n, 121, "the buffered edge must be visible in-txn");
    });
    assert_eq!(
        trace.counters().get("interp.expand parallel").copied().unwrap_or(0),
        0,
        "a transaction must decline parallel expand: {:?}",
        trace.counters()
    );
    g.set_parallel_expand(false);
    g.set_exec(None);
}
