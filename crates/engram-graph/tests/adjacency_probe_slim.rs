#![allow(non_snake_case)]
//! `Graph::edge_count_slim` — the sorted-CSR edge-membership probe — must
//! answer EXACTLY what a walk answers, on every path it can take.
//!
//! The probe binary-searches a single-type adjacency row for the far end
//! instead of walking it. A binary search over a row that is not actually
//! sorted does not crash: it returns SOME count, and a wrong count is what a
//! query that legitimately matched differently also returns. So every
//! assertion here is DIFFERENTIAL, against two oracles that share no code with
//! the table:
//!
//!   * `rels_of` — a per-node prefix walk that materialises each relationship
//!     record and dedups by id, and
//!   * `for_each_rel` — one walk of the whole relationship partition, folded
//!     into a `(src, dst) -> multiplicity` map,
//!
//! plus `adjacency_probe`, the existing existence probe, which must agree
//! with `count > 0`. The fixtures are random graphs with PARALLEL edges (so a
//! count above one is reachable), SELF-LOOPS (so the `Both` dedup is
//! observable), three relationship types (so a multi-type token set exists),
//! and are probed in all three directions — resident, after `into_paged`, and
//! after `open_paged_dir`.
//!
//! Which path answered is asserted through the two counters the probe fires,
//! `graph.edge probe binary search` and `graph.edge probe walked` — a parity
//! test whose two arms took the same path proves nothing, and the canary at
//! the end clears the sorted flag to force the walk and requires identical
//! numbers.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// xorshift64* — the workspace has no `rand` dev-dependency, and a test's
/// randomness should be a seed written in the file.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const TYPES: [&str; 3] = ["A", "B", "C"];
const DIRS: [Dir; 3] = [Dir::Out, Dir::In, Dir::Both];

/// One created edge: `(src, type, dst)`.
type Edge = (u64, &'static str, u64);

/// A random multigraph: `nodes` nodes and `edges` edges, of which roughly a
/// tenth are self-loops and a third duplicate an earlier edge (same source,
/// type and destination — a parallel edge). Returns the ids and the edge list.
fn populate(g: &Graph, seed: u64, nodes: usize, edges: usize) -> (Vec<u64>, Vec<Edge>) {
    let mut rng = Rng(seed);
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..nodes)
        .map(|_| g.create_node(&["N".into()], &none).expect("node"))
        .collect();
    let mut list: Vec<Edge> = Vec::with_capacity(edges);
    for _ in 0..edges {
        let roll = rng.below(10);
        let (src, ty, dst) = if roll < 1 {
            let n = ids[rng.below(nodes as u64) as usize];
            (n, TYPES[rng.below(3) as usize], n)
        } else if roll < 4 && !list.is_empty() {
            list[rng.below(list.len() as u64) as usize]
        } else {
            (
                ids[rng.below(nodes as u64) as usize],
                TYPES[rng.below(3) as usize],
                ids[rng.below(nodes as u64) as usize],
            )
        };
        g.create_rel(src, ty, dst, &none).expect("rel");
        list.push((src, ty, dst));
    }
    (ids, list)
}

/// The `(from, to)` pairs to probe: every edge forward (so a count of at
/// least one is reachable in `Out`), every edge reversed (so `In` is), every
/// node against itself (self-loops and the `Both` dedup), and random pairs
/// (most of which have no edge at all).
fn probe_pairs(seed: u64, ids: &[u64], edges: &[Edge]) -> Vec<(u64, u64)> {
    let mut rng = Rng(seed ^ 0xA5A5);
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    for &(s, _, d) in edges {
        pairs.push((s, d));
        pairs.push((d, s));
    }
    for &n in ids {
        pairs.push((n, n));
    }
    for _ in 0..300 {
        pairs.push((
            ids[rng.below(ids.len() as u64) as usize],
            ids[rng.below(ids.len() as u64) as usize],
        ));
    }
    pairs
}

/// The type-name sets a probe is run under: each single type (the
/// binary-search shape), one multi-type set and the untyped set (the walk
/// shapes).
fn type_sets() -> Vec<Option<Vec<String>>> {
    vec![
        Some(vec!["A".into()]),
        Some(vec!["B".into()]),
        Some(vec!["C".into()]),
        Some(vec!["A".into(), "B".into()]),
        None,
    ]
}

