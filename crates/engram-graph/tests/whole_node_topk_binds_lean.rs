#![allow(non_snake_case)]
//! Fix 56: a concluding top-k RETURN that reads a MATCH-bound node WHOLE
//! (`properties(w)`, bare `w`) while everything before it — the pattern
//! maps, the WHERE, the ORDER BY — reads only properties, binds the node
//! LEAN on those key properties and hydrates only its skip+limit survivors
//! in full. The production viewer-visibility listing decoded every one of
//! 15,494 work items in full before its WHERE ran (2.7 s against Neo4j's
//! 116 ms) for the 200 it paged.
//!
//! Every answer is checked against the generating rules and against the
//! same statement with the columnar paths off.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const LEAN: &str = "interp.stage bound a whole-node output lean for the top-k";
const HYDRATED: &str = "interp.late projection re-materialised a carried node for a survivor";
const FULL: &str = "graph.nodes materialised in full";
const KEY_DIRECT: &str = "interp.top-k key read from the lean row";

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("me".to_string(), Value::Str("u1".to_string()));
    p.insert("limit".to_string(), Value::Int(10));
    p.insert("offset".to_string(), Value::Int(5));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const N: i64 = 3_000;
fn open(i: i64) -> bool {
    i % 5 != 0
}
fn org_scoped(i: i64) -> bool {
    i % 2 == 0
}
fn project(i: i64) -> i64 {
    i % 3
}
/// `u1` is a member of project 0 alone.
fn visible(i: i64) -> bool {
    org_scoped(i) || project(i) == 0
}
fn updated(i: i64) -> i64 {
    (i * 7919) % N // a permutation, so the ORDER BY is not the id order
}

/// 3,000 fat items (a 2 KB body each) over three projects.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut projects = Vec::new();
    for p in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Int(p));
        projects.push(g.create_node(&["Proj".into()], &m).expect("proj"));
    }
    let mut m = BTreeMap::new();
    m.insert("pid".to_string(), Value::Str("u1".to_string()));
    let u1 = g.create_node(&["Person".into()], &m).expect("person");
    g.create_rel(u1, "MEMBER", projects[0], &BTreeMap::new()).expect("m");
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        m.insert("updated".to_string(), Value::Int(updated(i)));
        m.insert(
            "status".to_string(),
            Value::Str(if open(i) { "open" } else { "done" }.to_string()),
        );
        if org_scoped(i) {
            m.insert("scope".to_string(), Value::Str("org".to_string()));
        }
        m.insert("body".to_string(), Value::Str("b".repeat(2_000)));
        let w = g.create_node(&["Item".into()], &m).expect("item");
        g.create_rel(w, "IN", projects[project(i) as usize], &BTreeMap::new())
            .expect("in");
    }
    g
}

/// The page the rules give: visible open items by `updated` DESC, then the
/// SKIP/LIMIT window.
fn expected_page(skip: usize, limit: usize) -> Vec<i64> {
    let mut all: Vec<i64> = (0..N).filter(|&i| open(i) && visible(i)).collect();
    all.sort_by_key(|&i| std::cmp::Reverse(updated(i)));
    all.into_iter().skip(skip).take(limit).collect()
}

