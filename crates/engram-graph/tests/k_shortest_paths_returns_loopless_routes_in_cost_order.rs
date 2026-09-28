//! Yen's `k` shortest paths: the routes, their order, and what it refuses.
//!
//! The word doing the work is **loopless**. The `k` shortest WALKS are easy
//! and almost never what anyone wants: the second-best walk is usually the
//! best one with some detour taken twice. Yen's returns simple paths and pays
//! a Dijkstra per node of each accepted route to do it, which is why this is
//! priced against the all-pairs work ceiling rather than the ordinary ones.
//!
//! Every expected route below is derived in its test's comment from the
//! fixture's arithmetic, not copied from a run.

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

fn edge(g: &Graph, a: u64, b: u64, w: f64) {
    let mut m = BTreeMap::new();
    m.insert("w".to_string(), Value::Float(w));
    g.create_rel(a, "R", b, &m).expect("rel");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn err(g: &Graph, src: &str) -> String {
    match run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new()) {
        Err(e) => format!("{e:?}"),
        Ok(r) => panic!("expected a refusal, got {} row(s)", r.rows.len()),
    }
}

fn call(src: u64, dst: u64, k: usize, extra: &str) -> String {
    format!(
        "CALL engram.algo.kshortestpaths.stream({{nodeLabels:['N'], relationshipTypes:['R'], \
         sourceNode:{src}, targetNode:{dst}, k:{k}{extra}}}) \
         YIELD index, totalCost, nodeIds RETURN index, totalCost, nodeIds"
    )
}

/// s -> a -> t   (cost 2)
/// s -> b -> t   (cost 3)
/// s -> c -> t   (cost 10)
/// Three disjoint routes with distinct costs, so the ordering is unambiguous.
fn three_routes() -> (Graph, u64, u64) {
    let g = g();
    let s = node(&g, "s");
    let t = node(&g, "t");
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, s, a, 1.0);
    edge(&g, a, t, 1.0);
    edge(&g, s, b, 1.0);
    edge(&g, b, t, 2.0);
    edge(&g, s, c, 5.0);
    edge(&g, c, t, 5.0);
    (g, s, t)
}

// ─── The routes ────────────────────────────────────────────────────────────

#[test]
fn the_routes_come_back_in_ascending_cost_order() {
    let (g, s, t) = three_routes();
    let got = rows(&g, &call(s, t, 3, ", relationshipWeightProperty:'w'"));
    assert_eq!(got.len(), 3, "three disjoint routes exist, got {got:?}");
    let costs: Vec<f64> = got
        .iter()
        .map(|r| match r[1] {
            Value::Float(f) => f,
            ref other => panic!("expected a float cost, got {other:?}"),
        })
        .collect();
    assert_eq!(
        costs,
        vec![2.0, 3.0, 10.0],
        "routes must be returned cheapest first",
    );
    for (i, r) in got.iter().enumerate() {
        assert_eq!(
            r[0],
            Value::Int(i as i64),
            "`index` must number the routes in the order they are returned",
        );
    }
}

#[test]
fn asking_for_fewer_routes_returns_the_cheapest_ones() {
    // `k` is a bound, not a target: k=2 must return the two CHEAPEST, not the
    // first two the search happened to find.
    let (g, s, t) = three_routes();
    let got = rows(&g, &call(s, t, 2, ", relationshipWeightProperty:'w'"));
    assert_eq!(got.len(), 2);
    assert_eq!(got[0][1], Value::Float(2.0));
    assert_eq!(got[1][1], Value::Float(3.0));
}

#[test]
fn an_unweighted_projection_counts_hops() {
    // With no `relationshipWeightProperty` every step costs 1, so the cost IS
    // the hop count and the 5+5 route becomes the SAME length as the others.
    // All three are then 2 hops, and all three must be returned.
    let (g, s, t) = three_routes();
    let got = rows(&g, &call(s, t, 5, ""));
    assert_eq!(
        got.len(),
        3,
        "unweighted, all three routes are two hops and all three are shortest",
    );
    for r in &got {
        assert_eq!(r[1], Value::Float(2.0), "two hops each");
    }
}

#[test]
fn the_routes_are_loopless() {
    // A cycle hanging off the middle of the only route. The `k` shortest WALKS
    // would go round it; the `k` shortest PATHS may not, so there is exactly
    // ONE route and asking for five must not manufacture four more by
    // circling.
    let g = g();
    let s = node(&g, "s");
    let m = node(&g, "m");
    let t = node(&g, "t");
    let loop_a = node(&g, "la");
    let loop_b = node(&g, "lb");
    edge(&g, s, m, 1.0);
    edge(&g, m, t, 1.0);
    edge(&g, m, loop_a, 1.0);
    edge(&g, loop_a, loop_b, 1.0);
    edge(&g, loop_b, m, 1.0);

    let got = rows(&g, &call(s, t, 5, ", relationshipWeightProperty:'w'"));
    assert_eq!(
        got.len(),
        1,
        "only one LOOPLESS route exists; the others would repeat `m`, and returning them \
         would make this k-shortest-walks under a name that promises paths — got {got:?}",
    );
}

