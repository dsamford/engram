//! An algorithm's stream reads a vertex's RECORD only if the statement asks
//! for its node.
//!
//! `engram.algo.*.stream` builds one row per vertex carrying `nodeId`, `node`,
//! the algorithm's value and `asOf`. It materialised the `node` — a store get
//! and a full record decode — for every vertex, whether or not the `YIELD`
//! named it. `YIELD depth RETURN count(*)` never looks at a node.
//!
//! Measured on the SF3 friendship graph (24,328 people, 1.13M friendships):
//! the BFS kernel is O(V+E) and the projection is reused after the first call,
//! yet a cold BFS took 35 s. Its counters name the reason — 24,328
//! `graph.nodes materialised in full`, 343,686 block reads against 326,771
//! evictions — and the WARM run did exactly the same work in 0 s, because by
//! then the blocks were resident. That is why this hid for so long: the reads
//! are free when the cache already holds them, and the first call pays for
//! all of them.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const MATERIALISED: &str = "graph.nodes materialised in full";
const SKIPPED: &str = "algo.stream skipped the node column nobody yielded";

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// A ring of people, so every vertex is reachable and the answer is a known
/// function of `n` rather than of the layout.
fn ring(n: i64) -> (Graph, u64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(i));
        // a fat property, so a needless record read is a real read
        m.insert("pad".to_string(), Value::Str("x".repeat(256)));
        ids.push(g.create_node(&["Person".into()], &m).expect("node"));
    }
    for i in 0..n as usize {
        g.create_rel(ids[i], "KNOWS", ids[(i + 1) % n as usize], &BTreeMap::new())
            .expect("knows");
    }
    let _ = g.warm();
    (g, ids[0])
}

fn bfs(src: u64, yields: &str, tail: &str) -> String {
    format!(
        "CALL engram.algo.bfs.stream({{nodeLabels: ['Person'], \
         relationshipTypes: ['KNOWS'], orientation: 'UNDIRECTED', \
         sourceNode: {src}}}) YIELD {yields} {tail}"
    )
}

#[test]
fn a_stream_that_yields_no_node_reads_no_record() {
    let (g, src) = ring(200);
    let (rows, t) = engram_observe::with_trace(|| {
        run(
            &g,
            &bfs(src, "depth", "RETURN count(*) AS n, max(depth) AS d"),
        )
    });
    assert_eq!(
        rows,
        vec![vec![Value::Int(200), Value::Int(100)]],
        "{rows:?}"
    );
    assert_eq!(
        counter(&t, MATERIALISED),
        0,
        "the stream decoded records for nodes nobody yielded: {:?}",
        t.counters()
    );
    assert!(counter(&t, SKIPPED) >= 1, "{:?}", t.counters());
}

#[test]
fn a_stream_that_yields_the_node_still_gets_it() {
    // THE CONTROL, and the thing that must not break: the column is not
    // removed, it is deferred to demand.
    let (g, src) = ring(200);
    let rows = run(
        &g,
        &bfs(
            src,
            "node, depth",
            "WITH node, depth WHERE depth = 0 RETURN node.k AS k",
        ),
    );
    assert_eq!(
        rows,
        vec![vec![Value::Int(0)]],
        "the source's own row: {rows:?}"
    );
}

#[test]
fn the_answer_does_not_depend_on_what_is_yielded() {
    // A read that is skipped must change the COST and nothing else.
    let (g, src) = ring(200);
    let with_node = run(
        &g,
        &bfs(src, "node, depth", "RETURN count(*) AS n, max(depth) AS d"),
    );
    let without = run(
        &g,
        &bfs(src, "depth", "RETURN count(*) AS n, max(depth) AS d"),
    );
    assert_eq!(with_node, without);
}

#[test]
fn a_bare_call_still_binds_every_column() {
    // An empty YIELD binds every declared column, `node` included — so a bare
    // CALL must still carry its nodes, and this is where the demand test could
    // silently go wrong.
    let (g, src) = ring(50);
    let (rows, t) = engram_observe::with_trace(|| {
        run(
            &g,
            &bfs(
                src,
                "node, nodeId, depth",
                "WITH node, depth WHERE depth = 1 RETURN count(node) AS n",
            ),
        )
    });
    assert_eq!(
        rows,
        vec![vec![Value::Int(2)]],
        "both ring neighbours: {rows:?}"
    );
    assert!(
        counter(&t, MATERIALISED) > 0,
        "the nodes were yielded but never read: {:?}",
        t.counters()
    );
}
