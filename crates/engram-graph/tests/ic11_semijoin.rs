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

/// More survivors than the LIMIT, with years colliding across friends: the
/// semijoin ranks its id rows on the key columns and projects the ten winners
/// alone. It decoded every friend and company whole before ranking — IC11 at
/// SF3 read 11,374 nodes in full for its ten rows.
#[test]
fn ic11_semijoin_projects_its_winners_alone() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p0 = node(&g, "Person", &[("id", i(1000))]);
    let c0 = node(&g, "Country", &[("name", s("Country0"))]);
    let mut comps = Vec::new();
    for k in 0..5 {
        let c = node(&g, "Company", &[("name", s(&format!("Comp{k}")))]);
        rel(&g, c, "IS_LOCATED_IN", c0);
        comps.push(c);
    }
    for f in 0..40i64 {
        let p = node(&g, "Person", &[("id", i(f))]);
        rel(&g, p0, "KNOWS", p);
        rel_from(&g, p, comps[(f % 5) as usize], 2000 + (f % 7));
        rel_from(&g, p, comps[((f + 2) % 5) as usize], 2000 + (f % 3));
    }
    let src = "MATCH (:Person {id: 1000})-[:KNOWS*1..2]-(friend:Person) \
        WITH DISTINCT friend \
        MATCH (friend)-[w:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(:Country {name: 'Country0'}) \
        WHERE w.workFrom < 2005 \
        RETURN friend.id AS pid, company.name AS org, w.workFrom AS yr \
        ORDER BY yr ASC, toInteger(pid) ASC, org DESC LIMIT 10";
    let (sj, general, interp) = three(&g, src);
    assert_eq!(sj, general, "ic11 semijoin vs general disagree");
    assert_eq!(sj, interp, "ic11 semijoin vs interp disagree");
    assert_eq!(sj.len(), 10, "{sj:?}");
    g.set_columnar_scans(true);
    g.set_ic11_semijoin(true);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    let c = trace.counters();
    let n = |k: &str| c.get(k).copied().unwrap_or(0);
    assert!(
        n("interp.pipeline ic11 semijoin ranked its rows before projecting them") > 0,
        "{c:?}"
    );
    assert!(
        n("graph.nodes materialised in full") <= 2 * 10 + 4,
        "more than the winners were decoded whole: {c:?}"
    );
}

/// The country is the far end of more than companies' `IS_LOCATED_IN`: at SNB
/// SF10 every message is located in a country, and the semijoin inserted
/// China's whole in-adjacency into its company set (300 of IC11's 337 ms). The
/// set now holds members of the pattern's `Company` label, found from the
/// company side when that side is smaller -- and a WORK_AT onto a node of
/// another label located in the country is refused, as the pattern says and
/// as the general path and the interpreter already refused it.
#[test]
fn ic11_semijoin_keeps_only_the_companies_of_the_country() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p0 = node(&g, "Person", &[("id", i(10))]);
    let p1 = node(&g, "Person", &[("id", i(1))]);
    rel(&g, p0, "KNOWS", p1);
    let c0 = node(&g, "Country", &[("name", s("Country0"))]);
    let compa = node(&g, "Company", &[("name", s("CompA"))]);
    rel(&g, compa, "IS_LOCATED_IN", c0);
    let uni = node(&g, "University", &[("name", s("Uni"))]);
    rel(&g, uni, "IS_LOCATED_IN", c0);
    for m in 0..300i64 {
        let msg = node(&g, "Message", &[("id", i(10_000 + m))]);
        rel(&g, msg, "IS_LOCATED_IN", c0);
    }
    rel_from(&g, p1, compa, 2010);
    rel_from(&g, p1, uni, 2011); // not a Company: the pattern refuses it
    let src = "MATCH (:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
        WITH DISTINCT friend \
        MATCH (friend)-[w:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(:Country {name: 'Country0'}) \
        WHERE w.workFrom < 2015 \
        RETURN friend.id AS pid, company.name AS org, w.workFrom AS yr \
        ORDER BY yr ASC, toInteger(pid) ASC, org DESC LIMIT 10";
    let (sj, general, interp) = three(&g, src);
    assert_eq!(sj, general, "ic11 semijoin vs general disagree");
    assert_eq!(sj, interp, "ic11 semijoin vs interp disagree");
    assert_eq!(sj, vec![vec![i(1), s("CompA"), i(2010)]]);
    g.set_columnar_scans(true);
    g.set_ic11_semijoin(true);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    let c = trace.counters();
    assert!(
        c.get("interp.pipeline ic11 companies found from the company side").copied().unwrap_or(0) > 0,
        "one company against 302 located entries: {c:?}"
    );
}
