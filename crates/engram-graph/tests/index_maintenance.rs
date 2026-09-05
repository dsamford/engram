//! Is the range index MAINTAINED across writes, or rebuilt from scratch?
//!
//! An index that must be rebuilt after every write is not an index. It is a
//! cache with a very expensive miss, and a workload that interleaves reads and
//! writes misses on every single read.
//!
//! # What the stress harness measured
//!
//! Against a 100k-node LDBC SNB corpus over Bolt, adding **5%** writes to a
//! read workload cost 14x the throughput:
//!
//! | profile | ops/s |
//! |---|---|
//! | read-only | 620 |
//! | read-heavy (5% writes) | 43 |
//! | balanced (50% writes) | 15 |
//!
//! The per-shape breakdown named the mechanism. `is1-profile` — a single
//! indexed point lookup, 0.13 ms in the read-only profile — kept a fast p50 of
//! 0.15 ms but grew a **p95 of 107 ms** once 5% of operations were writes.
//! Most reads still hit the cached index; the ones that follow a write pay a
//! full rebuild over every node carrying the property. That bimodal
//! p50-fast/p95-catastrophic shape is what a rebuild looks like from outside.
//!
//! # Two distinct facts, pinned separately below
//!
//! 1. **A write triggers a whole-index rebuild.** `ensure_range_index` compares
//!    the index's build clock against the clock at which its property was last
//!    written, and on any difference discards it and calls `RangeIndex::build`,
//!    which re-scans the partition and re-sorts. There is no insert path.
//!
//! 2. **The index ignores the label it was declared on.** `CREATE INDEX ... FOR
//!    (n:Person) ON (n.id)` builds `IndexDef::new(token, PropertyId(token))` —
//!    keyed on the PROPERTY token alone. So `Person.id` and `Message.id` are
//!    one shared index over every node carrying `id`, and creating a `Message`
//!    invalidates lookups on `Person`. In the SNB corpus both labels carry
//!    `id`, which is why the effect is so large there and invisible in a
//!    single-label fixture.
//!
//! Fact 2 makes fact 1 worse, but fact 1 is the defect: even scoped perfectly
//! to one label, rebuilding on every write would still collapse a mixed
//! workload.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn indexed_graph(n: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX person_id FOR (n:Person) ON (n.id)").expect("parse index"),
        BTreeMap::new(),
    )
    .expect("create index");
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:Person {{id: i}})", n - 1),
    );
    g
}

/// Count `graph.range index builds` over a body.
///
/// The counter, not the clock. An earlier diagnosis on this same subsystem was
/// reasoned from a doc comment and was wrong; the counters settled it then and
/// are the instrument of record here.
fn builds_during(f: impl FnOnce()) -> u64 {
    let ((), trace) = engram_observe::with_trace(f);
    trace
        .counters()
        .get("graph.range index builds")
        .copied()
        .unwrap_or(0)
}

/// Interleaving writes with reads must not cost one index rebuild per write.
///
/// # Status
///
/// FAILING as written — this pins an unfixed defect. Committed failing-visible
/// so the number can only move down.
#[test]
fn a_read_after_a_write_does_not_rebuild_the_whole_index() {
    let g = indexed_graph(5_000);
    // Warm: the first lookup builds the index, which is not the thing measured.
    run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");

    const ROUNDS: u64 = 20;
    let builds = builds_during(|| {
        for i in 0..ROUNDS {
            // A write to the SAME label and the SAME indexed property — the
            // honest hard case, and the one the SNB insert performs.
            run(&g, &format!("CREATE (:Person {{id: {}}})", 100_000 + i));
            run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
        }
    });

    eprintln!("{ROUNDS} write+read rounds caused {builds} full index rebuild(s)");

    // One rebuild is defensible (a fold of accumulated deltas); one PER WRITE
    // is the defect. The bound sits between the two so it cannot be satisfied
    // by a partial improvement that still rebuilds linearly.
    let ceiling = ROUNDS / 4;
    assert!(
        builds <= ceiling,
        "{builds} full index rebuilds for {ROUNDS} writes (ceiling {ceiling}) — the range \
         index is discarded and rebuilt by re-scanning the partition on every write, so \
         every read that follows a write pays a whole-corpus scan. `RangeIndex` is a \
         sorted Vec of (key, body); it needs an insert/delete path so a write maintains \
         it instead of invalidating it."
    );
}

