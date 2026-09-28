#![allow(non_snake_case)]
//! An SNB temporal keeps its TYPE from the CSV through to the store.
//!
//! The converter used to flatten `creationDate` to epoch millis. The INSTANT
//! survived exactly and every count reconciled, so nothing in the load path
//! could tell — but Cypher could. `Int < DateTime` is not an error, it is
//! FALSE, so the SNB BI queries that filter on `creationDate < $datetime`
//! returned zero rows and were recorded as clean executions (bi1, bi9, bi13 at
//! SF3, measured 2026-09-14). The two that cannot be written against an
//! integer at all — `date(message.creationDate)` in bi16 and
//! `creationDate + duration({hours: ...})` in bi17 — failed loudly, which is
//! the only reason the silent three were ever looked at.
//!
//! So the guard is not "does the value round-trip". It is: does a comparison
//! against a `datetime()` literal SELECT THE ROW. That is the question the
//! benchmark asks, and a type error cannot hide from it.
//!
//! FinBench is deliberately the other way and is not touched here: its
//! published reference compares `e.timestamp` against INTEGER parameters, so
//! epoch millis is the correct type for that corpus.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn one(g: &Graph, src: &str) -> Value {
    run(g, src)
        .rows
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null)
}

/// One Message at 2010-02-14T17:32:10.447Z, written the way `snbload` now
/// spells a tagged temporal.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        "CREATE (:Message {id: 1, creationDate: datetime('2010-02-14T17:32:10.447Z')})",
    );
    g
}

#[test]
fn a_datetime_property_is_selected_by_a_datetime_comparison() {
    // THE regression. With an integer in the store this returns 0, without any
    // error, which is what made the defect invisible for the whole BI battery.
    let g = graph();
    assert_eq!(
        one(
            &g,
            "MATCH (m:Message) WHERE m.creationDate < datetime('2011-12-01T00:00:00.000') \
             RETURN count(m)"
        ),
        Value::Int(1),
        "a Message from 2010 must be selected by `< datetime('2011-12-01')`; \
         an integer property compares FALSE here and reports zero rows rather \
         than failing"
    );
    // And the other side of the comparison, so a property that matched
    // everything would not pass either.
    assert_eq!(
        one(
            &g,
            "MATCH (m:Message) WHERE m.creationDate > datetime('2011-12-01T00:00:00.000') \
             RETURN count(m)"
        ),
        Value::Int(0),
        "the comparison must still DISCRIMINATE"
    );
}

#[test]
fn the_accessors_and_arithmetic_BI_needs_all_work_on_it() {
    // bi1 and bi13 read `.year` / `.month`; bi16 calls `date()`; bi17 adds a
    // duration. None of the four is expressible against an integer, and each
    // fails differently, so each is asserted rather than sampled.
    let g = graph();
    assert_eq!(
        one(&g, "MATCH (m:Message) RETURN m.creationDate.year"),
        Value::Int(2010),
        "bi1 groups by `.year`"
    );
    assert_eq!(
        one(&g, "MATCH (m:Message) RETURN m.creationDate.month"),
        Value::Int(2),
        "bi13 computes months from `.year` and `.month`"
    );
    assert_eq!(
        one(&g, "MATCH (m:Message) RETURN date(m.creationDate)"),
        Value::Date(14_654),
        "bi16 compares `date(message.creationDate)` against `date(param)`; \
         2010-02-14 is day 14654"
    );
    assert_eq!(
        one(
            &g,
            "MATCH (m:Message) RETURN m.creationDate + duration({hours: 4}) > \
             datetime('2010-02-14T21:00:00.000')"
        ),
        Value::Bool(true),
        "bi17 shifts a creationDate by `duration({{hours: $delta}})`; \
         17:32:10 + 4h is 21:32:10, which is after 21:00"
    );
}
