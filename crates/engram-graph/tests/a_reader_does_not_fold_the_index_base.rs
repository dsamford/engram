#![allow(non_snake_case)]
//! A point seek must not clone an entire index base on the query thread.
//!
//! `RangeIndex::with_changes` folds when `added + removed` passes `FOLD_AT`
//! (4,096), and `folded()` walks and clones the WHOLE base. The catch-up that
//! runs it is `Graph::range_index_caught_up` — called from a reader's point
//! probe, before the requested key is looked up. A new insert enters both
//! bookkeeping sets, so ~2,049 distinct inserts reach the threshold.
//!
//! So a read that returns ONE row can pay O(base) copying, on the query
//! thread, and it gets worse as the base grows — which is the SF10 shape.
//!
//! The fold is correct and must keep happening; what must not happen is a
//! READER doing it. These tests pin where it runs, not whether.

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
fn count_of(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

const FOLDS: &str = "index.overlay folds";

/// A declared Person.id index, built, then `n` more Persons written — enough
/// pending overlay to cross `FOLD_AT` on the next catch-up.
fn corpus_past_the_fold_threshold(n: i64) -> Graph {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    // Build it.
    let _ = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    for i in 0..n {
        run(&g, &format!("CREATE (:Person {{id: {}}})", 1_000 + i));
    }
    g
}

#[test]
fn a_point_seek_does_not_fold_the_base_on_its_own_thread() {
    // 2,050 inserts: each enters `added` and its body enters the removal
    // bookkeeping, so the pair crosses 4,096 on the next catch-up.
    let g = corpus_past_the_fold_threshold(2_050);

    let (_, t) = engram_observe::with_trace(|| {
        // ONE row back. The work must be proportional to that, not to the base.
        let _ = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    });

    assert_eq!(
        count_of(&t, FOLDS),
        0,
        "a point seek returning one row folded the whole index base on the query \
         thread: {:?}",
        t.counters()
    );
}

/// WHERE DOES THE FOLD ACTUALLY HAPPEN through a Cypher path? The index-level
/// tests in engram-store prove the fold is reachable and that its cost grows
/// with the base. This asks the separate question the plan's prioritisation
/// depends on: can a READER reach it, or do the writes fold it first?
#[test]
fn where_the_fold_lands_between_writers_and_readers() {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    let _ = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");

    let (_, tw) = engram_observe::with_trace(|| {
        for i in 0..2_050 {
            run(&g, &format!("CREATE (:Person {{id: {}}})", 1_000 + i));
        }
    });
    let (_, tr) = engram_observe::with_trace(|| {
        let _ = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    });
    println!(
        "FOLDS during 2,050 writes: {} | during the following seek: {}",
        count_of(&tw, FOLDS),
        count_of(&tr, FOLDS)
    );
    println!(
        "writer catch-ups: {}",
        count_of(&tw, "graph.range index caught up")
    );
}

#[test]
fn the_answer_is_unchanged_whether_or_not_the_reader_folds() {
    // The invariant the deferral must not break: base + overlay answers exactly
    // what a folded base answers.
    let g = corpus_past_the_fold_threshold(2_050);
    let got = run(&g, "MATCH (p:Person {id: 1500}) RETURN p.id");
    assert_eq!(got.rows.len(), 1, "the seek must still find its row");

    let missing = run(&g, "MATCH (p:Person {id: 999999}) RETURN p.id");
    assert_eq!(missing.rows.len(), 0, "and must still miss what is absent");

    // Every written id is findable.
    for probe in [1i64, 1_000, 1_999, 2_049] {
        let r = run(&g, &format!("MATCH (p:Person {{id: {probe}}}) RETURN p.id"));
        assert_eq!(r.rows.len(), 1, "id {probe} must be findable");
    }
}
