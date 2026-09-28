//! A variable-length hop in the MIDDLE of a path — `*0..` or `*1..`, bounded
//! or not — runs on the columnar pipeline as a frontier walk when the path's
//! hops share no relationship type and the breaker depends only on the SET of
//! rows (`count(DISTINCT …)`, `min`, `max`, a DISTINCT projection); anything
//! that counts rows declines to the enumerating path. Answers are compared
//! with `set_columnar_scans(false)`.
//!
//! SNB BI bi3 (`… (forum)-[:CONTAINER_OF]->(post)<-[:REPLY_OF*0..]-(message)
//! -[:HAS_TAG]->(:Tag)-[:HAS_TYPE]->(:TagClass {name: $c}) … count(DISTINCT
//! message)`) and bi9 (`(person)<-[:HAS_CREATOR]-(post)<-[:REPLY_OF*0..]-
//! (reply) … count(DISTINCT post), count(DISTINCT reply)`) ran on the general
//! matcher: 4.2M adjacency visits, a frame and a row clone each, 11.4 s serial
//! for bi3 at SF3.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The test-lane threaded executor — the server's shape (tests may spawn; the
/// engine may not).
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

fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let (rows, _) = run(g, q);
    g.set_columnar_scans(true);
    rows
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const WALKED: &str = "interp.pipeline var-length BFS ran";

/// Two countries of three cities; 24 people, each moderating one forum; each
/// forum holds four posts; every post roots a reply tree three levels deep,
/// two replies per message. Under each forum's first post one depth-2 comment
/// ALSO replies to its parent's sibling, so two walks from that post reach it
/// and everything under it. Messages carry one or two tags; tags belong to
/// one of three tag classes.
fn threads() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 1) AS c CREATE (:Country {name: 'C' + toString(c)})");
    ddl(
        &g,
        "MATCH (co:Country) UNWIND range(0, 2) AS k \
         CREATE (:City {id: k, country: co.name})-[:IS_PART_OF]->(co)",
    );
    ddl(
        &g,
        "UNWIND range(0, 23) AS i MATCH (ci:City {id: i % 3, country: 'C' + toString(i % 2)}) \
         CREATE (:Person {id: i})-[:IS_LOCATED_IN]->(ci)",
    );
    ddl(
        &g,
        "MATCH (p:Person) CREATE (:Forum {id: p.id, title: 'f' + toString(p.id)})-[:HAS_MODERATOR]->(p)",
    );
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE p.id = (f.id + 5) % 24 UNWIND range(0, 3) AS k \
         CREATE (f)-[:CONTAINER_OF]->\
         (:Post:Message {id: 1000000 + f.id * 1000 + k, day: k * 3, depth: 0})-[:HAS_CREATOR]->(p)",
    );
    for depth in 1..=3 {
        ddl(
            &g,
            &format!(
                "MATCH (m:Message {{depth: {parent}}}) UNWIND range(1, 2) AS k \
                 CREATE (:Comment:Message {{id: m.id * 10 + k, day: {depth} * 2 + k - 2, depth: {depth}}})\
                 -[:REPLY_OF]->(m)",
                parent = depth - 1,
            ),
        );
    }
    ddl(
        &g,
        "MATCH (c:Comment), (p:Person) WHERE p.id = c.id % 24 CREATE (c)-[:HAS_CREATOR]->(p)",
    );
    // Two walks from one post: d (under c1) also replies to c2, c1's sibling.
    ddl(
        &g,
        "MATCH (d:Comment)-[:REPLY_OF]->(c1:Comment)-[:REPLY_OF]->(p:Post)<-[:REPLY_OF]-(c2:Comment) \
         WHERE p.id % 1000 = 0 AND c1.id % 10 = 1 AND c2.id % 10 = 2 AND d.id % 10 = 1 \
         CREATE (d)-[:REPLY_OF]->(c2)",
    );
    ddl(&g, "UNWIND range(0, 2) AS k CREATE (:TagClass {name: 'K' + toString(k)})");
    ddl(
        &g,
        "MATCH (k:TagClass) UNWIND range(0, 3) AS t \
         CREATE (:Tag {name: k.name + 't' + toString(t)})-[:HAS_TYPE]->(k)",
    );
    ddl(
        &g,
        "MATCH (m:Message), (t:Tag) WHERE t.name = 'K' + toString(m.id % 3) + 't' + toString(m.id % 4) \
         CREATE (m)-[:HAS_TAG]->(t)",
    );
    ddl(
        &g,
        "MATCH (m:Message), (t:Tag) WHERE m.id % 5 = 0 AND t.name = 'K1t' + toString(m.id % 2) \
         CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    g
}

