#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! UNIQUE / NODE KEY enforcement through OCC marker rows (W1.2): the
//! phantom — two concurrent creates of one value both passing an unrecorded
//! population walk — becomes a write-write conflict on a marker key; the
//! walk survives only as the v1 / uncovered-tuple fallback; and every
//! maintenance path (SET, label changes, deletes, drops, bulk exit) keeps
//! the marker family consistent, checked by the fsck.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, GraphError, run_query, run_stmt};
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn graph_over(store: Store) -> Graph {
    Graph::new(store, Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn try_run(g: &Graph, src: &str) -> Result<(), String> {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
    run_query(g, &q, BTreeMap::new())
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

fn rows(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .len()
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn buffered(g: &Graph, src: &str) -> engram_graph::GraphTxn {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    r.unwrap_or_else(|e| panic!("txn `{src}`: {e}"));
    txn
}

#[test]
fn the_concurrent_duplicate_phantom_is_now_an_occ_conflict() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    // Two transactions create the SAME fresh value; neither sees the other's
    // buffered marker. Both write the marker KEY — validation aborts the
    // second, whichever committed first.
    let a = buffered(&g, "CREATE (:U {u: 77})");
    let b = buffered(&g, "CREATE (:U {u: 77})");
    g.commit_owned(a).expect("first committer wins");
    let e = g.commit_owned(b).expect_err("second must conflict");
    assert!(matches!(e, GraphError::TxnConflict), "{e:?}");
    // The Bolt loop's re-run then sees the winner's marker and refuses.
    let retry = {
        let q = parse_statement("CREATE (:U {u: 77})").unwrap();
        let txn = g.open_txn();
        let (txn, r) = g.with_txn(txn, || run_query(&g, &q, BTreeMap::new()));
        g.rollback_owned(txn);
        r
    };
    assert!(
        format!("{:?}", retry.expect_err("duplicate refused on re-run")).contains("already exists"),
    );
    assert_eq!(rows(&g, "MATCH (n:U) RETURN id(n)"), 1);
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn distinct_values_do_not_conflict() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    let a = buffered(&g, "CREATE (:U {u: 1})");
    let b = buffered(&g, "CREATE (:U {u: 2})");
    g.commit_owned(a).expect("a");
    g.commit_owned(b).expect("distinct values must not serialise");
}

#[test]
fn numeric_normalisation_makes_1_and_1_point_0_one_value() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    run(&g, "CREATE (:U {u: 1})");
    let e = try_run(&g, "CREATE (:U {u: 1.0})").expect_err("1.0 duplicates 1");
    assert!(e.contains("already exists"), "{e}");
    // And -0.0 is 0.
    run(&g, "CREATE (:U {u: 0})");
    let e = try_run(&g, "CREATE (:U {u: -0.0})").expect_err("-0.0 duplicates 0");
    assert!(e.contains("already exists"), "{e}");
    // A non-integral float is its own value.
    run(&g, "CREATE (:U {u: 1.5})");
}

#[test]
fn a_single_statement_duplicate_is_refused_through_the_buffer() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    // Through the TRANSACTIONAL path (how the Bolt server runs every write
    // statement): the second create sees the first's BUFFERED marker, the
    // statement refuses, and the rollback leaves nothing behind.
    let q = parse_statement("CREATE (:U {u: 9}), (:U {u: 9})").unwrap();
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(&g, &q, BTreeMap::new()));
    g.rollback_owned(txn);
    assert!(
        format!("{:?}", r.expect_err("in-statement duplicate")).contains("already exists"),
    );
    assert_eq!(rows(&g, "MATCH (n:U) RETURN id(n)"), 0, "nothing survives the refusal");
    // The DIRECT path refuses too (no atomicity claim — that is the
    // transactional path's property).
    let e = try_run(&g, "CREATE (:U {u: 9}), (:U {u: 9})").expect_err("direct refusal");
    assert!(e.contains("already exists"), "{e}");
}

