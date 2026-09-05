#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! `REMOVE n:Label` must not evict a live node from the PARTITION-WIDE index.
//!
//! # The defect this pins
//!
//! When property change logs were re-keyed `(label, property)`, `remove_labels`
//! gained a call to `note_prop_change` recording `None` — "this row leaves the
//! index" — for each removed label. But `note_prop_change` writes to
//! `ALL_LABELS` *in addition to* every label it is given, and `ALL_LABELS` is
//! the log the partition-wide index reads. A scope retraction was therefore
//! routed through the value path, and `RangeIndex::with_changes` put the body
//! in `removed` and never re-added it.
//!
//! The node stays alive and keeps its property; only the equality SEEK loses
//! it. So `MATCH (p:Person {id: 7})` returns nothing while
//! `MATCH (p:Person) WHERE p.id > 6 AND p.id < 9` returns it — two forms of one
//! predicate disagreeing.
//!
//! # Why the whole suite stayed green
//!
//! Corpus size. Below the planner's seek threshold every one of these queries
//! SCANS, and a scan never consults the index. Every existing `REMOVE n:L` test
//! uses a small fixture, so all 1,161 tests, the TCK ratchet and the
//! determinism digest passed while the database answered wrong — determinism
//! included, because both of its runs were wrong identically.
//!
//! 2,000 nodes is the point of this fixture, not incidental to it.
use std::collections::BTreeMap;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> usize {
    let q = engram_cypher::parse_statement(src).expect("parse");
    engram_graph::run_query(g, &q, BTreeMap::new()).expect("run").rows.len()
}

#[test]
fn remove_label_must_not_evict_from_the_partition_wide_index() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Big enough that the planner SEEKS rather than scans.
    for i in 0..2000i64 {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), engram_cypher::Value::Int(i));
        g.create_node(&["Person".to_string(), "Employee".to_string()], &p)
            .expect("node");
    }
    g.shared_store().seal();
    assert_eq!(run(&g, "MATCH (p:Person {id: 7}) RETURN p.id"), 1, "warm: id=7 seeks");

    run(&g, "MATCH (p:Person {id: 7}) REMOVE p:Employee");

    let still_there = run(&g, "MATCH (p:Person) WHERE p.id > 6 AND p.id < 9 RETURN p.id");
    let by_seek = run(&g, "MATCH (p:Person {id: 7}) RETURN p.id");
    let control = run(&g, "MATCH (p:Person {id: 8}) RETURN p.id");
    eprintln!("[verify] range->{still_there} rows, seek(id=7)->{by_seek}, seek(id=8)->{control}");
    assert_eq!(control, 1, "control id=8 must still seek");
    assert_eq!(
        by_seek, 1,
        "REMOVE p:Employee evicted a LIVE :Person from the partition-wide id \
         index — the range form still finds it ({still_there} rows) but the \
         equality seek does not. Two forms of one predicate disagree."
    );
}
