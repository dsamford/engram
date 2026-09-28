//! The procedure surface must reach the morsel seam, and the lever must stop it.
//!
//! `pagerank_is_bit_identical_at_every_scoped_exec_width` proves the fixpoint
//! driver is width-invariant when it is handed an executor. It says nothing
//! about whether anything ever hands it one. For a revision, nothing did:
//! `algo_procedure` constructed a `serial()` executor and passed that,
//! ignoring the one the server had installed — so the parallel lane was
//! correct, tested, and dead.
//!
//! This file is the other half. It drives the REAL surface — `CALL
//! engram.algo.pagerank.stream` — and asserts, from the executor's own side,
//! that the work arrived. Then it turns the lever off and asserts the work
//! stops arriving while the answer does not change, which is what makes
//! `--no-algo-parallel` an A/B arm rather than a switch nobody can read.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// An executor that records what it was handed, and runs it inline.
///
/// Inline rather than threaded because what is under test is DISPATCH, not
/// concurrency — the width-invariance file owns the concurrency claim. The
/// counters are read by the test that owns them rather than through
/// `counted!`, because a counter fired inside a morsel records onto whichever
/// thread ran it and is dropped.
struct RecordingExec {
    width: usize,
    calls: AtomicUsize,
    morsels: AtomicUsize,
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

/// A graph the driver will split, with the parallel floor lowered to let it.
///
/// The real floor is 65,536 vertices — a COST threshold measured by
/// `algowidth`, not a correctness one — and building that many nodes per test
/// would buy nothing, since the answers are identical either side of it.
/// Lowering it is the same accommodation `parallel_min_rows` exists for:
/// behaviour at a threshold must be testable without a corpus at the
/// threshold.
fn corpus() -> (Graph, Arc<RecordingExec>) {
    let (g, exec) = corpus_without_levers();
    g.set_algo_min_vertices(1);
    (g, exec)
}

/// The same corpus with NO lever set, for the cross-thread test to set them
/// from somewhere else.
fn corpus_without_levers() -> (Graph, Arc<RecordingExec>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n: Vec<u64> = (0..4_999)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("i".to_string(), Value::Int(i));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for (i, src) in n.iter().enumerate() {
        for k in 0..(i % 4) + 1 {
            g.create_rel(
                *src,
                "R",
                n[(i * 7 + k * 13 + 1) % n.len()],
                &BTreeMap::new(),
            )
            .expect("rel");
        }
    }
    let exec = Arc::new(RecordingExec {
        width: 4,
        calls: AtomicUsize::new(0),
        morsels: AtomicUsize::new(0),
    });
    g.set_exec(Some(exec.clone()));
    (g, exec)
}

const Q: &str = "CALL engram.algo.pagerank.stream({nodeLabels: ['N'], relationshipTypes: ['R']}) \
                 YIELD nodeId, score RETURN nodeId, score";

fn scores(g: &Graph) -> Vec<(i64, u64)> {
    let r = run_query(g, &parse_statement(Q).expect("parses"), Default::default())
        .expect("the procedure runs");
    r.rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int(id), Value::Float(s)) => (*id, s.to_bits()),
            other => panic!("unexpected row shape: {other:?}"),
        })
        .collect()
}

#[test]
fn an_algorithm_procedure_reaches_the_parallel_lane() {
    let (g, exec) = corpus();
    g.set_algo_parallel(true);
    let parallel = scores(&g);

    let calls = exec.calls.load(Ordering::Relaxed);
    let morsels = exec.morsels.load(Ordering::Relaxed);
    assert!(
        calls > 0,
        "`CALL engram.algo.pagerank.stream` never reached the installed executor — the parallel \
         lane is unreachable from the only surface a user has, which is what this file exists \
         to catch",
    );
    assert_eq!(
        morsels,
        calls * 4,
        "a width-4 executor must be handed 4 morsels per iteration, not {morsels} across \
         {calls} calls",
    );
    assert!(
        !parallel.is_empty(),
        "the procedure produced no rows, so nothing above is evidence of anything",
    );
}

#[test]
fn turning_the_lever_off_keeps_the_work_off_the_executor_and_the_answer_the_same() {
    // The negative, and the A/B in one: the lever must change WHERE the work
    // runs and must not change WHAT it computes. Either half alone would be
    // satisfied by a lever that did nothing.
    let (g, exec) = corpus();

    g.set_algo_parallel(true);
    let parallel = scores(&g);
    let after_parallel = exec.calls.load(Ordering::Relaxed);
    assert!(after_parallel > 0, "the parallel arm never ran");

    g.set_algo_parallel(false);
    let serial = scores(&g);
    let after_serial = exec.calls.load(Ordering::Relaxed);

    assert_eq!(
        after_parallel,
        after_serial,
        "with `--no-algo-parallel` the fixpoint must not touch the installed executor at all, \
         but it made {} further call(s)",
        after_serial - after_parallel,
    );
    assert_eq!(
        parallel, serial,
        "the two lanes disagreed — they must be BIT-identical, which is the whole reason the \
         lever is safe to flip on a running server",
    );
}

