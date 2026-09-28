//! Strongly connected components and closeness centrality.
//!
//! Both exist because a neighbouring algorithm answers a *different* question
//! that is easy to mistake for theirs.
//!
//! **SCC against WCC.** WCC asks which nodes are joined ignoring arrow
//! direction; SCC asks which nodes can REACH each other following them. On a
//! directed graph WCC will report one component where no node can reach any
//! other — a true answer to a question the caller probably did not ask. The
//! tests below are built so the two disagree, because a fixture where they
//! agree cannot tell you which one you implemented.
//!
//! **Closeness against degree.** Degree is local; closeness is global, and its
//! whole difficulty is the disconnected case, where the plain definition
//! divides by an infinite distance. The Wasserman-Faust scaling is what makes
//! it defined, and it is the part worth pinning.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::algo::{AlgoConfig, AlgoValues, Algorithm, ProjectionKey};
use engram_graph::{Dir, Graph, ScopedExec, run_query};
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

fn ids(g: &Graph, alg: Algorithm, dir: Dir) -> Vec<u64> {
    match &g.algo_run(alg, &cfg(dir), &Exec).expect("run").values {
        AlgoValues::Id(v) | AlgoValues::Count(v) => v.clone(),
        other => panic!("expected ids, got {other:?}"),
    }
}

fn floats(g: &Graph, alg: Algorithm, dir: Dir) -> Vec<f64> {
    match &g.algo_run(alg, &cfg(dir), &Exec).expect("run").values {
        AlgoValues::Float(v) => v.clone(),
        other => panic!("expected floats, got {other:?}"),
    }
}

fn distinct(v: &[u64]) -> usize {
    v.iter().collect::<std::collections::BTreeSet<_>>().len()
}

// ─── SCC ───────────────────────────────────────────────────────────────────

#[test]
fn scc_splits_a_chain_that_wcc_calls_one_component() {
    // THE FIXTURE THAT SEPARATES THEM. a -> b -> c, directed and acyclic.
    //
    // WCC: one component — they are all joined if you ignore direction.
    // SCC: three components — no node can get back to its predecessor.
    //
    // A fixture where both say the same thing would pass whichever of the two
    // had actually been implemented, which is why this one is built to make
    // them disagree.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);

    assert_eq!(
        distinct(&ids(&g, Algorithm::Wcc, Dir::Out)),
        1,
        "WCC ignores direction, so the chain is one weakly connected component",
    );
    assert_eq!(
        distinct(&ids(&g, Algorithm::Scc, Dir::Out)),
        3,
        "SCC follows the arrows: nothing can return, so every node is its own \
         strongly connected component",
    );
}

#[test]
fn scc_finds_a_cycle_as_one_component_and_its_tail_as_another() {
    // a -> b -> c -> a is a cycle: every node reaches every other, so one
    // component. `t` hangs off it and can be reached but cannot return, so it
    // is its own. Two components, of sizes 3 and 1.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    let t = node(&g, "t");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, c, a);
    edge(&g, c, t);

    let comp = ids(&g, Algorithm::Scc, Dir::Out);
    assert_eq!(distinct(&comp), 2, "one cycle and one tail, got {comp:?}");
    assert!(
        comp[0] == comp[1] && comp[1] == comp[2],
        "the three cycle members share a component, got {comp:?}",
    );
    assert_ne!(
        comp[3], comp[0],
        "the tail cannot return to the cycle, so it is not part of it",
    );
}

#[test]
fn scc_labels_a_component_by_its_smallest_node_id() {
    // The same canonicalisation WCC uses, so the two are comparable and a
    // label means the same thing on every run.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, c, a);
    let comp = ids(&g, Algorithm::Scc, Dir::Out);
    assert_eq!(
        comp,
        vec![a, a, a],
        "one cycle, labelled by its smallest member id",
    );
}

