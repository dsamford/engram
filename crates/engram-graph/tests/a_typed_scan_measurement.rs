#![allow(non_snake_case)]
// A real clock: this file is a MEASUREMENT.
#![allow(clippy::disallowed_methods)]
//! The MEASUREMENT behind `a_typed_relationship_scan_reads_only_that_type`.
//!
//! `#[ignore]`d: it builds a large graph and reports wall-clock, which is a
//! number to read, not a threshold to assert. A timing assertion here would
//! be flaky on shared CI and would say nothing the row tests do not.
//!
//!   cargo test -p engram-graph --test a_typed_scan_measurement -- --ignored --nocapture

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).expect("parse");
    run_query(g, &q, BTreeMap::new()).expect("run").rows
}

#[test]
#[ignore = "measurement, not an assertion"]
fn how_much_of_the_partition_a_typed_match_reads() {
    const HAYSTACK: i64 = 300_000;
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["N".into()], &m).expect("node")
    };
    let ns: Vec<u64> = (0..=HAYSTACK).map(&mut mk).collect();
    // The haystack FIRST, so it owns the low relationship ids and a partition
    // scan must cross all of it to reach the needles — the id-order effect.
    for w in ns.windows(2) {
        g.create_rel(w[0], "common", w[1], &BTreeMap::new())
            .expect("common");
    }
    for i in 0..4usize {
        g.create_rel(ns[i * 3], "RARE", ns[i * 3 + 1], &BTreeMap::new())
            .expect("rare");
    }
    let _ = g.warm();

    const Q: &str = "MATCH (a)-[r:RARE]->(b) RETURN id(r)";

    let t = Instant::now();
    let fast = rows(&g, Q);
    let fast_ms = t.elapsed().as_secs_f64() * 1000.0;

    g.set_adj_table_max_entries(0); // force the partition scan
    let t = Instant::now();
    let slow = rows(&g, Q);
    let slow_ms = t.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(fast.len(), slow.len(), "the two paths disagree");
    println!(
        "\n{HAYSTACK} `common` edges + {} `RARE`\n  \
         adjacency walk : {fast_ms:8.2} ms\n  \
         partition scan : {slow_ms:8.2} ms\n  \
         ratio          : {:8.1}x\n",
        fast.len(),
        slow_ms / fast_ms.max(0.0001)
    );
}
