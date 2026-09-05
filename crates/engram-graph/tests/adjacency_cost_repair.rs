//! The COST-BASED repair gate: a stale adjacency table is repaired (its
//! changed rows re-read) when the changed set is within the fixed
//! 4,096-node cap — the old rule, still admitted — OR when re-reading it
//! (`log entries + changed nodes × ADJ_REPAIR_SCAN_ROWS`) costs less than
//! the table's own entries, and rebuilt otherwise. The cap alone declined
//! repair on 9,892 changed persons and rescanned a 17M-row span on SF1
//! (`measurements/official-sf1-paged-2026-08-29.md`).
//!
//! Pinned here, with the lever on and off:
//!
//! 1. 5,000 changed nodes on a 1,000,000-entry table REPAIRS (counter
//!    `graph.adjacency tables repaired`), and the repaired table answers
//!    identically to the direct walk. With the lever OFF the same change set
//!    is over the old cap and REBUILDS — the canary for the gate.
//! 2. Changes whose re-read work exceeds the table REBUILD (counter
//!    `graph.adjacency repair declined by cost`, then `... tables built`).
//! 3. Changed rows that are 20% of a 1M-entry table (200,000 rows over
//!    20,000 nodes) REPAIR; changed rows that EXCEED the table REBUILD.
//! 4. The gate is ADDITIVE: a change set under the cap repairs on a table
//!    so small the cost model would decline it — the case every existing
//!    repair test is, and the first cut of the gate broke three of them.
//!
//! The fixtures are built through the write API rather than Cypher so a
//! million relationships take seconds, not minutes, in a debug build.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph, SlimAdj};
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

/// `nodes` nodes, each the source of `per` `T` relationships to random
/// targets: a `(O, [T])` table of `nodes × per` entries. Sealed, as the
/// server serves it. Returns the graph, the ids, and T's token.
fn seeded(nodes: u64, per: u64) -> (Graph, Vec<u64>, u32) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // The first probe admits a table: the gate governs BUILDING only, and
    // these tests are about what happens to a table that exists.
    g.set_degree_table_after(0);
    // A single-node reader no longer repairs a stale table by default — it
    // declines and walks its own span, leaving republication to the
    // maintenance pass (`Graph::set_single_node_stale_walk`). That is a
    // POLICY about who repairs; this file is about the cost model that
    // decides HOW a repair happens once one is asked for, which the pass and
    // an opted-out reader both still use. Turning the policy off is what puts
    // the reader back on the path this file measures — without it every
    // assertion below reads zero, which is a true statement about the new
    // default and no statement at all about the gate.
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let ids: Vec<u64> = (0..nodes)
        .map(|_| g.create_node(&label, &BTreeMap::new()).expect("node"))
        .collect();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let none = BTreeMap::new();
    for &src in &ids {
        for _ in 0..per {
            let dst = ids[(rng.next() % nodes) as usize];
            g.create_rel(src, "T", dst, &none).expect("rel");
        }
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];
    (g, ids, tok)
}

fn out_row(g: &Graph, node: u64, tok: u32) -> Vec<SlimAdj> {
    g.adjacent_slim(node, Dir::Out, &Some(vec![tok]))
}

