#![allow(non_snake_case)]
//! L1 cardinality model — the estimate is validated against the ACTUAL row
//! count on a uniform-degree graph, where the average-fan-out model is exact.
//! On uniform degree `estimate == actual` to the row; the tests assert that
//! equality, so a regression in the fan-out chaining fails here rather than
//! silently mis-ordering a join two layers up.

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::{Value, parse_statement};
use engram_graph::{Dir, Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// 20 Persons (id 0..19), each KNOWS the next three (mod 20): out-degree and
/// in-degree are EXACTLY 3, so every fan-out the model computes is exact. Plus
/// one City, for the independent-join case.
fn uniform_graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n = 20u64;
    let mut ids = Vec::new();
    for i in 0..n {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(i as i64));
        ids.push(g.create_node(&["Person".into()], &p).expect("person"));
    }
    for i in 0..n {
        for k in 1..=3u64 {
            g.create_rel(
                ids[i as usize],
                "KNOWS",
                ids[((i + k) % n) as usize],
                &BTreeMap::new(),
            )
            .expect("knows");
        }
    }
    g.create_node(&["City".into()], &BTreeMap::new())
        .expect("city");
    g
}

fn actual_rows(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .len()
}

fn estimate(g: &Graph, src: &str) -> f64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    g.estimate_match_rows(&q).expect("a leading MATCH")
}

#[test]
fn hop_fanout_is_count_hop_over_start_count() {
    let g = uniform_graph();
    let person = vec!["Person".to_string()];
    let knows = vec!["KNOWS".to_string()];
    // 60 KNOWS edges / 20 Person = 3.0 each way; Both sums the two directions.
    assert_eq!(g.hop_fanout(&person, Dir::Out, &knows, &person), 3.0);
    assert_eq!(g.hop_fanout(&person, Dir::In, &knows, &person), 3.0);
    assert_eq!(g.hop_fanout(&person, Dir::Both, &knows, &person), 6.0);
    // No such start label → no rows, no divide-by-zero.
    assert_eq!(
        g.hop_fanout(&["Ghost".into()], Dir::Out, &knows, &person),
        0.0
    );
}

#[test]
fn estimate_equals_actual_on_uniform_degree() {
    let g = uniform_graph();
    // Each case: the structural estimate is exact because degree is uniform.
    for (src, want) in [
        // full label scan then one hop: 20 * 3
        ("MATCH (p:Person)-[:KNOWS]->(f:Person) RETURN f", 60.0),
        // point-lookup seed then one hop: 1 * 3
        ("MATCH (p:Person {id: 0})-[:KNOWS]->(f) RETURN f", 3.0),
        // friends-of-friends (intermediates labelled, as SNB queries write
        // them): 1 * 3 * 3. An UNlabelled intermediate falls back to an
        // all-nodes denominator and only approximates.
        (
            "MATCH (p:Person {id: 0})-[:KNOWS]->(m:Person)-[:KNOWS]->(f:Person) RETURN f",
            9.0,
        ),
        // variable-length 1..2: 1 * (3 + 9)
        ("MATCH (p:Person {id: 0})-[:KNOWS*1..2]->(f) RETURN f", 12.0),
        // independent comma-join is a cartesian product: 20 * 1
        ("MATCH (p:Person), (c:City) RETURN p, c", 20.0),
    ] {
        let est = estimate(&g, src);
        let act = actual_rows(&g, src) as f64;
        assert_eq!(
            act, want,
            "`{src}`: the graph actually returns {act}, not {want}"
        );
        assert_eq!(
            est, want,
            "`{src}`: estimate {est} vs the exact {want} (== actual)"
        );
    }
}

#[test]
fn estimate_path_rows_respects_bound_variables() {
    let g = uniform_graph();
    // `(p)-[:KNOWS]->(f)` with p ALREADY bound is one seed, not a 20-node scan.
    let q = parse_statement("MATCH (p:Person)-[:KNOWS]->(f:Person) RETURN f").unwrap();
    let engram_cypher::stmt::Query::Single(s) = &q else {
        panic!()
    };
    let engram_cypher::stmt::Clause::Match { pattern, .. } = &s.clauses[0] else {
        panic!()
    };
    let path = &pattern.paths[0];

    let free = g.estimate_path_rows(path, &BTreeSet::new());
    let mut bound = BTreeSet::new();
    bound.insert("p".to_string());
    let with_p = g.estimate_path_rows(path, &bound);

    assert_eq!(
        free, 60.0,
        "unbound start scans all 20 Persons then fans out ×3"
    );
    assert_eq!(with_p, 3.0, "a bound start is one seed then fans out ×3");
}
