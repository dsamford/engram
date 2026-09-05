//! The maintenance refresh is OFF THE READ PATH — three claims, each with
//! the arm that would fail without it:
//!
//! 1. A reader building table B is not delayed by the refresh rebuilding
//!    table A. Build guards are per table (`Slot::enter_build`); they were
//!    one guard per family, so a refresh rebuilding one table held every
//!    worker that needed to build any other for the whole span walk.
//!    Asserted by ORDERING, not wall time: the reader's build completes while
//!    the refresh is still running (`done` is false after the read).
//! 2. The refresh never REBUILDS an untyped table — only the warm asks for
//!    those, and a rebuild is the whole span. It repairs one when the logs
//!    allow and otherwise defers it to a reader that wants it.
//! 3. A pass rebuilds at most ONE table; the rest are deferred to the next.
//!
//! The fixture: a small type `A` (and `D`) whose tables a burst over more
//! than `ADJ_REPAIR_MAX` nodes makes stale beyond repair (the cost gate
//! declines: 14,000 changed nodes against a 5,000-entry table), and a big
//! type `C` nobody reads that makes every span walk long. The entry budget
//! is set so `A`'s rebuild fits and a build of `C`'s table is declined a
//! few thousand rows into the walk — which is what makes the reader's
//! build FAST while the refresh's rebuild is SLOW, so the ordering in (1)
//! cannot come out right by accident.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const NODES: u64 = 20_000;
/// Nodes with one `A` (and one `D`) relationship each.
const SMALL: u64 = 5_000;
/// `C` relationships per node — 400,000 in all.
const BIG_PER: u64 = 20;
/// The burst: one more `A` (and `D`) from each of this many distinct nodes.
/// Over `ADJ_REPAIR_MAX` (4,096), and `14,000 + 14,000 × 32 ≥ 5,000`, so the
/// cost gate declines the repair and the table must be rebuilt.
const BURST_NODES: u64 = 14_000;
/// Entries a table may hold: `A`'s 19,000 after the burst fits; `C`'s
/// 400,000 does not, so a build of it stops after this many rows.
const BUDGET: usize = 50_000;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

struct Fixture {
    g: Arc<Graph>,
    ids: Vec<u64>,
    a: u32,
    c: u32,
    d: u32,
}

fn fixture() -> Fixture {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    g.set_adj_table_max_entries(BUDGET);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    for &src in &ids[..SMALL as usize] {
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(src, "A", dst, &none).expect("A");
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(src, "D", dst, &none).expect("D");
    }
    for &src in &ids {
        for _ in 0..BIG_PER {
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(src, "C", dst, &none).expect("C");
        }
    }
    g.shared_store().seal();
    let tok = |name: &str| g.type_tokens_peek(&[name.to_string()]).expect("minted")[0];
    let (a, c, d) = (tok("A"), tok("C"), tok("D"));
    Fixture {
        g: Arc::new(g),
        ids,
        a,
        c,
        d,
    }
}

fn out_row(g: &Graph, node: u64, tok: u32) -> usize {
    g.adjacent_slim(node, Dir::Out, &Some(vec![tok])).len()
}

/// One more relationship of `ty` from each of the first `BURST_NODES` nodes.
fn burst(f: &Fixture, ty: &str) {
    let none = BTreeMap::new();
    let mut rng = Lcg(0x1234_5678_9ABC_DEF1);
    for &src in &f.ids[..BURST_NODES as usize] {
        let dst = f.ids[(rng.next() % NODES) as usize];
        f.g.create_rel(src, ty, dst, &none).expect("burst");
    }
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

#[test]
fn a_reader_building_table_b_is_not_delayed_by_the_refresh_rebuilding_table_a() {
    let f = fixture();
    // §5.3 DEFAULTS THE PASS'S REBUILD OFF, and this test is about what the
    // rebuild does to a concurrent reader — so it runs the arm that still has
    // one. The mechanism is gated, not deleted, and the property below is what
    // makes it safe to keep for the operator who turns it back on.
    f.g.set_demote_adjacency_rebuild(false);
    // Build `A`'s table, then make it stale beyond repair.
    let (_, trace) = engram_observe::with_trace(|| out_row(&f.g, f.ids[0], f.a));
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1, "{:?}", trace.counters());
    burst(&f, "A");

    let started = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let refresh = {
        let (g, started, done) = (Arc::clone(&f.g), Arc::clone(&started), Arc::clone(&done));
        std::thread::spawn(move || {
            started.store(true, Ordering::SeqCst);
            let r = g.refresh_stale_derived();
            done.store(true, Ordering::SeqCst);
            r
        })
    };
    // The reader: waits for the refresh to be running, then needs `C`'s
    // table — a build, which the budget declines a few thousand rows in.
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    let (rows, trace) = engram_observe::with_trace(|| out_row(&f.g, f.ids[1], f.c));
    let refresh_still_running = !done.load(Ordering::SeqCst);
    let report = refresh.join().expect("refresh");

    assert_eq!(rows, BIG_PER as usize, "the reader must answer from the direct walk");
    // A `sometimes!` event, not a counter: the decline is a declared state.
    assert!(
        trace.sometimes_hit().contains("graph.adjacency table declined by the entry budget"),
        "the reader must have ATTEMPTED a build of C's table (and been declined): {:?} / {:?}",
        trace.sometimes_hit(),
        trace.counters()
    );
    assert_eq!(report.adjacency_rebuilt, 1, "the refresh must have REBUILT A's table: {report:?}");
    assert!(
        refresh_still_running,
        "the reader's build of C completed only after the refresh finished rebuilding A — \
         the reader waited behind the refresh's span walk (a shared build guard). If the \
         refresh genuinely finished first, the fixture is too small to order the two: {report:?}"
    );
}

