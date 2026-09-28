//! The graph algorithm layer: determinism first, then each algorithm.
//!
//! **THE LOAD-BEARING TEST IS THE FIRST ONE.** A parallel run must be
//! BIT-IDENTICAL to a serial one, not merely close — because the convergence
//! delta feeds the loop condition, so a regrouped floating-point sum changes
//! how many iterations run, and a result that depends on how many threads
//! happened to be available cannot be replayed from a seed.
//!
//! It is easy to break later. Someone adding a `+=` into a shared accumulator
//! reintroduces order-dependence with no visible symptom, which is why this
//! runs in `cargo test` rather than living in a benchmark.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::Value;
use engram_graph::algo::{AlgoConfig, AlgoValues, Algorithm, ProjectionKey};
use engram_graph::{Dir, Graph, ScopedExec};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Drop the parallel floor so these tests can split a corpus small enough
    // to build in a unit test. The floor is a COST decision measured at
    // 65,536 vertices (see `Graph::algo_min_vertices`); building that many
    // here would trade seconds per test for nothing, since the answers are
    // identical either side of it. Settable for the same reason
    // `parallel_min_rows` is: behaviour at a threshold must be testable
    // without a corpus at the threshold.
    g.set_algo_min_vertices(1);
    g
}

fn node(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["N".into()], &m).expect("node")
}

fn edge(g: &Graph, a: u64, b: u64) {
    g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");
}

fn cfg() -> AlgoConfig {
    AlgoConfig {
        projection: ProjectionKey {
            labels: vec!["N".into()],
            types: vec!["R".into()],
            dir: Dir::Out,
            weight: None,
        },
        ..AlgoConfig::default()
    }
}

/// A width-N executor that runs morsels on REAL THREADS, work-stealing, in
/// whatever order the OS hands them out.
///
/// The same shape as `parallel_expand.rs`'s and as the server's production
/// implementor: tests may spawn, the engine may not. Threads and not an
/// inline loop, deliberately — an inline executor makes this file prove only
/// that the morsel ARITHMETIC is width-invariant, which is true of a driver
/// that never parallelises at all. It was, for one revision. Bit-identity
/// under a nondeterministic completion order is the claim; only real threads
/// can put it at risk, so only real threads can support it.
struct ThreadedExec(usize);

