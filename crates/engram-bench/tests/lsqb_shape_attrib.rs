//! LSQB q4-vs-q7 shape ATTRIBUTION (scratch repro, local-only) — why the two
//! OPTIONAL-MATCH form of the same join runs a class slower than the comma
//! form.
//!
//! The pod measured official SF1: q4 (comma-joined LIKES + REPLY_OF legs)
//! 16,312,503 rows in 6,850 ms; q7 (the SAME base MATCH with those legs as two
//! OPTIONAL MATCH clauses) 26,097,816 in 187,548 ms — 27×. This test loads a
//! small `snbgen` corpus IN-PROCESS and attributes the two shapes with
//! `engram_observe::with_trace` counter tallies, not the wall clock: the
//! recognisers (and `fuse_consecutive_matches`, which deliberately excludes
//! OPTIONAL) claim q4 into one columnar aggregate pass, and the columnar
//! OPTIONAL operator claims q7 as one `left_join_null_extend` round PER
//! OPTIONAL clause feeding the same aggregate tail (`pipeline_optional.rs`,
//! the multi-OPTIONAL section). Both therefore pay a corpus-size-independent
//! handful of point-gets; the per-tuple streaming interp — one projected
//! point-get per row-binding and one adjacency re-probe per tuple, the
//! ic6-class row-at-a-time shape, ~305k gets on the 1000-person corpus — is
//! what a DECLINED q7 used to pay, and this test pins that it no longer does.
//!
//! GATED on `ENGRAM_LSQB_ATTRIB_DIR` (an `snbgen` export dir): without it the
//! test is a no-op, so the suite never depends on a corpus it did not build.
//! Run with:
//!   cargo run -p engram-bench --bin snbgen --release -- <dir> 300 1
//!   ENGRAM_LSQB_ATTRIB_DIR=<dir> cargo test -p engram-bench --release \
//!     --test lsqb_shape_attrib -- --nocapture

// Wall-clock is attribution-only here (ratios), like the sibling scratch
// tests; the engine crates' Instant ban does not apply to a bench test.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The pipeline-operator counters `qcheck` keys on — exactly one of these
/// firing means a columnar recogniser claimed the statement; none firing
/// means the per-tuple streaming interp ran it.
const PIPELINE_COUNTERS: &[&str] = &[
    "interp.pipeline hop runs",
    "interp.pipeline aggregate runs",
    "interp.pipeline multistage runs",
    "interp.pipeline join runs",
    "interp.pipeline multistage-join runs",
    "interp.pipeline ic5 runs",
    "interp.pipeline optional runs",
];

/// The row-at-a-time cost counters this attribution tallies.
const COST_COUNTERS: &[&str] = &[
    "store.gets",
    "store.projected gets",
    "graph.projected node materialisations",
    "graph.projected gets served from columns",
    "graph.adjacency tables reused",
    "store.block probes",
    "store.visitor scans",
    "interp.consecutive matches fused for the recognisers",
];

/// Run `src` under a fresh trace; return the single count value and the
/// tallies of every counter this attribution reads.
fn traced_count(g: &Graph, src: &str) -> (i64, BTreeMap<&'static str, u64>, Option<&'static str>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (res, trace) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
    let res = res.unwrap_or_else(|e| panic!("run `{src}`: {e}"));
    let count = match res.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("`{src}` returned {other:?}, expected one Int"),
    };
    let mut tallies = BTreeMap::new();
    for k in PIPELINE_COUNTERS.iter().chain(COST_COUNTERS) {
        if let Some(v) = trace.counters().get(*k) {
            tallies.insert(*k, *v);
        }
    }
    let fired = PIPELINE_COUNTERS
        .iter()
        .find(|k| trace.counters().get(**k).copied().unwrap_or(0) > 0)
        .copied();
    (count, tallies, fired)
}

