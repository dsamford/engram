#![allow(non_snake_case)]
//! An UNDIRECTED hop whose relationship variable is read only by property
//! binds it lean, as a directed hop has since fix 103: its two sides are
//! visited one after the other (O then I, the I side's self-loop skipped, the
//! order a `Both` visit delivers), so each entry knows which end it leaves
//! from. It used to take the full walk: the start's adjacency bodies read from
//! the store and every relationship decoded in full. SNB Interactive IS3,
//! `(n:Person)-[r:KNOWS]-(friend)` reading `r.creationDate`, decoded 1,190.
//!
//! A hop binding 64 or more such relationships reads their properties as one
//! batch per property (`Graph::rel_prop_aligned`: what an earlier statement
//! read is served again without a read); fewer keep the projected read per
//! edge.
//!
//! The oracle is the same pattern with the relationship returned WHOLE, which
//! keeps the full walk: the rows must agree as multisets, self-loops,
//! parallel edges and missing properties included.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn traced(g: &Graph, src: &str, id: i64) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Int(id));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, p.clone())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const BATCHED: &str = "interp.matcher bound a hop's relationships by a batched property read";
const PROJECTED: &str = "interp.matcher bound a relationship by a projected read";
const FULL_RELS: &str = "graph.rels materialised in full";
const SERVED: &str = "graph.relationship property served from values already read";

/// Node 0 is a hub: 120 neighbours, a third reached OUT, a third IN, a third
/// both ways (two relationships); two parallel edges to neighbour 1; a
/// self-loop. Every tenth relationship carries `w` and no `since`. Node 1000
/// has three relationships, one each way and one self-loop.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&["P".into()], &m).expect("node")
    };
    let mut since = 0i64;
    let mut rel = |a: u64, b: u64| {
        since += 1;
        let mut m = BTreeMap::new();
        if since % 10 == 0 {
            m.insert("w".to_string(), Value::Float(since as f64 / 4.0));
        } else {
            m.insert("since".to_string(), Value::Int(since));
        }
        g.create_rel(a, "KNOWS", b, &m).expect("rel");
    };
    let hub = node(0);
    for i in 1..=120i64 {
        let b = node(i);
        match i % 3 {
            0 => rel(hub, b),
            1 => rel(b, hub),
            _ => {
                rel(hub, b);
                rel(b, hub);
            }
        }
        if i == 1 {
            rel(hub, b);
            rel(hub, b);
        }
    }
    rel(hub, hub);
    let small = node(1000);
    let x = node(1001);
    let y = node(1002);
    rel(small, x);
    rel(y, small);
    rel(small, small);
    g
}

/// IS3's shape: its ORDER BY is what sends it to the matcher (without one the
/// pipeline answers, and binds the relationship in full).
const LEAN: &str = "MATCH (a:P {id: $id})-[r:KNOWS]-(b) \
    RETURN b.id AS b, r.since AS since, r.w AS w ORDER BY since DESC, toInteger(b) ASC";
const WHOLE: &str = "MATCH (a:P {id: $id})-[r:KNOWS]-(b) RETURN b.id AS b, r AS r";

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    rows
}

/// The full walk's rows, projected as the lean statement projects them.
fn oracle(g: &Graph, id: i64) -> Vec<Vec<Value>> {
    let (whole, c) = traced(g, WHOLE, id);
    assert!(count_of(&c, FULL_RELS) > 0, "the oracle did not take the full walk: {c:?}");
    sorted(
        whole
            .into_iter()
            .map(|r| {
                let Value::Rel { props, .. } = &r[1] else { panic!("{:?}", r[1]) };
                vec![
                    r[0].clone(),
                    props.get("since").cloned().unwrap_or(Value::Null),
                    props.get("w").cloned().unwrap_or(Value::Null),
                ]
            })
            .collect(),
    )
}

#[test]
fn a_hub_binds_its_relationships_lean_in_one_batch() {
    let g = corpus();
    let want = oracle(&g, 0);
    // 40 OUT + 40 IN + 40 x 2 both ways + 2 parallel + the self-loop once
    assert_eq!(want.len(), 40 + 40 + 80 + 2 + 1, "fixture");
    let (got, c) = traced(&g, LEAN, 0);
    assert_eq!(sorted(got), want, "the lean rows differ from the full walk's");
    assert_eq!(count_of(&c, FULL_RELS), 0, "a relationship was decoded in full: {c:?}");
    assert_eq!(count_of(&c, BATCHED), 163, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
    // the second statement reads the values the first kept
    let (again, c) = traced(&g, LEAN, 0);
    assert_eq!(sorted(again), want, "the second run");
    assert!(count_of(&c, SERVED) > 0, "nothing was served from the kept values: {c:?}");
}

#[test]
fn b_a_small_hop_binds_lean_with_a_read_per_edge() {
    let g = corpus();
    let want = oracle(&g, 1000);
    assert_eq!(
        want,
        sorted(vec![
            vec![Value::Int(1000), Value::Int(166), Value::Null],
            vec![Value::Int(1001), Value::Int(164), Value::Null],
            vec![Value::Int(1002), Value::Int(165), Value::Null],
        ]),
        "fixture"
    );
    let (got, c) = traced(&g, LEAN, 1000);
    assert_eq!(sorted(got), want);
    assert_eq!(count_of(&c, FULL_RELS), 0, "{c:?}");
    assert_eq!(count_of(&c, BATCHED), 0, "{c:?}");
    // a projected read finds each of the three records
    assert_eq!(count_of(&c, PROJECTED), 3, "{c:?}");
}

/// A self-loop is ONE match of an undirected hop on either path.
#[test]
fn c_a_self_loop_is_matched_once() {
    let g = corpus();
    let (lean, _) = traced(
        &g,
        "MATCH (a:P {id: $id})-[r:KNOWS]-(b) WHERE b.id = $id \
         RETURN b.id AS b, r.since AS since ORDER BY since DESC, toInteger(b) ASC",
        0,
    );
    let (whole, c) = traced(&g, "MATCH (a:P {id: $id})-[r:KNOWS]-(b) WHERE b.id = $id RETURN r AS r", 0);
    assert!(count_of(&c, FULL_RELS) > 0, "{c:?}");
    assert_eq!(lean, vec![vec![Value::Int(0), Value::Int(163)]]);
    assert_eq!(whole.len(), 1, "{whole:?}");
}
