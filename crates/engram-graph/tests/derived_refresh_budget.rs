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
//!
//! # Claim 1 now belongs to the OFF arm — and that is the finding
//!
//! Claim 1's "repairs one and DEFERS the rest" describes a budget that is
//! RACED FOR: the pass takes the first stale table it prices and defers every
//! later one. Deferral looked like the budget working. It was also the thing
//! that starved those tables — the map's iteration order does not change
//! between passes, so the same table was taken and the same tables deferred
//! every time, and their deltas grew until one was taken whole (109 refreshes
//! totalling 82,748 ms in a 400 s sweep, the longest 9,935 ms, every one
//! reporting `adjacency repaired=1 adjacency deferred=2`).
//!
//! Under `set_bounded_derived_repair` (default ON) the budget is SHARED
//! max-min across every stale table and each repair is bounded to its slice,
//! so the adjacency half defers nothing and every table drains a little every
//! pass. Claim 1's test therefore sets the lever OFF, to keep pinning the arm
//! it was written for, and `a_shared_budget_defers_nothing_and_still_converges`
//! below states the ON arm's invariant beside it. Claims 2 and 3 are unchanged
//! and hold on both arms — which is the real point: the two arms differ in WHO
//! does the work WHEN, and never in the answer.

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
    // THE FIRST-COME ARM. Deferral is what a raced-for budget does, and it is
    // the behaviour this claim was written about — see the module doc for why
    // it is no longer the default.
    g.set_bounded_derived_repair(false);
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
    assert!(
        passes < 50,
        "the budget never converged after {passes} passes"
    );
    let last = g.refresh_stale_derived();
    assert_eq!(
        last.adjacency_deferred, 0,
        "a settled graph defers nothing: {last:?}"
    );
}

/// The ON arm's invariant, stated beside the OFF arm's so the file says what
/// the budget does under BOTH rules on the same fixture.
///
/// Six stale tables, a budget under any one of their repairs. The shared rule
/// defers NOTHING — every table gets a slice — and still converges, because a
/// slice that cannot finish leaves the table stale for the next pass rather
/// than dropping it. That is the same "delay, never a drop" guarantee claim 1
/// makes, reached without starving anybody.
#[test]
fn a_shared_budget_defers_nothing_and_still_converges() {
    let (g, _ids, _toks) = staled_graph();
    g.set_refresh_pass_rows(BURST as usize * 40);

    let first = g.refresh_stale_derived();
    eprintln!("[shared] first pass: {first:?}");
    assert_eq!(
        first.adjacency_deferred, 0,
        "the shared budget serves every stale table, so nothing is deferred by \
         it — deferral is the first-come rule's behaviour, and it is what \
         starved the tables it skipped: {first:?}"
    );
    assert!(
        first.adjacency_repaired > 0,
        "and it still makes progress: {first:?}"
    );

    let mut passes = 1;
    let mut report = first;
    while (report.adjacency_repaired > 0 || report.adjacency_deferred > 0) && passes < 50 {
        report = g.refresh_stale_derived();
        passes += 1;
    }
    assert!(passes < 50, "the shared budget never converged: {report:?}");
    let last = g.refresh_stale_derived();
    assert_eq!(
        (last.adjacency_repaired, last.adjacency_deferred),
        (0, 0),
        "a settled graph needs nothing: {last:?}"
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
        .map(|&t| {
            gref.adjacent_slim(idsref[0], Dir::Out, &Some(vec![t]))
                .len()
        })
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
    assert_eq!(
        got, want,
        "a budgeted refresh must answer what an unbudgeted one does"
    );
}