/// Load the corpus once, run the q4-shape and the q7-shape (plus the base
/// match and the census counts that size them), print every tally, and pin
/// the attribution: BOTH are claimed by a columnar recogniser with a
/// corpus-size-independent handful of point-gets — q7 by the OPTIONAL
/// operator, whose per-clause null-fill keeps every base row.
#[test]
fn q4_columnar_vs_q7_per_tuple_attribution() {
    let Ok(dir) = std::env::var("ENGRAM_LSQB_ATTRIB_DIR") else {
        eprintln!("[lsqb-attrib] ENGRAM_LSQB_ATTRIB_DIR unset — skipping (see the header)");
        return;
    };
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let stats = engram_bench::load_export(&g, std::path::Path::new(&dir));
    let store = g.shared_store();
    store.seal();
    store.compact();
    eprintln!(
        "[lsqb-attrib] loaded {} nodes, {} rels from {dir}",
        stats.nodes, stats.rels
    );

    // The corpus quantities the row counts derive from.
    for (name, src) in [
        ("messages", "MATCH (m:Message) RETURN count(*) AS c"),
        ("HAS_TAG", "MATCH ()-[:HAS_TAG]->() RETURN count(*) AS c"),
        ("LIKES", "MATCH ()-[:LIKES]->() RETURN count(*) AS c"),
        ("REPLY_OF", "MATCH ()-[:REPLY_OF]->() RETURN count(*) AS c"),
    ] {
        let (n, _, _) = traced_count(&g, src);
        eprintln!("[lsqb-attrib] census {name} = {n}");
    }

    let base = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                RETURN count(*) AS count";
    let opt1 = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) RETURN count(*) AS count";
    let q4 = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person), \
              (message)<-[:LIKES]-(liker:Person), \
              (message)<-[:REPLY_OF]-(comment:Comment) RETURN count(*) AS count";
    let q7 = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
              OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
              OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
              RETURN count(*) AS count";

    // Warm every cache once so the traced runs compare steady state, not
    // build costs (benchmark-first-run-is-cold applies to counters too: the
    // first run tallies adjacency/membership BUILDS the rest never repay).
    for src in [base, opt1, q4, q7] {
        let q = parse_statement(src).expect("parse");
        run_query(&g, &q, BTreeMap::new()).expect("warm run");
    }

    let (base_n, base_t, base_fired) = traced_count(&g, base);
    let (opt1_n, opt1_t, opt1_fired) = traced_count(&g, opt1);
    let (q4_n, q4_t, q4_fired) = traced_count(&g, q4);
    let (q7_n, q7_t, q7_fired) = traced_count(&g, q7);
    for (name, n, fired, t) in [
        ("base", base_n, base_fired, &base_t),
        ("base+1opt", opt1_n, opt1_fired, &opt1_t),
        ("q4-shape", q4_n, q4_fired, &q4_t),
        ("q7-shape", q7_n, q7_fired, &q7_t),
    ] {
        eprintln!(
            "[lsqb-attrib] {name}: count={n} path={} tallies={t:?}",
            fired.unwrap_or("INTERP (per-tuple streaming)")
        );
    }

    // Steady-warm wall clock, LOCAL ONLY — the q7/q4 RATIO attributes the
    // path class (per-tuple vs columnar); the absolute numbers are never
    // publishable (co-located, unpinned, laptop). Best-of-N after the warm
    // runs above so a cold adjacency/membership build never lands in a sample.
    let steady_ms = |src: &str| -> f64 {
        let q = parse_statement(src).expect("parse");
        let mut best = f64::INFINITY;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            run_query(&g, &q, BTreeMap::new()).expect("timed run");
            best = best.min(t0.elapsed().as_secs_f64() * 1e3);
        }
        best
    };
    let q4_ms = steady_ms(q4);
    let q7_ms = steady_ms(q7);
    eprintln!(
        "[lsqb-attrib] steady-warm best-of-5 wall: q4={q4_ms:.1} ms q7={q7_ms:.1} ms ratio q7/q4={:.2}x (local, ratio-only)",
        q7_ms / q4_ms.max(1e-9)
    );

    // ── The attribution, pinned ────────────────────────────────────────────
    // q4: fused (comma or clause fusion) into a recogniser's columnar pass.
    assert!(
        q4_fired.is_some(),
        "q4-shape must be claimed by a columnar recogniser, tallies {q4_t:?}"
    );
    // ONE optional leg is still inside the OPTIONAL operator's class (it
    // fires ALONGSIDE the aggregate operator that serves the tail)…
    assert!(
        opt1_t.get("interp.pipeline optional runs").copied() == Some(1),
        "a single OPTIONAL leg is the pipeline OPTIONAL operator's shape: {opt1_t:?}"
    );
    let _ = opt1_fired;
    // …and so is the SECOND: each OPTIONAL clause is one null-extension round
    // of the same operator (the multi-OPTIONAL admission `pipeline_optional.rs`
    // pins), so the whole statement stays columnar.
    assert!(
        q7_t.get("interp.pipeline optional runs").copied() == Some(1),
        "q7-shape (two OPTIONAL clauses) is the pipeline OPTIONAL operator's shape: {q7_t:?}"
    );
    let _ = q7_fired;
    // The cost class: NEITHER shape pays projected point-gets per row-binding
    // — both columnar passes pay a corpus-size-independent handful (the
    // per-tuple interp paid ~6 gets per q7 result row on this corpus).
    let q4_gets = q4_t.get("store.gets").copied().unwrap_or(0);
    let q7_gets = q7_t.get("store.gets").copied().unwrap_or(0);
    assert!(
        q7_gets <= q4_gets.max(1) * 4,
        "q7's columnar gets ({q7_gets}) must stay in q4's class ({q4_gets}), not per-row"
    );
    assert!(
        (q7_gets as i64) < q7_n,
        "q7's gets ({q7_gets}) must not scale with its result rows ({q7_n})"
    );
    // q7's null-preserving semantics can only add rows over q4's inner join.
    assert!(
        q7_n >= q4_n,
        "q7 keeps every base row (null legs included): {q7_n} vs {q4_n}"
    );
}
