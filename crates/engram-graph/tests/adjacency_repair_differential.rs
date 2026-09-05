//! ADVERSARIAL DIFFERENTIAL for the cost-based repair / maintenance-refresh
//! change: after rounds of RANDOM mutation bursts — creates (parallel edges
//! and self-loops included), relationship deletes, detach-deletes of nodes,
//! NEW nodes past the base's offsets, transaction-buffered batches (committed
//! AND rolled back), repair after repair, overlay folds, the maintenance
//! refresh, and both arms of every lever — every node's row in every cached
//! adjacency table (typed, untyped, multi-typed, both sides) must equal,
//! entry for entry, the row a FRESH graph over the same store builds from a
//! full walk, and the row the direct walk (tables declined) returns.
//!
//! Resident and paged (a 64 KiB block cache under a k-way merge with a tail
//! and later resident segments) both run.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph, MembersView, SlimAdj};
use engram_key::{Namespace, Realm};
use engram_store::Store;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const NODES: u64 = 6_000;
const PER: u64 = 4;

/// The live state the test tracks so deletes are valid and rolled-back
/// writes are provably absent.
struct World {
    nodes: Vec<u64>,
    /// `(rel id, src, dst)` of every committed, undeleted relationship.
    rels: Vec<(u64, u64, u64)>,
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

fn sorted(mut v: Vec<SlimAdj>) -> Vec<SlimAdj> {
    v.sort_by_key(|e| (e.type_token, e.peer, e.rel));
    v
}

fn seeded(paged: Option<std::path::PathBuf>) -> (Graph, World, u32, u32) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let nodes: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0xD1CE_F00D_BAAD_5EED);
    let mut rels = Vec::new();
    for &src in &nodes {
        for _ in 0..PER {
            let dst = nodes[rng.below(NODES) as usize];
            let ty = if rng.below(4) == 0 { "U" } else { "T" };
            let id = g.create_rel(src, ty, dst, &none).expect("rel");
            rels.push((id, src, dst));
        }
    }
    let store = g.shared_store();
    store.seal();
    if let Some(dir) = paged {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("paged dir");
        // A cache far smaller than the segment: every rebuild walk faults.
        let _cache = store.into_paged(&dir, 64 << 10).expect("into_paged");
        assert!(store.segment_count() >= 1);
    }
    let t = g.type_tokens_peek(&["T".to_string()]).expect("T")[0];
    let u = g.type_tokens_peek(&["U".to_string()]).expect("U")[0];
    (g, World { nodes, rels }, t, u)
}

/// The (side, type set) tables the differential covers.
fn keys(t: u32, u: u32) -> Vec<(Dir, Option<Vec<u32>>)> {
    let mut tu = vec![t, u];
    tu.sort_unstable();
    vec![
        (Dir::Out, Some(vec![t])),
        (Dir::In, Some(vec![t])),
        (Dir::Out, None),
        (Dir::In, None),
        (Dir::Out, Some(vec![u])),
        (Dir::In, Some(tu)),
    ]
}

/// Touch every key on a few nodes so every table exists and is current.
fn read_all_keys(g: &Graph, world: &World, t: u32, u: u32, rng: &mut Lcg) {
    for (dir, tt) in keys(t, u) {
        for _ in 0..3 {
            let n = world.nodes[rng.below(world.nodes.len() as u64) as usize];
            let _ = g.adjacent_slim(n, dir, &tt);
        }
    }
}

