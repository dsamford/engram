#![allow(non_snake_case)]
//! Fix 83: a READER's adjacency repair does not fold its overlay; the
//! maintenance pass does.
//!
//! A repair past `adj_overlay_fold` rows folds the overlay into a fresh base
//! — `AdjTable::folded`, one pass over every row of the table, 50–100 MB of
//! new base for the SF1 tables. The pass pays that on its own thread. A
//! reader paid it on a QUERY thread: under a 5k-writes/s stream every table
//! crosses the 4,096-row threshold about once a second, and every multi-node
//! read that arrived before the pass repaired the table folded it. The v187
//! sweep's server log shows it as `[bolt] statement grew rss by 130–240 MB`
//! on 877 read statements, and the stall detector shows it as `write-heavy
//! @ 8` stopping for ~2 s every ~6 s while `write-only @ 8` — the same
//! writes with no readers — never dips.
//!
//! What this file pins:
//!   1. On the default arm a reader's repair past the threshold publishes the
//!      overlay UNFOLDED (the fold counter stays at zero, the deferral counter
//!      fires) and answers exactly what the folded arm answers.
//!   2. The maintenance pass folds it on its next repair of that table.
//!   3. On the metered arm (`set_deferred_reader_fold(false)`) the same read
//!      folds — the old shape, kept as the control.

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

const NODES: u64 = 2_000;
/// Rows the burst moves — well past the fold threshold below.
const MOVED: usize = 600;
const FOLD_AT: usize = 64;

/// A warmed typed table, then a burst that moves `MOVED` of its rows, so the
/// next reader finds it stale by more than `FOLD_AT` rows. The single-node
/// stale walk is OFF so the reader REPAIRS (as every multi-node reader does)
/// instead of walking its own span.
fn stale_table(g: &Graph) -> (Vec<u64>, u32) {
    g.set_degree_table_after(0);
    g.set_single_node_stale_walk(false);
    g.set_adj_overlay_fold(FOLD_AT);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0xF01D_0000_8300_0001);
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

/// The `(O, T)` table's overlay, as rows — what a reader's deferred fold
/// leaves in place.
fn overlay_rows(g: &Graph, tok: u32) -> usize {
    g.adj_table_parts_for_test(b'O', &Some(vec![tok]))
        .map(|p| p.overlay.len())
        .unwrap_or(0)
}

/// Fix 83's BOUND, in the regime that has NO pass at all (a bare `Graph`;
/// `--no-derived-refresh` on the server): a stream of bursts, each on ~50
/// distinct sources, with a multi-node read after every one. Without the
/// ceiling nothing would ever fold — every winning reader publish prunes the
/// change log, so the log never overflows into a rebuild — and the overlay
/// would grow toward the node count, with every repair's `overlay.clone()`
/// growing with it. With the ceiling (16 x the 64-row threshold here) the
/// deferral fires, the ceiling fires, and the overlay never exceeds the
/// ceiling. The answers are the arm that folds on every read's, throughout.
#[test]
fn a_reader_folds_past_the_deferral_ceiling_when_nothing_else_will() {
    const ROUNDS: u64 = 60;
    const BURST: u64 = 50;
    let ceiling = FOLD_AT * 16;
    let run = |deferred: bool| {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let (ids, tok) = stale_table(&g);
        g.set_deferred_reader_fold(deferred);
        let none = BTreeMap::new();
        let mut rng = Lcg(0xCE11_1000_8300_0002);
        let (mut left, mut past, mut folded, mut worst) = (0u64, 0u64, 0u64, 0usize);
        for round in 0..ROUNDS {
            for k in 0..BURST {
                let src = ids[((round * BURST + k) % NODES) as usize];
                g.create_rel(src, "T", ids[(rng.next() % NODES) as usize], &none)
                    .expect("burst");
            }
            let (_, t) =
                engram_observe::with_trace(|| g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])));
            left += count(
                &t,
                "graph.adjacency reader repair left its fold to the pass",
            );
            past += count(
                &t,
                "graph.adjacency reader repair folded past the deferral ceiling",
            );
            folded += count(&t, "graph.adjacency table overlay folded");
            worst = worst.max(overlay_rows(&g, tok));
        }
        (answers(&g, &ids, tok), left, past, folded, worst)
    };
    let (on, left, past, folded, worst) = run(true);
    let (off, left_off, past_off, folded_off, worst_off) = run(false);
    assert_eq!(
        on, off,
        "the bounded deferral and the fold-on-every-read arm disagree"
    );
    assert!(left > 0, "the deferral never fired on the ON arm");
    assert!(
        past > 0 && folded > 0,
        "{ROUNDS} bursts of {BURST} sources with no pass never reached the ceiling of {ceiling}: \
         the deferral is unbounded (left {left}, folded {folded})"
    );
    assert!(
        worst <= ceiling,
        "the overlay reached {worst} rows against a ceiling of {ceiling}"
    );
    assert!(
        left_off == 0 && past_off == 0 && folded_off > 0 && worst_off <= FOLD_AT.max(MOVED),
        "the OFF arm must fold on every read past the threshold: left {left_off}, past {past_off}, \
         folded {folded_off}, worst overlay {worst_off}"
    );
}

