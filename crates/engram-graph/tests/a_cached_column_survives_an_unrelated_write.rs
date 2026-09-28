#![allow(non_snake_case)]
//! Fix 124: a cached property column is judged current by the (label,
//! property) EPOCH PAIR, not by the global commit clock.
//!
//! `PropColumnEntry.at` was compared against `Store::now_ts()`, which every
//! write bumps — so a commit that created a `Message` retired the cached
//! `(Person, firstName)` column, and the next listing rebuilt it from
//! records. In the read-heavy platform mix that is most commits: the writes
//! create Messages and touch neither Person's membership nor `firstName`.
//! `plat-limit-listing` measured 21.91 ms read-only against 25.64 ms
//! read-heavy for exactly this reason.
//!
//! This is not a new mechanism. `members_at_token` already tests
//! `label_epoch` and `ensure_range_index_scoped` already tests `prop_epoch`;
//! the property-column cache was the one derived structure that never got the
//! protocol.
//!
//! # The hole this had to avoid
//!
//! `prop_epoch` reads a per-property change log, and a log exists only once
//! an index has been built over that property. For a property with NO log it
//! returns 0 for ever — so keying currency on it would mean the column never
//! invalidates, which is a wrong answer rather than a slow one. Fix 124
//! stamps `epochs: None` in that case and keeps the commit-clock test. Test
//! `c` is that case, and it is the one that would have shipped a silent
//! wrong answer.
//!
//! Canary, run: making `prop_column_epochs` return `Some` unconditionally —
//! that is, ignoring the missing log — fails `c` with a stale value served
//! after a write.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SERVED: &str = "graph.property column served";
const RETIRED: &str = "graph.property column retired by a commit";
const SURVIVED: &str = "graph.property column survived a commit that touched neither epoch";

const PERSONS: i64 = 300;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, t) = engram_observe::with_trace(|| rows(g, src));
    (r, t.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// `firstName` is INDEXED, which is what gives the property a change log and
/// therefore a trustworthy epoch. `nickname` deliberately is not.
fn corpus(index_first_name: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    if index_first_name {
        ddl(&g, "CREATE INDEX p_first FOR (n:Person) ON (n.firstName)");
    }
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("firstName".to_string(), Value::Str(format!("P{i}")));
        m.insert("nickname".to_string(), Value::Str(format!("N{i}")));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    if index_first_name {
        // The change log a property's epoch reads is created by the index
        // BUILD, not by the declaration — the build runs on first seek. Without
        // this the epoch would be untrustworthy and fix 124 would correctly
        // decline to use it, so the test would exercise the old regime while
        // claiming to exercise the new one.
        // An inline-map seek is answered from the property column and never
        // probes the index, so it does NOT create the log. A counted equality
        // over the property does — verified by counter, not by assumption.
        let _ = rows(
            &g,
            "MATCH (p:Person) WHERE p.firstName = 'P5' RETURN count(p) AS n",
        );
    }
    g
}

/// A write that touches NEITHER Person's membership NOR `firstName`: it
/// creates a Message. This is the platform mix's write, in miniature.
fn unrelated_write(g: &Graph, seq: i64) {
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(1_000_000 + seq));
    m.insert("content".to_string(), Value::Str("stress".into()));
    g.create_node(&["Message".into()], &m).expect("message");
}

const LISTING: &str = "MATCH (p:Person) RETURN p.firstName AS name LIMIT 50";
const NICK_LISTING: &str = "MATCH (p:Person) RETURN p.nickname AS nick LIMIT 50";

#[test]
fn a_an_unrelated_write_no_longer_retires_the_column() {
    let g = corpus(true);
    let want = rows(&g, LISTING);
    assert_eq!(want.len(), 50);
    let _ = rows(&g, LISTING); // the column is cached by now

    for seq in 0..5 {
        unrelated_write(&g, seq);
        let (got, c) = traced(&g, LISTING);
        assert_eq!(got, want, "round {seq}: the answer moved");
        assert_eq!(
            count_of(&c, RETIRED),
            0,
            "round {seq}: a Message write retired the Person.firstName column: {c:?}"
        );
        assert!(
            count_of(&c, SERVED) > 0,
            "round {seq}: the column was not served at all: {c:?}"
        );
    }
}

#[test]
fn b_the_survival_is_counted_so_a_run_can_say_which_regime_it_is_in() {
    let g = corpus(true);
    let _ = rows(&g, LISTING);
    let _ = rows(&g, LISTING);
    unrelated_write(&g, 0);
    let (_, c) = traced(&g, LISTING);
    assert!(
        count_of(&c, SURVIVED) > 0,
        "the column survived but nothing said so, which is how the previous \
         regime stayed invisible for so long: {c:?}"
    );
}

#[test]
fn c_a_property_with_no_change_log_keeps_the_commit_clock_test() {
    // `nickname` is never indexed, so no change log is ever built for it and
    // `prop_epoch` returns 0 for ever. Keying currency on that would mean the
    // column NEVER invalidates — a wrong answer, not a slow one. The entry
    // must fall back to the commit clock.
    //
    // Asserted on the ANSWER rather than on the decline counter: the counter
    // fires when the column is KEPT, which is not the statement under test,
    // and an earlier draft asserted it on the read and failed for that reason.
    let g = corpus(false);
    let before = rows(&g, NICK_LISTING);
    let _ = rows(&g, NICK_LISTING); // cached by now

    run_query(
        &g,
        &parse_statement("MATCH (p:Person {id: 0}) SET p.nickname = 'CHANGED'").expect("parse"),
        BTreeMap::new(),
    )
    .expect("set");
    let after = rows(&g, NICK_LISTING);
    assert_ne!(
        before, after,
        "the stale nickname column was served after a write"
    );
    assert!(
        after.contains(&vec![Value::Str("CHANGED".into())]),
        "the updated nickname is missing: {after:?}"
    );
}

#[test]
fn d_a_write_that_DOES_touch_the_property_still_retires_it() {
    // The other control. Epoch currency must not become "never invalidate":
    // a write to `firstName` itself has to be seen.
    let g = corpus(true);
    let before = rows(&g, LISTING);
    let _ = rows(&g, LISTING);
    run_query(
        &g,
        &parse_statement("MATCH (p:Person {id: 0}) SET p.firstName = 'CHANGED'").expect("parse"),
        BTreeMap::new(),
    )
    .expect("set");
    let after = rows(&g, LISTING);
    assert_ne!(
        before, after,
        "a write to firstName did not retire its column"
    );
    assert!(
        after.contains(&vec![Value::Str("CHANGED".into())]),
        "the updated firstName is missing from the listing: {after:?}"
    );
}

#[test]
fn e_the_lever_off_restores_the_commit_clock_behaviour() {
    let g = corpus(true);
    g.set_prop_column_epoch_currency(false);
    let want = rows(&g, LISTING);
    let _ = rows(&g, LISTING);
    unrelated_write(&g, 0);
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want, "the OFF arm changed an answer");
    assert!(
        count_of(&c, RETIRED) > 0,
        "with the lever OFF an unrelated write should still retire the column: {c:?}"
    );
}
