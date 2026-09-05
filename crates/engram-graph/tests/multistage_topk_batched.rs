#![allow(non_snake_case)]
//! Track B working-set bound: the two-stage top-k tail (IC5/IC9's shape) folds
//! its stage-2 expansion into a bounded accumulator ONE DRIVING BATCH at a time,
//! so a high-fan-out expand never materialises the whole widened chunk. This
//! forces a genuine multi-batch split (carried set > the batch size) and pins
//! that (a) the batched path fired and (b) the answer is byte-identical to the
//! independently-computed top-k — batching changed memory, never results.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e}"))
        .rows
}

/// The IC5 shape's data: one hub (id 0) KNOWS `mids` middle persons (ids
/// `1..=mids`), and each middle person `i` FOLLOWS a distinct leaf with id
/// `10_000 + i` — so `(hub)-[:KNOWS]->(p) WITH p MATCH (p)-[:FOLLOWS]->(f)` has a
/// carried set of exactly `mids`, and every reachable `f.id` is unique (no ORDER
/// BY tie ambiguity). Returns the leaf id ceiling for the expected top-k.
fn build_hub(mids: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Int(0));
    let hub = g.create_node(&["Person".into()], &p).expect("hub");
    let empty = BTreeMap::new();
    for i in 1..=mids {
        let mut mp = BTreeMap::new();
        mp.insert("id".to_string(), Value::Int(i));
        let mid = g.create_node(&["Person".into()], &mp).expect("mid");
        g.create_rel(hub, "KNOWS", mid, &empty).expect("knows");

        let mut lp = BTreeMap::new();
        lp.insert("id".to_string(), Value::Int(10_000 + i));
        let leaf = g.create_node(&["Person".into()], &lp).expect("leaf");
        g.create_rel(mid, "FOLLOWS", leaf, &empty).expect("follows");
    }
    g.shared_store().seal();
    g
}

const QUERY: &str = "MATCH (hub:Person {id: 0})-[:KNOWS]->(p:Person) WITH p \
                     MATCH (p)-[:FOLLOWS]->(f:Person) \
                     RETURN f.id ORDER BY f.id DESC LIMIT 10";

#[test]
fn multistage_topk_batches_a_large_carry_and_stays_byte_identical() {
    // 1500 carried mids > MULTISTAGE_TOPK_BATCH (1024) → a genuine 2-batch split.
    let mids = 1500i64;
    let g = build_hub(mids);

    let (got, trace) = engram_observe::with_trace(|| rows(&g, QUERY));

    // (a) The batched path actually fired (more than one batch).
    assert!(
        trace
            .counters()
            .get("interp.pipeline top-k batched")
            .copied()
            .unwrap_or(0)
            > 0,
        "the bounded batched top-k path did not fire on a 1500-row carry; \
         counters: {:?}",
        trace.counters()
    );

    // (b) Byte-identical to the independent top-k: leaf ids are 10_001..=11_500,
    // so the 10 largest f.id DESC are 11_500, 11_499, …, 11_491.
    let want: Vec<Vec<Value>> = (0..10)
        .map(|k| vec![Value::Int(10_000 + mids - k)])
        .collect();
    assert_eq!(got, want, "batched multistage top-k diverged from expected");
}

#[test]
fn multistage_topk_small_carry_takes_the_single_batch_route() {
    // A carried set that fits one batch must NOT record a split — proving small
    // (warm/resident benchmark) queries keep the identical single-push route,
    // while still returning the correct answer.
    let mids = 50i64;
    let g = build_hub(mids);
    let (got, trace) = engram_observe::with_trace(|| rows(&g, QUERY));
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline top-k batched")
            .copied()
            .unwrap_or(0),
        0,
        "a 50-row carry should be a single batch, not a split"
    );
    let want: Vec<Vec<Value>> = (0..10)
        .map(|k| vec![Value::Int(10_000 + mids - k)])
        .collect();
    assert_eq!(got, want, "single-batch top-k wrong");
}
