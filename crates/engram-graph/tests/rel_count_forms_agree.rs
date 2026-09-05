#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Two ways of counting the same edges must never disagree — including while
//! the derived structures are STALE.
//!
//! # Why this file exists
//!
//! The SF1 pod sweep's own integrity check reported:
//!
//! ```text
//! unique-create @ 8 clients: 577054 edge(s) but only 586126 bind both
//! endpoints — DANGLING EDGES
//! ```
//!
//! The label is wrong for the numbers. A dangling edge makes the BOUND count
//! LOWER than the bare one; here bound was HIGHER by 9,072. And the profile it
//! is attributed to writes `CREATE (:Uniq {u: ...})` — nodes only — so nothing
//! should have been writing those edges between the two counts.
//!
//! Two candidates, opposite in severity:
//!
//! - a **harness race**: the two queries ran at different instants with writes
//!   still landing. Benign, and then the check needs fixing.
//! - an **over-complete adjacency CSR**: `MATCH (a)-[r]->(b)` can drive from
//!   the adjacency tables where `MATCH ()-[r]->()` scans the relationship
//!   partition. A CSR holding edges the store no longer has answers the first
//!   form with rows the second cannot produce. That is the silent wrong answer
//!   §5.2 and §5.4 exist to prevent.
//!
//! Re-querying the quiescent pod store answered 585,765 both ways, three times
//! — so the CSR is not over-complete AT REST. That does not settle it: a
//! divergence that appears while a table is stale and disappears when it is
//! repaired would look exactly like this, and is still a wrong answer while it
//! lasts. So this file drives the two forms against DELIBERATELY STALE derived
//! structures, which is the state the sweep was in and the state a rest-time
//! query can never reproduce.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g
            .commit_owned(txn)
            .unwrap_or_else(|e| panic!("commit {src}: {e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            panic!("run {src}: {e:?}");
        }
    }
}

fn count(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected a count from `{src}`, got {other:?}"),
    }
}

/// The two forms the pod's check compares.
fn both(g: &Graph) -> (i64, i64) {
    (
        count(g, "MATCH ()-[r:T]->() RETURN count(r)"),
        count(g, "MATCH (a)-[r:T]->(b) RETURN count(r)"),
    )
}

/// The SINGLE-STATEMENT form the harness now uses, so the fix is exercised
/// here and not only on the pod.
///
/// A one-statement check that silently returned nothing — a shape the engine
/// declined to plan, a column read at the wrong depth — would report "PASS"
/// for every run and never verify anything again. That failure is invisible by
/// construction, which is why the harness's own query shape is pinned by a
/// test rather than trusted because it compiled.
fn both_in_one_statement(g: &Graph) -> (i64, i64) {
    let src = "MATCH ()-[r:T]->() WITH count(r) AS bare                MATCH (a)-[q:T]->(b) RETURN bare, count(q) AS bound";
    let stmt = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    let r = run_query(g, &stmt, BTreeMap::new()).unwrap_or_else(|e| panic!("run: {e:?}"));
    let row = r.rows.first().expect("the check must return a row");
    match (row.first(), row.get(1)) {
        (Some(Value::Int(x)), Some(Value::Int(y))) => (*x, *y),
        other => panic!("the check must return two integer columns, got {other:?}"),
    }
}

