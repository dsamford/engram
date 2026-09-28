#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! One per pass bounds the COUNT of oversized repairs and says nothing about
//! their COST — and the cost is what a client feels.
//!
//! # The defect
//!
//! `refresh_stale_derived` is row-budgeted, and has one escape hatch: a table
//! whose repair alone exceeds the WHOLE budget is taken anyway, because
//! deferring it would defer it for ever and its delta would only grow (that is
//! `refresh_budget_conserves_work`, and it is right). What the hatch did not
//! do is bound the item it took. `adj_table_snapshot_reporting` repairs
//! all-or-nothing to the CURRENT epoch, so there was no partial catch-up to
//! fall back to and the pass took the whole delta however long it was.
//!
//! Measured on the SF1 stress sweep: **109 derived refreshes totalling
//! 82,748 ms** in a 400 s run, 55 of them over 500 ms and the longest
//! 9,935 ms, every one reporting `adjacency repaired=1`. The per-second
//! throughput series shows the matching hole — `write-only @ 1` runs at
//! ~1,600 ops/s for nineteen seconds and at exactly 0 for the twentieth. The
//! levels that failed the sweep's intra-level stall detector moved between
//! runs because the pass fires at a different second each time, not because
//! the machine differed; that is what made it read as environmental noise for
//! three runs.
//!
//! # The fix, and why a PARTIAL repair is sound
//!
//! The oversized item is truncated to what the budget has left. A repair
//! carries the table forward over the nodes named by log entries stamped at or
//! below a cut, and publishes it AT THAT CUT — so the table is honestly
//! current at an earlier stamp rather than dishonestly current at a later one.
//! Nothing downstream can tell that apart from a fenced publish, which the
//! write fence already produces on every busy slot: `snap.at >= epoch` is the
//! entire currency test, and a reader that wants more repairs the rest itself.
//!
//! The cut is taken at a STAMP boundary, never mid-stamp, and the first stamp
//! is always admitted whatever it costs — so every pass still makes progress
//! and the escape hatch's guarantee is untouched.
//!
//! # The other half: the budget was RACED FOR, so it starved
//!
//! Truncation alone would not have produced those numbers, and asking where a
//! 262,144-entry delta comes from when the pass fires every 8,192 commits is
//! what found the rest. The old rule takes the first stale table it prices and
//! defers every later one for the remainder of the pass. The map's iteration
//! order does not change between passes, so it is the same table taken and the
//! same tables deferred, every time — their deltas never drain, they grow, and
//! one of them is eventually taken whole. `adjacency repaired=1 adjacency
//! deferred=2` on all 109 refreshes is that rule, printed.
//!
//! So each stale table now gets an equal SLICE of the pass (adjacency takes
//! half the budget; the membership family, which has no truncation of its own,
//! keeps the other half). Every table drains a little, every pass. The
//! guarantee is progress everywhere rather than currency anywhere.
//!
//! # What this file pins
//!
//! 1. A bounded pass does NOT bring an oversized table current — it takes a
//!    slice and leaves the rest — while the unbounded arm finishes in one.
//! 2. **No stale table is skipped.** Two oversized tables and a budget under
//!    either one: the old rule repairs one and defers the other; the shared
//!    budget repairs both.
//! 3. Passes converge: the bounded arm reaches the same table, and the same
//!    answers, as the unbounded arm.
//! 4. **A read taken while the table is only PARTIALLY repaired is correct.**
//!    That is the claim the whole change rests on, and it is the one that
//!    would fail if a truncated repair published a stamp it had not reached.

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
const MOVED: usize = 1_000;
/// Below `MOVED + MOVED * ADJ_REPAIR_SCAN_ROWS` (33,000) by an order of
/// magnitude, so the whole delta is far past one pass's budget — and large
/// enough that the bounded arm converges in a handful of passes rather than
/// five hundred.
const PASS_ROWS: usize = 3_300;

