#![allow(non_snake_case)]
//! Track B working-set bound, increment 2: the N-stage `run_pipeline`'s LAST
//! stage batches its expansion when the tail is a GROUPED aggregate (IC3's
//! per-friend `sum` shape). Each group lives in one batch (the driving var is
//! DISTINCT-carried), so per-batch reduced groups concatenate byte-identically.
//! This forces a real multi-batch split (>1024 distinct driving rows) and pins
//! byte-identity two ways: an in-process A/B (batching on vs off) AND a computed
//! expected top-k.

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

/// A 3-MATCH pipeline fixture: one hub (H id 0) `-E0->` `mids` M-nodes (id
/// `1..=mids`); each M `-E1->` a W (so it survives the middle stage) and `-E2->`
/// one L leaf whose `val` equals the M's id. So the final stage
/// `MATCH (m)-[:E2]->(leaf) RETURN m.id, sum(leaf.val)` yields `sum = m.id` per
/// distinct m — a clean, tie-free ORDER BY.
fn build_pipeline(mids: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let empty = BTreeMap::new();
    let mut hp = BTreeMap::new();
    hp.insert("id".to_string(), Value::Int(0));
    let hub = g.create_node(&["H".into()], &hp).expect("hub");
    for i in 1..=mids {
        let mut mp = BTreeMap::new();
        mp.insert("id".to_string(), Value::Int(i));
        let m = g.create_node(&["M".into()], &mp).expect("m");
        g.create_rel(hub, "E0", m, &empty).expect("e0");
        let w = g.create_node(&["W".into()], &empty).expect("w");
        g.create_rel(m, "E1", w, &empty).expect("e1");
        let mut lp = BTreeMap::new();
        lp.insert("val".to_string(), Value::Int(i));
        let leaf = g.create_node(&["L".into()], &lp).expect("leaf");
        g.create_rel(m, "E2", leaf, &empty).expect("e2");
    }
    g.shared_store().seal();
    g
}

const QUERY: &str = "MATCH (hub:H {id: 0})-[:E0]->(m:M) WITH DISTINCT m \
                     MATCH (m)-[:E1]->(w:W) WITH DISTINCT m \
                     MATCH (m)-[:E2]->(leaf:L) \
                     RETURN m.id AS mid, sum(leaf.val) AS s ORDER BY s DESC, mid ASC LIMIT 10";

#[test]
fn pipeline_agg_batches_a_large_carry_and_stays_byte_identical() {
    let mids = 1500i64; // > MULTISTAGE_TOPK_BATCH (1024) → a genuine split
    let g = build_pipeline(mids);

    // Batching ON (default): the aggregate-batched path must fire.
    let (got_on, trace) = engram_observe::with_trace(|| rows(&g, QUERY));
    assert!(
        trace
            .counters()
            .get("interp.pipeline agg batched")
            .copied()
            .unwrap_or(0)
            > 0,
        "the N-stage aggregate-batched path did not fire; counters: {:?}",
        trace.counters()
    );

    // In-process A/B: the SAME query with batching OFF must return the SAME rows.
    g.set_multistage_topk_batch(false);
    let got_off = rows(&g, QUERY);
    assert_eq!(
        got_on, got_off,
        "batched N-stage aggregate diverged from the whole-chunk expand"
    );

    // Computed expected: sum == m.id, so top 10 by s DESC are ids 1500..=1491.
    let want: Vec<Vec<Value>> = (0..10)
        .map(|k| vec![Value::Int(mids - k), Value::Int(mids - k)])
        .collect();
    assert_eq!(got_on, want, "batched N-stage aggregate wrong");
}

#[test]
fn pipeline_agg_small_carry_takes_the_single_batch_route() {
    let mids = 40i64; // < batch size → no split recorded
    let g = build_pipeline(mids);
    let (got, trace) = engram_observe::with_trace(|| rows(&g, QUERY));
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline agg batched")
            .copied()
            .unwrap_or(0),
        0,
        "a 40-row carry should be a single batch, not a split"
    );
    let want: Vec<Vec<Value>> = (0..10)
        .map(|k| vec![Value::Int(mids - k), Value::Int(mids - k)])
        .collect();
    assert_eq!(got, want, "single-batch N-stage aggregate wrong");
}
