//! Differential: every write statement in the corpus below answers the SAME
//! rows and leaves the SAME graph whether it runs directly (autocommit, each
//! write published as it happens) or inside an owned transaction (every
//! write buffered until commit, every read overlaying the buffer). The
//! transaction form is how the Bolt server now runs every statement, so a
//! divergence here is a read path that fails to see the statement's own
//! earlier writes.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX p_id FOR (n:Person) ON (n.id)").expect("parse"),
        BTreeMap::new(),
    )
    .expect("index");
    g
}

fn direct(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("direct `{src}`: {e}"))
        .rows
}

fn in_txn(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    let rows = r.unwrap_or_else(|e| panic!("txn `{src}`: {e}")).rows;
    g.commit_owned(txn).expect("commit");
    rows
}

/// The state a graph is in, as rows the differential can compare.
fn fingerprint(g: &Graph) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for q in [
        "MATCH (n) RETURN count(n) AS nodes",
        "MATCH ()-[r]->() RETURN count(r) AS rels",
        "MATCH (n) RETURN labels(n) AS l, count(*) AS c ORDER BY l",
        "MATCH ()-[r]->() RETURN type(r) AS t, count(*) AS c ORDER BY t",
        "MATCH (p:Person) RETURN p.id AS id, p.name AS name, p.hits AS hits ORDER BY id",
        "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN p.id AS a, q.id AS b ORDER BY a, b",
    ] {
        out.extend(direct(g, q));
    }
    out
}

const CORPUS: &[&str] = &[
    "UNWIND range(1, 5) AS i CREATE (:Person {id: i, name: 'p' + toString(i)})",
    "UNWIND [1, 1, 2, 6, 6] AS x MERGE (p:Person {id: x}) ON CREATE SET p.name = 'new' + toString(x)",
    "MATCH (a:Person {id: 1}), (b:Person {id: 2}) MERGE (a)-[:KNOWS]->(b)",
    "MATCH (a:Person {id: 1}), (b:Person {id: 2}) MERGE (a)-[:KNOWS]->(b)",
    "MATCH (a:Person {id: 1}) CREATE (a)-[:KNOWS]->(:Person {id: 7, name: 'seven'}) WITH a \
     MATCH (a)-[:KNOWS]->(x) SET x.hits = coalesce(x.hits, 0) + 1 RETURN x.id ORDER BY x.id",
    "CREATE (c:Person {id: 8}) WITH c MATCH (p:Person) WITH c, count(p) AS n SET c.hits = n RETURN n",
    "MATCH (p:Person {id: 3}) SET p.name = 'three' WITH p MATCH (q:Person {name: 'three'}) RETURN q.id",
    "MATCH (p:Person {id: 4}) DETACH DELETE p WITH 1 AS one MATCH (q:Person) RETURN count(q)",
    "MATCH (a:Person {id: 1})-[k:KNOWS]->(b) DELETE k WITH a MATCH (a)-[:KNOWS]->(z) RETURN count(z)",
    "UNWIND range(1, 3) AS i MATCH (p:Person {id: i}) SET p:Tagged WITH p MATCH (t:Tagged) RETURN count(t)",
    "MATCH (p:Person {id: 5}) REMOVE p:Tagged WITH p MATCH (t:Tagged) RETURN count(DISTINCT t)",
    "MERGE (p:Person {id: 9}) MERGE (q:Person {id: 9}) RETURN p = q",
    "MATCH (p:Person) WHERE p.id IN [6, 7] MERGE (p)-[:KNOWS]->(p) RETURN count(*)",
    // The rel-driven seed: a whole-graph relationship scan after a create.
    "MATCH (a:Person {id: 1}) CREATE (a)-[:LIKES]->(:Person {id: 10}) WITH a \
     MATCH ()-[r]->() RETURN count(r)",
    // The type histogram after a buffered create of a new type.
    "CREATE (:Person {id: 11})-[:FOLLOWS]->(:Person {id: 12}) WITH 1 AS one \
     MATCH ()-[r]->() RETURN type(r) AS t, count(*) AS c ORDER BY t",
    // The both-ends-labelled hop count after a buffered edge.
    "MATCH (a:Person {id: 1}) CREATE (a)-[:KNOWS]->(:Person {id: 13}) WITH a \
     MATCH (x:Person)-[:KNOWS]->(y:Person) RETURN count(*)",
    // An EXISTS predicate over buffered edges.
    "CREATE (:Person {id: 14})-[:KNOWS]->(:Person {id: 15}) WITH 1 AS one \
     MATCH (n:Person) WHERE EXISTS { MATCH (n)-[:KNOWS]->() } RETURN count(n)",
];

#[test]
fn every_write_statement_answers_identically_direct_and_inside_a_transaction() {
    let a = graph();
    let b = graph();
    for (i, stmt) in CORPUS.iter().enumerate() {
        let ra = direct(&a, stmt);
        let rb = in_txn(&b, stmt);
        assert_eq!(ra, rb, "statement {i} answered differently inside a transaction:\n{stmt}");
        assert_eq!(
            fingerprint(&a),
            fingerprint(&b),
            "statement {i} left a different graph behind when run inside a transaction:\n{stmt}"
        );
    }
}

#[test]
fn a_transaction_sees_its_own_writes_through_every_derived_structure_and_others_do_not() {
    let g = std::sync::Arc::new(graph());
    direct(&g, "UNWIND range(1, 50) AS i CREATE (:Person {id: i})");
    let _ = g.members(Some("Person")).expect("warm");
    let txn = g.open_txn();
    let (txn, ()) = g.with_txn(txn, || {
        direct(&g, "CREATE (:Person {id: 51})-[:KNOWS]->(:Person {id: 52})");
        // Inside: memberships, counts, index probes and adjacency all see it.
        assert_eq!(g.members(Some("Person")).expect("members").len(), 52);
        assert_eq!(g.count_label_nodes("Person"), 52);
        assert_eq!(g.count_all_nodes(), 52);
        assert_eq!(direct(&g, "MATCH (p:Person {id: 51}) RETURN p.id").len(), 1);
        assert_eq!(
            direct(&g, "MATCH (p:Person {id: 51})-[:KNOWS]->(q) RETURN q.id"),
            vec![vec![Value::Int(52)]]
        );
        // Another session (no transaction installed) sees the committed state only.
        std::thread::scope(|s| {
            let g2 = std::sync::Arc::clone(&g);
            s.spawn(move || {
                assert_eq!(g2.members(Some("Person")).expect("members").len(), 50);
                assert_eq!(g2.count_label_nodes("Person"), 50);
                assert_eq!(direct(&g2, "MATCH (p:Person {id: 51}) RETURN p").len(), 0);
            });
        });
    });
    g.commit_owned(txn).expect("commit");
    assert_eq!(g.members(Some("Person")).expect("members").len(), 52);
    assert_eq!(g.count_label_nodes("Person"), 52);
}
