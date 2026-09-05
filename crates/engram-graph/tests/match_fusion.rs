#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Clause fusion for the recognisers (W3): a run of consecutive plain MATCH
//! clauses is handed to the columnar recognisers as ONE multi-path MATCH.
//! Every statement here must answer IDENTICALLY (rows AND order) with the
//! columnar family on and off — the streaming path over the ORIGINAL
//! clauses is the truth the fused pipeline must reproduce. The canary
//! proves the fused ic6 shape actually runs the pipeline, so the equalities
//! cannot green over a path that never fired.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX p_id FOR (n:Person) ON (n.id)").unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    run(&g, "UNWIND range(0, 99) AS i CREATE (:Person {id: i})");
    run(&g, "UNWIND range(0, 19) AS i CREATE (:Tag {id: i, name: 'tag' + toString(i)})");
    run(
        &g,
        "MATCH (a:Person), (b:Person) \
         WHERE b.id IN [(a.id + 1) % 100, (a.id + 7) % 100, (a.id + 13) % 100] \
         CREATE (a)-[:KNOWS {w: a.id}]->(b)",
    );
    run(
        &g,
        "MATCH (p:Person) UNWIND range(0, 4) AS j \
         CREATE (:Message {id: p.id * 10 + j})-[:HAS_CREATOR]->(p)",
    );
    run(
        &g,
        "MATCH (m:Message) MATCH (t:Tag) WHERE t.id = m.id % 20 CREATE (m)-[:HAS_TAG]->(t)",
    );
    g
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

const STATEMENTS: &[&str] = &[
    // The ic6 shape — the measured 67% of one-worker read time.
    "MATCH (p:Person {id: 42})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
     MATCH (m)-[:HAS_TAG]->(t:Tag) \
     RETURN t.name, count(*) AS c ORDER BY c DESC, t.name LIMIT 10",
    // WHERE on both clauses — ANDed by the fusion, must be unobservable.
    "MATCH (p:Person)-[:KNOWS]->(f:Person) WHERE p.id < 10 \
     MATCH (f)<-[:HAS_CREATOR]-(m:Message) WHERE m.id % 2 = 0 \
     RETURN p.id, f.id, m.id ORDER BY p.id, f.id, m.id",
    // Relationship REUSE across clauses: this engine scopes isomorphism per
    // path, so the same edge may bind r and s — fused or not.
    "MATCH (a:Person {id: 1})-[r:KNOWS]->(b:Person) \
     MATCH (a2:Person {id: 1})-[s:KNOWS]->(b2:Person) \
     RETURN b.id, b2.id ORDER BY b.id, b2.id",
    // Three consecutive MATCHes.
    "MATCH (p:Person {id: 7})-[:KNOWS]->(f:Person) \
     MATCH (f)<-[:HAS_CREATOR]-(m:Message) \
     MATCH (m)-[:HAS_TAG]->(t:Tag) \
     RETURN count(*) AS c",
    // A DISJOINT second MATCH (a cartesian join) — fused, then declined by
    // the chain recogniser, then answered by the general path.
    "MATCH (p:Person) WHERE p.id < 3 MATCH (t:Tag) WHERE t.id < 3 \
     RETURN p.id, t.id ORDER BY p.id, t.id",
    // Var-length in the second clause: fused form declines, streaming answers.
    "MATCH (p:Person {id: 5})-[:KNOWS]->(f:Person) \
     MATCH (f)-[:KNOWS*1..2]->(g2:Person) \
     RETURN count(DISTINCT g2) AS c",
    // OPTIONAL between two plain MATCHes: no fusion across it.
    "MATCH (p:Person {id: 9}) \
     OPTIONAL MATCH (p)-[:NOPE]->(x) \
     MATCH (p)-[:KNOWS]->(f:Person) \
     RETURN p.id, x, f.id ORDER BY f.id",
    // An aggregate over the fused chain with grouping on a mid var.
    "MATCH (p:Person)-[:KNOWS]->(f:Person) MATCH (f)<-[:HAS_CREATOR]-(m:Message) \
     WHERE p.id < 20 RETURN f.id, count(m) AS c ORDER BY c DESC, f.id LIMIT 5",
];

#[test]
fn fused_columnar_answers_equal_streaming_on_the_original_clauses() {
    let g = corpus();
    for q in STATEMENTS {
        g.set_columnar_scans(true);
        let columnar = rows(&g, q);
        g.set_columnar_scans(false);
        let streaming = rows(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(
            columnar, streaming,
            "fusion diverged from the original-clause streaming answer on:\n{q}"
        );
    }
}

#[test]
fn the_fused_ic6_shape_actually_runs_the_pipeline() {
    let g = corpus();
    let ((), t) = engram_observe::with_trace(|| {
        let _ = rows(&g, STATEMENTS[0]);
    });
    let c = t.counters();
    assert!(
        c.get("interp.consecutive matches fused for the recognisers")
            .copied()
            .unwrap_or(0)
            > 0,
        "fusion must fire: {c:?}"
    );
    assert!(
        c.get("interp.pipeline aggregate runs").copied().unwrap_or(0) > 0,
        "the fused shape must run the PIPELINE, not stream: {c:?}"
    );
}

#[test]
fn fusion_changes_nothing_inside_a_transaction() {
    // The columnar gates already decline in a txn-with-writes; fusion must
    // not reopen that door.
    let g = corpus();
    let q = parse_statement(
        "CREATE (:Person {id: 900}) WITH 1 AS one \
         MATCH (p:Person {id: 900}) MATCH (q:Person {id: 900}) RETURN p.id, q.id",
    )
    .unwrap();
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(&g, &q, BTreeMap::new()));
    let rows = r.expect("in-txn").rows;
    g.rollback_owned(txn);
    assert_eq!(rows, vec![vec![Value::Int(900), Value::Int(900)]]);
}