/// A warmed typed table, then a burst that moves `MOVED` of its rows.
///
/// `set_single_node_stale_walk(false)` keeps the warming reads from being the
/// thing that repairs the table: this file is about what the PASS does with a
/// table it finds stale.
fn stale_table_with_a_burst(g: &Graph) -> (Vec<u64>, u32) {
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
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    for &src in &ids[..MOVED] {
        g.create_rel(src, "T", ids[(rng.next() % NODES) as usize], &none)
            .expect("burst");
    }
    (ids, tok)
}

/// Every node's outgoing `T` peers, sorted — the answer the two arms must
/// agree on. Reading this REPAIRS whatever the pass left, which is exactly
/// what a reader does in production and is the point of the comparison.
fn answers(g: &Graph, ids: &[u64], tok: u32) -> Vec<Vec<u64>> {
    ids.iter()
        .map(|&id| {
            let mut peers: Vec<u64> = g
                .adjacent_slim(id, Dir::Out, &Some(vec![tok]))
                .iter()
                .map(|e| e.peer)
                .collect();
            peers.sort_unstable();
            peers
        })
        .collect()
}

/// Run passes until one changes nothing, and say how many it took.
fn passes_to_converge(g: &Graph) -> usize {
    for n in 1..=2_000 {
        let r = g.refresh_stale_derived();
        if r.adjacency_repaired == 0 && r.adjacency_rebuilt == 0 && r.adjacency_deferred == 0 {
            return n;
        }
    }
    panic!("the pass never converged in 2,000 rounds");
}

/// THE CLAIM: the pass takes a SLICE of the oversized repair, not all of it.
#[test]
fn an_oversized_repair_is_truncated_to_what_the_budget_has_left() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (_ids, _tok) = stale_table_with_a_burst(&g);
    g.set_refresh_pass_rows(PASS_ROWS);

    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    eprintln!("[bounded] one pass: {report:?} {:?}", trace.counters());

    assert!(
        report.adjacency_repaired >= 1,
        "the oversized table must still be repaired — a bounded step is still \
         a step, and refusing it is the permanent deferral this whole escape \
         hatch exists to prevent: {report:?}"
    );
    assert!(
        count(&trace, "graph.adjacency repair truncated to a row budget") >= 1,
        "and it must SAY it was truncated, or this passed because the fixture \
         happened to fit: {:?}",
        trace.counters()
    );
    assert!(
        count(
            &trace,
            "graph.derived refresh shared its budget across stale tables"
        ) >= 1,
        "and the pass must say it handed out slices rather than racing for the \
         pool — that is what the bound is derived from: {:?}",
        trace.counters()
    );

    // The table is NOT current: a second pass finds work left. That is the
    // whole difference from the unbounded arm, so it is asserted rather than
    // implied.
    let after = g.refresh_stale_derived();
    eprintln!("[bounded] second pass: {after:?}");
    assert!(
        after.adjacency_repaired >= 1,
        "a truncated repair must leave the table stale for the next pass — if \
         one bounded pass finished the job, the bound did nothing: {after:?}"
    );
}

