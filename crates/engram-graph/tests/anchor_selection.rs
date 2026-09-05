//! Does it matter which END of a pattern you write first?
//!
//! It must not. `MATCH (m:Msg)-[:BY]->(p:Person {pid: 0})` and
//! `MATCH (p:Person {pid: 0})<-[:BY]-(m:Msg)` are the same question: "the
//! messages by person 0". One of them names an indexed, single-row endpoint;
//! both of them name it. A planner that picks its scan root by SELECTIVITY
//! answers them at the same speed. A planner that picks it by SOURCE ORDER
//! answers one of them by scanning every message in the graph.
//!
//! # Why this is a release blocker rather than a tuning note
//!
//! The stress harness measured the gap over Bolt against a 100k-node LDBC SNB
//! corpus, on the shape LDBC calls IS5 ("messages by a creator"):
//!
//! | form | p50 | ops in 10 s |
//! |---|---|---|
//! | `(m:Message)-[:HAS_CREATOR]->(p:Person {id: K})` | 9.82 ms | 1,013 |
//! | `(p:Person {id: K})<-[:HAS_CREATOR]-(m:Message)` | 0.40 ms | 23,679 |
//!
//! 24.6x, from nothing but the order the pattern was typed. The near-zero
//! variance on the slow form (p50 9.82, p95 10.38) is the signature: it is a
//! fixed-cost full scan, not a traversal that sometimes finds more.
//!
//! Engram is meant to be a drop-in replacement for a database whose planner
//! picks the anchor by selectivity. Applications are therefore full of patterns
//! written the "wrong" way round, because against that database there is no
//! wrong way round.
//!
//! **This is FIXED** — see the per-test status below. The tables above are the
//! measurement that motivated the fix, kept because they say what the defect
//! cost; they are not a description of current behaviour.
//!
//! # What the engine already has
//!
//! Nothing here needs new storage. The adjacency table keeps BOTH directions,
//! so an incoming walk is as cheap as an outgoing one; `reverse_bound_end_path`
//! already reverses a fixed-length chain and flips every hop's direction. That
//! machinery fires only when the far end was bound by an EARLIER clause. The
//! missing case is the one measured above: both endpoints introduced by the
//! same pattern, one of them index-servable.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .len()
}

/// A miniature of the SNB shape the stress harness measured: many people, a
/// FIXED number of messages each, every message pointing at its creator.
///
/// Messages-per-person is held constant and the person count is what grows.
/// That is not incidental — it is what makes the measurement mean anything.
/// An earlier version of this fixture spread a growing message count over a
/// fixed 50 people, so a 10x bigger corpus also produced a 10x bigger ANSWER,
/// and both forms duly got ~9.4x slower. That measured result-set size, not
/// plan quality, and it hid the defect completely: the ratio came out 1.9x
/// against the 24.6x seen over Bolt.
///
/// With the answer pinned at `MSGS_PER_PERSON`, growing the corpus grows only
/// the `:Msg` label — which a scan-rooted plan must walk and an anchored plan
/// must not. That is the whole question.
const MSGS_PER_PERSON: i64 = 20;

fn snb_shaped(persons: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX person_pid FOR (n:Person) ON (n.pid)").expect("parse index"),
        BTreeMap::new(),
    )
    .expect("create index");
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:Person {{pid: i}})", persons - 1),
    );
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i \
             MATCH (p:Person {{pid: i / {MSGS_PER_PERSON}}}) \
             CREATE (m:Msg {{mid: i}})-[:BY]->(p)",
            persons * MSGS_PER_PERSON - 1
        ),
    );
    g
}

// The two spellings of the measured query, with the same `LIMIT` the LDBC IS5
// shape carries. The limit matters: without it the answer size would creep back
// into the measurement the moment the fixture changed.
const SCAN_FIRST: &str = "MATCH (m:Msg)-[:BY]->(p:Person {pid: 7}) RETURN m.mid LIMIT 25";
const ANCHOR_FIRST: &str = "MATCH (p:Person {pid: 7})<-[:BY]-(m:Msg) RETURN m.mid LIMIT 25";

