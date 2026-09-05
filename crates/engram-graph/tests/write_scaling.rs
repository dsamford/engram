//! Does the cost of ONE write depend on how much data is already there?
//!
//! It should not. A `SET` on a node the query already matched is O(1) work
//! against the store plus whatever index maintenance that property actually
//! requires. If the per-write cost grows with the size of the corpus, then a
//! write is doing work proportional to data it did not touch — the classic
//! shape of an index or membership cache being invalidated wholesale and
//! rebuilt on the next read.
//!
//! This test exists because the stress harness measured exactly that over Bolt:
//! a read-modify-write loop on ONE hot node fell from 2,727 ops/s at 500 nodes
//! to 197 ops/s at 8,000 — 16x the corpus for 13.8x less throughput — while
//! plain `CREATE` held at 14,000+ ops/s on the same build. Measuring it
//! in-process removes the network, the client and the server's scheduling from
//! the question and leaves the engine.
//!
//! These are RATIO tests, deliberately. An absolute millisecond figure on a
//! shared dev machine is not reproducible; the ratio between two corpus sizes
//! measured back to back on the same machine is far steadier, and every ratio
//! here is a median of independent repeats because even the ratio moved between
//! 17.7x and 30.1x on single measurements of an unchanged build.
//!
//! Two CONTROLS run alongside, and they are what make the headline number
//! trustworthy rather than a story about a busy laptop: plain `CREATE` at the
//! same two corpus sizes, and repeated writes to the same node. Both are flat
//! (~1.0x). If the machine were simply slow, or the harness wrong, they would
//! move too.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

/// Build a graph of `n` `:Scale` nodes, optionally with an index on the key,
/// then time `writes` read-modify-writes against ONE of them.
fn time_rmw(n: i64, writes: usize, with_index: bool) -> f64 {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    if with_index {
        run_stmt(
            &g,
            &parse_any("CREATE INDEX scale_k FOR (n:Scale) ON (n.k)").expect("parse index"),
            BTreeMap::new(),
        )
        .expect("create index");
    }
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i CREATE (:Scale {{k: i, b: i % 16}})",
            n - 1
        ),
    );

    let t = Instant::now();
    for _ in 0..writes {
        run(
            &g,
            "MATCH (n:Scale {k: 0}) SET n.hits = coalesce(n.hits, 0) + 1",
        );
    }
    t.elapsed().as_secs_f64() / writes as f64
}

