//! The frontier walk (`set_frontier_expand(true)`, the default) answers every
//! variable-length query it takes exactly as the enumeration does
//! (`set_frontier_expand(false)`), on the pipeline and on the general path.
//!
//! Two wrong answers it gave, found 2026-09-24 by probe on the unchanged
//! source, both admitted because the gate asked only how the walk's END was
//! consumed:
//!  * `count(*)` beside `count(DISTINCT n)`: one row per REACHED node against
//!    one per WALK — `MATCH (a)-[:T*1..2]->(n) RETURN count(DISTINCT n),
//!    count(*)` answered 3, 3 where two walks reach `b` and the answer is 3, 4;
//!  * an UNDIRECTED walk re-reaching its start through the edge it left by —
//!    `MATCH (p)-[:KNOWS*1..2]-(f) RETURN count(DISTINCT f)` counted `p` among
//!    its own friends (2 against 1). The LDBC queries hide it behind
//!    `NOT friend = person`; a query without one did not.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The test-lane threaded executor — the server's shape.
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

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const PIPELINE_WALK: &str = "interp.pipeline var-length BFS ran";
const RETURNED: &str = "interp.undirected frontier walk returned to its start along a cycle";

/// Every combination of columnar (pipeline or general path) against the
/// enumeration; the frontier-on answers must equal it. Returns the columnar
/// run's counters.
fn agrees(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    g.set_frontier_expand(false);
    g.set_columnar_scans(false);
    let (want, _) = run(g, q);
    g.set_frontier_expand(true);
    let (general, _) = run(g, q);
    g.set_columnar_scans(true);
    let (piped, c) = run(g, q);
    let sorted = |mut v: Vec<Vec<Value>>| {
        v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        v
    };
    assert_eq!(sorted(general), sorted(want.clone()), "general path: {q}");
    assert_eq!(sorted(piped), sorted(want), "pipeline: {q}");
    c
}

#[test]
fn a_breaker_that_counts_rows_is_not_given_a_walk() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // a -> x -> b and a -> y -> b: two walks reach b.
    ddl(
        &g,
        "CREATE (a:A {id: 1}), (x:N {id: 2}), (y:N {id: 3}), (b:N {id: 4}), \
         (a)-[:T]->(x), (a)-[:T]->(y), (x)-[:T]->(b), (y)-[:T]->(b)",
    );
    let _ = g.warm();
    for q in [
        "MATCH (a:A)-[:T*1..2]->(n:N) RETURN count(DISTINCT n) AS d, count(*) AS c",
        "MATCH (a:A)-[:T*1..2]->(n:N) RETURN a.id AS id, count(DISTINCT n) AS d, count(*) AS c",
        "MATCH (a:A)-[:T*1..2]->(n:N) RETURN count(DISTINCT n) AS d, sum(a.id) AS s",
    ] {
        let c = agrees(&g, q);
        assert_eq!(counter(&c, PIPELINE_WALK), 0, "a counting breaker took the walk: {q}");
    }
    // A set-semantic breaker keeps the walk — for this sole `*1..max` hop, with
    // its end consumed DISTINCT-only, which is when the general path walks it
    // too (`max(n.id)` would be a second use of the end, and both enumerate).
    let c = agrees(&g, "MATCH (a:A)-[:T*1..2]->(n:N) RETURN count(DISTINCT n) AS d, max(a.id) AS m");
    assert!(counter(&c, PIPELINE_WALK) > 0, "a set-semantic breaker lost the walk: {c:?}");
    let c = agrees(&g, "MATCH (a:A)-[:T*1..2]->(n:N) RETURN count(DISTINCT n) AS d, max(n.id) AS m");
    assert_eq!(counter(&c, PIPELINE_WALK), 0, "the end used twice took the walk: {c:?}");
}

