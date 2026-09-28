#![allow(non_snake_case)]
//! Fix 115: a DECLARED COMPOSITE index (`CREATE INDEX … ON (n.userId,
//! n.nodeType)`) is one structure ordered by the tuple, derived from its
//! component single-key indexes, and a seek on all of its keys is ONE probe
//! of it — where it used to be two single-key probes, the smaller taken (or
//! both whole match sets intersected, for a count).
//!
//! The production `MATCH (n:UserDataNode {userId: $userId, nodeType:
//! 'contact'}) RETURN count(n)` intersected the user's every node with every
//! contact in the store to answer 391: 0.6 ms in the engine against Neo4j's
//! whole 0.7 from its composite.
//!
//! Answers are pinned against hand-computed counts and rows; the composite
//! follows a write (created and deleted nodes); a non-string value declines
//! to the per-key path; the general (non-columnar) seed takes it too.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const COVERED: &str = "interp.columnar covered count sought a composite";
const SEEK: &str = "interp.columnar seek probed a declared composite";
const SEED: &str = "interp.seed probed a declared composite";
const DERIVED: &str = "graph.composite index derived";
const RANGE_QUERIES: &str = "index.range queries";

const USERS: i64 = 8;
const TYPES: [&str; 4] = ["email", "contact", "post", "event"];
const N: i64 = 4_096;

fn user(i: i64) -> String {
    format!("user-{}", i % USERS)
}

fn node_type(i: i64) -> &'static str {
    TYPES[(i / USERS) as usize % TYPES.len()]
}

fn params(u: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str(u.into()));
    p
}

fn rows(g: &Graph, src: &str, p: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    p: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

/// `N` nodes over eight users and four types (128 per pair), the composite
/// declared; every `contact` of `user-3` MENTIONS the one entity.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ddl =
        parse_any("CREATE INDEX udn_user_type FOR (n:UserDataNode) ON (n.userId, n.nodeType)")
            .expect("parse ddl");
    run_stmt(&g, &ddl, BTreeMap::new()).expect("index");
    let entity = g
        .create_node(
            &["Entity".into()],
            &BTreeMap::from([("name".to_string(), Value::Str("Acme".into()))]),
        )
        .expect("entity");
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("nodeId".to_string(), Value::Str(format!("n-{i:05}")));
        m.insert("userId".to_string(), Value::Str(user(i)));
        m.insert("nodeType".to_string(), Value::Str(node_type(i).into()));
        m.insert("title".to_string(), Value::Str(format!("Title {i}")));
        let id = g.create_node(&["UserDataNode".into()], &m).expect("node");
        if user(i) == "user-3" && node_type(i) == "contact" {
            g.create_rel(id, "MENTIONS", entity, &BTreeMap::new())
                .expect("rel");
        }
    }
    g
}

fn expected_count(u: &str, t: &str) -> i64 {
    (0..N)
        .filter(|&i| user(i) == u && node_type(i) == t)
        .count() as i64
}

const COUNT: &str =
    "MATCH (n:UserDataNode {userId: $u, nodeType: 'contact'}) RETURN count(n) AS cnt";

/// The covered count is ONE probe of the composite (one index range query,
/// where the per-key path made two), warm from the derived index.
#[test]
fn a_a_covered_count_on_both_keys_is_one_probe_of_the_composite() {
    let g = corpus();
    let want = vec![vec![Value::Int(expected_count("user-3", "contact"))]];
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(got, want, "cold: {c:?}");
    assert_eq!(count_of(&c, COVERED), 1, "cold: {c:?}");
    assert_eq!(count_of(&c, RANGE_QUERIES), 1, "cold: {c:?}");
    assert_eq!(count_of(&c, DERIVED), 1, "cold: {c:?}");
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(got, want, "warm: {c:?}");
    assert_eq!(count_of(&c, COVERED), 1, "warm: {c:?}");
    assert_eq!(count_of(&c, RANGE_QUERIES), 1, "warm: {c:?}");
    assert_eq!(count_of(&c, DERIVED), 0, "warm: {c:?}");
    // Another user's count reads the same derived index.
    let (got, c) = traced(&g, COUNT, params("user-5"));
    assert_eq!(
        got,
        vec![vec![Value::Int(expected_count("user-5", "contact"))]]
    );
    assert_eq!(count_of(&c, DERIVED), 0, "{c:?}");
}

