#![allow(non_snake_case)]
// Real threads on one shared graph — the race this fix closes needs them;
// the same allowance `merge_race_convergence` carries.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Fix 75: a MERGE that loses its create race converges on the winner even
//! when the winner's commit has not yet reached the derived index the
//! re-match would seek through.
//!
//! The race-window hook stands in for the winner (as the sim sweep does);
//! the refusal names the winner's id and the loser binds it directly. The
//! real-thread suite (`merge_race_convergence`) is the probabilistic half:
//! it failed 1–3 % of its runs alone before this fix.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, MergeRaceHook, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn try_rows(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e:?}"))
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    try_rows(g, src).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

/// Run a statement AS THE SERVER DOES: inside an autocommit transaction.
/// Bare `run_query` autocommits every store write separately — no
/// statement-level OCC, so eight racers would create eight nodes.
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const BY_ID: &str = "interp.merge converged on the refusing node by id";
const CONVERGED: &str = "interp.merge races converged";

/// A hook that creates the merged value between the loser's empty match
/// and its create.
fn winner_hook(u: i64) -> MergeRaceHook {
    Arc::new(move |g: &Graph| {
        let mut p = BTreeMap::new();
        p.insert("u".to_string(), Value::Int(u));
        let _ = g.create_node(&["M".to_string()], &p);
    })
}

/// The lost race converges on the winner's node, bound by the id the
/// refusal named — no re-match, one node afterwards.
#[test]
fn a_the_loser_binds_the_winner_by_id() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    ddl(&g, "CREATE INDEX m_u FOR (n:M) ON (n.u)");
    g.set_merge_race_hook_for_test(Some(winner_hook(7)));
    let (got, trace) = engram_observe::with_trace(|| rows(&g, "MERGE (n:M {u: 7}) RETURN n.u AS u"));
    let c = trace.counters().clone();
    assert_eq!(got, vec![vec![Value::Int(7)]]);
    assert_eq!(count_of(&c, BY_ID), 1, "{c:?}");
    assert_eq!(count_of(&c, CONVERGED), 1, "{c:?}");
    assert_eq!(rows(&g, "MATCH (n:M) RETURN count(n) AS n"), vec![vec![Value::Int(1)]]);
    g.set_merge_race_hook_for_test(None);
}

/// ON MATCH runs against the winner's node when the loser converges.
#[test]
fn b_on_match_applies_to_the_winner() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    g.set_merge_race_hook_for_test(Some(winner_hook(3)));
    let got = rows(
        &g,
        "MERGE (n:M {u: 3}) ON CREATE SET n.how = 'created' ON MATCH SET n.how = 'matched' RETURN n.how AS how",
    );
    assert_eq!(got, vec![vec![Value::Str("matched".into())]]);
    assert_eq!(
        rows(&g, "MATCH (n:M) RETURN n.u AS u, n.how AS how"),
        vec![vec![Value::Int(3), Value::Str("matched".into())]]
    );
    g.set_merge_race_hook_for_test(None);
}

/// A refusal whose node does NOT satisfy the merged pattern is genuine:
/// the winner carries `u: 5` but not `extra: 1`, so the MERGE of
/// `{u: 5, extra: 1}` can neither match it nor create beside it — the
/// violation surfaces, and nothing is converged onto the wrong node.
#[test]
fn c_a_refusal_the_pattern_cannot_match_stays_a_violation() {
    let g = graph();
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    g.set_merge_race_hook_for_test(Some(winner_hook(5)));
    let (got, trace) =
        engram_observe::with_trace(|| try_rows(&g, "MERGE (n:M {u: 5, extra: 1}) RETURN n.u AS u"));
    let c = trace.counters().clone();
    let err = got.expect_err("a genuine violation surfaces");
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(count_of(&c, BY_ID), 0, "{c:?}");
    assert_eq!(count_of(&c, CONVERGED), 0, "{c:?}");
    assert_eq!(
        rows(&g, "MATCH (n:M) RETURN n.u AS u, n.extra AS extra"),
        vec![vec![Value::Int(5), Value::Null]]
    );
    g.set_merge_race_hook_for_test(None);
}

/// The real race, longer than the convergence suite runs it: eight
/// threads MERGE one value for forty rounds on a shared graph; every MERGE
/// succeeds (a conflict is retried, as the Bolt loop does) and every round
/// converges on one node.
#[test]
fn d_forty_rounds_of_eight_racing_merges_all_succeed() {
    let g = Arc::new(graph());
    ddl(&g, "CREATE CONSTRAINT mu FOR (n:M) REQUIRE n.u IS UNIQUE");
    ddl(&g, "CREATE INDEX m_u FOR (n:M) ON (n.u)");
    for round in 0..40i64 {
        let errors = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let g = Arc::clone(&g);
                let errors = Arc::clone(&errors);
                std::thread::spawn(move || {
                    let mut last = None;
                    for _ in 0..64 {
                        match stmt(&g, &format!("MERGE (n:M {{u: {round}}}) RETURN n.u AS u")) {
                            Ok(()) => return,
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
        assert!(errs.is_empty(), "round {round}: {} error(s): {errs:?}", errs.len());
        assert_eq!(
            rows(&g, &format!("MATCH (n:M {{u: {round}}}) RETURN count(n) AS n")),
            vec![vec![Value::Int(1)]],
            "round {round}: the racers must converge on ONE node"
        );
    }
}