/// The rebuild cost must not scale with the corpus.
///
/// The counter test above says rebuilds happen; this says what they cost, and
/// is the one that maps directly onto the p95 the stress harness saw. If a
/// rebuild is a full partition scan then a 4x larger corpus makes the read
/// following a write ~4x more expensive, while the read itself is a point
/// lookup whose cost should not move at all.
#[test]
fn report_rebuild_cost_against_corpus_size() {
    let measure = |n: i64| -> f64 {
        let g = indexed_graph(n);
        run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
        let t = std::time::Instant::now();
        for i in 0..20 {
            run(&g, &format!("CREATE (:Person {{id: {}}})", 900_000 + i));
            run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
        }
        t.elapsed().as_secs_f64() / 20.0
    };
    let small = measure(2_500);
    let large = measure(10_000);
    let ratio = large / small.max(f64::MIN_POSITIVE);
    eprintln!(
        "write+point-read round: 2.5k corpus {:.3} ms, 10k corpus {:.3} ms, ratio {ratio:.1}x",
        small * 1e3,
        large * 1e3
    );
    assert!(small > 0.0, "both corpus sizes must be measured");
}

/// A write to one label must not invalidate an index used by another.
///
/// This is the SNB shape exactly: `Person.id` and `Message.id`, one property
/// name, two labels. The declared index says `FOR (n:Person)`, so a `Message`
/// write is outside it by every reading of that declaration.
///
/// # Status
///
/// FAILING as written — `IndexDef` is keyed on the property token alone and
/// the label in the `CREATE INDEX` statement is discarded.
#[test]
fn a_write_to_a_different_label_does_not_invalidate_this_index() {
    let g = indexed_graph(5_000);
    run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");

    const ROUNDS: u64 = 20;
    let builds = builds_during(|| {
        for i in 0..ROUNDS {
            // A :Message, NOT a :Person. The index is declared FOR (n:Person).
            run(&g, &format!("CREATE (:Message {{id: {}}})", 200_000 + i));
            run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
        }
    });

    eprintln!("{ROUNDS} foreign-label write+read rounds caused {builds} rebuild(s)");

    assert!(
        builds <= ROUNDS / 4,
        "writing {ROUNDS} :Message nodes caused {builds} rebuilds of an index declared \
         FOR (n:Person) ON (n.id). `CREATE INDEX ... FOR (n:Label)` discards the label: \
         `IndexDef::new(token, PropertyId(token))` is keyed on the property token alone, \
         so every label sharing a property name shares one index and every write to any \
         of them invalidates it for all of them."
    );
}

/// Control: with NO writes, repeated reads must reuse the index.
///
/// Without this the two tests above could pass or fail for reasons unrelated to
/// writes — a counter that never fires, a trace that was not installed, a
/// lookup that never touches the index at all. This is the canary on the
/// instrument: it asserts the measurement apparatus can see a cached index, so
/// a zero above means "no rebuild" rather than "no observation".
#[test]
fn the_no_write_control_reuses_the_index() {
    let g = indexed_graph(5_000);
    run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    let builds = builds_during(|| {
        for i in 0..20 {
            run(&g, &format!("MATCH (p:Person {{id: {i}}}) RETURN p.id"));
        }
    });
    eprintln!("20 reads with no writes caused {builds} rebuild(s)");
    assert_eq!(
        builds, 0,
        "20 reads with NO writes between them caused {builds} index rebuilds — the \
         cache is not serving even in the quiet case, so the numbers in the tests \
         above are not about write invalidation"
    );
}

/// The A/B toggle must really revert the behaviour.
///
/// `portserve`'s `ENGRAM_BENCH_BASELINE` arm exists so a before/after can be
/// measured on ONE host in ONE run. That is only worth anything if the switch
/// actually turns the fix off — otherwise the "baseline" arm is the fixed
/// engine wearing a label, both arms measure the same thing, and the comparison
/// reports whatever the noise happened to be.
///
/// So: with maintenance off, the rebuild-per-write behaviour must come back.
#[test]
fn the_baseline_toggle_restores_rebuild_per_write() {
    let g = indexed_graph(5_000);
    g.set_incremental_caches(false);
    run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");

    const ROUNDS: u64 = 20;
    let builds = builds_during(|| {
        for i in 0..ROUNDS {
            run(&g, &format!("CREATE (:Person {{id: {}}})", 300_000 + i));
            run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
        }
    });
    eprintln!("with incremental caches OFF, {ROUNDS} rounds caused {builds} rebuild(s)");
    assert!(
        builds >= ROUNDS,
        "with incremental caches OFF only {builds} rebuilds happened for {ROUNDS} writes — \
         the toggle does not restore the pre-fix behaviour, so the benchmark's baseline arm \
         is not a baseline and any before/after measured with it is meaningless"
    );
}

/// Canary on the counter itself: a cold graph MUST record a build.
///
/// `builds_during` returning 0 is the assertion the two pins above rest on. If
/// the counter were renamed, or the trace not installed, or the lookup routed
/// somewhere that never builds an index, every test in this file would pass
/// while measuring nothing. This one fails in exactly that case.
#[test]
fn the_counter_fires_when_an_index_is_actually_built() {
    let g = indexed_graph(1_000);
    let builds = builds_during(|| {
        run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    });
    assert!(
        builds >= 1,
        "the first indexed lookup on a cold graph recorded {builds} index builds — the \
         `graph.range index builds` counter is not being observed, so every other \
         assertion in this file is vacuous"
    );
}