/// THE SLICES ARE MAX-MIN FAIR, NOT EQUAL.
///
/// A lopsided stale set is the normal case: several tables one write behind
/// and one holding the backlog. Under a flat split the backlogged table gets
/// 1/n of the pass while the others cannot spend their share, so the budget
/// goes unused and the one table that needed it is throttled. Under max-min
/// each table takes the smaller of its own cost and an even share of what is
/// left, cheapest first, so the remainder falls to the tables that can use it.
///
/// This is not a theoretical preference: the flat split truncated
/// `adjacency_repair_differential`'s 2,500-node ranged repairs, its overlay
/// never reached the 4,096 fold, and the suite's non-vacuity assertion caught
/// it. The claim below is the one that fixed it — a lone big table gets a
/// slice big enough to finish, so its answer converges in ONE pass.
#[test]
fn a_lone_big_table_gets_the_whole_pool_not_one_nth_of_it() {
    // Four tables stale, three of them trivially so. The big one's delta is
    // well inside HALF the pass budget, so max-min must let it finish; a flat
    // quarter (and a flat half of a half) would not.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x2468_ACE0_1357_9BDF);
    let types = ["T", "U", "V", "W"];
    for &src in &ids {
        for ty in types {
            g.create_rel(src, ty, ids[(rng.next() % NODES) as usize], &none)
                .expect("rel");
        }
    }
    g.shared_store().seal();
    let toks: Vec<u32> = types
        .iter()
        .map(|ty| g.type_tokens_peek(&[ty.to_string()]).expect("minted")[0])
        .collect();
    for &tok in &toks {
        let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    }
    // T carries the backlog; U, V and W are one write each.
    for &src in &ids[..MOVED] {
        g.create_rel(src, "T", ids[(rng.next() % NODES) as usize], &none)
            .expect("burst");
    }
    for ty in ["U", "V", "W"] {
        g.create_rel(ids[0], ty, ids[1], &none).expect("nudge");
    }

    // Half of this is 60,000, comfortably over T's `MOVED + MOVED * 32`
    // (33,000) — but a flat quarter of the half is 15,000, which is not.
    g.set_refresh_pass_rows(120_000);
    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    eprintln!("[max-min] {report:?} {:?}", trace.counters());
    assert!(
        report.adjacency_repaired >= 4,
        "every stale table is served: {report:?}"
    );
    assert_eq!(
        count(&trace, "graph.adjacency repair truncated to a row budget"),
        0,
        "and NOTHING is truncated — the three cheap tables cannot spend their \
         even share, so the remainder falls to the one table that can, which \
         is then big enough to finish. A flat split truncates T here: {:?}",
        trace.counters()
    );
    let after = g.refresh_stale_derived();
    assert_eq!(
        after.adjacency_repaired, 0,
        "so one pass brings the lopsided set current: {after:?}"
    );
}

/// THE CANARY: with the lever off, the same fixture and the same tiny budget
/// swallow the whole delta in ONE pass. Without this the test above says
/// nothing — it would pass on a fixture that simply needs two passes.
#[test]
fn the_unbounded_arm_takes_the_whole_delta_in_one_pass() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (_ids, _tok) = stale_table_with_a_burst(&g);
    g.set_refresh_pass_rows(PASS_ROWS);
    g.set_bounded_derived_repair(false);

    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    eprintln!("[unbounded] one pass: {report:?} {:?}", trace.counters());
    assert!(report.adjacency_repaired >= 1, "{report:?}");
    assert_eq!(
        count(&trace, "graph.adjacency repair truncated to a row budget"),
        0,
        "the off arm must not truncate: {:?}",
        trace.counters()
    );
    assert!(
        count(
            &trace,
            "graph.derived refresh took a repair over its whole budget"
        ) >= 1,
        "the off arm is the one that goes over budget — that is what it is \
         for: {:?}",
        trace.counters()
    );

    let after = g.refresh_stale_derived();
    assert_eq!(
        after.adjacency_repaired, 0,
        "the unbounded arm finishes in one pass, so the second finds nothing: \
         {after:?}"
    );
}

/// TWO oversized tables, one pass, a budget under either of them.
///
/// THE CLAIM: every stale table is served. The old rule takes the first and
/// defers the second — and defers it again next pass, and the pass after, from
/// the same stable iteration order — so the second table's delta only grows.
/// That is the starvation that produces a change set 32 passes long on a mix
/// whose pass fires every 8,192 commits.
fn two_stale_tables(g: &Graph) -> Vec<u64> {
    g.set_degree_table_after(0);
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x1357_9BDF_0246_8ACE);
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
    for &src in &ids[..MOVED] {
        for ty in ["T", "U"] {
            g.create_rel(src, ty, ids[(rng.next() % NODES) as usize], &none)
                .expect("burst");
        }
    }
    ids
}