/// Oracle 1: `rels_of`, the per-node record walk. Counts the relationships
/// (deduplicated by id, as `rels_of` does) whose other end is `to`.
fn via_rels_of(g: &Graph, from: u64, dir: Dir, names: Option<&[String]>, to: u64) -> u64 {
    g.rels_of(from, dir, names)
        .expect("rels_of")
        .iter()
        .filter(|r| match dir {
            Dir::Out => r.src == from && r.dst == to,
            Dir::In => r.dst == from && r.src == to,
            Dir::Both => (r.src == from && r.dst == to) || (r.dst == from && r.src == to),
        })
        .count() as u64
}

/// Oracle 2: `for_each_rel`, one walk of the relationship partition folded
/// into directed multiplicities.
fn multiplicities(g: &Graph, names: Option<&[String]>) -> BTreeMap<(u64, u64), u64> {
    let mut m: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    g.for_each_rel(names, &mut |r| {
        *m.entry((r.src, r.dst)).or_insert(0) += 1;
        Ok(())
    })
    .expect("for_each_rel");
    m
}

fn via_multiplicities(m: &BTreeMap<(u64, u64), u64>, from: u64, dir: Dir, to: u64) -> u64 {
    let out = m.get(&(from, to)).copied().unwrap_or(0);
    let inc = m.get(&(to, from)).copied().unwrap_or(0);
    match dir {
        Dir::Out => out,
        Dir::In => inc,
        // A self-loop has one row each side; the probe offers it once.
        Dir::Both => {
            if from == to {
                out
            } else {
                out + inc
            }
        }
    }
}

/// What one full sweep observed, so a sweep over a paged store can be
/// compared with the resident one number for number.
#[derive(Default)]
struct Sweep {
    answers: Vec<u64>,
    /// Probes that counted more than one edge — parallel edges reached.
    parallel: usize,
    /// Probes with `from == to` that counted at least one — self-loops reached.
    self_loops: usize,
}

/// Run `edge_count_slim` over every pair, direction and type set, checking
/// it against both oracles and the existence probe at every step.
fn sweep(g: &Graph, pairs: &[(u64, u64)], sets: &[Option<Vec<String>>]) -> Sweep {
    let mut out = Sweep::default();
    for names in sets {
        let names: Option<&[String]> = names.as_deref();
        let tokens = match names {
            Some(n) => g.type_tokens_peek(n),
            None => None,
        };
        assert!(
            names.is_none() || tokens.as_ref().is_some_and(|t| !t.is_empty()),
            "every type in the fixture has been minted"
        );
        let m = multiplicities(g, names);
        for &(from, to) in pairs {
            for dir in DIRS {
                let got = g.edge_count_slim(from, dir, &tokens, to);
                let want_rels = via_rels_of(g, from, dir, names, to);
                let want_walk = via_multiplicities(&m, from, dir, to);
                assert_eq!(
                    got, want_rels,
                    "edge_count_slim({from}, {dir:?}, {names:?}, {to}) = {got}, rels_of says {want_rels}"
                );
                assert_eq!(
                    got, want_walk,
                    "edge_count_slim({from}, {dir:?}, {names:?}, {to}) = {got}, for_each_rel says {want_walk}"
                );
                let exists = g
                    .adjacency_probe(from, dir, names.unwrap_or(&[]), to)
                    .expect("adjacency_probe");
                assert_eq!(
                    exists,
                    got > 0,
                    "adjacency_probe({from}, {dir:?}, {names:?}, {to}) = {exists} but the count is {got}"
                );
                out.answers.push(got);
                if got > 1 {
                    out.parallel += 1;
                }
                if from == to && got > 0 {
                    out.self_loops += 1;
                }
            }
        }
    }
    out
}

