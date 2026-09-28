//! Betweenness centrality: checked against arithmetic, not against a golden
//! file.
//!
//! Every expected score here is derived in the test's own comment from the
//! definition — the fraction of shortest paths between each pair that passes
//! through the node. A golden file would pin whatever the implementation did
//! on the day it was written, including its mistakes, and betweenness has
//! three places to make one that all produce plausible numbers: the
//! shortest-path COUNTS (`sigma`), the dependency accumulation, and the
//! undirected halving.
//!
//! The halving is the one worth stating. On an undirected graph each unordered
//! pair is reached from both ends, so the raw accumulation is twice the
//! conventional score. GDS halves; so does this. A caller comparing against
//! GDS would otherwise see every value doubled and have no way to tell which
//! of the two was wrong.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::algo::{AlgoConfig, AlgoValues, Algorithm, ProjectionKey};
use engram_graph::{Dir, Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Width-1 inline executor. Betweenness is serial by construction — see the
/// kernel — so this exists only to satisfy the signature.
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

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["N".into()], &m).expect("node")
}

fn edge(g: &Graph, a: u64, b: u64) {
    g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");
}

fn cfg(dir: Dir) -> AlgoConfig {
    AlgoConfig {
        projection: ProjectionKey {
            labels: vec!["N".into()],
            types: vec!["R".into()],
            dir,
            weight: None,
        },
        ..AlgoConfig::default()
    }
}

