#![allow(non_snake_case)]
//! THE TEST TWO COMMENTS ALREADY CITE, AND WHICH DID NOT EXIST.
//!
//! `Graph::range_index_caught_up` deletes the filter that kept a label-scoped
//! index from absorbing another label's rows, and says why:
//!
//!   "Keying the log `(label, property)` means the log holds only this label's
//!    rows by construction, so there is nothing to reject."
//!
//! The log is `PropLogs = BTreeMap<u32, ChangeLog<..>>`, keyed by the PROPERTY
//! token alone (`note_prop_change(token, ..)` takes no label). The re-keying the
//! removal rests on is not in the code.
//!
//! Both that comment and `derived_structures.rs` then defer the property to
//! `scoped_index_catch_up.rs` — this file, which was absent. So the isolation
//! property was guarded by nothing at all: a filter removed against a claim
//! about keying, and the claim delegated to a test that was never written.
//!
//! These tests assert on the INDEX, not on an answer. A polluted index can
//! still return the right answer because later filtering rejects the foreign
//! row — which is exactly why an answer-level test would not have caught this.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}
fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}
fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

/// Entries in the `:Person`-scoped index over `id`, after a catch-up.
fn person_id_entries(g: &Graph) -> usize {
    // Touch the index the way a read does, then measure it.
    let _ = run(g, "MATCH (p:Person {id: 1}) RETURN p.id");
    g.range_index_len_for_test("id", Some("Person"))
        .expect("the Person.id index exists once declared and probed")
}

#[test]
fn a_foreign_labels_write_does_not_enter_a_scoped_index() {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    let before = person_id_entries(&g);
    assert_eq!(before, 1, "one Person, one entry");

    // A DIFFERENT label carrying the SAME property name. `balanced` does this
    // constantly — it creates Message and Comment nodes with `id` while reads
    // anchor on Person.id.
    run(&g, "CREATE (:Message {id: 99})");
    let after = person_id_entries(&g);

    assert_eq!(
        after, before,
        "a :Message write grew the :Person-scoped id index from {before} to {after} — \
         the log is keyed by property token alone, so another label's rows are \
         applied to this index without filtering"
    );
}

#[test]
fn a_scoped_index_never_answers_with_a_foreign_labels_node() {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    let _ = person_id_entries(&g);
    run(&g, "CREATE (:Message {id: 99})");

    // The probe the pollution actually corrupts. The full query still answers
    // correctly because a later filter rejects the Message — the index is
    // wrong, the answer is not — so this asserts on the probe.
    let hits = g
        .range_index_probe_for_test("id", Some("Person"), 99)
        .expect("scoped probe");
    assert!(
        hits.is_empty(),
        "the :Person-scoped id index returned {hits:?} for id=99, which is a :Message"
    );
}

#[test]
fn foreign_churn_does_no_catch_up_work_on_this_index() {
    // The cost half. Even where pollution is later filtered out, foreign writes
    // must not drag this index through catch-up work on a reader's thread.
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    let _ = person_id_entries(&g);

    let (_, t) = engram_observe::with_trace(|| {
        for i in 0..64 {
            run(&g, &format!("CREATE (:Message {{id: {}}})", 1_000 + i));
        }
        let _ = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    });
    let caught = t
        .counters()
        .get("graph.range index caught up")
        .copied()
        .unwrap_or(0);
    assert_eq!(
        caught, 0,
        "64 :Message writes caused {caught} catch-up(s) on the :Person.id index; \
         staleness is tested against a property-name clock every :Message write advances"
    );
}
