#![allow(non_snake_case)]
// Real OS threads exercising the D2-revised concurrent write path. The store and
// graph are `Send + Sync` now, so N threads sharing one `Graph` via `Arc` is
// legitimate — and this test is the proof the allocation path holds under it.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

//! Concurrent graph writes: N threads creating nodes must allocate DISTINCT ids
//! and mint the SHARED label/prop tokens exactly once, with no lost updates —
//! the allocation-latch fix for `next_id`/`token` under the D2 revision. Before
//! the latch, two threads read the same id/token counter and mint duplicates.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

#[test]
fn concurrent_node_creation_allocates_distinct_ids_and_shared_tokens() {
    let graph = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    const THREADS: usize = 8;
    const PER: usize = 100;

    let mut handles = Vec::with_capacity(THREADS);
    for ti in 0..THREADS {
        let graph = Arc::clone(&graph);
        handles.push(std::thread::spawn(move || {
            let mut ids = Vec::with_capacity(PER);
            for j in 0..PER {
                // Same label + property NAMES across every thread, so their
                // TOKENS are minted concurrently the first time each appears —
                // exercising the `token` race alongside the `next_id` race.
                let mut props = BTreeMap::new();
                props.insert("thread".to_string(), Value::Int(ti as i64));
                props.insert("seq".to_string(), Value::Int(j as i64));
                let id = graph
                    .create_node(&["Thing".to_string()], &props)
                    .expect("create_node");
                ids.push(id);
            }
            ids
        }));
    }

    let mut all_ids = Vec::new();
    for h in handles {
        all_ids.extend(h.join().expect("thread"));
    }

    let total = THREADS * PER;

    // 1) No DUPLICATE ids — the `next_id` counter read-modify-write race is
    //    fixed. A duplicate would mean two nodes collided on one key.
    assert_eq!(all_ids.len(), total, "every create_node returned");
    let mut uniq = all_ids.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(
        uniq.len(),
        total,
        "duplicate ids allocated under concurrency — the id counter race is not fixed"
    );

    // 2) Every node landed with its membership + the maintained stats agree —
    //    the concurrent writes did not lose a node or corrupt the shared
    //    label/prop token counters.
    assert_eq!(graph.count_all_nodes(), total as u64, "stats node count");
    assert_eq!(
        graph.count_label_nodes("Thing"),
        total as u64,
        "membership count for the shared label"
    );
}
