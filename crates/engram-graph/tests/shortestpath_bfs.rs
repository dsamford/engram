#![allow(non_snake_case)]
//! `shortestPath` between two BOUND endpoints (the LDBC IC1/IC13 shape) now runs
//! as a BFS over a visited-node set — O(reachable nodes) — instead of enumerating
//! every rel-distinct walk (O(paths)), which exhausted the process on an unbounded
//! `(a)-[:KNOWS*]-(b)`. The bomb canary below (a clique) would HANG if the old
//! enumeration ran; the BFS returns instantly.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn mk_persons(g: &Graph, n: i64) -> Vec<u64> {
    (0..n)
        .map(|id| {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(id));
            g.create_node(&["Person".into()], &m).expect("person")
        })
        .collect()
}

fn knows(g: &Graph, a: u64, b: u64) {
    g.create_rel(a, "KNOWS", b, &BTreeMap::new())
        .expect("KNOWS");
}

fn run1(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// IC13 shape — unbounded `KNOWS*` between two bound persons. Chain
/// p0—p1—p2—p3—p4 PLUS a shortcut p0—p4: the shortest path is length 1.
#[test]
fn ic13_unbounded_shortest_between_bound_endpoints() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 5);
    for w in p.windows(2) {
        knows(&g, w[0], w[1]);
    }
    knows(&g, p[0], p[4]); // the shortcut
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 4}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(
        r,
        vec![vec![i(1)]],
        "the shortcut gives a length-1 shortest path"
    );
}

/// Without the shortcut, the same unbounded query returns the chain length 4.
#[test]
fn ic13_chain_length() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 5);
    for w in p.windows(2) {
        knows(&g, w[0], w[1]);
    }
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 4}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(r, vec![vec![i(4)]], "the chain is the only path, length 4");
}

/// IC1 shape — a bounded `KNOWS*1..3`. A target 4 hops away is OUT of range → no
/// row; a target within 3 hops returns its distance.
#[test]
fn ic1_bounded_range() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 6);
    for w in p.windows(2) {
        knows(&g, w[0], w[1]); // pure chain p0..p5
    }
    // p0 → p3 is 3 hops (in range).
    let in_range = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 3}), path = shortestPath((a)-[:KNOWS*1..3]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(
        in_range,
        vec![vec![i(3)]],
        "p0→p3 is exactly 3 hops, in range"
    );
    // p0 → p5 is 5 hops (out of the *1..3 range) → no row.
    let out_of_range = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 5}), path = shortestPath((a)-[:KNOWS*1..3]-(b)) \
         RETURN length(path) AS len",
    );
    assert!(
        out_of_range.is_empty(),
        "p0→p5 is 5 hops, out of *1..3 range → no row"
    );
}

/// THE BOMB CANARY: a 40-node KNOWS clique. The old enumeration explores every
/// rel-distinct walk — factorial in the clique size — and never returns (OOM/hang).
/// The BFS visits each node once and finds the length-1 path instantly. If this
/// test does not complete near-instantly, the enumeration regressed back in.
#[test]
fn clique_does_not_enumerate() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 40);
    for &a in &p {
        for &b in &p {
            if a < b {
                knows(&g, a, b);
            }
        }
    }
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 39}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(r, vec![vec![i(1)]], "every pair in a clique is 1 hop apart");
}

/// Two paths of DIFFERENT length between the endpoints — the bidirectional search
/// must return the SHORTER one even when the longer shares the graph. p0—p1—p4
/// (length 2) and p0—p2—p3—p4 (length 3): shortest is 2.
#[test]
fn picks_the_shorter_of_two_paths() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 5);
    knows(&g, p[0], p[1]);
    knows(&g, p[1], p[4]); // p0-p1-p4  (len 2)
    knows(&g, p[0], p[2]);
    knows(&g, p[2], p[3]);
    knows(&g, p[3], p[4]); // p0-p2-p3-p4 (len 3)
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 4}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(
        r,
        vec![vec![i(2)]],
        "the length-2 path wins over the length-3 one"
    );
}

/// An ASYMMETRIC shortest path (odd length 5) forces an uneven fwd/bwd split — the
/// meeting-node accounting must still return 5, and the reconstructed trail's
/// `length` matches.
#[test]
fn odd_length_asymmetric_split() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 6);
    for w in p.windows(2) {
        knows(&g, w[0], w[1]); // chain p0..p5, shortest p0→p5 is 5
    }
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 5}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(r, vec![vec![i(5)]], "the only path is the length-5 chain");
}

/// DIRECTED shortestPath — the forward search follows OUT, the backward search IN.
/// p0→p1→p2 exists but p2→p0 does not; `(a)-[:KNOWS*]->(b)` from p0 to p2 is 2,
/// and from p2 to p0 has NO directed path.
#[test]
fn directed_shortest_path() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 3);
    knows(&g, p[0], p[1]);
    knows(&g, p[1], p[2]);
    let fwd = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 2}), path = shortestPath((a)-[:KNOWS*]->(b)) \
         RETURN length(path) AS len",
    );
    assert_eq!(fwd, vec![vec![i(2)]], "p0→p2 directed is 2 hops");
    let back = run1(
        &g,
        "MATCH (a:Person {id: 2}), (b:Person {id: 0}), path = shortestPath((a)-[:KNOWS*]->(b)) \
         RETURN length(path) AS len",
    );
    assert!(back.is_empty(), "no directed path p2→p0");
}

/// An unreachable endpoint (disjoint components) → no row, and it terminates.
#[test]
fn unreachable_is_empty() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p = mk_persons(&g, 4);
    knows(&g, p[0], p[1]); // {0,1} and {2,3} are disjoint
    knows(&g, p[2], p[3]);
    let r = run1(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person {id: 3}), path = shortestPath((a)-[:KNOWS*]-(b)) \
         RETURN length(path) AS len",
    );
    assert!(r.is_empty(), "no path between disjoint components → no row");
}