/// One random mutation, applied to the graph and mirrored in `world`.
/// `lo..hi` bounds the source nodes a create picks, so a burst's changed
/// set can be steered onto a range (to force a fold across two repairs).
fn mutate(g: &Graph, world: &mut World, rng: &mut Lcg, lo: usize, hi: usize) {
    let none = BTreeMap::new();
    let label = vec!["N".to_string()];
    // Re-clamped per mutation: a detach-delete above shrinks `nodes`.
    let pick = |rng: &mut Lcg, world: &World| -> u64 {
        let hi = hi.min(world.nodes.len());
        let lo = lo.min(hi.saturating_sub(1));
        world.nodes[lo + rng.below((hi - lo) as u64) as usize]
    };
    match rng.below(100) {
        0..=54 => {
            // Create; every fourth is a deliberate parallel edge or self-loop.
            let src = pick(rng, world);
            let (dst, ty) = match rng.below(4) {
                0 => (src, "T"),
                1 => {
                    let existing = world
                        .rels
                        .iter()
                        .find(|(_, s, _)| *s == src)
                        .map(|(_, _, d)| *d)
                        .unwrap_or(src);
                    (existing, "T")
                }
                2 => (world.nodes[rng.below(world.nodes.len() as u64) as usize], "U"),
                _ => (world.nodes[rng.below(world.nodes.len() as u64) as usize], "T"),
            };
            let id = g.create_rel(src, ty, dst, &none).expect("create_rel");
            world.rels.push((id, src, dst));
        }
        55..=79 => {
            // Delete a random live relationship.
            if !world.rels.is_empty() {
                let i = rng.below(world.rels.len() as u64) as usize;
                let (id, _, _) = world.rels.swap_remove(i);
                g.delete_rel(id).expect("delete_rel");
            }
        }
        80..=89 => {
            // Detach-delete a node (its rows and every peer's row change).
            if world.nodes.len() > 100 {
                let i = rng.below(world.nodes.len() as u64) as usize;
                let n = world.nodes.swap_remove(i);
                g.delete_node(n, true).expect("detach delete");
                world.rels.retain(|(_, s, d)| *s != n && *d != n);
            }
        }
        _ => {
            // A NEW node with a relationship each way — past the base offsets.
            let n = g.create_node(&label, &none).expect("new node");
            let peer = pick(rng, world);
            let a = g.create_rel(n, "T", peer, &none).expect("rel out");
            let b = g.create_rel(peer, "T", n, &none).expect("rel in");
            world.nodes.push(n);
            world.rels.push((a, n, peer));
            world.rels.push((b, peer, n));
        }
    }
}

/// A burst of `n` mutations over sources in `lo..hi`. Every fifth burst is
/// a transaction; every other transaction is ROLLED BACK (and the world is
/// restored), so buffered-then-dropped rows must never surface.
fn burst(g: &Graph, world: &mut World, rng: &mut Lcg, n: usize, lo: usize, hi: usize, seq: usize) {
    let hi = hi.min(world.nodes.len());
    let lo = lo.min(hi.saturating_sub(1));
    if seq % 5 == 4 {
        let snapshot = (world.nodes.clone(), world.rels.clone());
        g.begin_txn().expect("begin");
        for _ in 0..n {
            mutate(g, world, rng, lo, hi);
        }
        if seq % 10 == 9 {
            g.rollback_txn();
            world.nodes = snapshot.0;
            world.rels = snapshot.1;
        } else {
            g.commit_txn().expect("commit");
        }
        return;
    }
    for _ in 0..n {
        mutate(g, world, rng, lo, hi);
    }
}

/// Every node's row for every key from the graph under test, the direct
/// walk on the same graph, and a FRESH graph over the same store — three
/// ways to the same answer, compared entry for entry.
fn check_against_oracles(g: &Graph, world: &World, t: u32, u: u32, round: usize) {
    let fresh = Graph::new(g.shared_store(), Realm(1), Namespace(1));
    fresh.set_degree_table_after(0);
    let ft = fresh.type_tokens_peek(&["T".to_string()]).expect("T")[0];
    let fu = fresh.type_tokens_peek(&["U".to_string()]).expect("U")[0];
    assert_eq!((ft, fu), (t, u), "the fresh graph reads a different catalog");
    let (_, trace) = engram_observe::with_trace(|| {
        for (dir, tt) in keys(t, u) {
            let _ = fresh.adjacent_slim(world.nodes[0], dir, &tt);
        }
    });
    assert!(
        count(&trace, "graph.adjacency tables built") >= 1,
        "round {round}: the fresh graph built no table — the oracle is not a rebuild: {:?}",
        trace.counters()
    );
    let mut checked = 0usize;
    for (dir, tt) in keys(t, u) {
        for &n in &world.nodes {
            let repaired = sorted(g.adjacent_slim(n, dir, &tt));
            let rebuilt = sorted(fresh.adjacent_slim(n, dir, &tt));
            assert_eq!(
                repaired, rebuilt,
                "round {round}: node {n} {dir:?} {tt:?}: repaired table != fresh rebuild"
            );
            checked += 1;
        }
    }
    // The direct walk on the graph under test (tables declined).
    g.set_adj_table_max_entries(0);
    for (dir, tt) in keys(t, u) {
        for &n in world.nodes.iter().step_by(7) {
            let direct = sorted(g.adjacent_slim(n, dir, &tt));
            let rebuilt = sorted(fresh.adjacent_slim(n, dir, &tt));
            assert_eq!(direct, rebuilt, "round {round}: node {n} {dir:?} {tt:?}: direct walk != fresh rebuild");
        }
    }
    g.set_adj_table_max_entries(64 << 20);
    // The world's own bookkeeping: the untyped OUT table's total must be the
    // live relationship count (rolled-back rows absent, deleted rows gone).
    let total: usize = world.nodes.iter().map(|&n| g.adjacent_slim(n, Dir::Out, &None).len()).sum();
    assert_eq!(total, world.rels.len(), "round {round}: live relationship count drifted");
    assert!(checked > 0);
    // Membership too — the batched fold's graph-level differential.
    let mine: Vec<u64> = g.members(Some("N")).expect("members").iter().collect();
    let theirs: Vec<u64> = fresh.members(Some("N")).expect("members").iter().collect();
    assert_eq!(mine, theirs, "round {round}: :N membership differs from a fresh walk");
    let mut want = world.nodes.clone();
    want.sort_unstable();
    assert_eq!(mine, want, "round {round}: :N membership differs from the world");
    let all: Vec<u64> = g.members(None).expect("all").iter().collect();
    assert_eq!(all, want, "round {round}: all-nodes membership differs from the world");
}