impl ScopedExec for ThreadedExec {
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

/// A width-N executor that runs morsels in DESCENDING index order, inline.
///
/// [`ThreadedExec`] alone is not enough and it is worth saying why, because
/// the obvious reading is that real threads are the stronger test. They are
/// not, here. Each morsel is microseconds of work, so the first worker drains
/// the whole cursor before its siblings are scheduled and the completion
/// order comes out ascending anyway — a merge that appends partials in
/// COMPLETION order rather than morsel order passes `ThreadedExec` every
/// time. It was tried; it passed three runs in a row.
///
/// The trait's contract is that the implementor chooses scheduling, so
/// descending is a legal executor and any correct operator is immune to it.
/// It turns "the merge might be order-dependent" from something a race has to
/// expose into something arithmetic does, on every run, on one thread.
struct ReverseExec(usize);

impl ScopedExec for ReverseExec {
    fn width(&self) -> usize {
        self.0
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        for i in (0..n).rev() {
            f(i);
        }
    }
}

/// A width-N executor that RECORDS what it was asked to do.
///
/// Counted on the test's side and not with `counted!`, deliberately:
/// `engram-observe`'s counters are thread-local and flushed by the statement's
/// own thread, so anything a morsel counted would be recorded onto a worker
/// and dropped. A counter that cannot fire is not evidence. These two atomics
/// are read back by the test that owns them, whichever thread did the work.
struct RecordingExec {
    width: usize,
    calls: AtomicUsize,
    morsels: AtomicUsize,
}

impl RecordingExec {
    fn new(width: usize) -> Self {
        Self {
            width,
            calls: AtomicUsize::new(0),
            morsels: AtomicUsize::new(0),
        }
    }
}

impl ScopedExec for RecordingExec {
    fn width(&self) -> usize {
        self.width
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.morsels.fetch_add(n, Ordering::Relaxed);
        for i in 0..n {
            f(i);
        }
    }
}

fn floats(v: &AlgoValues) -> Vec<f64> {
    match v {
        AlgoValues::Float(f) => f.clone(),
        other => panic!("expected floats, got {other:?}"),
    }
}

fn ids(v: &AlgoValues) -> Vec<u64> {
    match v {
        AlgoValues::Id(i) | AlgoValues::Count(i) => i.clone(),
        other => panic!("expected ids, got {other:?}"),
    }
}

// ─── THE DETERMINISM TEST ──────────────────────────────────────────────────

#[test]
fn pagerank_is_bit_identical_at_every_scoped_exec_width() {
    let g = g();
    // A PRIME node count, so no width divides it evenly and every width
    // produces a different morsel split. The parallel floor is dropped to 1
    // by `g()` — see there for why — because below it the executor is not
    // entered at all and every width collapses to one morsel, which would
    // make this comparison vacuous.
    //
    // That is not hypothetical: the floor was added after this test was
    // written and silently disarmed it. The same failure as the two below,
    // arriving from a third direction — a change elsewhere quietly took the
    // test's inputs out of the range where they discriminate.
    let n: Vec<u64> = (0..4_999).map(|i| node(&g, &format!("n{i}"))).collect();
    // AN IRREGULAR DEGREE DISTRIBUTION, and that is the whole point of this
    // corpus rather than a detail of it.
    //
    // The obvious construction — `i -> a*i+b` twice over a prime modulus — is
    // a pair of BIJECTIONS whenever `a` is invertible, which over a prime it
    // always is. Every vertex then has in-degree and out-degree exactly two,
    // PageRank converges to the uniform vector, and a uniform vector is
    // invariant under permutation. A merge that concatenated the morsels'
    // partials in completion order instead of morsel order passed that
    // corpus at every width, on real threads, bit for bit. It was tried.
    //
    // Degrees from one to five give each vertex a score of its own, so the
    // answer has something to lose when the partials are reassembled wrongly.
    for i in 0..n.len() {
        for k in 0..(i % 5) + 1 {
            edge(&g, n[i], n[(i * i + k * 7 + 1) % n.len()]);
        }
    }
    // Guard the guard: if this ever converges to a uniform vector again, the
    // permutation checks below stop meaning anything and say nothing about
    // it. Assert the corpus discriminates BEFORE trusting what it proves.
    let spread = floats(
        &g.algo_run(Algorithm::PageRank, &cfg(), &ThreadedExec(1))
            .expect("pagerank")
            .values,
    );
    let lo = spread.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = spread.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        hi - lo > 1e-6,
        "the corpus converged to a near-uniform vector ({lo} to {hi}) — a permutation of it \
         is itself, so the width comparisons below would pass on a broken merge",
    );

    let base = floats(
        &g.algo_run(Algorithm::PageRank, &cfg(), &ThreadedExec(1))
            .expect("pagerank")
            .values,
    );
    for width in [2usize, 3, 4, 7, 8, 16] {
        // Both schedulers at every width: real threads for concurrency, and
        // the reverse order for the completion-order dependence threads are
        // too fast to expose reliably.
        for (kind, other) in [
            (
                "threaded",
                floats(
                    &g.algo_run(Algorithm::PageRank, &cfg(), &ThreadedExec(width))
                        .expect("pagerank")
                        .values,
                ),
            ),
            (
                "reverse",
                floats(
                    &g.algo_run(Algorithm::PageRank, &cfg(), &ReverseExec(width))
                        .expect("pagerank")
                        .values,
                ),
            ),
        ] {
            assert_eq!(
                base.len(),
                other.len(),
                "{kind} width {width} changed the length",
            );
            for (i, (a, b)) in base.iter().zip(other.iter()).enumerate() {
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "{kind} width {width} changed vertex {i}: {a} vs {b} — the parallel lane \
                     must be BIT-identical, not merely close",
                );
            }
        }
    }
}