#[test]
fn set_moves_the_marker_and_frees_the_old_value() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    run(&g, "CREATE (:U {u: 1})");
    run(&g, "MATCH (n:U {u: 1}) SET n.u = 2");
    run(&g, "CREATE (:U {u: 1})"); // the old value is free again
    let e = try_run(&g, "CREATE (:U {u: 2})").expect_err("the new value is taken");
    assert!(e.contains("already exists"), "{e}");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn delete_and_label_removal_free_the_value() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    run(&g, "CREATE (:U {u: 5})");
    run(&g, "MATCH (n:U {u: 5}) DELETE n");
    run(&g, "CREATE (:U {u: 5})");
    run(&g, "MATCH (n:U {u: 5}) REMOVE n:U");
    run(&g, "CREATE (:U {u: 5})");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn rel_constraints_take_the_marker_path_too() {
    let g = graph_over(Store::new());
    run(&g, "CREATE (:A {id: 1})-[:R {u: 1}]->(:A {id: 2})");
    ddl(&g, "CREATE CONSTRAINT ru FOR ()-[r:R]-() REQUIRE r.u IS UNIQUE");
    let e = try_run(
        &g,
        "MATCH (a:A {id: 1}), (b:A {id: 2}) CREATE (a)-[:R {u: 1}]->(b)",
    )
    .expect_err("duplicate rel value");
    assert!(e.contains("already exists"), "{e}");
    run(&g, "MATCH (a:A {id: 1}), (b:A {id: 2}) CREATE (a)-[:R {u: 2}]->(b)");
    run(&g, "MATCH ()-[r:R {u: 1}]->() DELETE r");
    run(&g, "MATCH (a:A {id: 1}), (b:A {id: 2}) CREATE (a)-[:R {u: 1}]->(b)");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn ddl_on_populated_data_backfills_and_refuses_existing_duplicates() {
    let g = graph_over(Store::new());
    run(&g, "UNWIND range(1, 50) AS i CREATE (:U {u: i})");
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
    let e = try_run(&g, "CREATE (:U {u: 25})").expect_err("backfilled value is taken");
    assert!(e.contains("already exists"), "{e}");

    // And a population that already violates refuses the DDL outright.
    let g2 = graph_over(Store::new());
    run(&g2, "CREATE (:V {u: 1}), (:V {u: 1})");
    let q = parse_any("CREATE CONSTRAINT v FOR (n:V) REQUIRE n.u IS UNIQUE").expect("parse");
    let r = run_stmt(&g2, &q, BTreeMap::new());
    assert!(format!("{r:?}").contains("duplicate"), "{r:?}");
}

#[test]
fn drop_then_recreate_does_not_inherit_stale_markers() {
    let g = graph_over(Store::new());
    run(&g, "CREATE (:U {u: 1, v: 9})");
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    ddl(&g, "DROP CONSTRAINT u");
    // Recreate under the SAME name over a DIFFERENT property: the old
    // family must be gone, and the new one built for `v`.
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.v IS UNIQUE");
    run(&g, "CREATE (:U {u: 1, v: 8})"); // duplicate u is fine now
    let e = try_run(&g, "CREATE (:U {v: 9})").expect_err("v is constrained");
    assert!(e.contains("already exists"), "{e}");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn ddl_inside_an_open_transaction_is_refused() {
    let g = graph_over(Store::new());
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || {
        let q = parse_any("CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE").expect("parse");
        run_stmt(&g, &q, BTreeMap::new())
    });
    g.rollback_owned(txn);
    assert!(
        format!("{:?}", r.expect_err("refused")).contains("cannot run inside"),
    );
}

#[test]
fn a_v1_constraint_stays_walk_enforced_until_upgraded() {
    // Forge a constraint row the OLD code would have written (no `markers`
    // flag) and prove: (a) enforcement still refuses duplicates through the
    // walk; (b) the boot upgrade builds the family and reports it.
    let store = Store::new();
    let kv = KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(0),
    };
    let g = graph_over(store.clone());
    run(&g, "UNWIND range(1, 10) AS i CREATE (:U {u: i})");
    store
        .put(
            &kv,
            b"con:legacy",
            StoredValue::Plain(
                br#"{"kind":"unique","label":"U","props":["u"]}"#.to_vec(),
            ),
        )
        .expect("forge v1 row");
    // A fresh graph over the store (cold cache) sees the v1 constraint.
    let g = graph_over(store.clone());
    let e = try_run(&g, "CREATE (:U {u: 5})").expect_err("walk enforcement holds");
    assert!(e.contains("already exists"), "{e}");
    let (upgraded, skipped) = g.upgrade_constraint_markers().expect("upgrade");
    assert_eq!((upgraded, skipped.len()), (1, 0));
    let e = try_run(&g, "CREATE (:U {u: 5})").expect_err("marker enforcement holds");
    assert!(e.contains("already exists"), "{e}");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}

#[test]
fn bulk_exit_rebuilds_the_marker_family() {
    let g = graph_over(Store::new());
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    g.set_bulk_ingest(true).expect("bulk on");
    run(&g, "UNWIND range(1, 20) AS i CREATE (:U {u: 100 + i})");
    g.set_bulk_ingest(false).expect("bulk exit rebuild");
    assert_eq!(g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
    let e = try_run(&g, "CREATE (:U {u: 110})").expect_err("bulk-loaded value is taken");
    assert!(e.contains("already exists"), "{e}");
}

#[test]
fn constrained_writes_answer_identically_direct_and_inside_a_transaction() {
    // The differential, over a constrained label: same refusals, same
    // survivors, whichever path ran the statement.
    let corpus = [
        "CREATE (:U {u: 1})",
        "CREATE (:U {u: 2})",
        "MATCH (n:U {u: 1}) SET n.u = 3",
        "CREATE (:U {u: 1})",
        "MATCH (n:U {u: 2}) DELETE n",
        "CREATE (:U {u: 2})",
    ];
    let direct = graph_over(Store::new());
    ddl(&direct, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    let txn_g = graph_over(Store::new());
    ddl(&txn_g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    for stmt in corpus {
        let a = try_run(&direct, stmt).is_ok();
        let b = {
            let q = parse_statement(stmt).unwrap();
            let txn = txn_g.open_txn();
            let (txn, r) = txn_g.with_txn(txn, || run_query(&txn_g, &q, BTreeMap::new()));
            match r {
                Ok(_) => {
                    txn_g.commit_owned(txn).expect("commit");
                    true
                }
                Err(_) => {
                    txn_g.rollback_owned(txn);
                    false
                }
            }
        };
        assert_eq!(a, b, "statement diverged: {stmt}");
    }
    let fa: Vec<usize> = vec![
        rows(&direct, "MATCH (n:U) RETURN id(n)"),
        rows(&direct, "MATCH (n:U {u: 3}) RETURN id(n)"),
    ];
    let fb: Vec<usize> = vec![
        rows(&txn_g, "MATCH (n:U) RETURN id(n)"),
        rows(&txn_g, "MATCH (n:U {u: 3}) RETURN id(n)"),
    ];
    assert_eq!(fa, fb);
    assert_eq!(direct.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
    assert_eq!(txn_g.verify_constraint_markers().expect("fsck"), Vec::<String>::new());
}
