#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The adjacency GUARD row is written VISIBLE but not DURABLE.
//!
//! The guard (`'G' | node id`) exists so a relationship write and a node DELETE
//! are a write-write conflict in either commit order. Its content is never read
//! — `guard_row` appears only at its write sites and its own definition — so it
//! needs to reach the tail, where a concurrent validator sees it, but not the
//! commit log. After recovery there are no in-flight transactions, and an
//! absent guard is indistinguishable from a present one.
//!
//! **The line that must not be crossed:** a node delete's guard write stays a
//! real, LOGGED tombstone. RC1's put-vs-put exemption is sound only because the
//! second committer sees a non-put and aborts. Making the delete volatile too
//! would trade the dangling-edge guarantee for a throughput number, so these
//! tests assert the guarantee on BOTH lever arms — if it held only on one, the
//! lever would be selling isolation.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, GraphError, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph_over(store: Store) -> Graph {
    Graph::new(store, Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn buffered(g: &Graph, src: &str) -> engram_graph::GraphTxn {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    r.unwrap_or_else(|e| panic!("txn `{src}`: {e}"));
    txn
}

fn count(t: &engram_observe::Trace, k: &str) -> u64 {
    t.counters().get(k).copied().unwrap_or(0)
}

/// Run a statement AS THE SERVER DOES: inside an autocommit transaction.
///
/// This matters more than it looks. A bare `run_query` autocommits every store
/// write SEPARATELY, so `CREATE (a)-[:R]->(b)` is not one atomic statement but
/// nine independent commits — a concurrent delete can interleave between the
/// relationship record and its guard rows and strand an edge, on today's code,
/// with no lever involved. The first version of the hammer below did exactly
/// that and "found" 2 dangling edges on the CONTROL arm.
///
/// Statement atomicity comes from the enclosing transaction. The Bolt loop
/// opens one per statement; a test that wants to measure the write path has to
/// do the same or it is measuring a shape production never runs.
fn stmt(g: &Graph, src: &str) -> Result<(), String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g.commit_owned(txn).map_err(|e| format!("{e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            Err(format!("{e:?}"))
        }
    }
}

fn seed(g: &Graph) {
    run(g, "CREATE (:P {id: 1})");
    run(g, "CREATE (:P {id: 2})");
}

/// THE guarantee, on both arms: a relationship create and a node delete on the
/// same endpoint conflict, in EITHER commit order. This is what the guard is
/// for, and a volatile guard must not weaken it.
#[test]
fn create_versus_delete_conflicts_in_either_order_on_both_arms() {
    for on in [false, true] {
        // Order A: the create commits first.
        {
            let g = graph_over(Store::new());
            g.set_volatile_guards(on);
            seed(&g);
            let create = buffered(&g, "MATCH (a:P {id: 1}), (b:P {id: 2}) CREATE (a)-[:R]->(b)");
            let del = buffered(&g, "MATCH (n:P {id: 1}) DELETE n");
            g.commit_owned(create).expect("first committer wins");
            assert!(
                matches!(g.commit_owned(del), Err(GraphError::TxnConflict)),
                "arm on={on}, order A: a node delete must abort against a \
                 relationship write it did not see"
            );
            assert!(
                g.verify_rel_endpoints().expect("fsck").is_empty(),
                "arm on={on}, order A: FSCK found a dangling edge"
            );
        }
        // Order B: the delete commits first.
        {
            let g = graph_over(Store::new());
            g.set_volatile_guards(on);
            seed(&g);
            let create = buffered(&g, "MATCH (a:P {id: 1}), (b:P {id: 2}) CREATE (a)-[:R]->(b)");
            let del = buffered(&g, "MATCH (n:P {id: 1}) DELETE n");
            g.commit_owned(del).expect("first committer wins");
            assert!(
                matches!(g.commit_owned(create), Err(GraphError::TxnConflict)),
                "arm on={on}, order B: a relationship write must abort against \
                 a node delete it did not see — this is the direction that \
                 would otherwise leave a DANGLING EDGE"
            );
            assert!(
                g.verify_rel_endpoints().expect("fsck").is_empty(),
                "arm on={on}, order B: FSCK found a dangling edge"
            );
        }
    }
}

/// Two relationship writes touching ONE node must still commit together — the
/// RC1 exemption, which the volatile path must not disturb.
#[test]
fn two_relationship_writes_on_one_node_still_both_commit() {
    for on in [false, true] {
        let g = graph_over(Store::new());
        g.set_volatile_guards(on);
        run(&g, "CREATE (:P {id: 0})");
        for i in 1..=4i64 {
            run(&g, &format!("CREATE (:P {{id: {i}}})"));
        }
        let a = buffered(&g, "MATCH (h:P {id: 0}), (x:P {id: 1}) CREATE (h)-[:R]->(x)");
        let b = buffered(&g, "MATCH (h:P {id: 0}), (x:P {id: 2}) CREATE (h)-[:R]->(x)");
        g.commit_owned(a).expect("a");
        g.commit_owned(b)
            .expect("arm: two relationship writes on one node are both valid");
        assert_eq!(
            rows(&g, "MATCH ()-[r:R]->() RETURN r").len(),
            2,
            "arm on={on}: both relationships must exist"
        );
    }
}

