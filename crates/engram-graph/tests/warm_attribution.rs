#![allow(non_snake_case)]
//! Track S: `Graph::warm` reports what its adjacency tables HOLD and what they
//! ALLOCATED, so the served process's memory can be attributed from the log
//! instead of inferred from `SlimAdj` arithmetic. The two are different
//! numbers by construction (both builders grow by `push`), and the difference
//! was measured to be virtual — 2,767 MB of capacity on served SF3 cost 0 B
//! of RSS, so a shrink at publish was built, measured and removed. What
//! remains is the report, and this is its contract.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::{Graph, WarmReport};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// One adjacency row as the table stores it: `SlimAdj { rel: u64, type_token:
/// u32, peer: u64 }` — 24 bytes with the padding the compiler adds.
const ENTRY_BYTES: usize = 24;

/// `edges` KNOWS edges over a chain of persons plus `edges / 3` LIKES edges:
/// two types, so the warm publishes an untyped and two typed tables per
/// direction, and every edge is held FOUR times (untyped + typed, out + in).
fn warmed(edges: usize) -> WarmReport {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let e = BTreeMap::new();
    let persons: Vec<u64> = (0..=edges as i64)
        .map(|i| {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(i));
            g.create_node(&["Person".into()], &p).expect("node")
        })
        .collect();
    for w in persons.windows(2) {
        g.create_rel(w[0], "KNOWS", w[1], &e).expect("knows");
    }
    for i in 0..edges / 3 {
        g.create_rel(persons[i], "LIKES", persons[(i * 7 + 3) % persons.len()], &e).expect("likes");
    }
    g.warm()
}

#[test]
fn held_bytes_count_every_edge_four_times_plus_the_offsets() {
    let w = warmed(1000);
    assert!(w.tables >= 6, "untyped + 2 typed per direction expected, got {}", w.tables);
    let edges = w.out_edges;
    assert_eq!(w.in_edges, edges, "the two directions hold different edge counts");
    // Entries: untyped + typed copies, both directions. Offsets: at least one
    // u32 per table (the closing offset), at most one per node id per table.
    let entries = 4 * edges * ENTRY_BYTES;
    assert!(
        w.table_bytes >= entries && w.table_bytes <= entries + w.tables * (w.nodes + 2) * 4,
        "held {} B is not {} B of entries plus offsets for {} tables over {} nodes",
        w.table_bytes,
        entries,
        w.tables,
        w.nodes
    );
}

#[test]
fn allocated_is_at_least_held_and_below_twice_it() {
    // Push growth: capacity in [len, 2·len). Not asserted equal — the
    // difference is real and reported; it is simply not resident.
    let w = warmed(1000);
    assert!(w.table_capacity_bytes >= w.table_bytes, "allocated {} < held {}", w.table_capacity_bytes, w.table_bytes);
    assert!(
        w.table_capacity_bytes < 2 * w.table_bytes,
        "allocated {} B is not below twice the {} B held — that is not push growth",
        w.table_capacity_bytes,
        w.table_bytes
    );
}

#[test]
fn the_report_grows_with_the_corpus_so_it_is_read_not_fixed() {
    let small = warmed(300);
    let large = warmed(3000);
    assert!(large.table_bytes > 5 * small.table_bytes, "{} vs {}", large.table_bytes, small.table_bytes);
    assert!(large.table_capacity_bytes > large.table_bytes / 2);
}
