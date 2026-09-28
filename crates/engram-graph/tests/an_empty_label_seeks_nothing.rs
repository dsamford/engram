#![allow(non_snake_case)]
//! Fix 95: a start requiring a label with no live node seeks nothing — no
//! index probe, no index read. The production Part feature-extraction pick
//! probed its declared index twice (13 store gets) for a label that has
//! never held a node: 2.4 ms against Neo4j's 1.1 for zero rows.
//!
//! The listing, its count and an OPTIONAL form answer their empty shapes
//! without a probe; one created node brings the probe back, inside the
//! transaction that created it too.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("orgId".to_string(), Value::Str("org-1".into()));
    p.insert(
        "cutoff".to_string(),
        Value::Str("2026-09-01T00:00:00Z".into()),
    );
    p.insert("limit".to_string(), Value::Int(25));
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

const EMPTY: &str = "interp.seed answered empty from a label with no member";
const PROBED: &str = "interp.seed probed a declared scoped index";
const PROBED_COLUMNAR: &str = "interp.columnar seek probed a declared scoped index";
const RANGE_QUERIES: &str = "index.range queries";

/// The Part index declared, no Part ever created; 300 Widgets carry the
/// same key so the partition is not empty.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "CREATE INDEX part_org FOR (p:Part) ON (p.orgId, p.featuresExtractedAt)",
    );
    for i in 0..300i64 {
        let mut m = BTreeMap::new();
        m.insert("orgId".to_string(), Value::Str("org-1".into()));
        m.insert("widgetId".to_string(), Value::Str(format!("w-{i}")));
        g.create_node(&["Widget".into()], &m).expect("widget");
    }
    g
}

const PICK: &str = "MATCH (p:Part {orgId: $orgId}) WHERE p.featuresExtractedAt IS NULL OR p.featuresExtractedAt < $cutoff \
    RETURN properties(p) AS p LIMIT toInteger($limit)";

/// The pick, its count and an OPTIONAL form answer without a probe.
#[test]
fn a_the_pick_its_count_and_an_optional_form_answer_without_a_probe() {
    let g = corpus();
    let (got, c) = traced(&g, PICK);
    assert!(got.is_empty());
    assert_eq!(count_of(&c, EMPTY), 1, "{c:?}");
    assert_eq!(
        count_of(&c, PROBED) + count_of(&c, PROBED_COLUMNAR),
        0,
        "{c:?}"
    );
    assert_eq!(count_of(&c, RANGE_QUERIES), 0, "{c:?}");
    let (got, c) = traced(&g, "MATCH (p:Part {orgId: $orgId}) RETURN count(p) AS n");
    assert_eq!(got, vec![vec![Value::Int(0)]]);
    assert_eq!(count_of(&c, EMPTY), 1, "{c:?}");
    assert_eq!(count_of(&c, RANGE_QUERIES), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        "OPTIONAL MATCH (p:Part {orgId: $orgId}) RETURN p.orgId AS o",
    );
    assert_eq!(got, vec![vec![Value::Null]]);
    assert_eq!(count_of(&c, RANGE_QUERIES), 0, "{c:?}");
}

/// CONTROLS: a label that holds a node probes as before — and a node
/// created in the same transaction is seen by the seek that follows it.
#[test]
fn b_one_node_brings_the_probe_back_within_its_transaction_too() {
    let g = corpus();
    let mut m = BTreeMap::new();
    m.insert("orgId".to_string(), Value::Str("org-1".into()));
    m.insert("partId".to_string(), Value::Str("part-1".into()));
    g.create_node(&["Part".into()], &m).expect("part");
    let (got, c) = traced(&g, PICK);
    assert_eq!(got.len(), 1, "{c:?}");
    assert_eq!(count_of(&c, EMPTY), 0, "{c:?}");
    assert!(
        count_of(&c, PROBED) + count_of(&c, PROBED_COLUMNAR) >= 1,
        "{c:?}"
    );
    // Inside one statement: the create, then the seek over the new label.
    let g2 = corpus();
    let got = rows(
        &g2,
        "CREATE (:Gadget {orgId: $orgId, gadgetId: 'g-1'}) WITH 1 AS one MATCH (x:Gadget {orgId: $orgId}) RETURN x.gadgetId AS id",
    );
    assert_eq!(got, vec![vec![Value::Str("g-1".into())]]);
}
