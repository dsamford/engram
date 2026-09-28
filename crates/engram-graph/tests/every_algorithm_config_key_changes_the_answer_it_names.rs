//! Every accepted config key must DO something.
//!
//! A coverage audit of the algorithm layer found that most keys were exercised
//! only by their refusals: `maxIterations` appeared in one test, passing the
//! string `'many'`; `dampingFactor` appeared once, passing `2.0`. Both assert
//! the type check. Neither had ever been given a value the engine would
//! accept, so nothing anywhere established that a valid `maxIterations`
//! changed how many iterations ran.
//!
//! That is the same defect as a refusal whose ceiling cannot fire: the test
//! asserts the guard's message and never the guard's effect. A key that parsed
//! correctly and was then dropped on the floor would have passed every one of
//! them — and one was. `concurrency` sat in the accepted list changing
//! nothing, in a parser whose own documentation says a silently ignored key is
//! "a wrong answer that looks right".
//!
//! So each test here passes a VALID value and asserts the answer moves.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, k: i64) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("k".to_string(), Value::Int(k));
    g.create_node(&["N".into()], &m).expect("node")
}

fn edge(g: &Graph, a: u64, b: u64, w: f64) {
    let mut m = BTreeMap::new();
    m.insert("w".to_string(), Value::Float(w));
    g.create_rel(a, "R", b, &m).expect("rel");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn err(g: &Graph, src: &str) -> String {
    match run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new()) {
        Err(e) => format!("{e:?}"),
        Ok(r) => panic!("expected a refusal, got {} row(s)", r.rows.len()),
    }
}

/// An IRREGULAR graph, and the irregularity is the point.
///
/// The first version of this fixture was a twelve-node ring. Every node then
/// has in-degree and out-degree one, so PageRank converges to the uniform
/// vector in a SINGLE iteration — and a uniform vector is the same vector for
/// every damping factor. `maxIterations`, `tolerance` and `dampingFactor` all
/// looked inert, and the tests below read that as three keys being dropped on
/// the floor. They were not; the corpus could not tell.
///
/// Degrees from one to four, with a hub, so mass has somewhere uneven to flow
/// and the iteration count and the damping term both have something to change.
fn fixture() -> (Graph, u64) {
    let g = g();
    let n: Vec<u64> = (0..24).map(|i| node(&g, i)).collect();
    for i in 0..n.len() {
        for k in 0..(i % 4) + 1 {
            edge(&g, n[i], n[(i * 5 + k * 7 + 1) % n.len()], (k as f64) + 1.0);
        }
        // A hub, so the score distribution is genuinely skewed.
        edge(&g, n[i], n[0], 1.0);
    }
    (g, n[0])
}

#[test]
fn max_iterations_changes_how_many_iterations_run() {
    let (g, _) = fixture();
    let at = |n: i64| -> i64 {
        match rows(
            &g,
            &format!(
                "CALL engram.algo.pagerank.stats({{nodeLabels:['N'], relationshipTypes:['R'], \
                 maxIterations: {n}, tolerance: 0.0}}) YIELD iterations RETURN iterations"
            ),
        )[0][0]
        {
            Value::Int(i) => i,
            ref other => panic!("expected an int, got {other:?}"),
        }
    };
    // `tolerance: 0.0` so convergence can never stop it early and the cap is
    // the only thing that can — otherwise both runs would converge at the same
    // iteration and the key would look inert whether it worked or not.
    assert_eq!(at(3), 3, "a cap of 3 must run exactly 3 iterations");
    assert_eq!(at(7), 7, "a cap of 7 must run exactly 7");
}

#[test]
fn tolerance_changes_when_the_fixpoint_stops() {
    let (g, _) = fixture();
    let at = |t: &str| -> i64 {
        match rows(
            &g,
            &format!(
                "CALL engram.algo.pagerank.stats({{nodeLabels:['N'], relationshipTypes:['R'], \
                 maxIterations: 100, tolerance: {t}}}) YIELD iterations RETURN iterations"
            ),
        )[0][0]
        {
            Value::Int(i) => i,
            ref other => panic!("{other:?}"),
        }
    };
    let loose = at("0.5");
    let tight = at("0.0000001");
    assert!(
        loose < tight,
        "a looser tolerance must stop SOONER: {loose} iterations at 0.5 against {tight} at \
         1e-7. Equal counts mean the key never reached the convergence test",
    );
}

