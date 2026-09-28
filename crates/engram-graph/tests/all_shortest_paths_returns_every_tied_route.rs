//! `allShortestPaths` — and the per-endpoint grouping that `shortestPath`
//! also needed.
//!
//! Two things are under test and they are easy to confuse.
//!
//! **`allShortestPaths` returns every route of minimum length**, where
//! `shortestPath` returns one. That is not a `LIMIT` relationship: a caller
//! cannot recover the set from the single answer, which is why the two are
//! different wrappers rather than one wrapper and a clause.
//!
//! **Shortest is per ENDPOINT PAIR, not per seed row.** This is the bug the
//! file was written to pin. The path expansion runs once per seed and takes
//! its minimum over all of that seed's partials — which looks per-pair and is
//! not, because one seed reaches many end nodes. Three ends at distances 1, 1
//! and 2 returned ONE row where Neo4j returns three: the nearest end kept, the
//! rest silently dropped. It was invisible to every existing test because they
//! all bind the end node, and a bound end has exactly one pair.

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

fn err(g: &Graph, src: &str) -> String {
    match parse_statement(src) {
        Err(e) => format!("{e:?}"),
        Ok(stmt) => match run_query(g, &stmt, BTreeMap::new()) {
            Err(e) => format!("{e:?}"),
            Ok(r) => panic!("expected a refusal, got {} row(s)", r.rows.len()),
        },
    }
}

/// `a` reaches `z` by three routes of length 2 and one of length 3, and
/// separately reaches `near` at 1 and `far` at 2.
fn fixture() -> (Graph, u64) {
    let g = g();
    let a = node(&g, "a");
    for (l, r) in [("l", "z"), ("m", "z"), ("d", "z")] {
        let mid = node(&g, l);
        let end = if r == "z" {
            // One `z`, created on the first pass and found thereafter.
            match rows(&g, "MATCH (z:N {name:'z'}) RETURN id(z)").first() {
                Some(row) => match row.first() {
                    Some(Value::Int(i)) => *i as u64,
                    _ => node(&g, "z"),
                },
                None => node(&g, "z"),
            }
        } else {
            node(&g, r)
        };
        edge(&g, a, mid);
        edge(&g, mid, end);
    }
    // A longer route to `z`, which must never appear in either answer.
    let x = node(&g, "x");
    let y = node(&g, "y");
    edge(&g, a, x);
    edge(&g, x, y);
    let z = match rows(&g, "MATCH (z:N {name:'z'}) RETURN id(z)")[0][0].clone() {
        Value::Int(i) => i as u64,
        other => panic!("expected an id, got {other:?}"),
    };
    edge(&g, y, z);
    (g, a)
}

// ─── The new capability ────────────────────────────────────────────────────

#[test]
fn all_shortest_paths_returns_every_tied_route_and_shortest_path_returns_one() {
    let (g, _) = fixture();
    let one = rows(
        &g,
        "MATCH p = shortestPath((a:N {name:'a'})-[*]->(b:N {name:'z'})) RETURN length(p)",
    );
    let all = rows(
        &g,
        "MATCH p = allShortestPaths((a:N {name:'a'})-[*]->(b:N {name:'z'})) RETURN length(p)",
    );
    assert_eq!(one.len(), 1, "shortestPath must return exactly one route");
    assert_eq!(
        all.len(),
        3,
        "allShortestPaths must return every route of minimum length — three here, and a \
         caller cannot recover them from shortestPath's single answer, which is why these \
         are two wrappers and not a wrapper plus a LIMIT",
    );
    for r in one.iter().chain(all.iter()) {
        assert_eq!(
            r[0],
            Value::Int(2),
            "a longer route reached the answer: minimum length is 2 and the 3-hop route \
             through x and y must never appear",
        );
    }
}

#[test]
fn all_shortest_paths_agrees_whether_or_not_the_endpoints_were_already_bound() {
    // THE FAST-PATH TRAP. `try_shortest_path_bfs` answers with ONE route and
    // fires only when both endpoints are already bound in the row. Ungated it
    // would make the same query return one row or three depending on whether
    // an earlier clause happened to bind them — a result that depends on the
    // shape of the query around it rather than on the graph.
    let (g, _) = fixture();
    let inline = rows(
        &g,
        "MATCH p = allShortestPaths((a:N {name:'a'})-[*]->(b:N {name:'z'})) RETURN length(p)",
    );
    let bound = rows(
        &g,
        "MATCH (a:N {name:'a'}), (b:N {name:'z'}) WITH a, b \
         MATCH p = allShortestPaths((a)-[*]->(b)) RETURN length(p)",
    );
    assert_eq!(
        inline.len(),
        bound.len(),
        "binding the endpoints in an earlier clause changed the answer: {} rows inline \
         against {} bound — the fast path answered a question it was not asked",
        inline.len(),
        bound.len(),
    );
    assert_eq!(inline.len(), 3);
}

// ─── The grouping bug this file exists to pin ──────────────────────────────

