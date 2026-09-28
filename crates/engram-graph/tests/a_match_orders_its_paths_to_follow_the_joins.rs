//! The paths of one `MATCH` are planned in an order that follows their joins.
//!
//! A `MATCH`'s paths are conjunctive, so their order changes COST and not the
//! result set — the same argument `normalize_cartesian_matches` makes to split
//! a cartesian `MATCH` into clauses. That one only handles paths that are bare
//! single nodes; this handles paths with hops, which is where the cost is.
//!
//! SNB BI bi14 opens with three paths — every China person, every India
//! person, and `(person1)-[:KNOWS]-(person2)` joining them. In written order
//! the first two build the full PRODUCT and the selective join runs last.
//! Measured at SF3, that ONE CLAUSE exceeds the 300 s ceiling by itself,
//! against Neo4j's 166 s for the whole query.
//!
//! Reordering conjunctive paths cannot change the answer, so these tests spend
//! their effort on proving exactly that — including the case where the product
//! IS the intended answer.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, label: &str, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&[label.into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// bi14 in miniature: two countries, people in each, and a few KNOWS edges
/// across. The product is 6 x 6; only three pairs actually know each other.
fn fixture() -> Graph {
    let g = g();
    let cn = node(&g, "Country", "China");
    let inn = node(&g, "Country", "India");
    let mut cn_people = Vec::new();
    let mut in_people = Vec::new();
    for (country, who, store) in [(cn, "cn", &mut cn_people), (inn, "in", &mut in_people)] {
        for i in 0..6 {
            let city = node(&g, "City", &format!("{who}city{i}"));
            g.create_rel(city, "IS_PART_OF", country, &BTreeMap::new())
                .expect("rel");
            let p = node(&g, "Person", &format!("{who}{i}"));
            g.create_rel(p, "IS_LOCATED_IN", city, &BTreeMap::new())
                .expect("rel");
            store.push(p);
        }
    }
    for i in 0..3 {
        g.create_rel(cn_people[i], "KNOWS", in_people[i], &BTreeMap::new())
            .expect("rel");
    }
    let _ = g.warm();
    g
}

const BI14_HEAD: &str = "MATCH (c1:Country {name:'China'})<-[:IS_PART_OF]-(city1:City)<-[:IS_LOCATED_IN]-(p1:Person), \
     (c2:Country {name:'India'})<-[:IS_PART_OF]-(city2:City)<-[:IS_LOCATED_IN]-(p2:Person), \
     (p1)-[:KNOWS]-(p2) ";

#[test]
fn the_join_still_selects_only_the_connected_pairs() {
    let g = fixture();
    let r = rows(&g, &format!("{BI14_HEAD} RETURN count(*) AS n"));
    assert_eq!(r[0][0], Value::Int(3), "three KNOWS pairs, not 36: {r:?}");
}

#[test]
fn the_pairs_are_the_right_ones() {
    let g = fixture();
    let r = rows(
        &g,
        &format!("{BI14_HEAD} RETURN p1.name AS a, p2.name AS b ORDER BY a, b"),
    );
    let got: Vec<(String, String)> = r
        .iter()
        .map(|x| match (&x[0], &x[1]) {
            (Value::Str(a), Value::Str(b)) => (a.to_string(), b.to_string()),
            _ => panic!("strings"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("cn0".into(), "in0".into()),
            ("cn1".into(), "in1".into()),
            ("cn2".into(), "in2".into())
        ],
        "{r:?}"
    );
}

#[test]
fn it_engages_on_that_shape() {
    let g = fixture();
    let t = engram_observe::with_trace(|| rows(&g, &format!("{BI14_HEAD} RETURN count(*) AS n"))).1;
    assert!(
        t.counters()
            .contains_key("interp.MATCH paths ordered to follow their joins"),
        "the paths were not reordered: {:?}",
        t.counters()
    );
}

#[test]
fn a_genuine_cartesian_is_still_a_cartesian() {
    // THREE paths that share NOTHING. The product IS the answer, and the
    // reordering must not drop or dedupe any of it.
    let g = g();
    for l in ["A", "B", "C"] {
        for i in 0..3 {
            node(&g, l, &format!("{l}{i}"));
        }
    }
    let r = rows(&g, "MATCH (a:A), (b:B), (c:C) RETURN count(*) AS n");
    assert_eq!(r[0][0], Value::Int(27), "3 x 3 x 3: {r:?}");
}

#[test]
fn paths_already_in_a_connected_order_are_left_alone() {
    let g = fixture();
    let src = "MATCH (p1:Person {name:'cn0'})-[:KNOWS]-(p2:Person), \
               (p2)-[:IS_LOCATED_IN]->(city2:City), \
               (city2)-[:IS_PART_OF]->(c2:Country) \
               RETURN c2.name AS name";
    let (r, t) = engram_observe::with_trace(|| rows(&g, src));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::Str("India".into()));
    assert!(
        !t.counters()
            .contains_key("interp.MATCH paths ordered to follow their joins"),
        "already connected in order; nothing to do: {:?}",
        t.counters()
    );
}

#[test]
fn an_optional_match_is_never_reordered() {
    // OPTIONAL MATCH's null row covers the whole pattern at once; reordering
    // its paths would change which rows null out. The rewrite skips it, and
    // the answer must still carry the unmatched row.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:Person {name:'cn5'}) \
         OPTIONAL MATCH (p)-[:KNOWS]-(q:Person), (q)-[:IS_LOCATED_IN]->(:City) \
         RETURN p.name AS name, q.name AS friend",
    );
    assert_eq!(r.len(), 1, "cn5 knows nobody but still appears: {r:?}");
    assert_eq!(r[0][1], Value::Null, "{r:?}");
}

#[test]
fn a_two_path_match_is_out_of_scope_and_unchanged() {
    // The rewrite only considers three or more paths; two is the shape the
    // existing cartesian split already handles.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (a:Person {name:'cn0'}), (b:Person {name:'in0'}) \
         RETURN count(*) AS n",
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}
