#![allow(non_snake_case)]
//! A projection is built ONCE per statement, however many times the statement
//! calls an algorithm over it.
//!
//! `algo_graph` is a pure function of committed state and was rebuilt on every
//! call. LDBC SNB BI **bi19** calls `engram.algo.kshortestpaths.stream` once
//! per (person, person) pair — 52 x 43 = 2,236 times at SF3 — over the SAME
//! 565,247-edge projection. Measured before the memo: 1 call 7 s, 5 calls
//! 29 s, 20 calls 105 s, with `algo.graph built` equal to the call count. That
//! is ~3.3 hours for bi19a, essentially all of it rebuilding one projection.
//!
//! After: 20 calls in 9 s, one build and nineteen reuses; bi19a and bi19b both
//! answer in ~37 s at SF3.
//!
//! The memo is keyed by the STATEMENT generation, not an epoch: a statement
//! sees one snapshot, so a projection built inside it cannot go stale inside
//! it. A statement that WRITES is excluded, because its own buffered writes
//! could change the projection under the next call — and that exclusion is
//! what the second test pins.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn counters(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (_, trace) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
    });
    trace
        .counters()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 39) AS i CREATE (:P {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 38) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) \
         CREATE (a)-[:W {w: 1.0}]->(b)",
    );
    g
}

const PROJ: &str = "nodeLabels: ['P'], relationshipTypes: ['W'], \
                    orientation: 'UNDIRECTED', relationshipWeightProperty: 'w'";

#[test]
fn many_algorithm_calls_in_ONE_statement_build_the_projection_once() {
    let g = graph();
    let q = format!(
        "MATCH (a:P) WHERE a.id < 8 MATCH (b:P) WHERE b.id > 30 \
         CALL engram.algo.kshortestpaths.stream({{{PROJ}, sourceNode: id(a), \
         targetNode: id(b), k: 1}}) YIELD totalCost RETURN count(*) AS n"
    );
    let c = counters(&g, &q);
    let built = c.get("algo.graph built").copied().unwrap_or(0);
    let reused = c
        .get("algo.graph reused within the statement")
        .copied()
        .unwrap_or(0);
    assert!(
        reused > 0,
        "8 x 9 = 72 calls over one projection must REUSE it; built={built} reused={reused}"
    );
    assert_eq!(
        built, 1,
        "the projection must be built exactly once per statement, not once per \
         call; built={built} reused={reused}"
    );
}

#[test]
fn a_transaction_WITH_BUFFERED_WRITES_does_not_reuse_a_projection() {
    // THE EXCLUSION, pinned. A transaction that has buffered writes could have
    // changed the projection, so the memo must decline — being slower there is
    // the correct trade.
    //
    // The precondition has to be built EXPLICITLY. An autocommit statement in
    // this harness is not wrapped in a transaction the way the server wraps
    // one, so `in_txn_with_writes()` is false here and a `MATCH … SET … CALL`
    // would reuse the projection locally while declining on a server. Opening
    // the transaction by hand makes the test mean the same thing in both.
    let g = graph();
    g.begin_txn().expect("begin");
    let w = parse_statement("MATCH (a:P) WHERE a.id < 3 SET a.touched = 1 RETURN count(*) AS n")
        .expect("parses");
    run_query(&g, &w, BTreeMap::new()).expect("the write runs");

    let q = format!(
        "MATCH (a:P) WHERE a.id < 4 MATCH (b:P) WHERE b.id > 36          CALL engram.algo.kshortestpaths.stream({{{PROJ}, sourceNode: id(a),          targetNode: id(b), k: 1}}) YIELD totalCost RETURN count(*) AS n"
    );
    let c = counters(&g, &q);
    g.commit_txn().expect("commit");
    assert_eq!(
        c.get("algo.graph reused within the statement")
            .copied()
            .unwrap_or(0),
        0,
        "a transaction with buffered writes must NOT reuse a projection"
    );
}