#[test]
fn scc_is_reproducible_across_runs() {
    let g = g();
    let n: Vec<u64> = (0..19).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() {
        edge(&g, n[i], n[(i * 7 + 1) % n.len()]);
        edge(&g, n[i], n[(i + 3) % n.len()]);
    }
    let first = ids(&g, Algorithm::Scc, Dir::Out);
    for _ in 0..5 {
        assert_eq!(ids(&g, Algorithm::Scc, Dir::Out), first);
    }
}

#[test]
fn scc_over_a_deep_chain_does_not_overflow_the_stack() {
    // THE REASON THIS IS KOSARAJU AND NOT TARJAN. Tarjan is naturally
    // recursive, and a recursive descent over a projection the node ceiling
    // permits — twenty million — overflows long before it finishes. Both of
    // Kosaraju's passes are iterative with an explicit stack.
    //
    // Ten thousand is far below the ceiling and far above any default thread
    // stack's tolerance for one frame per node.
    let g = g();
    let n: Vec<u64> = (0..10_000).map(|i| node(&g, &format!("n{i}"))).collect();
    for i in 0..n.len() - 1 {
        edge(&g, n[i], n[i + 1]);
    }
    let comp = ids(&g, Algorithm::Scc, Dir::Out);
    assert_eq!(
        distinct(&comp),
        10_000,
        "an acyclic chain is one component per node",
    );
}

// ─── Closeness ─────────────────────────────────────────────────────────────

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn closeness_ranks_the_middle_of_a_path_above_its_ends() {
    // UNDIRECTED a - b - c. Distances: b reaches both at 1, so its summed
    // distance is 2 over 2 reachable. a reaches b at 1 and c at 2, summing 3.
    //
    // Wasserman-Faust: C = (reached / sum) x (reached / (n - 1)).
    //   b: (2/2) x (2/2) = 1
    //   a: (2/3) x (2/2) = 0.6666...
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);

    let s = floats(&g, Algorithm::Closeness, Dir::Both);
    assert!(close(s[1], 1.0), "the middle scores 1, got {}", s[1]);
    assert!(
        close(s[0], 2.0 / 3.0) && close(s[2], 2.0 / 3.0),
        "the ends score 2/3, got {} and {}",
        s[0],
        s[2],
    );
}

#[test]
fn closeness_scales_by_how_much_of_the_graph_a_node_reaches() {
    // THE WASSERMAN-FAUST TERM, and the whole reason closeness needs a
    // definition rather than a formula.
    //
    // Two components: a tight pair {a, b} and a looser triple {x, y, z} in a
    // path. Without the scaling, `a` reaches only `b` at distance 1 and scores
    // a perfect 1 — outranking `y`, which sits at the centre of a larger
    // component and is by any reasonable reading more central.
    //
    // With it: a reaches 1 of 4 others, so (1/1) x (1/4) = 0.25, while y
    // reaches 2 of 4 at total distance 2, giving (2/2) x (2/4) = 0.5.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let x = node(&g, "x");
    let y = node(&g, "y");
    let z = node(&g, "z");
    edge(&g, a, b);
    edge(&g, x, y);
    edge(&g, y, z);

    let s = floats(&g, Algorithm::Closeness, Dir::Both);
    assert!(close(s[0], 0.25), "a scores 0.25, got {}", s[0]);
    assert!(close(s[3], 0.5), "y scores 0.5, got {}", s[3]);
    assert!(
        s[3] > s[0],
        "the centre of the LARGER component must outrank a node in a tight pair — without \
         the reachability scaling the pair scores a perfect 1 and wins, which is the whole \
         defect the scaling exists to fix",
    );
}

#[test]
fn a_node_that_reaches_nothing_scores_zero_rather_than_dividing_by_zero() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let _isolated = node(&g, "i");
    edge(&g, a, b);
    let s = floats(&g, Algorithm::Closeness, Dir::Both);
    assert!(
        close(s[2], 0.0),
        "an isolated node scores 0, not NaN or infinity — got {}",
        s[2],
    );
    assert!(
        s.iter().all(|v| v.is_finite()),
        "every score must be finite, got {s:?}",
    );
}

