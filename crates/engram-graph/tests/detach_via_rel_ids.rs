#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! `delete_node` enumerates incident relationship IDS instead of decoding every
//! incident relationship record.
//!
//! The loop only ever read `r.id`, and `delete_rel` fetches and decodes the
//! record again under its own latch — so `rels_of`'s decode was pure waste. The
//! id is already in the adjacency KEY (`tag | node | type | peer | rel`).
//!
//! Two things must not move, and both are subtle:
//!
//! 1. **`StillConnected` stays exact.** `rels_of` silently drops an adjacency
//!    row whose relationship record is absent, so it refuses only when a LIVE
//!    relationship exists. Enumerating ids does not know that, so the existence
//!    check is explicit — and an orphan row must not turn a legal delete into
//!    a refusal.
//! 2. **Visit order is preserved.** On the non-transactional path each
//!    `delete_rel` autocommits its own timestamp, so a different order produces
//!    a different commit log byte-for-byte.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Dir, Graph, GraphError, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn count(t: &engram_observe::Trace, k: &str) -> u64 {
    t.counters().get(k).copied().unwrap_or(0)
}

/// A hub with mixed in/out edges, parallel edges and a self-loop — every shape
/// the enumeration has to order identically.
fn wired(g: &Graph) -> u64 {
    run(g, "CREATE (:Hub {h: 1})");
    for i in 0..12i64 {
        run(g, &format!("CREATE (:Leaf {{l: {i}}})"));
    }
    // Out edges, in edges, a parallel pair, and a self-loop.
    run(
        g,
        "MATCH (h:Hub {h: 1}), (l:Leaf) WHERE l.l < 6 CREATE (h)-[:OUT]->(l)",
    );
    run(
        g,
        "MATCH (h:Hub {h: 1}), (l:Leaf) WHERE l.l >= 6 CREATE (l)-[:IN]->(h)",
    );
    run(
        g,
        "MATCH (h:Hub {h: 1}), (l:Leaf {l: 0}) CREATE (h)-[:OUT]->(l)",
    );
    run(g, "MATCH (h:Hub {h: 1}) CREATE (h)-[:LOOP]->(h)");
    let r = rows(g, "MATCH (h:Hub {h: 1}) RETURN id(h)");
    match r.first().and_then(|row| row.first()) {
        Some(Value::Int(i)) => *i as u64,
        other => panic!("expected the hub id, got {other:?}"),
    }
}

/// The enumeration must yield EXACTLY what `rels_of` yields, in order.
#[test]
fn id_enumeration_matches_rels_of_exactly_and_in_order() {
    let g = graph();
    let hub = wired(&g);
    for dir in [Dir::Out, Dir::In, Dir::Both] {
        let decoded: Vec<u64> = g
            .rels_of(hub, dir, None)
            .expect("rels_of")
            .into_iter()
            .map(|r| r.id)
            .collect();
        let ids = g.incident_rel_ids(hub, dir, None).expect("ids");
        assert_eq!(
            ids, decoded,
            "{dir:?}: the id enumeration must match rels_of row for row, in \
             order — each delete_rel autocommits its own ts, so a different \
             order is a different commit log"
        );
    }
    // And with a type filter, which takes the same rejection branch.
    let decoded: Vec<u64> = g
        .rels_of(hub, Dir::Both, Some(&["OUT".to_string()]))
        .expect("rels_of")
        .into_iter()
        .map(|r| r.id)
        .collect();
    let ids = g
        .incident_rel_ids(hub, Dir::Both, Some(&["OUT".to_string()]))
        .expect("ids");
    assert_eq!(ids, decoded, "type filtering must agree too");
}

/// The whole delete behaves identically on both arms, over every shape.
#[test]
fn detach_delete_is_identical_on_both_arms() {
    let mut finals = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_detach_via_rel_ids(on);
        let _hub = wired(&g);
        run(&g, "MATCH (h:Hub {h: 1}) DETACH DELETE h");
        assert!(
            g.verify_rel_endpoints().expect("fsck").is_empty(),
            "arm on={on}: FSCK found a dangling edge after DETACH DELETE"
        );
        let leaves = rows(&g, "MATCH (n:Leaf) RETURN n.l").len();
        let hubs = rows(&g, "MATCH (n:Hub) RETURN n.h").len();
        let edges = rows(&g, "MATCH ()-[r]->() RETURN r").len();
        finals.push((leaves, hubs, edges));
    }
    assert_eq!(
        finals[0], finals[1],
        "the enumeration may change what the delete PAYS, never what it LEAVES"
    );
    assert_eq!(finals[0], (12, 0, 0), "and the hub and all its edges are gone");
}

/// A plain DELETE of a connected node must still be refused — on both arms.
#[test]
fn a_connected_node_is_still_refused_on_both_arms() {
    for on in [false, true] {
        let g = graph();
        g.set_detach_via_rel_ids(on);
        let hub = wired(&g);
        let err = g.delete_node(hub, false).expect_err("must refuse");
        assert!(
            matches!(err, GraphError::StillConnected(n) if n == hub),
            "arm on={on}: expected StillConnected, got {err:?}"
        );
        // And nothing was deleted on the way to refusing.
        assert_eq!(
            rows(&g, "MATCH (n:Hub) RETURN n.h").len(),
            1,
            "arm on={on}: a refused delete must leave the node"
        );
    }
}

/// An UNCONNECTED node deletes cleanly on both arms — the other side of the
/// `StillConnected` branch.
#[test]
fn an_unconnected_node_deletes_on_both_arms() {
    for on in [false, true] {
        let g = graph();
        g.set_detach_via_rel_ids(on);
        run(&g, "CREATE (:Lonely {x: 1})");
        let r = rows(&g, "MATCH (n:Lonely) RETURN id(n)");
        let id = match r.first().and_then(|row| row.first()) {
            Some(Value::Int(i)) => *i as u64,
            other => panic!("expected an id, got {other:?}"),
        };
        g.delete_node(id, false).expect("an unconnected node deletes");
        assert!(rows(&g, "MATCH (n:Lonely) RETURN n.x").is_empty());
    }
}

/// Non-vacuity: the ON arm really enumerates ids, and it really stops decoding.
#[test]
fn the_lever_actually_switches_the_enumeration() {
    let g = graph();
    g.set_detach_via_rel_ids(true);
    let hub = wired(&g);
    let (_, on) = engram_observe::with_trace(|| {
        let _ = g.delete_node(hub, true);
    });
    assert!(
        count(&on, "graph.incident rel ids enumerated") >= 1,
        "ON arm must enumerate ids, counters: {:?}",
        on.counters()
    );

    let g2 = graph();
    g2.set_detach_via_rel_ids(false);
    let hub2 = wired(&g2);
    let (_, off) = engram_observe::with_trace(|| {
        let _ = g2.delete_node(hub2, true);
    });
    assert_eq!(
        count(&off, "graph.incident rel ids enumerated"),
        0,
        "OFF arm must take rels_of — it is the differential's control"
    );

    // THE POINT: the ON arm fetches each incident relationship ONCE (inside
    // delete_rel), where the OFF arm fetched it twice.
    let on_rels = count(&on, "graph.rels materialised in full");
    let off_rels = count(&off, "graph.rels materialised in full");
    eprintln!("[detach] rels materialised: ids {on_rels}, rels_of {off_rels}");
    assert!(
        on_rels < off_rels,
        "the id path must decode strictly fewer relationship records: \
         {on_rels} vs {off_rels}"
    );
}