#[test]
fn each_route_visits_the_source_first_and_the_target_last() {
    let (g, s, t) = three_routes();
    for r in rows(&g, &call(s, t, 3, ", relationshipWeightProperty:'w'")) {
        let Value::List(ids) = &r[2] else {
            panic!("nodeIds must be a list, got {:?}", r[2]);
        };
        assert_eq!(ids.first(), Some(&Value::Int(s as i64)));
        assert_eq!(ids.last(), Some(&Value::Int(t as i64)));
        assert_eq!(ids.len(), 3, "each route here is source, middle, target");
    }
}

#[test]
fn a_repeated_call_returns_the_same_routes_in_the_same_order() {
    // Equal-cost routes are ordered by their node sequence, so ties are total
    // and replayable. Ordering by cost alone would leave them in whatever
    // order the spur loop produced, which is stable until the loop changes.
    let (g, s, t) = three_routes();
    let q = call(s, t, 5, "");
    let first = rows(&g, &q);
    assert_eq!(first.len(), 3);
    for _ in 0..5 {
        assert_eq!(rows(&g, &q), first, "the route order was not reproducible");
    }
}

// ─── Negatives ─────────────────────────────────────────────────────────────

#[test]
fn a_missing_endpoint_is_refused_by_name_rather_than_defaulted() {
    let (g, s, t) = three_routes();
    for (src, want) in [
        (
            format!(
                "CALL engram.algo.kshortestpaths.stream({{nodeLabels:['N'], sourceNode:{s}}}) \
                 YIELD index RETURN index"
            ),
            "targetNode",
        ),
        (
            format!(
                "CALL engram.algo.kshortestpaths.stream({{nodeLabels:['N'], targetNode:{t}}}) \
                 YIELD index RETURN index"
            ),
            "sourceNode",
        ),
    ] {
        let e = err(&g, &src);
        assert!(
            e.contains(want),
            "a call missing an endpoint must name what it needs, got {e}",
        );
    }
}

#[test]
fn an_endpoint_outside_the_projection_is_refused_rather_than_answered_empty() {
    // "No route" and "you named a node this projection does not contain" are
    // different facts. Answering both with nothing leaves a caller unable to
    // tell a typo from a disconnection.
    let (g, s, _) = three_routes();
    let outside = {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str("outside".into()));
        g.create_node(&["Other".into()], &m).expect("node")
    };
    let e = err(&g, &call(s, outside, 2, ""));
    assert!(
        e.contains("not in this projection"),
        "the refusal must say the node is outside the projection, got {e}",
    );
}

#[test]
fn a_k_below_one_is_refused_rather_than_clamped() {
    let (g, s, t) = three_routes();
    let e = err(&g, &call(s, t, 0, ""));
    assert!(
        e.contains('k'),
        "`k` of zero must be refused by name — clamping it to 1 answers a question that was \
         not asked, got {e}",
    );
}

#[test]
fn an_unreachable_target_answers_nothing_rather_than_erroring() {
    // The empty answer IS the answer here: both endpoints are in the
    // projection and no route joins them.
    let g = g();
    let s = node(&g, "s");
    let t = node(&g, "t");
    let x = node(&g, "x");
    edge(&g, s, x, 1.0);
    assert!(
        rows(&g, &call(s, t, 3, "")).is_empty(),
        "an unreachable pair returns no routes and no error",
    );
}

#[test]
fn a_negative_weight_yields_no_route_rather_than_a_wrong_one() {
    // Dijkstra settles each node once, which assumes non-negative steps. A
    // negative one does not make it slower, it makes it WRONG — so the search
    // declines rather than returning a route it cannot vouch for.
    let g = g();
    let s = node(&g, "s");
    let m = node(&g, "m");
    let t = node(&g, "t");
    edge(&g, s, m, 1.0);
    edge(&g, m, t, -5.0);
    assert!(
        rows(&g, &call(s, t, 2, ", relationshipWeightProperty:'w'")).is_empty(),
        "a negative weight must not produce a route the search cannot vouch for",
    );
}

