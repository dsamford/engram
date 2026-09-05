//! The negative canary for the guard put-vs-put exemption (RC1 / O3).
//!
//! The exemption lets two relationship writes touching one node commit
//! without aborting each other. It is the highest-risk item in the
//! write-concurrency programme because it relaxes a conflict the guard row
//! exists to force, and the failure it could cause is silent: a relationship
//! whose endpoint no longer exists. `docs/write-concurrency-ceiling.md` made
//! it conditional on this file existing.
//!
//! The guarantee, restated: a relationship write and a node DELETE must
//! conflict in EITHER commit order. The exemption fires only when the
//! committed version AND our own intent are both PUTs, and `delete_node`
//! writes a TOMBSTONE — so whichever side commits second fails one of the two
//! tests. These tests assert that on every ordering, and the last one hammers
//! it concurrently and then runs the FSCK.
//!
//! Every test here must also pass with the exemption OFF: the guarantee is
//! not supposed to depend on the lever, only the cost is.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::parse_statement;
use engram_graph::{Graph, GraphError, GraphTxn, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, q: &str) {
    let stmt = parse_statement(q).expect("parse");
    run_query(g, &stmt, BTreeMap::new()).expect("run");
}

/// Buffer a statement into its own transaction without committing it.
fn buffered(g: &Graph, q: &str) -> GraphTxn {
    let stmt = parse_statement(q).expect("parse");
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &stmt, BTreeMap::new()));
    r.expect("statement runs");
    txn
}

fn seeded(exempt: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_guard_put_put_exempt(exempt);
    run(&g, "CREATE (:Hub {id: 1}), (:Sat {id: 2})");
    g
}

#[test]
fn a_create_racing_a_node_delete_conflicts_in_both_orders_on_both_arms() {
    for exempt in [true, false] {
        // Order 1: the DELETE commits first.
        let g = seeded(exempt);
        let create = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 2}) CREATE (s)-[:R]->(h)");
        let del = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
        g.commit_owned(del).expect("delete commits");
        let e = g
            .commit_owned(create)
            .expect_err("exempt={exempt}: a create onto a DELETED node must abort");
        assert!(matches!(e, GraphError::TxnConflict), "exempt={exempt}");
        assert_eq!(
            g.verify_rel_endpoints().expect("fsck"),
            Vec::<u64>::new(),
            "exempt={exempt}: no dangling edge"
        );

        // Order 2: the CREATE commits first.
        let g = seeded(exempt);
        let create = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 2}) CREATE (s)-[:R]->(h)");
        let del = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
        g.commit_owned(create).expect("create commits");
        let e = g
            .commit_owned(del)
            .expect_err("exempt={exempt}: a delete of a node that just gained an edge must abort");
        assert!(matches!(e, GraphError::TxnConflict), "exempt={exempt}");
        assert_eq!(
            g.verify_rel_endpoints().expect("fsck"),
            Vec::<u64>::new(),
            "exempt={exempt}: no dangling edge"
        );
    }
}

#[test]
fn a_rel_delete_racing_a_node_delete_still_conflicts_on_both_arms() {
    // `delete_rel` PUTS both guards and `delete_node` writes a TOMBSTONE, so
    // this pair must conflict even though one side is a put.
    for exempt in [true, false] {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        g.set_guard_put_put_exempt(exempt);
        run(&g, "CREATE (:Sat {id: 2})-[:R]->(:Hub {id: 1})");
        let del_rel = buffered(&g, "MATCH (:Sat {id: 2})-[r:R]->(:Hub {id: 1}) DELETE r");
        let del_hub = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
        g.commit_owned(del_rel).expect("rel delete commits");
        let e = g
            .commit_owned(del_hub)
            .expect_err("exempt={exempt}: the node delete must still abort");
        assert!(matches!(e, GraphError::TxnConflict), "exempt={exempt}");
    }
}

#[test]
fn concurrent_creates_and_node_deletes_never_leave_a_dangling_edge() {
    // The hammer. N threads create relationships onto one hub while another
    // deletes and recreates it. Losers re-run, as the Bolt loop would. The
    // bar is the FSCK: whatever the interleaving, no relationship may name an
    // endpoint that does not exist.
    for exempt in [true, false] {
        let g = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
        g.set_guard_put_put_exempt(exempt);
        run(&g, "CREATE (:Hub {id: 1})");
        for i in 0..64 {
            run(&g, &format!("CREATE (:Sat {{id: {i}}})"));
        }

        // Successful create COMMITS, not survivors: the deleter's
        // `DETACH DELETE` removes the hub's relationships, so counting what
        // is left at the end would depend on which thread happened to finish
        // last. What must be non-zero is the work that actually raced.
        let committed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for t in 0..6u64 {
            let g = Arc::clone(&g);
            let committed = Arc::clone(&committed);
            handles.push(std::thread::spawn(move || {
                for i in 0..24u64 {
                    let sat = (t * 24 + i) % 64;
                    let q = format!(
                        "MATCH (h:Hub {{id: 1}}), (s:Sat {{id: {sat}}}) CREATE (s)-[:R]->(h)"
                    );
                    // Re-run on conflict, exactly as the server's autocommit
                    // loop does; a create onto a deleted hub simply finds
                    // nothing to match and is a no-op.
                    for _ in 0..64 {
                        let stmt = parse_statement(&q).expect("parse");
                        let txn = g.open_txn();
                        let (txn, r) = g.with_txn(txn, || run_query(&g, &stmt, BTreeMap::new()));
                        if r.is_err() {
                            break;
                        }
                        match g.commit_owned(txn) {
                            Ok(()) => {
                                committed.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            Err(GraphError::TxnConflict) => continue,
                            Err(_) => break,
                        }
                    }
                }
            }));
        }
        let gd = Arc::clone(&g);
        handles.push(std::thread::spawn(move || {
            for _ in 0..8 {
                for _ in 0..64 {
                    let stmt = parse_statement("MATCH (h:Hub {id: 1}) DETACH DELETE h")
                        .expect("parse");
                    let txn = gd.open_txn();
                    let (txn, r) = gd.with_txn(txn, || run_query(&gd, &stmt, BTreeMap::new()));
                    if r.is_err() {
                        break;
                    }
                    match gd.commit_owned(txn) {
                        Ok(()) => break,
                        Err(GraphError::TxnConflict) => continue,
                        Err(_) => break,
                    }
                }
                let stmt = parse_statement("CREATE (:Hub {id: 1})").expect("parse");
                let txn = gd.open_txn();
                let (txn, r) = gd.with_txn(txn, || run_query(&gd, &stmt, BTreeMap::new()));
                if r.is_ok() {
                    let _ = gd.commit_owned(txn);
                }
            }
        }));
        for h in handles {
            h.join().expect("worker");
        }

        assert_eq!(
            g.verify_rel_endpoints().expect("fsck"),
            Vec::<u64>::new(),
            "exempt={exempt}: a relationship names an endpoint that does not exist"
        );
        // NON-VACUITY: an FSCK over an empty graph passes trivially, so the
        // assertion above proves nothing unless creates actually committed
        // while deletes were racing them.
        assert!(
            committed.load(Ordering::Relaxed) > 0,
            "exempt={exempt}: no create committed, so the FSCK checked nothing"
        );
    }
}