/// Median of `reps` independent measurements.
///
/// A single timing on a shared machine is not a measurement — the raw ratio
/// here was observed between 17.7x and 30.1x across consecutive runs of the
/// same unchanged build. A ratchet set loose enough to survive that variance
/// would be too loose to catch a real regression, so the noise is removed
/// rather than tolerated: the median of independent repeats rejects the
/// occasional scheduling outlier that a mean would carry.
fn median_rmw(n: i64, writes: usize, with_index: bool, reps: usize) -> f64 {
    let mut v: Vec<f64> = (0..reps).map(|_| time_rmw(n, writes, with_index)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a timing"));
    v[v.len() / 2]
}

/// Per-write cost must not scale with corpus size. RATCHETED.
///
/// # The defect this pins, and its fix
///
/// A read-modify-write (`MATCH … SET …`) used to cost work proportional to the
/// CORPUS rather than to the one node it touched — ~19x more per operation
/// against a 10x larger corpus, while plain `CREATE` stayed flat.
///
/// The cause was found by counter, not by reading: a trace over 30
/// read-modify-writes showed `graph.range index builds = 30` — **one full index
/// rebuild per write**, each a full partition scan. `ensure_range_index` keyed
/// its cache on the global commit epoch, and any write bumps that epoch, so
/// `SET n.hits = …` invalidated the index on `n.k`.
///
/// (An earlier diagnosis blamed label-membership invalidation, reasoning from a
/// doc comment. The counters disproved it: `membership snapshots built` never
/// fired on this path at all. The membership delta was kept because it is a
/// real improvement, but it was not this.)
///
/// The fix is property-scoped invalidation: record the clock at which each
/// PROPERTY token was last written, and treat an index as current when its own
/// property has not been written since it was built. Correctness rests on every
/// path that writes a property recording it — `create_node`, `set_prop` and
/// `delete_node` all do, and anything missed costs a rebuild rather than a
/// stale answer.
///
/// A second defect of the same family was found later and is fixed in
/// `crates/engram-store/src/index.rs`: the property-scoped check above still
/// REBUILT the index whenever the property itself had been written, so a
/// workload that writes the indexed property — an insert — paid a full rescan
/// per write. `RangeIndex` now carries changes forward over a shared base. See
/// `tests/index_maintenance.rs` and `tests/index_agrees_with_scan.rs`.
///
/// Measured effect on the RATIO this test asserts:
///
/// | | original | property-scoped | + maintained index |
/// |---|---|---|---|
/// | ratio | 18.6x | 7.7x | **~5.0x** |
/// | index builds / 30 writes | 30 | 1 | **0** |
///
/// # Read the ratio, not the milliseconds
///
/// Absolute per-op figures from earlier runs are deliberately NOT reproduced
/// here. They were measured on a machine in an unknown state and did not
/// reproduce later: an A/B with both fixes toggled off returned the same
/// ~0.060 ms/op at 1k that the fixed build gives, so the difference from the
/// numbers once recorded in this comment was the machine, not the code. A
/// figure that cannot be reproduced does not belong in a test's documentation,
/// where the next reader will take it for a baseline and "diagnose" a
/// regression that never happened.
///
/// The ratio is the measurement. It is taken back-to-back on one machine in one
/// run, with two controls beside it, which is what makes it survive a noisy
/// host.
///
/// # Why still a ratchet
///
/// ~5x is a large improvement and not yet flat. What remains is ordinary scan
/// and cache cost, not a rebuild. The bound below locks in the gain; if it is
/// ever reached again the regression is real, and if the residual is fixed the
/// test says so and asks to be tightened again.
#[test]
fn write_cost_scaling_ratchet() {
    let small = median_rmw(1_000, 60, true, 5);
    let large = median_rmw(10_000, 60, true, 5);
    let ratio = large / small.max(f64::MIN_POSITIVE);

    eprintln!(
        "read-modify-write (median of 5): 1k corpus {:.3} ms/op, 10k corpus {:.3} ms/op, ratio {ratio:.1}x",
        small * 1e3,
        large * 1e3
    );

    // Measured 7.7x after the property-scoped invalidation fix (was 18.6x).
    // The ceiling carries headroom for a contended developer machine, where the
    // ratio moved across runs of an unchanged build; a tighter bound needs a
    // pinned single-tenant host. The CONTROLS are the trustworthy half — flat
    // at ~1.0x on the same noisy machine in the same run, which is what makes
    // the residual here a real signal rather than a busy afternoon.
    const RATCHET: f64 = 13.0;
    assert!(
        ratio < RATCHET,
        "read-modify-write now costs {ratio:.1}x more against a 10x larger corpus \
         ({:.3} ms vs {:.3} ms per op), past the {RATCHET}x ratchet. It was already ~19x \
         (any write invalidates every label membership snapshot, so the next MATCH rebuilds \
         the whole label) — it has got WORSE.",
        large * 1e3,
        small * 1e3
    );

    // The other direction: if the membership delta lands, this bound becomes
    // stale and must be tightened in the same commit. Saying so in the output
    // is what makes that happen.
    if ratio < 3.0 {
        eprintln!(
            "NOTE: ratio is now {ratio:.1}x — the membership-invalidation defect appears FIXED. \
             Tighten RATCHET in this test to lock the gain in."
        );
    }
}

/// Isolates WHERE the cost lives: with an index versus without one.
///
/// If the indexed arm degrades and the unindexed arm does not, the cost is
/// index maintenance rather than the store write. Reported rather than
/// asserted on its own, because it is a diagnostic for the assertion above —
/// but the ratio between the two arms is printed so a failure says which half
/// moved.
#[test]
fn report_where_the_write_cost_lives() {
    let idx_small = time_rmw(1_000, 40, true);
    let idx_large = time_rmw(10_000, 40, true);
    let raw_small = time_rmw(1_000, 40, false);
    let raw_large = time_rmw(10_000, 40, false);

    eprintln!(
        "  indexed:  1k {:.3} ms -> 10k {:.3} ms  ({:.1}x)",
        idx_small * 1e3,
        idx_large * 1e3,
        idx_large / idx_small.max(f64::MIN_POSITIVE)
    );
    eprintln!(
        "  no index: 1k {:.3} ms -> 10k {:.3} ms  ({:.1}x)",
        raw_small * 1e3,
        raw_large * 1e3,
        raw_large / raw_small.max(f64::MIN_POSITIVE)
    );

    // The unindexed arm has to scan to find the node, so it is EXPECTED to grow
    // with corpus size — that is a missing index, not a defect. The only thing
    // asserted here is that the diagnostic actually ran on both arms, so this
    // test cannot silently measure nothing.
    assert!(
        idx_small > 0.0 && raw_small > 0.0,
        "both arms must be measured"
    );
}

/// A write must not get slower the more times the same node has been written.
///
/// MVCC keeps versions; if a read walks the whole chain, a hot node degrades
/// linearly in its own update count and a counter becomes quadratic. The stress
/// run's per-second series was flat rather than declining, which argued against
/// this — but "argued against" is not "pinned", and this is cheap to pin.
#[test]
fn write_cost_does_not_scale_with_a_nodes_own_version_count() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "UNWIND range(0, 499) AS i CREATE (:Chain {k: i})");

    let bump = "MATCH (n:Chain {k: 0}) SET n.hits = coalesce(n.hits, 0) + 1";
    // Warm, so the first-touch costs are not attributed to the early window.
    for _ in 0..50 {
        run(&g, bump);
    }
    let t = Instant::now();
    for _ in 0..100 {
        run(&g, bump);
    }
    let early = t.elapsed().as_secs_f64() / 100.0;
    // 400 more updates to the SAME node.
    for _ in 0..400 {
        run(&g, bump);
    }
    let t = Instant::now();
    for _ in 0..100 {
        run(&g, bump);
    }
    let late = t.elapsed().as_secs_f64() / 100.0;

    let ratio = late / early.max(f64::MIN_POSITIVE);
    eprintln!(
        "version chain: early {:.3} ms/op, after 400 more updates {:.3} ms/op, ratio {ratio:.1}x",
        early * 1e3,
        late * 1e3
    );
    assert!(
        ratio < 5.0,
        "writing the same node got {ratio:.1}x slower after 400 more updates to it \
         ({:.3} ms vs {:.3} ms) — the version chain is being walked, so a counter is quadratic",
        late * 1e3,
        early * 1e3
    );
}

