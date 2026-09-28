#![allow(non_snake_case)]
//! What a `SET` actually DEMANDS of the entity it writes.
//!
//! `demands_after` walks the clauses after a MATCH to work out which
//! properties each variable still needs, so the per-row matcher can bind a hop
//! end leanly instead of decoding its whole record. Its catch-all stamps
//! `DEMAND_EVERYTHING` for any clause it "cannot see through" — which includes
//! every write — and its own doc comment gives that as the reason: analysis
//! incompleteness, NOT correctness. Nothing in the engine claims a writing
//! statement must materialise in full.
//!
//! These tests pin the SEMANTICS a narrower demand has to preserve. They are
//! written against the engine as it is, so they pass before the change and
//! must still pass after it — which is the only thing that makes them worth
//! having.

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
    ddl(
        &g,
        "UNWIND range(0, 19) AS i CREATE (:P {id: i, keep: i * 10})",
    );
    ddl(
        &g,
        "UNWIND range(0, 18) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) \
         CREATE (a)-[:T {w: i * 1.0}]->(b)",
    );
    g
}

#[test]
fn a_constant_SET_still_writes_and_the_node_keeps_its_other_properties() {
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 3}) SET p.x = 1 RETURN p.x, p.keep, p.id",
    );
    assert_eq!(
        out[0],
        vec![Value::Int(1), Value::Int(30), Value::Int(3)],
        "{out:?}"
    );
}

#[test]
fn a_SET_that_READS_its_own_property_computes_from_the_stored_value() {
    // The case a narrower demand must NOT get wrong: the right-hand side reads
    // `p.keep`, so `keep` is demanded even though the statement writes.
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 4}) SET p.derived = p.keep + 1 RETURN p.derived",
    );
    assert_eq!(out[0], vec![Value::Int(41)], "{out:?}");
}

#[test]
fn a_RELATIONSHIP_SET_reading_its_own_property_is_computed_from_the_stored_value() {
    let g = graph();
    let out = rows(
        &g,
        "MATCH (:P {id: 5})-[r:T]->(:P) SET r.w = r.w + 100.0 RETURN r.w",
    );
    assert_eq!(out[0], vec![Value::Float(105.0)], "{out:?}");
}

#[test]
fn a_MAP_REPLACE_drops_the_properties_it_does_not_mention() {
    let g = graph();
    let out = rows(&g, "MATCH (p:P {id: 6}) SET p = {q: 7} RETURN p.q, p.keep");
    assert_eq!(
        out[0],
        vec![Value::Int(7), Value::Null],
        "map assign replaces: {out:?}"
    );
}

#[test]
fn a_MAP_MERGE_keeps_the_properties_it_does_not_mention() {
    let g = graph();
    let out = rows(&g, "MATCH (p:P {id: 7}) SET p += {q: 7} RETURN p.q, p.keep");
    assert_eq!(
        out[0],
        vec![Value::Int(7), Value::Int(70)],
        "map merge keeps: {out:?}"
    );
}

#[test]
fn the_WHOLE_NODE_returned_after_a_SET_carries_every_property() {
    // If the matcher bound this node leanly and the projection then returned
    // it whole, the missing properties would show up here.
    let g = graph();
    let out = rows(&g, "MATCH (p:P {id: 8}) SET p.x = 1 RETURN p");
    match &out[0][0] {
        Value::Node { props, .. } => {
            assert_eq!(props.get("id"), Some(&Value::Int(8)), "{props:?}");
            assert_eq!(props.get("keep"), Some(&Value::Int(80)), "{props:?}");
            assert_eq!(props.get("x"), Some(&Value::Int(1)), "{props:?}");
        }
        other => panic!("expected a node: {other:?}"),
    }
}

#[test]
fn a_clause_AFTER_the_SET_that_reads_the_var_still_sees_every_property() {
    // `demands_after` currently BREAKS at a write. A version that walks past
    // one must keep collecting what the later clauses demand.
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 9}) SET p.x = 1 WITH p RETURN p.keep, p.x, p.id",
    );
    assert_eq!(
        out[0],
        vec![Value::Int(90), Value::Int(1), Value::Int(9)],
        "{out:?}"
    );
}

#[test]
fn a_SET_of_a_LABEL_leaves_the_existing_labels_in_place() {
    let g = graph();
    let out = rows(
        &g,
        "MATCH (p:P {id: 10}) SET p:Extra WITH p MATCH (q:P:Extra) RETURN count(q) AS n",
    );
    assert_eq!(
        out[0],
        vec![Value::Int(1)],
        "both labels must hold: {out:?}"
    );
}