/// A sweep that reached nothing interesting proves nothing about it.
fn require_reached(s: &Sweep, what: &str) {
    assert!(
        s.parallel > 0,
        "{what}: no probe counted a parallel edge — the fixture has no multiplicity to test"
    );
    assert!(
        s.self_loops > 0,
        "{what}: no self-loop was counted — the Both dedup was never exercised"
    );
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

const SEARCHED: &str = "graph.edge probe binary search";
const WALKED: &str = "graph.edge probe walked";

/// A graph with its adjacency tables ADMITTED from the first probe, so a
/// typed probe finds a table to search.
fn admitted(seed: u64) -> (Graph, Vec<u64>, Vec<Edge>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, edges) = populate(&g, seed, 120, 700);
    g.set_degree_table_after(0);
    (g, ids, edges)
}

/// The single-type sets only — the shape that binary-searches.
fn single_type_sets() -> Vec<Option<Vec<String>>> {
    type_sets()
        .into_iter()
        .filter(|s| s.as_ref().is_some_and(|v| v.len() == 1))
        .collect()
}

/// The shapes that walk by construction: multi-type and untyped.
fn walk_only_sets() -> Vec<Option<Vec<String>>> {
    type_sets()
        .into_iter()
        .filter(|s| s.as_ref().is_none_or(|v| v.len() != 1))
        .collect()
}

#[test]
fn typed_probes_binary_search_and_agree_with_both_oracles() {
    let (g, ids, edges) = admitted(11);
    let pairs = probe_pairs(11, &ids, &edges);
    let (s, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &single_type_sets()));
    require_reached(&s, "resident typed");
    let searched = counter(&trace, SEARCHED);
    let walked = counter(&trace, WALKED);
    eprintln!(
        "resident typed: {} probes, searched {searched}, walked {walked}",
        s.answers.len()
    );
    assert!(
        searched >= 1,
        "no typed probe took the binary search — the fast path never ran"
    );
    // Admitted from the first probe, `with_adj_table` builds on the miss and
    // serves the same call, so not one typed probe has a reason to walk.
    assert_eq!(
        walked, 0,
        "typed probes walked {walked} times against {searched} searches — the table is not serving them"
    );
}

#[test]
fn untyped_and_multi_type_probes_walk_and_agree() {
    let (g, ids, edges) = admitted(23);
    let pairs = probe_pairs(23, &ids, &edges);
    let (s, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &walk_only_sets()));
    require_reached(&s, "resident walk-only");
    assert_eq!(
        counter(&trace, SEARCHED),
        0,
        "an untyped or multi-type probe binary-searched a row ordered by type first"
    );
    assert!(counter(&trace, WALKED) >= 1, "the walk counter never fired");
}

#[test]
fn before_admission_the_probe_walks_and_agrees() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, edges) = populate(&g, 37, 120, 700);
    g.set_degree_table_after(u64::MAX); // never admit a table build
    let pairs = probe_pairs(37, &ids, &edges);
    let (s, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &type_sets()));
    require_reached(&s, "pre-admission");
    assert_eq!(
        counter(&trace, SEARCHED),
        0,
        "a probe searched a table that was never admitted"
    );
    assert!(counter(&trace, WALKED) >= 1);
    assert_eq!(
        counter(&trace, "graph.adjacency tables built"),
        0,
        "the probe built a table past the admission gate"
    );
}

