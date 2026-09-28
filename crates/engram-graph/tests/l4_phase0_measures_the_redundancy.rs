#![allow(non_snake_case)]
//! L4 PHASE 0, READ. The instrument built in `keep_prop_column` says whether
//! the redundancy L4 wanted to single-flight away actually happens. This runs
//! it under the concurrency that would create that redundancy and reports the
//! number, because Phase 1 (building the single-flight) must be decided by
//! Phase 0's reading and not by analogy to the adjacency arm.
//!
//! The counter is trace-scoped, so each thread reports its own and the test
//! sums them — a process-wide total would need an atomic the engine does not
//! carry for this counter.
//!
//! What a ZERO means, stated before the run so it cannot be reinterpreted
//! afterwards: concurrent readers of the same whole-label column are NOT
//! duplicating the build, so there is nothing for a single-flight to remove
//! and L4's Phase 1 is refuted on its OWN site's evidence rather than on the
//! adjacency precedent. What a LARGE number would mean: the duplication is
//! real, and Phase 1 becomes a live question to be settled by measuring the
//! mutex it would introduce against the work it would save.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const REDUNDANT: &str = "graph.property column rebuilt while a current one was already cached";
const KEPT: &str = "graph.property column kept";
const SERVED: &str = "graph.property column served";

const PERSONS: i64 = 2_000;
const THREADS: usize = 8;
const ROUNDS: usize = 20;

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("nickname".to_string(), Value::Str(format!("N{i}")));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    g
}

const LISTING: &str = "MATCH (p:Person) RETURN count(p.nickname) AS c";

fn count_of(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

#[test]
fn phase0_reports_whether_concurrent_readers_duplicate_the_column_build() {
    let g = corpus();
    // A write between rounds retires the column, so every round has a real
    // rebuild to race for. Without this the first round caches it and the
    // rest are served — no contention, and the instrument would read zero
    // for a reason that says nothing about L4.
    let mut totals = (0u64, 0u64, 0u64);
    for round in 0..ROUNDS {
        // `scope`, not `thread::spawn` — the latter is a disallowed method
        // here and the scoped form is what the other concurrent tests in this
        // directory use. It also lets the threads BORROW the graph, so the
        // `Arc` and the join bookkeeping both disappear.
        let round_totals = std::thread::scope(|s| {
            let hs: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        let (_, t) = engram_observe::with_trace(|| {
                            let q = parse_statement(LISTING).expect("parse");
                            run_query(&g, &q, BTreeMap::new()).expect("run")
                        });
                        let c = t.counters().clone();
                        (
                            count_of(&c, REDUNDANT),
                            count_of(&c, KEPT),
                            count_of(&c, SERVED),
                        )
                    })
                })
                .collect();
            hs.into_iter().fold((0u64, 0u64, 0u64), |a, h| {
                let (r, k, sv) = h.join().expect("thread");
                (a.0 + r, a.1 + k, a.2 + sv)
            })
        });
        totals.0 += round_totals.0;
        totals.1 += round_totals.1;
        totals.2 += round_totals.2;
        // Retire the column for the next round.
        let src = format!("CREATE (:Msg {{id: {}}})", 900_000 + round);
        run_stmt(&g, &parse_any(&src).expect("parse"), BTreeMap::new()).expect("write");
    }

    println!(
        "L4 PHASE 0 READING over {ROUNDS} rounds x {THREADS} threads: \
         redundant-rebuilds={} kept={} served={}",
        totals.0, totals.1, totals.2
    );

    // The instrument must have been EXERCISED — builds happened at all.
    assert!(
        totals.1 > 0,
        "no column was kept in the whole run, so the instrument never had a \
         chance to fire and this measures nothing: {totals:?}"
    );
}
