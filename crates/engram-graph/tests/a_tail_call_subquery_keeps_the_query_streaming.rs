//! A read-only `CALL {}` before the final `RETURN` no longer costs the WHOLE
//! query its streaming.
//!
//! `streamable` refused `Clause::CallSubquery`, and the consequence was not
//! that the subquery ran unstreamed — it was that the ENTIRE query, prefix
//! included, dropped onto the materialising clause-by-clause interpreter.
//!
//! Measured at SF3 with the cleanest discriminator available: SNB BI bi4's
//! prefix alone ran in 96 s, and the SAME prefix with
//! `CALL { WITH topForums RETURN 1 AS x }` appended ran OVER 300 s. That
//! subquery reads nothing and returns one constant row, so nothing but the
//! loss of streaming can account for the difference.
//!
//! These tests pin the ANSWER first — a streaming join and a materialising one
//! must agree exactly, including on the awkward cases (a subquery returning no
//! rows, returning several, or returning no columns at all) — and the
//! engagement second.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn person(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["P".into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn fixture() -> Graph {
    let g = g();
    let a = person(&g, "a");
    let b = person(&g, "b");
    let c = person(&g, "c");
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).expect("rel");
    g.create_rel(a, "KNOWS", c, &BTreeMap::new()).expect("rel");
    g
}

#[test]
fn a_tail_call_streams_and_answers() {
    let g = fixture();
    let (r, t) = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(f:P) RETURN f } \
             RETURN p.name AS p, f.name AS f ORDER BY p, f",
        )
    });
    let got: Vec<(String, String)> = r
        .iter()
        .map(|x| match (&x[0], &x[1]) {
            (Value::Str(a), Value::Str(b)) => (a.to_string(), b.to_string()),
            _ => panic!("strings"),
        })
        .collect();
    assert_eq!(
        got,
        vec![("a".into(), "b".into()), ("a".into(), "c".into())],
        "{r:?}"
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.streamed a read-only chain"),
        "a tail CALL must not cost the query its streaming: {:?}",
        t.sometimes_hit()
    );
}

#[test]
fn a_subquery_returning_no_rows_drops_the_outer_row() {
    // The join is INNER: a subquery with no rows removes its outer row. The
    // streaming path must agree with that, not pass the row through.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(f:P) RETURN f } \
         RETURN p.name AS name ORDER BY name",
    );
    let names: Vec<String> = r
        .iter()
        .map(|x| match &x[0] {
            Value::Str(s) => s.to_string(),
            o => format!("{o:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec!["a", "a"],
        "b and c know nobody, so only a survives — twice: {r:?}"
    );
}

#[test]
fn a_read_only_subquery_must_return_so_the_no_column_case_cannot_reach_here() {
    // Recorded because the first version of this file tested the zero-column
    // case and could not: openCypher requires a subquery to end in RETURN or
    // an update, so a body with NO columns is necessarily a WRITING one — and
    // a writing body never takes the streaming path. The `columns.is_empty()`
    // branch in the streaming join therefore mirrors the interpreter for
    // safety rather than for a shape that can arrive.
    let g = fixture();
    let err = match parse_statement(
        "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(:P) } RETURN p",
    ) {
        Err(e) => format!("{e:?}"),
        Ok(stmt) => match run_query(&g, &stmt, BTreeMap::new()) {
            Err(e) => format!("{e:?}"),
            Ok(r) => panic!("expected a refusal, got {} row(s)", r.rows.len()),
        },
    };
    assert!(
        err.contains("RETURN") || err.contains("conclude"),
        "a subquery ending in MATCH is refused by name: {err}"
    );
}

#[test]
fn a_writing_subquery_stays_on_the_interpreter() {
    // A body that writes keeps the old path deliberately — and must still be
    // correct.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WHERE p.name = 'a' WITH p \
         CALL { WITH p CREATE (n:Made {of: p.name}) RETURN n } \
         RETURN n.of AS made",
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::Str("a".into()));
    let check = rows(&g, "MATCH (n:Made) RETURN count(n) AS c");
    assert_eq!(check[0][0], Value::Int(1), "the write really happened");
}

#[test]
fn a_call_that_is_not_in_the_tail_position_is_left_alone() {
    // A CALL followed by more than the RETURN binds names later clauses plan
    // against; the planner would need the subquery's output columns to seed
    // them. Out of scope, and the answer must still be right.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(f:P) RETURN f } \
         MATCH (f)-[:KNOWS]->(g2:P) RETURN count(*) AS c",
    );
    assert_eq!(r[0][0], Value::Int(0), "nobody b or c knows anyone: {r:?}");
}

#[test]
fn the_streamed_join_matches_the_interpreted_one() {
    // The two paths must produce identical rows. A writing subquery forces the
    // interpreter; the read-only twin takes the streaming path.
    let g = fixture();
    let streamed = rows(
        &g,
        "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(f:P) RETURN f } \
         RETURN p.name AS p, f.name AS f ORDER BY p, f",
    );
    // same shape, forced onto the other path by a trailing clause
    let interpreted = rows(
        &g,
        "MATCH (p:P) WITH p CALL { WITH p MATCH (p)-[:KNOWS]->(f:P) RETURN f } \
         WITH p, f RETURN p.name AS p, f.name AS f ORDER BY p, f",
    );
    assert_eq!(streamed, interpreted, "the two paths must agree");
}

#[test]
fn the_subquery_sees_the_outer_nodes_properties_not_a_lean_stub() {
    // THE BUG THIS NEARLY SHIPPED WITH, and the reason it is dangerous: the
    // demand walk binds a node LEAN, carrying only the properties something
    // in the stage will ask for, and it cannot see inside a subquery. With
    // `CALL` newly streaming and demanding nothing, this query returned the
    // right FOUR rows with the right column name and `y` = NULL in every one,
    // because `n` arrived without `v`.
    //
    // A wrong answer of the correct SHAPE passes any check that asks whether
    // rows came back. Only asserting the values catches it.
    let g = g();
    for v in [10i64, 20] {
        let mut m = BTreeMap::new();
        m.insert("v".to_string(), Value::Int(v));
        g.create_node(&["N".into()], &m).expect("node");
    }
    let r = rows(
        &g,
        "MATCH (n:N) CALL { WITH n RETURN n.v AS y UNION ALL WITH n RETURN n.v * 2 AS y }          RETURN y ORDER BY y",
    );
    let got: Vec<i64> = r
        .iter()
        .map(|x| match &x[0] {
            Value::Int(i) => *i,
            other => panic!("y must be an integer, got {other:?} — the lean-seed bug"),
        })
        .collect();
    assert_eq!(got, vec![10, 20, 20, 40], "{r:?}");
}