#[test]
fn a_k_shortest_call_over_the_work_ceiling_is_refused_and_names_the_lever() {
    // Yen's cost is `k x V x (E + V log V)`, so `k` multiplies the work — a
    // projection that betweenness would accept can still be refused here for
    // a large enough `k`. The ceiling must see that factor.
    let (g, s, t) = three_routes();
    g.set_algo_work_ceiling(1);
    let e = err(&g, &call(s, t, 3, ""));
    g.set_algo_work_ceiling(10_000_000_000);
    assert!(
        e.contains("ENGRAM_ALGO_WORK_CEILING"),
        "the refusal must name the lever, got {e}",
    );
    assert!(
        e.contains("k x node x edge work"),
        "the refusal must name the quantity, and it must include k — got {e}",
    );
}

#[test]
fn a_spur_may_not_re_enter_an_earlier_node_of_its_own_root() {
    // THE NODE-WITHDRAWAL TEST, and the reason it needs its own fixture.
    //
    // Yen withdraws two things per spur: the EDGES already used by routes
    // sharing this root, and the root's own NODES before the spur. The first
    // test above is satisfied by the edge withdrawal alone, so it proves
    // nothing about the second — removing the node ban left it green.
    //
    // This shape separates them. The shortest route is s-a-b-t. Spurring at
    // `a` withdraws the edge a->b, and the search from `a` can then reach `t`
    // by a->x->s->c->t — which re-enters `s`, a node already on the root. The
    // route would be s,a,x,s,c,t: a repeated node, which is exactly what
    // "loopless" forbids, and the node ban is the only thing that stops it.
    let g = g();
    let s = node(&g, "s");
    let a = node(&g, "a");
    let b = node(&g, "b");
    let t = node(&g, "t");
    let x = node(&g, "x");
    let c = node(&g, "c");
    edge(&g, s, a, 1.0);
    edge(&g, a, b, 1.0);
    edge(&g, b, t, 1.0);
    edge(&g, a, x, 1.0);
    edge(&g, x, s, 1.0);
    edge(&g, s, c, 1.0);
    edge(&g, c, t, 1.0);

    for r in rows(&g, &call(s, t, 5, ", relationshipWeightProperty:'w'")) {
        let Value::List(ids) = &r[2] else {
            panic!("nodeIds must be a list");
        };
        let mut seen = std::collections::BTreeSet::new();
        for id in (ids).iter() {
            assert!(
                seen.insert(format!("{id:?}")),
                "route {ids:?} visits a node twice — a spur re-entered its own root, which \
                 is the loop the node withdrawal exists to prevent",
            );
        }
    }
}

/// k = 1 is one Dijkstra, not Yen's all-pairs shape, so the all-pairs work
/// ceiling must not refuse it. It refused SNB BI bi19 at SF3 and bi19/bi20 at
/// SF10 — "254,507,765,640 k x node x edge work … Nothing was computed" — for
/// a single shortest path. k = 2 still pays the all-pairs price.
#[test]
fn a_single_shortest_path_is_not_priced_as_all_pairs() {
    use std::collections::BTreeMap;
    let g = engram_graph::Graph::new(
        engram_store::Store::new(),
        engram_key::Realm(1),
        engram_key::Namespace(1),
    );
    let s = engram_cypher::parse_any(
        "UNWIND range(0, 29) AS i CREATE (:N {i: i}) WITH count(*) AS c \
         UNWIND range(0, 28) AS i MATCH (a:N {i: i}), (b:N {i: i + 1}) \
         CREATE (a)-[:R {w: 1.0}]->(b)",
    )
    .expect("parse");
    engram_graph::run_stmt(&g, &s, BTreeMap::new()).expect("build");
    let _ = g.warm();
    // 30 nodes x 29 edges = 870 "work": set the ceiling below it
    g.set_algo_work_ceiling(100);
    let q = |k: u32| {
        format!(
            "MATCH (a:N {{i: 0}}), (b:N {{i: 29}}) \
             CALL engram.algo.kshortestpaths.stream({{nodeLabels: ['N'], relationshipTypes: ['R'], \
               relationshipWeightProperty: 'w', sourceNode: id(a), targetNode: id(b), k: {k}}}) \
             YIELD totalCost RETURN totalCost"
        )
    };
    let run = |src: String| {
        let st = engram_cypher::parse_statement(&src).expect("parse");
        engram_graph::run_query(&g, &st, BTreeMap::new()).map(|r| r.rows)
    };
    let one = run(q(1)).expect("k = 1 must be admitted: it is one Dijkstra");
    assert_eq!(one, vec![vec![engram_cypher::Value::Float(29.0)]]);
    let two = run(q(2));
    assert!(
        two.as_ref().err().is_some_and(|e| e.to_string().contains("k x node x edge work")),
        "k = 2 must still pay the all-pairs price: {two:?}"
    );
}
