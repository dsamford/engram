#![allow(non_snake_case)]
//! A HOPLESS stage 1 in the multistage pipeline (fix 26, v102): `MATCH (t:RT)
//! WHERE t.userId = $u WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN count(p)` is a
//! filtered scan carried into a stage-2 expansion — the OPTIONAL outer's shape
//! — and used to decline to the general path, which expanded the 416 seeds one
//! at a time (6.7 ms on the mirror against Neo4j's 1.7) while the same hop in
//! one MATCH ran on the pipeline in 0.8.
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
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

const MULTISTAGE: &str = "interp.pipeline multistage runs";
const FULL: &str = "graph.nodes materialised in full";

/// 1,200 `:RT {userId, id}` (every third `u1`), each with one or two
/// `-[:PGW]->(:GWP {status})`, beside 2,000 `:Other {userId: 'u1'}` so the
/// partition-wide `userId` probe answers more than the label holds.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for _ in 0..2000 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("u1".into()));
        g.create_node(&["Other".into()], &m).expect("other");
    }
    for i in 0..1200i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "userId".to_string(),
            Value::Str(if i % 3 == 0 { "u1".into() } else { format!("u{}", 2 + i % 5) }),
        );
        m.insert("id".to_string(), Value::Str(format!("rt-{i:05}")));
        let t = g.create_node(&["RT".into()], &m).expect("rt");
        for k in 0..(1 + i % 2) {
            let mut pm = BTreeMap::new();
            pm.insert(
                "status".to_string(),
                Value::Str(if (i + k) % 4 == 0 { "pending".into() } else { "done".into() }),
            );
            let p = g.create_node(&["GWP".into()], &pm).expect("gwp");
            g.create_rel(t, "PGW", p, &BTreeMap::new()).expect("pgw");
        }
    }
    g
}

#[test]
fn a_hopless_stage_one_carried_into_a_stage_two_hop_runs_on_the_pipeline() {
    let g = corpus();
    let _ = rows(&g, "MATCH (t:RT) WHERE t.userId = $u RETURN count(t) AS n"); // warm
    for src in [
        "MATCH (t:RT) WHERE t.userId = $u WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN count(p) AS n",
        "MATCH (t:RT {userId: $u}) WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN count(p) AS n",
        "MATCH (t:RT {userId: $u}) WITH DISTINCT t MATCH (t)-[:PGW]->(p:GWP {status: 'pending'}) RETURN count(p) AS n",
        "MATCH (t:RT) WHERE t.userId = $u WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN p.status AS s, count(*) AS n ORDER BY s",
        "MATCH (t:RT) WHERE t.userId = $u WITH t MATCH (t)-[:PGW]->(p:GWP) WITH p.status AS s, count(*) AS n RETURN s, n ORDER BY s",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, MULTISTAGE) > 0, "`{src}` runs on the multistage pipeline: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}` materialises nothing in full: {c:?}");
    }
    // 400 seeds × (1 or 2 ends): 400 + 200 = 600.
    assert_eq!(
        rows(&g, "MATCH (t:RT) WHERE t.userId = $u WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN count(p) AS n"),
        vec![vec![Value::Int(600)]]
    );
}

/// CONTROL: a stage 1 whose carry is not a pattern variable, or a stage 2 the
/// chain cannot model, still declines — and agrees.
#[test]
fn shapes_outside_the_multistage_class_still_agree() {
    let g = corpus();
    for src in [
        "MATCH (t:RT) WHERE t.userId = $u WITH t.id AS id MATCH (x:RT {id: id}) RETURN count(x) AS n",
        "MATCH (t:RT) WHERE t.userId = $u WITH t MATCH (t)-[:PGW*1..2]->(p) RETURN count(p) AS n",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, MULTISTAGE), 0, "`{src}`: {c:?}");
    }
}
