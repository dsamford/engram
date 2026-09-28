//! A both-ends-bound TWO-hop leg is decided by its first-hop degrees, with the
//! whole-path estimate on or off.
//!
//! For two hops the estimate's tail is identically 1 (the only hop past the
//! first reaches the other bound end), so it knows nothing the first-hop
//! comparison does not — but it demanded a 4x margin, and inside that margin
//! it kept the slower end. SNB BI bi8's leg `(tag)<-[:HAS_TAG]-(m)-
//! [:HAS_CREATOR]->(person)` is two hops, evaluated per person and per friend,
//! and paid for it: at SF10, lever off 86.2 / 59.5 s, lever on 104.7 / 77.0 s.
//!
//! The fixture puts the two ends' degrees 2.5x apart — inside the margin, where
//! the two rules used to DISAGREE.

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

#[test]
fn the_estimate_lever_does_not_change_a_two_hop_decision() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // one tag on 30 messages; person 0 wrote 12 messages, 5 of them tagged
    ddl(&g, "CREATE (:Tag {name: 't'}), (:Person {id: 0}), (:Person {id: 1})");
    ddl(
        &g,
        "UNWIND range(0, 36) AS i MATCH (p:Person {id: CASE WHEN i < 12 THEN 0 ELSE 1 END}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: i})",
    );
    ddl(
        &g,
        "MATCH (m:Message), (t:Tag) WHERE m.id < 5 OR m.id >= 12 CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    let q = "MATCH (tag:Tag {name: 't'}), (person:Person {id: 0}) \
             RETURN size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(person) | m]) AS n";

    g.set_path_estimate(false);
    let (off_rows, off) = run(&g, q);
    g.set_path_estimate(true);
    let (on_rows, on) = run(&g, q);
    g.set_path_estimate(false);

    assert_eq!(on_rows, off_rows);
    assert_eq!(on_rows, vec![vec![Value::Int(5)]], "the fixture's answer");
    let reversed = "interp.pattern reversed to drive from the cheaper bound end";
    assert!(get(&off, reversed) > 0, "the fixture must make the first-hop rule reverse");
    assert_eq!(
        get(&on, reversed),
        get(&off, reversed),
        "the lever changed a two-hop decision"
    );
    assert_eq!(
        get(&on, "interp.pattern priced the whole path, not its first hop"),
        0,
        "a two-hop leg was priced by the whole-path estimate"
    );
}
