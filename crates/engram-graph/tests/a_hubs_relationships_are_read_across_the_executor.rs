//! A node's many relationships are read across the executor: one contiguous
//! run of their records per worker, the runs concatenated in order, so the
//! walk sees them exactly as one thread reads them. SNB Interactive IS3 read
//! a person's 1,190 KNOWS records at ~4 µs each on one thread — near half of
//! its engine time. Never inside a transaction: a worker cannot see the
//! calling thread's buffered writes.
//!
//! IS3 itself reads no record any more: an undirected hop whose relationship
//! is read by property binds it lean, its properties in one batch
//! (`an_undirected_hop_binds_its_relationship_lean.rs`). The split serves a
//! walk that needs the WHOLE relationship, which is what these statements
//! return.

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

const SPLIT: &str = "graph.relationship reads split across the executor";

/// A hub with 1,500 friends through KNOWS — half pointing in, half out — each
/// relationship dated; and a quiet node with 20.
fn hub() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE (:Person {id: 0, firstName: 'hub'}), (:Person {id: 1, firstName: 'quiet'})");
    ddl(
        &g,
        "UNWIND range(1, 1500) AS i CREATE (:Person {id: 100 + i, firstName: 'f' + toString(i)})",
    );
    ddl(
        &g,
        "MATCH (h:Person {id: 0}), (f:Person) WHERE f.id > 100 AND f.id % 2 = 0 \
         CREATE (h)-[:KNOWS {creationDate: ((f.id - 100) * 7919) % 1511}]->(f)",
    );
    ddl(
        &g,
        "MATCH (h:Person {id: 0}), (f:Person) WHERE f.id > 100 AND f.id % 2 = 1 \
         CREATE (f)-[:KNOWS {creationDate: ((f.id - 100) * 7919) % 1511}]->(h)",
    );
    ddl(
        &g,
        "MATCH (q:Person {id: 1}) UNWIND range(1, 20) AS i \
         CREATE (q)-[:KNOWS {creationDate: i}]->(:Person {id: 5000 + i, firstName: 'q'})",
    );
    let _ = g.warm();
    g
}

/// IS3's walk over person `p`, returning the relationship whole (the split's
/// case); `is3` below is the benchmark's own text, which binds it lean.
fn whole(p: i64) -> String {
    format!(
        "MATCH (n:Person {{id: {p}}})-[r:KNOWS]-(friend) \
         RETURN friend.id AS personId, r AS r ORDER BY personId ASC"
    )
}

/// IS3's shape over person `p`.
fn is3(p: i64) -> String {
    format!(
        "MATCH (n:Person {{id: {p}}})-[r:KNOWS]-(friend) \
         RETURN friend.id AS personId, friend.firstName AS firstName, r.creationDate AS d \
         ORDER BY d DESC, personId ASC"
    )
}

#[test]
fn a_hubs_relationships_are_read_across_the_executor_in_their_order() {
    let g = hub();
    for (q, columnar) in [
        (whole(0), true),
        // the records themselves in the walk's own order — no ORDER BY — with
        // the columnar paths off, so the matcher's walk (not the pipeline's
        // hop) is what answers, and its order is the answer's order
        ("MATCH (n:Person {id: 0})-[r:KNOWS]-(friend) RETURN r AS r".to_string(), false),
    ] {
        g.set_columnar_scans(columnar);
        g.set_exec(None);
        let (want, serial) = run(&g, &q);
        assert_eq!(counter(&serial, SPLIT), 0, "{serial:?}");
        g.set_exec(Some(Arc::new(TestExec(4))));
        let (got, c) = run(&g, &q);
        g.set_exec(None);
        assert_eq!(got, want, "`{q}`");
        assert_eq!(want.len(), 1500, "`{q}`");
        assert!(counter(&c, SPLIT) > 0, "`{q}` read its relationships on one thread: {c:?}");
    }
    g.set_columnar_scans(true);
}

#[test]
fn a_few_relationships_and_a_transaction_are_read_here() {
    let g = hub();
    g.set_exec(Some(Arc::new(TestExec(4))));
    // twenty: below the split floor
    let (got, c) = run(&g, &is3(1));
    assert_eq!(got.len(), 20);
    assert_eq!(counter(&c, SPLIT), 0, "{c:?}");
    // a writing statement inside its transaction, as the server runs one,
    // reads its relationships here: never split
    let q = parse_statement(
        "MATCH (n:Person {id: 0})-[r:KNOWS]-(friend) SET r.seen = true RETURN count(r) AS n",
    )
    .expect("parse");
    let txn = g.open_txn();
    let (txn, (out, t)) =
        g.with_txn(txn, || engram_observe::with_trace(|| run_query(&g, &q, BTreeMap::new())));
    let out = out.expect("the write runs");
    g.commit_owned(txn).expect("the write commits");
    assert_eq!(out.rows, vec![vec![Value::Int(1500)]]);
    assert_eq!(t.counters().get(SPLIT).copied().unwrap_or(0), 0, "a transaction split its reads");
    // and every relationship took the write
    let (seen, _) = run(
        &g,
        "MATCH (n:Person {id: 0})-[r:KNOWS]-() WHERE r.seen = true RETURN count(r) AS n",
    );
    g.set_exec(None);
    assert_eq!(seen, vec![vec![Value::Int(1500)]]);
}
