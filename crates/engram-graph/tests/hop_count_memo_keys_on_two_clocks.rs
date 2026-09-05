#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The hop-count memo must be valid under TWO clocks — the types' adjacency
//! epoch AND the labels' membership epochs.
//!
//! # What this guards
//!
//! `count_hop` answers `(:start)-[:types]->(:end)` for the planner, and a
//! labelled answer WALKS the smaller label — 2M nodes for `(:Comment)` at SF1.
//! The planner asks the same handful of questions on every plan build; LSQB
//! q2's two-path pattern asked sixteen across the three builds of one
//! statement, ~1 s per execution, misread for a day as ~1,000 ns per fold leaf
//! until the event trace named it. The memo makes the walk once per epoch.
//!
//! Its validity needs BOTH clocks because the answer depends on both worlds:
//! a relationship write moves the adjacency epoch, and a node gaining or losing
//! a label moves membership with — in general — no relationship write at all.
//! An entry keyed on the adjacency epoch alone survives exactly that, and
//! serves a count over a membership that no longer exists. That is
//! `derived.rs`'s defect class #1, validity keyed on the wrong clock, in its
//! subtlest form: the wrong clock is not stale, it just does not SEE the
//! change.
//!
//! # Proven to bite
//!
//! Each clock was removed from the validity check in turn:
//! * ignore the adjacency epoch — `a_relationship_write_is_seen` fails with the
//!   stale count served;
//! * ignore the label epoch — `a_membership_change_alone_invalidates` fails
//!   with `served == 1` where the entry must have been declined.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Dir, Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SERVED: &str = "graph.hop count served from the memo";

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn served(t: &engram_observe::Trace) -> u64 {
    t.counters().get(SERVED).copied().unwrap_or(0)
}

fn hop(g: &Graph, start: &str, dir: Dir, ty: &str, end: &[&str]) -> u64 {
    let end: Vec<String> = end.iter().map(|s| s.to_string()).collect();
    g.count_hop(&[start.to_string()], dir, &[ty.to_string()], &end)
        .expect("count_hop")
}

const N: i64 = 12;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    for p in 0..N {
        let q = (p + 1) % N;
        stmt(
            &g,
            &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:T]->(b)"),
        );
    }
    g.shared_store().seal();
    g
}

#[test]
fn a_repeat_is_served_from_the_memo() {
    let g = fixture();
    let first = hop(&g, "P", Dir::Out, "T", &[]);
    assert_eq!(first, N as u64, "one T out of every P");
    let (again, trace) = engram_observe::with_trace(|| hop(&g, "P", Dir::Out, "T", &[]));
    assert_eq!(again, first, "the memo must not change the answer");
    assert_eq!(
        served(&trace),
        1,
        "the identical question, both clocks unmoved, must be served"
    );
}

#[test]
fn a_relationship_write_is_seen() {
    let g = fixture();
    let before = hop(&g, "P", Dir::Out, "T", &[]);
    stmt(
        &g,
        "MATCH (a:P {id: 0}), (b:P {id: 5}) CREATE (a)-[:T]->(b)",
    );
    let after = hop(&g, "P", Dir::Out, "T", &[]);
    assert_eq!(
        after,
        before + 1,
        "the T edge just written must be counted — a memo that ignores the \
         adjacency epoch serves {before}"
    );
}

#[test]
fn a_membership_change_alone_invalidates() {
    let g = fixture();
    // Memoise a question about a label with NO members: the answer is 0 and
    // the label's epoch is 0 (never minted).
    let first = hop(&g, "Q", Dir::Out, "T", &[]);
    assert_eq!(first, 0, "no Q exists yet");

    // An ORPHAN labelled create: membership moves, adjacency does not — the
    // exact write an adjacency-epoch-only memo cannot see. The ANSWER happens
    // to stay 0 (the new Q has no edges), so the assertion is on the memo's
    // behaviour: the entry must be DECLINED and recomputed, not served.
    stmt(&g, "CREATE (:Q {id: 99})");
    let (again, trace) = engram_observe::with_trace(|| hop(&g, "Q", Dir::Out, "T", &[]));
    assert_eq!(again, 0, "the orphan has no T edges");
    assert_eq!(
        served(&trace),
        0,
        "membership moved with no relationship write; the memoised entry must \
         MISS — serving here means validity is keyed on the adjacency clock \
         alone"
    );

    // And after the recompute, the refreshed entry serves again.
    let (_, trace) = engram_observe::with_trace(|| hop(&g, "Q", Dir::Out, "T", &[]));
    assert_eq!(served(&trace), 1, "the refreshed entry serves");
}

#[test]
fn the_lever_changes_cost_and_not_answers() {
    let g = fixture();
    g.set_hop_count_memo(true);
    let (on, on_trace) = engram_observe::with_trace(|| {
        (hop(&g, "P", Dir::Out, "T", &[]), hop(&g, "P", Dir::Out, "T", &[]))
    });
    g.set_hop_count_memo(false);
    let (off, off_trace) = engram_observe::with_trace(|| {
        (hop(&g, "P", Dir::Out, "T", &[]), hop(&g, "P", Dir::Out, "T", &[]))
    });
    g.set_hop_count_memo(true);
    assert_eq!(on, off, "the memo must be invisible in the answers");
    assert!(served(&on_trace) >= 1, "the ON arm's repeat must serve");
    assert_eq!(
        served(&off_trace),
        0,
        "the OFF arm must never serve — that is what makes it the control"
    );
}