/// Build (or resolve) the `(O, [T])` and untyped `(O, [])` tables by probing.
fn warm_out_tables(g: &Graph, node: u64, tok: u32) {
    let _ = out_row(g, node, tok);
    let _ = g.adjacent_slim(node, Dir::Out, &None);
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

/// Add one `T` relationship from each of the first `changed` nodes — a
/// changed set of exactly `changed` distinct OUT rows, with no read between.
fn burst(g: &Graph, ids: &[u64], changed: usize, seed: u64) {
    burst_each(g, ids, changed, 1, seed);
}

/// Add `each` `T` relationships from each of the first `changed` nodes —
/// `changed × each` changed OUT rows over `changed` distinct nodes.
fn burst_each(g: &Graph, ids: &[u64], changed: usize, each: usize, seed: u64) {
    let mut rng = Lcg(seed);
    let none = BTreeMap::new();
    for &src in &ids[..changed] {
        for _ in 0..each {
            let dst = ids[(rng.next() % ids.len() as u64) as usize];
            g.create_rel(src, "T", dst, &none).expect("burst rel");
        }
    }
}

/// The repaired table must answer exactly as the direct walk (tables
/// declined) for changed and unchanged nodes alike.
fn agrees_with_direct_walk(g: &Graph, ids: &[u64], tok: u32, sample: &[usize]) {
    for &i in sample {
        let with_table = out_row(g, ids[i], tok);
        g.set_adj_table_max_entries(0);
        let direct = out_row(g, ids[i], tok);
        g.set_adj_table_max_entries(64 << 20);
        let key = |v: &[SlimAdj]| {
            let mut s: Vec<(u64, u64)> = v.iter().map(|e| (e.peer, e.rel)).collect();
            s.sort_unstable();
            s
        };
        assert_eq!(key(&with_table), key(&direct), "node #{i}: table and direct walk disagree");
    }
}

#[test]
fn five_thousand_changed_nodes_on_a_million_entry_table_repair() {
    // 100k nodes × 10 = 1,000,000 entries, average degree 10: the re-read
    // work of 5,000 changed rows is 5,000 × (scan setup + ~10 entries) —
    // well under half the span.
    let (g, ids, tok) = seeded(100_000, 10);
    warm_out_tables(&g, ids[0], tok);
    let table_len = ids.iter().map(|&n| out_row(&g, n, tok).len()).sum::<usize>();
    assert_eq!(table_len, 1_000_000, "the fixture is not the table it claims to be");

    burst(&g, &ids, 5_000, 7);

    let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
    assert_eq!(row.len(), 11, "node 0 gained one relationship");
    assert_eq!(
        count(&trace, "graph.adjacency tables repaired"),
        1,
        "5k changed nodes on a 1M-entry table must REPAIR: {:?}",
        trace.counters()
    );
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "…not rebuild");
    assert_eq!(count(&trace, "graph.adjacency repair declined by cost"), 0);
    // Repaired ≡ rebuilt, on changed nodes, unchanged nodes, and the edge of
    // the changed range.
    agrees_with_direct_walk(&g, &ids, tok, &[0, 1, 2_500, 4_999, 5_000, 50_000, 99_999]);
}

/// The canary: the same change set under the OLD fixed cap is over it
/// (5,000 > 4,096) and rebuilds — which is what the gate exists to stop.
#[test]
fn with_the_lever_off_the_same_changes_rebuild_under_the_node_cap() {
    let (g, ids, tok) = seeded(100_000, 10);
    g.set_adj_cost_repair(false);
    warm_out_tables(&g, ids[0], tok);
    burst(&g, &ids, 5_000, 7);
    let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
    assert_eq!(row.len(), 11);
    assert_eq!(count(&trace, "graph.adjacency repair declined by the node cap"), 1);
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0);
}

/// Changes whose re-read work exceeds the table: 6,000 changed rows over
/// 6,000 nodes (over the cap) on a 100,000-entry table — `6,000 + 6,000 × 32
/// = 198,000 ≥ 100,000` — must REBUILD, and say why. The rebuilt table
/// answers exactly as the direct walk.
#[test]
fn changes_past_half_the_table_rebuild() {
    let (g, ids, tok) = seeded(10_000, 10);
    warm_out_tables(&g, ids[0], tok);
    burst(&g, &ids, 6_000, 11);
    let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
    assert_eq!(row.len(), 11);
    assert_eq!(
        count(&trace, "graph.adjacency repair declined by cost"),
        1,
        "the cost gate must decline: {:?}",
        trace.counters()
    );
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1);
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0);
    agrees_with_direct_walk(&g, &ids, tok, &[0, 5_999, 6_000, 9_999]);
}

