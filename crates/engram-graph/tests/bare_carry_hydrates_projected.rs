#![allow(non_snake_case)]
//! Fix 62: a bare carry's survivor (fix 57) is hydrated to what the
//! CONTINUATION reads of it — a projected record — instead of the whole
//! node. The inbox page's continuation reads a dozen of the email's
//! properties and its HAS_ASK adjacency, never the body, yet each of its
//! 1,000 survivors was decoded in full on the paged mirror (296–357 ms
//! against Neo4j's 107–115 on v121, the stage already running). A bare
//! use after the breaker keeps the full record.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the general path).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
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

const HYDRATED: &str = "interp.columnar stage hydrated a bare node for a survivor";
const PROJECTED: &str = "interp.columnar stage hydrated a survivor projected to its continuation";
const FULL: &str = "graph.nodes materialised in full";

/// 3,000 emails of ONE user (the label is the user's, so no seek keeps the
/// carry off the stage) with 3 KB bodies; a fifth have an EmailAsk.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(3072);
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str("u1".into()));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        m.insert("subject".to_string(), Value::Str(format!("subject {i}")));
        m.insert("createdAt".to_string(), Value::Str(format!("2026-{:02}-{:02}T{:02}:00:00Z", 1 + (i % 12), 1 + (i % 28), i % 24)));
        m.insert("rawData".to_string(), Value::Str(body.clone()));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        if i % 5 == 0 {
            let mut a = BTreeMap::new();
            a.insert("resolved".to_string(), Value::Bool(i % 10 == 0));
            let ask = g.create_node(&["EmailAsk".into()], &a).expect("ask");
            g.create_rel(n, "HAS_ASK", ask, &BTreeMap::new()).expect("has ask");
        }
    }
    g
}

const PAGE: &str = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
    WITH n ORDER BY n.createdAt DESC SKIP 100 LIMIT 100 \
    OPTIONAL MATCH (n)-[:HAS_ASK]->(a:EmailAsk) \
    WITH n, count(CASE WHEN a IS NOT NULL AND coalesce(a.resolved, false) = false THEN a END) AS openAsks \
    RETURN n.nodeId AS nodeId, n.subject AS subject, openAsks ORDER BY nodeId";

#[test]
fn a_survivor_is_hydrated_to_what_the_continuation_reads() {
    let g = corpus();
    let want = general(&g, PAGE);
    assert_eq!(want.len(), 100);
    let (got, c) = traced(&g, PAGE);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, HYDRATED), 100, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 100, "projected, not full: {c:?}");
    // The only whole-node reads are the 20 asks the `THEN a` returns
    // bare; no email body is decoded.
    assert_eq!(count_of(&c, FULL), 20, "the asks alone, never an email: {c:?}");
}

/// A bare use after the breaker — `RETURN n` — needs the whole node.
#[test]
fn a_bare_use_after_the_breaker_hydrates_in_full() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
        WITH n ORDER BY n.createdAt DESC LIMIT 20 \
        RETURN n ORDER BY n.nodeId";
    let want = general(&g, src);
    assert_eq!(want.len(), 20);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, HYDRATED), 20, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "a bare RETURN keeps the record: {c:?}");
    assert_eq!(count_of(&c, FULL), 20, "{c:?}");
}

/// The pattern test on a projected survivor still sees its labels.
#[test]
fn a_projected_survivor_still_passes_its_pattern_tests() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
        WITH n ORDER BY n.createdAt DESC LIMIT 30 \
        MATCH (n:UserDataNode)-[:HAS_ASK]->(a:EmailAsk) \
        RETURN n.nodeId AS nodeId, a.resolved AS resolved ORDER BY nodeId";
    let want = general(&g, src);
    assert!(!want.is_empty());
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PROJECTED), 30, "{c:?}");
}
