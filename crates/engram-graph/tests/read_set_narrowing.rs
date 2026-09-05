#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A MATCH candidate enters the OCC read set only once it BECOMES a binding.
//!
//! **Ships OFF.** This is the one change in the write-path programme that is
//! not transparent: it narrows what is RECORDED given the same records
//! materialised, where everything else narrows what is materialised.
//!
//! The read set exists to stop a write being computed from a stale value — a
//! DATA-FLOW property. A candidate `node_satisfies` rejects contributes nothing
//! but its absence, so recording it is conservative. Removing it is, on a label
//! scan, the difference between O(label) and O(1) entries for validation to
//! walk under the global commit latch.
//!
//! What it admits is an anti-dependency on a PREDICATE, and these tests pin
//! that as a named, deterministic fact rather than leaving it as a hope. They
//! also pin the two things that must NOT change: answers, and MERGE.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, GraphError, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
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

fn count(t: &engram_observe::Trace, k: &str) -> u64 {
    t.counters().get(k).copied().unwrap_or(0)
}

fn seeded(g: &Graph) {
    for i in 0..200i64 {
        run(g, &format!("CREATE (:T {{k: {i}, tag: {}}})", i % 5));
    }
}

/// It is OFF unless asked for. A change that admits an anomaly must not arrive
/// by default, however small the anomaly.
#[test]
fn narrowing_is_off_by_default() {
    let g = graph();
    assert!(
        !g.read_set_bindings_only(),
        "this is the one non-transparent item in the programme; it ships off"
    );
}

/// ANSWERS are unchanged on both arms — narrowing changes aborts, not results.
#[test]
fn answers_are_identical_on_both_arms() {
    let mut arms = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_read_set_bindings_only(on);
        seeded(&g);
        let mut answers = Vec::new();
        for q in [
            "MATCH (n:T {k: 7}) RETURN n.k",
            "MATCH (n:T {tag: 2}) RETURN n.k",
            "MATCH (n:T) WHERE n.k > 190 RETURN n.k",
            "MATCH (n:T {k: 9999}) RETURN n.k",
        ] {
            answers.push(rows(&g, q));
        }
        // And through a WRITE, which is the path that validates.
        assert!(stmt(&g, "MATCH (n:T {k: 3}) SET n.touched = 1").is_ok());
        answers.push(rows(&g, "MATCH (n:T {k: 3}) RETURN n.touched"));
        arms.push(answers);
    }
    assert_eq!(
        arms[0], arms[1],
        "narrowing may change what a transaction ABORTS ON, never what it answers"
    );
}

/// Non-vacuity: the ON arm really keeps rejected candidates out.
#[test]
fn the_lever_actually_narrows() {
    let g = graph();
    g.set_read_set_bindings_only(true);
    seeded(&g);
    let (_, on) = engram_observe::with_trace(|| {
        // A MULTI-key map with NO declared index: the seek declines and the
        // label is SCANNED, which is the only shape where `node_satisfies`
        // rejects anything. A one-key map takes the arity-1 seek, materialises
        // its two hits and rejects nothing — the first version of this test
        // asserted against that and measured `index.range queries: 2`.
        let _ = stmt(&g, "MATCH (n:T {k: 3, tag: 3}) SET n.touched = 1");
    });
    assert!(
        count(&on, "graph.rejected candidates kept out of the read set") >= 100,
        "a label scan of 200 rejecting all but one must keep ~199 out, got {} — \
         counters: {:?}",
        count(&on, "graph.rejected candidates kept out of the read set"),
        on.counters()
    );

    let g2 = graph();
    g2.set_read_set_bindings_only(false);
    seeded(&g2);
    let (_, off) = engram_observe::with_trace(|| {
        let _ = stmt(&g2, "MATCH (n:T {k: 3, tag: 3}) SET n.touched = 1");
    });
    assert_eq!(
        count(&off, "graph.rejected candidates kept out of the read set"),
        0,
        "OFF arm records every candidate — it is the differential's control"
    );
}

/// A read that BECAME a binding is still recorded, so the ordinary
/// read-modify-write still aborts against a concurrent writer. This is the half
/// narrowing must not break.
#[test]
fn a_binding_that_moves_still_aborts_on_both_arms() {
    for on in [false, true] {
        let g = graph();
        g.set_read_set_bindings_only(on);
        seeded(&g);
        let t = buffered(&g, "MATCH (n:T {k: 42}) SET n.v = 1");
        // Another transaction moves the very node T matched.
        assert!(stmt(&g, "MATCH (n:T {k: 42}) SET n.v = 99").is_ok());
        assert!(
            matches!(g.commit_owned(t), Err(GraphError::TxnConflict)),
            "arm on={on}: a BINDING that moved must still abort — narrowing \
             removes rejected candidates, never accepted ones"
        );
    }
}

