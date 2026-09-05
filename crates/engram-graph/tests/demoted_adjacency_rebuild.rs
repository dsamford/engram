#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! §5.3 — the maintenance pass stops REBUILDING adjacency tables.
//!
//! A rebuild is a walk of the whole adjacency span; on SF1 paged an untyped
//! table costs 25 s. The refresh pass did one per tick to bring a stale table
//! current — and after §5.2 a full compaction produces that same base from work
//! it has to do anyway, so paying for the walk separately is paying twice.
//!
//! # What is asserted, and what is not
//!
//! ASSERTED: the pass stops rebuilding, and answers do not change. A stale
//! derived table is a PERFORMANCE state, never a correctness one — every hop
//! either finds the table current for its epoch or falls back to the span walk
//! — so the two arms must agree exactly on every answer.
//!
//! NOT asserted here: that the rebuild disappears from the process. It does
//! not. It moves to the reader's thread for a cold start and for a change set
//! the log or the cost gate refuses. That trade is only good while full
//! compactions stay frequent, which is a MEASURED quantity (§5.5's decision
//! rule), not something this test can establish.
//!
//! The fixture is built through the write API rather than Cypher, for
//! `adjacency_cost_repair.rs`' reason: the decline this file needs takes more
//! than `ADJ_REPAIR_MAX` (4,096) changed nodes, and that many through the
//! parser is minutes in a debug build.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A deterministic xorshift — no `rand` dependency, reproducible fixtures.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const NODES: u64 = 6_000;

/// A warmed typed table, then a change set past `ADJ_REPAIR_MAX` so repair
/// declines and the pass reaches its REBUILD — the only branch §5.3 changes.
///
/// Without the burst, every stale table is repairable and the pass never
/// rebuilds at all, so both arms would report zero and agree for the wrong
/// reason.
fn build_and_outrun_repair(g: &Graph) -> (Vec<u64>, u32) {
    g.set_degree_table_after(0);
    // The FIXED node cap, not the cost model: `nodes > ADJ_REPAIR_MAX` is a
    // flat refusal, which makes the decline a property of the fixture's size
    // rather than of a cost comparison that a later tuning could move.
    g.set_adj_cost_repair(false);

    let label = vec!["N".to_string()];
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &BTreeMap::new()).expect("node"))
        .collect();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let none = BTreeMap::new();
    for &src in &ids {
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(src, "T", dst, &none).expect("rel");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];

    // Warm both tables, so the pass has a published slot to find stale.
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    let _ = g.adjacent_slim(ids[0], Dir::Out, &None);

    // 5,000 changed nodes > 4,096: repair refuses, and the pass would rebuild.
    for &src in &ids[..5_000] {
        let dst = ids[(rng.next() % NODES) as usize];
        g.create_rel(src, "T", dst, &none).expect("burst rel");
    }
    (ids, tok)
}

/// A sample of out-neighbourhoods — the answer both arms must agree on.
fn answers(g: &Graph, ids: &[u64], tok: u32) -> Vec<(u64, Vec<u64>)> {
    let mut out = Vec::new();
    for &n in ids.iter().step_by(311) {
        let mut peers: Vec<u64> = g
            .adjacent_slim(n, Dir::Out, &Some(vec![tok]))
            .iter()
            .map(|e| e.peer)
            .collect();
        peers.sort_unstable();
        out.push((n, peers));
    }
    out
}

fn arm(demote: bool) -> (usize, usize, Vec<(u64, Vec<u64>)>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_demote_adjacency_rebuild(demote);
    let (ids, tok) = build_and_outrun_repair(&g);
    let report = g.refresh_stale_derived();
    (
        report.adjacency_rebuilt,
        report.adjacency_deferred,
        answers(&g, &ids, tok),
    )
}