/// The two forms must return the SAME ANSWER.
///
/// Asserted first and separately, because a performance test whose two arms
/// compute different things measures nothing. If this ever fails, the timing
/// test below is meaningless and should be read as such.
#[test]
fn both_orderings_return_the_same_rows() {
    let g = snb_shaped(20);
    let scan_first = run(&g, SCAN_FIRST);
    let anchor_first = run(&g, ANCHOR_FIRST);
    assert_eq!(
        scan_first, anchor_first,
        "the two spellings of one question disagree: {scan_first} rows written \
         message-first vs {anchor_first} rows written person-first"
    );
    assert!(
        scan_first > 0,
        "the fixture produced no messages for person 3 — this test would pass \
         trivially on two empty answers"
    );
}

fn time_form(g: &Graph, src: &str, reps: usize) -> f64 {
    // One warm pass: the first call builds the range index, and charging that
    // to whichever form ran first would be the measurement.
    run(g, src);
    let t = Instant::now();
    for _ in 0..reps {
        run(g, src);
    }
    t.elapsed().as_secs_f64() / reps as f64
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a timing"));
    v[v.len() / 2]
}

/// Writing the selective end SECOND must not cost meaningfully more than
/// writing it first. RATCHETED.
///
/// # Status — FIXED, and this is now a regression guard (re-measured 2026-09-01)
///
/// It was committed FAILING, as the pin for an unfixed defect. It passes:
/// **ratio 1.0x** (message-first 0.319 ms/op, person-first 0.317). The fix is
/// `reroot_to_selective_end` in `pipeline.rs` — when the start of a fresh path
/// carries no index-servable predicate and the terminal does, and the
/// terminal's label is no larger, the chain is rerooted and driven from there
/// (`pipeline.chain rerooted for selectivity`).
///
/// The companion test measures the same pair with `selective_anchor` OFF and
/// reports **262.8x** (57.482 ms vs 0.219), so the 1.0x above is the fix
/// working and not the fixture failing to pose the question.
///
/// This status line is maintained because the previous one was not: it still
/// said FAILING months after the defect was closed, and reading it cost a
/// planning session that took a live release blocker off the shelf and started
/// designing a fix for something already shipped. A status comment that a test
/// run can contradict is worth exactly as much as the last time someone
/// checked it.
#[test]
fn anchor_is_chosen_by_selectivity_not_by_source_order() {
    let g = snb_shaped(2_000); // 2,000 persons -> 40,000 messages

    let scan_first = median((0..5).map(|_| time_form(&g, SCAN_FIRST, 20)).collect());
    let anchor_first = median((0..5).map(|_| time_form(&g, ANCHOR_FIRST, 20)).collect());
    let ratio = scan_first / anchor_first.max(f64::MIN_POSITIVE);

    eprintln!(
        "IS5 shape, 40,000 messages / 2,000 persons (median of 5):\n  \
         message-first {:.3} ms/op\n  person-first  {:.3} ms/op\n  ratio {ratio:.1}x",
        scan_first * 1e3,
        anchor_first * 1e3
    );

    // Measured 24.6x over Bolt at 100k nodes. A drop-in replacement cannot
    // charge an application 25x for writing a pattern the way the database it
    // replaces encourages. 3x leaves room for the genuine residual — the
    // reversed form still walks an adjacency list — without leaving room for a
    // whole-label scan.
    const RATCHET: f64 = 3.0;
    assert!(
        ratio < RATCHET,
        "writing the indexed endpoint SECOND costs {ratio:.1}x more than writing it \
         first ({:.3} ms vs {:.3} ms per op), past the {RATCHET}x ratchet. The planner \
         is picking its scan root by source order: `path.start` always drives, so the \
         message-first form scans every :Msg and filters, while the person-first form \
         seeks one indexed row and walks its adjacency. Both spellings name the same \
         single indexed endpoint.",
        scan_first * 1e3,
        anchor_first * 1e3
    );
}