#[test]
fn the_fixpoint_actually_hands_its_morsels_to_the_executor() {
    // THE CANARY FOR THE TEST ABOVE, and the one that was missing.
    //
    // The driver shipped one revision computing its morsel split from
    // `exec.width()` and then evaluating the morsels with a plain iterator
    // chain — `ScopedExec::for_each` was never called. Every width comparison
    // still passed, because varying a width that no work ever reaches proves
    // the partitioning arithmetic is deterministic and nothing else.
    //
    // A differential is only evidence once the path it compares has run. This
    // asserts the seam is crossed, so a green comparison above cannot be a
    // false pass over a lane that does not exist.
    let g = g();
    // Above the size floor, or the driver asks for one morsel per call and
    // the width assertion below is testing the floor rather than the seam.
    let n: Vec<u64> = (0..4_999).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        for k in 0..(i % 5) + 1 {
            edge(&g, n[i], n[(i * i + k * 7 + 1) % n.len()]);
        }
    }

    let exec = RecordingExec::new(4);
    let r = g
        .algo_run(Algorithm::PageRank, &cfg(), &exec)
        .expect("pagerank");
    let iterations = r.iterations;
    assert!(iterations > 0, "the fixpoint ran no iterations");

    let calls = exec.calls.load(Ordering::Relaxed);
    let morsels = exec.morsels.load(Ordering::Relaxed);
    assert_eq!(
        calls, iterations as usize,
        "the fixpoint must reach `for_each` once per iteration: {iterations} iterations \
         produced {calls} calls",
    );
    assert_eq!(
        morsels,
        calls * 4,
        "a width-4 executor over 4,999 vertices must be handed 4 morsels each time, not {morsels} \
         across {calls} calls",
    );
}

#[test]
fn every_algorithm_answers_identically_at_every_width() {
    let g = g();
    let n: Vec<u64> = (0..23).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        edge(&g, n[i], n[(i + 1) % n.len()]);
        edge(&g, n[i], n[(i * 5 + 2) % n.len()]);
    }
    let mut c = cfg();
    c.source = Some(n[0] as i64);

    for alg in [
        Algorithm::Wcc,
        Algorithm::Degree,
        Algorithm::Bfs,
        Algorithm::TriangleCount,
        Algorithm::LocalClustering,
        Algorithm::LabelPropagation,
        Algorithm::Louvain,
    ] {
        let base = g.algo_run(alg, &c, &ThreadedExec(1)).expect("run");
        for width in [2usize, 5, 8] {
            let other = g.algo_run(alg, &c, &ThreadedExec(width)).expect("run");
            let rev = g.algo_run(alg, &c, &ReverseExec(width)).expect("run");
            assert_eq!(
                format!("{:?}", base.values),
                format!("{:?}", other.values),
                "{:?} differed at width {width} under real threads",
                alg,
            );
            assert_eq!(
                format!("{:?}", base.values),
                format!("{:?}", rev.values),
                "{:?} differed at width {width} when morsels ran in reverse order",
                alg,
            );
        }
    }
}

#[test]
fn a_repeated_run_over_an_unchanged_graph_is_identical() {
    // The simplest determinism property, and the one that would catch an
    // iteration order leaking in from a map or a cache.
    let g = g();
    let n: Vec<u64> = (0..15).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        edge(&g, n[i], n[(i * 3 + 1) % n.len()]);
    }
    for alg in [
        Algorithm::PageRank,
        Algorithm::Wcc,
        Algorithm::LabelPropagation,
        Algorithm::Louvain,
    ] {
        let a = g.algo_run(alg, &cfg(), &ThreadedExec(1)).expect("run");
        for _ in 0..5 {
            let b = g.algo_run(alg, &cfg(), &ThreadedExec(1)).expect("run");
            assert_eq!(
                format!("{:?}", a.values),
                format!("{:?}", b.values),
                "{alg:?} was not reproducible",
            );
        }
    }
}

// ─── PageRank ──────────────────────────────────────────────────────────────

#[test]
fn pagerank_over_a_known_graph_matches_the_hand_computed_scores() {
    // A star: three leaves all pointing at one centre. With damping 0.85 and
    // four nodes, every leaf keeps (1-d)/n = 0.0375, and the centre receives
    // all three leaves' mass in full because each has exactly one out-edge.
    //
    // The centre has no out-edge, so its own mass is DANGLING and is
    // redistributed evenly — which is the term most implementations get wrong
    // by dropping, and is why this is checked against arithmetic rather than
    // against a golden file.
    let g = g();
    let centre = node(&g, "centre");
    let leaves: Vec<u64> = (0..3).map(|i| node(&g, &format!("leaf{i}"))).collect();
    for l in &leaves {
        edge(&g, *l, centre);
    }
    let r = g
        .algo_run(Algorithm::PageRank, &cfg(), &ThreadedExec(1))
        .expect("pagerank");
    let scores = floats(&r.values);
    let at = |id: u64| scores[r.ids.binary_search(&id).expect("in projection")];

    assert!(
        at(centre) > at(leaves[0]),
        "the centre must outrank a leaf: {:?}",
        scores,
    );
    for l in &leaves {
        assert!(
            (at(*l) - at(leaves[0])).abs() < 1e-12,
            "the leaves are symmetric and must score identically",
        );
    }
    // Every score is a probability-like quantity: positive and finite.
    for s in &scores {
        assert!(*s > 0.0 && s.is_finite(), "bad score {s} in {scores:?}");
    }
}