#[test]
fn an_undirected_walk_returns_to_its_start_only_along_a_cycle() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // p - q: one edge, no cycle.
    ddl(&g, "CREATE (:P {id: 10})-[:KNOWS]->(:Q {id: 11})");
    // s on a triangle s - t - u - s.
    ddl(
        &g,
        "CREATE (s:S {id: 20}), (t:Q {id: 21}), (u:Q {id: 22}), \
         (s)-[:KNOWS]->(t), (t)-[:KNOWS]->(u), (u)-[:KNOWS]->(s)",
    );
    // v - w twice: a cycle of two parallel edges.
    ddl(
        &g,
        "CREATE (v:V {id: 30}), (w:Q {id: 31}), (v)-[:KNOWS]->(w), (w)-[:KNOWS]->(v)",
    );
    // z with a self-loop.
    ddl(&g, "CREATE (z:Z {id: 40}), (z)-[:KNOWS]->(z)");
    let _ = g.warm();
    for (q, returns) in [
        ("MATCH (p:P)-[:KNOWS*1..2]-(f) RETURN count(DISTINCT f) AS d", false),
        ("MATCH (p:P)-[:KNOWS*1..2]-(f) RETURN DISTINCT f.id AS id", false),
        // the triangle is three long: out of reach at two, in reach at three
        ("MATCH (s:S)-[:KNOWS*1..2]-(f) RETURN count(DISTINCT f) AS d", false),
        ("MATCH (s:S)-[:KNOWS*1..3]-(f) RETURN count(DISTINCT f) AS d", true),
        ("MATCH (s:S)-[:KNOWS*1..3]-(f) RETURN DISTINCT f.id AS id", true),
        ("MATCH (v:V)-[:KNOWS*1..2]-(f) RETURN count(DISTINCT f) AS d", true),
        ("MATCH (z:Z)-[:KNOWS*1..1]-(f) RETURN count(DISTINCT f) AS d", true),
        // IC9's shape: its own filter removes the start either way
        (
            "MATCH (s:S)-[:KNOWS*1..3]-(f) WHERE NOT f = s RETURN count(DISTINCT f) AS d",
            true,
        ),
    ] {
        let c = agrees(&g, q);
        assert!(counter(&c, PIPELINE_WALK) > 0, "vacuous: the walk did not run: {q}\n{c:?}");
        assert_eq!(counter(&c, RETURNED) > 0, returns, "the start's return: {q}\n{c:?}");
    }
}

/// The same rule with the walk's driving rows split across the executor: every
/// `:Q` node walks, the triangle's two among them return along it at `*1..3`,
/// the parallel pair's `w` at `*1..2`.
#[test]
fn a_walk_split_across_the_executor_returns_to_its_start_alike() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE (:P {id: 10})-[:KNOWS]->(:Q {id: 11})");
    ddl(
        &g,
        "CREATE (s:S {id: 20}), (t:Q {id: 21}), (u:Q {id: 22}), \
         (s)-[:KNOWS]->(t), (t)-[:KNOWS]->(u), (u)-[:KNOWS]->(s)",
    );
    ddl(
        &g,
        "CREATE (v:V {id: 30}), (w:Q {id: 31}), (v)-[:KNOWS]->(w), (w)-[:KNOWS]->(v)",
    );
    let _ = g.warm();
    g.set_exec(Some(Arc::new(TestExec(4))));
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(2);
    for q in [
        "MATCH (x:Q)-[:KNOWS*1..2]-(f) RETURN x.id AS x, count(DISTINCT f) AS d ORDER BY x",
        "MATCH (x:Q)-[:KNOWS*1..3]-(f) RETURN x.id AS x, count(DISTINCT f) AS d ORDER BY x",
    ] {
        // The answers are held to the enumeration inside `agrees`; the start's
        // return is counted on the WORKER threads, which this thread's trace
        // does not see (the serial test above pins that counter).
        let c = agrees(&g, q);
        assert!(
            counter(&c, "interp.pipeline var-length BFS parallel") > 0,
            "the walk did not split: {q}\n{c:?}"
        );
    }
    g.set_exec(None);
}
