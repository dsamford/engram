#![allow(non_snake_case)]
//! Splitting a SEED SCAN across morsels — the A/B differential.
//!
//! # The gap this closes
//!
//! `drive_stage_rows` splits a stage's INPUT ROWS, which is the right unit
//! when a stage has many and useless when it has one. A first-stage `MATCH`
//! always has one: its input is the single empty seed row, so
//! `input.len() >= parallel_min_rows()` declines it by construction and every
//! bit of the stage's cost — the label scan and the whole expansion hanging
//! off it — runs on one core.
//!
//! MEASURED on SNB BI at SF3, 2026-09-21. bi17 carries no `WITH` at all, so it
//! is ONE stage driven by ONE row; it overran a 900 s ceiling pinned at
//! exactly 1.00 load on a 40-core pod with `ENGRAM_QUERY_PARALLELISM=40` and
//! the fairness stamp verifying width 40. bi12's expensive stage is the same
//! shape, and its single parallel drive landed on the trivial final
//! aggregation while the stage doing the work ran serial — a NON-ZERO
//! parallelism counter that proved nothing.
//!
//! # What these tests pin
//!
//! Agreement is necessary and not sufficient. The first version of the
//! `drive_stage_rows` differential ran SIX of its nine cases serially on both
//! arms and agreed trivially, which is exactly how a parallel path that never
//! fires passes every differential written for it. So each case here states
//! whether it expects the split to ENGAGE, and `seed_split_fired` reads the
//! counter to check.
//!
//! Row ORDER is part of the contract: morsels are concatenated in slice order,
//! so a split scan reproduces the serial one byte-for-byte.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, SerialExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The test-lane threaded executor — the same shape as the server's
/// production implementor (tests may spawn; the engine may not).
struct TestExec(usize);

impl ScopedExec for TestExec {
    fn width(&self) -> usize {
        self.0
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.0.min(n).max(1);
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(i);
                    }
                });
            }
        });
    }
}

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn rows(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

type Rows = Vec<Vec<Value>>;

/// 60 People, each KNOWS three others, edges weighted. Sixty seeds is past the
/// lowered `parallel_min_rows` so the scan splits, and the KNOWS fan-out gives
/// each seed real expansion work to carry into its morsel.
fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 59) AS i CREATE (:P {id: i, grp: i % 5})",
    );
    ddl(
        &g,
        "MATCH (a:P), (b:P) \
         WHERE b.id IN [(a.id + 1) % 60, (a.id + 11) % 60, (a.id + 23) % 60] \
         CREATE (a)-[:R {w: a.id * 100 + b.id}]->(b)",
    );
    g
}

/// Serial (no exec installed), then the THREADED executor, then the inline
/// `SerialExec` — all three must agree byte-for-byte, order included.
fn all_three(g: &Graph, q: &str) -> (Rows, Rows, Rows) {
    g.set_exec(None);
    let serial = rows(g, q);
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let threaded = rows(g, q);
    g.set_exec(Some(Arc::new(SerialExec)));
    let inline = rows(g, q);
    g.set_exec(None);
    (serial, threaded, inline)
}

/// Did the seed split actually run? A green differential over a path that
/// never fired proves nothing, which is the failure this guards.
/// MUST install a WIDE executor. The first version of this helper installed
/// `SerialExec`, whose `width()` is 1, against a gate that reads `width() > 1`
/// -- so the counter it read could never be bumped and every case it guarded
/// "failed" identically. A canary wired to a gate it cannot pass measures the
/// canary.
fn counter(g: &Graph, q: &str, name: &str) -> u64 {
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (_, trace) = engram_observe::with_trace(|| rows(g, q));
    g.set_exec(None);
    trace.counters().get(name).copied().unwrap_or(0)
}

fn seed_split_fired(g: &Graph, q: &str) -> bool {
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (_, trace) = engram_observe::with_trace(|| rows(g, q));
    g.set_exec(None);
    trace
        .counters()
        .get("interp.seed scan driven in parallel")
        .copied()
        .unwrap_or(0)
        > 0
}

fn agree(g: &Graph, q: &str) -> Rows {
    let (serial, threaded, inline) = all_three(g, q);
    assert_eq!(
        serial, inline,
        "the inline SerialExec must reproduce the serial path exactly: {q}"
    );
    assert_eq!(
        serial, threaded,
        "the THREADED split must reproduce the serial path exactly, row order \
         included: {q}"
    );
    serial
}

