#![allow(non_snake_case)]
//! Frontier-BFS variable-length expansion. The load-bearing test is
//! DIFFERENTIAL: the same query with the frontier lever ON must return exactly
//! what it returns with the lever OFF (the enumerating DFS path). The graph is
//! built with cycles and multiple 2-hop paths per node, so the enumerating path
//! genuinely produces duplicate endpoints that the frontier collapses — if the
//! two ever disagree, the frontier is unsound and this fails.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Persons in a small social graph with cycles and diamonds, plus a few
/// messages, so friends-of-friends is reached by more than one path.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut p = Vec::new();
    for i in 0..6u64 {
        let mut props = BTreeMap::new();
        props.insert("id".to_string(), Value::Int(i as i64));
        p.push(g.create_node(&["Person".into()], &props).expect("person"));
    }
    // A diamond (p3,p4 reachable from p0 via both p1 and p2) and a cycle
    // (p3 -> p0), so DFS enumerates several walks to the same node.
    for (a, b) in [(0, 1), (0, 2), (1, 3), (2, 3), (1, 4), (3, 0), (3, 5)] {
        g.create_rel(p[a], "KNOWS", p[b], &BTreeMap::new())
            .expect("knows");
    }
    // Messages so the friends-of-friends -> HAS_CREATOR join has data.
    for (author, n) in [(1usize, 3u64), (3, 2), (4, 1)] {
        for _ in 0..n {
            let m = g
                .create_node(&["Message".into()], &BTreeMap::new())
                .expect("msg");
            g.create_rel(m, "HAS_CREATOR", p[author], &BTreeMap::new())
                .expect("hascreator");
        }
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the frontier lever on and off; return both row sets.
fn both_ways(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_frontier_expand(true);
    let on = rows(g, src);
    g.set_frontier_expand(false);
    let off = rows(g, src);
    g.set_frontier_expand(true);
    (on, off)
}

#[test]
fn frontier_equals_enumeration_on_the_snb_shapes() {
    let g = social();
    // Each of these makes the frontier fire (bounded *1..2 + DISTINCT-only
    // endpoint), over a graph where the enumerating path emits duplicates.
    let queries = [
        // Case 1: WITH DISTINCT, undirected.
        "MATCH (p:Person {id: 0})-[:KNOWS*1..2]-(friend:Person) WHERE NOT friend = p \
         WITH DISTINCT friend RETURN friend.id AS id ORDER BY id",
        // Case 1: WITH DISTINCT, directed.
        "MATCH (p:Person {id: 0})-[:KNOWS*1..2]->(friend:Person) \
         WITH DISTINCT friend RETURN friend.id AS id ORDER BY id",
        // Case 2: collect(DISTINCT ...) — the IC9 breaker.
        "MATCH (root:Person {id: 0})-[:KNOWS*1..2]-(friend:Person) WHERE NOT friend = root \
         WITH collect(DISTINCT friend) AS fs UNWIND fs AS f RETURN f.id AS id ORDER BY id",
        // The IC9 join: DISTINCT friends then their messages.
        "MATCH (root:Person {id: 0})-[:KNOWS*1..2]-(friend:Person) WHERE NOT friend = root \
         WITH DISTINCT friend MATCH (friend)<-[:HAS_CREATOR]-(m:Message) \
         RETURN count(m) AS c",
    ];
    for src in queries {
        let (on, off) = both_ways(&g, src);
        assert_eq!(on, off, "frontier vs enumeration disagree on `{src}`");
        assert!(!on.is_empty(), "`{src}` returned nothing — a vacuous test");
    }
}

#[test]
fn frontier_does_not_fire_without_distinct() {
    // Without DISTINCT the endpoint's multiplicity is OBSERVABLE (several walks
    // reach the same friend), so the frontier must NOT fire. If it wrongly did,
    // ON would collapse duplicates and disagree with OFF. Equality here proves
    // the trigger stayed off.
    let g = social();
    let (on, off) = both_ways(
        &g,
        "MATCH (p:Person {id: 0})-[:KNOWS*1..2]-(friend:Person) \
         RETURN friend.id AS id ORDER BY id",
    );
    assert_eq!(
        on, off,
        "a non-DISTINCT var-length must enumerate identically"
    );
    // And it genuinely has duplicates (the diamond), so the test is not vacuous.
    let ids: Vec<&Value> = on.iter().map(|r| &r[0]).collect();
    let distinct = {
        let mut v: Vec<&Value> = ids.clone();
        v.dedup();
        v.len()
    };
    assert!(
        ids.len() > distinct,
        "expected duplicate endpoints to distinguish the paths"
    );
}

#[test]
fn frontier_respects_a_bound_endpoint_and_min_two() {
    let g = social();
    // A `*2..2` hop (min 2) must NOT use the frontier (shortest-depth != exact
    // length), and a bound endpoint still resolves. Both checked by equality
    // to the enumerating path.
    for src in [
        "MATCH (p:Person {id: 0})-[:KNOWS*2..2]-(friend:Person) WHERE NOT friend = p \
         WITH DISTINCT friend RETURN friend.id AS id ORDER BY id",
        "MATCH (p:Person {id: 0})-[:KNOWS*1..2]-(friend:Person {id: 3}) \
         WITH DISTINCT friend RETURN friend.id AS id ORDER BY id",
    ] {
        let (on, off) = both_ways(&g, src);
        assert_eq!(on, off, "disagreement on `{src}`");
    }
}
