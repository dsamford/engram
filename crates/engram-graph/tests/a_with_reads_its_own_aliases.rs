//! A WITH's own `WHERE` and `ORDER BY` read the projection's OUTPUT names, and
//! what they read of an aliased node must reach the variable the alias carries.
//!
//! Found 2026-09-24 while testing a demand change: with `m` aliased and read
//! only by the WITH itself, the stage planner bound `m` with NO properties —
//! the demand walk credited `mm.n` to `mm`, which no pattern binds, and the
//! item's liveness looked only after the WITH:
//!
//! ```text
//! MATCH (m:Message) WITH m AS mm WHERE mm.n > 5 RETURN count(*)          -> 0, not 5
//! MATCH (m:Message) WITH m AS mm ORDER BY mm.n DESC LIMIT 3 RETURN mm.n  -> 1,2,3, not 10,9,8
//! ```
//!
//! A bare carry (`WITH m WHERE m.n > 5`) was right only because its output and
//! input names coincide.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

/// Ten messages n = 1..10 in a NEXT chain; the odd ones are also `:Odd`.
fn chain() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(1, 10) AS i CREATE (:Message {n: i, sq: i * i})");
    ddl(&g, "MATCH (a:Message), (b:Message) WHERE b.n = a.n + 1 CREATE (a)-[:NEXT]->(b)");
    ddl(&g, "MATCH (m:Message) WHERE m.n % 2 = 1 SET m:Odd");
    let _ = g.warm();
    g
}

fn ints(v: &[i64]) -> Vec<Vec<Value>> {
    v.iter().map(|&i| vec![Value::Int(i)]).collect()
}

#[test]
fn a_with_filters_and_orders_on_its_own_aliases() {
    let g = chain();
    for (q, want) in [
        // the breaker WITH's WHERE, nothing after it reading the alias
        ("MATCH (m:Message) WITH m AS mm WHERE mm.n > 5 RETURN count(*) AS c", ints(&[5])),
        // ...beside another item
        (
            "MATCH (m:Message) WITH m AS mm, 1 AS one WHERE mm.n > 5 RETURN count(*) AS c",
            ints(&[5]),
        ),
        // the breaker WITH's ORDER BY + LIMIT, then a different property after it
        (
            "MATCH (m:Message) WITH m AS mm ORDER BY mm.n DESC LIMIT 3 RETURN mm.sq AS s",
            ints(&[100, 81, 64]),
        ),
        // a label the WITH tests through the alias
        ("MATCH (m:Message) WITH m AS mm WHERE mm:Odd RETURN count(*) AS c", ints(&[5])),
        // a PREFIX WITH (a MATCH follows it in the same stage)
        (
            "MATCH (m:Message) WITH m AS mm WHERE mm.n > 5 \
             MATCH (mm)-[:NEXT]->(q:Message) RETURN count(q) AS c",
            ints(&[4]),
        ),
        // swapped names: the output name wins inside the WITH's own scope
        (
            "MATCH (a:Message)-[:NEXT]->(b:Message) WITH a AS b, b AS a WHERE b.n = 3 \
             RETURN a.n AS n",
            ints(&[4]),
        ),
        // inside a CALL {} body, which is where it was found
        (
            "CALL { MATCH (m:Message) WITH m AS mm WHERE mm.n > 5 RETURN count(*) AS c } \
             RETURN c",
            ints(&[5]),
        ),
    ] {
        assert_eq!(rows(&g, q), want, "{q}");
    }
}

#[test]
fn the_bare_spelling_still_answers_the_same() {
    // THE CONTROL: the spelling that was always right must stay right, and
    // must agree with the aliased one.
    let g = chain();
    for (bare, aliased) in [
        (
            "MATCH (m:Message) WITH m WHERE m.n > 5 RETURN count(*) AS c",
            "MATCH (m:Message) WITH m AS mm WHERE mm.n > 5 RETURN count(*) AS c",
        ),
        (
            "MATCH (m:Message) WITH m ORDER BY m.n DESC LIMIT 3 RETURN m.sq AS s",
            "MATCH (m:Message) WITH m AS mm ORDER BY mm.n DESC LIMIT 3 RETURN mm.sq AS s",
        ),
    ] {
        assert_eq!(rows(&g, bare), rows(&g, aliased), "{bare}\n  against\n{aliased}");
    }
}