/// Scores in ascending node-id order, which is the order `ids` is built in.
fn scores(g: &Graph, dir: Dir) -> Vec<f64> {
    let r = g
        .algo_run(Algorithm::Betweenness, &cfg(dir), &Exec)
        .expect("betweenness");
    match &r.values {
        AlgoValues::Float(f) => f.clone(),
        other => panic!("expected floats, got {other:?}"),
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ─── Hand-computed cases ───────────────────────────────────────────────────

#[test]
fn a_path_graph_gives_its_middle_node_the_only_nonzero_score() {
    // UNDIRECTED a - b - c. The only pair whose shortest path passes through
    // a third node is {a, c}, and its single route goes through b. So
    // b = 1, a = c = 0.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);

    let s = scores(&g, Dir::Both);
    assert_eq!(s.len(), 3);
    assert!(close(s[0], 0.0), "a should be 0, got {}", s[0]);
    assert!(
        close(s[1], 1.0),
        "b lies on the only route between a and c, so its score is exactly 1 — got {}. A \
         value of 2 means the undirected halving is missing",
        s[1],
    );
    assert!(close(s[2], 0.0), "c should be 0, got {}", s[2]);
}

#[test]
fn a_star_gives_its_centre_one_per_pair_of_leaves() {
    // UNDIRECTED star: centre c, leaves l0 l1 l2. Every pair of leaves has
    // exactly one shortest route and it passes through the centre. Three
    // pairs, so centre = 3 and every leaf = 0.
    let g = g();
    let c = node(&g, "c");
    let leaves: Vec<u64> = (0..3).map(|i| node(&g, &format!("l{i}"))).collect();
    for l in &leaves {
        edge(&g, c, *l);
    }

    let s = scores(&g, Dir::Both);
    assert!(
        close(s[0], 3.0),
        "the centre lies on all three leaf-to-leaf routes, so its score is exactly 3 — got {}",
        s[0],
    );
    for (i, v) in s.iter().enumerate().skip(1) {
        assert!(close(*v, 0.0), "leaf {i} should be 0, got {v}");
    }
}

#[test]
fn a_tie_splits_the_credit_between_the_two_routes() {
    // THE `sigma` TEST, and the one an implementation that merely counted
    // paths rather than weighting them would fail.
    //
    // UNDIRECTED: a - p - z, a - q - z, AND p - q. The pair {a, z} has two
    // shortest routes, so p and q each carry half of it: 0.5 apiece, not 1.
    // Every other pair is adjacent.
    //
    // The `p - q` edge is load-bearing and was missing the first time. Without
    // it {p, q} is ALSO a tied pair — two routes, through a and through z — so
    // a and z score 0.5 as well and the fixture reads 0.5 everywhere. That
    // passes an implementation which divides the credit and one which does
    // not, because the symmetry hides the very thing under test. Joining p and
    // q leaves exactly one tied pair, and the halves have nowhere else to come
    // from.
    let g = g();
    let a = node(&g, "a");
    let p = node(&g, "p");
    let q = node(&g, "q");
    let z = node(&g, "z");
    edge(&g, a, p);
    edge(&g, p, z);
    edge(&g, a, q);
    edge(&g, q, z);
    edge(&g, p, q);

    let s = scores(&g, Dir::Both);
    assert!(close(s[0], 0.0), "a should be 0, got {}", s[0]);
    assert!(
        close(s[1], 0.5) && close(s[2], 0.5),
        "p and q each carry HALF of the one pair with two shortest routes — 0.5 each, got \
         {} and {}. Two 1.0s would mean the credit was not divided by the path count",
        s[1],
        s[2],
    );
    assert!(close(s[3], 0.0), "z should be 0, got {}", s[3]);
}

#[test]
fn a_directed_path_is_not_halved() {
    // DIRECTED a -> b -> c. One ordered pair (a, c) routes through b, so
    // b = 1. The undirected halving must not apply: 0.5 here would mean the
    // flag was read as "always halve" rather than "this projection is
    // symmetric".
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);

    let s = scores(&g, Dir::Out);
    assert!(
        close(s[1], 1.0),
        "a directed path gives its middle node exactly 1 — got {}. 0.5 means the directed \
         run was halved",
        s[1],
    );
    assert!(close(s[0], 0.0) && close(s[2], 0.0));
}

#[test]
fn a_parallel_edge_does_not_multiply_the_shortest_path_counts() {
    // THE MULTIGRAPH TRAP, on the shape where it actually shows.
    //
    // Betweenness is defined over a simple graph — GDS and networkx both treat
    // parallel edges as one — so an extra a-q edge must not make routes
    // through q count double. Put it on a TIE and the skew is visible: a - p -
    // z and a - q - z with p and q joined, plus a SECOND a-q edge.
    //
    // Deduped, {a, z} has two routes and p and q take 0.5 each. Counting the
    // parallel edge separately makes it three routes, and the credit becomes
    // 1/3 to p and 2/3 to q — plausible numbers, silently wrong.
    //
    // A mutual pair (a->b AND b->a) does NOT reach the kernel twice: the
    // store's own `Dir::Both` dedup already collapses it, which is why the
    // first version of this test passed with the skip removed and proved
    // nothing.
    let g = g();
    let a = node(&g, "a");
    let p = node(&g, "p");
    let q = node(&g, "q");
    let z = node(&g, "z");
    edge(&g, a, p);
    edge(&g, p, z);
    edge(&g, a, q);
    edge(&g, q, z);
    edge(&g, p, q);
    edge(&g, a, q); // the parallel edge

    let s = scores(&g, Dir::Both);
    assert!(
        close(s[1], 0.5) && close(s[2], 0.5),
        "a parallel edge must not multiply the path counts: p and q still split the tie 0.5 \
         and 0.5, got {} and {}. Roughly 0.33 and 0.67 means the duplicate was counted as a \
         third route",
        s[1],
        s[2],
    );
}

// ─── Determinism ───────────────────────────────────────────────────────────

#[test]
fn betweenness_is_bit_identical_across_runs_and_widths() {
    // Serial by construction, so width cannot change the answer — and that is
    // exactly why it must be asserted rather than assumed: a later change that
    // parallelised the source loop would be summing each vertex's
    // contributions in thread order, and floating point addition is not
    // associative.
    let g = g();
    let n: Vec<u64> = (0..17).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        for k in 0..(i % 3) + 1 {
            edge(&g, n[i], n[(i * 5 + k * 3 + 1) % n.len()]);
        }
    }
    struct Wide(usize);
    impl ScopedExec for Wide {
        fn width(&self) -> usize {
            self.0
        }
        fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
            for i in 0..n {
                f(i);
            }
        }
    }
    let base = scores(&g, Dir::Both);
    assert!(
        base.iter().any(|v| *v > 0.0),
        "the fixture produced an all-zero result, so the comparisons below prove nothing",
    );
    for w in [1usize, 2, 4, 8] {
        let other = match &g
            .algo_run(Algorithm::Betweenness, &cfg(Dir::Both), &Wide(w))
            .expect("run")
            .values
        {
            AlgoValues::Float(f) => f.clone(),
            other => panic!("{other:?}"),
        };
        for (i, (a, b)) in base.iter().zip(other.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "width {w} changed vertex {i}: {a} vs {b}",
            );
        }
    }
}