/// THE TEST: the two forms agree after every write, at every point where a
/// derived structure is stale, and after deletes.
///
/// Interleaving the counts WITH the writes is the whole method. A test that
/// wrote everything and then counted would be the quiescent query that already
/// answered — and the state under suspicion is the one where a table is behind
/// its epoch and a reader is deciding whether to use it.
#[test]
fn the_two_count_forms_agree_while_the_derived_tables_are_stale() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Probes admit a table immediately, so the tables EXIST and can go stale.
    // With the default gate a write-heavy fixture never builds one, and a
    // table that is never built cannot be over-complete — which would make
    // this test pass for the wrong reason.
    g.set_degree_table_after(0);

    for i in 0..40i64 {
        stmt(&g, &format!("CREATE (:N {{k: {i}}})"));
    }
    // Warm the adjacency tables so the counts below can consult them.
    let _ = g.adjacent_slim(1, engram_graph::Dir::Out, &None);
    let _ = g.adjacent_slim(1, engram_graph::Dir::In, &None);

    let mut expected = 0i64;
    for i in 0..60i64 {
        let a = i % 40;
        let b = (a * 7 + 3) % 40;
        stmt(
            &g,
            &format!("MATCH (x:N {{k: {a}}}), (y:N {{k: {b}}}) CREATE (x)-[:T]->(y)"),
        );
        expected += 1;
        // COUNTED IMMEDIATELY, with the tables now behind their epoch.
        let (bare, bound) = both(&g);
        assert_eq!(
            bare, bound,
            "after {expected} creates the two forms disagree: bare {bare}, \
             bound {bound}. One of them is reading a derived structure that no \
             longer describes the store."
        );
        assert_eq!(
            bare, expected,
            "and both must equal the number of edges actually created"
        );
        // The harness's ONE-STATEMENT form must agree with the two-query form
        // and with reality — otherwise the fix silently stops verifying.
        let one = both_in_one_statement(&g);
        assert_eq!(
            one,
            (bare, bound),
            "the single-statement check must see what the two queries see"
        );
    }

    // DELETES are the direction that makes an over-complete CSR visible: the
    // rows leave the store, and a table that still holds them answers with
    // edges that are gone.
    for a in (0..40i64).step_by(3) {
        stmt(&g, &format!("MATCH (x:N {{k: {a}}}) DETACH DELETE x"));
        let (bare, bound) = both(&g);
        assert_eq!(
            bare, bound,
            "after deleting node {a} the two forms disagree: bare {bare}, \
             bound {bound} — a CSR holding a deleted node's edges answers the \
             endpoint-binding form with rows the partition scan cannot produce"
        );
    }
}

/// The same, with the derived structures forced stale by a compaction between
/// the write and the count — the sweep's actual shape, where §5.2's emit
/// republishes the tables underneath a running workload.
#[test]
fn the_two_forms_agree_across_a_compaction_that_emits() {
    let dir = std::env::temp_dir().join(format!("engram-relcount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let store = Store::new();
    let g = std::sync::Arc::new(Graph::new(store.clone(), Realm(1), Namespace(1)));
    g.set_degree_table_after(0);
    for i in 0..40i64 {
        stmt(&g, &format!("CREATE (:N {{k: {i}}})"));
        if i % 8 == 7 {
            store.seal();
        }
    }
    for i in 0..60i64 {
        let a = i % 40;
        let b = (a * 7 + 3) % 40;
        stmt(
            &g,
            &format!("MATCH (x:N {{k: {a}}}), (y:N {{k: {b}}}) CREATE (x)-[:T]->(y)"),
        );
        if i % 12 == 11 {
            store.seal();
        }
    }
    // Delete some, so the merge has tombstones and the emitted CSR must NOT
    // carry the rows they shadow.
    for a in (0..40i64).step_by(5) {
        stmt(&g, &format!("MATCH (x:N {{k: {a}}}) DETACH DELETE x"));
    }
    store.seal();
    let _ = g.adjacent_slim(1, engram_graph::Dir::Out, &None);
    let before = both(&g);

    let cache = store.into_paged(&dir, 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, &dir, &cache).expect("compaction");

    let after = both(&g);
    assert_eq!(
        after.0, after.1,
        "after a compaction that EMITS the CSR, the two forms must still \
         agree: bare {}, bound {}",
        after.0, after.1
    );
    assert_eq!(
        after, before,
        "and a compaction changes no answer — it retires unreachable versions"
    );

    // A fresh graph adopting the sidecar must answer the same.
    let g2 = std::sync::Arc::new(Graph::new(store.clone(), Realm(1), Namespace(1)));
    g2.set_degree_table_after(0);
    g2.adopt_derived_sidecar(&dir);
    let adopted = both(&g2);
    assert_eq!(
        adopted, before,
        "and so must a graph that ADOPTED the persisted bases rather than \
         building them"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
