#![allow(non_snake_case)]
//! The core (`recognise_core`) read chain now SEEKS an inline `(a:L {id: val})`
//! start anchor through the range index instead of scanning the whole label —
//! the fix for IS5 (`MATCH (m:Message {id: X})-[:HAS_CREATOR]->(p)`), a point
//! lookup that scanned ALL ~2M Messages (95 ms) where Neo4j sought the id (1 ms).
//!
//! Contract (every `pipeline_*.rs`'s): for the accepted shape, columnar
//! (`set_columnar_scans(true)`) equals the general `run_streaming` path byte-for-
//! byte, AND — above the seek floor — the anchored seed COUNTER fires (a scan of
//! the whole label would not). The anchor equality also rides in the WHERE, so the
//! seeded result equals a whole-label scan then that filter.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SEEK_COUNTER: &str = "interp.pipeline anchored seed sought a property index";

/// `n` Persons (id 0..n) and `n` Messages (id 1000+i), each Message HAS_CREATOR
/// its Person. `n` chosen by the caller to sit above or below the seek floor.
fn g(n: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let persons: Vec<u64> = (0..n).map(|i| mk("Person", i)).collect();
    for i in 0..n {
        let msg = mk("Message", 1000 + i);
        g.create_rel(msg, "HAS_CREATOR", persons[i as usize], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn seek_count(g: &Graph, src: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(SEEK_COUNTER).copied().unwrap_or(0)
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

const IS5: &str = "MATCH (m:Message {id: 1500})-[:HAS_CREATOR]->(p:Person) RETURN p.id AS personId";

/// ABOVE the seek floor (600 Messages > 512): the core chain SEEKS the Message id
/// index and expands only that one message's creator — ON==OFF, correct, and the
/// anchored-seed counter fires (a whole-label scan would leave it at 0).
#[test]
fn is5_shape_seeks_above_floor() {
    let g = g(600);
    let (on, off) = both(&g, IS5);
    assert_eq!(on, off, "IS5 columnar vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(500)]],
        "message 1500 (index 500) → person 500"
    );
    assert!(
        seek_count(&g, IS5) >= 1,
        "above the floor the core anchor must SEEK the id index, not scan the label"
    );
}

/// A `$param` anchor value resolves at seed time and seeks identically.
#[test]
fn is5_param_anchor_seeks() {
    let g = g(600);
    let src = "MATCH (m:Message {id: $mid})-[:HAS_CREATOR]->(p:Person) RETURN p.id AS personId";
    let q = parse_statement(src).unwrap();
    let mut params = BTreeMap::new();
    params.insert("mid".to_string(), i(1500));
    g.set_columnar_scans(true);
    let (on, trace) =
        engram_observe::with_trace(|| run_query(&g, &q, params.clone()).unwrap().rows);
    g.set_columnar_scans(false);
    let off = run_query(&g, &q, params).unwrap().rows;
    g.set_columnar_scans(true);
    assert_eq!(on, off, "param-anchor IS5 ON must equal OFF");
    assert_eq!(on, vec![vec![i(500)]]);
    assert!(
        trace.counters().get(SEEK_COUNTER).copied().unwrap_or(0) >= 1,
        "the param anchor must seek the index"
    );
}

/// BELOW the seek floor (50 Messages < 512): the core chain SCANS the label (the
/// seek is not worth probing on a tiny label) — still correct, ON==OFF, and the
/// seek counter stays 0. Confirms the anchor path did not change the result on the
/// scan side, only the seed strategy above the floor.
#[test]
fn is5_shape_scans_below_floor() {
    let g = g(50);
    let src = "MATCH (m:Message {id: 1025})-[:HAS_CREATOR]->(p:Person) RETURN p.id AS personId";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "below-floor IS5 ON must equal OFF");
    assert_eq!(on, vec![vec![i(25)]], "message 1025 (index 25) → person 25");
    assert_eq!(
        seek_count(&g, src),
        0,
        "below the floor a tiny label is scanned, not sought"
    );
}

/// The IC8 SHAPE — an anchor on the START node feeding a chain. The core executor
/// applies the anchor WHERE *after* the hops, so a whole-label scan expands EVERY
/// person's chain and only then filters to the anchor. Seeking the anchor seeds
/// ONE person, so the chain expands from that person alone — the same rows in the
/// same order (ON==OFF), but without the whole-label fan-out (IC8 218 ms → fast).
/// Here: `(p:Person {id: 300})<-[:HAS_CREATOR]-(m:Message)` — person 300 authored
/// message 1300 only.
#[test]
fn anchor_restricts_a_chain_expansion() {
    let g = g(600);
    let src = "MATCH (p:Person {id: 300})<-[:HAS_CREATOR]-(m:Message) RETURN m.id AS mid";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "anchored-chain ON must equal OFF");
    assert_eq!(on, vec![vec![i(1300)]], "person 300 authored message 1300");
    assert!(
        seek_count(&g, src) >= 1,
        "the Person anchor must seek, seeding one person instead of scanning the label"
    );
}

/// A non-existent id: the seek returns nothing, the result is empty, ON==OFF.
#[test]
fn is5_missing_id_is_empty() {
    let g = g(600);
    let src = "MATCH (m:Message {id: 999999})-[:HAS_CREATOR]->(p:Person) RETURN p.id AS personId";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "missing-id IS5 ON must equal OFF");
    assert!(on.is_empty(), "no message with that id → empty");
}