/// Control: the same measurement over plain `CREATE`, which the stress run
/// showed holding at 14,000+ ops/s regardless of corpus size.
///
/// Without this the test above could fail for a reason that has nothing to do
/// with writes — a slow machine, a noisy neighbour — and the reader would have
/// no way to tell. If this control also degrades, the problem is the harness.
#[test]
fn the_create_control_stays_flat() {
    let time_create = |n: i64| -> f64 {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        run(
            &g,
            &format!("UNWIND range(0, {}) AS i CREATE (:Ctl {{k: i}})", n - 1),
        );
        let t = Instant::now();
        for i in 0..60 {
            run(&g, &format!("CREATE (:CtlW {{s: {i}}})"));
        }
        t.elapsed().as_secs_f64() / 60.0
    };
    let small = time_create(1_000);
    let large = time_create(10_000);
    let ratio = large / small.max(f64::MIN_POSITIVE);
    eprintln!(
        "CREATE control: 1k {:.3} ms/op, 10k {:.3} ms/op, ratio {ratio:.1}x",
        small * 1e3,
        large * 1e3
    );
    assert!(
        ratio < 8.0,
        "even plain CREATE scales with corpus size ({ratio:.1}x) — the finding is broader \
         than read-modify-write, or the machine is too noisy for this measurement"
    );
    let _ = Value::Null;
}
