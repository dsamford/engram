//! A hop end the row already binds is decided by its ID, and — when the
//! pattern tests nothing further on it — never read again.
//!
//! SNB BI bi15's interaction leg `(pA)<-[:HAS_CREATOR]-(m1)-[:REPLY_OF]-(m2)
//! -[:HAS_CREATOR]->(pB)` has both ends bound, and `pB` is a bare grouping key
//! (`WITH pA, pB, count(m1)`), so it is demanded in full. The matcher read the
//! end in full BEFORE checking it was the node the row already held: 26,591
//! full reads at SF0.1 — exactly its interaction count.
//!
//! DIFFERENTIAL: the bare end `(pB)` takes the new path; `(pB:Person)` tests a
//! label and takes the old one. Both must answer identically.

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

/// 200 people who each KNOW the next four; 5 messages each, every third one a
/// reply to a message two people along. One row per edge in every setup
/// statement — never a cartesian in a setup write.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 199) AS i CREATE (:Person {id: i, bio: 'a long biography'})");
    ddl(
        &g,
        "UNWIND range(0, 199) AS i UNWIND range(1, 4) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 200}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS m MATCH (p:Person {id: m % 200}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m, content: 'some text'})",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS i WITH i WHERE i % 3 = 0 \
         MATCH (a:Message {id: i}), (b:Message {id: (i + 2) % 1000}) \
         CREATE (a)-[:REPLY_OF]->(b)",
    );
    let _ = g.warm();
    g
}

fn interactions(end: &str) -> String {
    format!(
        "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
         OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->{end} \
         WITH pA, pB, count(m1) AS i \
         RETURN pA.id AS a, pB.id AS b, i ORDER BY a, b"
    )
}

#[test]
fn a_bound_end_answers_the_same_without_being_read_again() {
    let g = social();
    let (bare, c_bare) = run(&g, &interactions("(pB)"));
    let (labelled, c_labelled) = run(&g, &interactions("(pB:Person)"));
    assert_eq!(bare, labelled, "reusing the row's binding changed the answer");
    assert!(
        bare.iter().any(|r| matches!(r.get(2), Some(Value::Int(n)) if *n > 0)),
        "no pair interacts; this compares nothing"
    );
    let reused = get(&c_bare, "interp.hop end reused the row's own binding")
        + get(&c_bare, "interp.hop end refused by the row's own binding before any read");
    assert!(reused > 0, "the bare end never took the new path");
    // The bare end reads NO node in full. The labelled one used to be the
    // contrast — it was re-read in full per interaction, because
    // `id(pA) < id(pB)` demanded both people in full — but `id(v)` now
    // demands identity alone, so it reads none either; the difference that
    // remains between them is the reuse counted above.
    let full = "graph.nodes materialised in full";
    assert_eq!(
        get(&c_bare, full),
        0,
        "the bare end was read in full: {c_bare:?}"
    );
    assert!(
        get(&c_bare, full) <= get(&c_labelled, full),
        "the bare end read MORE nodes in full than the labelled one: {} vs {}",
        get(&c_bare, full),
        get(&c_labelled, full)
    );
}

/// A bound end tested for a label, or an inline property, still takes the old
/// path — the row's binding may not carry what the test reads.
#[test]
fn a_bound_end_the_pattern_tests_is_still_tested() {
    let g = social();
    for end in ["(pB:Person)", "(pB {bio: 'a long biography'})", "(pB {bio: 'nothing'})"] {
        let (_, c) = run(&g, &interactions(end));
        assert_eq!(
            get(&c, "interp.hop end reused the row's own binding"),
            0,
            "`{end}` tests the end, so its binding must not be reused untested"
        );
    }
    // and the test still bites: a map no person matches leaves every count 0
    let (rows, _) = run(&g, &interactions("(pB {bio: 'nothing'})"));
    assert!(
        rows.iter().all(|r| r.get(2) == Some(&Value::Int(0))),
        "an inline map no end satisfies still matched"
    );
}