/// A concurrent hammer, ending in the FSCK. The guarantee is not "the two
/// specific orders above" — it is that no interleaving strands an edge.
#[test]
fn a_concurrent_hammer_leaves_no_dangling_edge_on_both_arms() {
    use std::sync::Arc;
    for on in [false, true] {
        let g = Arc::new(graph_over(Store::new()));
        g.set_volatile_guards(on);
        for i in 0..40i64 {
            run(&g, &format!("CREATE (:P {{id: {i}}})"));
        }
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let g = Arc::clone(&g);
                std::thread::spawn(move || {
                    for i in 0..20i64 {
                        let a = (w * 7 + i) % 40;
                        let b = (a + 1) % 40;
                        let q = format!(
                            "MATCH (x:P {{id: {a}}}), (y:P {{id: {b}}}) CREATE (x)-[:R]->(y)"
                        );
                        let _ = stmt(&g, &q);
                    }
                })
            })
            .collect();
        let deleters: Vec<_> = (0..2)
            .map(|d| {
                let g = Arc::clone(&g);
                std::thread::spawn(move || {
                    for i in 0..10i64 {
                        let n = (d * 13 + i * 3) % 40;
                        let q = format!("MATCH (n:P {{id: {n}}}) DETACH DELETE n");
                        let _ = stmt(&g, &q);
                    }
                })
            })
            .collect();
        for t in writers {
            t.join().expect("writer");
        }
        for t in deleters {
            t.join().expect("deleter");
        }
        let dangling = g.verify_rel_endpoints().expect("fsck");
        assert!(
            dangling.is_empty(),
            "arm on={on}: {} dangling edge(s) after a concurrent hammer — the \
             guard row's whole purpose is that this is empty",
            dangling.len()
        );
    }
}

/// Non-vacuity: the ON arm really writes guards volatile, the OFF arm does not.
#[test]
fn the_lever_actually_switches_the_write() {
    let g = graph_over(Store::new());
    g.set_volatile_guards(true);
    seed(&g);
    let (_, on) = engram_observe::with_trace(|| {
        stmt(&g, "MATCH (a:P {id: 1}), (b:P {id: 2}) CREATE (a)-[:R]->(b)").expect("create");
    });
    assert!(
        count(&on, "graph.guard rows written volatile") >= 2,
        "a relationship create writes TWO guards (src and dst), counters: {:?}",
        on.counters()
    );

    let g2 = graph_over(Store::new());
    g2.set_volatile_guards(false);
    seed(&g2);
    let (_, off) = engram_observe::with_trace(|| {
        stmt(&g2, "MATCH (a:P {id: 1}), (b:P {id: 2}) CREATE (a)-[:R]->(b)").expect("create");
    });
    assert_eq!(
        count(&off, "graph.guard rows written volatile"),
        0,
        "OFF arm must log its guards — it is the differential's control"
    );
}

/// The saving is real: a volatile guard produces no LOG entry, while everything
/// else the statement writes still does.
#[test]
fn a_volatile_guard_produces_no_log_entry() {
    let measure = |on: bool| -> u64 {
        let store = Store::new();
        let g = graph_over(store.clone());
        g.set_volatile_guards(on);
        seed(&g);
        let before = store.log_len();
        stmt(&g, "MATCH (a:P {id: 1}), (b:P {id: 2}) CREATE (a)-[:R]->(b)").expect("create");
        store.log_len() - before
    };
    let with = measure(true);
    let without = measure(false);
    eprintln!("[volatile guards] log entries for one rel create: volatile {with}, logged {without}");
    assert!(
        with < without,
        "a volatile guard must not reach the log: {with} vs {without}"
    );
    assert_eq!(
        without - with,
        2,
        "exactly the two guard rows (src and dst) should disappear from the log"
    );
}

/// After a CRASH and reopen, a store whose guards were never logged is still
/// correct — and a create/delete race against it still conflicts.
///
/// This is the argument for why the guard needs visibility but not durability,
/// executed rather than asserted. After recovery there are no in-flight
/// transactions for a guard to conflict with, so an absent guard is
/// indistinguishable from a present one: the next `create_rel` puts it, and the
/// next `delete_node` tombstones a key that may not exist, which is harmless.
#[test]
fn a_reopened_store_is_correct_and_still_races_correctly() {
    let path = {
        let mut p = std::env::temp_dir();
        p.push(format!("engram-volatile-guard-{}.wal", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    };
    // Build a graph with volatile guards, then drop it — the stand-in for kill -9.
    {
        let store = Store::open_wal(&path).expect("open wal");
        let g = graph_over(store);
        g.set_volatile_guards(true);
        for i in 0..12i64 {
            stmt(&g, &format!("CREATE (:P {{id: {i}}})")).expect("create");
        }
        for i in 0..6i64 {
            let b = i + 6;
            stmt(
                &g,
                &format!("MATCH (x:P {{id: {i}}}), (y:P {{id: {b}}}) CREATE (x)-[:R]->(y)"),
            )
            .expect("rel");
        }
    }

    // Reopen from the WAL. The guard rows were never written to it.
    let store = Store::open_wal(&path).expect("reopen");
    let g = graph_over(store);
    g.set_volatile_guards(true);
    assert_eq!(
        rows(&g, "MATCH ()-[r:R]->() RETURN r").len(),
        6,
        "every relationship must survive: the guards were elided, not the data"
    );
    assert!(
        g.verify_rel_endpoints().expect("fsck").is_empty(),
        "a recovered store with no guard rows must still be structurally sound \
         — the guard is a CONCURRENCY device, not a referential-integrity record"
    );

    // And the race still works on the recovered store, in both orders.
    let create = buffered(&g, "MATCH (a:P {id: 0}), (b:P {id: 7}) CREATE (a)-[:R]->(b)");
    let del = buffered(&g, "MATCH (n:P {id: 0}) DETACH DELETE n");
    g.commit_owned(del).expect("first committer wins");
    assert!(
        matches!(g.commit_owned(create), Err(GraphError::TxnConflict)),
        "after recovery the guard must be re-established by the write itself, \
         so a create still aborts against a delete it did not see"
    );
    assert!(g.verify_rel_endpoints().expect("fsck").is_empty());
    let _ = std::fs::remove_file(&path);
}
