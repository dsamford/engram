//! A both-ends-bound existence probe — `(a)-[:KNOWS]-(b)` as a predicate — is
//! answered from the resident adjacency table.
//!
//! `Graph::adjacency_probe` kept its own per-node snapshots: a store prefix
//! scan each, in ONE latched map capped at 65,536 entries and cleared whole at
//! the cap. SNB BI bi15's edge-centric weights probe `(c1)-[:KNOWS]-(c2)` once
//! per reply between two people: 20,471 snapshot builds for 1/200 of SF3's
//! comments, and at SF10 — ~65k people, twice over, past the cap — the map
//! thrashed until a step that needs no store read ran past 900 s with a fifth
//! of its CPU in the kernel.
//!
//! DIFFERENTIAL: the predicate answers as the same KNOWS written as a pattern;
//! the table answers it and no snapshot is built; and a statement's OWN new
//! edge is still seen, through the per-node path a writing transaction keeps.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// 500 people: each KNOWS the next one and the one after that, and has a NEXT
/// to the next one (a KNOWS pair) and to the third along (not one).
fn ring() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 499) AS i CREATE (:P {id: i})");
    ddl(&g, "CREATE INDEX p_id FOR (n:P) ON (n.id)");
    ddl(
        &g,
        "UNWIND range(0, 499) AS i UNWIND [1, 2] AS d \
         MATCH (a:P {id: i}), (b:P {id: (i + d) % 500}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 499) AS i UNWIND [1, 3] AS d \
         MATCH (a:P {id: i}), (b:P {id: (i + d) % 500}) CREATE (a)-[:NEXT]->(b)",
    );
    let _ = g.warm();
    g
}

const PREDICATE: &str = "MATCH (a:P) MATCH (a)-[:NEXT]->(b:P) WHERE (a)-[:KNOWS]-(b) \
     OPTIONAL MATCH (b)-[:NEXT]->(c) WITH a.id AS i, c IS NULL AS none \
     RETURN count(*) AS n, sum(i) AS s";

const AS_A_PATTERN: &str = "MATCH (a:P) MATCH (a)-[:NEXT]->(b:P), (a)-[:KNOWS]-(b) \
     OPTIONAL MATCH (b)-[:NEXT]->(c) WITH a.id AS i, c IS NULL AS none \
     RETURN count(*) AS n, sum(i) AS s";

#[test]
fn the_probe_answers_from_the_table_as_the_pattern_does() {
    let g = ring();
    let (want, _) = run(&g, AS_A_PATTERN);
    // 500 NEXT pairs that KNOW each other, each b with two NEXTs onward
    assert_eq!(
        want,
        vec![vec![Value::Int(1_000), Value::Int(2 * (0..500).sum::<i64>())]],
        "the pattern's own answer"
    );
    let _ = run(&g, PREDICATE); // warm: the probes admit the tables
    let (got, c) = run(&g, PREDICATE);
    assert_eq!(got, want, "the table-served probe changed the answer");
    assert!(
        get(&c, "graph.adjacency probe answered by the table") > 0,
        "the probe never reached the table: {c:?}"
    );
    assert_eq!(
        get(&c, "graph.adjacency snapshots built"),
        0,
        "a per-node snapshot was built beside a current table: {c:?}"
    );
}

/// Inside a statement that WRITES the edge, the probe must see it: the table is
/// committed state, and a writing transaction keeps the per-node path, which
/// overlays its own rows.
#[test]
fn a_statements_own_new_edge_is_seen_by_the_probe() {
    let g = ring();
    let _ = run(&g, PREDICATE);
    let (rows, _) = run(
        &g,
        "MATCH (a:P {id: 0}), (b:P {id: 250}) CREATE (a)-[:KNOWS]->(b) \
         WITH a, b WHERE (a)-[:KNOWS]-(b) RETURN count(*) AS n",
    );
    assert_eq!(rows, vec![vec![Value::Int(1)]], "the new edge was not seen");
}
