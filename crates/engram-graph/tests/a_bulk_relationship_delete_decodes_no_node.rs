//! A DELETE needs only the IDENTITY of what it removes.
//!
//! `MATCH ()-[r:STRESSED]->() DELETE r` took 417 s on the SF3 bench store on
//! 2026-09-27 to delete ~90,000 relationships, where PostgreSQL and Neo4j did
//! the same reset in 2-12 s. The cost had nothing to do with the deletes:
//! `demands_after` did not model a DELETE clause, so it fell to the catch-all
//! and demanded EVERY variable in full, and every node the unlabelled start
//! seeded — all 9.28M of them — was decoded (~45 us each) to find the ones
//! with a `STRESSED` relationship.
//!
//! This pins the fix on counters, not on time: the delete removes exactly the
//! relationships it names, and decodes no node in full to find them.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn count(g: &Graph, q: &str) -> i64 {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let r = run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("`{q}`: expected a count, got {other:?}"),
    }
}

/// 2,000 `:P` nodes with properties worth decoding, 999 `:T` relationships
/// and 500 `:U` ones the delete must leave alone.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 1999) AS i CREATE (:P {id: i, name: 'person ' + toString(i), bio: 'x'})",
    );
    ddl(
        &g,
        "UNWIND range(0, 998) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) CREATE (a)-[:T]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 499) AS i MATCH (a:P {id: i}), (b:P {id: i + 2}) CREATE (a)-[:U]->(b)",
    );
    g
}

#[test]
fn a_bulk_relationship_delete_decodes_no_node_in_full() {
    let g = fixture();
    assert_eq!(count(&g, "MATCH ()-[r:T]->() RETURN count(r) AS n"), 999);

    let s = parse_statement("MATCH ()-[r:T]->() DELETE r").expect("parses");
    let (_, trace) = engram_observe::with_trace(|| {
        run_query(&g, &s, BTreeMap::new()).expect("runs");
    });
    let c = trace.counters();
    let get = |k: &str| c.get(k).copied().unwrap_or(0);
    assert_eq!(
        get("graph.nodes materialised in full"),
        0,
        "the anonymous ends are read by nothing, and the DELETE needs only r's id: \
         no node may be decoded in full to find what to delete ({c:?})"
    );

    // And it deleted exactly what it named.
    assert_eq!(
        count(&g, "MATCH ()-[r:T]->() RETURN count(r) AS n"),
        0,
        "every :T relationship is gone"
    );
    assert_eq!(
        count(&g, "MATCH ()-[r:U]->() RETURN count(r) AS n"),
        500,
        "no :U relationship was touched"
    );
    assert_eq!(
        count(&g, "MATCH (n:P) RETURN count(n) AS n"),
        2000,
        "no node was deleted"
    );
}

/// A DETACH DELETE of a bound node still removes the node AND its
/// relationships when the node is bound by identity alone.
#[test]
fn a_detach_delete_by_identity_still_removes_the_relationships() {
    let g = fixture();
    ddl(&g, "MATCH (n:P) WHERE n.id < 10 DETACH DELETE n");
    assert_eq!(count(&g, "MATCH (n:P) RETURN count(n) AS n"), 1990);
    assert_eq!(
        count(&g, "MATCH (a:P)-[r:T]->() WHERE a.id < 10 RETURN count(r) AS n"),
        0
    );
    // `:T` joins i -> i+1 for i < 999. Every one touching nodes 0..9 leaves
    // one of them (0->1 .. 9->10; the ones entering 1..9 are the same edges),
    // so 999 - 10 = 989 remain.
    assert_eq!(count(&g, "MATCH ()-[r:T]->() RETURN count(r) AS n"), 989);
    // `:U` joins i -> i+2 for i < 500: likewise the ten leaving nodes 0..9,
    // so 500 - 10 = 490 remain.
    assert_eq!(count(&g, "MATCH ()-[r:U]->() RETURN count(r) AS n"), 490);
}