/// THE DIFFERENTIAL AND ITS COUNTER HALF, together.
///
/// The counter half is not optional. Without it this could be comparing a pass
/// that rebuilt nothing against a pass that rebuilt nothing — two no-ops
/// agreeing — which is exactly what happens on a change set the log still
/// covers, since that table is REPAIRED and never reaches the rebuild at all.
#[test]
fn the_pass_stops_rebuilding_and_answers_do_not_change() {
    let (demoted_rebuilds, demoted_deferred, demoted_answers) = arm(true);
    let (kept_rebuilds, _kept_deferred, kept_answers) = arm(false);

    eprintln!(
        "[demote rebuild] maintenance-pass adjacency rebuilds: {demoted_rebuilds} \
         demoted ({demoted_deferred} deferred instead), {kept_rebuilds} kept"
    );
    assert!(
        kept_rebuilds > 0,
        "the OFF arm must actually rebuild in the pass, or the ON arm's zero \
         proves nothing: {kept_rebuilds}"
    );
    assert_eq!(
        demoted_rebuilds, 0,
        "the demoted pass must rebuild NOTHING — that is the whole item"
    );
    assert!(
        demoted_deferred > 0,
        "and it must say so: a table it declined to rebuild is DEFERRED, not \
         silently dropped from the report"
    );
    assert!(
        kept_answers.iter().any(|(_, p)| !p.is_empty()),
        "the fixture must have adjacency to answer, or agreement is vacuous"
    );
    assert_eq!(
        demoted_answers, kept_answers,
        "a stale derived table is a performance state, never a correctness \
         one: every hop either finds a table current for its epoch or walks \
         the span, so the arms must answer identically"
    );
}

/// The rebuild is DEMOTED, not deleted — a reader that actually wants the table
/// still gets one built.
///
/// Worth pinning separately because the failure mode of over-applying §5.3 is
/// silent: if the demotion leaked into the reader's path, hops would answer
/// from the span walk for ever and the only symptom would be a slow server.
#[test]
fn a_reader_still_builds_the_table_the_pass_declined() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_demote_adjacency_rebuild(true);
    let (ids, tok) = build_and_outrun_repair(&g);
    let report = g.refresh_stale_derived();
    assert_eq!(report.adjacency_rebuilt, 0, "the pass declines");

    let (_, trace) = engram_observe::with_trace(|| {
        let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    });
    let built = trace
        .counters()
        .get("graph.adjacency tables built")
        .copied()
        .unwrap_or(0);
    eprintln!("[demote rebuild] a reader built {built} table(s) the pass declined");
    assert!(
        built > 0,
        "the demotion moves the rebuild to the reader — it must not remove it, \
         or a stale table would never come back and every hop would walk the \
         span for ever"
    );
}

/// THE OTHER HALF OF §5.3: a READER holding a stale snapshot it cannot repair
/// must be kept off the full-span rebuild too.
///
/// `adj_table_snapshot_reporting`'s guard read `if !admit && tried.is_none()`,
/// so a stale snapshot turned the probe-count admission gate OFF and the reader
/// fell through to a rebuild — a walk of the whole span, 25 s for an untyped
/// table at SF1, on its own query thread.
///
/// That was a corner while the maintenance pass also rebuilt. Demoting that
/// pass made readers the ONLY ones who rebuild, which promoted the bypass to
/// the entire policy. This should have landed with §5.3 and did not.
///
/// Both halves are asserted: the rebuild stops, and the ANSWER does not change
/// — a table is a performance structure and the direct span walk is the same
/// truth, so a decline costs latency and never rows.
#[test]
fn a_reader_is_kept_off_the_full_span_rebuild_by_admission() {
    let arm = |gated: bool| -> (u64, Vec<(u64, Vec<u64>)>) {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        g.set_reader_rebuild_admission(gated);
        let (ids, tok) = build_and_outrun_repair(&g);
        // The gate counts probes per adjacency epoch, and the burst above bumped
        // it — so the next reader has a stale snapshot, a repair that declines
        // (5,000 changed nodes > ADJ_REPAIR_MAX), and a probe count of zero.
        // That is exactly the state the bypass used to route into a rebuild.
        g.set_degree_table_after(1_000_000);
        let (_, trace) = engram_observe::with_trace(|| {
            let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
        });
        let built = trace
            .counters()
            .get("graph.adjacency tables built")
            .copied()
            .unwrap_or(0);
        (built, answers(&g, &ids, tok))
    };
    let (gated_built, gated_answers) = arm(true);
    let (open_built, open_answers) = arm(false);

    eprintln!(
        "[reader admission] full-span rebuilds on the reader's thread:          {gated_built} gated, {open_built} open"
    );
    assert!(
        open_built > 0,
        "the UNGATED arm must actually rebuild on the reader's thread, or the          gated arm's zero proves nothing: {open_built}"
    );
    assert_eq!(
        gated_built, 0,
        "a reader with a stale snapshot it cannot repair must DECLINE, not walk          the whole span on its query thread"
    );
    assert!(
        gated_answers.iter().any(|(_, p)| !p.is_empty()),
        "the fixture must answer something, or agreement is vacuous"
    );
    assert_eq!(
        gated_answers, open_answers,
        "and declining must change no answer: the direct span walk is the same          truth the table would have been built from"
    );
}
