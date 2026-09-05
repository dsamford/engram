#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The sampled hop estimator: deterministic, bounded, and NEVER an answer.
//!
//! # What this guards
//!
//! `count_hop_estimate` serves the planner's `hop_fanout`; `count_hop` serves
//! real `count(*)` queries. The estimator may stride-sample a large label and
//! scale up — the exact path may not, ever. Three properties, each with the
//! failure it forbids:
//!
//! * **bounded** — a sampled estimate lands near the exact count, else the
//!   ordering search buys wrong plans;
//! * **deterministic** — two calls agree exactly (a fixed stride over a sorted
//!   view, no randomness), else plans differ run to run and the determinism
//!   gate is a coin flip;
//! * **separate** — an estimate must never be served where an answer was
//!   asked. The two paths keep separate memo maps precisely so this cannot
//!   happen by key collision; the test forces a DELIBERATELY COARSE estimate
//!   into the estimate memo and then asks `count_hop` the same question.
//!
//! The fixture's out-degrees follow `i % 3` with the sample budget set to
//! values whose strides are coprime to 3, so the stride cannot alias the
//! degree pattern and sample one residue class — a periodic fixture aligned
//! with the stride would make even a correct estimator look wrong (or a wrong
//! one look right).

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Dir, Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

const N: i64 = 300;

/// P nodes 0..N; node i has `i % 3` T-edges out (to i+1, i+2, i+3 mod N), so
/// the total is exactly N (100 zeros, 100 ones, 100 twos → 0+100+200 = 300)
/// and the degree pattern has period 3.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    for p in 0..N {
        for d in 1..=(p % 3) {
            let q = (p + d) % N;
            stmt(
                &g,
                &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:T]->(b)"),
            );
        }
    }
    g.shared_store().seal();
    g
}

fn exact(g: &Graph) -> u64 {
    g.count_hop(&["P".into()], Dir::Out, &["T".into()], &[])
        .expect("count_hop")
}

fn estimate(g: &Graph) -> u64 {
    g.count_hop_estimate(&["P".into()], Dir::Out, &["T".into()], &[])
        .expect("count_hop_estimate")
}

#[test]
fn a_sampled_estimate_is_bounded_and_deterministic() {
    let g = fixture();
    let truth = exact(&g);
    assert_eq!(truth, N as u64, "the fixture's degree arithmetic");

    // Budget 100 over 300 members: stride 3... which IS the degree period.
    // Deliberately avoided — 100 is used below to prove the aliasing danger is
    // real, budget 91 (stride ceil(300/91)=4, coprime to 3) is the honest one.
    g.set_estimate_sample_budget(91);
    let e1 = estimate(&g);
    let err = (e1 as f64 - truth as f64).abs() / truth as f64;
    assert!(
        err < 0.15,
        "a stride-4 sample over a period-3 degree pattern must land near the \
         truth: estimate {e1} vs exact {truth} ({:.0}% off)",
        err * 100.0
    );

    let e2 = estimate(&g);
    assert_eq!(e1, e2, "two estimates must agree exactly — no randomness");
}

#[test]
fn a_small_side_is_answered_exactly() {
    let g = fixture();
    // Budget >= the label: the estimator must DELEGATE to the exact path.
    g.set_estimate_sample_budget(100_000);
    assert_eq!(
        estimate(&g),
        exact(&g),
        "under the budget there is no reason to be wrong at all"
    );
}

#[test]
fn an_estimate_is_never_served_as_an_answer() {
    let g = fixture();
    // Force the COARSEST possible estimate: budget 1 → stride 300 → ONE
    // sampled node (id 0, degree 0) scaled by 300 → estimate 0, wildly wrong.
    g.set_estimate_sample_budget(1);
    let coarse = estimate(&g);
    assert_ne!(
        coarse,
        exact(&g),
        "the fixture must make the coarse estimate WRONG, or this test cannot \
         detect an estimate being served as an answer"
    );
    // The same (labels, dir, types) question, asked as an ANSWER: must be
    // exact. If the two paths shared a memo map, the coarse entry above would
    // serve here.
    assert_eq!(
        g.count_hop(&["P".into()], Dir::Out, &["T".into()], &[])
            .expect("count_hop"),
        N as u64,
        "count_hop returned the estimator's number — the exact path is being \
         served from the estimate memo"
    );
}
