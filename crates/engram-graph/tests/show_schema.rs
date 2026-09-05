//! `SHOW INDEXES` / `SHOW CONSTRAINTS` (R-5): the catalogue listings, in
//! Neo4j's column vocabulary — tools key on those exact column names.
//!
//! SHOW is the one schema command that ANSWERS rather than mutates. The
//! parser swallows any tail after the subject unvalidated, so a YIELD/WHERE
//! tail must refuse rather than answer with the projection silently ignored.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, QueryResult, RunError, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> QueryResult {
    try_run(g, src).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn try_run(g: &Graph, src: &str) -> Result<QueryResult, RunError> {
    let stmt = parse_any(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_stmt(g, &stmt, BTreeMap::new())
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn l(items: &[&str]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

#[test]
fn show_indexes_lists_every_kind_with_its_scope() {
    let g = graph();
    run(&g, "CREATE INDEX ix_node FOR (n:Person) ON (n.name)");
    // The relationship scope must survive storage — CREATE used to discard
    // it, which both misreported the index here and let the node planner
    // consult it.
    run(&g, "CREATE INDEX ix_rel FOR ()-[r:KNOWS]-() ON (r.since)");
    run(&g, "CREATE VECTOR INDEX ix_vec FOR (n:Doc) ON (n.embedding)");
    run(
        &g,
        "CREATE FULLTEXT INDEX ix_full FOR (n:Doc|Post) ON EACH [n.title, n.body]",
    );
    let res = run(&g, "SHOW INDEXES");
    assert_eq!(
        res.columns,
        [
            "name",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "state"
        ]
        .map(String::from)
        .to_vec()
    );
    // Name-ordered, one row per stored index.
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("ix_full"),
                s("FULLTEXT"),
                s("NODE"),
                l(&["Doc", "Post"]),
                l(&["title", "body"]),
                s("ONLINE"),
            ],
            vec![
                s("ix_node"),
                s("RANGE"),
                s("NODE"),
                l(&["Person"]),
                l(&["name"]),
                s("ONLINE"),
            ],
            vec![
                s("ix_rel"),
                s("RANGE"),
                s("RELATIONSHIP"),
                l(&["KNOWS"]),
                l(&["since"]),
                s("ONLINE"),
            ],
            vec![
                s("ix_vec"),
                s("VECTOR"),
                s("NODE"),
                l(&["Doc"]),
                l(&["embedding"]),
                s("ONLINE"),
            ],
        ]
    );
}

#[test]
fn show_constraints_names_kind_and_scope() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT c_key FOR (n:Account) REQUIRE (n.realm, n.id) IS NODE KEY",
    );
    run(
        &g,
        "CREATE CONSTRAINT c_notnull_rel FOR ()-[r:KNOWS]-() REQUIRE r.since IS NOT NULL",
    );
    run(
        &g,
        "CREATE CONSTRAINT c_unique FOR (n:Person) REQUIRE n.id IS UNIQUE",
    );
    let res = run(&g, "SHOW CONSTRAINTS");
    assert_eq!(
        res.columns,
        ["name", "type", "entityType", "labelsOrTypes", "properties"]
            .map(String::from)
            .to_vec()
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("c_key"),
                s("NODE_KEY"),
                s("NODE"),
                l(&["Account"]),
                l(&["realm", "id"]),
            ],
            vec![
                s("c_notnull_rel"),
                s("RELATIONSHIP_PROPERTY_EXISTENCE"),
                s("RELATIONSHIP"),
                l(&["KNOWS"]),
                l(&["since"]),
            ],
            vec![
                s("c_unique"),
                s("UNIQUENESS"),
                s("NODE"),
                l(&["Person"]),
                l(&["id"]),
            ],
        ]
    );
}

#[test]
fn an_empty_catalogue_answers_zero_rows_not_an_error() {
    let g = graph();
    let idx = run(&g, "SHOW INDEXES");
    assert_eq!(idx.columns.len(), 6);
    assert!(idx.rows.is_empty());
    let con = run(&g, "SHOW CONSTRAINTS");
    assert_eq!(con.columns.len(), 5);
    assert!(con.rows.is_empty());
}

#[test]
fn a_dropped_index_leaves_the_listing() {
    let g = graph();
    run(&g, "CREATE INDEX ix FOR (n:Person) ON (n.name)");
    assert_eq!(run(&g, "SHOW INDEXES").rows.len(), 1);
    run(&g, "DROP INDEX ix");
    assert!(run(&g, "SHOW INDEXES").rows.is_empty());
}

#[test]
fn lowercase_show_answers_too() {
    let g = graph();
    run(&g, "CREATE INDEX ix FOR (n:Person) ON (n.name)");
    assert_eq!(run(&g, "show indexes").rows.len(), 1);
}

#[test]
fn an_unimplemented_subject_still_refuses_by_name() {
    let g = graph();
    let err = try_run(&g, "SHOW PROCEDURES").expect_err("must refuse");
    let msg = format!("{err:?}");
    assert!(msg.contains("PROCEDURES"), "names the subject: {msg}");
}

#[test]
fn a_swallowed_tail_refuses_rather_than_answering() {
    let g = graph();
    run(&g, "CREATE INDEX ix FOR (n:Person) ON (n.name)");
    // The parser consumed `YIELD name` without validating it; answering the
    // full table here would silently ignore the caller's projection.
    let err = try_run(&g, "SHOW INDEXES YIELD name").expect_err("must refuse the tail");
    let msg = format!("{err:?}");
    assert!(msg.contains("INDEXES"), "names the subject: {msg}");
}
