//! A MATCH whose hop has BOTH ends bound walks from the smaller side.
//!
//! The companion to the pattern-comprehension form. `reverse_bound_end_path`
//! fires only when the start is UNBOUND — a bound start already drives from
//! something concrete, so it looks settled — but when both ends are bound the
//! direction still decides the cost and nothing chose it.
//!
//! SNB BI bi11 closes a triangle with `(c)-[k3:KNOWS]-(a)`, both bound.
//! Decomposed at SF3: the first two legs cost 8 s warm, the third took it to
//! 182 s, and this closure took it past the 300 s ceiling — against Neo4j's
//! 6 s for the whole query. Walking `c`'s ~47 KNOWS edges per row to look for
//! one `a` is the query.
//!
//! Reversal changes only the order nodes are DISCOVERED in, never which ones
//! match, so these assert answers first.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn person(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["P".into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// `hub` knows many people; `leaf` knows only `hub`. Walking from `hub` is
/// expensive, from `leaf` is one edge.
fn fixture(friends: usize) -> Graph {
    let g = g();
    let hub = person(&g, "hub");
    let leaf = person(&g, "leaf");
    g.create_rel(hub, "KNOWS", leaf, &BTreeMap::new())
        .expect("rel");
    for i in 0..friends {
        let f = person(&g, &format!("f{i}"));
        g.create_rel(hub, "KNOWS", f, &BTreeMap::new())
            .expect("rel");
    }
    let _ = g.warm();
    g
}

#[test]
fn the_closing_edge_is_found() {
    let g = fixture(40);
    let r = rows(
        &g,
        "MATCH (h:P {name:'hub'}), (l:P {name:'leaf'}) \
         MATCH (h)-[:KNOWS]-(l) RETURN count(*) AS n",
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn a_missing_edge_is_still_missing() {
    let g = fixture(40);
    let r = rows(
        &g,
        "MATCH (a:P {name:'f0'}), (b:P {name:'f1'}) \
         MATCH (a)-[:KNOWS]-(b) RETURN count(*) AS n",
    );
    assert_eq!(
        r[0][0],
        Value::Int(0),
        "two leaves do not know each other: {r:?}"
    );
}

#[test]
fn direction_is_respected_after_reversal() {
    // A DIRECTED hop must stay directed: `hub -> leaf` exists, `leaf -> hub`
    // does not. A reversal that forgot to flip the arrow would answer 1 here.
    let g = fixture(40);
    let forward = rows(
        &g,
        "MATCH (h:P {name:'hub'}), (l:P {name:'leaf'}) \
         MATCH (h)-[:KNOWS]->(l) RETURN count(*) AS n",
    );
    assert_eq!(forward[0][0], Value::Int(1), "{forward:?}");
    let backward = rows(
        &g,
        "MATCH (h:P {name:'hub'}), (l:P {name:'leaf'}) \
         MATCH (l)-[:KNOWS]->(h) RETURN count(*) AS n",
    );
    assert_eq!(backward[0][0], Value::Int(0), "{backward:?}");
}

#[test]
fn the_relationship_variable_still_binds() {
    let g = fixture(10);
    let r = rows(
        &g,
        "MATCH (h:P {name:'hub'}), (l:P {name:'leaf'}) \
         MATCH (h)-[k:KNOWS]-(l) RETURN type(k) AS t",
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::Str("KNOWS".into()));
}

#[test]
fn a_multi_hop_both_bound_path_keeps_its_intermediates() {
    // Reversal reorders discovery across every hop; an intermediate variable
    // must still bind to the same node.
    let g = g();
    let a = person(&g, "a");
    let mid = person(&g, "mid");
    let z = person(&g, "z");
    g.create_rel(a, "KNOWS", mid, &BTreeMap::new())
        .expect("rel");
    g.create_rel(mid, "KNOWS", z, &BTreeMap::new())
        .expect("rel");
    for i in 0..30 {
        let f = person(&g, &format!("f{i}"));
        g.create_rel(a, "KNOWS", f, &BTreeMap::new()).expect("rel");
    }
    let _ = g.warm();
    let r = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'z'}) \
         MATCH (x)-[:KNOWS]->(m:P)-[:KNOWS]->(y) RETURN m.name AS name",
    );
    let got: Vec<String> = r
        .iter()
        .map(|v| match &v[0] {
            Value::Str(s) => s.to_string(),
            o => format!("{o:?}"),
        })
        .collect();
    assert_eq!(got, vec!["mid"], "{r:?}");
}

#[test]
fn it_engages_when_the_ends_differ_in_size() {
    let g = fixture(40);
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (h:P {name:'hub'}), (l:P {name:'leaf'}) \
             MATCH (h)-[:KNOWS]-(l) RETURN count(*) AS n",
        )
    })
    .1;
    assert!(
        t.sometimes_hit()
            .contains("interp.hop reversed to its cheaper bound end"),
        "the hop was not reversed: {:?}",
        t.sometimes_hit()
    );
}
