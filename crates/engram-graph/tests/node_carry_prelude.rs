#![allow(non_snake_case)]
//! Node-carry prelude + node-identity membership — the IC3 building block:
//! `MATCH (countryX:Country {name}), (countryY:Country {name}) WITH countryX,
//! countryY LIMIT 1 MATCH (city:City)-[:IS_PART_OF]->(country:Country) WHERE
//! country IN [countryX, countryY] RETURN …`. The prelude binds two NODES
//! (injected as params by the pre-pass), and the downstream `country IN
//! [countryX, countryY]` vectorises by node identity (the id-only-node column).
//! Byte-identical to the interp, and fires columnar (does not stream).

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
    let c2 = mk("Country", &[("name", Value::Str("Country2".into()))]);
    let city = |name: &str, country: u64| {
        let ci = mk("City", &[("name", Value::Str(name.into()))]);
        g.create_rel(ci, "IS_PART_OF", country, &BTreeMap::new())
            .unwrap();
    };
    city("Alpha", c0); // in Country0 → keep
    city("Beta", c1); // in Country1 → keep
    city("Gamma", c2); // in Country2 → drop
    g
}

const FRAG: &str = "MATCH (countryX:Country {name: 'Country0'}), (countryY:Country {name: 'Country1'}) \
    WITH countryX, countryY LIMIT 1 \
    MATCH (city:City)-[:IS_PART_OF]->(country:Country) WHERE country IN [countryX, countryY] \
    RETURN city.name AS cityName ORDER BY cityName ASC LIMIT 10";

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
fn node_carry_prelude_and_node_in_fires() {
    let g = g();
    let (on, off) = both(&g, FRAG);
    assert_eq!(on, off, "node-carry prelude columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Alpha")], vec![s("Beta")]],
        "cities in Country0/Country1, not Country2"
    );
    assert!(
        !streamed(&g, FRAG),
        "node-carry prelude + node-IN must fire columnar, not stream"
    );
}
