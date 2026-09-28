//! A pipeline reading a property of a FEW bound ids through a label whose
//! column is cached looks the ids up in the cached column, where it copied
//! the label's whole column out and walked it to the last id. LDBC FinBench
//! tcr12 groups by two accounts' ids: the 2.1M-entry Account column was
//! copied and walked per statement at SF10, ~30 of its 36 ms against Neo4j's 2.
//!
//! The oracle is the same statement with the columnar paths off.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const LOOKED_UP: &str = "interp.pipeline bound-var columns looked up in the cached label column";

/// 5,000 accounts (more than the always-whole label size) each with a
/// `score`; one owner owning three of them, one of which carries no score.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut accounts = Vec::new();
    for i in 0..5_000i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        if i != 4_321 {
            m.insert("score".to_string(), Value::Int(i % 97));
        }
        accounts.push(g.create_node(&["Account".into()], &m).expect("account"));
    }
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(1));
    let owner = g.create_node(&["Owner".into()], &m).expect("owner");
    for &a in &[accounts[10], accounts[2_500], accounts[4_321]] {
        g.create_rel(owner, "own", a, &BTreeMap::new()).expect("own");
    }
    g
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

#[test]
fn a_few_ids_are_looked_up_in_the_cached_column() {
    let g = corpus();
    // a whole-label read files the Account columns in the cache
    let (whole, _) = traced(&g, "MATCH (a:Account) RETURN sum(a.score) AS s, count(a.id) AS n");
    assert_eq!(whole.len(), 1);
    let src = "MATCH (o:Owner {id: 1})-[:own]->(a:Account) \
               RETURN a.id AS id, a.score AS score, count(*) AS n ORDER BY id";
    g.set_columnar_scans(false);
    let (want, _) = traced(&g, src);
    g.set_columnar_scans(true);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(
        got,
        vec![
            vec![Value::Int(10), Value::Int(10), Value::Int(1)],
            vec![Value::Int(2_500), Value::Int(2_500 % 97), Value::Int(1)],
            vec![Value::Int(4_321), Value::Null, Value::Int(1)],
        ]
    );
    assert!(
        c.get(LOOKED_UP).copied().unwrap_or(0) > 0,
        "the few ids were not looked up in the cached column: {c:?}"
    );
}
