//! `allShortestPaths` walks only where a shortest route can still go.
//!
//! The depth clamp (`an_unbounded_all_shortest_paths_is_bounded_by_its_answer`)
//! stopped the enumeration at the shortest distance `d` and still let it go
//! EVERYWHERE within it. SNB Interactive IC14, between two people two hops
//! apart at SF3, walked every person within two hops of the first: 84,044
//! relationships and 166,808 people decoded whole, for trails, to keep the
//! routes through the few friends the two share.
//!
//! The prune pushes a frame at `v` after `k` hops only when `v` is at most
//! `d - k` from the far end — a distance map from one BFS off the far end.
//! Every frame at `v` already has `dist(v) >= d - k`, so what survives is
//! exactly the shortest-route DAG. The oracle here does not go near that
//! machinery: between two nodes at distance `d`, every relationship-distinct
//! walk of exactly `d` hops IS a shortest path (one that revisited a node
//! would contain a cycle, and cutting it out would leave a shorter walk), so
//! `MATCH p = (a)-[:R*d..d]-(b)` enumerates the same set by the plain walk.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const PRUNED: &str = "interp.full walk read only the edges on a shortest route";
const RELS_WHOLE: &str = "graph.rels materialised in full";

/// A deterministic tangle: 48 nodes, ~140 `R` edges from a fixed LCG, plus
/// the shapes a prune could get wrong — two PARALLEL edges on a shortest
/// route (two routes, both must survive), a SELF-LOOP beside one (never on a
/// shortest route), a diamond of tied routes, and a longer decoy.
fn tangle() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for i in 0..48 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("n{i}")));
        ids.push(g.create_node(&["N".into()], &m).expect("node"));
    }
    let e = |a: usize, b: usize| {
        g.create_rel(ids[a], "R", ids[b], &BTreeMap::new())
            .expect("rel");
    };
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |n: u64| {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((x >> 33) % n) as usize
    };
    for _ in 0..140 {
        let (a, b) = (next(40), next(40));
        if a != b {
            e(a, b);
        }
    }
    // the diamond n40 -> {n41, n42, n43} -> n44, and a decoy n40-n45-n46-n44
    for m in [41, 42, 43] {
        e(40, m);
        e(m, 44);
    }
    e(40, 45);
    e(45, 46);
    e(46, 44);
    // parallel edges on a shortest route, and a self-loop on its middle node
    e(44, 47);
    e(44, 47);
    e(47, 47);
    // tie the gadget into the tangle
    e(0, 40);
    e(47, 1);
    g
}

/// One route per row: its node names and relationship ids, in walk order.
const SHAPE: &str = "RETURN [n IN nodes(p) | n.name] AS ns, [r IN relationships(p) | id(r)] AS rs";

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{r:?}")).collect();
    out.sort();
    out
}

/// The shortest distance by `shortestPath`, which the prune never touches.
fn distance(g: &Graph, a: &str, arrow: (&str, &str), b: &str) -> Option<i64> {
    let (l, r) = arrow;
    let (rows, _) = run(
        g,
        &format!(
            "MATCH p = shortestPath((a:N {{name:'{a}'}}){l}[:R*1..]{r}(b:N {{name:'{b}'}})) \
             RETURN length(p)"
        ),
    );
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Int(d)) => Some(*d),
        _ => None,
    }
}

