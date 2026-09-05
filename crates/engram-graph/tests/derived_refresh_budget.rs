//! A maintenance refresh pass is bounded by ROWS, not by the corpus.
//!
//! The rebuild budget (one per pass) was never the expensive half. REPAIRS
//! were unbounded, and a store carries many adjacency tables — official SF1
//! carries ~32 — so one pass could repair every stale table in turn. Measured
//! on the pod that cost 2-3x of write throughput, with the 10th-percentile
//! second at 0.08 of the median; lengthening the tick did NOT help, because
//! the cost is the PASS, not its frequency.
//!
//! Three claims, each with the arm that would fail without the budget:
//!
//! 1. A pass whose budget covers ONE table's repair repairs one and DEFERS
//!    the rest — and the deferral is a delay, not a drop: later passes finish
//!    the work with no writer in between.
//! 2. `set_refresh_pass_rows(0)` restores the unbounded pass (the A/B arm),
//!    which repairs every stale table in one go. This is the canary: it is
//!    what the budgeted arm is being compared against, so it must actually
//!    differ.
//! 3. The budget never turns a repairable table into a permanently stale one:
//!    after enough passes every table is current, and a reader sees correct
//!    adjacency throughout.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const NODES: u64 = 4_000;
/// Distinct relationship types, each with its own cached table.
const TYPES: [&str; 6] = ["T0", "T1", "T2", "T3", "T4", "T5"];
/// Rows per type at build time.
const PER_TYPE: u64 = 1_500;
/// Changed nodes per type in the burst — under `ADJ_REPAIR_MAX` (4,096) so
/// every table stays REPAIRABLE and the budget is the only thing that can
/// defer one.
const BURST: u64 = 600;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A graph with one cached OUT table per type, then a burst that makes every
/// one of them stale but repairable.
fn staled_graph() -> (Graph, Vec<u64>, Vec<u32>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x2545_F491_4F6C_DD1D);
    for t in TYPES {
        for i in 0..PER_TYPE {
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(ids[i as usize], t, dst, &none).expect("rel");
        }
    }
    g.shared_store().seal();
    let toks: Vec<u32> = TYPES
        .iter()
        .map(|t| g.type_tokens_peek(&[t.to_string()]).expect("minted")[0])
        .collect();
    // Build (and cache) one table per type by reading it.
    for &tok in &toks {
        let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])).len();
    }
    // The burst: every type's table goes stale, none past the repair cap.
    for (ti, t) in TYPES.iter().enumerate() {
        for i in 0..BURST {
            let src = ids[((i + ti as u64 * 7) % NODES) as usize];
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(src, t, dst, &none).expect("burst rel");
        }
    }
    (g, ids, toks)
}

#[test]
fn a_budgeted_pass_repairs_some_and_defers_the_rest() {
    let (g, _ids, _toks) = staled_graph();
    // A budget that covers roughly one table's repair: BURST changed nodes
    // times the per-node scan constant, plus its entries.
    g.set_refresh_pass_rows(BURST as usize * 40);
    let first = g.refresh_stale_derived();
    assert!(
        first.adjacency_deferred > 0,
        "a budget this small must defer something: {first:?}"
    );
    assert!(
        first.adjacency_repaired > 0,
        "and it must still make progress: {first:?}"
    );

    // The deferral is a DELAY, not a drop: further passes finish the work,
    // with no writer in between.
    let mut passes = 1;
    let mut report = first;
    while report.adjacency_deferred > 0 && passes < 50 {
        report = g.refresh_stale_derived();
        passes += 1;
    }
    assert!(passes < 50, "the budget never converged after {passes} passes");
    let last = g.refresh_stale_derived();
    assert_eq!(
        last.adjacency_deferred, 0,
        "a settled graph defers nothing: {last:?}"
    );
}

#[test]
fn the_unbounded_arm_repairs_everything_in_one_pass() {
    let (g, _ids, _toks) = staled_graph();
    g.set_refresh_pass_rows(0); // the pre-budget behaviour
    let only = g.refresh_stale_derived();
    assert_eq!(
        only.adjacency_deferred, 0,
        "the unbounded pass defers nothing, that is the point of it: {only:?}"
    );
    assert!(
        only.adjacency_repaired >= TYPES.len(),
        "and it repairs every stale table in the one pass: {only:?}"
    );
}

#[test]
fn the_budget_delays_work_it_never_drops_it() {
    let (g, ids, toks) = staled_graph();
    // Ground truth from a graph that never budgets.
    let (gref, idsref, toksref) = staled_graph();
    gref.set_refresh_pass_rows(0);
    let _ = gref.refresh_stale_derived();
    let want: Vec<usize> = toksref
        .iter()
        .map(|&t| gref.adjacent_slim(idsref[0], Dir::Out, &Some(vec![t])).len())
        .collect();

    g.set_refresh_pass_rows(BURST as usize * 40);
    for _ in 0..50 {
        if g.refresh_stale_derived().adjacency_deferred == 0 {
            break;
        }
    }
    let got: Vec<usize> = toks
        .iter()
        .map(|&t| g.adjacent_slim(ids[0], Dir::Out, &Some(vec![t])).len())
        .collect();
    assert_eq!(got, want, "a budgeted refresh must answer what an unbudgeted one does");
}