#[test]
fn a_graph_below_the_size_floor_never_touches_the_executor() {
    // The floor is a performance decision, so it must be invisible in the
    // answer and visible in the dispatch. A test that only checked the answer
    // would pass whether the floor existed or not.
    //
    // "Never touches" and not "asks for one morsel", because that distinction
    // is what the floor got wrong the first time: applied inside the driver it
    // still called `for_each(1, ..)`, and a thread pool honours that by
    // spawning a scope to run one closure. `algowidth` measured 0.56x at 1,024
    // vertices — the guard against a slowdown WAS the slowdown. The floor now
    // swaps the executor out entirely, and this asserts the swap.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_algo_min_vertices(4_096);
    let n: Vec<u64> = (0..64)
        .map(|_| {
            g.create_node(&["N".into()], &BTreeMap::new())
                .expect("node")
        })
        .collect();
    for (i, src) in n.iter().enumerate() {
        g.create_rel(*src, "R", n[(i * 5 + 1) % n.len()], &BTreeMap::new())
            .expect("rel");
    }
    let exec = Arc::new(RecordingExec {
        width: 4,
        calls: AtomicUsize::new(0),
        morsels: AtomicUsize::new(0),
    });
    g.set_exec(Some(exec.clone()));
    g.set_algo_parallel(true);

    let rows = scores(&g);
    assert_eq!(
        rows.len(),
        64,
        "the procedure did not answer over the corpus"
    );
    assert_eq!(
        exec.calls.load(Ordering::Relaxed),
        0,
        "below the vertex floor the installed executor must not be entered AT ALL — asking it \
         for one morsel is not the same thing, and costs a thread spawn per iteration",
    );
}

#[test]
fn a_lever_set_on_one_thread_is_read_on_another() {
    // THE GAP THIS CLOSES, which the two tests above do not.
    //
    // They assert the lever's effect from the executor's side, which sounds
    // like the right shape — but `RecordingExec` runs each morsel inline on
    // the calling thread, so a lever held in a THREAD-LOCAL would have been
    // perfectly visible to them and they would have passed. A test lane whose
    // executor never leaves the calling thread verifies the machinery and
    // hides exactly this class.
    //
    // It is not hypothetical. A threshold elsewhere in this engine was found
    // to be read inside a `ScopedExec` morsel from a thread-local, so the
    // fresh scoped threads read the DEFAULT and the lever silently did
    // nothing on the only path that mattered. The value was right, the read
    // was right, and the thread was wrong.
    //
    // So: set the levers from a thread that is not the one that runs the
    // query. This passes only because they are atomics on the `Graph`; it
    // fails outright if either becomes a `thread_local!`.
    let (g, exec) = corpus_without_levers();

    std::thread::scope(|s| {
        s.spawn(|| {
            g.set_algo_min_vertices(1);
            g.set_algo_parallel(true);
        });
    });

    let rows = scores(&g);
    assert!(!rows.is_empty(), "the procedure produced no rows");
    assert!(
        exec.calls.load(Ordering::Relaxed) > 0,
        "levers set on another thread did not reach the run — they are per-thread state, so \
         the server setting them at session start would be invisible to the query thread that \
         reads them",
    );
}

#[test]
fn a_dense_graph_below_the_vertex_floor_still_reaches_the_executor() {
    // rev62. The floor counted vertices alone, and Graphalytics' dota-league
    // (61,170 vertices, 50,870,313 edges) sat under the 65,536 default, so
    // every kernel on it ran on one thread; its LCC held one core of 40 for
    // 50 minutes. A projection now also goes to the executor once its edges
    // reach 16x the vertex floor. Here: a floor of 256 vertices, 128 vertices
    // and 5,120 edges (past the 4,096 the edge clause asks). The sparse case
    // is `a_graph_below_the_size_floor_never_touches_the_executor` above,
    // which still must not enter it. The answer must be the serial lane's,
    // bit for bit.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_algo_min_vertices(256);
    let n: Vec<u64> = (0..128)
        .map(|_| {
            g.create_node(&["N".into()], &BTreeMap::new())
                .expect("node")
        })
        .collect();
    for (i, src) in n.iter().enumerate() {
        // 40 distinct targets a vertex, none of them itself
        for k in 1..=40 {
            g.create_rel(*src, "R", n[(i + k) % n.len()], &BTreeMap::new())
                .expect("rel");
        }
    }
    let exec = Arc::new(RecordingExec {
        width: 4,
        calls: AtomicUsize::new(0),
        morsels: AtomicUsize::new(0),
    });
    g.set_exec(Some(exec.clone()));
    g.set_algo_parallel(true);
    let parallel = scores(&g);
    assert!(
        exec.calls.load(Ordering::Relaxed) > 0,
        "5,120 edges over 128 vertices, past 16x the 256-vertex floor, never reached the executor"
    );
    g.set_algo_parallel(false);
    let serial = scores(&g);
    assert_eq!(parallel.len(), 128, "the procedure did not answer over the corpus");
    assert_eq!(parallel, serial, "the dense graph's parallel lane disagreed with the serial one");
}
