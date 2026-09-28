#![allow(non_snake_case)]
//! MEASURE the OCC read set, rather than probing it behaviourally.
//!
//! A conflict probe ("bind a node bare, move it from another thread, require
//! the commit to abort") cannot isolate one binding's contribution: any other
//! read of the same entity aborts the commit just as well, so the probe passes
//! whether or not the bare bind recorded anything. One written for exactly
//! this question passed with every recording call REMOVED — a canary wired to
//! nothing.
//!
//! A count can tell them apart. This records the read-set size of a writing
//! statement so a change that narrows it is visible as a number.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn graph(n: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i CREATE (:P {{id: i, keep: i}})",
            n - 1
        ),
    );
    ddl(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:P {{id: i}}), (b:P {{id: i + 1}}) \
             CREATE (a)-[:T {{w: 1.0}}]->(b)",
            n - 2
        ),
    );
    g
}

/// The read set of a writing statement, as it stands today.
///
/// This is a BASELINE, not a target: its value is that any change letting a
/// write bind leanly must be compared against it. A drop means conflict
/// detection narrowed, which is a decision to take deliberately — see
/// `Graph::set_read_set_bindings_only`, which gates exactly that narrowing OFF
/// by choice and says MERGE must keep full recording regardless.
#[test]
fn a_writing_statements_read_set_is_recorded_so_a_narrowing_is_visible() {
    let g = graph(200);
    g.begin_txn().expect("begin");
    run(
        &g,
        "MATCH (a:P)-[r:T]->(b:P) WHERE a.id < 50 SET r.w = 39.0 RETURN count(*) AS n",
    );
    let len = g.read_set_len_for_test();
    g.commit_txn().expect("uncontended commit");

    eprintln!("[read set] writing statement over 50 edges of 200 nodes: {len} entries");
    // 400 is the size this statement's read set had BEFORE writes could bind
    // leanly, and it is the number that made the difference measurable:
    //
    //   baseline ......................... 400
    //   lean binds, nothing recorded ..... 251   <- narrowed by 149
    //   lean binds + `note_*_read` ....... 400   <- identical again
    //
    // which shows the recording is NECESSARY (251 != 400) and SUFFICIENT
    // (400 == 400). A behavioural probe could show neither: one written for
    // this exact question passed with every recording call REMOVED, because
    // any other read of the same entity aborts the commit just as well.
    //
    // A DROP here means conflict detection has narrowed. That may be wanted --
    // `Graph::set_read_set_bindings_only` gates exactly that narrowing and
    // says when it is sound -- but it is a decision to take deliberately, and
    // MERGE keeps full recording regardless.
    assert_eq!(
        len, 400,
        "the writing statement's read set changed size; a DROP means a lean \
         bind stopped recording a binding and conflict detection narrowed"
    );
}

/// The control: a READ-ONLY statement in a transaction also records, and that
/// is free — "a transaction that never writes never validates".
#[test]
fn a_read_only_statement_also_records_but_never_validates() {
    let g = graph(200);
    g.begin_txn().expect("begin");
    run(
        &g,
        "MATCH (a:P)-[r:T]->(b:P) WHERE a.id < 50 RETURN count(*) AS n",
    );
    let len = g.read_set_len_for_test();
    g.commit_txn().expect("a read-only commit cannot conflict");
    eprintln!("[read set] read-only statement, same shape: {len} entries");
}
