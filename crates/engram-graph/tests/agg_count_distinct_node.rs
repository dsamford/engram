#![allow(non_snake_case)]
//! `count(DISTINCT <bare node var>)` — the DISTINCT dedup key of a node is its id
//! ALONE (`agg_key` in interp.rs), so the columnar aggregate gathers id-only LIGHT
//! nodes and never calls `graph.node()`. Byte-identical to the interp and to a
//! materialised count; proven here by (a) on==off, (b) exact rows, (c) the
//! group-by fires columnar, and (d) ZERO `graph.nodes materialised in full` for
//! the count arg — with a `collect(DISTINCT b)` contrast that DOES materialise, so
//! the id-only shortcut is scoped to `count` and nothing else.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// a1 reaches b1 through TWO parallel `R` edges and b2 through one; a2 reaches b2
/// once. So within group a1, `b` appears three times but is two distinct ids —
/// DISTINCT must collapse the duplicate, and the count-vs-count(DISTINCT) split is
/// observable.
fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |name: &str, label: &str| {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(name.into()));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let a1 = node("a1", "A");
    let a2 = node("a2", "A");
    let b1 = node("b1", "B");
    let b2 = node("b2", "B");
    g.create_rel(a1, "R", b1, &BTreeMap::new()).expect("r");
    g.create_rel(a1, "R", b1, &BTreeMap::new()).expect("r"); // parallel edge → b1 twice under a1
    g.create_rel(a1, "R", b2, &BTreeMap::new()).expect("r");
    g.create_rel(a2, "R", b2, &BTreeMap::new()).expect("r");
    g
}

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

/// (fired-columnar, full-node-materialisations) for one query on the columnar path.
fn fired_and_mats(g: &Graph, src: &str) -> (bool, u64) {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    let fired = !trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain");
    let mats = trace
        .counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0);
    (fired, mats)
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn i(n: i64) -> Value {
    Value::Int(n)
}

#[test]
fn count_distinct_node_is_byte_identical_and_loads_no_nodes() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) RETURN a.name AS an, \
        count(DISTINCT b) AS d, count(b) AS c ORDER BY an ASC";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "count(DISTINCT node) columnar vs interp disagree");
    assert_eq!(
        on,
        vec![
            // a1: b appears {b1,b1,b2} → DISTINCT 2, plain 3
            vec![s("a1"), i(2), i(3)],
            // a2: b appears {b2} → DISTINCT 1, plain 1
            vec![s("a2"), i(1), i(1)],
        ],
        "DISTINCT collapses the parallel-edge duplicate; plain count does not"
    );

    let (fired, mats) = fired_and_mats(&g, src);
    assert!(fired, "the count(DISTINCT node) group-by must run columnar");
    assert_eq!(
        mats, 0,
        "count(DISTINCT node) keys by id — it must load ZERO full nodes"
    );
}

#[test]
fn collect_distinct_node_still_materialises_the_nodes() {
    // The contrast: collect RETURNS the node values, so the id-only shortcut does
    // NOT apply — the full nodes must be materialised (and the result carries their
    // properties). Guards that the fix is scoped to `count` alone.
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) WHERE a.name = 'a1' \
        RETURN collect(DISTINCT b) AS bs";
    let (on, off) = both(&g, src);
    assert_eq!(
        on, off,
        "collect(DISTINCT node) columnar vs interp disagree"
    );
    let (_, mats) = fired_and_mats(&g, src);
    assert!(
        mats >= 2,
        "collect(DISTINCT node) returns the nodes → they must be materialised (got {mats})"
    );
}
