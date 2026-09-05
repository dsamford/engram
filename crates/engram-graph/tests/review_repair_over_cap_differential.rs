//! REVIEW DIFFERENTIAL for the ADDITIVE repair admission: a change set OVER
//! the 4,096-node cap that the cost model ADMITS (`entries + nodes × 32 <
//! table.len()`) — the path `adjacency_repair_differential.rs` never lands
//! in (its over-cap round is declined by cost) — under random mutations that
//! include relationship deletes, parallel edges, self-loops, detach-deletes
//! and new nodes past the base offsets, repeated so the overlay FOLDS across
//! repairs, on a resident and a paged store. Every node's row in every cached
//! table must equal, entry for entry, a fresh graph's full rebuild over the
//! same store, and the direct walk.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph, SlimAdj};
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

const NODES: u64 = 40_000;
const PER: u64 = 10;
/// Distinct changed source nodes per burst: over the cap, admitted by cost on
/// the 400k-entry `T` tables (`~9k + 8k × 32 ≈ 265k < 400k`).
const CHANGED: usize = 8_000;

struct World {
    nodes: Vec<u64>,
    rels: Vec<(u64, u64, u64, &'static str)>,
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
    // This file's subject is the OVER-THE-CAP repair — 8,000 changed nodes on a
    // 400k-entry table, admitted by the cost model — and since §8 a single-node
    // reader does not drive one: past `ADJ_READER_REPAIR_MAX_ROWS` it declines
    // and walks its own span. That is the new default working, not a fault, and
    // the path is still reached by the maintenance pass and by an opted-out
    // reader, so opting out here is what keeps this file testing the mechanism
    // it names.
    //
    // It also restores the fixture's DYNAMICS, which matter more than the
    // counter: with readers repairing, each round's delta stays inside a
    // repair's reach, and round 1's `refresh_stale_derived` has something it
    // can bring current. With readers declining, two rounds of bursts
    // accumulate until the repair costs more than a rebuild — and §5.3 demoted
    // the pass's rebuild, so the pass correctly defers and the round-1
    // assertion fires. That interaction is real and is recorded in §8 of
    // `docs/write-path-phase0.md`; it is not what this file is for.
    g.set_single_node_stale_walk(false);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let nodes: Vec<u64> = (0..NODES).map(|_| g.create_node(&label, &none).expect("node")).collect();
    let mut rng = Lcg(0xC0FF_EE00_D15E_A5E5);
    let mut rels = Vec::new();
    for &src in &nodes {
        for _ in 0..PER {
            let dst = nodes[rng.below(NODES) as usize];
            let id = g.create_rel(src, "T", dst, &none).expect("rel");
            rels.push((id, src, dst, "T"));
        }
        let dst = nodes[rng.below(NODES) as usize];
        let id = g.create_rel(src, "U", dst, &none).expect("rel");
        rels.push((id, src, dst, "U"));
    }
    let store = g.shared_store();
    store.seal();
    if let Some(dir) = paged {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("paged dir");
        let _cache = store.into_paged(&dir, 64 << 10).expect("into_paged");
    }
    let t = g.type_tokens_peek(&["T".to_string()]).expect("T")[0];
    let u = g.type_tokens_peek(&["U".to_string()]).expect("U")[0];
    (g, World { nodes, rels }, t, u)
}

fn keys(t: u32, u: u32) -> Vec<(Dir, Option<Vec<u32>>)> {
    let mut tu = vec![t, u];
    tu.sort_unstable();
    vec![
        (Dir::Out, Some(vec![t])),
        (Dir::In, Some(vec![t])),
        (Dir::Out, None),
        (Dir::In, None),
        (Dir::In, Some(tu)),
    ]
}

fn read_all_keys(g: &Graph, world: &World, t: u32, u: u32) {
    for (dir, tt) in keys(t, u) {
        let _ = g.adjacent_slim(world.nodes[0], dir, &tt);
    }
}

/// One burst: `CHANGED` distinct source nodes each get ONE mutation of a
/// random kind, plus a sprinkle of detach-deletes and new nodes.
fn burst(g: &Graph, world: &mut World, rng: &mut Lcg, seq: u64) {
    let none = BTreeMap::new();
    let label = vec!["N".to_string()];
    let start = (seq as usize * 7_919) % world.nodes.len();
    for k in 0..CHANGED {
        let src = world.nodes[(start + k) % world.nodes.len()];
        match rng.below(10) {
            0..=3 => {
                // A parallel edge to an existing peer, or a self-loop.
                let dst = if rng.below(4) == 0 {
                    src
                } else {
                    world
                        .rels
                        .iter()
                        .find(|(_, s, _, _)| *s == src)
                        .map(|(_, _, d, _)| *d)
                        .unwrap_or(src)
                };
                let id = g.create_rel(src, "T", dst, &none).expect("create");
                world.rels.push((id, src, dst, "T"));
            }
            4..=6 => {
                // Delete one of this node's out-rels (if any).
                if let Some(i) = world.rels.iter().position(|(_, s, _, _)| *s == src) {
                    let (id, _, _, _) = world.rels.swap_remove(i);
                    g.delete_rel(id).expect("delete");
                } else {
                    let dst = world.nodes[rng.below(world.nodes.len() as u64) as usize];
                    let id = g.create_rel(src, "T", dst, &none).expect("create");
                    world.rels.push((id, src, dst, "T"));
                }
            }
            7..=8 => {
                let dst = world.nodes[rng.below(world.nodes.len() as u64) as usize];
                let id = g.create_rel(src, "U", dst, &none).expect("create U");
                world.rels.push((id, src, dst, "U"));
            }
            _ => {
                // A new node with a relationship each way.
                let n = g.create_node(&label, &none).expect("new node");
                let a = g.create_rel(n, "T", src, &none).expect("rel out");
                let b = g.create_rel(src, "T", n, &none).expect("rel in");
                world.nodes.push(n);
                world.rels.push((a, n, src, "T"));
                world.rels.push((b, src, n, "T"));
            }
        }
    }
    // A few detach-deletes of random nodes (every peer's row changes).
    for _ in 0..25 {
        let i = rng.below(world.nodes.len() as u64) as usize;
        let n = world.nodes.swap_remove(i);
        g.delete_node(n, true).expect("detach delete");
        world.rels.retain(|(_, s, d, _)| *s != n && *d != n);
    }
}

fn check_against_oracles(g: &Graph, world: &World, t: u32, u: u32, round: u64) {
    let fresh = Graph::new(g.shared_store(), Realm(1), Namespace(1));
    fresh.set_degree_table_after(0);
    let mut mismatches = 0usize;
    let mut first: Option<String> = None;
    for (dir, tt) in keys(t, u) {
        for &n in &world.nodes {
            let repaired = sorted(g.adjacent_slim(n, dir, &tt));
            let rebuilt = sorted(fresh.adjacent_slim(n, dir, &tt));
            if repaired != rebuilt {
                mismatches += 1;
                first.get_or_insert_with(|| {
                    format!("round {round}: node {n} {dir:?} {tt:?}: repaired {repaired:?} != rebuilt {rebuilt:?}")
                });
            }
        }
    }
    assert_eq!(mismatches, 0, "round {round}: {mismatches} row(s) differ; first: {}", first.unwrap_or_default());
    g.set_adj_table_max_entries(0);
    for (dir, tt) in keys(t, u) {
        for &n in world.nodes.iter().step_by(13) {
            assert_eq!(
                sorted(g.adjacent_slim(n, dir, &tt)),
                sorted(fresh.adjacent_slim(n, dir, &tt)),
                "round {round}: node {n} {dir:?} {tt:?}: direct walk != fresh rebuild"
            );
        }
    }
    g.set_adj_table_max_entries(64 << 20);
    let total: usize = world.nodes.iter().map(|&n| g.adjacent_slim(n, Dir::Out, &None).len()).sum();
    assert_eq!(total, world.rels.len(), "round {round}: live relationship count drifted");
}

fn run(paged: Option<std::path::PathBuf>, seed: u64) {
    let (g, mut world, t, u) = seeded(paged.clone());
    let mut rng = Lcg(seed);
    read_all_keys(&g, &world, t, u);
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    for round in 0..3u64 {
        burst(&g, &mut world, &mut rng, round);
        let (_, trace) = engram_observe::with_trace(|| {
            if round == 1 {
                let r = g.refresh_stale_derived();
                assert!(r.any(), "round {round}: the refresh brought nothing current: {r:?}");
            } else {
                read_all_keys(&g, &world, t, u);
            }
        });
        for k in [
            "graph.adjacency tables repaired",
            "graph.adjacency tables built",
            "graph.adjacency repair admitted by cost over the node cap",
            "graph.adjacency repair declined by cost",
            "graph.adjacency table overlay folded",
        ] {
            *totals.entry(k).or_default() += count(&trace, k);
        }
        check_against_oracles(&g, &world, t, u, round);
    }
    eprintln!("[over-cap differential] paged={} totals={totals:?}", paged.is_some());
    assert!(
        totals["graph.adjacency repair admitted by cost over the node cap"] >= 2,
        "the over-cap admission path was not exercised: {totals:?}"
    );
    assert!(totals["graph.adjacency tables repaired"] >= 2, "{totals:?}");
    assert!(totals["graph.adjacency table overlay folded"] >= 1, "no fold across repairs: {totals:?}");
    if let Some(dir) = paged {
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn over_the_cap_admitted_repairs_equal_a_fresh_rebuild_resident() {
    run(None, 0x1111_2222_3333_4444);
}

#[test]
fn over_the_cap_admitted_repairs_equal_a_fresh_rebuild_paged() {
    let dir = std::env::temp_dir().join(format!("engram-review-overcap-{}", std::process::id()));
    run(Some(dir), 0x5555_6666_7777_8888);
}
