//! A stage whose FIRST clause is a MATCH and whose work is a LATER clause
//! drives that continuation across the morsel executor.
//!
//! `drive_seeds` splits only the first clause's seed scan: each morsel's rows
//! were drained into the rest of the stage on the CALLING thread. SNB BI
//! bi15's weighting join — `MATCH (pA)-[:KNOWS]-(pB) … OPTIONAL MATCH (pA)<-…
//! -(m1)-[:REPLY_OF]-(m2)-…->(pB)` — reported its seed scan "driven in
//! parallel" and ran the OPTIONAL MATCH, all of its cost, on one core: 120 s
//! for 1/20 of SF3's pairs.
//!
//! THE LOAD-BEARING TEST IS DIFFERENTIAL, and on ORDER as well as rows: the
//! parallel continuation must produce exactly what the serial loop produces,
//! in the same order, so nothing downstream can tell.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

struct TestExec(usize);

impl ScopedExec for TestExec {
    fn width(&self) -> usize {
        self.0
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.0.min(n).max(1);
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(i);
                    }
                });
            }
        });
    }
}

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

const CONTINUATION: &str = "interp.stage drove its continuation in parallel";

/// 400 people who each KNOW the next four; 6 messages each, every third a
/// reply to a message two along. One row per edge in every setup statement.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 399) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 399) AS i UNWIND range(1, 4) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 400}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 2399) AS m MATCH (p:Person {id: m % 400}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m})",
    );
    ddl(
        &g,
        "UNWIND range(0, 2399) AS i WITH i WHERE i % 3 = 0 \
         MATCH (a:Message {id: i}), (b:Message {id: (i + 2) % 2400}) \
         CREATE (a)-[:REPLY_OF]->(b)",
    );
    let _ = g.warm();
    g
}

/// bi15's shape, and variants: the continuation must answer exactly as the
/// serial loop does — every row, in the same order.
#[test]
fn the_parallel_continuation_answers_exactly_as_the_serial_loop() {
    let serial = social();
    let parallel = social();
    parallel.set_exec(Some(Arc::new(TestExec(4))));
    for q in [
        // bi15's weighting join, per pair, in row order (no ORDER BY — the
        // ORDER itself must match)
        "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
         OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
         RETURN pA.id AS a, pB.id AS b, m1.id AS m",
        // aggregated, as bi15 does
        "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
         OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
         WITH pA, pB, count(m1) AS i RETURN count(*) AS pairs, sum(i) AS interactions",
        // a non-optional continuation, with a pipelined WITH and an UNWIND
        "MATCH (p:Person)-[:KNOWS]->(f:Person) WITH p, f \
         UNWIND [1, 2] AS k MATCH (f)<-[:HAS_CREATOR]-(m:Message) \
         RETURN p.id AS p, f.id AS f, k, m.id AS m",
    ] {
        let (s, _) = run(&serial, q);
        let (p, c) = run(&parallel, q);
        assert_eq!(s, p, "the parallel continuation changed the answer (or its order) of `{q}`");
        assert!(!s.is_empty(), "`{q}` answered nothing; this compares nothing");
        assert!(
            c.get(CONTINUATION).copied().unwrap_or(0) > 0,
            "`{q}` never drove its continuation in parallel; the comparison is vacuous"
        );
    }
}

/// A continuation that does not EXPAND is not worth a worker, and a stage that
/// writes is serial by construction.
#[test]
fn only_an_expanding_reading_continuation_is_split() {
    let g = social();
    g.set_exec(Some(Arc::new(TestExec(4))));
    for q in [
        // nothing after the first MATCH expands
        "MATCH (p:Person)-[:KNOWS]->(f:Person) WITH p, f RETURN count(*) AS n",
        // an OPTIONAL MATCH first: its null row belongs to the clause
        "OPTIONAL MATCH (p:Person)-[:KNOWS]->(f:Person) MATCH (f)<-[:HAS_CREATOR]-(m) RETURN count(*) AS n",
    ] {
        let (_, c) = run(&g, q);
        assert_eq!(c.get(CONTINUATION).copied().unwrap_or(0), 0, "`{q}` was split");
    }
}