/// Diagnostic: which counters does a bare scan actually fire?
#[test]
#[ignore = "diagnostic, not an assertion"]
fn which_path_does_a_bare_scan_take() {
    let g = graph();
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    for q in [
        "MATCH (p:P) RETURN p.id",
        "MATCH (a:P)-[r:R]->(b:P) RETURN a.id, b.id, r.w",
        "MATCH (a:P)-[r:R]->(b:P) RETURN a.id, b.id, r.w ORDER BY a.id",
        "MATCH (a:P)-[r:R*1..1]->(b:P) RETURN a.id, b.id",
    ] {
        let (_, trace) = engram_observe::with_trace(|| rows(&g, q));
        let c = trace.counters();
        println!("--- {q}");
        for k in [
            "interp.seed scan driven in parallel",
            "interp.expand parallel",
            "interp.pipeline fold parallel",
            "interp.pipeline optional fold parallel",
            "interp.stage rows driven in parallel",
            "interp.pipeline anchored seed scanned the whole label",
        ] {
            println!("   {:>6}  {k}", c.get(k).copied().unwrap_or(0));
        }
    }
    g.set_exec(None);
}

#[test]
fn a_plain_label_projection_has_no_scan_to_split() {
    // MEASURED, not assumed. A bare label projection never reaches a seed
    // scan at all: it is served from the property-column cache
    // (`interp.columnar projection emitted its rows from the columns`), and
    // NO parallel counter fires -- not this split, not the pipeline's.
    //
    // That is correct, and this test says so rather than demanding the split
    // engage. There is no per-seed work here to spread: the rows are read out
    // of a column. Splitting it would add morsel bookkeeping to a memcpy.
    let g = graph();
    let q = "MATCH (p:P) RETURN p.id ORDER BY p.id";
    let out = agree(&g, q);
    assert_eq!(out.len(), 60);
    assert!(
        !seed_split_fired(&g, q),
        "a columnar projection has no seed scan to split"
    );
}

#[test]
fn a_scan_with_expansion_splits_and_agrees() {
    // The shape bi17 and bi12 have: one stage, one driving row, all the cost
    // inside the seeds' expansion.
    let g = graph();
    let q = "MATCH (a:P)-[r:R]->(b:P) RETURN a.id, b.id, r.w ORDER BY a.id, b.id";
    let out = agree(&g, q);
    assert_eq!(out.len(), 180, "60 seeds x 3 edges");
    assert!(seed_split_fired(&g, q), "the expansion case MUST engage");
}

#[test]
fn the_pipelines_own_anchored_seed_scan_is_STILL_SERIAL() {
    // A GAP, pinned deliberately as a gap.
    //
    // This shape is taken by the VECTORISED PIPELINE, which the split added in
    // `interp.rs` never reaches -- so it fires
    // `interp.pipeline anchored seed scanned the whole label` and NO parallel
    // counter at all. The whole label is scanned on one core.
    //
    // It is asserted rather than left in a doc because the alternative was to
    // delete the case, and a deleted case is a gap nobody rediscovers. When
    // the pipeline learns to split its anchored scan this test FAILS, and the
    // person who fixed it is the right person to retire it.
    //
    // Agreement is still checked, so the case earns its keep either way.
    let g = graph();
    let q = "MATCH (p:P)-[:R]->(q:P) RETURN p.grp, count(*) AS n ORDER BY p.grp";
    let out = agree(&g, q);
    assert_eq!(out.len(), 5);
    assert_eq!(
        counter(
            &g,
            q,
            "interp.pipeline anchored seed scanned the whole label"
        ),
        1,
        "this shape is the pipeline's, not the interpreter's"
    );
    assert!(
        !seed_split_fired(&g, q),
        "KNOWN GAP: the pipeline's anchored seed scan does not split.          If this now fires, the gap is closed -- delete this test."
    );
}

#[test]
fn an_unordered_projection_keeps_its_serial_row_order() {
    // No ORDER BY, so nothing re-sorts the result and the merge discipline is
    // the ONLY thing keeping the order stable. If morsels were drained out of
    // slice order this is the test that would catch it.
    let g = graph();
    // A VARIABLE-LENGTH hop, because that is what makes the vectorised
    // pipeline decline and hands the query to the interpreter where the split
    // lives. `ORDER BY` does the same -- which is the pair of reasons bi17
    // reaches this path -- but an ORDER BY would re-sort the result and hide
    // exactly the defect this case exists to catch.
    let q = "MATCH (a:P)-[r:R*1..1]->(b:P) RETURN a.id, b.id";
    let out = agree(&g, q);
    assert_eq!(out.len(), 180);
    assert!(
        seed_split_fired(&g, q),
        "the order contract is only under test if the split actually ran"
    );
}

