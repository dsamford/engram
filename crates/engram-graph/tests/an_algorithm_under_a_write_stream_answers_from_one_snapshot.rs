//! Graph algorithms while the graph is being written.
//!
//! Every other algorithm test runs on a quiet graph, which is the state no
//! production caller is ever in. The properties this file pins are the ones
//! that only exist under concurrency, and each of them is a claim the layer
//! makes in prose somewhere:
//!
//! - **A run answers from ONE snapshot.** The projection is built at an epoch
//!   and the whole computation reads it; a run must never mix adjacency from
//!   before a write with adjacency from after it.
//! - **`asOf` never goes backwards** across runs on one thread. It is the
//!   stamp a caller uses to decide whether a cached result still describes the
//!   graph, and a stamp that moved backwards would make `stale` meaningless.
//! - **A cached `mutate` result keeps describing the snapshot it was computed
//!   at**, however much the graph moves underneath it, and says so.
//! - **Nothing panics, deadlocks or corrupts** when a projection is
//!   invalidated by a writer while a reader is mid-build.
//!
//! Tests may spawn; the engine may not. The writer here is a real OS thread,
//! because a simulated one cannot invalidate a projection *during* a build —
//! which is the interleaving that matters.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use engram_cypher::Value;
use engram_graph::algo::{AlgoConfig, AlgoValues, Algorithm, ProjectionKey};
use engram_graph::{Dir, Graph, ScopedExec};
use engram_key::{Namespace, Realm};
use engram_store::Store;

struct Exec;

impl ScopedExec for Exec {
    fn width(&self) -> usize {
        1
    }
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        for i in 0..n {
            f(i);
        }
    }
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

/// A graph with `n` nodes in a ring, so every projection is connected and
/// every algorithm has something to say about it.
fn ring(g: &Graph, n: usize) -> Vec<u64> {
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i as i64));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for i in 0..n {
        g.create_rel(ids[i], "R", ids[(i + 1) % n], &BTreeMap::new())
            .expect("rel");
    }
    ids
}

