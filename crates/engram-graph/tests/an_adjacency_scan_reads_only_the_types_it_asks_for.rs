//! A node's adjacency, asked for by TYPE, is read by a prefix scan of
//! `tag | node | type` per type — never the node's whole row filtered after.
//!
//! The key is `tag | node | type | peer | rel`, so the type is the next key
//! field after the node. `rels_of`, `incident_rel_ids`, `count_adjacent`, the
//! hop-count fallback and the table repair all scanned `tag | node` and
//! dropped the other types in memory. SNB BI bi11 expands `(a)-[k1:KNOWS]-(b)`
//! from 3,613 people; each person's row holds their LIKES, every HAS_CREATOR
//! from their messages and their forum memberships — ~3,400 rows per scan to
//! keep ~40 KNOWS, 24.7M row visits in leg 1 alone.
//!
//! The ORDER must not move: the key is type-major, so the narrowed scans,
//! ascending by type token, concatenate to exactly the filtered whole-row
//! order — which a delete's commit log (`incident_rel_ids`) and a repaired
//! adjacency table (`adj_row_for`, byte-identical to a rebuild) depend on.
//! Pinned here against the unfiltered call, filtered by hand, with a
//! transaction's buffered edges included.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn node(g: &Graph, label: &str, id: i64) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Int(id));
    g.create_node(&[label.to_string()], &m).expect("node")
}

fn rel(g: &Graph, a: u64, t: &str, b: u64, w: i64) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("w".to_string(), Value::Int(w));
    g.create_rel(a, t, b, &m).expect("rel")
}

/// A hub with 600 LIKES and 400 MEMBER_OF edges out, 800 CREATED edges in,
/// and a handful of KNOWS each way — the shape of an SNB person's row.
fn hub() -> (Graph, u64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let h = node(&g, "Person", 0);
    let mut next = 1;
    let mut make = |label: &str| {
        next += 1;
        node(&g, label, next)
    };
    for i in 0..600 {
        let m = make("Message");
        rel(&g, h, "LIKES", m, i);
    }
    for i in 0..400 {
        let f = make("Forum");
        rel(&g, h, "MEMBER_OF", f, i);
    }
    for i in 0..800 {
        let m = make("Message");
        rel(&g, m, "CREATED", h, i);
    }
    for i in 0..7 {
        let p = make("Person");
        rel(&g, h, "KNOWS", p, 1000 + i);
        rel(&g, p, "KNOWS", h, 2000 + i);
    }
    (g, h)
}

fn only(g: &Graph, rels: Vec<u64>, types: &[&str]) -> Vec<u64> {
    rels.into_iter()
        .filter(|&id| {
            let r = g.rel(id).expect("read").expect("present");
            types.contains(&r.rel_type.as_str())
        })
        .collect()
}

#[test]
fn a_typed_adjacency_read_equals_the_whole_row_filtered_in_order() {
    let (g, h) = hub();
    for dir in [Dir::Out, Dir::In, Dir::Both] {
        for types in [&["KNOWS"][..], &["KNOWS", "LIKES"][..], &["CREATED", "MEMBER_OF"][..]] {
            let owned: Vec<String> = types.iter().map(|t| t.to_string()).collect();
            let all = g.incident_rel_ids(h, dir, None).expect("all");
            let (narrow, t) = engram_observe::with_trace(|| {
                g.incident_rel_ids(h, dir, Some(&owned[..])).expect("narrow")
            });
            assert_eq!(
                narrow,
                only(&g, all, types),
                "{dir:?} {types:?}: the narrowed ids differ from the whole row filtered, or their order moved"
            );
            assert!(
                t.counters().get("graph.adjacency row scanned per type").copied().unwrap_or(0) > 0,
                "{dir:?} {types:?}: the read was not narrowed by type"
            );
            let rows: Vec<u64> = g.rels_of(h, dir, Some(&owned[..])).expect("rels").iter().map(|r| r.id).collect();
            assert_eq!(rows, narrow, "{dir:?} {types:?}: rels_of and incident_rel_ids disagree");
        }
    }
    let knows = ["KNOWS".to_string()];
    assert_eq!(g.rels_of(h, Dir::Out, Some(&knows[..])).expect("out").len(), 7);
    assert_eq!(g.rels_of(h, Dir::Both, Some(&knows[..])).expect("both").len(), 14);
}

#[test]
fn a_typed_adjacency_read_sees_the_transactions_own_edges() {
    let (g, h) = hub();
    let knows = ["KNOWS".to_string()];
    g.begin_txn().expect("begin");
    let p = node(&g, "Person", 99_999);
    let fresh = rel(&g, h, "KNOWS", p, 7);
    let ids = g.incident_rel_ids(h, Dir::Out, Some(&knows[..])).expect("ids");
    assert!(ids.contains(&fresh), "the buffered KNOWS edge was not seen by a narrowed read");
    assert_eq!(g.rels_of(h, Dir::Out, Some(&knows[..])).expect("rels").len(), 8);
    g.commit_txn().expect("commit");
    assert_eq!(g.rels_of(h, Dir::Out, Some(&knows[..])).expect("after").len(), 8);
}