fn bi3(country: &str, class: &str) -> String {
    format!(
        "MATCH (:Country {{name: '{country}'}})<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(person:Person)\
         <-[:HAS_MODERATOR]-(forum:Forum)-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*0..]-(message:Message)\
         -[:HAS_TAG]->(:Tag)-[:HAS_TYPE]->(:TagClass {{name: '{class}'}}) \
         RETURN forum.id, forum.title, person.id, count(DISTINCT message) AS messageCount \
         ORDER BY messageCount DESC, forum.id ASC LIMIT 20"
    )
}

const BI9: &str = "MATCH (person:Person)<-[:HAS_CREATOR]-(post:Post)<-[:REPLY_OF*0..]-(reply:Message) \
    WHERE post.day >= 3 AND reply.day <= 7 \
    RETURN person.id, count(DISTINCT post) AS threadCount, count(DISTINCT reply) AS messageCount \
    ORDER BY messageCount DESC, person.id ASC LIMIT 100";

#[test]
fn bi3_and_bi9_walk_their_reply_trees_on_the_pipeline() {
    let g = threads();
    for q in [
        bi3("C0", "K0"),
        bi3("C1", "K1"),
        bi3("C0", "K2"),
        BI9.to_string(),
    ] {
        let want = control(&g, &q);
        let (got, c) = run(&g, &q);
        assert!(!want.is_empty(), "vacuous: {q}");
        assert_eq!(got, want, "\n  {q}");
        assert!(counter(&c, WALKED) > 0, "the walk was not the pipeline's: {q}\n{c:?}");
    }
}

#[test]
fn a_breaker_that_counts_rows_keeps_the_enumeration() {
    let g = threads();
    for (q, walks) in [
        // count(*) counts WALKS: the DAG comment is reached twice
        (
            "MATCH (forum:Forum)-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*0..]-(message:Message) \
             RETURN forum.id AS f, count(*) AS walks ORDER BY f",
            false,
        ),
        // a sum over the rows
        (
            "MATCH (forum:Forum)-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*0..]-(message:Message) \
             RETURN forum.id AS f, sum(message.day) AS s ORDER BY f",
            false,
        ),
        // collect(DISTINCT): the set is the walk's, the list order the rows'
        (
            "MATCH (forum:Forum {id: 3})-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*0..]-(message:Message) \
             RETURN forum.id AS f, collect(DISTINCT message.id) AS ms",
            false,
        ),
        // min and max are the set's
        (
            "MATCH (forum:Forum)-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*1..]-(message:Message) \
             RETURN forum.id AS f, min(message.day) AS lo, max(message.id) AS hi, \
                    count(DISTINCT message) AS n ORDER BY f",
            true,
        ),
        // a bounded walk from zero, mid-path
        (
            "MATCH (forum:Forum)-[:CONTAINER_OF]->(post:Post)<-[:REPLY_OF*0..2]-(message:Message)-[:HAS_TAG]->(t:Tag) \
             RETURN t.name AS tag, count(DISTINCT message) AS n ORDER BY tag",
            true,
        ),
    ] {
        let want = control(&g, q);
        let (got, c) = run(&g, q);
        assert!(!want.is_empty(), "vacuous: {q}");
        assert_eq!(got, want, "\n  {q}");
        assert_eq!(
            counter(&c, WALKED) > 0,
            walks,
            "the pipeline's walk {} here: {q}\n{c:?}",
            if walks { "should run" } else { "must not run" }
        );
    }
}

/// The walk's driving rows split across the executor (bi9 walks from ~1M posts
/// at SF3, and serially it lost to the general path); the partials concatenate
/// in order, so the parallel walk answers byte-for-byte as the serial one.
#[test]
fn a_walk_split_across_the_executor_answers_as_the_serial_walk() {
    let g = threads();
    for q in [bi3("C0", "K0"), bi3("C1", "K1"), BI9.to_string()] {
        let (serial, _) = run(&g, &q);
        g.set_exec(Some(Arc::new(TestExec(4))));
        g.set_parallel_expand(true);
        g.set_parallel_min_rows(2);
        let (parallel, c) = run(&g, &q);
        g.set_exec(None);
        assert!(!serial.is_empty(), "vacuous: {q}");
        assert_eq!(parallel, serial, "\n  {q}");
        assert!(
            counter(&c, "interp.pipeline var-length BFS parallel") > 0,
            "the walk did not split: {q}\n{c:?}"
        );
    }
}

#[test]
fn hops_sharing_a_type_keep_the_enumeration() {
    let g = threads();
    // REPLY_OF both fixed and variable in one path: isomorphism can refuse.
    let q = "MATCH (post:Post)<-[:REPLY_OF]-(c:Comment)<-[:REPLY_OF*0..]-(message:Message) \
             RETURN post.id AS p, count(DISTINCT message) AS n ORDER BY p";
    let want = control(&g, q);
    let (got, c) = run(&g, q);
    assert!(!want.is_empty(), "vacuous");
    assert_eq!(got, want);
    assert_eq!(counter(&c, WALKED), 0, "a shared type ran as a walk: {c:?}");
}
