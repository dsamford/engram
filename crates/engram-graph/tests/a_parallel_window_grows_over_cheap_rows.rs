//! A parallel window GROWS over rows that are cheap, and stays small over rows
//! that expand.
//!
//! A window is a round trip through the executor — its threads are spawned
//! for it and joined after it — and 64 rows per worker was sized for rows
//! that EXPAND. SNB BI bi15's edge-centric weights seed 22M comments at SF10,
//! each yielding one reply row: ~8,600 seed windows and as many continuation
//! windows, and one 900 s step spent 3,642 CPU-seconds in the kernel. A window
//! now doubles after one that produced few rows per input.
//!
//! DIFFERENTIAL: every answer is the serial loop's; over cheap rows the
//! executor is entered a fraction of the times a fixed window would; and a
//! continuation that EXPANDS never grows its window — the memory bound the
//! fixed window existed for.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A 4-wide executor that counts how often it is entered.
struct CountingExec {
    calls: AtomicUsize,
}

impl ScopedExec for CountingExec {
    fn width(&self) -> usize {
        4
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4.min(n).max(1) {
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

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const SEED_GREW: &str = "interp.seed window grew: its rows were cheap";
const ROW_GREW: &str = "interp.row window grew: its rows were cheap";

/// 20,000 items, each with ONE leaf: every seed and every continuation row
/// yields exactly one row.
fn cheap() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 19999) AS i CREATE (:Item {id: i})-[:NEXT]->(:Leaf {id: i})");
    let _ = g.warm();
    g
}

const CHEAP: &str = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Leaf) \
     OPTIONAL MATCH (b)<-[:NEXT]-(c) \
     WITH b.id AS k, c IS NULL AS none \
     RETURN count(*) AS n, sum(k) AS s, sum(CASE WHEN none THEN 1 ELSE 0 END) AS u";

#[test]
fn a_window_grows_over_cheap_rows_and_answers_the_same() {
    let serial = cheap();
    let (want, _) = run(&serial, CHEAP);
    assert_eq!(
        want,
        vec![vec![Value::Int(20_000), Value::Int((0..20_000).sum()), Value::Int(0)]],
        "the serial loop's own answer"
    );
    let g = cheap();
    let exec = Arc::new(CountingExec { calls: AtomicUsize::new(0) });
    g.set_exec(Some(exec.clone()));
    let (got, c) = run(&g, CHEAP);
    assert_eq!(got, want, "growing the window changed the answer");
    assert!(
        get(&c, SEED_GREW) > 0 && get(&c, ROW_GREW) > 0,
        "neither window grew over one row per input: {c:?}"
    );
    // fixed windows of 64 x 4 would enter the executor ~79 times for the
    // seeds and ~79 for the continuation
    let calls = exec.calls.load(Ordering::Relaxed);
    assert!(calls < 50, "the executor was entered {calls} times");
}

/// 500 hubs of 100 spokes: the CONTINUATION yields 100 rows per input, so its
/// window must not grow — the buffered output is what the window bounds.
#[test]
fn a_continuation_that_expands_keeps_its_window() {
    let build = || {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        ddl(
            &g,
            "UNWIND range(0, 499) AS h CREATE (hub:Hub {id: h}) \
             WITH hub UNWIND range(0, 99) AS k CREATE (hub)-[:SPOKE]->(:Rim {k: k})",
        );
        let _ = g.warm();
        g
    };
    let q = "MATCH (h:Hub) MATCH (h)-[:SPOKE]->(r:Rim) \
         OPTIONAL MATCH (r)<-[:SPOKE]-(x) \
         WITH r.k AS k, x IS NULL AS none \
         RETURN count(*) AS n, sum(k) AS s, sum(CASE WHEN none THEN 1 ELSE 0 END) AS u";
    let (want, _) = run(&build(), q);
    assert_eq!(
        want,
        vec![vec![Value::Int(50_000), Value::Int(500 * (0..100).sum::<i64>()), Value::Int(0)]],
        "the serial loop's own answer"
    );
    let g = build();
    g.set_exec(Some(Arc::new(CountingExec { calls: AtomicUsize::new(0) })));
    let (got, c) = run(&g, q);
    assert_eq!(got, want, "the expanding continuation changed its answer");
    assert_eq!(get(&c, ROW_GREW), 0, "a window of expanding rows grew: {c:?}");
}
