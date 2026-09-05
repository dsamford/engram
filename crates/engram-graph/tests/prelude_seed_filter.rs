#![allow(non_snake_case)]
//! A prelude node used as a traversal SEED — `MATCH (p:Person {id}) WITH p LIMIT
//! 1 MATCH (p)-[:KNOWS]-(f) RETURN …`. `p` cannot be a param (a param is not a
//! pattern var), so the pre-pass keeps it a bound, labelled scan var and
//! SEED-FILTERS it to the exact prelude node by identity (`p = $__prelude_p`),
//! dropping its pre-seed carries. Byte-identical to the interp, and fires
//! columnar. This is IC3's `person` handling in isolation.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |id: i64, name: &str| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        m.insert("name".to_string(), Value::Str(name.into()));
        g.create_node(&["Person".into()], &m).expect("person")
    };
    let p = mk(10, "Root");
    let a = mk(11, "Ana");
    let b = mk(12, "Bob");
    let c = mk(13, "Cid"); // not a friend
    let _ = c;
    g.create_rel(p, "KNOWS", a, &BTreeMap::new()).unwrap();
    g.create_rel(p, "KNOWS", b, &BTreeMap::new()).unwrap();
    g
}

const SEEDED: &str = "MATCH (p:Person {id: 10}) WITH p LIMIT 1 \
    MATCH (p)-[:KNOWS]-(f:Person) RETURN f.name AS name ORDER BY name ASC LIMIT 10";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
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

fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn prelude_seed_node_fires() {
    let g = g();
    let (on, off) = both(&g, SEEDED);
    assert_eq!(on, off, "prelude seed node columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Ana")], vec![s("Bob")]],
        "person 10's two friends"
    );
    assert!(
        !streamed(&g, SEEDED),
        "prelude seed node must be seed-filtered + fire columnar, not stream"
    );
}
