#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! MERGE converges when it loses a create race — and still reports a GENUINE
//! uniqueness violation.
//!
//! Two concurrent MERGEs of one value can both find nothing and both take the
//! create arm. The loser's create then meets the winner's committed uniqueness
//! marker and gets a `ConstraintViolation`, which the Bolt retry loop does not
//! re-run (it retries `TxnConflict`). So `MERGE` surfaced a constraint
//! violation to a client instead of converging — measured at roughly 1 run in 4
//! of `engram-server`'s racing-merge test, which had been passing by luck.
//!
//! The fix must not be "treat every violation as a race". A re-MATCH is the
//! exact discriminator AND it terminates: if the value is now visible the race
//! is over and the row takes the match arm; if it still is not, the violated
//! constraint is about something other than the merged pattern and the error is
//! real. Mapping unconditionally would spin a genuine violation up to the
//! retry bound before reporting it.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn try_run(g: &Graph, src: &str) -> Result<usize, String> {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows.len())
        .map_err(|e| format!("{e:?}"))
}

/// Run a statement AS THE SERVER DOES: inside an autocommit transaction.
///
/// Bare `run_query` autocommits every store write SEPARATELY, so there is no
/// statement-level OCC at all and eight racers happily create eight nodes.
/// Statement atomicity comes from the enclosing transaction; the Bolt loop
/// opens one per statement, and a concurrency test that does not is measuring a
/// shape production never runs.
fn stmt(g: &Graph, src: &str) -> Result<(), String> {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
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

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// A GENUINE violation must still be reported.
///
/// The constraint is on `u`; the MERGE pattern also fixes `w`, so it matches
/// nothing and creates a node whose `u` duplicates an existing one. A re-MATCH
/// still finds nothing — `w` really is new — so this is not a lost race and the
/// violation is the right answer.
#[test]
fn a_genuine_uniqueness_violation_is_still_reported() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    assert!(try_run(&g, "CREATE (:M {u: 1, w: 9})").is_ok());

    let err = try_run(&g, "MERGE (n:M {u: 1, w: 8})")
        .expect_err("this duplicates u and is NOT a lost race");
    assert!(
        err.contains("ConstraintViolation"),
        "a genuine duplicate must surface as a violation, got: {err}"
    );
    assert_eq!(
        rows(&g, "MATCH (n:M) RETURN n.u").len(),
        1,
        "and nothing was created"
    );
}

/// MERGE of a value that ALREADY exists takes the match arm — the ordinary
/// path, which must be untouched.
#[test]
fn merge_of_an_existing_value_still_matches() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    assert!(try_run(&g, "CREATE (:M {u: 7})").is_ok());
    assert!(try_run(&g, "MERGE (n:M {u: 7})").is_ok());
    assert_eq!(
        rows(&g, "MATCH (n:M) RETURN n.u").len(),
        1,
        "MERGE on an existing value must not create a second node"
    );
}

/// MERGE of a fresh value creates exactly one.
#[test]
fn merge_of_a_fresh_value_creates_one() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    assert!(try_run(&g, "MERGE (n:M {u: 3})").is_ok());
    assert!(try_run(&g, "MERGE (n:M {u: 3})").is_ok());
    assert_eq!(
        rows(&g, "MATCH (n:M) RETURN n.u").len(),
        1,
        "two MERGEs of one value converge on one node"
    );
}

/// Under real threads on one shared graph, every MERGE must SUCCEED and they
/// must converge — this is the shape that was failing.
#[test]
fn concurrent_merges_of_one_value_all_succeed_and_converge() {
    use std::sync::Arc;
    let g = Arc::new(graph());
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    ddl(&g, "CREATE INDEX m_u FOR (n:M) ON (n.u)");

    for round in 0..12i64 {
        let errors = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let g = Arc::clone(&g);
                let errors = Arc::clone(&errors);
                std::thread::spawn(move || {
                    // The engine-level retry the Bolt loop provides: a lost
                    // race that DOES surface as a conflict is retryable, and a
                    // converged one returns Ok on the first attempt.
                    let mut last = None;
                    for _ in 0..64 {
                        match stmt(&g, &format!("MERGE (n:M {{u: {round}}})")) {
                            Ok(_) => return,
                            Err(e) if e.contains("TxnConflict") => {
                                last = Some(e);
                                std::thread::yield_now();
                            }
                            Err(e) => {
                                errors.lock().expect("lock").push(e);
                                return;
                            }
                        }
                    }
                    if let Some(e) = last {
                        errors.lock().expect("lock").push(e);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("merger");
        }
        let errs = errors.lock().expect("lock").clone();
        assert!(
            errs.is_empty(),
            "round {round}: MERGE must always succeed, got {} error(s): {:?}",
            errs.len(),
            errs
        );
        assert_eq!(
            rows(&g, &format!("MATCH (n:M {{u: {round}}}) RETURN n.u")).len(),
            1,
            "round {round}: the racers must converge on ONE node"
        );
    }
}