#[test]
fn pagerank_reports_that_it_did_not_converge_rather_than_pretending() {
    // An ASYMMETRIC graph, deliberately. A ring is already at its fixed point
    // — every vertex has one in-edge and one out-edge, so the uniform initial
    // vector IS the answer and one iteration converges honestly. Testing the
    // cap needs a graph whose scores actually move.
    let g = g();
    let hub = node(&g, "hub");
    let spokes: Vec<u64> = (0..20).map(|i| node(&g, &format!("s{i}"))).collect();
    for s in &spokes {
        edge(&g, *s, hub);
    }
    edge(&g, hub, spokes[0]);

    let mut c = cfg();
    c.max_iterations = 1;
    c.tolerance = 0.0; // unreachable
    let r = g
        .algo_run(Algorithm::PageRank, &c, &ThreadedExec(1))
        .expect("pagerank");
    assert!(
        !r.converged,
        "one iteration over a graph whose scores move cannot have converged",
    );
    assert_eq!(r.iterations, 1);

    // And the same graph, given room, DOES converge — so the flag reports the
    // run rather than the fixture. The tolerance is loosened deliberately:
    // convergence here is geometric at the damping factor, so 1e-7 needs about
    // ninety-nine iterations and would sit right on the cap. This test is
    // about the FLAG, not about the rate.
    let mut c = cfg();
    c.max_iterations = 100;
    c.tolerance = 1e-4;
    let r = g
        .algo_run(Algorithm::PageRank, &c, &ThreadedExec(1))
        .expect("pagerank");
    assert!(r.converged, "given room it must converge");
}

// ─── Components and communities ────────────────────────────────────────────

#[test]
fn wcc_labels_a_component_by_its_smallest_node_id() {
    // The label must be a property of the COMPONENT, not of whichever vertex
    // the union-find happened to make the root.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let x = node(&g, "x");
    let y = node(&g, "y");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, x, y);

    let r = g
        .algo_run(Algorithm::Wcc, &cfg(), &ThreadedExec(1))
        .expect("wcc");
    let comps = ids(&r.values);
    let at = |id: u64| comps[r.ids.binary_search(&id).expect("in projection")];

    assert_eq!(at(a), at(b), "a and b are connected");
    assert_eq!(at(b), at(c), "b and c are connected");
    assert_eq!(at(x), at(y), "x and y are connected");
    assert_ne!(at(a), at(x), "the two components are separate");
    assert_eq!(at(a), a.min(b).min(c), "labelled by the smallest id");
    assert_eq!(at(x), x.min(y), "labelled by the smallest id");
}

#[test]
fn wcc_follows_an_edge_in_either_direction() {
    // "Weakly" connected: direction does not matter.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    edge(&g, a, b);
    let r = g
        .algo_run(Algorithm::Wcc, &cfg(), &ThreadedExec(1))
        .expect("wcc");
    let comps = ids(&r.values);
    assert_eq!(comps[0], comps[1]);
}

#[test]
fn louvain_finds_two_cliques_joined_by_one_edge() {
    // The textbook case. Two dense groups with a single bridge should come out
    // as two communities, not one.
    let g = g();
    let left: Vec<u64> = (0..5).map(|i| node(&g, &format!("l{i}"))).collect();
    let right: Vec<u64> = (0..5).map(|i| node(&g, &format!("r{i}"))).collect();
    for i in 0..left.len() {
        for j in (i + 1)..left.len() {
            edge(&g, left[i], left[j]);
            edge(&g, right[i], right[j]);
        }
    }
    edge(&g, left[0], right[0]);

    let mut c = cfg();
    c.max_iterations = 10;
    let r = g
        .algo_run(Algorithm::Louvain, &c, &ThreadedExec(1))
        .expect("louvain");
    let comm = ids(&r.values);
    let at = |id: u64| comm[r.ids.binary_search(&id).expect("in projection")];

    for w in left.windows(2) {
        assert_eq!(at(w[0]), at(w[1]), "the left clique must be one community");
    }
    for w in right.windows(2) {
        assert_eq!(at(w[0]), at(w[1]), "the right clique must be one community");
    }
    assert_ne!(
        at(left[0]),
        at(right[0]),
        "one bridge must not merge two cliques",
    );
    let modularity = r
        .extra
        .iter()
        .find(|(k, _)| k == "modularity")
        .map(|(_, v)| v.clone());
    assert!(
        matches!(modularity, Some(Value::Float(q)) if q > 0.0),
        "a real community structure has positive modularity: {modularity:?}",
    );
}