/// Under the cap, a small change set repairs on both arms — the gate changes
/// nothing for the case the cap handled.
#[test]
fn a_small_change_set_repairs_on_both_arms() {
    for lever in [true, false] {
        let (g, ids, tok) = seeded(10_000, 10);
        g.set_adj_cost_repair(lever);
        warm_out_tables(&g, ids[0], tok);
        burst(&g, &ids, 100, 3);
        let (_, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
        assert_eq!(count(&trace, "graph.adjacency tables repaired"), 1, "lever={lever}");
        assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "lever={lever}");
    }
}

/// ADDITIVE admission: 100 changed nodes on a 500-entry table. The cost
/// model alone would decline (`100 + 100 × 32 = 3,300 ≥ 500`); the cap
/// admits it, on both arms, as the old rule always did. This is the shape
/// of every fixture in `adjacency_probe_slim*.rs`, where the first cut of
/// the gate — cost model INSTEAD of the cap — declined every repair and
/// left the repaired-table properties those tests guard untested.
#[test]
fn under_the_cap_a_table_too_small_for_the_cost_model_still_repairs() {
    for lever in [true, false] {
        let (g, ids, tok) = seeded(500, 1);
        g.set_adj_cost_repair(lever);
        warm_out_tables(&g, ids[0], tok);
        burst(&g, &ids, 100, 5);
        let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
        assert_eq!(row.len(), 2);
        assert_eq!(
            count(&trace, "graph.adjacency tables repaired"),
            1,
            "lever={lever}: a change set under the cap must repair whatever the table's size: {:?}",
            trace.counters()
        );
        assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "lever={lever}");
        assert_eq!(count(&trace, "graph.adjacency repair declined by cost"), 0, "lever={lever}");
        agrees_with_direct_walk(&g, &ids, tok, &[0, 99, 100, 499]);
    }
}

/// Changed rows that are 20% of a 1,000,000-entry table REPAIR: 200,000
/// changed rows over 20,000 nodes (over the cap, so the cost model decides:
/// `200,000 + 20,000 × 32 = 840,000 < 1,000,000`). The log holds them —
/// `ADJ_LOG_CAP` is 262,144 — and the repaired table answers as the walk.
#[test]
fn changed_rows_at_a_fifth_of_a_million_entry_table_repair() {
    let (g, ids, tok) = seeded(100_000, 10);
    warm_out_tables(&g, ids[0], tok);
    burst_each(&g, &ids, 20_000, 10, 13);
    let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
    assert_eq!(row.len(), 20, "node 0 gained ten relationships");
    assert_eq!(
        count(&trace, "graph.adjacency tables repaired"),
        1,
        "200k changed rows on a 1M-entry table must REPAIR: {:?}",
        trace.counters()
    );
    assert_eq!(count(&trace, "graph.adjacency repair admitted by cost over the node cap"), 1);
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0);
    assert_eq!(count(&trace, "graph.adjacency repair declined by cost"), 0);
    assert_eq!(count(&trace, "derived.change log overflowed"), 0, "the log must hold the burst");
    agrees_with_direct_walk(&g, &ids, tok, &[0, 1, 9_999, 19_999, 20_000, 50_000, 99_999]);
}

/// Changed rows that EXCEED the table REBUILD: 110,000 changed rows over
/// 10,000 nodes on a 100,000-entry table — declined by COST (the log holds
/// 110,000 entries without overflowing, so the decline is the gate's, not
/// the log's), then rebuilt, answering as the walk.
#[test]
fn changed_rows_exceeding_the_table_rebuild() {
    let (g, ids, tok) = seeded(10_000, 10);
    warm_out_tables(&g, ids[0], tok);
    burst_each(&g, &ids, 10_000, 11, 17);
    let (row, trace) = engram_observe::with_trace(|| out_row(&g, ids[0], tok));
    assert_eq!(row.len(), 21);
    assert_eq!(count(&trace, "derived.change log overflowed"), 0, "the log must hold the burst");
    assert_eq!(
        count(&trace, "graph.adjacency repair declined by cost"),
        1,
        "changed rows past the table must be declined by the gate: {:?}",
        trace.counters()
    );
    assert_eq!(count(&trace, "graph.adjacency tables built"), 1);
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0);
    agrees_with_direct_walk(&g, &ids, tok, &[0, 5_000, 9_999]);
}
