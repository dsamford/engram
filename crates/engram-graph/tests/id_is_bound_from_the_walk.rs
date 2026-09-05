#![allow(non_snake_case)]
//! Fix 46: `id(var)` over a columnar walk is bound from the id the walk is
//! visiting — a local, never a record read. `MATCH (s:NewsStory) RETURN
//! min(id(s)), max(id(s)), count(*)` ran on the general path and decoded
//! every one of 20k fat records in full (4.5 s on the mirror against
//! Neo4j's 17 ms); `RETURN s.__mid, id(s) LIMIT 5` under a predicate cost
//! 131 ms for five rows.
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const BOUND: &str = "interp.columnar id bound from the walk";
const FULL: &str = "graph.nodes materialised in full";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
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

/// 3,000 fat items and a few relationships among them.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("key".to_string(), Value::Str(format!("k-{i:05}")));
        m.insert("kind".to_string(), Value::Str(if i % 3 == 0 { "a".into() } else { "b".into() }));
        m.insert("body".to_string(), Value::Str("x".repeat(2000)));
        ids.push(g.create_node(&["Item".into()], &m).expect("item"));
    }
    for i in (0..3000usize).step_by(17) {
        g.create_rel(ids[i], "LINKS", ids[(i * 7 + 3) % 3000], &BTreeMap::new()).expect("link");
    }
    g
}

fn check(g: &Graph, src: &str) -> BTreeMap<String, u64> {
    let want = general(g, src);
    let first = rows(g, src);
    assert_eq!(first, want, "first run `{src}`");
    let (got, c) = traced(g, src);
    assert_eq!(got, want, "second run `{src}`");
    c
}

#[test]
fn an_id_span_reads_no_record() {
    let g = corpus();
    let c = check(&g, "MATCH (n:Item) RETURN min(id(n)) AS lo, max(id(n)) AS hi, count(*) AS c");
    assert!(count_of(&c, BOUND) > 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 0, "{c:?}");
}

#[test]
fn ids_in_projections_predicates_and_order_keys_read_no_record() {
    let g = corpus();
    for src in [
        "MATCH (n:Item) WHERE n.kind = 'a' RETURN n.key AS key, id(n) AS id ORDER BY id LIMIT 5",
        "MATCH (n:Item) WHERE id(n) > 10 AND n.kind = 'b' RETURN count(*) AS c",
        "MATCH (n:Item) WHERE n.kind = 'a' RETURN id(n) AS id ORDER BY n.key DESC LIMIT 3",
        "MATCH (n:Item) RETURN n.kind AS kind, min(id(n)) AS lo, count(*) AS c ORDER BY kind",
    ] {
        let c = check(&g, src);
        assert!(count_of(&c, BOUND) > 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}`: {c:?}");
    }
}

#[test]
fn a_relationship_s_id_binds_from_its_walk_too() {
    let g = corpus();
    let src = "MATCH ()-[r:LINKS]->() RETURN min(id(r)) AS lo, max(id(r)) AS hi, count(r) AS c";
    let c = check(&g, src);
    assert!(count_of(&c, BOUND) > 0, "{c:?}");
}