// ─── Degenerate shapes ─────────────────────────────────────────────────────

#[test]
fn an_empty_and_a_single_node_projection_answer_zero_rather_than_failing() {
    let g = g();
    assert!(
        scores(&g, Dir::Both).is_empty(),
        "an empty projection answers no scores",
    );
    node(&g, "only");
    assert_eq!(
        scores(&g, Dir::Both),
        vec![0.0],
        "one node lies on no path between others",
    );
}

#[test]
fn a_disconnected_graph_scores_each_component_on_its_own() {
    // Unreachable pairs contribute nothing — the accumulation must not treat
    // an unreached vertex's `sigma` of zero as a division to perform.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let x = node(&g, "x");
    let y = node(&g, "y");
    let z = node(&g, "z");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, x, y);
    edge(&g, y, z);

    let s = scores(&g, Dir::Both);
    assert!(
        close(s[1], 1.0) && close(s[4], 1.0),
        "each component's middle node scores 1 on its own component's single pair, got {s:?}",
    );
    for i in [0usize, 2, 3, 5] {
        assert!(close(s[i], 0.0), "endpoint {i} should be 0, got {}", s[i]);
    }
}

// ─── The procedure surface, and its negatives ──────────────────────────────

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

fn star() -> Graph {
    let g = g();
    let c = node(&g, "c");
    for i in 0..3 {
        let l = node(&g, &format!("l{i}"));
        edge(&g, c, l);
    }
    g
}

#[test]
fn every_mode_of_the_betweenness_procedure_answers() {
    let g = star();
    assert_eq!(
        rows(
            &g,
            "CALL engram.algo.betweenness.stream({nodeLabels:['N'], relationshipTypes:['R'], \
             orientation:'UNDIRECTED'}) YIELD nodeId, score RETURN count(score)",
        )[0][0],
        Value::Int(4),
        "stream must answer one row per node",
    );
    assert!(
        !rows(
            &g,
            "CALL engram.algo.betweenness.stats({nodeLabels:['N'], relationshipTypes:['R']}) \
             YIELD nodeCount, asOf RETURN nodeCount, asOf",
        )
        .is_empty(),
        "stats must answer one summary row",
    );
    assert!(
        !rows(
            &g,
            "CALL engram.algo.betweenness.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
             mutateKey:'b1'}) YIELD mutateKey RETURN mutateKey",
        )
        .is_empty(),
        "mutate must answer a receipt",
    );
    assert_eq!(
        rows(
            &g,
            "CALL engram.algo.result.stream({mutateKey:'b1'}) YIELD value RETURN count(value)",
        )[0][0],
        Value::Int(4),
        "the mutated result must be readable back by key, or `mutate` is a no-op with a receipt",
    );
    assert!(
        !rows(
            &g,
            "CALL engram.algo.betweenness.write({nodeLabels:['N'], relationshipTypes:['R'], \
             writeProperty:'bw'}) YIELD nodesWritten, asOf, committedAt RETURN nodesWritten",
        )
        .is_empty(),
        "write must answer a receipt",
    );
    assert_eq!(
        rows(&g, "MATCH (n:N) WHERE n.bw IS NOT NULL RETURN count(n)")[0][0],
        Value::Int(4),
        "write must land the property on every projected node",
    );
}

