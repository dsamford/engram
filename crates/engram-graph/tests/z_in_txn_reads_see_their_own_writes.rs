#![allow(non_snake_case)]
//! A statement must see its OWN writes, including through the skips that
//! `mat_end` takes on a snapshot.
//!
//! `mat_end` binds a hop end without reading its record when nothing reads it,
//! and rejects a non-member straight from the label's membership. Both trust a
//! SNAPSHOT, and both are gated on `!graph.in_txn_with_writes()` because inside
//! a writing transaction the overlay's buffered labels must win over a snapshot
//! that predates them.
//!
//! These tests pin that guarantee from the outside, so it can be checked
//! against any narrowing of that guard: the question a narrower guard must keep
//! answering is not "does this statement write" but "could this write change
//! what THIS read returns".

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 99) AS i CREATE (:P {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 98) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) \
         CREATE (a)-[:T {w: 1.0}]->(b)",
    );
    g
}

#[test]
fn a_label_added_in_this_statement_is_visible_to_a_later_match() {
    // The membership snapshot predates the SET. If a skip trusted it, the
    // second MATCH would see zero.
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 0}) SET p:Q WITH count(p) AS added \
         MATCH (x:Q) RETURN added, count(x) AS seen",
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][1],
        Value::Int(1),
        "the label written by this statement must be visible: {out:?}"
    );
}

#[test]
fn a_property_written_in_this_statement_is_visible_to_a_later_match() {
    // The property COLUMN is a snapshot too, and `mat_end` can bind an end
    // from it without reading the record.
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 5}) SET p.tag = 'written' WITH count(p) AS n \
         MATCH (q:P) WHERE q.tag = 'written' RETURN n, count(q) AS seen",
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][1],
        Value::Int(1),
        "the property written here must be visible: {out:?}"
    );
}

#[test]
fn a_hop_END_whose_label_this_statement_added_is_matched() {
    // The endpoint is the one `mat_end` decides about, and its membership
    // changed inside this statement.
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 7}) SET p:Q WITH count(p) AS n \
         MATCH (a:P)-[:T]->(b:Q) RETURN n, count(b) AS seen",
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][1],
        Value::Int(1),
        "the hop end's new label must be seen: {out:?}"
    );
}

#[test]
fn a_node_DELETED_in_this_statement_is_not_matched_afterwards() {
    // The other direction: a skip that binds an end "bare" from an adjacency
    // entry must not resurrect a node this statement removed.
    let g = graph();
    ddl(&g, "CREATE (:Z {id: 1})");
    let out = rows(
        &g,
        // OPTIONAL, because a plain MATCH finding nothing eliminates the row
        // and the test would then assert about an empty result rather than
        // about the delete -- which is how the first cut of this case passed
        // for the wrong reason.
        "MATCH (z:Z) DELETE z WITH count(*) AS gone \
         OPTIONAL MATCH (x:Z) RETURN gone, count(x) AS left_over",
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][1],
        Value::Int(0),
        "a deleted node must not be matched: {out:?}"
    );
}
