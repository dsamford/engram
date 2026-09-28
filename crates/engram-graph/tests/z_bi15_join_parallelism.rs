#![allow(non_snake_case)]
//! Why does bi15's weighting join run on ONE core at SF10?
//!
//! `MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB)` alone took
//! 123 s to count 1,938,516 pairs, and with the OPTIONAL interaction join the
//! load stayed at 1.00 on 40 cores. Which parallel-drive gate declines it?

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[test]
#[ignore = "diagnostic — run with --ignored --nocapture"]
fn which_gate_keeps_bi15s_join_serial() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 999) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 999) AS i UNWIND range(1, 6) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d * 7) % 1000}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 3999) AS m MATCH (p:Person {id: m % 1000}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m})",
    );
    // ONE ROW PER EDGE. The first version of this setup was
    // `MATCH (a:Message), (b:Message) WHERE … CREATE` — a 64M-row cartesian
    // join inside a writing statement — and on a shared workstation it took
    // 177 GB. Never a cartesian in a setup write; and this runs on the build
    // pod, whose cgroup bounds it, never on the workstation.
    ddl(
        &g,
        "UNWIND range(0, 3999) AS i WITH i WHERE i % 3 = 0 \
         MATCH (a:Message {id: i}), (b:Message {id: (i * 31 + 7) % 4000}) \
         CREATE (a)-[:REPLY_OF]->(b)",
    );
    let _ = g.warm();
    g.set_exec(Some(Arc::new(TestExec(8))));
    for (label, q) in [
        (
            "pairs alone",
            "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) RETURN count(*) AS n",
        ),
        (
            "pairs, directed",
            "MATCH (pA:Person)-[:KNOWS]->(pB:Person) RETURN count(*) AS n",
        ),
        (
            "pairs + interaction join",
            "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
             OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
             WITH pA, pB, count(m1) AS i RETURN count(*) AS n, sum(i) AS s",
        ),
    ] {
        let s = parse_statement(q).unwrap();
        let (_, t) = engram_observe::with_trace(|| {
            run_query(&g, &s, BTreeMap::new()).unwrap();
        });
        let mut c: Vec<(String, u64)> = t
            .counters()
            .iter()
            .filter(|(k, v)| {
                **v > 0
                    && (k.contains("drive")
                        || k.contains("parallel")
                        || k.contains("morsel")
                        || k.contains("seed")
                        || k.contains("split")
                        || k.contains("declin")
                        || k.contains("serial"))
            })
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        c.sort();
        println!("--- {label}");
        for (k, v) in c {
            println!("    {v:>8}  {k}");
        }
    }
}