/// THE ADMITTED ANOMALY, constructed deterministically and pinned by name.
///
/// T scans the label and finds nothing matching `k = 777`. Concurrently, an
/// EXISTING node is mutated to satisfy that predicate. With full recording T
/// read that node and aborts; narrowed, it did not and commits.
///
/// This is asserted in BOTH directions on purpose: a test that only showed the
/// OFF arm aborting would not prove the ON arm admits it, and an admitted
/// anomaly nobody has written down is the kind that gets rediscovered as a bug.
#[test]
fn the_admitted_anomaly_is_a_pinned_fact_not_a_hope() {
    let verdict = |on: bool| -> bool {
        let g = graph();
        g.set_read_set_bindings_only(on);
        seeded(&g);
        // T finds nothing for k=777 and writes on that basis.
        // Two keys, nothing declared: this SCANS, so every non-matching node
        // is a rejected candidate — the entries narrowing removes.
        let t = buffered(&g, "MATCH (n:T {k: 777, tag: 2}) SET n.seen = 1");
        let t = {
            // A second clause in the same transaction, so it has a write set.
            let q = parse_statement("CREATE (:Marker {m: 1})").expect("parse");
            let (t, r) = g.with_txn(t, || run_query(&g, &q, BTreeMap::new()));
            r.expect("marker");
            t
        };
        // Concurrently, an EXISTING node becomes one T would have matched.
        // Node k=7 already has tag=2 (7 % 5 == 2), so moving its `k` makes it
        // a node T's predicate WOULD have matched.
        assert!(stmt(&g, "MATCH (n:T {k: 7, tag: 2}) SET n.k = 777").is_ok());
        g.commit_owned(t).is_ok()
    };
    assert!(
        !verdict(false),
        "with full recording T read the node that changed, so it must abort — \
         this is the arm that does NOT admit the anomaly"
    );
    assert!(
        verdict(true),
        "narrowed, T never recorded the node it rejected, so it commits: an \
         anti-dependency on a PREDICATE. This is the admitted anomaly, and it \
         is why the lever ships off"
    );
}

/// THE ENDGAME'S LAST LINK: §7 makes the admitted anomaly unreachable, which
/// is what reclassifies D4 from "admits an anti-dependency" to transparent.
///
/// The plan's sequence is §6 → §7 → flip D4 ON, and it named this test as the
/// proof: the SAME deterministic anomaly pinned above must ABORT once precision
/// locking is on, with narrowing also on.
///
/// The mechanism, stated so the pass is not mistaken for a coincidence.
/// Narrowing removes the rejected candidate from the read set, so read-set
/// validation has nothing to test. Precision locking does not look at the read
/// set at all: it takes the rows COMMITTED since the snapshot and tests each
/// against the predicate `(:T {k: 777, tag: 2})`. The concurrent `SET n.k =
/// 777` makes node k=7 satisfy exactly that, so the row is found by the half of
/// validation that narrowing does not touch.
#[test]
fn precision_locking_closes_the_anomaly_narrowing_admits() {
    let verdict = |narrow: bool, precision: bool| -> bool {
        let g = graph();
        g.set_read_set_bindings_only(narrow);
        g.set_precision_locking(precision);
        seeded(&g);
        let t = buffered(&g, "MATCH (n:T {k: 777, tag: 2}) SET n.seen = 1");
        let t = {
            let q = parse_statement("CREATE (:Marker {m: 1})").expect("parse");
            let (t, r) = g.with_txn(t, || run_query(&g, &q, BTreeMap::new()));
            r.expect("marker");
            t
        };
        assert!(stmt(&g, "MATCH (n:T {k: 7, tag: 2}) SET n.k = 777").is_ok());
        g.commit_owned(t).is_ok()
    };
    // The pinned anomaly, restated here so this test stands on its own: with
    // narrowing on and precision locking off, T commits.
    assert!(
        verdict(true, false),
        "the anomaly must still be present with §7 off, or this test is          asserting that something already fixed stays fixed"
    );
    assert!(
        !verdict(true, true),
        "§7 ON must close it: the changed row satisfies T's predicate, and          predicate validation does not consult the read set narrowing shrank"
    );
    // And the two levers together must not abort a transaction that neither
    // alone would — the over-abort check, on the arm that ships.
    let g = graph();
    g.set_read_set_bindings_only(true);
    g.set_precision_locking(true);
    seeded(&g);
    let t = buffered(&g, "MATCH (n:T {k: 777, tag: 2}) SET n.seen = 1");
    let t = {
        let q = parse_statement("CREATE (:Marker {m: 2})").expect("parse");
        let (t, r) = g.with_txn(t, || run_query(&g, &q, BTreeMap::new()));
        r.expect("marker");
        t
    };
    // A concurrent write that does NOT make anything satisfy T's predicate.
    assert!(stmt(&g, "MATCH (n:T {k: 8}) SET n.seen = 9").is_ok());
    assert!(
        g.commit_owned(t).is_ok(),
        "an unrelated concurrent write is not a phantom, with either lever on          or both — an isolation upgrade that also refuses correct commits is          not the trade this item is making"
    );
}

/// MERGE keeps FULL recording whatever the lever says.
///
/// MERGE is the write-on-the-basis-of-absence shape, so for it the absence IS
/// the data flow. `match_path` carried a `_for_merge` flag that every call site
/// passed `false` — it was dead. It is live now.
#[test]
fn merge_keeps_full_recording_even_when_narrowing_is_on() {
    let g = graph();
    g.set_read_set_bindings_only(true);
    ddl(&g, "CREATE CONSTRAINT tu FOR (n:U) REQUIRE n.u IS UNIQUE");
    for i in 0..50i64 {
        run(&g, &format!("CREATE (:U {{u: {i}}})"));
    }
    let (_, t) = engram_observe::with_trace(|| {
        let _ = stmt(&g, "MERGE (n:U {u: 900})");
    });
    assert_eq!(
        count(&t, "graph.rejected candidates kept out of the read set"),
        0,
        "MERGE must record every candidate it examined even with narrowing on — \
         its decision is made ON the absence, counters: {:?}",
        t.counters()
    );
    // And it still converges.
    assert!(stmt(&g, "MERGE (n:U {u: 900})").is_ok());
    assert_eq!(
        rows(&g, "MATCH (n:U {u: 900}) RETURN n.u").len(),
        1,
        "two MERGEs of one value converge on one node"
    );
}