#[test]
fn louvain_is_reproducible_across_twenty_runs() {
    // Its local-moving phase is Gauss-Seidel BY DEFINITION — the gain of
    // moving a vertex depends on where its neighbours are right now — so the
    // sweep order is part of the specification. This is what pins it.
    let g = g();
    let n: Vec<u64> = (0..24).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        for j in (i + 1)..n.len() {
            if i / 8 == j / 8 {
                edge(&g, n[i], n[j]);
            }
        }
    }
    let first = ids(&g
        .algo_run(Algorithm::Louvain, &cfg(), &ThreadedExec(1))
        .expect("louvain")
        .values);
    for _ in 0..20 {
        let again = ids(&g
            .algo_run(Algorithm::Louvain, &cfg(), &ThreadedExec(1))
            .expect("louvain")
            .values);
        assert_eq!(first, again, "louvain must be reproducible");
    }
}

#[test]
fn label_propagation_reaches_the_same_communities_on_every_run() {
    let g = g();
    let n: Vec<u64> = (0..18).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        for j in (i + 1)..n.len() {
            if i / 6 == j / 6 {
                edge(&g, n[i], n[j]);
            }
        }
    }
    let first = ids(&g
        .algo_run(Algorithm::LabelPropagation, &cfg(), &ThreadedExec(1))
        .expect("lpa")
        .values);
    for _ in 0..10 {
        let again = ids(&g
            .algo_run(Algorithm::LabelPropagation, &cfg(), &ThreadedExec(1))
            .expect("lpa")
            .values);
        assert_eq!(first, again);
    }
}

// ─── Paths ─────────────────────────────────────────────────────────────────

#[test]
fn an_unweighted_bfs_reports_hop_counts_and_leaves_the_unreachable_null() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let island = node(&g, "island");
    edge(&g, a, b);
    edge(&g, b, c);

    let mut cf = cfg();
    cf.source = Some(a as i64);
    let r = g
        .algo_run(Algorithm::Bfs, &cf, &ThreadedExec(1))
        .expect("bfs");
    let at = |id: u64| {
        r.values
            .at(r.ids.binary_search(&id).expect("in projection"))
    };

    assert_eq!(at(a), Value::Int(0));
    assert_eq!(at(b), Value::Int(1));
    assert_eq!(at(c), Value::Int(2));
    assert_eq!(
        at(island),
        Value::Null,
        "an unreachable node must be null, not zero",
    );
}

#[test]
fn a_weighted_sssp_finds_the_cheaper_longer_path() {
    // Two hops of weight 1 beat one hop of weight 5. An unweighted BFS would
    // get this wrong, which is the point of having both.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let mut heavy = BTreeMap::new();
    heavy.insert("w".to_string(), Value::Float(5.0));
    g.create_rel(a, "R", c, &heavy).expect("heavy");
    let mut light = BTreeMap::new();
    light.insert("w".to_string(), Value::Float(1.0));
    g.create_rel(a, "R", b, &light).expect("light");
    g.create_rel(b, "R", c, &light).expect("light");

    let mut cf = cfg();
    cf.source = Some(a as i64);
    cf.projection.weight = Some("w".into());
    let r = g
        .algo_run(Algorithm::Sssp, &cf, &ThreadedExec(1))
        .expect("sssp");
    let at = |id: u64| {
        r.values
            .at(r.ids.binary_search(&id).expect("in projection"))
    };
    assert_eq!(at(c), Value::Float(2.0), "the two-hop path is cheaper");
}