#[test]
fn every_stale_table_is_served_by_the_pass_not_just_the_first() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let _ids = two_stale_tables(&g);
    g.set_refresh_pass_rows(PASS_ROWS);

    let report = g.refresh_stale_derived();
    eprintln!("[shared] two oversized tables, one pass: {report:?}");
    assert_eq!(
        report.adjacency_repaired, 2,
        "both stale tables must be repaired — each gets a slice of the pass. \
         Serving only the first is the starvation: the map's order does not \
         change, so the same table is skipped every pass and its delta grows \
         until it is taken whole: {report:?}"
    );
    assert_eq!(
        report.adjacency_deferred, 0,
        "and nothing is deferred by the budget under the shared rule: {report:?}"
    );

    // THE CANARY: the old rule on the same fixture starves the second table.
    let old = Graph::new(Store::new(), Realm(1), Namespace(1));
    let _ = two_stale_tables(&old);
    old.set_refresh_pass_rows(PASS_ROWS);
    old.set_bounded_derived_repair(false);
    let before = old.refresh_stale_derived();
    eprintln!("[first-come] same fixture: {before:?}");
    assert_eq!(
        before.adjacency_repaired, 1,
        "the first-come rule takes exactly one: {before:?}"
    );
    assert!(
        before.adjacency_deferred >= 1,
        "and defers the rest — this is the arm the fix replaces: {before:?}"
    );
}

/// THE DIFFERENTIAL: both arms converge, to the SAME answers.
///
/// The bounded arm must take more passes — that is the bound working — and
/// must not take a different view of the graph when it gets there.
#[test]
fn both_arms_converge_to_the_same_adjacency() {
    let bounded = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids_b, tok_b) = stale_table_with_a_burst(&bounded);
    bounded.set_refresh_pass_rows(PASS_ROWS);

    let plain = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids_p, tok_p) = stale_table_with_a_burst(&plain);
    plain.set_refresh_pass_rows(PASS_ROWS);
    plain.set_bounded_derived_repair(false);

    assert_eq!(ids_b, ids_p, "the two fixtures must be built identically");

    let n_bounded = passes_to_converge(&bounded);
    let n_plain = passes_to_converge(&plain);
    eprintln!("[differential] passes: bounded {n_bounded}, unbounded {n_plain}");
    assert!(
        n_bounded > n_plain,
        "the bounded arm spreads the same work over MORE passes — that is the \
         entire mechanism; {n_bounded} vs {n_plain}"
    );

    assert_eq!(
        answers(&bounded, &ids_b, tok_b),
        answers(&plain, &ids_p, tok_p),
        "a partial repair changes WHEN the work is done, never what it \
         produces"
    );
}

/// THE SAFETY CLAIM: a read taken while the table is only PARTIALLY repaired
/// is correct.
///
/// This is what a truncated publish would break if it stamped an epoch it had
/// not reached: the reader would find `snap.at >= epoch`, be served the stale
/// table, and never repair the rows the pass had skipped.
#[test]
fn a_read_against_a_partially_repaired_table_is_correct() {
    let truth = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids_t, tok_t) = stale_table_with_a_burst(&truth);
    truth.set_bounded_derived_repair(false);
    truth.set_refresh_pass_rows(0); // no budget at all: repair everything now
    let _ = truth.refresh_stale_derived();
    let expected = answers(&truth, &ids_t, tok_t);

    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = stale_table_with_a_burst(&g);
    g.set_refresh_pass_rows(PASS_ROWS);

    // ONE bounded pass, then read. The table is stale by construction (the
    // test above pins that), so this read is the mid-convergence case.
    let report = g.refresh_stale_derived();
    assert!(report.adjacency_repaired >= 1, "{report:?}");

    assert_eq!(
        answers(&g, &ids, tok),
        expected,
        "a reader must see the same graph whether the maintenance pass had \
         finished catching the table up or had only taken a slice of it"
    );
}
