#![allow(non_snake_case)]
//! An OPTIONAL leg that matches NOTHING — every row null-fills — used to
//! decline the whole statement to the general path: the leg's var had an
//! empty distinct set and `eval_column` refused an empty input for its
//! aggregate argument / group key. The reduce now keys and folds such a var
//! as Null without reading a column. Answers are checked against the general
//! path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const OPTIONAL_RUNS: &str = "interp.pipeline optional runs";

/// 200 `:P {n}`; the `:Q` label exists (one node) but no `P` has a `LINK`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        g.create_node(&["P".into()], &m).expect("p");
    }
    let mut q = BTreeMap::new();
    q.insert("id".to_string(), Value::Str("lonely".into()));
    g.create_node(&["Q".into()], &q).expect("q");
    g
}

#[test]
fn a_leg_that_matches_nothing_runs_on_the_optional_pipeline() {
    let g = corpus();
    for src in [
        "MATCH (p:P) OPTIONAL MATCH (p)-[:LINK]->(q:Q) RETURN count(q.id) AS n, count(*) AS rows",
        "MATCH (p:P) OPTIONAL MATCH (p)-[:LINK]->(q:Q) WITH p, collect(q.id) AS ids RETURN p.n AS n, ids ORDER BY n LIMIT 5",
        "MATCH (p:P) OPTIONAL MATCH (p)-[:LINK]->(q:Q) WITH q.id AS qid, count(*) AS c RETURN qid, c",
        "MATCH (p:P) OPTIONAL MATCH (p)-[:LINK]->(q:Q) WITH q, count(*) AS c RETURN q.id AS qid, c",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, OPTIONAL_RUNS) > 0, "`{src}`: {c:?}");
    }
}
