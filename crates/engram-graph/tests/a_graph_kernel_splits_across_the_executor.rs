//! LCC (both definitions), the triangle count and label propagation split their
//! vertices across the executor, and every answer is the one thread's.
//!
//! They ran serially until rev56: on the Graphalytics S set's densest graph,
//! dota-league (61,170 vertices, 50,870,313 edges, mean degree ~1,663), a
//! serial LCC is thousands of seconds against the 900 s ceiling. The triangle
//! count is now the forward algorithm over a degree-oriented CSR -- each
//! triangle found once, at its lowest-ranked vertex, as a merge of two sorted
//! rows -- with integer counts added atomically; a label propagation round is
//! a pure function of read-only inputs, vertex by vertex. The directed LCC
//! was one too until rev61, and O(sum d^2): on dota-league one thread was
//! still merging a morsel of hubs after 50 minutes. It now counts on the
//! triangle enumeration as well (`the_directed_lcc_matches_its_definition`).
//!
//! Each statement runs at width 1 and on four real threads, and the two
//! renderings must be identical -- floats included, since every coefficient is
//! computed from the same integers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Four scoped threads claiming morsels, and a count of the calls it served.
struct ThreadedExec {
    calls: AtomicUsize,
}

impl ScopedExec for ThreadedExec {
    fn width(&self) -> usize {
        4
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4.min(n.max(1)) {
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

/// 3,000 vertices: a hub joined to 600 of them, a 30-clique, reciprocal
/// pairs, a few multi-edges, and 15,000 pseudo-random edges.
fn world() -> Graph {
    world_and_edges().0
}

/// The graph, its node ids by index, and every edge created, by index.
fn world_and_edges() -> (Graph, Vec<u64>, Vec<(usize, usize)>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..3_000)
        .map(|_| g.create_node(&["N".to_string()], &none).expect("node"))
        .collect();
    let edges = std::cell::RefCell::new(Vec::new());
    let rel = |a: usize, b: usize| {
        g.create_rel(ids[a], "R", ids[b], &none).expect("rel");
        edges.borrow_mut().push((a, b));
    };
    for k in 1..=600 {
        rel(0, k * 4);
    }
    for a in 100..130 {
        for b in 100..130 {
            if a != b {
                rel(a, b);
            }
        }
    }
    for a in 200..260 {
        rel(a, a + 1);
        rel(a + 1, a);
        rel(a, a + 1);
    }
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..15_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = (x % 3_000) as usize;
        let b = ((x >> 20) % 3_000) as usize;
        if a != b {
            rel(a, b);
        }
    }
    // below the floor, or with the lever off (the server turns it on;
    // `--no-algo-parallel` is the A/B arm), a kernel never sees the executor
    g.set_algo_min_vertices(1);
    g.set_algo_parallel(true);
    let edges = edges.into_inner();
    (g, ids, edges)
}

fn rendered(g: &Graph, q: &str) -> String {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let rows = run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows;
    assert!(rows.len() > 1_000, "`{q}` answered {} rows", rows.len());
    format!("{rows:?}")
}

const SCOPE: &str = "nodeLabels: ['N'], relationshipTypes: ['R']";

#[test]
fn every_kernel_answers_as_one_thread_does() {
    let g = world();
    let statements = [
        format!(
            "CALL engram.algo.localclusteringcoefficient.stream({{{SCOPE}, orientation: 'UNDIRECTED'}}) \
             YIELD node, coefficient RETURN id(node) AS v, coefficient AS c ORDER BY v"
        ),
        format!(
            "CALL engram.algo.localclusteringcoefficient.stream({{{SCOPE}, orientation: 'NATURAL', \
             graphalytics: true}}) YIELD node, coefficient RETURN id(node) AS v, coefficient AS c ORDER BY v"
        ),
        format!(
            "CALL engram.algo.trianglecount.stream({{{SCOPE}, orientation: 'UNDIRECTED'}}) \
             YIELD node, triangleCount RETURN id(node) AS v, triangleCount AS t ORDER BY v"
        ),
        format!(
            "CALL engram.algo.labelpropagation.stream({{{SCOPE}, orientation: 'UNDIRECTED', \
             maxIterations: 10}}) YIELD node, communityId RETURN id(node) AS v, communityId AS c ORDER BY v"
        ),
        format!(
            "CALL engram.algo.labelpropagation.stream({{{SCOPE}, orientation: 'UNDIRECTED', \
             maxIterations: 10, graphalytics: true}}) YIELD node, communityId \
             RETURN id(node) AS v, communityId AS c ORDER BY v"
        ),
    ];
    for q in &statements {
        g.set_exec(None);
        let want = rendered(&g, q);
        let exec = Arc::new(ThreadedExec {
            calls: AtomicUsize::new(0),
        });
        g.set_exec(Some(exec.clone()));
        let got = rendered(&g, q);
        g.set_exec(None);
        assert_eq!(got, want, "`{q}` answered differently on four threads");
        assert!(
            exec.calls.load(Ordering::Relaxed) > 0,
            "`{q}` never reached the executor"
        );
    }
}

#[test]
fn the_triangle_count_matches_a_brute_force_count() {
    // An oracle that shares no code with the kernel: the undirected simple
    // graph from the edge list (a multi-edge is one edge, a direction is none),
    // and for each vertex the neighbour pairs that are themselves adjacent.
    let (g, ids, edges) = world_and_edges();
    let n = ids.len();
    let mut adj: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); n];
    for &(a, b) in &edges {
        adj[a].insert(b);
        adj[b].insert(a);
    }
    let mut want = vec![0i64; n];
    for v in 0..n {
        let nb: Vec<usize> = adj[v].iter().copied().collect();
        for (i, &a) in nb.iter().enumerate() {
            for &b in &nb[i + 1..] {
                if adj[a].contains(&b) {
                    want[v] += 1;
                }
            }
        }
    }
    assert!(want.iter().sum::<i64>() > 1_000, "the fixture closes few triangles");
    let index: BTreeMap<u64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let q = format!(
        "CALL engram.algo.trianglecount.stream({{{SCOPE}, orientation: 'UNDIRECTED'}})          YIELD node, triangleCount RETURN id(node) AS v, triangleCount AS t"
    );
    let s = parse_statement(&q).expect("parse");
    g.set_exec(Some(Arc::new(ThreadedExec {
        calls: AtomicUsize::new(0),
    })));
    let rows = run_query(&g, &s, BTreeMap::new()).expect("run").rows;
    g.set_exec(None);
    let mut got = vec![-1i64; n];
    for r in &rows {
        if let (Value::Int(id), Value::Int(t)) = (&r[0], &r[1]) {
            got[index[&(*id as u64)]] = *t;
        }
    }
    assert_eq!(got, want);
}

