//! IC11's anchored-endpoint semijoin fast path proven byte-identical to the ordinary
//! multistage expand + country filter AND to the interp — on the shape it targets
//! (`(:Person{id})-[:KNOWS*1..2]-(friend) WITH DISTINCT friend MATCH (friend)-
//! [w:WORK_AT]->(company)-[:IS_LOCATED_IN]->(:Country{name}) WHERE w.workFrom < T …`),
//! including a 2-hop friend, a NON-friend (excluded), a company in the WRONG country
//! (excluded by the anchor), and a `workFrom >= T` edge (excluded by the bound).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn node(g: &Graph, label: &str, props: &[(&str, Value)]) -> u64 {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    g.create_node(&[label.into()], &m).expect("node")
}
fn rel(g: &Graph, s: u64, t: &str, d: u64) {
    g.create_rel(s, t, d, &BTreeMap::new()).expect("rel");
}
fn rel_from(g: &Graph, s: u64, d: u64, from: i64) {
    let mut m = BTreeMap::new();
    m.insert("workFrom".to_string(), Value::Int(from));
    g.create_rel(s, "WORK_AT", d, &m).expect("work");
}
fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap();
    run_query(g, &q, BTreeMap::new()).unwrap().rows
}
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn three(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_ic11_semijoin(true);
    let sj = rows(g, src);
    g.set_ic11_semijoin(false);
    let general = rows(g, src);
    g.set_ic11_semijoin(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (sj, general, interp)
}
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_ic11_semijoin(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline ic11 semijoin")
        .copied()
        .unwrap_or(0)
        > 0
}

#[test]
fn ic11_semijoin_matches_general_and_interp() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p0 = node(&g, "Person", &[("id", i(10))]);
    let p1 = node(&g, "Person", &[("id", i(1))]);
    let p2 = node(&g, "Person", &[("id", i(2))]);
    let p3 = node(&g, "Person", &[("id", i(3))]); // NOT reachable → not a friend
    rel(&g, p0, "KNOWS", p1); // direct friend
    rel(&g, p1, "KNOWS", p2); // 2-hop friend (via p1)
    let c0 = node(&g, "Country", &[("name", s("Country0"))]);
    let cx = node(&g, "Country", &[("name", s("Other"))]);
    let compa = node(&g, "Company", &[("name", s("CompA"))]);
    let compb = node(&g, "Company", &[("name", s("CompB"))]);
    let compc = node(&g, "Company", &[("name", s("CompC"))]);
    rel(&g, compa, "IS_LOCATED_IN", c0);
    rel(&g, compb, "IS_LOCATED_IN", c0);
    rel(&g, compc, "IS_LOCATED_IN", cx); // wrong country
    rel_from(&g, p1, compa, 2010); // survives
    rel_from(&g, p1, compb, 2018); // workFrom >= T → excluded
    rel_from(&g, p2, compa, 2012); // survives
    rel_from(&g, p2, compc, 2013); // wrong country → excluded
    rel_from(&g, p3, compa, 2011); // non-friend → excluded

    let src = "MATCH (:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
        WITH DISTINCT friend \
        MATCH (friend)-[w:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(:Country {name: 'Country0'}) \
        WHERE w.workFrom < 2015 \
        RETURN friend.id AS pid, company.name AS org, w.workFrom AS yr \
        ORDER BY yr ASC, toInteger(pid) ASC, org DESC LIMIT 10";
    let (sj, general, interp) = three(&g, src);
    assert_eq!(sj, general, "ic11 semijoin vs general disagree");
    assert_eq!(sj, interp, "ic11 semijoin vs interp disagree");
    // survivors ordered by (workFrom ASC, friend.id ASC): (p1,CompA,2010),(p2,CompA,2012).
    assert_eq!(
        sj,
        vec![
            vec![i(1), s("CompA"), i(2010)],
            vec![i(2), s("CompA"), i(2012)]
        ],
        "IC11: non-friend, wrong-country, and workFrom>=T all excluded"
    );
    assert!(fired(&g, src), "the IC11 shape must take the semijoin");
}
