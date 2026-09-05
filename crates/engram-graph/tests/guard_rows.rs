#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Guard rows (W1.1): a relationship write racing a node delete must be a
//! write-write conflict on the endpoint's `'G'` guard row in EITHER commit
//! order. Without the guard, the delete-commits-second order slipped
//! through: the delete's unrecorded adjacency scan saw nothing, its
//! write-set (node record, memberships) never intersected the create's
//! (rel record, adjacency rows), and both committed — a dangling edge.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, GraphError, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .len()
}

/// Buffer `src` into a fresh owned transaction WITHOUT committing.
fn buffered(g: &Graph, src: &str) -> engram_graph::GraphTxn {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    r.unwrap_or_else(|e| panic!("txn `{src}`: {e}"));
    txn
}

fn race(create_commits_first: bool) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:Hub {id: 1})");
    run(&g, "CREATE (:Sat {id: 2})");
    let create = buffered(
        &g,
        "MATCH (h:Hub {id: 1}), (s:Sat {id: 2}) CREATE (s)-[:R]->(h)",
    );
    let delete = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
    let (first, second) = if create_commits_first {
        (create, delete)
    } else {
        (delete, create)
    };
    g.commit_owned(first).expect("the first commit wins");
    let e = g.commit_owned(second).expect_err("the second must conflict");
    assert!(
        matches!(e, GraphError::TxnConflict),
        "expected TxnConflict, got {e:?}"
    );
    assert_eq!(
        g.verify_rel_endpoints().expect("fsck"),
        Vec::<u64>::new(),
        "no dangling edges in either order"
    );
}

#[test]
fn a_rel_create_and_a_node_delete_conflict_when_the_create_commits_first() {
    // The order the guard exists FOR: the delete's scan saw no rels, and
    // only its buffered delete of the guard row collides with the create.
    race(true);
}

#[test]
fn a_rel_create_and_a_node_delete_conflict_when_the_delete_commits_first() {
    race(false);
}

#[test]
fn distinct_endpoint_creates_do_not_conflict_with_each_other() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:Hub {id: 1}), (:Hub {id: 2}), (:Sat {id: 3}), (:Sat {id: 4})");
    let a = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 3}) CREATE (s)-[:R]->(h)");
    let b = buffered(&g, "MATCH (h:Hub {id: 2}), (s:Sat {id: 4}) CREATE (s)-[:R]->(h)");
    g.commit_owned(a).expect("disjoint endpoints");
    g.commit_owned(b).expect("disjoint endpoints do not serialise");
    assert_eq!(rows(&g, "MATCH ()-[r:R]->() RETURN id(r)"), 2);
}

#[test]
fn two_creates_sharing_an_endpoint_no_longer_abort_each_other() {
    // RC1 / O3: the guard exists to make a relationship write and a node
    // DELETE conflict. Two relationship writes are both valid, and making
    // them abort each other was pure cost — measured at ~48% of the OCC
    // re-runs on the `rel-hub` shape. Both now commit, and both edges land
    // with no re-run at all.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:Hub {id: 1}), (:Sat {id: 2}), (:Sat {id: 3})");
    let a = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 2}) CREATE (s)-[:R]->(h)");
    let b = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 3}) CREATE (s)-[:R]->(h)");
    g.commit_owned(a).expect("first hub write");
    g.commit_owned(b)
        .expect("a second write to the same endpoint is not a conflict");
    assert_eq!(rows(&g, "MATCH (:Hub {id: 1})<-[r:R]-() RETURN id(r)"), 2);
    assert_eq!(g.verify_rel_endpoints().expect("fsck"), Vec::<u64>::new());
}

#[test]
fn with_the_exemption_off_two_creates_still_serialise() {
    // THE CANARY for the test above: with the lever off this is the old
    // behaviour, so the passing test above is evidence the exemption fires
    // rather than evidence that nothing ever conflicted here.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_guard_put_put_exempt(false);
    run(&g, "CREATE (:Hub {id: 1}), (:Sat {id: 2}), (:Sat {id: 3})");
    let a = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 2}) CREATE (s)-[:R]->(h)");
    let b = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 3}) CREATE (s)-[:R]->(h)");
    g.commit_owned(a).expect("first hub write");
    let e = g
        .commit_owned(b)
        .expect_err("with the exemption off, a shared endpoint conflicts");
    assert!(matches!(e, GraphError::TxnConflict));
    let retry = buffered(&g, "MATCH (h:Hub {id: 1}), (s:Sat {id: 3}) CREATE (s)-[:R]->(h)");
    g.commit_owned(retry).expect("the re-run lands");
    assert_eq!(rows(&g, "MATCH (:Hub {id: 1})<-[r:R]-() RETURN id(r)"), 2);
    assert_eq!(g.verify_rel_endpoints().expect("fsck"), Vec::<u64>::new());
}

#[test]
fn rel_delete_also_moves_both_guards() {
    // delete_rel racing delete_node of an endpoint: the rel delete PUTS
    // both guards, the node delete DELETES its own — write-write again.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:Sat {id: 2})-[:R]->(:Hub {id: 1})");
    let del_rel = buffered(&g, "MATCH (:Sat {id: 2})-[r:R]->(:Hub {id: 1}) DELETE r");
    let del_hub = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
    g.commit_owned(del_rel).expect("rel delete commits");
    let e = g.commit_owned(del_hub).expect_err("the node delete must re-run");
    assert!(matches!(e, GraphError::TxnConflict));
    let retry = buffered(&g, "MATCH (h:Hub {id: 1}) DETACH DELETE h");
    g.commit_owned(retry).expect("re-run on fresh state");
    assert_eq!(g.verify_rel_endpoints().expect("fsck"), Vec::<u64>::new());
}