#[test]
fn every_shortest_route_survives_and_nothing_else_is_walked() {
    let g = tangle();
    let mut checked = 0;
    let mut routes = 0;
    for (a, b) in [
        ("n40", "n44"),
        ("n40", "n47"),
        ("n40", "n1"),
        ("n0", "n47"),
        ("n3", "n17"),
        ("n5", "n33"),
        ("n12", "n2"),
        ("n21", "n8"),
        ("n9", "n39"),
        ("n30", "n6"),
    ] {
        for arrow in [("-", "-"), ("-", "->"), ("<-", "-")] {
            let Some(d) = distance(&g, a, arrow, b) else {
                continue;
            };
            let (l, r) = arrow;
            let oracle = format!(
                "MATCH p = (a:N {{name:'{a}'}}){l}[:R*{d}..{d}]{r}(b:N {{name:'{b}'}}) {SHAPE}"
            );
            let (want, walked) = run(&g, &oracle);
            for spelling in ["*0..", "*1..", "*..6", "*"] {
                let q = format!(
                    "MATCH p = allShortestPaths((a:N {{name:'{a}'}}){l}[:R{spelling}]{r}\
                     (b:N {{name:'{b}'}})) {SHAPE}"
                );
                let (got, c) = run(&g, &q);
                assert_eq!(
                    sorted(got.clone()),
                    sorted(want.clone()),
                    "`{q}` (distance {d}) answered other routes than the {d}-hop walk"
                );
                assert!(
                    counter(&c, PRUNED) > 0,
                    "`{q}` never took the pruned walk: {c:?}"
                );
                assert!(
                    counter(&c, RELS_WHOLE) <= counter(&walked, RELS_WHOLE),
                    "`{q}` decoded {} relationships whole, more than the plain {d}-hop walk's {}",
                    counter(&c, RELS_WHOLE),
                    counter(&walked, RELS_WHOLE)
                );
                routes += got.len();
                checked += 1;
            }
        }
    }
    assert!(checked >= 60, "only {checked} spellings checked");
    // the parallel edges double the n40 -> n47 routes: 3 x 2
    let (got, _) = run(
        &g,
        &format!(
            "MATCH p = allShortestPaths((a:N {{name:'n40'}})-[:R*0..]->(b:N {{name:'n47'}})) \
             {SHAPE}"
        ),
    );
    assert_eq!(got.len(), 6, "each parallel edge is its own route: {got:?}");
    assert!(routes > checked, "the fixture has no ties to lose: {routes} routes");
}

#[test]
fn the_prune_reads_only_the_routes_between_the_two() {
    // n40 reaches n44 through three middles; the decoy, the tangle behind n0
    // and everything past n44 are never decoded.
    let g = tangle();
    let q = format!(
        "MATCH p = allShortestPaths((a:N {{name:'n40'}})-[:R*0..]-(b:N {{name:'n44'}})) {SHAPE}"
    );
    let (got, c) = run(&g, &q);
    assert_eq!(got.len(), 3, "{got:?}");
    // two hops: the three edges out of n40 toward a middle, then one edge from
    // each middle into n44 — six, where the unpruned walk decoded every edge
    // at n40 and at each of its neighbours
    assert_eq!(
        counter(&c, RELS_WHOLE),
        6,
        "the pruned walk decoded relationships off the shortest routes: {c:?}"
    );
}

#[test]
fn a_shape_the_probe_declines_is_walked_as_before() {
    // a relationship property map, and a minimum above one: the BFS probe
    // declines both, so there is no distance to prune by and the walk (and
    // its answer) is the enumeration's. The minimum case carries a written
    // bound: with no clamp either, `*2..` would enumerate every trail.
    let g = tangle();
    for q in [
        format!(
            "MATCH p = allShortestPaths((a:N {{name:'n40'}})-[:R*2..4]-(b:N {{name:'n44'}})) {SHAPE}"
        ),
        format!(
            "MATCH p = allShortestPaths((a:N {{name:'n40'}})-[:R*0.. {{w: 1}}]-(b:N {{name:'n44'}})) \
             {SHAPE}"
        ),
    ] {
        let (_, c) = run(&g, &q);
        assert_eq!(counter(&c, PRUNED), 0, "`{q}` pruned without a probe: {c:?}");
    }
    let (got, _) = run(
        &g,
        &format!(
            "MATCH p = allShortestPaths((a:N {{name:'n40'}})-[:R*2..4]-(b:N {{name:'n44'}})) {SHAPE}"
        ),
    );
    assert_eq!(got.len(), 3, "{got:?}");
}
