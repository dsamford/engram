#![allow(non_snake_case)]
//! The graph-level write transaction (#1, the transaction bridge). A statement's
//! (or a BEGIN..COMMIT block's) graph writes buffer into ONE store transaction
//! and publish atomically at commit — or vanish entirely at rollback, leaving no
//! partial state. Read-your-writes holds WITHIN the transaction (a relationship
//! can name a node the same transaction just created), while another observer
//! sees nothing until commit.
//!
//! These exercise the `Graph::begin_txn/commit_txn/rollback_txn` API directly;
//! wiring it under Cypher BEGIN/COMMIT (and per-statement atomicity) is the next
//! layer up.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::{Graph, GraphError};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(labels: &[&str], props: &[(&str, Value)]) -> (Vec<String>, BTreeMap<String, Value>) {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    (labels.iter().map(|s| s.to_string()).collect(), m)
}

#[test]
fn a_committed_transaction_publishes_its_whole_write_set() {
    let g = graph();
    g.begin_txn().expect("begin");
    let (l, p) = node(&["Person"], &[("name", Value::Str("ana".into()))]);
    let a = g.create_node(&l, &p).expect("create a");
    let (l, p) = node(&["Person"], &[("name", Value::Str("bo".into()))]);
    let b = g.create_node(&l, &p).expect("create b");
    // The endpoint check must see the SAME transaction's just-created nodes —
    // read-your-writes. With committed-only reads this would be "Missing node".
    let r = g
        .create_rel(a, "KNOWS", b, &BTreeMap::new())
        .expect("create rel");

    // Within the transaction a POINT read sees its own buffered write — the
    // read-your-writes overlay that also lets `create_rel` find a just-created
    // endpoint. (A MATCH *scan* would not; scan-over-buffer is a separate,
    // deferred capability. Cross-session invisibility is proven at the Bolt
    // layer, where a second session cannot see these buffered writes.)
    assert!(
        g.node(a).expect("read a mid-txn").is_some(),
        "read-your-writes: the transaction sees its own buffered node"
    );

    g.commit_txn().expect("commit");

    // Now the whole set is visible.
    assert!(g.node(a).expect("read a").is_some(), "a is published");
    assert!(g.node(b).expect("read b").is_some(), "b is published");
    assert!(
        g.rel(r).expect("read rel").is_some(),
        "the relationship too"
    );
    assert!(!g.in_txn(), "commit cleared the active transaction");
}

#[test]
fn a_rolled_back_transaction_leaves_no_trace() {
    let g = graph();
    g.begin_txn().expect("begin");
    let (l, p) = node(&["T"], &[("k", Value::Int(1))]);
    let a = g.create_node(&l, &p).expect("create a");
    let (l, p) = node(&["T"], &[("k", Value::Int(2))]);
    let b = g.create_node(&l, &p).expect("create b");
    g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");

    g.rollback_txn();

    assert!(!g.in_txn(), "rollback cleared the active transaction");
    assert!(g.node(a).expect("read a").is_none(), "a never published");
    assert!(g.node(b).expect("read b").is_none(), "b never published");
    // And the store is genuinely untouched — a fresh autocommit write lands.
    let (l, p) = node(&["T"], &[("k", Value::Int(3))]);
    let c = g.create_node(&l, &p).expect("post-rollback autocommit");
    assert!(g.node(c).expect("read c").is_some());
}

#[test]
fn a_failed_write_mid_transaction_rolls_back_atomically_no_orphans() {
    // The orphan-node hazard the bridge exists to close: under autocommit, a
    // CREATE that violates a constraint partway leaves the entities it already
    // wrote. In a transaction, the caller rolls the whole thing back and NONE
    // of it is published.
    let g = graph();
    let mut m = BTreeMap::new();
    m.insert("email".to_string(), Value::Str("a@x".into()));
    // A pre-existing owner of the unique email.
    g.create_node(&["User".into()], &m).expect("seed");
    // Constrain uniqueness AFTER the seed (population has one, no dup — ok).
    // (Constraint DDL autocommits; it is schema, not part of the data txn.)
    use engram_graph::run_stmt;
    let stmt = engram_cypher::parse_any("CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS UNIQUE")
        .expect("parse");
    run_stmt(&g, &stmt, BTreeMap::new()).expect("constraint");

    g.begin_txn().expect("begin");
    let (l, p) = node(&["User"], &[("email", Value::Str("new@x".into()))]);
    let good = g.create_node(&l, &p).expect("a valid node in the txn");
    // This one collides with the seed's email → ConstraintViolation.
    let (l, p) = node(&["User"], &[("email", Value::Str("a@x".into()))]);
    let err = g.create_node(&l, &p).expect_err("must violate");
    assert!(matches!(err, GraphError::ConstraintViolation(_)), "{err:?}");
    // The statement failed — roll back the whole thing.
    g.rollback_txn();

    // The VALID node created earlier in the same txn is gone too — atomicity.
    assert!(
        g.node(good).expect("read good").is_none(),
        "a partial transaction must not leave the node it did manage to write"
    );
}

#[test]
fn a_deletion_inside_a_transaction_is_atomic() {
    let g = graph();
    // Seed two committed nodes + an edge.
    let (l, p) = node(&["N"], &[("id", Value::Int(1))]);
    let a = g.create_node(&l, &p).expect("a");
    let (l, p) = node(&["N"], &[("id", Value::Int(2))]);
    let b = g.create_node(&l, &p).expect("b");
    let r = g.create_rel(a, "R", b, &BTreeMap::new()).expect("r");

    g.begin_txn().expect("begin");
    g.delete_rel(r).expect("delete rel in txn");
    g.rollback_txn();
    // Rolled back → the edge is still there.
    assert!(
        g.rel(r).expect("rel after rollback").is_some(),
        "a rolled-back delete restores nothing because it published nothing"
    );

    g.begin_txn().expect("begin 2");
    g.delete_rel(r).expect("delete rel");
    g.commit_txn().expect("commit");
    assert!(
        g.rel(r).expect("rel after commit").is_none(),
        "the committed delete is durable"
    );
    // Both endpoints survive.
    assert!(g.node(a).expect("a").is_some());
    assert!(g.node(b).expect("b").is_some());
}

#[test]
fn nested_begin_and_stray_commit_are_refused() {
    let g = graph();
    g.begin_txn().expect("begin");
    assert!(
        matches!(g.begin_txn(), Err(GraphError::Txn(_))),
        "a nested begin is refused"
    );
    g.commit_txn().expect("commit");
    assert!(
        matches!(g.commit_txn(), Err(GraphError::Txn(_))),
        "a commit with nothing active is refused"
    );
    // A stray rollback is a harmless no-op (so a failed statement can roll back
    // blindly without first checking).
    g.rollback_txn();
}