#[test]
fn the_triangle_count_is_the_textbook_one_on_a_small_graph() {
    // K4 (every vertex in three triangles) plus a pendant vertex (none), so the
    // forward algorithm's once-per-triangle claim is checked against a count
    // anyone can do by hand -- at width 1 and on four threads.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..5)
        .map(|_| g.create_node(&["N".to_string()], &none).expect("node"))
        .collect();
    for a in 0..4 {
        for b in (a + 1)..4 {
            g.create_rel(ids[a], "R", ids[b], &none).expect("rel");
        }
    }
    g.create_rel(ids[0], "R", ids[4], &none).expect("rel");
    g.set_algo_min_vertices(1);
    g.set_algo_parallel(true);
    let q = format!(
        "CALL engram.algo.trianglecount.stream({{{SCOPE}, orientation: 'UNDIRECTED'}}) \
         YIELD node, triangleCount RETURN id(node) AS v, triangleCount AS t ORDER BY v"
    );
    let s = parse_statement(&q).expect("parse");
    for exec in [
        None,
        Some(Arc::new(ThreadedExec {
            calls: AtomicUsize::new(0),
        }) as Arc<dyn ScopedExec>),
    ] {
        g.set_exec(exec);
        let rows = run_query(&g, &s, BTreeMap::new()).expect("run").rows;
        let counts: Vec<Value> = rows.iter().map(|r| r[1].clone()).collect();
        assert_eq!(
            counts,
            vec![Value::Int(3), Value::Int(3), Value::Int(3), Value::Int(3), Value::Int(0)]
        );
    }
    g.set_exec(None);
}

#[test]
fn the_directed_lcc_matches_its_definition() {
    // rev61 counts the Graphalytics numerator per TRIANGLE -- each corner
    // credited the directed edges between the other two -- where rev56 merged
    // every vertex's neighbour set against each neighbour's out-row. The
    // oracle here is the spec's definition and shares no code with either:
    // N(v) is v's in- and out-neighbours without v, and the numerator counts
    // ORDERED pairs (u, w) of N(v), u != w, with an edge u -> w. Self-loops
    // are added, since a loop must never count, and a multi-edge is one edge.
    let (g, ids, mut edges) = world_and_edges();
    let none = BTreeMap::new();
    for k in [0usize, 105, 205, 1_500, 2_999] {
        g.create_rel(ids[k], "R", ids[k], &none).expect("loop");
        edges.push((k, k));
    }
    let n = ids.len();
    let e: std::collections::BTreeSet<(usize, usize)> = edges.iter().copied().collect();
    let mut nb: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); n];
    for &(a, b) in &edges {
        if a != b {
            nb[a].insert(b);
            nb[b].insert(a);
        }
    }
    let mut num = vec![0u64; n];
    for v in 0..n {
        for &u in &nb[v] {
            for &w in &nb[v] {
                if u != w && e.contains(&(u, w)) {
                    num[v] += 1;
                }
            }
        }
    }
    assert!(num.iter().sum::<u64>() > 1_000, "the fixture closes few triangles");
    assert!(
        num.iter().any(|c| c % 2 == 1),
        "no one-way edge closes a triangle, so the direction bits are untested"
    );
    let want: Vec<f64> = (0..n)
        .map(|v| {
            let d = nb[v].len() as f64;
            if d < 2.0 { 0.0 } else { num[v] as f64 / (d * (d - 1.0)) }
        })
        .collect();
    let index: BTreeMap<u64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let q = format!(
        "CALL engram.algo.localclusteringcoefficient.stream({{{SCOPE}, orientation: 'NATURAL', \
         graphalytics: true}}) YIELD node, coefficient RETURN id(node) AS v, coefficient AS c"
    );
    let s = parse_statement(&q).expect("parse");
    for exec in [
        None,
        Some(Arc::new(ThreadedExec {
            calls: AtomicUsize::new(0),
        }) as Arc<dyn ScopedExec>),
    ] {
        g.set_exec(exec);
        let rows = run_query(&g, &s, BTreeMap::new()).expect("run").rows;
        let mut got = vec![f64::NAN; n];
        for r in &rows {
            if let (Value::Int(id), Value::Float(c)) = (&r[0], &r[1]) {
                got[index[&(*id as u64)]] = *c;
            }
        }
        let wrong: Vec<(usize, f64, f64)> = (0..n)
            .filter(|&v| got[v].to_bits() != want[v].to_bits())
            .map(|v| (v, got[v], want[v]))
            .take(10)
            .collect();
        assert!(wrong.is_empty(), "coefficients differ from the definition: {wrong:?}");
    }
    g.set_exec(None);
}
