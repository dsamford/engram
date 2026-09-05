#![allow(non_snake_case)]
//! L5 — chunked posting lists, the concurrent-add race, and the half-edge seam.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use engram_key::{Namespace, Partition, Realm};
use engram_observe::with_crash_at;
use engram_runtime::{Runtime, SimRuntime};
use engram_store::adjacency::CHUNK_CAPACITY;
use engram_store::{
    EdgeDir, EdgeType, NodeAt, PeerRef, Store, add_edge, degree, find_half_edges, neighbors,
    remove_edge,
};

const ET: EdgeType = EdgeType(1);

fn node(id: u64) -> NodeAt {
    NodeAt {
        realm: Realm(1),
        ns: Namespace(1),
        partition: Partition(1),
        node: id,
    }
}

fn run_async<F: std::future::Future<Output = ()> + 'static>(seed: u64, f: F) {
    let rt = SimRuntime::new(seed);
    rt.spawn(f);
    rt.run(10_000_000).expect("completes");
}

#[test]
fn an_edge_is_visible_from_BOTH_endpoints() {
    let s = Store::new();
    let store = s.clone();
    run_async(1, async move {
        add_edge(&store, node(1), ET, node(2)).await.expect("edge");
    });
    assert_eq!(
        neighbors(&s, node(1), EdgeDir::Out, ET).expect("out"),
        vec![PeerRef { ns: 1, id: 2 }]
    );
    assert_eq!(
        neighbors(&s, node(2), EdgeDir::In, ET).expect("in"),
        vec![PeerRef { ns: 1, id: 1 }]
    );
    assert!(
        find_half_edges(&s, node(1), ET, node(2))
            .expect("check")
            .is_empty()
    );
}

#[test]
fn add_edge_is_IDEMPOTENT() {
    // Retrying a half-applied add must not double an edge — the property that
    // makes crash-recovery replay safe for adjacency.
    let s = Store::new();
    let store = s.clone();
    run_async(1, async move {
        add_edge(&store, node(1), ET, node(2)).await.expect("first");
        add_edge(&store, node(1), ET, node(2))
            .await
            .expect("second");
    });
    assert_eq!(degree(&s, node(1), EdgeDir::Out, ET).expect("deg"), 1);
}

#[test]
fn neighbors_are_sorted_regardless_of_insertion_order() {
    let s = Store::new();
    let store = s.clone();
    run_async(1, async move {
        for id in [9u64, 3, 7, 1, 5] {
            add_edge(&store, node(100), ET, node(id))
                .await
                .expect("edge");
        }
    });
    let ids: Vec<u64> = neighbors(&s, node(100), EdgeDir::Out, ET)
        .expect("n")
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(ids, vec![1, 3, 5, 7, 9]);
}

#[test]
fn a_SUPERNODE_spans_chunks_and_loses_nothing() {
    // The measured corpus holds a node of degree 245,340. Three chunks' worth
    // here proves the rollover; the count proves nothing fell between them.
    let s = Store::new();
    let store = s.clone();
    let n = (CHUNK_CAPACITY * 2 + 100) as u64;
    run_async(1, async move {
        for id in 0..n {
            add_edge(&store, node(999), ET, node(id + 10_000))
                .await
                .expect("edge");
        }
    });
    assert_eq!(
        degree(&s, node(999), EdgeDir::Out, ET).expect("deg"),
        n as usize
    );
    let peers = neighbors(&s, node(999), EdgeDir::Out, ET).expect("n");
    assert!(
        peers.windows(2).all(|w| w[0] < w[1]),
        "sorted and deduped across chunks"
    );
}

#[test]
fn CONCURRENT_adds_to_one_node_lose_NOTHING() {
    // The lost-update test, on real data: eight writers, one node, distinct
    // peers, interleaved through the CAS suspension point. Every peer must
    // survive — a plain read-modify-write here silently drops all but the
    // last writer in a window.
    let s = Store::new();
    let done = Rc::new(RefCell::new(0u32));
    let rt = SimRuntime::new(42);
    for w in 0..8u64 {
        let store = s.clone();
        let rt2 = rt.clone();
        let done = done.clone();
        rt.spawn(async move {
            rt2.sleep(Duration::from_millis(w % 3)).await;
            add_edge(&store, node(500), ET, node(1000 + w))
                .await
                .expect("edge");
            *done.borrow_mut() += 1;
        });
    }
    rt.run(10_000_000).expect("completes");
    assert_eq!(*done.borrow(), 8);
    assert_eq!(
        degree(&s, node(500), EdgeDir::Out, ET).expect("deg"),
        8,
        "an edge was lost to the race"
    );
}

#[test]
fn remove_edge_removes_BOTH_directions() {
    let s = Store::new();
    let store = s.clone();
    run_async(1, async move {
        add_edge(&store, node(1), ET, node(2)).await.expect("add");
        remove_edge(&store, node(1), ET, node(2))
            .await
            .expect("remove");
    });
    assert_eq!(degree(&s, node(1), EdgeDir::Out, ET).expect("deg"), 0);
    assert_eq!(degree(&s, node(2), EdgeDir::In, ET).expect("deg"), 0);
    assert!(
        find_half_edges(&s, node(1), ET, node(2))
            .expect("check")
            .is_empty()
    );
}