#[test]
fn a_run_concurrent_with_writes_answers_from_a_single_consistent_snapshot() {
    let g = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    ring(&g, 300);

    // THE WRITER HAS A FIXED BUDGET, NOT A STOP FLAG, and that is the whole
    // difference between this test and the one that ate 44 GB.
    //
    // The first version ran the writer "until the reader finishes". The
    // reader's cost grows with the node count, so each run took longer, so the
    // writer got longer to run, so the graph grew more — two threads each
    // feeding the other's workload, with no fixed point. Three abandoned
    // copies reached 44 GB, 38 GB and 15 GB before they were noticed, and the
    // runtime was set by whichever thread the machine happened to favour.
    //
    // **A test whose cost depends on how long it runs has no fixed cost at
    // all.** A bounded writer makes the corpus bounded, which makes the
    // reader's work bounded, which makes "did this leak" a question the test
    // can answer instead of a race it can lose.
    const WRITES: u64 = 600;
    let writes = Arc::new(AtomicU64::new(0));
    let runs = Arc::new(AtomicU64::new(0));

    std::thread::scope(|s| {
        // Relationships rather than properties, because only an EDGE change
        // moves the adjacency epoch the projection is keyed on — a property
        // write would leave every projection current and this test would
        // exercise nothing.
        let wg = Arc::clone(&g);
        let wcount = Arc::clone(&writes);
        s.spawn(move || {
            for i in 0..WRITES {
                let mut m = BTreeMap::new();
                m.insert("k".to_string(), Value::Int(10_000 + i as i64));
                if let Ok(id) = wg.create_node(&["N".into()], &m) {
                    let _ = wg.create_rel(id, "R", id, &BTreeMap::new());
                    wcount.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        // The reader, on this thread: run repeatedly and check every answer
        // against itself.
        let mut last_as_of = 0u64;
        let mut sizes: Vec<usize> = Vec::new();
        for _ in 0..12 {
            let r = g
                .algo_run(Algorithm::Wcc, &cfg(), &Exec)
                .expect("a concurrent run must still answer");
            runs.fetch_add(1, Ordering::Relaxed);

            // ONE SNAPSHOT: the value vector and the id vector are the same
            // length, because both come from the projection built at `as_of`.
            // A run that mixed a pre-write id set with post-write adjacency
            // would show up here first.
            let len = r.values.len();
            assert_eq!(
                len,
                r.ids.len(),
                "the value vector and the id vector disagree — the run read two different \
                 snapshots",
            );
            // Every node the writer adds is a self-loop, so it is its own
            // component; the ring is one. The component labels must all be
            // ids that were actually in the projection.
            if let AlgoValues::Id(v) = &r.values {
                let present: std::collections::BTreeSet<u64> = r.ids.iter().copied().collect();
                for c in v {
                    assert!(
                        present.contains(c),
                        "component label {c} is not a node of the projection it came from",
                    );
                }
            }
            assert!(
                r.as_of >= last_as_of,
                "asOf went BACKWARDS, {} then {} — a caller uses it to decide whether a \
                 cached result is still current, so a stamp that can move backwards makes \
                 `stale` meaningless",
                last_as_of,
                r.as_of,
            );
            last_as_of = r.as_of;
            sizes.push(r.ids.len());
        }
        // THE VACUITY GUARD. Bounding the writer made the test fast enough
        // that it could now finish before the reader's first run, and a test
        // where the writer has already stopped is a test of a quiet graph
        // wearing a concurrency test's name. The projection must be observed
        // GROWING across runs, or nothing above was concurrent with anything.
        assert!(
            sizes.first() < sizes.last(),
            "the projection never grew across {} runs ({:?}) — the writer finished before the              reader started, so this measured a static graph",
            sizes.len(),
            sizes,
        );
    });

    assert!(
        writes.load(Ordering::Relaxed) > 0,
        "the writer never landed a write, so nothing above ran concurrently with anything \
         and this test proves only that the algorithm works on a quiet graph",
    );
    assert_eq!(runs.load(Ordering::Relaxed), 12);
    // The corpus is bounded, so the final size is a fact rather than a race.
    assert!(
        g.members(Some("N")).expect("members").iter().count() <= 300 + WRITES as usize,
        "the graph grew beyond the writer's budget — something other than this test's writer          is creating nodes",
    );
}

#[test]
fn a_cached_result_keeps_describing_the_snapshot_it_was_computed_at() {
    // The `mutate` contract, under a moving graph, driven through the REAL
    // surface rather than a test hook: a cached result is a measurement with a
    // name, not a view. It must not silently recompute, and it must say it is
    // stale rather than pretending otherwise.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ids = ring(&g, 50);

    let q = |src: &str| -> Vec<Vec<Value>> {
        engram_graph::run_query(
            &g,
            &engram_cypher::parse_statement(src).expect("parses"),
            BTreeMap::new(),
        )
        .expect("runs")
        .rows
    };

    q(
        "CALL engram.algo.degree.mutate({nodeLabels:['N'], relationshipTypes:['R'],        mutateKey:'snap'}) YIELD mutateKey RETURN mutateKey",
    );
    let before = q(
        "CALL engram.algo.result.stream({mutateKey:'snap'}) YIELD value                     RETURN count(value)",
    );
    let as_of = q("CALL engram.algo.result.list() YIELD mutateKey, asOf, stale                    RETURN asOf")[0][0]
        .clone();
    assert_eq!(before[0][0], Value::Int(50));
    assert_eq!(
        q("CALL engram.algo.result.list() YIELD stale RETURN stale")[0][0],
        Value::Bool(false),
        "a result computed against the current graph is not stale",
    );

    // Move the graph substantially.
    for i in 0..25 {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(9_000 + i));
        let id = g.create_node(&["N".into()], &m).expect("node");
        g.create_rel(ids[0], "R", id, &BTreeMap::new())
            .expect("rel");
    }

    assert_eq!(
        q("CALL engram.algo.result.stream({mutateKey:'snap'}) YIELD value RETURN count(value)")[0]
            [0],
        Value::Int(50),
        "the cached result grew after 25 writes, so it was RECOMPUTED behind a read rather          than served — which is exactly what `stale` exists to avoid having to do",
    );
    assert_eq!(
        q("CALL engram.algo.result.list() YIELD asOf RETURN asOf")[0][0],
        as_of,
        "the cached result's vintage changed, so it is no longer the measurement that was          published under that name",
    );
    assert_eq!(
        q("CALL engram.algo.result.list() YIELD stale RETURN stale")[0][0],
        Value::Bool(true),
        "after 25 edge writes the cached result must REPORT itself stale rather than the          engine quietly refreshing it",
    );
}