#[test]
fn a_negative_relationship_weight_is_refused_by_sssp() {
    // Dijkstra settles a vertex permanently on first pop, which is only sound
    // while no later edge can shorten it. Running it over a negative weight
    // returns a PLAUSIBLE wrong answer, which is worse than refusing.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let mut neg = BTreeMap::new();
    neg.insert("w".to_string(), Value::Float(-1.0));
    g.create_rel(a, "R", b, &neg).expect("rel");

    let mut cf = cfg();
    cf.source = Some(a as i64);
    cf.projection.weight = Some("w".into());
    let e = g
        .algo_run(Algorithm::Sssp, &cf, &ThreadedExec(1))
        .expect_err("must refuse");
    let msg = e.to_string();
    assert!(msg.contains("non-negative"), "must say why: {msg}");
}

#[test]
fn a_traversal_without_a_source_is_refused_by_name() {
    let g = g();
    node(&g, "a");
    let e = g
        .algo_run(Algorithm::Bfs, &cfg(), &ThreadedExec(1))
        .expect_err("must refuse");
    assert!(e.to_string().contains("sourceNode"), "{e}");
}

// ─── Triangles ─────────────────────────────────────────────────────────────

#[test]
fn a_triangle_is_counted_once_for_each_of_its_three_vertices() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let lonely = node(&g, "lonely");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, c, a);

    let r = g
        .algo_run(Algorithm::TriangleCount, &cfg(), &ThreadedExec(1))
        .expect("triangles");
    let counts = ids(&r.values);
    let at = |id: u64| counts[r.ids.binary_search(&id).expect("in projection")];
    assert_eq!((at(a), at(b), at(c)), (1, 1, 1));
    assert_eq!(at(lonely), 0);
}

#[test]
fn a_local_clustering_coefficient_is_one_inside_a_clique_and_zero_outside() {
    let g = g();
    let n: Vec<u64> = (0..4).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        for j in (i + 1)..n.len() {
            edge(&g, n[i], n[j]);
        }
    }
    let alone = node(&g, "alone");
    let r = g
        .algo_run(Algorithm::LocalClustering, &cfg(), &ThreadedExec(1))
        .expect("lcc");
    let c = floats(&r.values);
    let at = |id: u64| c[r.ids.binary_search(&id).expect("in projection")];
    for x in &n {
        assert!(
            (at(*x) - 1.0).abs() < 1e-12,
            "a clique member has coefficient 1"
        );
    }
    assert_eq!(
        at(alone),
        0.0,
        "a vertex with fewer than two neighbours is defined as zero, not NaN",
    );
}

// ─── Degree ────────────────────────────────────────────────────────────────

#[test]
fn degree_counts_edges_and_weighted_degree_sums_weights() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let mut w = BTreeMap::new();
    w.insert("w".to_string(), Value::Float(2.5));
    g.create_rel(a, "R", b, &w).expect("rel");
    g.create_rel(a, "R", c, &w).expect("rel");

    let r = g
        .algo_run(Algorithm::Degree, &cfg(), &ThreadedExec(1))
        .expect("degree");
    let d = floats(&r.values);
    assert_eq!(d[r.ids.binary_search(&a).expect("in")], 2.0);

    let mut cf = cfg();
    cf.projection.weight = Some("w".into());
    let r = g
        .algo_run(Algorithm::Degree, &cf, &ThreadedExec(1))
        .expect("degree");
    let d = floats(&r.values);
    assert_eq!(d[r.ids.binary_search(&a).expect("in")], 5.0);
}

// ─── The write rule ────────────────────────────────────────────────────────

#[test]
fn a_result_carries_the_snapshot_it_was_computed_at() {
    let g = g();
    node(&g, "a");
    let r = g
        .algo_run(Algorithm::Degree, &cfg(), &ThreadedExec(1))
        .expect("degree");
    assert!(r.as_of > 0, "every result must carry its vintage");
}

#[test]
fn a_write_back_lands_through_the_ordinary_write_path() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    edge(&g, a, b);
    let r = g
        .algo_run(Algorithm::Degree, &cfg(), &ThreadedExec(1))
        .expect("degree");
    let (written, committed) = g.algo_write_back(&r, "deg", 10).expect("write");
    assert_eq!(written, 2);
    assert!(
        committed >= r.as_of,
        "the values describe snapshot {} and landed at {committed}; the commit \
         cannot precede the snapshot",
        r.as_of,
    );
    // And the property is readable exactly as any other written property.
    let node = g.node(a).expect("read").expect("exists");
    let Value::Node { props, .. } = node else {
        panic!("expected a node")
    };
    assert_eq!(props.get("deg"), Some(&Value::Float(1.0)));
}
