#![allow(non_snake_case)]
//! The IC11 shape: `MATCH (p {id})-[:KNOWS*1..2]-(friend) WITH DISTINCT friend
//! MATCH (friend)-[workAt:WORK_AT]->(company)-[:IS_LOCATED_IN]->(:Country {name})
//! WHERE workAt.workFrom < Y RETURN … ORDER BY workFrom, toInteger(personId), …`.
//! It exercises the two A1 primitives together: a MID-CHAIN inline `{name}` anchor
//! (folded into the stage-2 WHERE) and a `toInteger(personId)` ORDER BY key — both
//! previously declined. Byte-identical to the interp, and FIRES the multistage
//! pipeline (does not stream).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let p10 = mk("Person", &[("id", Value::Int(10))]);
    let f1 = mk("Person", &[("id", Value::Int(11))]);
    let f2 = mk("Person", &[("id", Value::Int(12))]);
    for f in [f1, f2] {
        g.create_rel(p10, "KNOWS", f, &BTreeMap::new())
            .expect("KNOWS");
    }
    let ca = mk("Country", &[("name", Value::Str("Country0".into()))]);
    let cb = mk("Country", &[("name", Value::Str("Country1".into()))]);
    let co1 = mk("Company", &[("name", Value::Str("Alpha".into()))]);
    let co2 = mk("Company", &[("name", Value::Str("Beta".into()))]);
    g.create_rel(co1, "IS_LOCATED_IN", ca, &BTreeMap::new())
        .expect("loc");
    g.create_rel(co2, "IS_LOCATED_IN", cb, &BTreeMap::new())
        .expect("loc");
    let work = |friend: u64, company: u64, from: i64| {
        let mut e = BTreeMap::new();
        e.insert("workFrom".to_string(), Value::Int(from));
        g.create_rel(friend, "WORK_AT", company, &e)
            .expect("WORK_AT");
    };
    work(f1, co1, 2010); // Country0, <2015 → KEEP
    work(f2, co1, 2020); // Country0 but workFrom 2020 ≥ 2015 → drop
    work(f2, co2, 2012); // <2015 but Country1 (not the anchor) → drop
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

const IC11: &str = "MATCH (person:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) WHERE NOT person = friend \
    WITH DISTINCT friend \
    MATCH (friend)-[workAt:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(:Country {name: 'Country0'}) \
    WHERE workAt.workFrom < 2015 \
    RETURN friend.id AS personId, company.name AS org, workAt.workFrom AS wf \
    ORDER BY wf ASC, toInteger(personId) ASC, org DESC LIMIT 10";

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn ic11_on_equals_off_and_fires() {
    let g = g();
    let (on, off) = both(&g, IC11);
    assert_eq!(on, off, "IC11 columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![i(11), s("Alpha"), i(2010)]],
        "only f1's Country0 job under the 2015 cutoff survives"
    );
    assert!(
        !streamed(&g, IC11),
        "IC11 must run columnar (mid-chain anchor + toInteger ORDER BY), not stream"
    );
}
