#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! An in-transaction count change is accumulated as a signed DELTA rather than
//! discovered by cloning the counts twice.
//!
//! `stats_apply` took a CLOSURE, so the only way to learn its effect inside a
//! transaction was to clone the committed counts, clone them again, apply the
//! closure to the copy and difference the two. Two `Stats` clones per call —
//! four `BTreeMap` deep copies — six times for a `CREATE (a)-[:R]->(b)`.
//!
//! **The bar is byte-identical `Stats`, not "close enough".** The counts feed
//! the planner: `count_label_nodes` decides seed selection and the columnar
//! thresholds, so a divergence here is a PLAN change, not a number nobody
//! reads.
//!
//! The one place the two paths can disagree is a NEGATIVE delta landing on an
//! ABSENT key. `bump` creates a zero entry; `add_diff` compares 0 against 0 and
//! records nothing. Signed accumulation could carry one, so
//! `StatsDelta::apply` skips it.
//!
//! **That rule is insurance, and this file says so rather than implying more.**
//! A canary that removed it failed no test — `count_label_nodes` is the only
//! consumer of `by_label` and it does `.get(&t).unwrap_or(0)`, so an absent
//! entry and a zero one are the same thing to every reader.
//! `an_absent_label_count_reads_the_same_as_a_zero_one` pins that property of
//! the READERS, so a future consumer that starts distinguishing them fails
//! loudly instead of inheriting a silent assumption.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
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

/// A rolled-back statement: the delta must be DROPPED, not applied.
fn rolled_back(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    let _ = r;
    g.rollback_owned(txn);
}

fn counts(g: &Graph) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    out.push((
        "nodes".to_string(),
        rows(g, "MATCH (n) RETURN n").len(),
    ));
    out.push((
        "rels".to_string(),
        rows(g, "MATCH ()-[r]->() RETURN r").len(),
    ));
    for l in ["A", "B", "C", "Gone"] {
        out.push((
            format!("label {l}"),
            rows(g, &format!("MATCH (n:{l}) RETURN n")).len(),
        ));
    }
    for t in ["R", "S"] {
        out.push((
            format!("type {t}"),
            rows(g, &format!("MATCH ()-[r:{t}]->() RETURN r")).len(),
        ));
    }
    out
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// A mutation stream covering every shape the six call sites see, INCLUDING
/// the ones that saturate and the ones that roll back.
fn drive(g: &Graph) {
    for i in 0..30i64 {
        assert!(stmt(g, &format!("CREATE (:A {{k: {i}}})")).is_ok());
    }
    for i in 0..10i64 {
        assert!(stmt(g, &format!("CREATE (:B:C {{k: {i}}})")).is_ok());
    }
    // Relationships of two types.
    for i in 0..8i64 {
        let j = (i + 1) % 8;
        assert!(
            stmt(
                g,
                &format!("MATCH (a:A {{k: {i}}}), (b:A {{k: {j}}}) CREATE (a)-[:R]->(b)")
            )
            .is_ok()
        );
    }
    for i in 0..4i64 {
        assert!(
            stmt(
                g,
                &format!("MATCH (a:A {{k: {i}}}), (b:B {{k: {i}}}) CREATE (a)-[:S]->(b)")
            )
            .is_ok()
        );
    }
    // Label add and remove — the two single-label call sites.
    assert!(stmt(g, "MATCH (n:A {k: 0}) SET n:Gone").is_ok());
    assert!(stmt(g, "MATCH (n:A {k: 1}) SET n:Gone").is_ok());
    assert!(stmt(g, "MATCH (n:Gone {k: 0}) REMOVE n:Gone").is_ok());
    // A label REMOVED down to zero — the count reaches 0, which is where a
    // vacant-key rule and a saturating one can diverge.
    assert!(stmt(g, "MATCH (n:Gone {k: 1}) REMOVE n:Gone").is_ok());
    // Deletes, including a DETACH DELETE that removes relationships too.
    for i in 20..25i64 {
        assert!(stmt(g, &format!("MATCH (n:A {{k: {i}}}) DELETE n")).is_ok());
    }
    assert!(stmt(g, "MATCH (n:A {k: 2}) DETACH DELETE n").is_ok());
    // Create AND delete inside ONE transaction: the case the seeding comment
    // is about — the delete must see the create's effect, or the count drifts.
    {
        let q = parse_statement("CREATE (:A {k: 999})").expect("parse");
        let q2 = parse_statement("MATCH (n:A {k: 999}) DELETE n").expect("parse");
        let txn = g.open_txn();
        let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
        r.expect("create");
        let (txn, r2) = g.with_txn(txn, || run_query(g, &q2, BTreeMap::new()));
        r2.expect("delete");
        g.commit_owned(txn).expect("commit");
    }
    // A rolled-back transaction contributes NOTHING.
    rolled_back(g, "CREATE (:A {k: 5000})");
    rolled_back(g, "MATCH (n:A {k: 3}) DELETE n");
    // And an out-of-transaction write, which takes the other path entirely.
    run(g, "CREATE (:A {k: 7000})");
}