#[test]
fn the_lever_off_walks_the_admitted_table_and_agrees() {
    let (g, ids, edges) = admitted(41);
    let pairs = probe_pairs(41, &ids, &edges);
    let sets = single_type_sets();
    let (on, _) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    g.set_edge_probe(false);
    let (off, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    assert_eq!(
        counter(&trace, SEARCHED),
        0,
        "the lever is off and a probe still searched"
    );
    assert!(counter(&trace, WALKED) >= 1);
    assert_eq!(on.answers, off.answers, "the lever changed an answer");
    g.set_edge_probe(true);
    let (back, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    assert!(
        counter(&trace, SEARCHED) >= 1,
        "the lever did not come back on"
    );
    assert_eq!(on.answers, back.answers);
}

/// Repaired tables: after the tables are built, edges are added (parallel
/// ones and self-loops among them) and deleted, so every later probe runs
/// against a table carried forward by repair, whose flag was re-established
/// over the re-read rows.
#[test]
fn a_repaired_table_still_binary_searches_and_agrees() {
    let (g, ids, edges) = admitted(53);
    let pairs = probe_pairs(53, &ids, &edges);
    let sets = single_type_sets();
    let (_, build) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    assert!(counter(&build, SEARCHED) >= 1);

    // Writes that change rows the tables cover.
    let none = BTreeMap::new();
    let mut rng = Rng(53 ^ 0x77);
    let mut created = Vec::new();
    for i in 0..40 {
        let s = ids[rng.below(ids.len() as u64) as usize];
        let d = if i % 5 == 0 {
            s
        } else {
            ids[rng.below(ids.len() as u64) as usize]
        };
        let ty = TYPES[rng.below(3) as usize];
        created.push(g.create_rel(s, ty, d, &none).expect("rel"));
        // A parallel twin of the same edge.
        created.push(g.create_rel(s, ty, d, &none).expect("twin"));
    }
    for id in created.iter().step_by(3) {
        g.delete_rel(*id).expect("delete");
    }
    // The original edges are still the fixture's — extend the pairs with the
    // new endpoints so the changed rows are probed directly.
    let mut pairs = pairs;
    for id in &created {
        if let Some(r) = g.rel(*id).expect("rel") {
            pairs.push((r.src, r.dst));
            pairs.push((r.dst, r.src));
        }
    }
    let (s, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    require_reached(&s, "after repair");
    assert!(
        counter(&trace, "graph.adjacency tables repaired") >= 1,
        "no table was repaired — this test did not exercise the repair path: {:?}",
        trace.counters()
    );
    assert_eq!(
        counter(&trace, "graph.adjacency tables built"),
        0,
        "a table was REBUILT rather than repaired, so the repaired flag was never consulted"
    );
    assert!(
        counter(&trace, SEARCHED) >= 1,
        "the repaired table lost its sorted flag and every probe walked"
    );
}

/// Inside a transaction with buffered adjacency rows for the probed node the
/// shared table cannot serve; the probe must WALK, and the walk must see the
/// buffered edges.
#[test]
fn inside_a_transaction_the_probe_walks_and_sees_buffered_edges() {
    let (g, ids, _) = admitted(67);
    let a = ids[3];
    let b = ids[9];
    let tok = g.type_tokens_peek(&["A".into()]);
    let none = BTreeMap::new();
    let before = (
        g.edge_count_slim(a, Dir::Out, &tok, b),
        g.edge_count_slim(b, Dir::In, &tok, a),
        g.edge_count_slim(a, Dir::Both, &tok, b),
    );
    assert_eq!(before.0, before.1, "an O row of a is an I row of b");

    g.begin_txn().expect("begin");
    g.create_rel(a, "A", b, &none).expect("rel");
    g.create_rel(a, "A", b, &none).expect("twin");
    let (mid, trace) = engram_observe::with_trace(|| {
        (
            g.edge_count_slim(a, Dir::Out, &tok, b),
            g.edge_count_slim(b, Dir::In, &tok, a),
            g.edge_count_slim(a, Dir::Both, &tok, b),
        )
    });
    assert_eq!(
        mid.0,
        before.0 + 2,
        "the transaction's own edges are invisible to the probe"
    );
    assert_eq!(
        mid.1,
        before.1 + 2,
        "the I side of the far end does not see the buffered rows"
    );
    assert_eq!(mid.2, before.2 + 2, "Both does not see the buffered rows");
    assert_eq!(
        counter(&trace, SEARCHED),
        0,
        "a probe searched the SHARED table while its node had buffered rows"
    );
    assert!(counter(&trace, WALKED) >= 3);
    g.rollback_txn();

    let (after, trace) = engram_observe::with_trace(|| g.edge_count_slim(a, Dir::Out, &tok, b));
    assert_eq!(after, before.0, "a rolled-back edge is still counted");
    assert!(
        counter(&trace, SEARCHED) >= 1,
        "outside the transaction the probe walked"
    );
}

/// The canary: clear `sorted_by_peer` on every cached table. The probe must
/// then WALK — and the walk must answer exactly what the search answered.
/// If the flag were not consulted, the search counter would keep firing; if
/// the search were wrong, the numbers would differ.
#[test]
fn clearing_the_sorted_flag_forces_the_walk_with_identical_answers() {
    let (g, ids, edges) = admitted(79);
    let pairs = probe_pairs(79, &ids, &edges);
    let sets = single_type_sets();
    let (searched, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    require_reached(&searched, "canary: searched arm");
    assert!(
        counter(&trace, SEARCHED) >= 1,
        "the searched arm never searched"
    );

    let flipped = g.clear_adjacency_sorted_flags();
    assert!(
        flipped >= 6,
        "the canary cleared {flipped} table(s); with three types in two directions there \
         should be at least 6 cached — the canary did not land"
    );

    let (walked, trace) = engram_observe::with_trace(|| sweep(&g, &pairs, &sets));
    assert_eq!(
        counter(&trace, SEARCHED),
        0,
        "the sorted flag is false and a probe still binary-searched — the flag is not consulted"
    );
    assert!(counter(&trace, WALKED) >= 1, "the walked arm never walked");
    assert_eq!(
        counter(&trace, "graph.adjacency tables built"),
        0,
        "the canary's republish was rejected and the table rebuilt sorted behind the test"
    );
    assert_eq!(
        searched.answers, walked.answers,
        "the binary search and the walk disagree on at least one probe"
    );
}

/// Resident answers for a fixture, then the SAME store paged to disk and
/// read through a fresh graph with cold caches. Number for number.
#[test]
fn paged_store_answers_identically_and_binary_searches() {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = Graph::new(Store::new(), realm, ns);
    let (ids, edges) = populate(&g, 97, 120, 700);
    g.set_degree_table_after(0);
    let pairs = probe_pairs(97, &ids, &edges);
    let sets = type_sets();
    let resident = sweep(&g, &pairs, &sets);
    require_reached(&resident, "resident");
    let store = g.shared_store();
    drop(g);

    let dir = std::env::temp_dir().join("engram_edge_probe_into_paged");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 8 * 1024).expect("into_paged");

    let paged = Graph::new(store.clone(), realm, ns);
    paged.set_degree_table_after(0);
    let (got, trace) = engram_observe::with_trace(|| sweep(&paged, &pairs, &sets));
    assert_eq!(resident.answers, got.answers, "paged and resident disagree");
    assert!(
        counter(&trace, "paged.pread") > 0,
        "no paged pread happened — the probes did not hit the paged store"
    );
    assert!(
        counter(&trace, SEARCHED) >= 1,
        "no probe searched a table built over the paged store"
    );
    assert!(counter(&trace, WALKED) >= 1, "the untyped set never walked");

    drop(paged);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The store REOPENED from its segment files (an empty log; every read faults
/// in from disk), through a graph that has never seen the resident data.
#[test]
fn open_paged_dir_answers_identically() {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = Graph::new(Store::new(), realm, ns);
    let (ids, edges) = populate(&g, 113, 120, 700);
    g.set_degree_table_after(0);
    let pairs = probe_pairs(113, &ids, &edges);
    let sets = type_sets();
    let resident = sweep(&g, &pairs, &sets);
    require_reached(&resident, "resident");
    let store = g.shared_store();
    drop(g);

    let dir = std::env::temp_dir().join("engram_edge_probe_open_paged_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _ = store.into_paged(&dir, 1024 * 1024).expect("into_paged");
    drop(store);

    let (reopened, _cache) = Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir");
    let g2 = Graph::new(reopened, realm, ns);
    g2.set_degree_table_after(0);
    let (got, trace) = engram_observe::with_trace(|| sweep(&g2, &pairs, &sets));
    assert_eq!(
        resident.answers, got.answers,
        "reopened and resident disagree"
    );
    assert!(
        counter(&trace, "paged.pread") > 0,
        "the reopened store was never read from disk"
    );
    assert!(counter(&trace, SEARCHED) >= 1);
    assert!(counter(&trace, WALKED) >= 1);

    drop(g2);
    let _ = std::fs::remove_dir_all(&dir);
}