/// `--adj-overlay-fold 0` means an ALWAYS-EMPTY overlay — the arm that
/// separates the per-hop descent from the per-read staleness check — and the
/// deferral must not take that meaning away: a ceiling of 16 x 0 is 0, so a
/// reader's repair folds at once.
#[test]
fn the_always_fold_arm_still_folds_on_every_reader_repair() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = stale_table(&g);
    g.set_adj_overlay_fold(0);
    let (_, t) = engram_observe::with_trace(|| g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])));
    assert!(
        count(&t, "graph.adjacency tables repaired") >= 1,
        "the read did not repair the table: {:?}",
        t.counters()
    );
    assert!(
        count(&t, "graph.adjacency table overlay folded") >= 1,
        "the always-fold arm did not fold on the reader's repair: {:?}",
        t.counters()
    );
    assert_eq!(
        count(
            &t,
            "graph.adjacency reader repair left its fold to the pass"
        ),
        0,
        "the always-fold arm deferred: {:?}",
        t.counters()
    );
    assert_eq!(
        overlay_rows(&g, tok),
        0,
        "the arm's overlay must be empty after a repair"
    );
}

/// A table that goes QUIET after a burst is current, so no repair would ever
/// fold what the reader left — the pass folds it in place, at the same stamp.
#[test]
fn the_pass_folds_a_current_table_the_reader_left_unfolded() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = stale_table(&g);
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    let before = overlay_rows(&g, tok);
    assert!(
        before > FOLD_AT,
        "the reader's repair left {before} overlay rows, not past the {FOLD_AT} threshold"
    );
    let want = answers(&g, &ids, tok);
    // No write in between: the table is CURRENT when the pass runs.
    let (report, t) = engram_observe::with_trace(|| g.refresh_stale_derived());
    assert_eq!(
        report.adjacency_repaired, 0,
        "the table was current; the pass must not have repaired it: {report:?}"
    );
    assert!(
        count(&t, "graph.adjacency pass folded a current table in place") >= 1,
        "the pass left a current table's over-threshold overlay in place: {:?}",
        t.counters()
    );
    assert_eq!(
        overlay_rows(&g, tok),
        0,
        "the in-place fold left overlay rows"
    );
    // This thread memoised the UNFOLDED snapshot in `want`; the refold keeps
    // the stamp, so the memo must retire its entry by generation or the
    // answers below would never read through the fold and prove nothing.
    let (after, t2) = engram_observe::with_trace(|| answers(&g, &ids, tok));
    assert!(
        count(&t2, "graph.adjacency memo entry retired by a refold") >= 1,
        "this thread's memo kept serving the unfolded snapshot: {:?}",
        t2.counters()
    );
    assert_eq!(after, want, "the in-place fold changed an answer");
}

#[test]
fn a_reader_past_the_fold_threshold_publishes_the_overlay_unfolded() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = stale_table(&g);
    let (_, trace) =
        engram_observe::with_trace(|| g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok])));
    assert!(
        count(&trace, "graph.adjacency tables repaired") >= 1,
        "the read did not repair the table, so it measured nothing: {:?}",
        trace.counters()
    );
    assert_eq!(
        count(&trace, "graph.adjacency table overlay folded"),
        0,
        "a reader folded on the query thread: {:?}",
        trace.counters()
    );
    assert!(
        count(
            &trace,
            "graph.adjacency reader repair left its fold to the pass"
        ) >= 1,
        "the reader's repair was past the threshold but the deferral never fired: {:?}",
        trace.counters()
    );
}

#[test]
fn the_pass_folds_what_the_reader_left() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = stale_table(&g);
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    // One more write makes the table stale again, so the pass has a repair
    // to run — and with the reader's overlay already past the threshold,
    // that repair folds.
    let none = BTreeMap::new();
    g.create_rel(ids[1], "T", ids[2], &none).expect("one more");
    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    assert!(
        report.adjacency_repaired >= 1,
        "the pass did not repair the table: {report:?}"
    );
    assert!(
        count(&trace, "graph.adjacency table overlay folded") >= 1,
        "the pass repaired without folding the overlay the reader left: {:?}",
        trace.counters()
    );
    assert_eq!(
        count(
            &trace,
            "graph.adjacency reader repair left its fold to the pass"
        ),
        0,
        "the pass is not a reader and must not defer: {:?}",
        trace.counters()
    );
}

#[test]
fn the_metered_arm_folds_on_the_read_and_both_arms_answer_the_same() {
    let on = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids_on, tok_on) = stale_table(&on);
    let off = Graph::new(Store::new(), Realm(1), Namespace(1));
    off.set_deferred_reader_fold(false);
    let (ids_off, tok_off) = stale_table(&off);

    let (_, t_off) = engram_observe::with_trace(|| {
        off.adjacent_slim(ids_off[0], Dir::Out, &Some(vec![tok_off]))
    });
    assert!(
        count(&t_off, "graph.adjacency table overlay folded") >= 1,
        "the control arm did not fold on the read, so the arms do not differ: {:?}",
        t_off.counters()
    );
    assert_eq!(
        count(
            &t_off,
            "graph.adjacency reader repair left its fold to the pass"
        ),
        0,
        "the control arm deferred: {:?}",
        t_off.counters()
    );
    let (_, t_on) =
        engram_observe::with_trace(|| on.adjacent_slim(ids_on[0], Dir::Out, &Some(vec![tok_on])));
    assert_eq!(count(&t_on, "graph.adjacency table overlay folded"), 0);

    // The differential: every node's peers, both arms, byte for byte. The
    // fixtures are seeded identically, so the ids line up.
    assert_eq!(ids_on, ids_off, "the fixtures diverged");
    assert_eq!(
        answers(&on, &ids_on, tok_on),
        answers(&off, &ids_off, tok_off),
        "a deferred fold changed an answer"
    );
}