#[test]
fn closeness_follows_direction_when_the_projection_is_directed() {
    // DIRECTED a -> b -> c. `a` reaches both; `c` reaches nothing and scores
    // zero. Undirected, `c` would score what `a` does — so this distinguishes
    // a directed run from one that quietly symmetrised.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);

    let s = floats(&g, Algorithm::Closeness, Dir::Out);
    assert!(s[0] > 0.0, "a reaches two nodes, got {}", s[0]);
    assert!(
        close(s[2], 0.0),
        "c reaches nothing following the arrows and must score 0, got {} — a nonzero value \
         means the projection was symmetrised",
        s[2],
    );
}

// ─── The procedure surface, and negatives ──────────────────────────────────

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

#[test]
fn both_procedures_answer_in_every_mode() {
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    let c = node(&g, "c");
    edge(&g, a, b);
    edge(&g, b, c);
    edge(&g, c, a);

    for (alg, col, prop) in [
        ("scc", "componentId", "comp"),
        ("closeness", "score", "clo"),
    ] {
        assert_eq!(
            rows(
                &g,
                &format!(
                    "CALL engram.algo.{alg}.stream({{nodeLabels:['N'], relationshipTypes:['R']}}) \
                     YIELD {col} RETURN count({col})"
                ),
            )[0][0],
            Value::Int(3),
            "{alg}.stream must answer one row per node",
        );
        assert!(
            !rows(
                &g,
                &format!(
                    "CALL engram.algo.{alg}.stats({{nodeLabels:['N']}}) YIELD nodeCount \
                     RETURN nodeCount"
                ),
            )
            .is_empty(),
            "{alg}.stats must answer",
        );
        assert!(
            !rows(
                &g,
                &format!(
                    "CALL engram.algo.{alg}.mutate({{nodeLabels:['N'], mutateKey:'{alg}k'}}) \
                     YIELD mutateKey RETURN mutateKey"
                ),
            )
            .is_empty(),
            "{alg}.mutate must answer",
        );
        assert!(
            !rows(
                &g,
                &format!(
                    "CALL engram.algo.{alg}.write({{nodeLabels:['N'], writeProperty:'{prop}'}}) \
                     YIELD nodesWritten RETURN nodesWritten"
                ),
            )
            .is_empty(),
            "{alg}.write must answer",
        );
        assert_eq!(
            rows(
                &g,
                &format!("MATCH (n:N) WHERE n.{prop} IS NOT NULL RETURN count(n)"),
            )[0][0],
            Value::Int(3),
            "{alg}.write must land the property on every projected node",
        );
    }
}

#[test]
fn closeness_is_priced_against_the_all_pairs_ceiling_and_scc_is_not() {
    // Closeness runs a BFS from every source, so it is `O(V x E)` and belongs
    // behind the all-pairs ceiling. SCC is two traversals — `O(V + E)` — and
    // must NOT be refused by it, or the ceiling would be punishing an
    // algorithm that does not cost what it charges for.
    let g = g();
    let a = node(&g, "a");
    let b = node(&g, "b");
    edge(&g, a, b);
    edge(&g, b, a);

    g.set_algo_work_ceiling(1);
    let e = err(
        &g,
        "CALL engram.algo.closeness.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
         YIELD score RETURN count(score)",
    );
    assert!(
        e.contains("ENGRAM_ALGO_WORK_CEILING"),
        "closeness must be refused by the all-pairs ceiling, got {e}",
    );
    assert_eq!(
        rows(
            &g,
            "CALL engram.algo.scc.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
             YIELD componentId RETURN count(componentId)",
        )[0][0],
        Value::Int(2),
        "SCC is O(V + E) and must still answer with the all-pairs ceiling at its floor",
    );
    g.set_algo_work_ceiling(10_000_000_000);
}