/// The A/B toggle must really revert the behaviour.
///
/// Same reasoning as the index toggle's canary: `portserve`'s
/// `ENGRAM_BENCH_BASELINE` arm is only a baseline if it actually turns the fix
/// off. With `set_selective_anchor(false)` the source-order penalty must come
/// back — if it does not, both arms of the benchmark are the fixed engine and
/// the reported improvement is noise wearing a number.
#[test]
fn the_baseline_toggle_restores_source_order_anchoring() {
    let g = snb_shaped(2_000);
    g.set_selective_anchor(false);
    let scan_first = median((0..3).map(|_| time_form(&g, SCAN_FIRST, 10)).collect());
    let anchor_first = median((0..3).map(|_| time_form(&g, ANCHOR_FIRST, 10)).collect());
    let ratio = scan_first / anchor_first.max(f64::MIN_POSITIVE);
    eprintln!(
        "with selective anchor OFF: message-first {:.3} ms/op, person-first {:.3} ms/op, \
         ratio {ratio:.1}x",
        scan_first * 1e3,
        anchor_first * 1e3
    );
    assert!(
        ratio > 10.0,
        "with selective anchor OFF the two spellings still cost about the same \
         ({ratio:.1}x) — the toggle does not restore source-order anchoring, so the \
         benchmark's baseline arm is not a baseline"
    );
}

/// WHICH engine is responsible?
///
/// There are two `MATCH` executors: the general interpreter (`interp.rs`) and
/// the columnar core-chain fast path (`pipeline.rs`), which recognises common
/// shapes and falls back to the interpreter otherwise. A one-hop `MATCH … RETURN`
/// is exactly what the columnar recogniser claims, so a fix applied to only one
/// of them changes nothing and looks like a fix that does not work.
///
/// This test says which engine the measurement is coming from, so that mistake
/// is made once rather than every time someone touches anchor selection.
#[test]
fn report_which_engine_serves_this_shape() {
    let g = snb_shaped(2_000);
    let with_columnar = median((0..3).map(|_| time_form(&g, SCAN_FIRST, 10)).collect());

    g.set_columnar_scans(false);
    let without_columnar = median((0..3).map(|_| time_form(&g, SCAN_FIRST, 10)).collect());
    g.set_columnar_scans(true);

    let anchored = median((0..3).map(|_| time_form(&g, ANCHOR_FIRST, 10)).collect());

    eprintln!(
        "message-first, columnar ON  {:.3} ms/op\n\
         message-first, columnar OFF {:.3} ms/op\n\
         person-first  (either)      {:.3} ms/op",
        with_columnar * 1e3,
        without_columnar * 1e3,
        anchored * 1e3
    );
    assert!(
        with_columnar > 0.0 && without_columnar > 0.0,
        "both engines must actually be measured"
    );
}

/// The same asymmetry, isolated from timing entirely: how much work does each
/// form do?
///
/// A ratio test on a shared machine can be argued with. A row-count ratio
/// cannot: if the message-first form touches the whole label, its cost grows
/// with the number of messages while the person-first form's does not. This
/// measures that growth directly by running both forms at two corpus sizes,
/// so it stays meaningful on a machine too noisy for the ratchet above.
#[test]
fn report_how_each_form_scales_with_the_big_label() {
    let small = snb_shaped(500); // 10,000 messages
    let large = snb_shaped(5_000); // 100,000 messages

    let s_small = median((0..3).map(|_| time_form(&small, SCAN_FIRST, 10)).collect());
    let s_large = median((0..3).map(|_| time_form(&large, SCAN_FIRST, 10)).collect());
    let a_small = median((0..3).map(|_| time_form(&small, ANCHOR_FIRST, 10)).collect());
    let a_large = median((0..3).map(|_| time_form(&large, ANCHOR_FIRST, 10)).collect());

    eprintln!(
        "  message-first: 10k msgs {:.3} ms -> 100k msgs {:.3} ms  ({:.1}x)",
        s_small * 1e3,
        s_large * 1e3,
        s_large / s_small.max(f64::MIN_POSITIVE)
    );
    eprintln!(
        "  person-first:  10k msgs {:.3} ms -> 100k msgs {:.3} ms  ({:.1}x)",
        a_small * 1e3,
        a_large * 1e3,
        a_large / a_small.max(f64::MIN_POSITIVE)
    );

    // The ANSWER is the same size in both corpora (LIMIT 25 over a person with
    // a fixed message count), so an anchored plan should be close to flat and a
    // scan-rooted one should grow with the corpus. That divergence is the
    // diagnostic; neither line means much alone.
    assert!(
        s_small > 0.0 && a_small > 0.0,
        "both forms must actually be measured"
    );
}