#[test]
fn a_scan_below_the_threshold_does_NOT_split() {
    // Under `parallel_min_rows` the serial loop runs, and the counter must say
    // so. Without this the suite could not tell "agreed because both arms were
    // serial" from "agreed because the split is correct".
    let g = graph();
    let q = "MATCH (p:P) WHERE p.id < 3 RETURN p.id ORDER BY p.id";
    g.set_parallel_min_rows(1_000_000);
    // A WIDE executor, deliberately: asserting "did not split" under an
    // executor that cannot split proves nothing about the threshold.
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (_, trace) = engram_observe::with_trace(|| rows(&g, q));
    g.set_exec(None);
    assert_eq!(
        trace
            .counters()
            .get("interp.seed scan driven in parallel")
            .copied()
            .unwrap_or(0),
        0,
        "below the threshold the scan must stay serial"
    );
}

#[test]
fn a_limit_keeps_the_scan_serial() {
    // A `LIMIT` over a bare label scan sets `seed_cap`, and such a drive can
    // end EARLY — a worker cannot see that another morsel already filled it.
    // `drive_stage_rows` excludes the same case for the same reason.
    let g = graph();
    let q = "MATCH (p:P) RETURN p.id LIMIT 5";
    let out = agree(&g, q);
    assert_eq!(out.len(), 5);
    assert!(
        !seed_split_fired(&g, q),
        "a capped scan must NOT split: the cap can end the drive early"
    );
}

#[test]
fn a_write_statement_keeps_the_scan_serial() {
    // THE INVARIANT, not a precaution. `scoped_exec`'s module docs record that
    // a transaction's overlays and its OCC read-set are THREAD-LOCAL, so a
    // worker would read a stale graph and its reads would never reach the
    // read-set. A write runs inside the server's single-statement wrapper, so
    // `in_txn()` is true and the split must decline.
    let g = graph();
    g.set_parallel_min_rows(2);
    // Wide, for the same reason the threshold test is wide.
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (_, trace) = engram_observe::with_trace(|| {
        ddl(&g, "MATCH (p:P) SET p.touched = true");
    });
    g.set_exec(None);
    assert_eq!(
        trace
            .counters()
            .get("interp.seed scan driven in parallel")
            .copied()
            .unwrap_or(0),
        0,
        "a writing statement must never split its seed scan"
    );
}

#[test]
fn an_error_in_a_morsel_reaches_the_caller() {
    // A worker's failure must not be swallowed with its buffer. Division by a
    // property that is zero for exactly one seed fails inside ONE morsel.
    let g = graph();
    ddl(&g, "MATCH (p:P) WHERE p.id = 7 SET p.z = 0");
    g.set_parallel_min_rows(2);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let s = parse_statement("MATCH (p:P) RETURN 1 / p.z").expect("parses");
    let got = run_query(&g, &s, BTreeMap::new());
    g.set_exec(None);
    assert!(
        got.is_err(),
        "a morsel's error must surface, not vanish with its buffer"
    );
}

/// A handful of seeds splits when each walks SEVERAL hops (`heavy_seeds`):
/// SNB BI bi4's prefix walked 111 countries out to ~45k memberships each, one
/// core for the whole stage, because 111 seeds sat under the split floor. A
/// one-hop path's handful stays under it — splitting cheap seeds costs more
/// than it saves. The general path's own seed driver (columnar off), a floor
/// far above the seed count, and the serial answer as the control.
#[test]
fn a_few_seeds_walking_several_hops_split_below_the_floor() {
    let g = graph();
    g.set_columnar_scans(false);
    let heavy = "MATCH (a:P)-[:R]->(b:P)-[:R]->(c:P) WHERE a.grp = 1 \
                 RETURN a.id, b.id, c.id ORDER BY a.id, b.id, c.id";
    let light = "MATCH (a:P)-[:R]->(b:P) WHERE a.grp = 1 RETURN a.id, b.id ORDER BY a.id, b.id";
    g.set_exec(None);
    let (serial_heavy, serial_light) = (rows(&g, heavy), rows(&g, light));
    g.set_parallel_min_rows(1_000_000);
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (split_heavy, th) = engram_observe::with_trace(|| rows(&g, heavy));
    let (split_light, tl) = engram_observe::with_trace(|| rows(&g, light));
    g.set_exec(None);
    g.set_parallel_min_rows(256);
    g.set_columnar_scans(true);
    let fired = |t: &engram_observe::Trace| {
        t.counters()
            .get("interp.seed scan driven in parallel")
            .copied()
            .unwrap_or(0)
            > 0
    };
    assert!(!serial_heavy.is_empty() && !serial_light.is_empty(), "vacuous");
    assert_eq!(split_heavy, serial_heavy, "the split changed the answer");
    assert_eq!(split_light, serial_light);
    assert!(fired(&th), "a multi-hop path's few seeds must split");
    assert!(!fired(&tl), "a one-hop path's few seeds stay under the floor");
}
