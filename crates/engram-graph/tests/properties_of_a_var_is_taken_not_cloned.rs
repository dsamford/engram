#![allow(non_snake_case)]
//! Fix 97: a projection's `properties(v)` takes the map out of its row when
//! nothing else reads `v` — the evaluator's `properties` clones it, 4.7 µs
//! of a 32-property record's 25.9 µs listing row on the record-decode
//! bench, and the production repository listing returns 182 multi-kilobyte
//! records — and the ORDER BY's scope row binds only the columns the keys
//! read.
//!
//! The rows are pinned against a hand-computed expectation; another item
//! reading the variable, and an ORDER BY reading it under a name the item
//! does not shadow, keep the clone and answer the same rows.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str("user-1".into()));
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

const TAKEN: &str = "interp.projection took a properties map out of its row";

fn repo(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-1".into()));
    m.insert("nodeType".to_string(), Value::Str("repository".into()));
    m.insert("nodeId".to_string(), Value::Str(format!("repo-{i:03}")));
    m.insert(
        "createdAt".to_string(),
        Value::Str(format!("2026-08-{:02}T00:00:00Z", 1 + i % 28)),
    );
    m.insert("readme".to_string(), Value::Str("# readme\n".repeat(200)));
    m
}

/// Sixty repositories of one user, fat `readme`s, `createdAt` on 28 days.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..60i64 {
        g.create_node(&["UserDataNode".into()], &repo(i))
            .expect("repo");
    }
    let mut other = repo(999);
    other.insert("userId".to_string(), Value::Str("user-2".into()));
    g.create_node(&["UserDataNode".into()], &other)
        .expect("other");
    g
}

fn text<'a>(m: &'a BTreeMap<String, Value>, k: &str) -> &'a str {
    match &m[k] {
        Value::Str(s) => s.as_str(),
        _ => unreachable!(),
    }
}

fn expected() -> Vec<Vec<Value>> {
    let mut all: Vec<BTreeMap<String, Value>> = (0..60).map(repo).collect();
    all.sort_by(|a, b| {
        text(b, "createdAt")
            .cmp(text(a, "createdAt"))
            .then(text(a, "nodeId").cmp(text(b, "nodeId")))
    });
    all.into_iter().map(|m| vec![Value::Map(m)]).collect()
}

/// The production listing (the alias shadows the variable, the ORDER BY
/// reads the alias) and an unordered one take the map; the rows are the
/// records.
#[test]
fn a_the_listing_takes_each_records_map_out_of_its_row() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'repository'}) RETURN properties(n) AS n ORDER BY n.createdAt DESC, n.nodeId",
    );
    assert_eq!(got, expected());
    assert_eq!(count_of(&c, TAKEN), 60, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'repository'}) WITH n ORDER BY n.createdAt DESC, n.nodeId RETURN properties(n) AS props",
    );
    assert_eq!(got, expected());
    assert_eq!(count_of(&c, TAKEN), 60, "{c:?}");
}

/// CONTROLS: another item reading the variable, an ORDER BY reading it
/// under a name the item does not shadow, and an OPTIONAL null keep the
/// evaluator's answer.
#[test]
fn b_another_read_of_the_variable_keeps_the_clone_and_the_rows() {
    let g = corpus();
    let want: Vec<Vec<Value>> = expected()
        .into_iter()
        .map(|r| {
            let id = match &r[0] {
                Value::Map(m) => m["nodeId"].clone(),
                _ => unreachable!(),
            };
            vec![r[0].clone(), id]
        })
        .collect();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'repository'}) RETURN properties(n) AS props, n.nodeId AS id ORDER BY n.createdAt DESC, n.nodeId",
    );
    assert_eq!(got, want);
    assert_eq!(count_of(&c, TAKEN), 0, "{c:?}");
    // Fix 105: the keys `n.createdAt`, `n.nodeId` read the row alone and
    // are evaluated over it first, so the map is taken even though the
    // item's alias (`props`) does not shadow the variable.
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'repository'}) RETURN properties(n) AS props ORDER BY n.createdAt DESC, n.nodeId",
    );
    assert_eq!(got, expected());
    assert_eq!(count_of(&c, TAKEN), 60, "{c:?}");
    let (got, c) = traced(
        &g,
        "OPTIONAL MATCH (n:UserDataNode {userId: 'nobody', nodeType: 'repository'}) RETURN properties(n) AS n",
    );
    assert_eq!(got, vec![vec![Value::Null]]);
    assert_eq!(count_of(&c, TAKEN), 0, "{c:?}");
    // A subquery names the variable in its PATTERN — a read no expression
    // walk reports: the comprehension must still see the node.
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'repository'}) \
         RETURN properties(n) AS n, [(n)-[:OWNED_BY]->(o) | o.userId] AS owners, COUNT { (n)-[:OWNED_BY]->() } AS k \
         ORDER BY n.createdAt DESC, n.nodeId",
    );
    assert_eq!(got.len(), 60);
    assert!(
        got.iter()
            .all(|r| r[1] == Value::List((Vec::new()).into()) && r[2] == Value::Int(0)),
        "{:?}",
        got[0]
    );
    assert_eq!(count_of(&c, TAKEN), 0, "{c:?}");
}
