#![allow(non_snake_case)]
//! A GLOBAL-collect prelude — `MATCH (city:City)-[:IS_PART_OF]->(:Country {name})
//! WITH collect(city) AS cities MATCH … WHERE NOT loc IN cities …`. The collect
//! yields ONE row; its LIST is injected as a param, so `NOT loc IN cities`
//! becomes `NOT loc IN $cities` — a const-param node-membership the node-identity
//! `IN` vectorises. Byte-identical to the interp, and fires columnar. This is
//! IC3's `cities` handling in isolation.

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
    let c0 = mk("Country", &[("name", Value::Str("Country0".into()))]);
    let c1 = mk("Country", &[("name", Value::Str("Country1".into()))]);
    let city_a = mk("City", &[("name", Value::Str("Alpha".into()))]);
    let city_b = mk("City", &[("name", Value::Str("Beta".into()))]);
    let city_c = mk("City", &[("name", Value::Str("Gamma".into()))]);
    g.create_rel(city_a, "IS_PART_OF", c0, &BTreeMap::new())
        .unwrap(); // in cities
    g.create_rel(city_b, "IS_PART_OF", c0, &BTreeMap::new())
        .unwrap(); // in cities
    g.create_rel(city_c, "IS_PART_OF", c1, &BTreeMap::new())
        .unwrap(); // NOT in cities
    let person = |name: &str, loc: u64| {
        let p = mk("Person", &[("name", Value::Str(name.into()))]);
        g.create_rel(p, "IS_LOCATED_IN", loc, &BTreeMap::new())
            .unwrap();
    };
    person("Pat", city_a); // in cities → dropped
    person("Quinn", city_c); // not in cities → kept
    person("Ray", city_b); // in cities → dropped
    g
}

const SRC: &str = "MATCH (city:City)-[:IS_PART_OF]->(:Country {name: 'Country0'}) \
    WITH collect(city) AS cities \
    MATCH (p:Person)-[:IS_LOCATED_IN]->(loc:City) WHERE NOT loc IN cities \
    RETURN p.name AS name ORDER BY name ASC LIMIT 10";

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
fn collect_list_prelude_and_node_in_fires() {
    let g = g();
    let (on, off) = both(&g, SRC);
    assert_eq!(on, off, "collect-list prelude columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Quinn")]],
        "only Quinn's city is outside Country0"
    );
    assert!(
        !streamed(&g, SRC),
        "collect-list prelude + node-IN must fire columnar, not stream"
    );
}