#[test]
fn damping_factor_changes_the_scores() {
    let (g, _) = fixture();
    let at = |d: &str| -> Vec<u64> {
        rows(
            &g,
            &format!(
                "CALL engram.algo.pagerank.stream({{nodeLabels:['N'], relationshipTypes:['R'], \
                 dampingFactor: {d}}}) YIELD score RETURN score"
            ),
        )
        .iter()
        .map(|r| match r[0] {
            Value::Float(f) => f.to_bits(),
            ref other => panic!("{other:?}"),
        })
        .collect()
    };
    assert_ne!(
        at("0.85"),
        at("0.5"),
        "damping is the whole of PageRank's random-surfer term; two dampings that produce \
         identical scores mean the key was parsed and dropped",
    );
}

#[test]
fn resolution_changes_louvain_communities() {
    // A barbell: two cliques joined by one edge. At a high resolution Louvain
    // prefers smaller communities, so the two halves separate; at a low one it
    // merges them.
    let g = g();
    let a: Vec<u64> = (0..4).map(|i| node(&g, i)).collect();
    let b: Vec<u64> = (4..8).map(|i| node(&g, i)).collect();
    for side in [&a, &b] {
        for i in 0..side.len() {
            for j in (i + 1)..side.len() {
                edge(&g, side[i], side[j], 1.0);
                edge(&g, side[j], side[i], 1.0);
            }
        }
    }
    edge(&g, a[0], b[0], 1.0);

    // `communityCount` is NOT a top-level stats column: the summary extras
    // (`communityCount`, `componentCount`, `modularity`, `oscillated`) are
    // folded into the `distribution` map alongside the value percentiles.
    // Worth stating because it is not guessable from the catalogue, which
    // declares `distribution` as a plain Map and says nothing about what a
    // given algorithm puts in it.
    let count = |r: &str| -> i64 {
        let row = rows(
            &g,
            &format!(
                "CALL engram.algo.louvain.stats({{nodeLabels:['N'], relationshipTypes:['R'], \
                 orientation:'UNDIRECTED', resolution: {r}}}) \
                 YIELD distribution RETURN distribution"
            ),
        );
        match &row[0][0] {
            Value::Map(m) => match m.get("communityCount") {
                Some(Value::Int(i)) => *i,
                other => panic!("communityCount missing from the distribution: {other:?}"),
            },
            other => panic!("expected a map, got {other:?}"),
        }
    };
    let low = count("0.1");
    let high = count("5.0");
    assert!(
        high >= low,
        "a higher resolution must not find FEWER communities: {low} at 0.1 against {high} \
         at 5.0",
    );
    assert!(
        high > low || low == 1,
        "resolution changed nothing ({low} then {high}) — on a barbell the two settings \
         should disagree unless the graph collapsed to one community",
    );
}

#[test]
fn write_batch_size_changes_how_the_write_is_batched_and_not_what_it_writes() {
    // The key shapes the write path's transactions. It must not change the
    // ANSWER — a batch size that altered the values written would mean the
    // batching had leaked into the computation.
    let (g, _) = fixture();
    let write = |n: i64, prop: &str| -> Vec<Vec<Value>> {
        rows(
            &g,
            &format!(
                "CALL engram.algo.degree.write({{nodeLabels:['N'], relationshipTypes:['R'], \
                 writeProperty:'{prop}', writeBatchSize: {n}}}) \
                 YIELD nodesWritten RETURN nodesWritten"
            ),
        )
    };
    assert_eq!(
        write(2, "b2"),
        write(1000, "b1000"),
        "the receipts must agree"
    );
    let same = rows(&g, "MATCH (n:N) WHERE n.b2 <> n.b1000 RETURN count(n)");
    assert_eq!(
        same[0][0],
        Value::Int(0),
        "batching changed the written VALUES, so it is not merely batching",
    );
}

