//! The tutorial from the book, as a program that must compile and run.
//!
//! `docs/book/src/intro/first-graph.md` builds this graph and asks it these
//! questions. Prose cannot be compiled, so a snippet in a book is only as true
//! as the last time someone ran it — and the first draft of that page shipped
//! a `CALL` form that returns no rows here.
//!
//! This runs the same shapes in-process, so `cargo test --examples` fails when
//! the engine stops answering them the way the page says it does.
//!
//! ```text
//! cargo run -p engram-graph --example first_graph
//! ```

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Run one statement and return its rows.
fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let stmt = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &stmt, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// The one scalar out of a one-row, one-column answer.
fn scalar(g: &Graph, src: &str) -> Value {
    let rows = run(g, src);
    assert_eq!(rows.len(), 1, "expected one row from `{src}`, got {rows:?}");
    rows[0][0].clone()
}

fn demo() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));

    // ── The graph the tutorial builds ────────────────────────────────────
    run(
        &g,
        "CREATE (ada:Person     {name: 'Ada Lovelace',    born: 1815}),
                (charles:Person {name: 'Charles Babbage', born: 1791}),
                (mary:Person    {name: 'Mary Somerville', born: 1780})",
    );
    run(
        &g,
        "MATCH (a:Person {name: 'Mary Somerville'}), (b:Person {name: 'Ada Lovelace'})
         CREATE (a)-[:MENTORED {from: 1833}]->(b)",
    );
    run(
        &g,
        "MATCH (a:Person {name: 'Mary Somerville'}), (b:Person {name: 'Charles Babbage'})
         CREATE (a)-[:INTRODUCED]->(b)",
    );
    run(
        &g,
        "MATCH (a:Person {name: 'Ada Lovelace'}), (b:Person {name: 'Charles Babbage'})
         CREATE (a)-[:COLLABORATED_WITH {on: 'Analytical Engine'}]->(b)",
    );

    // ── Everyone, oldest first ───────────────────────────────────────────
    let ordered = run(
        &g,
        "MATCH (p:Person) RETURN p.name AS name, p.born AS born ORDER BY p.born",
    );
    let names: Vec<String> = ordered
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.clone(),
            other => panic!("expected a string name, got {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        ["Mary Somerville", "Charles Babbage", "Ada Lovelace"],
        "the page prints these three in this order"
    );

    // ── Every relationship, with type(r) ─────────────────────────────────
    let rels = run(
        &g,
        "MATCH (m:Person)-[r]->(p:Person)
         RETURN m.name AS from, type(r) AS rel, p.name AS to
         ORDER BY from, to",
    );
    assert_eq!(rels.len(), 3, "three relationships, three rows");

    // ── Variable-length: DISTINCT collapses the two routes to Charles ────
    //
    // The page's point: Charles is reachable directly AND through Ada, and
    // DISTINCT collapses them. If this ever returns three rows, the page is
    // wrong about what DISTINCT is doing.
    let reached = run(
        &g,
        "MATCH (m:Person {name: 'Mary Somerville'})-[*1..2]->(p:Person)
         RETURN DISTINCT p.name AS reached ORDER BY reached",
    );
    assert_eq!(
        reached.len(),
        2,
        "Ada and Charles, each once: {reached:?}"
    );

    // ── OPTIONAL MATCH keeps the row, and count(r) counts non-nulls ──────
    //
    // The page uses Charles to make the point: a plain MATCH would drop him,
    // and count(r) must give 0 rather than 1 for his null.
    let degrees = run(
        &g,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[r]->()
         RETURN p.name AS name, count(r) AS out
         ORDER BY out DESC, name",
    );
    assert_eq!(degrees.len(), 3, "every person keeps a row");
    let last = &degrees[2];
    assert!(
        matches!(&last[0], Value::Str(s) if s == "Charles Babbage"),
        "Charles sorts last on a zero count: {last:?}"
    );
    assert!(
        matches!(last[1], Value::Int(0)),
        "count(r) counts non-null values, so a node with no outgoing \
         relationships is 0 and not 1: {last:?}"
    );

    // ── count(*) is answered by a fold ───────────────────────────────────
    assert!(matches!(
        scalar(&g, "MATCH (p:Person) RETURN count(p) AS people"),
        Value::Int(3)
    ));

    println!("first_graph: every assertion the tutorial page makes still holds");
}

fn main() {
    demo();
}

/// `cargo test --examples` COMPILES an example but does not run its `main`, so
/// an example alone proves the snippet type-checks and nothing about whether it
/// still answers. This is what makes the assertions above run in CI.
#[cfg(test)]
mod tests {
    #[test]
    fn the_documented_behaviour_still_holds() {
        super::demo();
    }
}
