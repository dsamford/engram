//! SNB BI bi10's engram text replaces the reference's two
//! `apoc.path.subgraphNodes` calls with ONE `engram.algo.bfs.stream`.
//!
//! The reference takes the persons within `max` KNOWS hops of the start and
//! subtracts those within `min - 1`. That is the set whose SHORTEST distance
//! lies in `[min, max]` — a BFS depth band. The claim is only worth making if
//! it is checked, so this computes the reference set the long way, with
//! variable-length Cypher and no procedure at all, and requires the two to be
//! equal for every band on a graph built to break a careless equivalence:
//! cycles (a node reachable at 2 AND at 3 hops belongs to depth 2 only),
//! KNOWS edges in BOTH directions (apoc's arrowless filter follows both), and
//! a disconnected island.

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ids(g: &Graph, src: &str) -> BTreeSet<i64> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .into_iter()
        .map(|r| match r.first() {
            Some(Value::Int(n)) => *n,
            other => panic!("expected an id, got {other:?}"),
        })
        .collect()
}

/// A ring of 12 with chords, alternating edge direction, plus an island.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let people: Vec<u64> = (0..15i64)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(i));
            g.create_node(&["Person".into()], &m).expect("person")
        })
        .collect();
    let knows = |a: usize, b: usize| {
        g.create_rel(people[a], "KNOWS", people[b], &BTreeMap::new())
            .expect("knows");
    };
    for i in 0..12 {
        // alternate direction round the ring, so a directed walk would miss half
        if i % 2 == 0 {
            knows(i, (i + 1) % 12);
        } else {
            knows((i + 1) % 12, i);
        }
    }
    knows(0, 6); // a chord: 6 is at depth 1 as well as depth 6 round the ring
    knows(3, 9);
    knows(12, 13); // the island, unreachable from 0
    knows(13, 14);
    let _ = g.warm();
    g
}

#[test]
fn every_depth_band_equals_the_reference_set_difference() {
    let g = social();
    for min in 1..=4 {
        for max in min..=5 {
            let bfs = ids(
                &g,
                &format!(
                    "MATCH (s:Person {{id: 0}}) \
                     CALL engram.algo.bfs.stream({{nodeLabels: ['Person'], \
                       relationshipTypes: ['KNOWS'], orientation: 'UNDIRECTED', \
                       sourceNode: id(s)}}) \
                     YIELD nodeId, depth \
                     WITH nodeId WHERE depth >= {min} AND depth <= {max} \
                     MATCH (p:Person) WHERE id(p) = nodeId RETURN p.id"
                ),
            );
            let within = |k: i64| {
                if k < 1 {
                    return BTreeSet::new();
                }
                ids(
                    &g,
                    &format!(
                        "MATCH (s:Person {{id: 0}})-[:KNOWS*1..{k}]-(p:Person) \
                         WHERE p <> s RETURN DISTINCT p.id"
                    ),
                )
            };
            let reference: BTreeSet<i64> =
                within(max).difference(&within(min - 1)).copied().collect();
            assert_eq!(bfs, reference, "band [{min}, {max}] disagrees");
        }
    }
}

/// Guards against a vacuous pass: the bands must actually select people, the
/// island must never appear, and the chord must pull 6 in to depth 1.
#[test]
fn the_fixture_exercises_what_it_claims() {
    let g = social();
    let band = |min: i64, max: i64| {
        ids(
            &g,
            &format!(
                "MATCH (s:Person {{id: 0}}) \
                 CALL engram.algo.bfs.stream({{nodeLabels: ['Person'], \
                   relationshipTypes: ['KNOWS'], orientation: 'UNDIRECTED', \
                   sourceNode: id(s)}}) \
                 YIELD nodeId, depth \
                 WITH nodeId WHERE depth >= {min} AND depth <= {max} \
                 MATCH (p:Person) WHERE id(p) = nodeId RETURN p.id"
            ),
        )
    };
    assert!(band(1, 1).contains(&6), "the chord should put 6 at depth 1");
    assert!(!band(2, 5).contains(&6), "6 is at depth 1, not deeper");
    let all = band(1, 100);
    assert_eq!(all.len(), 11, "0's component minus 0 itself: {all:?}");
    assert!(all.is_disjoint(&[12, 13, 14].into()), "the island leaked in");
}