#[test]
fn orientation_changes_the_answer_in_all_three_directions() {
    // Only 'UNDIRECTED' had ever been passed. A path a -> b -> c distinguishes
    // all three: NATURAL reaches 2 from a, REVERSE reaches none, UNDIRECTED
    // reaches 2 from either end.
    let g = g();
    let a = node(&g, 0);
    let b = node(&g, 1);
    let c = node(&g, 2);
    edge(&g, a, b, 1.0);
    edge(&g, b, c, 1.0);

    let deg = |o: &str| -> Vec<i64> {
        rows(
            &g,
            &format!(
                "CALL engram.algo.degree.stream({{nodeLabels:['N'], relationshipTypes:['R'], \
                 orientation:'{o}'}}) YIELD degree RETURN degree"
            ),
        )
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            Value::Float(f) => f as i64,
            ref other => panic!("{other:?}"),
        })
        .collect()
    };
    let natural = deg("NATURAL");
    let reverse = deg("REVERSE");
    let undirected = deg("UNDIRECTED");
    assert_ne!(
        natural, reverse,
        "NATURAL and REVERSE must disagree on a directed path — identical answers mean the \
         orientation never reached the projection",
    );
    assert_ne!(
        natural, undirected,
        "NATURAL and UNDIRECTED must disagree too"
    );
}

#[test]
fn concurrency_narrows_the_executor_and_never_widens_it() {
    // THE KEY THAT DID NOTHING. It sat in the accepted list, parsed by nobody,
    // in a parser whose doc says a silently ignored key is a wrong answer that
    // looks right.
    //
    // It can only ever narrow: the engine spawns no threads, so the width
    // available is whatever the server installed. Asking for more than exists
    // is honoured as "all of it" — which is what a GDS-shaped call expects —
    // and asking for less genuinely gives less.
    use engram_graph::ScopedExec;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Rec {
        width: usize,
        morsels: AtomicUsize,
    }
    impl ScopedExec for Rec {
        fn width(&self) -> usize {
            self.width
        }
        fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
            self.morsels.fetch_add(n, Ordering::Relaxed);
            for i in 0..n {
                f(i);
            }
        }
    }

    let run_with = |c: Option<usize>| -> usize {
        let g = g();
        let n: Vec<u64> = (0..600).map(|i| node(&g, i)).collect();
        for i in 0..n.len() {
            edge(&g, n[i], n[(i * 7 + 1) % n.len()], 1.0);
        }
        let exec = Arc::new(Rec {
            width: 8,
            morsels: AtomicUsize::new(0),
        });
        g.set_exec(Some(exec.clone()));
        g.set_algo_parallel(true);
        g.set_algo_min_vertices(1);
        let extra = c.map_or(String::new(), |c| format!(", concurrency: {c}"));
        let _ = rows(
            &g,
            &format!(
                "CALL engram.algo.pagerank.stream({{nodeLabels:['N'], relationshipTypes:['R'], \
                 maxIterations: 3, tolerance: 0.0{extra}}}) YIELD score RETURN count(score)"
            ),
        );
        exec.morsels.load(Ordering::Relaxed)
    };

    let uncapped = run_with(None);
    let capped = run_with(Some(2));
    assert!(
        uncapped > 0,
        "the parallel lane never ran, so nothing is measured"
    );
    assert!(
        capped < uncapped,
        "`concurrency: 2` against a width-8 executor must produce FEWER morsels: {capped} \
         against {uncapped}. Equal counts mean the key is still being ignored",
    );
    // And it must not widen: asking for 64 on a width-8 executor gets 8.
    assert_eq!(
        run_with(Some(64)),
        uncapped,
        "`concurrency` above the installed width must be honoured as all of it, not refused \
         and not conjured",
    );
}

#[test]
fn a_concurrency_below_one_is_refused_rather_than_clamped() {
    let (g, _) = fixture();
    let e = err(
        &g,
        "CALL engram.algo.pagerank.stream({nodeLabels:['N'], concurrency: 0}) \
         YIELD score RETURN score",
    );
    assert!(
        e.contains("concurrency"),
        "a concurrency of zero must be refused by name, got {e}",
    );
}