#[test]
fn shortest_is_per_endpoint_pair_and_not_per_seed_row() {
    // The regression. One seed, three reachable ends at distances 1, 1 and 2.
    // A single minimum over the seed's partials keeps only the nearest and
    // drops the rest; the answer must carry one row per PAIR.
    let g = g();
    let a = node(&g, "a");
    let near = node(&g, "near");
    let mid = node(&g, "mid");
    let far = node(&g, "far");
    edge(&g, a, near);
    edge(&g, a, mid);
    edge(&g, mid, far);

    let got = rows(
        &g,
        "MATCH p = shortestPath((a:N {name:'a'})-[*]->(b:N)) \
         RETURN b.name, length(p) ORDER BY b.name",
    );
    let want = vec![
        vec![Value::Str("far".into()), Value::Int(2)],
        vec![Value::Str("mid".into()), Value::Int(1)],
        vec![Value::Str("near".into()), Value::Int(1)],
    ];
    assert_eq!(
        got, want,
        "shortestPath with an UNBOUND end must answer once per pair. Taking one minimum \
         across the seed's partials keeps the closest end and silently drops the others — \
         a wrong answer, and invisible whenever the end is bound",
    );
}

#[test]
fn all_shortest_paths_is_also_per_endpoint_pair() {
    // The same grouping, on the wrapper where a group can hold more than one
    // row — so a bug that merged groups would show as too many rows for one
    // pair rather than too few for another.
    let g = g();
    let a = node(&g, "a");
    let p1 = node(&g, "p1");
    let p2 = node(&g, "p2");
    let x = node(&g, "x");
    let y = node(&g, "y");
    // a reaches x by two routes of length 2, and y by one of length 1.
    edge(&g, a, p1);
    edge(&g, p1, x);
    edge(&g, a, p2);
    edge(&g, p2, x);
    edge(&g, a, y);

    let got = rows(
        &g,
        "MATCH p = allShortestPaths((a:N {name:'a'})-[*]->(b:N)) \
         RETURN b.name, length(p) ORDER BY b.name, length(p)",
    );
    // p1, p2 and y at 1; x twice at 2. Five rows.
    assert_eq!(
        got.len(),
        5,
        "expected five rows — p1, p2 and y at distance 1, and x TWICE at distance 2 — got \
         {got:?}",
    );
    let x_rows: Vec<_> = got
        .iter()
        .filter(|r| r[0] == Value::Str("x".into()))
        .collect();
    assert_eq!(
        x_rows.len(),
        2,
        "x is reachable by two equally short routes and both must appear",
    );
    assert!(
        x_rows.iter().all(|r| r[1] == Value::Int(2)),
        "x's routes are length 2",
    );
}

#[test]
fn a_repeated_run_returns_the_tied_routes_in_the_same_order() {
    // `allShortestPaths` returns a SET whose order the standard does not fix,
    // so this engine fixes it: the expansion order is deterministic, so the
    // rows are too. Without this the simulation lane could not replay a query
    // that used it.
    let (g, _) = fixture();
    const Q: &str = "MATCH p = allShortestPaths((a:N {name:'a'})-[*]->(b:N {name:'z'})) \
                     RETURN [n IN nodes(p) | n.name] AS route";
    let first = rows(&g, Q);
    assert_eq!(first.len(), 3);
    for _ in 0..5 {
        assert_eq!(
            rows(&g, Q),
            first,
            "the tied routes came back in a different order on a later run",
        );
    }
}

// ─── Negatives ─────────────────────────────────────────────────────────────

#[test]
fn an_unclosed_all_shortest_paths_is_refused_by_name() {
    let (g, _) = fixture();
    let e = err(
        &g,
        "MATCH p = allShortestPaths((a:N)-[*]->(b:N) RETURN length(p)",
    );
    assert!(
        e.contains("allShortestPaths"),
        "the parse error must name the construct that failed to close, got {e}",
    );
}

#[test]
fn all_shortest_paths_over_an_unreachable_pair_answers_nothing_rather_than_erroring() {
    // The empty case is an ANSWER, not a failure: two components, no route.
    // A wrapper that errored here would make `OPTIONAL MATCH` unusable over it.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let _ = (a, b);
    let got = rows(
        &g,
        "MATCH p = allShortestPaths((x:N {name:'a'})-[*]->(y:N {name:'b'})) RETURN length(p)",
    );
    assert!(
        got.is_empty(),
        "an unreachable pair must return no rows, got {got:?}",
    );
}

#[test]
fn a_zero_length_all_shortest_path_is_the_node_itself() {
    // `*0..` admits the empty path, so a node reaches ITSELF at length 0 and
    // that is the unique minimum — the tie-set must not also include the
    // longer loops back to it.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    edge(&g, a, b);
    edge(&g, b, a);
    let got = rows(
        &g,
        "MATCH p = allShortestPaths((x:N {name:'a'})-[*0..]->(y:N {name:'a'})) RETURN length(p)",
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(0)]],
        "a zero-length path to the node itself is the unique shortest, and the 2-hop loop \
         must not tie with it",
    );
}

#[test]
fn a_bounded_all_shortest_paths_refuses_routes_past_its_bound() {
    // The bound is not advisory: a pair reachable only in three hops must
    // answer NOTHING under `*..2`, not the three-hop route.
    let g = g();
    let a = node(&g, "a");
    let m1 = node(&g, "m1");
    let m2 = node(&g, "m2");
    let z = node(&g, "z");
    edge(&g, a, m1);
    edge(&g, m1, m2);
    edge(&g, m2, z);
    assert!(
        rows(
            &g,
            "MATCH p = allShortestPaths((x:N {name:'a'})-[*..2]->(y:N {name:'z'})) \
             RETURN length(p)",
        )
        .is_empty(),
        "`*..2` must not reach a pair that is three hops apart",
    );
    assert_eq!(
        rows(
            &g,
            "MATCH p = allShortestPaths((x:N {name:'a'})-[*..3]->(y:N {name:'z'})) \
             RETURN length(p)",
        ),
        vec![vec![Value::Int(3)]],
        "`*..3` must reach it",
    );
}
