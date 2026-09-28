#![allow(non_snake_case)]
//! Fix 105: an ordered projection of a whole node neither clones the node
//! for its output nor for its ORDER BY key. `RETURN p ORDER BY p.updatedAt
//! DESC` evaluated `p` (a clone of the node into the output row), bound
//! every output column into a scope row (a second clone) and read the key
//! there — 7.5 µs per project of the studio listing's 14.7, more than
//! the record's decode, on the hop-listing bench; the aggregating finish
//! (`RETURN p, count(t) …`, the production shape) did the same per group.
//! A key that reads the row alone (fix 91's rule) is evaluated over the
//! row first, and a bare or `properties(...)` item nothing else reads is
//! then TAKEN out of it.
//!
//! The rows are pinned by value against the lean spelling's order; another
//! item reading the node, and an ORDER BY over an aggregate alias, keep the
//! projected scope and answer the same rows.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("user-1".into()));
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

const TOOK_NODE: &str = "interp.projection took a whole value out of its row";
const TOOK_PROPS: &str = "interp.projection took a properties map out of its row";
const DIRECT: &str = "interp.projection ordered by keys read from its row";

/// One user owning 300 projects (`updatedAt` on twenty distinct days so
/// ties exist), every project with `i % 4` tracks.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-1".into()));
    let u = g.create_node(&["User".into()], &m).expect("user");
    for i in 0..300i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:03}")));
        m.insert("name".to_string(), Value::Str(format!("Project {i}")));
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!("2026-09-{:02}T00:00:00Z", 1 + i % 20)),
        );
        m.insert("bpm".to_string(), Value::Int(90 + i % 60));
        let p = g
            .create_node(&["StudioProject".into()], &m)
            .expect("project");
        g.create_rel(u, "OWNS_STUDIO_PROJECT", p, &BTreeMap::new())
            .expect("owns");
        for k in 0..(i % 4) {
            let mut t = BTreeMap::new();
            t.insert("order".to_string(), Value::Int(k));
            let tr = g.create_node(&["StudioTrack".into()], &t).expect("track");
            g.create_rel(p, "CONTAINS_TRACK", tr, &BTreeMap::new())
                .expect("contains");
        }
    }
    g
}

const HEAD: &str = "MATCH (u:User {userId: $u})-[:OWNS_STUDIO_PROJECT]->(p:StudioProject)";

fn id_of(v: &Value) -> String {
    match v {
        Value::Node { props, .. } => match props.get("id") {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("{other:?}"),
        },
        Value::Map(m) => match m.get("id") {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The plain and the aggregating whole-node listings order like the lean
/// spelling, carry every property, and take each node out of its row.
#[test]
fn a_the_ordered_whole_node_listings_take_their_nodes() {
    let g = corpus();
    let lean: Vec<String> = rows(
        &g,
        &format!("{HEAD} RETURN p.id AS id ORDER BY p.updatedAt DESC, p.id"),
    )
    .into_iter()
    .map(|r| match &r[0] {
        Value::Str(s) => s.clone(),
        other => panic!("{other:?}"),
    })
    .collect();
    assert_eq!(lean.len(), 300, "fixture");
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN p ORDER BY p.updatedAt DESC, p.id"),
    );
    assert_eq!(got.iter().map(|r| id_of(&r[0])).collect::<Vec<_>>(), lean);
    let Value::Node { props, labels, .. } = &got[0][0] else {
        panic!("{:?}", got[0][0])
    };
    assert_eq!(labels, &vec!["StudioProject".to_string()]);
    assert_eq!(props.len(), 4, "every property: {props:?}");
    assert_eq!(count_of(&c, TOOK_NODE), 300, "{c:?}");
    assert_eq!(count_of(&c, DIRECT), 300, "{c:?}");
    // The production shape: aggregating, the node the group key.
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN p, count(t) AS trackCount ORDER BY p.updatedAt DESC, p.id"
        ),
    );
    assert_eq!(got.iter().map(|r| id_of(&r[0])).collect::<Vec<_>>(), lean);
    let i: usize = lean[0][5..].parse().expect("index");
    assert_eq!(got[0][1], Value::Int((i % 4) as i64), "{:?}", got[0]);
    let Value::Node { props, .. } = &got[0][0] else {
        panic!("{:?}", got[0][0])
    };
    assert_eq!(props.len(), 4, "{props:?}");
    assert_eq!(count_of(&c, TOOK_NODE), 300, "{c:?}");
    // With `properties(p)`: the map is taken the same way.
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN properties(p) AS p, count(t) AS trackCount ORDER BY p.updatedAt DESC, p.id"
        ),
    );
    assert_eq!(got.iter().map(|r| id_of(&r[0])).collect::<Vec<_>>(), lean);
    assert_eq!(count_of(&c, TOOK_PROPS), 300, "{c:?}");
}

/// CONTROLS: another item reading the node keeps the clone; an ORDER BY
/// over an aggregate alias keeps the projected scope for that key; both
/// answer by value.
#[test]
fn b_another_read_of_the_node_and_an_aggregate_key_keep_the_scope() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN p, p.name AS name ORDER BY p.updatedAt DESC, p.id LIMIT 3"),
    );
    assert_eq!(got.len(), 3);
    assert_eq!(
        got[0][1],
        Value::Str(format!(
            "Project {}",
            id_of(&got[0][0])[5..].parse::<i64>().unwrap()
        ))
    );
    assert_eq!(count_of(&c, TOOK_NODE), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN p, count(t) AS trackCount ORDER BY trackCount DESC, p.id LIMIT 5"
        ),
    );
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|r| r[1] == Value::Int(3)), "{got:?}");
    assert_eq!(id_of(&got[0][0]), "proj-003");
    assert_eq!(
        count_of(&c, DIRECT),
        0,
        "an aggregate key needs the projected scope: {c:?}"
    );
    // A key over the alias of a bare node output reads the row under it.
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN p AS proj ORDER BY proj.updatedAt DESC, proj.id LIMIT 2"),
    );
    assert_eq!(got.len(), 2);
    assert_eq!(id_of(&got[0][0]), "proj-019");
    assert!(count_of(&c, TOOK_NODE) <= 300, "{c:?}");
}
