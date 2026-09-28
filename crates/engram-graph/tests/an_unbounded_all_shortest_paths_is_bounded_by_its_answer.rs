//! `allShortestPaths((a)-[:R*0..]-(b))` — an UNBOUNDED length whose answer is
//! nonetheless bounded.
//!
//! `try_shortest_path_bfs` answers `shortestPath` and declines
//! `allShortestPaths`, so this shape used to reach the general path
//! enumerator with `max = None` and walk the whole reachable space before the
//! "keep the shortest" filter threw away everything longer. That is not a slow
//! query, it is an unbounded one: SNB Interactive IC14 — `KNOWS*0..` between
//! two people across 565,247 edges — OOM-killed a 160 GiB pod at SF3 AND a
//! 1 GiB pod at SF0.1, while Neo4j answers it in a second at SF10.
//!
//! The bound is the answer's own length: no shortest path is longer than the
//! shortest path, so a single BFS names the depth to stop at. These tests pin
//! the ANSWER rather than the speed, because a depth clamp is exactly the kind
//! of optimisation that is fast and wrong — the failure mode is dropping tied
//! routes, or returning nothing where a route exists.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["N".into()], &m).expect("node")
}

fn edge(g: &Graph, a: u64, b: u64) {
    g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// `a` to `z`: THREE routes of length 2, and one of length 3 that must not
/// survive. `orphan` is reachable from nothing.
fn fixture() -> Graph {
    let g = g();
    let a = node(&g, "a");
    let z = node(&g, "z");
    for mid in ["m1", "m2", "m3"] {
        let m = node(&g, mid);
        edge(&g, a, m);
        edge(&g, m, z);
    }
    // the decoy: a-d1-d2-z, length 3
    let d1 = node(&g, "d1");
    let d2 = node(&g, "d2");
    edge(&g, a, d1);
    edge(&g, d1, d2);
    edge(&g, d2, z);
    node(&g, "orphan");
    g
}

#[test]
fn an_unbounded_all_shortest_paths_returns_every_tied_route() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (a {name:'a'}), (z {name:'z'}) \
         MATCH p = allShortestPaths((a)-[:R*0..]-(z)) RETURN length(p) AS l",
    );
    // three ties at 2 — not one, and not four
    assert_eq!(r.len(), 3, "expected all three tied routes, got {r:?}");
    for row in &r {
        assert_eq!(row[0], Value::Int(2), "a longer route survived: {row:?}");
    }
}

#[test]
fn an_unreachable_end_is_an_empty_answer_not_a_walk() {
    let g = fixture();
    // the shape that used to enumerate the whole component before concluding
    // nothing: there is no route at ANY depth, so the bound is "do not start".
    let r = rows(
        &g,
        "MATCH (a {name:'a'}), (o {name:'orphan'}) \
         MATCH p = allShortestPaths((a)-[:R*0..]-(o)) RETURN length(p) AS l",
    );
    assert!(r.is_empty(), "expected no rows, got {r:?}");
}

#[test]
fn a_zero_length_route_to_itself_is_still_found() {
    let g = fixture();
    // `*0..` admits the empty path, so the shortest distance is 0 and the
    // clamp must not round it up to 1 (which would return the a-m-a loops) or
    // treat 0 as "no bound".
    let r = rows(
        &g,
        "MATCH (a {name:'a'}) \
         MATCH p = allShortestPaths((a)-[:R*0..]-(a)) RETURN length(p) AS l",
    );
    assert_eq!(r.len(), 1, "expected the empty path alone, got {r:?}");
    assert_eq!(r[0][0], Value::Int(0));
}

#[test]
fn a_written_upper_bound_is_clamped_too() {
    let g = fixture();
    // A written bound gets the clamp as well. The first version of this file
    // exempted it, and SF3 showed why that was wrong: `*0..` answered in 0 s
    // while `*0..3` took 62 s over the same pair, because only the unbounded
    // form was clamped. The bound's ORIGIN never mattered — no shortest path
    // is longer than the shortest path — and the probe honours the written
    // bound itself, so its distance is already inside it.
    let r = rows(
        &g,
        "MATCH (a {name:'a'}), (z {name:'z'}) \
         MATCH p = allShortestPaths((a)-[:R*1..2]-(z)) RETURN length(p) AS l",
    );
    assert_eq!(r.len(), 3, "expected the three ties, got {r:?}");
}

#[test]
fn shortest_path_over_the_same_shape_still_returns_one() {
    let g = fixture();
    // the probe runs `shortestPath` internally; if its bookkeeping leaked into
    // the caller's plan, the singular form would start returning three.
    let r = rows(
        &g,
        "MATCH (a {name:'a'}), (z {name:'z'}) \
         MATCH p = shortestPath((a)-[:R*0..]-(z)) RETURN length(p) AS l",
    );
    assert_eq!(r.len(), 1, "expected exactly one route, got {r:?}");
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn the_bound_is_actually_applied_written_or_not() {
    // The four tests above pin the ANSWER, and the answer is the same whether
    // the clamp fires or the enumerator walks the whole graph — on a fixture
    // this small, both terminate. So assert the mechanism directly: the clamp
    // engages, for a written bound as well as an unwritten one.
    let g = fixture();
    let unbounded = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (a {name:'a'}), (z {name:'z'}) \
             MATCH p = allShortestPaths((a)-[:R*0..]-(z)) RETURN length(p) AS l",
        )
    })
    .1;
    assert_eq!(
        unbounded
            .counters()
            .get("interp.allShortestPaths bounded by its own BFS distance")
            .copied(),
        Some(1),
        "the clamp did not fire: {:?}",
        unbounded.counters()
    );

    let g = fixture();
    let written = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (a {name:'a'}), (z {name:'z'}) \
             MATCH p = allShortestPaths((a)-[:R*1..2]-(z)) RETURN length(p) AS l",
        )
    })
    .1;
    assert_eq!(
        written
            .counters()
            .get("interp.allShortestPaths bounded by its own BFS distance")
            .copied(),
        Some(1),
        "a written bound must be clamped too: {:?}",
        written.counters()
    );
}

#[test]
fn the_endpoints_may_be_matched_inside_the_pattern_not_bound_before_it() {
    // EVERY OTHER TEST HERE PRE-BINDS BOTH ENDS, and that is not the shape the
    // query this fix exists for writes. SNB Interactive IC14 puts the
    // properties INSIDE the path pattern:
    //
    //   allShortestPaths((p1:Person {id: $a})-[:KNOWS*0..]-(p2:Person {id: $b}))
    //
    // which reaches the matcher with neither end in the seed row. The six
    // tests above would all have passed while that shape walked the graph
    // unbounded, so the coverage was an illusion until this case existed.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH p = allShortestPaths((a:N {name:'a'})-[:R*0..]-(z:N {name:'z'})) \
         RETURN length(p) AS l",
    );
    assert_eq!(r.len(), 3, "the three tied routes, unbound ends: {r:?}");
    for row in &r {
        assert_eq!(row[0], Value::Int(2), "a longer route survived: {row:?}");
    }

    let t = engram_observe::with_trace(|| {
        let g = fixture();
        rows(
            &g,
            "MATCH p = allShortestPaths((a:N {name:'a'})-[:R*0..]-(z:N {name:'z'})) \
             RETURN length(p) AS l",
        )
    })
    .1;
    assert_eq!(
        t.counters()
            .get("interp.allShortestPaths bounded by its own BFS distance")
            .copied(),
        Some(1),
        "the clamp must engage for in-pattern endpoints too: {:?}",
        t.counters()
    );
}
