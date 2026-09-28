#![allow(non_snake_case)]
//! Fix 82: a membership catch-up the label's log covers is NOT metered
//! against the maintenance pass's row budget.
//!
//! The budget meters rows RE-READ from the store, and a catch-up re-reads
//! none — it folds the log's `(id, joined)` entries into the snapshot in
//! memory, O(k log k) with k bounded by the log's cap. It was priced at one
//! row per entry anyway, against the same pool a ~20 µs paged row read draws
//! on, and so was DEFERRED whenever its k exceeded the membership half of the
//! budget. A deferral is meant to be a delay ("the next pass takes it"), but
//! nothing drains a deferred label's log: it grows until it ages past its
//! cap, `covers` goes false, no catch-up is available at any price, and the
//! pass takes its one unbounded REBUILD — a walk of the whole label.
//!
//! That promotion is what the v186 budget curve measured: `members
//! rebuilt=1` on every 5–7 s pass at every budget below 250k rows, and none
//! at 250k where the catch-ups fit. It did not move with the budget because
//! a rebuild costs the label's size, not the pass's.
//!
//! The differential below runs the same write stream against both arms with
//! a budget below one burst's worth of entries. The ON arm catches up every
//! pass and never rebuilds; the OFF arm defers every pass until the log
//! overflows and then rebuilds — the deferral BECOMES the rebuild. Both arms
//! answer the membership correctly throughout (a reader catches up or
//! rebuilds for itself), which is why this is a cost differential and not a
//! correctness one.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::{Graph, RefreshReport};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Members before the snapshot is taken.
const BASE: usize = 2_000;
/// Entries one burst puts in the label's log.
const BURST: usize = 3_000;
/// The pass's row budget: below one burst, so the OFF arm's pricing defers
/// every catch-up (no adjacency table is cached here, so the membership half
/// is the whole budget).
const PASS_ROWS: usize = 2_000;
/// Bursts to run: 24 × 3,000 = 72,000 entries, past `LABEL_LOG_CAP` (65,536)
/// so a log nobody drains overflows inside the run.
const BURSTS: usize = 24;

fn mk(g: &Graph, i: usize) {
    let mut p = BTreeMap::new();
    p.insert("k".to_string(), Value::Int(i as i64));
    g.create_node(&["L".into()], &p).expect("node");
}

fn graph(unmetered: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_refresh_pass_rows(PASS_ROWS);
    g.set_members_unmetered_catch_up(unmetered);
    for i in 0..BASE {
        mk(&g, i);
    }
    // A published snapshot, so there is something to be stale: the first
    // read builds and caches it.
    assert_eq!(
        g.members(Some("L")).expect("members").len(),
        BASE,
        "the base snapshot was not built"
    );
    g
}

fn members(g: &Graph) -> usize {
    g.members(Some("L")).expect("members").len()
}

#[test]
fn a_covered_catch_up_over_the_budget_runs_on_the_default_arm() {
    let g = graph(true);
    for i in BASE..BASE + BURST {
        mk(&g, i);
    }
    let r = g.refresh_stale_derived();
    assert_eq!(
        (r.members_caught_up, r.members_deferred, r.members_rebuilt),
        (1, 0, 0),
        "the covered catch-up was not taken: {r:?}"
    );
    assert_eq!(members(&g), BASE + BURST);
}

#[test]
fn the_same_catch_up_is_deferred_on_the_metered_arm() {
    let g = graph(false);
    for i in BASE..BASE + BURST {
        mk(&g, i);
    }
    let r = g.refresh_stale_derived();
    assert_eq!(
        (r.members_caught_up, r.members_deferred, r.members_rebuilt),
        (0, 1, 0),
        "the metered arm did not defer a catch-up over its budget: {r:?}"
    );
    // The answer is right regardless — a reader catches up for itself.
    assert_eq!(members(&g), BASE + BURST);
}

/// The differential. Same stream, both arms: the metered arm's deferrals
/// become a rebuild once the log overflows; the unmetered arm catches up
/// every pass and never walks the label.
#[test]
fn a_deferred_catch_up_becomes_a_rebuild_and_an_unmetered_one_never_does() {
    let run = |unmetered: bool| -> RefreshReport {
        let g = graph(unmetered);
        let mut total = RefreshReport::default();
        let mut next = BASE;
        for _ in 0..BURSTS {
            for i in next..next + BURST {
                mk(&g, i);
            }
            next += BURST;
            let r = g.refresh_stale_derived();
            total.add(&r);
        }
        assert_eq!(members(&g), BASE + BURSTS * BURST, "unmetered={unmetered}");
        total
    };
    let on = run(true);
    assert_eq!(
        on.members_caught_up, BURSTS,
        "the unmetered arm did not catch up on every pass: {on:?}"
    );
    assert_eq!(on.members_rebuilt, 0, "the unmetered arm rebuilt: {on:?}");
    assert_eq!(on.members_deferred, 0, "the unmetered arm deferred: {on:?}");

    let off = run(false);
    assert_eq!(
        off.members_caught_up, 0,
        "the metered arm caught up, so the budget did not defer it: {off:?}"
    );
    assert!(
        off.members_rebuilt >= 1,
        "the metered arm's deferrals never became a rebuild — the log did not overflow, \
         or a rebuild is no longer the fallback: {off:?}"
    );
    assert!(
        off.members_deferred >= BURSTS / 2,
        "the metered arm deferred fewer passes than the budget implies: {off:?}"
    );
}