fn run_rounds(paged: Option<std::path::PathBuf>, seed: u64) {
    let (g, mut world, t, u) = seeded(paged.clone());
    let mut rng = Lcg(seed);
    read_all_keys(&g, &world, t, u, &mut rng);
    let _ = g.members(Some("N")).expect("members");
    let _ = g.members(None).expect("all");
    let store = g.shared_store();

    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    let mut add = |trace: &engram_observe::Trace| {
        for k in [
            "graph.adjacency tables repaired",
            "graph.adjacency tables built",
            "graph.adjacency repair declined by cost",
            "graph.adjacency repair declined by the node cap",
            "graph.adjacency table overlay folded",
            "graph.membership snapshots caught up",
            "graph.adjacency stale table declined to a single-node reader",
            "graph.adjacency stale table served an unmoved node",
            "derived.members view folded",
            "graph.derived refreshed by maintenance",
        ] {
            *totals.entry(k).or_default() += count(trace, k);
        }
    };

    // Round plan: (mutations, lo, hi, cost lever, scan lever, batch fold, how
    // the read happens: 0 = reader, 1 = maintenance refresh, 2 = no read).
    let plan: Vec<(usize, usize, usize, bool, bool, bool, u8)> = vec![
        (60, 0, 6000, true, true, true, 0),      // small set: repairs (lever on)
        (60, 0, 6000, true, false, false, 1),    // maintenance repairs it
        // Lever OFF, three ranged repairs in a row with a read between each:
        // the overlay accumulates across repairs past 4,096 nodes → FOLD.
        (2500, 0, 2000, false, true, true, 0),
        (2500, 2000, 4000, false, true, true, 1),
        (2500, 4000, 6000, false, true, true, 0),
        (2500, 0, 6000, false, true, true, 1),
        // Lever ON, past the ADDITIVE gate: ~5,000 distinct changed nodes
        // (over the 4,096 cap) whose re-read work — `entries + nodes × 32`
        // — exceeds a ~24k-entry table: declined by cost → rebuild. 3,000
        // mutations used to do it when the cost model replaced the cap; it
        // no longer does, because a set under the cap is always repaired.
        (12_000, 0, 6000, true, true, true, 0),
        (40, 0, 6000, true, true, false, 1),     // maintenance, serial fold arm
        (500, 1000, 2000, false, false, true, 2),// accumulate, no read
        (500, 2000, 4000, false, false, true, 1),// maintenance with the plain walk
        (80, 0, 6000, true, true, true, 0),
        (2500, 0, 6000, false, true, true, 0),   // lever OFF under the cap: repair a big set
        (30, 0, 6000, true, true, true, 0),
    ];
    for (round, (n, lo, hi, cost, scan, batch, how)) in plan.into_iter().enumerate() {
        g.set_adj_cost_repair(cost);
        g.set_scan_resistant_rebuild(scan);
        g.set_members_batch_fold(batch);
        burst(&g, &mut world, &mut rng, n, lo, hi, round);
        if round % 4 == 3 {
            store.seal(); // a fresh RESIDENT segment beside the paged one and the tail
        }
        let (_, trace) = engram_observe::with_trace(|| match how {
            0 => read_all_keys(&g, &world, t, u, &mut rng),
            1 => {
                let r = g.refresh_stale_derived();
                assert!(r.any(), "round {round}: the refresh found nothing stale after a burst: {r:?}");
            }
            _ => {}
        });
        add(&trace);
        if how != 2 {
            check_against_oracles(&g, &world, t, u, round);
        }
    }
    // A final read so the accumulated rounds are resolved through the reader.
    let (_, trace) = engram_observe::with_trace(|| read_all_keys(&g, &world, t, u, &mut rng));
    add(&trace);
    check_against_oracles(&g, &world, t, u, 99);

    eprintln!("[differential] paged={} totals={totals:?}", paged.is_some());
    // Not vacuous: repairs, rebuilds, a cost decline, a cap decline, a fold,
    // and the maintenance path all happened.
    assert!(totals["graph.adjacency tables repaired"] >= 4, "{totals:?}");
    // A READER no longer rebuilds on its query thread. Past
    // `ADJ_READER_REPAIR_MAX_DELTA` it declines the table and walks its own
    // span (§8), so "the reader fell through to a rebuild" is a path this
    // engine deliberately no longer has, and asserting it would pin the
    // behaviour that was removed. What still has to be non-vacuous is that the
    // reader reached the end of the repair options at all — which is what the
    // decline counts.
    //
    // The rebuild path itself is NOT untested by this: the per-round oracle
    // above is a fresh graph over the same store, and it asserts on every
    // round that it DID build and that its table matches the repaired one
    // entry for entry. That is the stronger of the two checks and it is
    // unaffected.
    assert!(
        totals["graph.adjacency stale table declined to a single-node reader"] >= 1,
        "{totals:?}"
    );
    assert!(totals["graph.adjacency repair declined by cost"] >= 1, "{totals:?}");
    assert!(totals["graph.adjacency table overlay folded"] >= 1, "{totals:?}");
    assert!(totals["graph.derived refreshed by maintenance"] >= 2, "{totals:?}");
    assert!(totals["graph.membership snapshots caught up"] >= 1, "{totals:?}");
    if let Some(dir) = paged {
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn random_mutation_bursts_repair_to_exactly_the_rebuilt_table_resident() {
    run_rounds(None, 0x1234_5678_9ABC_DEF1);
}

#[test]
fn random_mutation_bursts_repair_to_exactly_the_rebuilt_table_paged() {
    let dir = std::env::temp_dir().join(format!("engram-repair-diff-{}", std::process::id()));
    run_rounds(Some(dir), 0x0F1E_2D3C_4B5A_6978);
}

/// The batched membership fold against the serial one over RANDOM bases,
/// random pre-existing overlays and random change lists with duplicates,
/// re-joins and re-leaves — 400 trials, every id range compared, plus the
/// invariants `len == iter().count()` and `contains` agreement.
#[test]
fn batched_and_serial_membership_folds_agree_on_random_histories() {
    let mut rng = Lcg(0xBEEF_CAFE_F00D_0001);
    for trial in 0..400 {
        let base_n = rng.below(300) as usize;
        let mut base: Vec<u64> = (0..base_n).map(|_| rng.below(500)).collect();
        base.sort_unstable();
        base.dedup();
        let base = std::sync::Arc::new(base);
        // A pre-existing overlay reached by BOTH folds from the same history.
        let history: Vec<(u64, bool)> = (0..rng.below(200))
            .map(|_| (rng.below(500), rng.below(2) == 0))
            .collect();
        let vb = MembersView::from_base(std::sync::Arc::clone(&base)).apply_batched(history.iter().copied());
        let vs = MembersView::from_base(base).apply_serial(history.iter().copied());
        let changes: Vec<(u64, bool)> = (0..rng.below(400))
            .map(|_| (rng.below(500), rng.below(2) == 0))
            .collect();
        let b = vb.apply_batched(changes.iter().copied());
        let s = vs.apply_serial(changes.iter().copied());
        // Cross arms too: batched over the serial history and vice versa.
        let bs = vs.apply_batched(changes.iter().copied());
        let sb = vb.apply_serial(changes.iter().copied());
        let ib: Vec<u64> = b.iter().collect();
        let is: Vec<u64> = s.iter().collect();
        assert_eq!(ib, is, "trial {trial}: batched != serial\nhistory={history:?}\nchanges={changes:?}");
        assert_eq!(ib, bs.iter().collect::<Vec<_>>(), "trial {trial}: batched-over-serial");
        assert_eq!(ib, sb.iter().collect::<Vec<_>>(), "trial {trial}: serial-over-batched");
        assert_eq!(b.len(), ib.len(), "trial {trial}: len invariant broken (batched)");
        assert_eq!(s.len(), is.len(), "trial {trial}: len invariant broken (serial)");
        let mut expect: Vec<u64> = ib.clone();
        expect.dedup();
        assert_eq!(ib, expect, "trial {trial}: duplicates in the batched view");
        for id in 0..500u64 {
            assert_eq!(b.contains(id), ib.binary_search(&id).is_ok(), "trial {trial}: contains({id})");
        }
    }
}
