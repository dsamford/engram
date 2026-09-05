#![allow(non_snake_case)]
//! Anti-pattern GRAPH workloads: supernodes, degenerate vector populations,
//! deep chains, dense patterns. Each must stay CORRECT under the shape that
//! usually breaks it — wrong answers here look exactly like answers.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

#[test]
fn a_supernode_with_20k_rels_answers_exactly_in_both_directions() {
    // The classic graph anti-pattern. Counts must be exact, and the LEAF
    // side of each edge must see exactly one.
    let g = graph();
    let hub = g
        .create_node(&["Hub".into()], &BTreeMap::new())
        .expect("hub");
    let fan = 20_000usize;
    for i in 0..fan {
        let leaf = g
            .create_node(
                &["Leaf".into()],
                &BTreeMap::from([("i".to_string(), Value::Int(i as i64))]),
            )
            .expect("leaf");
        g.create_rel(hub, "FANS", leaf, &BTreeMap::new())
            .expect("rel");
    }
    let r = run(&g, "MATCH (h:Hub)-[:FANS]->(l:Leaf) RETURN count(l) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(fan as i64)]],
        "outgoing fan exact"
    );
    let r = run(
        &g,
        "MATCH (l:Leaf)<-[:FANS]-(h:Hub) WHERE l.i = 12345 RETURN count(h) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "one edge from the leaf side"
    );
    // A bounded var-length step THROUGH the supernode must not multiply.
    let r = run(
        &g,
        "MATCH (a:Leaf {i: 7})<-[:FANS*1..2]-(h) RETURN count(DISTINCT h) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "only the hub is 1..2 back"
    );
}

#[test]
fn identical_and_zero_vectors_never_panic_and_report_their_skips() {
    // A degenerate embedding population: hundreds of IDENTICAL vectors (tie
    // storms in any ANN structure) plus zero-norm rows (undefined cosine).
    // The query must return k rows, skip the zeros, and never panic.
    let g = graph();
    run_stmt(
        &g,
        &parse_any("CREATE VECTOR INDEX vi FOR (v:V) ON (v.e)").expect("ddl"),
        BTreeMap::new(),
    )
    .expect("index");
    for i in 0..500i64 {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(i));
        props.insert(
            "e".to_string(),
            Value::List(vec![
                Value::Float(1.0),
                Value::Float(0.0),
                Value::Float(0.0),
            ]),
        );
        g.create_node(&["V".into()], &props).expect("node");
    }
    for i in 0..25i64 {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(1_000 + i));
        props.insert(
            "e".to_string(),
            Value::List(vec![
                Value::Float(0.0),
                Value::Float(0.0),
                Value::Float(0.0),
            ]),
        );
        g.create_node(&["V".into()], &props)
            .expect("zero-norm node");
    }
    let (rows, plan) = g
        .vector_query("vi", 10, &[1.0, 0.0, 0.0])
        .expect("a degenerate population is still a population");
    assert_eq!(rows.len(), 10, "k rows despite total ties");
    assert_eq!(plan.skipped, 25, "every zero-norm row skipped and SAID so");
    for (_, score) in &rows {
        assert!(
            (score - 1.0).abs() < 1e-9,
            "ties all score 1.0, got {score}"
        );
    }
}

#[test]
fn a_5k_deep_chain_bounds_var_length_exactly() {
    // Deep-chain traversal: [*1..3] from the head must reach exactly 3
    // nodes, never walk the whole chain, and an exact-depth probe from the
    // head must land on the right node.
    let g = graph();
    let n = 5_000usize;
    let mut prev = g
        .create_node(
            &["C".into()],
            &BTreeMap::from([("i".to_string(), Value::Int(0))]),
        )
        .expect("head");
    for i in 1..n {
        let next = g
            .create_node(
                &["C".into()],
                &BTreeMap::from([("i".to_string(), Value::Int(i as i64))]),
            )
            .expect("node");
        g.create_rel(prev, "NEXT", next, &BTreeMap::new())
            .expect("rel");
        prev = next;
    }
    let r = run(
        &g,
        "MATCH (a:C {i: 0})-[:NEXT*1..3]->(b) RETURN count(b) AS c",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(3)]], "the bound is the bound");
    let r = run(&g, "MATCH (a:C {i: 100})-[:NEXT*5]->(b) RETURN b.i AS i");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(105)]],
        "exact depth lands exactly"
    );
}

#[test]
fn a_cartesian_product_under_LIMIT_stays_exact() {
    // The strange-workload classic: an unconstrained cross product. With
    // LIMIT it must produce exactly LIMIT rows; the aggregate over the full
    // product must be the arithmetic answer.
    let g = graph();
    for i in 0..60i64 {
        g.create_node(
            &["A".into()],
            &BTreeMap::from([("i".to_string(), Value::Int(i))]),
        )
        .expect("a");
        g.create_node(
            &["B".into()],
            &BTreeMap::from([("i".to_string(), Value::Int(i))]),
        )
        .expect("b");
    }
    let r = run(&g, "MATCH (a:A), (b:B) RETURN count(*) AS c");
    assert_eq!(r.rows, vec![vec![Value::Int(3_600)]], "60 × 60 exactly");
    let r = run(&g, "MATCH (a:A), (b:B) RETURN a.i, b.i LIMIT 17");
    assert_eq!(r.rows.len(), 17, "LIMIT caps the product");
}

#[test]
fn a_dense_clique_pattern_counts_exactly() {
    // A 40-node complete digraph (1,560 edges): triangle counting is the
    // canonical dense-pattern stressor, and the closed-form answer leaves
    // no room to be approximately right.
    let g = graph();
    let n = 40usize;
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            g.create_node(
                &["K".into()],
                &BTreeMap::from([("i".to_string(), Value::Int(i as i64))]),
            )
            .expect("node")
        })
        .collect();
    for (i, &a) in ids.iter().enumerate() {
        for (j, &b) in ids.iter().enumerate() {
            if i != j {
                g.create_rel(a, "E", b, &BTreeMap::new()).expect("rel");
            }
        }
    }
    // Directed 3-cycles through distinct nodes: n·(n−1)·(n−2) paths.
    let r = run(
        &g,
        "MATCH (a:K)-[:E]->(b:K)-[:E]->(c:K)-[:E]->(a) \
         WHERE a <> b AND b <> c AND a <> c RETURN count(*) AS c",
    );
    let want = (n * (n - 1) * (n - 2)) as i64;
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(want)]],
        "closed-form triangle count"
    );
}

#[test]
fn write_read_interleave_through_cypher_reads_its_own_writes() {
    // The simultaneous-read-write shape at the Cypher layer: every write is
    // immediately visible to the next statement, across 500 rounds with
    // periodic seals underneath.
    let g = graph();
    for i in 0..500i64 {
        let create = format!("CREATE (:W {{i: {i}}})");
        run(&g, &create);
        let r = run(&g, "MATCH (w:W) RETURN count(w) AS c");
        assert_eq!(
            r.rows,
            vec![vec![Value::Int(i + 1)]],
            "round {i} sees its write"
        );
        if i % 100 == 99 {
            g.shared_store().seal();
            g.shared_store().compact();
        }
    }
}