/// A listing on both keys seeks the composite and answers exactly the
/// pair's members; a hop from the sought population seeds from it too.
#[test]
fn b_a_listing_and_a_hop_seek_the_composite() {
    let g = corpus();
    let want: Vec<Vec<Value>> = (0..N)
        .filter(|&i| user(i) == "user-3" && node_type(i) == "contact")
        .map(|i| vec![Value::Str(format!("n-{i:05}"))])
        .collect();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $u, nodeType: 'contact'}) RETURN n.nodeId AS id ORDER BY id",
        params("user-3"),
    );
    assert_eq!(got, want, "{c:?}");
    assert!(count_of(&c, SEEK) + count_of(&c, SEED) >= 1, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $u, nodeType: 'contact'})-[:MENTIONS]->(e:Entity) RETURN count(e) AS n",
        params("user-3"),
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(expected_count("user-3", "contact"))]],
        "{c:?}"
    );
    assert!(count_of(&c, SEED) + count_of(&c, SEEK) >= 1, "{c:?}");
}

/// The composite follows a write: a created contact is counted, a deleted
/// one is not, and each write re-derives the index once.
#[test]
fn c_the_composite_follows_a_create_and_a_delete() {
    let g = corpus();
    let base = expected_count("user-3", "contact");
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(got, vec![vec![Value::Int(base)]], "{c:?}");
    let mut m = BTreeMap::new();
    m.insert("nodeId".to_string(), Value::Str("n-new".into()));
    m.insert("userId".to_string(), Value::Str("user-3".into()));
    m.insert("nodeType".to_string(), Value::Str("contact".into()));
    let fresh = g.create_node(&["UserDataNode".into()], &m).expect("node");
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(got, vec![vec![Value::Int(base + 1)]], "after create: {c:?}");
    assert_eq!(count_of(&c, DERIVED), 1, "after create: {c:?}");
    g.delete_node(fresh, true).expect("delete");
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(got, vec![vec![Value::Int(base)]], "after delete: {c:?}");
    assert_eq!(count_of(&c, DERIVED), 1, "after delete: {c:?}");
    let (_, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(count_of(&c, DERIVED), 0, "settled: {c:?}");
}

/// CONTROLS: a non-string value declines the composite and still answers;
/// a single-key seek never consults it; a node whose type is not a string
/// is neither counted nor lost.
#[test]
fn d_non_string_values_and_single_keys_keep_the_per_key_path() {
    let g = corpus();
    let mut m = BTreeMap::new();
    m.insert("nodeId".to_string(), Value::Str("n-int".into()));
    m.insert("userId".to_string(), Value::Str("user-3".into()));
    m.insert("nodeType".to_string(), Value::Int(7));
    g.create_node(&["UserDataNode".into()], &m).expect("node");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $u, nodeType: 7}) RETURN count(n) AS cnt",
        params("user-3"),
    );
    assert_eq!(got, vec![vec![Value::Int(1)]], "{c:?}");
    assert_eq!(
        count_of(&c, COVERED) + count_of(&c, SEEK) + count_of(&c, SEED),
        0,
        "{c:?}"
    );
    let (got, c) = traced(&g, COUNT, params("user-3"));
    assert_eq!(
        got,
        vec![vec![Value::Int(expected_count("user-3", "contact"))]],
        "{c:?}"
    );
    assert_eq!(count_of(&c, COVERED), 1, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $u}) RETURN count(n) AS cnt",
        params("user-3"),
    );
    assert_eq!(got, vec![vec![Value::Int(N / USERS + 1)]], "{c:?}");
    assert_eq!(
        count_of(&c, COVERED) + count_of(&c, SEEK) + count_of(&c, SEED),
        0,
        "{c:?}"
    );
}