fn n_of(row: &[Value]) -> i64 {
    match &row[0] {
        Value::Map(m) => match m.get("n") {
            Some(Value::Int(i)) => *i,
            other => panic!("{other:?}"),
        },
        Value::Node { props, .. } => match props.get("n") {
            Some(Value::Int(i)) => *i,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

const LISTING: &str = "MATCH (w:Item) WHERE ( coalesce(w.scope, '') = 'org' OR EXISTS { MATCH (w)-[:IN]->(:Proj)<-[:MEMBER]-(:Person {pid: $me}) } ) AND w.status = 'open' RETURN properties(w) AS w, [(w)-[:IN]->(p:Proj) | p.pid][0] AS pid ORDER BY w.updated DESC SKIP toInteger($offset) LIMIT toInteger($limit)";

#[test]
fn the_listing_hydrates_only_its_page_and_answers_the_rules() {
    let g = corpus();
    let want = expected_page(5, 10);
    assert_eq!(want.len(), 10);
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got.iter().map(|r| n_of(r)).collect::<Vec<_>>(), want);
    // Every row carries the WHOLE record (the body) and its project.
    for (r, &i) in got.iter().zip(&want) {
        let Value::Map(m) = &r[0] else { panic!("{:?}", r[0]) };
        assert_eq!(m.get("body").map(|v| matches!(v, Value::Str(s) if s.len() == 2_000)), Some(true));
        assert_eq!(r[1], Value::Int(project(i)));
    }
    assert!(count_of(&c, LEAN) >= 1, "{c:?}");
    assert!(count_of(&c, HYDRATED) >= 10, "{c:?}");
    assert!(count_of(&c, KEY_DIRECT) >= 100, "the key is read from the lean row: {c:?}");
    // Only the page is decoded in full — never the label.
    assert!(
        count_of(&c, FULL) <= 15 + 8,
        "{} full materialisations for a page of 15: {c:?}",
        count_of(&c, FULL)
    );
    assert_eq!(general(&g, LISTING).iter().map(|r| n_of(r)).collect::<Vec<_>>(), want);
}

/// The bare form (`RETURN w … ORDER BY w.updated`) binds lean too; the
/// hydrated survivor is the FULL node.
#[test]
fn a_bare_whole_node_output_binds_lean_as_well() {
    let g = corpus();
    let src = "MATCH (w:Item) WHERE w.status = 'open' RETURN w ORDER BY w.updated DESC LIMIT 3";
    let want: Vec<i64> = {
        let mut all: Vec<i64> = (0..N).filter(|&i| open(i)).collect();
        all.sort_by_key(|&i| std::cmp::Reverse(updated(i)));
        all.into_iter().take(3).collect()
    };
    let (got, c) = traced(&g, src);
    assert_eq!(got.iter().map(|r| n_of(r)).collect::<Vec<_>>(), want);
    for r in &got {
        let Value::Node { props, .. } = &r[0] else { panic!("{:?}", r[0]) };
        assert!(props.contains_key("body"), "hydrated in full: {props:?}");
    }
    // The columnar projection recogniser may claim this hop-less spelling
    // and hydrate its own survivors; either way the label is not decoded.
    assert!(
        count_of(&c, LEAN) + count_of(&c, "interp.columnar projection scans") >= 1,
        "{c:?}"
    );
    assert!(count_of(&c, FULL) <= 3 + 8, "{c:?}");
    assert_eq!(general(&g, src).iter().map(|r| n_of(r)).collect::<Vec<_>>(), want);
}

/// CONTROLS: a whole-node read BEFORE the breaker (`size(keys(w))` in the
/// WHERE), a DISTINCT, an aggregate and a plain limit (no ORDER BY) each
/// keep the node FULL as written — same rows, no lean binding.
#[test]
fn a_whole_node_read_before_the_breaker_or_a_non_topk_keeps_it_full() {
    let g = corpus();
    for src in [
        "MATCH (w:Item) WHERE w.status = 'open' AND size(keys(w)) > 3 RETURN properties(w) AS w ORDER BY w.updated DESC LIMIT 3",
        "MATCH (w:Item) WHERE w.status = 'open' RETURN DISTINCT properties(w) AS w ORDER BY w.updated DESC LIMIT 3",
        "MATCH (w:Item) WHERE w.status = 'open' RETURN properties(w) AS w, count(*) AS c ORDER BY w.updated DESC LIMIT 3",
        "MATCH (w:Item) WHERE w.status = 'open' RETURN properties(w) AS w LIMIT 3",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, LEAN), 0, "`{src}` is left as written: {c:?}");
    }
}
