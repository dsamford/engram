#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The maintenance pass's row budget bounds what it does ON TOP of one
//! unavoidable item — it must never make an item undoable.
//!
//! # The defect
//!
//! `refresh_stale_derived` prices each stale table's repair and defers any that
//! costs more than the budget it has left, on the stated ground that "what this
//! pass defers, the next one takes: the tick and the write-count trigger both
//! fire again, and a deferred table is still stale, so it is still a
//! candidate. Deferring is therefore a delay, never a drop."
//!
//! That holds only while a LATER pass can afford what this one skipped. A table
//! whose repair alone exceeds the whole budget breaks it: every future pass
//! declines it for the same reason, and its delta only grows, so the delay is
//! permanent. The table then comes back only when the change log finally
//! overflows and some reader rebuilds it — `ADJ_LOG_CAP` (262,144) entries
//! later, with every read walking the span meanwhile.
//!
//! # Why it was not reachable before
//!
//! Readers repaired on every stale read, which kept each table's delta small
//! for the pass as a side effect nobody had named. §8 stopped that — a
//! single-node reader whose node did not move repairs nothing — so the pass now
//! has to be able to finish the job it was already nominally responsible for.
//! `review_repair_over_cap_differential` is what surfaced the interaction.
//!
//! # What this file pins
//!
//! A budget SMALLER than a single table's repair. The pass must still repair
//! it, and must say so with `graph.derived refresh took a repair over its whole
//! budget` — and the canary is the same fixture with a budget large enough that
//! the over-budget path is NOT taken, so the assertion is about the branch and
//! not about the fixture always being over budget.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const NODES: u64 = 3_000;

/// A warmed table, then a burst big enough that its repair costs far more than
/// the tiny budget the over-budget arm sets.
///
/// `set_single_node_stale_walk(false)` is deliberate and is the point of the
/// fixture rather than an accommodation: this file is about what the PASS does
/// with a table it finds stale, so the reads that warm it must not themselves
/// be the thing that repairs it.
fn stale_table_with_a_burst(g: &Graph) -> u32 {
    g.set_degree_table_after(0);
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0xB0DE_1234_5678_9ABC);
    for &src in &ids {
        g.create_rel(src, "T", ids[(rng.next() % NODES) as usize], &none)
            .expect("rel");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];
    // Warm, so the pass has a published slot to find stale.
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    // Now move enough rows that a repair is expensive.
    for &src in &ids[..1_000] {
        g.create_rel(src, "T", ids[(rng.next() % NODES) as usize], &none)
            .expect("burst");
    }
    tok
}

/// THE CLAIM: a budget below one table's repair cost does not stop the pass.
#[test]
fn a_repair_larger_than_the_whole_budget_is_still_taken() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let _tok = stale_table_with_a_burst(&g);
    // Far below `1,000 + 1,000 * ADJ_REPAIR_SCAN_ROWS`.
    g.set_refresh_pass_rows(64);

    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    eprintln!("[budget] tiny budget: {report:?} {:?}", trace.counters());

    assert!(
        report.adjacency_repaired >= 1,
        "a table whose repair exceeds the whole budget must still be repaired — \
         every later pass would decline it for the same reason and its delta \
         only grows, so deferring it is permanent: {report:?}"
    );
    assert!(
        count(&trace, "graph.derived refresh took a repair over its whole budget") >= 1,
        "and it must SAY it went over budget, or this passed for some other \
         reason: {:?}",
        trace.counters()
    );
}

/// THE CANARY: with a budget the repair fits inside, the over-budget path is
/// not taken — so the counter above is a statement about the branch, not about
/// a fixture that is over budget no matter what.
#[test]
fn a_repair_inside_the_budget_does_not_take_the_over_budget_path() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let _tok = stale_table_with_a_burst(&g);
    g.set_refresh_pass_rows(1_000_000);

    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    eprintln!("[budget] ample budget: {report:?}");

    assert!(
        report.adjacency_repaired >= 1,
        "the same fixture must repair on the ample arm too, or the two arms \
         are not comparable: {report:?}"
    );
    assert_eq!(
        count(&trace, "graph.derived refresh took a repair over its whole budget"),
        0,
        "a repair that fits must not report going over: {:?}",
        trace.counters()
    );
}

/// The budget still BOUNDS a pass: past the one item it cannot defer, further
/// oversized tables wait for the next pass.
///
/// Without this the fix above reads as "the budget does nothing", which is a
/// different and much worse change than the one that was made.
#[test]
fn the_budget_still_defers_the_second_oversized_table() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x1357_9BDF_0246_8ACE);
    // TWO relationship types, so two tables go stale together.
    for &src in &ids {
        for ty in ["T", "U"] {
            g.create_rel(src, ty, ids[(rng.next() % NODES) as usize], &none)
                .expect("rel");
        }
    }
    g.shared_store().seal();
    for ty in ["T", "U"] {
        let tok = g.type_tokens_peek(&[ty.to_string()]).expect("minted")[0];
        let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    }
    for &src in &ids[..1_000] {
        for ty in ["T", "U"] {
            g.create_rel(src, ty, ids[(rng.next() % NODES) as usize], &none)
                .expect("burst");
        }
    }

    g.set_refresh_pass_rows(64);
    let report = g.refresh_stale_derived();
    eprintln!("[budget] two oversized tables, tiny budget: {report:?}");
    assert!(
        report.adjacency_repaired >= 1,
        "one oversized table must be taken: {report:?}"
    );
    assert!(
        report.adjacency_deferred >= 1,
        "and the rest must still be DEFERRED — the budget bounds a pass, it is \
         not switched off by the first item that exceeds it: {report:?}"
    );
}