#[test]
fn a_CROSS_NAMESPACE_edge_is_an_ordinary_entry() {
    let s = Store::new();
    let store = s.clone();
    let sys = NodeAt {
        realm: Realm(0),
        ns: Namespace(1),
        partition: Partition(1),
        node: 42,
    };
    run_async(1, async move {
        add_edge(&store, node(1), ET, sys).await.expect("edge");
    });
    let peers = neighbors(&s, node(1), EdgeDir::Out, ET).expect("n");
    assert_eq!(peers, vec![PeerRef { ns: 1, id: 42 }]);
    // The mirror lives on the system node, in ITS keyspace.
    assert_eq!(
        neighbors(&s, sys, EdgeDir::In, ET).expect("in"),
        vec![PeerRef { ns: 1, id: 1 }]
    );
}

#[test]
fn the_half_edge_window_EXISTS_and_the_checker_FINDS_it() {
    // Both halves pinned, deliberately: the crash between the two direction
    // writes leaves a half edge (the seam is real, not hidden), and
    // find_half_edges names it (the damage is detectable, not silent). When
    // multi-row commit closes the window, the first assertion flips and this
    // test becomes the proof of the fix.
    let s = Store::new();
    let store = s.clone();
    let crashed = with_crash_at("adjacency.between_out_and_in", || {
        run_async(1, async move {
            add_edge(&store, node(1), ET, node(2))
                .await
                .expect("never returns");
        });
    });
    assert!(crashed.is_err(), "the crash point must fire");

    let findings = find_half_edges(&s, node(1), ET, node(2)).expect("check");
    assert_eq!(
        findings.len(),
        1,
        "the half edge must be FOUND, not silently absorbed"
    );
    assert!(findings[0].contains("out=true in=false"), "{findings:?}");

    // And the repair is the idempotent re-add.
    let store = s.clone();
    run_async(2, async move {
        add_edge(&store, node(1), ET, node(2))
            .await
            .expect("repair");
    });
    assert!(
        find_half_edges(&s, node(1), ET, node(2))
            .expect("check")
            .is_empty()
    );
    assert_eq!(
        degree(&s, node(1), EdgeDir::Out, ET).expect("deg"),
        1,
        "the repair must not double"
    );
}

// ─── Structure, not just contents ──────────────────────────────────────────
//
// `neighbors` dedups, which blinds every content assertion to chunk-level
// defects. Canaries proved it: rollover removed and duplicates smuggled into a
// second chunk were both NOT DETECTED through the deduped API. These assert on
// `chunk_stats` — the raw view.

use engram_store::{StoredValue, chunk_key_body, chunk_stats};

#[test]
fn the_supernode_actually_SPANS_chunks() {
    // Degree alone passes with one unbounded chunk — the exact one-giant-value
    // regression the plan forbids. The chunk count is the real assertion.
    let s = Store::new();
    let store = s.clone();
    let n = (CHUNK_CAPACITY * 2 + 100) as u64;
    run_async(1, async move {
        for id in 0..n {
            add_edge(&store, node(998), ET, node(id + 10_000))
                .await
                .expect("edge");
        }
    });
    let (chunks, raw) = chunk_stats(&s, node(998), EdgeDir::Out, ET).expect("stats");
    assert_eq!(
        chunks, 3,
        "the list must roll over, not grow one chunk unboundedly"
    );
    assert_eq!(
        raw, n as usize,
        "no entry lost or doubled across the rollover"
    );
}

#[test]
fn re_adding_a_peer_in_a_FULL_chunk_does_not_smuggle_a_duplicate() {
    // The duplicate check must span every chunk. Checking only the last one
    // re-inserts a full-chunk peer into a later chunk, where neighbors' dedup
    // HIDES it — visible only in the raw count.
    let s = Store::new();
    let store = s.clone();
    let n = CHUNK_CAPACITY as u64;
    run_async(1, async move {
        for id in 0..n {
            add_edge(&store, node(997), ET, node(id + 10_000))
                .await
                .expect("edge");
        }
        // Chunk 0 is now exactly full. Re-add its first peer.
        add_edge(&store, node(997), ET, node(10_000))
            .await
            .expect("re-add");
    });
    let (chunks, raw) = chunk_stats(&s, node(997), EdgeDir::Out, ET).expect("stats");
    assert_eq!(
        raw, CHUNK_CAPACITY,
        "a duplicate was smuggled past the full chunk"
    );
    assert_eq!(chunks, 1, "the re-add must not open a second chunk");
}

#[test]
fn a_CORRUPT_chunk_is_refused_not_truncated() {
    // A 13-byte row is not a peer list. Truncating to the nearest multiple
    // silently loses an edge and reports the remainder as the degree — absence
    // read as a smaller, plausible answer. The read must refuse.
    let s = Store::new();
    let store = s.clone();
    run_async(1, async move {
        add_edge(&store, node(996), ET, node(1))
            .await
            .expect("edge");
    });
    // Overwrite the chunk row with garbage, straight through the store.
    let p = engram_key::KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: engram_key::Kind::ADJACENCY,
        partition: Partition(1),
    };
    let body = chunk_key_body(996, EdgeDir::Out, ET, 0);
    s.put(&p, &body, StoredValue::Plain(vec![0xAB; 13]))
        .expect("garbage lands");

    assert!(
        matches!(
            neighbors(&s, node(996), EdgeDir::Out, ET),
            Err(engram_store::AdjacencyError::CorruptChunk { chunk: 0 })
        ),
        "a corrupt chunk must refuse, not truncate",
    );
}
