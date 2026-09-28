//! A single hop with BOTH ends bound and NO edge between them answers without
//! expanding anything.
//!
//! The cheaper-end reversal picks which side to walk from; it cannot help when
//! both ends are the same kind of node. SNB BI bi11 closes its triangle with
//! `(c)-[k3:KNOWS]-(a)` where both are Persons of comparable degree (~47), so
//! the fan-out comparison declines — correctly, there is no cheaper direction —
//! and the walk happened anyway. Decomposed at SF3, that closure took the query
//! from 173 s past the 300 s ceiling, against Neo4j's 6 s for all of it.
//!
//! With both ends bound the question is EDGE EXISTENCE, which
//! `edge_count_slim` already answers by binary search on a row whose
//! sortedness is an ESTABLISHED invariant. Triangles are rare, so most pairs
//! die at the probe.
//!
//! Only the SOURCE of the adjacency entries changes: a far end the seed
//! already carries becomes a ONE-ELEMENT end set, and the expansion cuts the
//! row to it. Every guard downstream — isomorphism, membership, lean binding,
//! the demand — runs exactly as before, so the relationship variable still
//! binds and a `WHERE` on its properties still filters. These tests exist
//! mostly to prove that.
//!
//! A COUNT probe was tried first and bought nothing measurable: a count cannot
//! be filtered, so it only PRECEDED the walk it was meant to replace.

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

/// a—b with a dated edge, and c connected to neither.
fn fixture() -> Graph {
    let g = g();
    let a = person(&g, "a");
    let b = person(&g, "b");
    person(&g, "c");
    let mut m = BTreeMap::new();
    m.insert("since".to_string(), Value::Int(2011));
    g.create_rel(a, "KNOWS", b, &m).expect("rel");
    let _ = g.warm();
    g
}

#[test]
fn an_absent_edge_yields_no_rows() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'c'}) MATCH (x)-[:KNOWS]-(y) RETURN count(*) AS n",
    );
    assert_eq!(r[0][0], Value::Int(0), "{r:?}");
}

#[test]
fn a_present_edge_still_yields_its_row() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) MATCH (x)-[:KNOWS]-(y) RETURN count(*) AS n",
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn a_connected_pair_still_binds_its_relationship_variable() {
    // THE REASON THIS IS A PRE-FILTER. The probe returns a COUNT; the query
    // needs the edge itself, so a surviving pair must still expand.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) \
         MATCH (x)-[k:KNOWS]-(y) RETURN k.since AS since",
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::Int(2011), "the edge's property is readable");
}

#[test]
fn a_where_on_the_relationship_still_filters() {
    let g = fixture();
    let kept = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) \
         MATCH (x)-[k:KNOWS]-(y) WHERE k.since > 2010 RETURN count(*) AS n",
    );
    assert_eq!(kept[0][0], Value::Int(1), "{kept:?}");
    let dropped = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) \
         MATCH (x)-[k:KNOWS]-(y) WHERE k.since > 2020 RETURN count(*) AS n",
    );
    assert_eq!(
        dropped[0][0],
        Value::Int(0),
        "the edge exists but fails the WHERE"
    );
}

#[test]
fn direction_is_respected_by_the_probe() {
    // `a -> b` exists; `b -> a` does not. A probe that ignored direction would
    // answer 1 for both.
    let g = fixture();
    let fwd = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) MATCH (x)-[:KNOWS]->(y) RETURN count(*) AS n",
    );
    assert_eq!(fwd[0][0], Value::Int(1), "{fwd:?}");
    let back = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'b'}) MATCH (y)-[:KNOWS]->(x) RETURN count(*) AS n",
    );
    assert_eq!(back[0][0], Value::Int(0), "{back:?}");
}

#[test]
fn an_optional_match_over_an_absent_edge_still_nulls_its_row() {
    // Returning no rows is what the expansion would have done, so OPTIONAL
    // MATCH must still produce its null row — the probe must not swallow it.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (x:P {name:'a'}), (y:P {name:'c'}) \
         OPTIONAL MATCH (x)-[k:KNOWS]-(y) RETURN x.name AS name, k AS edge",
    );
    assert_eq!(r.len(), 1, "the row survives: {r:?}");
    assert_eq!(r[0][1], Value::Null, "with a null edge: {r:?}");
}

#[test]
fn it_engages_on_an_absent_edge() {
    let g = fixture();
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (x:P {name:'a'}), (y:P {name:'c'}) MATCH (x)-[:KNOWS]-(y) RETURN count(*) AS n",
        )
    })
    .1;
    assert!(
        t.counters()
            .contains_key("interp.hop end pinned to the id the seed already carries"),
        "the bound end was not pinned: {:?}",
        t.counters()
    );
    assert!(
        t.counters()
            .contains_key("interp.expansion read only the edges to a known peer"),
        "the row was walked rather than cut: {:?}",
        t.counters()
    );
}
