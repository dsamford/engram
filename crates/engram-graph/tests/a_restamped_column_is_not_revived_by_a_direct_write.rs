#![allow(non_snake_case)]
//! Fix 93 (strategy O4): a cached property column whose property has NO
//! change log keeps its currency across a commit that touched neither its
//! label nor its property — and is still invalidated by everything that
//! should invalidate it.
//!
//! Fix 124 gave the column cache an epoch pair, but `prop_epoch` reads a
//! per-property change log and a log exists only once an index is built over
//! that property. An unindexed property therefore fell all the way back to
//! `at >= now`, under which ANY write anywhere retires the column. That is
//! the hole this closes: the commit knows its own write set, so the columns
//! it did not touch can keep their stamps. No log is held open — the write
//! set is used at the instant it exists and discarded.
//!
//! # The test that matters is `b`
//!
//! The re-stamp is sound only while EVERY path that can invalidate a column
//! is accounted for, and the DIRECT (non-transaction) write path is the one
//! that nearly was not. It records into an existing log only, so for an
//! unindexed property it writes nothing, anywhere. A re-stamp keyed on the
//! commit replay alone would therefore advance such a column past a later,
//! unrelated commit and REVIVE it — serving values a direct write had
//! already replaced, with no error. `Graph::note_direct_prop_write` is what
//! stops that, and `b` is the test that would fail without it.
//!
//! This is the same shape as fix 92: a rule correct for the case it was
//! designed against and wrong for the case that reaches it by another path.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SERVED: &str = "graph.property column served";
const RETIRED: &str = "graph.property column retired by a commit";
const RESTAMPED: &str = "graph.property column re-stamped past an untouching commit";

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

/// `nickname` is deliberately UNINDEXED — that is the whole point. With no
/// index there is no change log, with no log there is no property epoch, and
/// the column falls to the commit clock. This is the population fix 124
/// explicitly declined to help.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("nickname".to_string(), Value::Str(format!("N{i}")));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    g
}

/// A COMMIT that touches neither Person's membership nor `nickname`. It has
/// to be a transaction: the re-stamp attaches to `touch_after_commit`, which
/// only the buffered path reaches. That is also the shape the server runs —
/// every statement there is a transaction.
fn unrelated_commit(g: &Graph, seq: i64) -> BTreeMap<String, u64> {
    let (_, t) = engram_observe::with_trace(|| {
        g.begin_txn().expect("begin");
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(1_000_000 + seq));
        m.insert("content".to_string(), Value::Str("stress".into()));
        g.create_node(&["Message".into()], &m).expect("message");
        g.commit_txn().expect("commit");
    });
    t.counters().clone()
}

const NICK_LISTING: &str = "MATCH (p:Person) RETURN p.nickname AS nick LIMIT 50";

fn nicks(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| match r.first() {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_an_unindexed_columns_currency_survives_an_untouching_commit() {
    let g = corpus();
    g.set_prop_column_restamp(true);
    let want = rows(&g, NICK_LISTING);
    assert_eq!(want.len(), 50);
    let _ = rows(&g, NICK_LISTING); // cached by now

    for seq in 0..5 {
        let wc = unrelated_commit(&g, seq);
        // The CANARY: the mechanism has to have fired, or this test passes
        // because nothing happened rather than because the fix works.
        assert!(
            count_of(&wc, RESTAMPED) > 0,
            "round {seq}: the commit re-stamped nothing — the column was not \
             in the cache, or the lever did not reach the commit: {wc:?}"
        );
        let (got, c) = traced(&g, NICK_LISTING);
        assert_eq!(got, want, "round {seq}: the answer moved");
        assert_eq!(
            count_of(&c, RETIRED),
            0,
            "round {seq}: an untouching commit retired the column: {c:?}"
        );
        assert!(
            count_of(&c, SERVED) > 0,
            "round {seq}: the column was not served at all: {c:?}"
        );
    }
}

#[test]
fn b_a_write_to_the_property_is_never_revived_by_a_later_untouching_commit() {
    let g = corpus();
    g.set_prop_column_restamp(true);
    let before = nicks(&rows(&g, NICK_LISTING));
    assert!(before.iter().any(|n| n == "N7"), "corpus: {before:?}");
    let _ = rows(&g, NICK_LISTING); // cached

    // The write the re-stamp must not erase. For an unindexed property this
    // records into no log at all, so nothing about it is remembered beyond
    // the commit clock.
    ddl(
        &g,
        "MATCH (p:Person) WHERE p.id = 7 SET p.nickname = 'CHANGED'",
    );

    // ...and now a commit that touches NEITHER Person's membership NOR
    // `nickname`. This is the step that revives the column if the direct
    // write went unaccounted.
    let _ = unrelated_commit(&g, 0);

    let after = nicks(&rows(&g, NICK_LISTING));
    assert!(
        after.iter().any(|n| n == "CHANGED"),
        "the write to nickname was lost: {after:?}"
    );
    assert!(
        !after.iter().any(|n| n == "N7"),
        "A STALE VALUE WAS SERVED. The column was revived past a write to its \
         own property — the re-stamp advanced a stamp it had no right to. \
         This is the silent wrong answer the fix exists to avoid: {after:?}"
    );
}

#[test]
fn c_a_commit_that_only_changes_membership_still_retires_the_column() {
    let g = corpus();
    g.set_prop_column_restamp(true);
    let _ = rows(&g, NICK_LISTING);
    let _ = rows(&g, NICK_LISTING); // cached

    // A new Person with NO nickname: the commit touches the LABEL and no
    // property of the cached column. Its id set is nonetheless stale, and
    // `label_at` is what has to catch it — the re-stamp only advances the
    // commit clock and would sail straight past.
    g.begin_txn().expect("begin");
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(-1));
    g.create_node(&["Person".into()], &m).expect("person");
    g.commit_txn().expect("commit");

    let (_, c) = traced(&g, NICK_LISTING);
    assert!(
        count_of(&c, SERVED) == 0 || count_of(&c, RETIRED) > 0,
        "a membership change was re-stamped past — the column's id set is \
         stale and it was served anyway: {c:?}"
    );
}

#[test]
fn d_the_restamp_is_off_by_default_and_the_old_regime_stands() {
    let g = corpus(); // no set_prop_column_restamp — the shipped default
    let _ = rows(&g, NICK_LISTING);
    let _ = rows(&g, NICK_LISTING); // cached

    let wc = unrelated_commit(&g, 0);
    assert_eq!(
        count_of(&wc, RESTAMPED),
        0,
        "the re-stamp ran with the lever off: {wc:?}"
    );
    let (_, c) = traced(&g, NICK_LISTING);
    assert!(
        count_of(&c, RETIRED) > 0,
        "with the lever off an unrelated commit must still retire the column \
         — the default has changed behaviour, which it must not: {c:?}"
    );
}