#[test]
fn an_all_pairs_projection_over_the_work_ceiling_is_refused_and_names_the_lever() {
    // THE NEGATIVE THAT MATTERS MOST. Betweenness is O(V x E) where everything
    // else here is O(V + E), so a projection comfortably inside the node and
    // edge ceilings is still days of work. Without a ceiling of its own the
    // refusal could never fire for the one algorithm most able to run away.
    let g = star();
    g.set_algo_work_ceiling(1);
    let e = err(
        &g,
        "CALL engram.algo.betweenness.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
         YIELD score RETURN count(score)",
    );
    g.set_algo_work_ceiling(10_000_000_000);
    assert!(
        e.contains("ENGRAM_ALGO_WORK_CEILING"),
        "the refusal must name the lever that would raise the ceiling, got {e}",
    );
    assert!(
        e.contains("node x edge work"),
        "the refusal must name the quantity it measured, got {e}",
    );
}

#[test]
fn the_named_ceiling_levers_are_real_and_each_one_bites() {
    // Every refusal names an environment variable. Those names shipped in
    // user-facing error text while NOTHING read them: an operator following
    // the message exactly would set the variable, see no change, and have no
    // way to tell the advice was fiction. A lever named in an error message is
    // a promise.
    /// A lever and the ceiling-setter behind it.
    type Case = (&'static str, fn(&Graph));
    let cases: [Case; 3] = [
        ("ENGRAM_ALGO_NODE_CEILING", |g| g.set_algo_node_ceiling(1)),
        ("ENGRAM_ALGO_EDGE_CEILING", |g| g.set_algo_edge_ceiling(0)),
        ("ENGRAM_ALGO_BYTE_CEILING", |g| g.set_algo_byte_ceiling(1)),
    ];
    for (name, set) in cases {
        let g = star();
        set(&g);
        let e = err(
            &g,
            "CALL engram.algo.pagerank.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
             YIELD score RETURN count(score)",
        );
        assert!(
            e.contains(name),
            "lowering the ceiling behind {name} must produce a refusal naming it, got {e}",
        );
    }
}

#[test]
fn an_unknown_algorithm_name_is_still_refused_after_betweenness_was_added() {
    let g = star();
    let e = err(
        &g,
        "CALL engram.algo.betweeness.stream({nodeLabels:['N']}) YIELD score RETURN score",
    );
    assert!(
        !e.is_empty(),
        "a misspelled algorithm must be refused, not silently resolved to the nearest match",
    );
}

#[test]
fn write_mode_still_refuses_inside_an_explicit_transaction() {
    // THE NEGATIVE THAT MUST SURVIVE THE FIX.
    //
    // `write` mode was unreachable over Bolt because the server wraps every
    // statement that CAN write in a serialisable autocommit transaction, and
    // the mode refused to run inside any transaction at all — including the
    // one-statement wrapper that existed to make its own write durable.
    //
    // The repair narrows the rule to EXPLICIT transactions. That is only
    // correct if the rule still fires for a real one, which is what this
    // asserts: the hazards it exists for — seeing one's own uncommitted writes
    // in the read snapshot, holding entity locks across a fixpoint — are about
    // a transaction containing OTHER statements, and `begin_txn` opens exactly
    // that.
    let g = star();
    g.begin_txn().expect("begin");
    let e = err(
        &g,
        "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
         writeProperty:'w'}) YIELD nodesWritten RETURN nodesWritten",
    );
    g.rollback_txn();
    assert!(
        e.contains("open transaction"),
        "inside a user's own transaction the refusal must still fire — narrowing it to \
         explicit transactions is only safe if it still catches one, got {e}",
    );
    assert!(
        e.contains("mutate"),
        "and it must still name the alternative that DOES work there, got {e}",
    );
}
