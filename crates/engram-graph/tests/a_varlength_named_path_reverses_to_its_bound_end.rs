#![allow(non_snake_case)]
//! A variable-length, NAMED path whose far end is bound must be driven from
//! that end — and must still bind EXACTLY what the forward walk binds.
//!
//! FinBench tcr2 writes
//! `MATCH (person:Person {id: $id})-[:own]->(account:Account),
//!        p=(other:Account)-[transfer:transfer*1..3]->(account)`
//! so `account` is bound when the second path is matched and `other` is not.
//! Driven forward, that path seeds from EVERY account. Measured on a 2,000
//! account fixture: 13,993 `store.gets` forward against 9 with the path
//! reversed by hand — and at SF10 the forward form ran into a 600 s ceiling.
//!
//! `reverse_bound_end_path` declined it twice over: the path is NAMED, and the
//! hop is VARIABLE-LENGTH. Both refusals protect something real. Reversing a
//! named path reverses `nodes(p)` and `relationships(p)`, and tcr2 filters on
//! timestamps being STRICTLY INCREASING along the path; reversing a
//! variable-length hop reverses the LIST its relationship variable binds, so
//! `transfer[0]` would name the wrong edge.
//!
//! So these tests compare the SAME statement with hop reversal OFF and ON,
//! row for row after sorting, over exactly the order-sensitive projections.

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
    let mut out = run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows;
    out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    out
}

fn counter(g: &Graph, q: &str, name: &str) -> u64 {
    let s = parse_statement(q).expect("parses");
    let (_, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new()).expect("runs");
    });
    t.counters().get(name).copied().unwrap_or(0)
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 299) AS i CREATE (:Account {id: i})");
    ddl(&g, "UNWIND range(0, 9) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 9) AS i MATCH (p:Person {id: i}), (a:Account {id: i}) \
         CREATE (p)-[:own]->(a)",
    );
    // Several edges INTO the low ids, with distinct timestamps, so paths into
    // a person's account exist at lengths 1, 2 and 3 and their timestamp order
    // is not monotone by construction.
    ddl(
        &g,
        "UNWIND range(10, 299) AS i MATCH (a:Account {id: i}), (b:Account {id: i % 23}) \
         CREATE (a)-[:transfer {timestamp: (i * 37) % 101}]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(10, 299) AS i MATCH (a:Account {id: i}), (b:Account {id: 10 + (i * 11) % 290}) \
         CREATE (a)-[:transfer {timestamp: (i * 53) % 97}]->(b)",
    );
    g
}

/// Same query, reversal OFF then ON. Identical sorted rows or the test fails.
fn both_ways(q: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let g = graph();
    g.set_hop_reversal(false);
    let off = rows(&g, q);
    g.set_hop_reversal(true);
    let on = rows(&g, q);
    (off, on)
}

const BOUND_END: &str = "MATCH (person:Person {id: 3})-[:own]->(account:Account), \
                         p=(other:Account)-[transfer:transfer*1..3]->(account)";

#[test]
fn nodes_of_a_named_path_come_back_in_WRITTEN_order() {
    let q = format!("{BOUND_END} RETURN [n IN nodes(p) | n.id] AS ns");
    let (off, on) = both_ways(&q);
    assert!(!off.is_empty(), "the fixture must produce paths");
    assert_eq!(
        off, on,
        "nodes(p) must start at `other` and end at `account` either way"
    );
}

#[test]
fn relationships_of_a_named_path_come_back_in_WRITTEN_order() {
    let q = format!("{BOUND_END} RETURN [r IN relationships(p) | r.timestamp] AS ts");
    let (off, on) = both_ways(&q);
    assert!(!off.is_empty());
    assert_eq!(off, on);
}

#[test]
fn a_varlength_relationship_variable_keeps_its_WRITTEN_order() {
    // `transfer[0]` must be the edge that leaves `other`, not the one that
    // arrives at `account`.
    let q = format!(
        "{BOUND_END} RETURN [r IN transfer | r.timestamp] AS ts, transfer[0].timestamp AS first"
    );
    let (off, on) = both_ways(&q);
    assert!(!off.is_empty());
    assert_eq!(off, on);
}

#[test]
fn tcr2s_strictly_increasing_timestamp_filter_answers_identically() {
    // The order-sensitive predicate itself, verbatim in shape.
    let q = format!(
        "{BOUND_END} WITH p, [e IN relationships(p) | e.timestamp] AS ts, other \
         WHERE reduce(curr = head(ts), x IN tail(ts) | \
           CASE WHEN curr < x THEN x ELSE 9223372036854775807 END) <> 9223372036854775807 \
         RETURN DISTINCT other.id AS o"
    );
    let (off, on) = both_ways(&q);
    assert!(
        !off.is_empty(),
        "the increasing-timestamp filter must admit some paths"
    );
    assert_eq!(off, on);
}

#[test]
fn every_differential_above_really_ran_REVERSED() {
    // Non-vacuity, checked on the SAME shapes the differentials compare. A
    // first version checked a `RETURN count(*)` spelling instead — which a
    // different planner answers without reaching the reversal at all — so it
    // could neither confirm nor refute that the comparisons above were
    // anything other than forward against forward.
    let g = graph();
    g.set_hop_reversal(true);
    for tail in [
        "RETURN [n IN nodes(p) | n.id] AS ns",
        "RETURN [r IN relationships(p) | r.timestamp] AS ts",
        "RETURN [r IN transfer | r.timestamp] AS ts, transfer[0].timestamp AS first",
    ] {
        let q = format!("{BOUND_END} {tail}");
        let hits = counter(&g, &q, "interp.path driven from its bound end")
            + counter(&g, &q, "interp.hop driven from its bound end");
        assert!(hits > 0, "`{tail}` must be matched from the bound end");
    }
}
