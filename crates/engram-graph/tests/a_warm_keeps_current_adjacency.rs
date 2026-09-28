//! Boot warm-up KEEPS an adjacency direction that is already current.
//!
//! `Graph::warm` used to rebuild both directions' tables with a full span walk
//! whatever was already published — including tables the server had just
//! adopted from the derived sidecar — and then lose every publish to them
//! (a slot publishes only forward), so the walk was thrown away. On
//! 2026-09-27 that was most of a 64-127 s SF3 warm after 17 structures had
//! been adopted in 1.3 s.
//!
//! Pinned by the thread-local trace, not the process-wide counter, so tests
//! warming other graphs in parallel cannot move the number.

use std::collections::BTreeMap;

use engram_cypher::parse_any;
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const KEPT: &str = "graph.warm kept an adopted adjacency direction";

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn kept_by(g: &Graph) -> (u64, engram_graph::WarmReport) {
    let (report, trace) = engram_observe::with_trace(|| g.warm());
    (trace.counters().get(KEPT).copied().unwrap_or(0), report)
}

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 499) AS i CREATE (:P {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 498) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) CREATE (a)-[:T]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 249) AS i MATCH (a:P {id: i}), (b:P {id: i + 2}) CREATE (a)-[:U]->(b)",
    );
    g
}

#[test]
fn a_second_warm_keeps_both_current_directions() {
    let g = fixture();
    let (kept, first) = kept_by(&g);
    assert_eq!(kept, 0, "nothing was published yet: the first warm builds");
    assert!(first.tables > 0, "the first warm built tables: {first:?}");

    let (kept, second) = kept_by(&g);
    assert_eq!(
        kept, 2,
        "no relationship changed since the first warm: both directions are \
         current and must be kept, not walked again"
    );
    assert_eq!(second.tables, 0, "nothing rebuilt: {second:?}");
    assert_eq!(
        (second.out_edges, second.in_edges),
        (first.out_edges, first.in_edges),
        "a kept direction still reports its edge count"
    );
}

#[test]
fn a_relationship_write_makes_the_next_warm_rebuild() {
    let g = fixture();
    let _ = kept_by(&g);
    ddl(
        &g,
        "MATCH (a:P {id: 0}), (b:P {id: 7}) CREATE (a)-[:T]->(b)",
    );
    let (kept, report) = kept_by(&g);
    assert_eq!(
        kept, 0,
        "a relationship written since the last warm makes both directions stale"
    );
    assert!(report.tables > 0, "and they are rebuilt: {report:?}");
    // The rebuilt tables carry the new edge.
    assert_eq!(report.out_edges, 499 + 250 + 1);
}