/// THE differential: both arms must leave identical counts.
#[test]
fn both_arms_leave_identical_counts() {
    let mut arms = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_stats_delta(on);
        drive(&g);
        arms.push(counts(&g));
    }
    assert_eq!(
        arms[0], arms[1],
        "the delta path may change what a count COSTS, never what it IS — \
         these feed the planner"
    );
}

/// The counts must also agree with the ground truth, on both arms. A
/// differential between two equally-wrong implementations proves nothing.
#[test]
fn the_counts_agree_with_the_graph_on_both_arms() {
    for on in [false, true] {
        let g = graph();
        g.set_stats_delta(on);
        drive(&g);
        // `count_label_nodes` reads the maintained counts; the MATCH walks the
        // graph. They must agree, which is what makes the differential above a
        // statement about correctness rather than about consistency.
        for l in ["A", "B", "C", "Gone"] {
            let walked = rows(&g, &format!("MATCH (n:{l}) RETURN n")).len() as u64;
            let counted = g.count_label_nodes(l);
            assert_eq!(
                counted, walked,
                "arm on={on}: the maintained count for :{l} is {counted}, the \
                 walk finds {walked}"
            );
        }
    }
}

/// Non-vacuity: the ON arm really takes the delta path.
#[test]
fn the_lever_actually_switches_the_path() {
    let g = graph();
    g.set_stats_delta(true);
    let (_, on) = engram_observe::with_trace(|| {
        let _ = stmt(&g, "CREATE (:A {k: 1})");
    });
    assert!(
        on.counters()
            .get("graph.stats applied as a delta")
            .copied()
            .unwrap_or(0)
            >= 1,
        "ON arm must accumulate a delta, counters: {:?}",
        on.counters()
    );

    let g2 = graph();
    g2.set_stats_delta(false);
    let (_, off) = engram_observe::with_trace(|| {
        let _ = stmt(&g2, "CREATE (:A {k: 1})");
    });
    assert_eq!(
        off.counters()
            .get("graph.stats applied as a delta")
            .copied()
            .unwrap_or(0),
        0,
        "OFF arm must clone and diff — it is the differential's control"
    );
}

/// The assumption the vacant-key rule rests on: for every reader, an ABSENT
/// label count and a ZERO one are the same thing.
///
/// `StatsDelta::apply` declines to create a zero entry for a negative delta on
/// an absent key, so the delta path stays byte-identical to the diff path. A
/// canary that removed that rule failed NO test — because `count_label_nodes`
/// is the only consumer and it does `.get(&t).unwrap_or(0)`.
///
/// So the rule is insurance, not a fix, and this test says so out loud: it pins
/// the property of the READERS that makes the difference unobservable. If a
/// future consumer starts distinguishing absent from zero, this fails and the
/// rule stops being optional.
#[test]
fn an_absent_label_count_reads_the_same_as_a_zero_one() {
    let g = graph();
    // A label that never existed: absent.
    assert_eq!(
        g.count_label_nodes("NeverSeen"),
        0,
        "a label with no token counts 0"
    );
    // A label driven down to zero: present in the counts, value 0.
    assert!(stmt(&g, "CREATE (:Ghost {k: 1})").is_ok());
    assert_eq!(g.count_label_nodes("Ghost"), 1);
    assert!(stmt(&g, "MATCH (n:Ghost {k: 1}) DELETE n").is_ok());
    assert_eq!(
        g.count_label_nodes("Ghost"),
        0,
        "a label emptied by deletes counts 0, exactly as one that never existed"
    );
    // And the walk agrees with both, which is what makes them interchangeable.
    assert_eq!(rows(&g, "MATCH (n:Ghost) RETURN n").len(), 0);
    assert_eq!(rows(&g, "MATCH (n:NeverSeen) RETURN n").len(), 0);
}
