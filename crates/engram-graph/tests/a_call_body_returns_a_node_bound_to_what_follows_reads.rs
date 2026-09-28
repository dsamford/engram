//! A `CALL {}` body that RETURNS a node binds it to what the clauses after
//! the CALL read of it, not in full — with the same answer.
//!
//! SNB BI bi4's second UNION arm is
//! `UNWIND topForums AS topForum1 MATCH (person:Person)<-[:HAS_MEMBER]-(topForum1:Forum)
//!  RETURN person, 0 AS messageCount`: one row per membership of the top-100
//! forums — ~5M at SF3 — and `RETURN person` read each person in full, for a
//! RETURN after the CALL that reads four of its properties. The trace was
//! 5,085,483 full node reads in a 42 s (serial) statement.

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

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const LEAN: &str = "interp.call body returned a node bound to what the clauses after the call read";
const FULL: &str = "graph.nodes materialised in full";

/// Five forums, 60 people with a wide record (a long `bio`), every person a
/// member of three forums; people with an id divisible by 4 are also `:Admin`.
fn forums() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 4) AS f CREATE (:Forum {id: f})");
    ddl(
        &g,
        "UNWIND range(0, 59) AS i \
         CREATE (:Person {id: i, name: 'p' + toString(i), bio: 'a long biography ' + toString(i * 7919)})",
    );
    ddl(&g, "MATCH (p:Person) WHERE p.id % 4 = 0 SET p:Admin");
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE (p.id + f.id) % 5 < 3 CREATE (f)-[:HAS_MEMBER]->(p)",
    );
    let _ = g.warm();
    g
}

const BODY: &str = "CALL { WITH fs UNWIND fs AS f1 MATCH (p:Person)<-[:HAS_MEMBER]-(f1:Forum) RETURN p, 1 AS c }";

#[test]
fn bi4s_second_arm_returns_people_without_reading_them_whole() {
    let g = forums();
    let lean = format!(
        "MATCH (f:Forum) WITH collect(f) AS fs {BODY} \
         RETURN p.id AS id, p.name AS name, sum(c) AS s ORDER BY id"
    );
    // The control reads the returned node WHOLE after the CALL, which keeps
    // the body's full demand; `whole` groups exactly as `id` does.
    let whole = format!(
        "MATCH (f:Forum) WITH collect(f) AS fs {BODY} \
         RETURN p.id AS id, p.name AS name, sum(c) AS s, p AS whole ORDER BY id"
    );
    let (got, c) = run(&g, &lean);
    let (control, cw) = run(&g, &whole);
    let trimmed: Vec<Vec<Value>> = control.iter().map(|r| r[..3].to_vec()).collect();
    assert_eq!(got, trimmed, "binding the returned node lean changed the answer");
    assert_eq!(got.len(), 60, "every person is a member somewhere: {got:?}");
    assert!(counter(&c, LEAN) > 0, "the body's RETURN kept its full demand: {c:?}");
    assert_eq!(counter(&cw, LEAN), 0, "a whole-node read after the CALL must keep it: {cw:?}");
    // 180 memberships: the control reads each returned person in full.
    assert!(
        counter(&cw, FULL) >= counter(&c, FULL) + 150,
        "{} full reads lean against {} whole",
        counter(&c, FULL),
        counter(&cw, FULL)
    );
}

#[test]
fn every_later_use_answers_as_the_whole_node_does() {
    let g = forums();
    let head = "MATCH (f:Forum) WITH collect(f) AS fs";
    // `lean`: whether the body must bind lean — a case that declines answers
    // as the control trivially, so the ones that must engage are pinned.
    for (tail, lean) in [
        // bi4's own shape: UNION ALL arms, both returning `p`
        (
            "CALL { WITH fs UNWIND fs AS f1 MATCH (p:Person)<-[:HAS_MEMBER]-(f1:Forum) WHERE f1.id < 2 RETURN p, 1 AS c \
             UNION ALL WITH fs UNWIND fs AS f1 MATCH (p:Person)<-[:HAS_MEMBER]-(f1:Forum) RETURN p, 0 AS c } \
             RETURN p.id AS id, sum(c) AS s ORDER BY id"
                .to_string(),
            true,
        ),
        // labels read after the CALL: `labels(p)` reads the record, so the
        // body keeps the whole node (no assertion either way)
        (format!("{BODY} RETURN p.id AS id, labels(p) AS l, count(*) AS n ORDER BY id"), false),
        // a label tested after the CALL, beyond the body's pattern: the
        // body binds `p` with the tested label read from membership
        (format!("{BODY} RETURN p.id AS id, p:Admin AS admin, count(*) AS n ORDER BY id"), true),
        // ...and through a bare carry, which `demands_after` counts as a
        // whole-node use: the body keeps the full node
        (format!("{BODY} WITH p WHERE p:Admin RETURN p.id AS id, count(*) AS n ORDER BY id"), false),
        // a property the body's pattern does not name, read after the CALL
        (format!("{BODY} RETURN p.bio AS bio, count(*) AS n ORDER BY bio LIMIT 5"), true),
    ] {
        let q = format!("{head} {tail}");
        let (got, c) = run(&g, &q);
        assert!(!got.is_empty(), "vacuous: {q}");
        if lean {
            assert!(counter(&c, LEAN) > 0, "the body kept its full demand: {q}");
        }
        // THE CONTROL: `WITH *` before the outer RETURN demands everything,
        // so the body's RETURN keeps its full demand.
        let at = q.rfind(" RETURN ").expect("an outer RETURN");
        let whole = format!("{} WITH *{}", &q[..at], &q[at..]);
        let (control, cc) = run(&g, &whole);
        assert_eq!(counter(&cc, LEAN), 0, "the control bound lean: {whole}");
        assert_eq!(got, control, "\n  {q}\n  against\n  {whole}");
    }
}
