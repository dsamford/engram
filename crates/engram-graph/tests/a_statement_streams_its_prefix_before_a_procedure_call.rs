//! A read-only statement the streaming pipeline refuses AS A WHOLE streams its
//! prefix, up to the last `WITH` before the clause it cannot run.
//!
//! `streamable` answers for the whole statement, and a procedure `CALL` in the
//! middle answered no for all of it — so SNB BI bi15, whose weighting join
//! feeds `engram.algo.project`, ran the join on the materialising loop: at
//! SF10, load 1.0 and 80 GB inside a minute, where the same join streamed on
//! its own splits across the executor.
//!
//! THE LOAD-BEARING TEST IS DIFFERENTIAL: every statement answers exactly as it
//! does with the cut turned off (`set_prefix_streaming(false)`), columns and
//! rows, including the zero-row case where only the schema carries the answer.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, ScopedExec, run_query, run_stmt};
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

fn run(g: &Graph, q: &str) -> (QueryResult, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (r, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"))
    });
    (r, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const CUT: &str = "interp.statement streamed its prefix before a clause the pipeline cannot run";
const CONTINUATION: &str = "interp.stage drove its continuation in parallel";

/// 100 people who each KNOW the next three; 10 messages each, every other one
/// a reply to a message by the next person along. One row per edge in every
/// setup statement.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 99) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 99) AS i UNWIND range(1, 3) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 100}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS m MATCH (p:Person {id: m % 100}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m})",
    );
    ddl(
        &g,
        "UNWIND range(0, 999) AS i WITH i WHERE i % 2 = 0 \
         MATCH (a:Message {id: i}), (b:Message {id: (i + 101) % 1000}) \
         CREATE (a)-[:REPLY_OF]->(b)",
    );
    let _ = g.warm();
    g
}

/// bi15's shape: the weighting join, its projection, a k=1 path between two
/// people. The join is the prefix; the procedure calls are what refuse the
/// whole statement.
const BI15: &str = "MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
     OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
     WITH pA, pB, count(m1) AS w \
     WITH collect({source: id(pA), target: id(pB), weight: 1.0 / (w + 1.0)}) AS edges \
     CALL engram.algo.project({name: 'q', nodeLabels: ['Person'], edges: edges, orientation: 'UNDIRECTED'}) \
     YIELD projection \
     MATCH (a:Person {id: 0}), (b:Person {id: 50}) \
     CALL engram.algo.kshortestpaths.stream({projection: projection, sourceNode: id(a), targetNode: id(b), k: 1}) \
     YIELD totalCost \
     RETURN totalCost";

fn both_ways(g: &Graph, q: &str) -> (QueryResult, BTreeMap<String, u64>) {
    g.set_prefix_streaming(false);
    let (off, c_off) = run(g, q);
    g.set_prefix_streaming(true);
    assert_eq!(get(&c_off, CUT), 0, "`{q}` was cut with the cut turned off");
    let (on, c_on) = run(g, q);
    assert_eq!(on, off, "cutting `{q}` changed its answer");
    (on, c_on)
}

#[test]
fn bi15s_join_streams_and_answers_as_the_materialising_loop() {
    let g = social();
    let (r, c) = both_ways(&g, BI15);
    assert_eq!(r.rows.len(), 1, "one route, one row: {r:?}");
    assert!(
        matches!(r.rows[0].first(), Some(Value::Float(f)) if *f > 0.0),
        "a route with a positive cost: {r:?}"
    );
    assert!(get(&c, CUT) > 0, "the prefix was never cut off: {c:?}");

    // and through the executor, the prefix's join splits its continuation
    let wide = social();
    wide.set_exec(Some(Arc::new(TestExec(4))));
    let (w, c) = run(&wide, BI15);
    assert_eq!(w, r, "the split prefix changed the answer");
    assert!(get(&c, CONTINUATION) > 0, "the streamed prefix never split its join: {c:?}");
}

/// The cut WITH's own WHERE, an ORDER BY / LIMIT on it, a later clause that
/// reads a projected node's property, and a prefix that yields nothing — where
/// only the column list carries the answer.
#[test]
fn other_shapes_around_a_procedure_answer_as_the_loop_does() {
    let g = social();
    for q in [
        "MATCH (p:Person) WITH p.id AS i WHERE i % 7 = 0 \
         CALL db.labels() YIELD label RETURN i, label ORDER BY i, label",
        "MATCH (p:Person)-[:KNOWS]->(f:Person) WITH p, count(f) AS n ORDER BY p.id DESC LIMIT 3 \
         CALL db.labels() YIELD label RETURN p.id AS id, n, label ORDER BY id, label",
        "MATCH (p:Person) WHERE p.id < 0 WITH p.id AS i \
         CALL db.labels() YIELD label RETURN *",
        "MATCH (p:Person) WHERE p.id < 0 WITH p.id AS i \
         CALL db.labels() YIELD label RETURN count(*) AS n",
    ] {
        let (_, c) = both_ways(&g, q);
        assert!(get(&c, CUT) > 0, "`{q}` was not cut: {c:?}");
    }
}

/// Not cut: a statement that may write (its reads build a read-set), and a
/// `WITH *` (its columns come from the loop's schema) — both still answer.
#[test]
fn a_writer_and_a_star_are_not_cut() {
    for q in [
        "MATCH (p:Person) WITH p.id AS i WHERE i < 3 \
         CALL db.labels() YIELD label CREATE (:Seen {i: i}) RETURN count(*) AS n",
        "MATCH (p:Person) WITH * CALL db.labels() YIELD label RETURN count(*) AS n",
    ] {
        let g = social();
        let (_, c) = run(&g, q);
        assert_eq!(get(&c, CUT), 0, "`{q}` was cut");
    }
}
