//! A plain projection's HEAVY items — a pattern comprehension, a `COUNT {}`,
//! a walk per row — are evaluated by the workers that produce the stage's
//! rows, not by the one thread that drains them into the projector.
//!
//! SNB Interactive IC14 weighs each of its 42 shortest paths at SF3 with four
//! comprehensions per relationship, each a pinned walk of ~20 ms: ~1.8 s in
//! the draining thread of a 40-core server, against Neo4j's 1.3. And 42 rows
//! sat under the floor that keeps cheap row sets serial.
//!
//! Every answer is the serial run's, row for row: the workers' rows reach the
//! projector in morsel order, which is input order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The test-lane threaded executor — the server's shape.
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

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// The streaming stage's lever: the breaker's heavy items evaluated by the
/// workers that produce its rows.
const PRE: &str = "interp.breaker's heavy items evaluated where its rows are produced";
const SPLIT: &str = "interp.stage split a heavy projection's rows below the floor";
/// The materialising interpreter's: a statement that does not stream.
const MATERIALISED: &str = "interp.projection evaluated its heavy items on the morsel executor";

/// The serial answer, then the answer on four workers; returns the parallel
/// run's counters.
fn agrees(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    g.set_exec(None);
    let (want, serial) = run(g, q);
    assert_eq!(counter(&serial, PRE), 0, "a serial run pre-evaluated: {serial:?}");
    assert_eq!(counter(&serial, MATERIALISED), 0, "a serial run split: {serial:?}");
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (got, c) = run(g, q);
    g.set_exec(None);
    assert_eq!(got, want, "the workers' answer differs from the serial one for `{q}`");
    assert!(!want.is_empty(), "vacuous: `{q}` answered nothing");
    c
}

/// Thirty people; 0 and 1 are joined through six friends (2-7), so six
/// two-hop shortest paths run between them. Every person posts five times;
/// people comment on each other's posts and on each other's comments.
fn forum() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 29) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "MATCH (a:Person), (b:Person) WHERE a.id IN [0, 1] AND b.id >= 2 AND b.id <= 7 \
         CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "MATCH (a:Person), (b:Person) WHERE a.id >= 2 AND b.id = a.id + 10 CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 29) AS i UNWIND range(0, 4) AS k MATCH (p:Person {id: i}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Post:Message {id: i * 100 + k})",
    );
    ddl(
        &g,
        "MATCH (c:Person), (post:Post)-[:HAS_CREATOR]->(p:Person) \
         WHERE c.id <> p.id AND (c.id + p.id + post.id) % 4 = 0 \
         CREATE (c)<-[:HAS_CREATOR]-(:Comment:Message {id: 10000 + c.id * 1000 + post.id})-[:REPLY_OF]->(post)",
    );
    ddl(
        &g,
        "MATCH (c:Person), (cm:Comment)-[:HAS_CREATOR]->(p:Person) \
         WHERE c.id <> p.id AND (c.id * 7 + cm.id) % 11 = 0 \
         CREATE (c)<-[:HAS_CREATOR]-(:Comment:Message {id: 900000 + c.id * 100000 + cm.id})-[:REPLY_OF]->(cm)",
    );
    let _ = g.warm();
    g
}

#[test]
fn ic14s_weights_are_evaluated_by_the_workers() {
    let g = forum();
    let w = |shape: &str, weight: &str| {
        format!(
            "[r in rels_in_path | reduce(w=0.0, v in [{shape} \
             WHERE (a.id = startNode(r).id and b.id=endNode(r).id) \
                OR (a.id=endNode(r).id and b.id=startNode(r).id) | {weight}] | w+v)]"
        )
    };
    let q = format!(
        "MATCH path = allShortestPaths((person1:Person {{id: 0}})-[:KNOWS*0..]-(person2:Person {{id: 1}})) \
         WITH collect(path) as paths UNWIND paths as path \
         WITH path, relationships(path) as rels_in_path \
         WITH [n in nodes(path) | n.id] as personIdsInPath, {} as weight1, {} as weight2 \
         WITH personIdsInPath, reduce(w=0.0,v in weight1| w+v) as w1, reduce(w=0.0,v in weight2| w+v) as w2 \
         RETURN personIdsInPath, (w1+w2) as pathWeight ORDER BY pathWeight desc, personIdsInPath",
        w(
            "(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)",
            "1.0"
        ),
        w(
            "(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Comment)-[:HAS_CREATOR]->(b:Person)",
            "0.5"
        ),
    );
    // `allShortestPaths` does not stream: the materialising interpreter's
    // projection takes the weights to the workers
    let c = agrees(&g, &q);
    assert!(counter(&c, MATERIALISED) > 0, "IC14's weights stayed on one core: {c:?}");
}

#[test]
fn a_row_order_the_projection_does_not_sort_is_kept() {
    // The columnar paths off, so the statements stream through the stage
    // driver rather than a column walk's continuation.
    let g = forum();
    g.set_columnar_scans(false);
    // A PLAIN heavy WITH is no breaker: it runs inside the RETURN's stage,
    // per row, in whatever worker drives the row. No ORDER BY after it: the
    // answer's order IS the input's.
    let q = "MATCH (p:Person) WITH p ORDER BY p.id DESC \
             WITH p.id AS id, size([(p)<-[:HAS_CREATOR]-(m:Message) | m]) AS n, \
                  COUNT { (p)-[:KNOWS]-() } AS k \
             RETURN id, n, k";
    let c = agrees(&g, q);
    assert!(counter(&c, SPLIT) > 0, "thirty rows stayed on one core: {c:?}");
    // A heavy WITH that orders IS a breaker: its projector takes the items
    // the workers evaluated — beside its own WHERE, which reads an alias.
    let q = "MATCH (p:Person) WITH p ORDER BY p.id \
             WITH p.id AS id, size([(p)<-[:HAS_CREATOR]-(m:Comment) | m]) AS n \
             ORDER BY n DESC, id WHERE n > 2 \
             RETURN id, n";
    let c = agrees(&g, q);
    assert!(counter(&c, PRE) > 0, "{c:?}");
    assert!(counter(&c, SPLIT) > 0, "{c:?}");
}

#[test]
fn a_projection_that_pages_or_aggregates_keeps_its_items() {
    let g = forum();
    g.set_columnar_scans(false);
    for q in [
        // a LIMIT projects its survivors alone
        "MATCH (p:Person) WITH p ORDER BY p.id \
         WITH p.id AS id, size([(p)<-[:HAS_CREATOR]-(m:Message) | m]) AS n LIMIT 5 RETURN id, n",
        // an aggregating projection
        "MATCH (p:Person) WITH p ORDER BY p.id \
         WITH p.id % 3 AS g, sum(size([(p)<-[:HAS_CREATOR]-(m:Message) | m])) AS n \
         RETURN g, n ORDER BY g",
        // nothing heavy
        "MATCH (p:Person) WITH p ORDER BY p.id WITH p.id AS id, p.id * 2 AS d RETURN id, d",
    ] {
        let c = agrees(&g, q);
        assert_eq!(counter(&c, PRE), 0, "`{q}` pre-evaluated: {c:?}");
    }
}
