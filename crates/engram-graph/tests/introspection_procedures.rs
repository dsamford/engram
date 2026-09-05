#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The connect-time introspection procedures (R-5): `db.labels`,
//! `db.relationshipTypes`, `dbms.components`.
//!
//! Drivers and tools call these on connect; refusing them breaks tooling
//! before the first real query. They answer from the maintained stats — no
//! scans — and compose like the data procedures: per input row, YIELD with
//! aliases, WHERE over the yielded fields.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    stmt(&g, "CREATE (:Person {id: 1}), (:Person {id: 2}), (:City {id: 3})");
    stmt(
        &g,
        "MATCH (a:Person {id: 1}), (b:City {id: 3}) CREATE (a)-[:LIVES_IN]->(b)",
    );
    stmt(
        &g,
        "MATCH (a:Person {id: 1}), (b:Person {id: 2}) CREATE (a)-[:KNOWS]->(b)",
    );
    g
}

fn strings(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut out: Vec<String> = rows
        .into_iter()
        .map(|r| match r.into_iter().next() {
            Some(Value::Str(s)) => s,
            other => panic!("expected one string, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

#[test]
fn db_labels_lists_exactly_the_live_labels() {
    let g = fixture();
    let labels = strings(rows(&g, "CALL db.labels() YIELD label RETURN label"));
    assert_eq!(labels, vec!["City".to_string(), "Person".to_string()]);
}

#[test]
fn db_relationship_types_lists_exactly_the_live_types() {
    let g = fixture();
    let types = strings(rows(
        &g,
        "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType",
    ));
    assert_eq!(types, vec!["KNOWS".to_string(), "LIVES_IN".to_string()]);
}

#[test]
fn yields_alias_and_where_compose() {
    let g = fixture();
    let labels = strings(rows(
        &g,
        "CALL db.labels() YIELD label AS l WHERE l <> 'City' RETURN l",
    ));
    assert_eq!(labels, vec!["Person".to_string()]);
}

#[test]
fn db_property_keys_lists_every_minted_key() {
    let g = fixture();
    let keys = strings(rows(
        &g,
        "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey",
    ));
    // The fixture writes exactly one property key. Enumerated from the
    // catalog's reverse rows, not the in-process cache, so a restarted
    // server would answer identically.
    assert_eq!(keys, vec!["id".to_string()]);
}

#[test]
fn dbms_components_names_the_engine() {
    let g = fixture();
    let out = rows(
        &g,
        "CALL dbms.components() YIELD name, versions, edition RETURN name, versions, edition",
    );
    assert_eq!(out.len(), 1, "one component row");
    let row = &out[0];
    assert_eq!(row[0], Value::Str("Engram".into()));
    match &row[1] {
        Value::List(v) => assert_eq!(v.len(), 1, "one version string"),
        other => panic!("versions must be a list, got {other:?}"),
    }
    assert_eq!(row[2], Value::Str("engram".into()));
}

#[test]
fn an_unknown_procedure_still_refuses_by_name() {
    let g = fixture();
    let q = parse_statement("CALL db.schema.visualization() YIELD nodes RETURN nodes")
        .expect("parses");
    let err = run_query(&g, &q, BTreeMap::new()).expect_err("must refuse");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("db.schema.visualization"),
        "the refusal names the procedure: {msg}"
    );
}

#[test]
fn a_wrong_yield_field_is_named_in_the_error() {
    let g = fixture();
    let q = parse_statement("CALL db.labels() YIELD nope RETURN nope").expect("parses");
    let err = run_query(&g, &q, BTreeMap::new()).expect_err("must refuse the field");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("nope") && msg.contains("label"),
        "the error names the bad field and the real one: {msg}"
    );
}
