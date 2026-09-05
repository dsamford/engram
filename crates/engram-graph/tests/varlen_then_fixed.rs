#![allow(non_snake_case)]
//! A VARLEN-then-FIXED pattern followed by `WITH DISTINCT <varlen-end>` —
//! `MATCH (p {id})-[:KNOWS*1..2]-(f)-[:IS_LOCATED_IN]->(city) WHERE … WITH
//! DISTINCT f …`. The frontier-BFS is sole-hop-only, so this declines unsplit;
//! the pre-pass SPLITS it into `MATCH (p)-[:KNOWS*1..2]-(f) … WITH DISTINCT f
//! MATCH (f)-[:IS_LOCATED_IN]->(city) … <WITH>`. Byte-identical to the interp
//! (the DISTINCT on the varlen end collapses path-multiplicity), and fires the
//! multistage pipeline. This is IC3's clause-5 shape in isolation.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let p = mk(
        "Person",
        &[("id", Value::Int(10)), ("name", Value::Str("Root".into()))],
    );
    let a = mk(
        "Person",
        &[("id", Value::Int(11)), ("name", Value::Str("Ana".into()))],
    );
    let b = mk(
        "Person",
        &[("id", Value::Int(12)), ("name", Value::Str("Bob".into()))],
    );
    let c = mk(
        "Person",
        &[("id", Value::Int(13)), ("name", Value::Str("Cid".into()))],
    );
    g.create_rel(p, "KNOWS", a, &BTreeMap::new()).unwrap();
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).unwrap(); // b is 2 hops
    g.create_rel(p, "KNOWS", c, &BTreeMap::new()).unwrap();
    let metro = mk("City", &[("name", Value::Str("Metro".into()))]);
    let rural = mk("City", &[("name", Value::Str("Rural".into()))]);
    g.create_rel(a, "IS_LOCATED_IN", metro, &BTreeMap::new())
        .unwrap(); // keep
    g.create_rel(b, "IS_LOCATED_IN", rural, &BTreeMap::new())
        .unwrap(); // drop
    g.create_rel(c, "IS_LOCATED_IN", metro, &BTreeMap::new())
        .unwrap(); // keep
    g
}

const SRC: &str = "MATCH (p:Person {id: 10})-[:KNOWS*1..2]-(f:Person)-[:IS_LOCATED_IN]->(city:City) \
    WHERE NOT p = f AND city.name = 'Metro' \
    WITH DISTINCT f \
    RETURN f.name AS name ORDER BY name ASC LIMIT 10";

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
fn varlen_then_fixed_splits_and_fires() {
    let g = g();
    let (on, off) = both(&g, SRC);
    assert_eq!(on, off, "varlen-then-fixed columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Ana")], vec![s("Cid")]],
        "Ana + Cid are in Metro; Bob is in Rural"
    );
    assert!(
        !streamed(&g, SRC),
        "varlen-then-fixed must split + fire multistage, not stream"
    );
}