#[test]
fn the_refresh_never_rebuilds_an_untyped_table() {
    let f = fixture();
    // The untyped O table (410,000 entries — the budget is lifted so it is
    // admitted), as the warm would leave it, then a burst that the cost
    // gate declines to repair (14,000 changed nodes × 32 ≥ 410,000): a
    // reader would rebuild it; the refresh must not.
    f.g.set_adj_table_max_entries(64 << 20);
    let (_, trace) = engram_observe::with_trace(|| f.g.adjacent_slim(f.ids[0], Dir::Out, &None).len());
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1, "{:?}", trace.counters());
    burst(&f, "A");
    let before = engram_graph::counters::ADJ_TABLES_BUILT.load(Ordering::Relaxed);
    let (report, trace) = engram_observe::with_trace(|| f.g.refresh_stale_derived());
    assert_eq!(report.adjacency_rebuilt, 0, "an untyped table was rebuilt by the refresh: {report:?}");
    assert_eq!(report.adjacency_repaired, 0, "{report:?}");
    assert_eq!(report.adjacency_deferred, 1, "the stale untyped table must be DEFERRED: {report:?}");
    assert_eq!(count(&trace, "graph.adjacency repair declined by cost"), 1, "{:?}", trace.counters());
    assert_eq!(engram_graph::counters::ADJ_TABLES_BUILT.load(Ordering::Relaxed), before);
    assert!(!report.any(), "a pass that deferred everything brought nothing current");
    // The reader that wants it pays, exactly as before the refresh existed.
    let (rows, trace) = engram_observe::with_trace(|| f.g.adjacent_slim(f.ids[0], Dir::Out, &None).len());
    // 20 `C`, one `A`, one `D`, and the burst's `A`.
    assert_eq!(rows, (BIG_PER + 3) as usize);
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1, "{:?}", trace.counters());
}

#[test]
fn a_pass_rebuilds_at_most_one_table_and_defers_the_rest_to_the_next() {
    let f = fixture();
    // The BUDGET's arm — see `the_demoted_pass_rebuilds_nothing_at_all` below
    // for the shipped default, which never reaches the budget at all.
    f.g.set_demote_adjacency_rebuild(false);
    assert_eq!(out_row(&f.g, f.ids[0], f.a), 1);
    assert_eq!(out_row(&f.g, f.ids[0], f.d), 1);
    burst(&f, "A");
    burst(&f, "D");
    let first = f.g.refresh_stale_derived();
    assert_eq!(
        (first.adjacency_rebuilt, first.adjacency_deferred, first.adjacency_repaired),
        (1, 1, 0),
        "one rebuild per pass, the other deferred: {first:?}"
    );
    assert!(first.any());
    let second = f.g.refresh_stale_derived();
    assert_eq!(
        (second.adjacency_rebuilt, second.adjacency_deferred),
        (1, 0),
        "the next pass takes the deferred one: {second:?}"
    );
    let third = f.g.refresh_stale_derived();
    assert!(!third.any(), "nothing left: {third:?}");
    assert_eq!(third, engram_graph::RefreshReport::default());
    // Both tables current and right, with no work left for the reader.
    let ((a, d), trace) = engram_observe::with_trace(|| (out_row(&f.g, f.ids[0], f.a), out_row(&f.g, f.ids[0], f.d)));
    assert_eq!((a, d), (2, 2));
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0);
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0);
    assert_eq!(count(&trace, "graph.adjacency tables reused"), 2, "{:?}", trace.counters());
}

/// §5.3's SHIPPED DEFAULT, stated here beside the budget it replaces.
///
/// The budget above is "one rebuild per pass". The default is now zero: a
/// compaction produces the same base as a by-product of work it must do anyway
/// (§5.2), so the pass paying separately for a span walk is paying twice. Both
/// tables are DEFERRED — reported, not silently dropped — and a reader that
/// actually wants one still builds it.
///
/// Written as its own test rather than folded into the two above, because the
/// two behaviours are both real and an operator can select either.
#[test]
fn the_demoted_pass_rebuilds_nothing_at_all() {
    let f = fixture();
    assert_eq!(out_row(&f.g, f.ids[0], f.a), 1);
    assert_eq!(out_row(&f.g, f.ids[0], f.d), 1);
    burst(&f, "A");
    burst(&f, "D");
    let r = f.g.refresh_stale_derived();
    assert_eq!(
        (r.adjacency_rebuilt, r.adjacency_repaired),
        (0, 0),
        "the demoted pass neither rebuilds nor repairs a change set past the          cost gate: {r:?}"
    );
    assert_eq!(
        r.adjacency_deferred, 2,
        "and it must SAY it deferred both — a table it declined to rebuild is          reported, never dropped from the report: {r:?}"
    );
    // The reader still gets a table, so the demotion moved the work rather
    // than removing it.
    let (rows, trace) = engram_observe::with_trace(|| out_row(&f.g, f.ids[0], f.a));
    assert_eq!(rows, 2, "one seeded A plus the burst's");
    assert_eq!(
        count(&trace, "graph.adjacency tables built"),
        1,
        "the reader that wants the table pays for it: {:?}",
        trace.counters()
    );
}
