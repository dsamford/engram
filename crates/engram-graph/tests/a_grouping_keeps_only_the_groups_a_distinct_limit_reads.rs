//! A grouping breaker whose next clause is `WITH DISTINCT <column> LIMIT k`
//! keeps only the groups that clause reads: its rows in order until the k-th
//! distinct value. The rest were sorted, projected into rows, handed on and
//! freed unread — SNB BI bi4's prefix finished 1,228,730 `(country, forum)`
//! groups for a `WITH DISTINCT forum AS topForum LIMIT 100`: 3.7 s of sort and
//! rows, and 2.75 s of the next stage taking and freeing the rest.
//!
//! Every answer here is the one the full sort gives, which each test takes
//! from a CONTROL: the same statement with `WHERE topForum IS NOT NULL` on
//! the downstream clause — always true, never folded away — which the
//! selection declines.

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

const KEPT: &str = "interp.grouping kept only the groups a downstream DISTINCT LIMIT reads";

/// Twelve countries of three cities of five people; sixty forums, members
/// drawn across the countries so a forum's members span several countries —
/// every forum in several `(country, forum)` groups — and member counts tie
/// often. The columnar paths are off: the pipeline answers some of these
/// statements whole, where bi4's streams through the stage driver.
fn world() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 11) AS c CREATE (:Country {id: c})");
    ddl(
        &g,
        "MATCH (c:Country) UNWIND range(0, 2) AS k CREATE (:City {id: c.id * 10 + k})-[:IS_PART_OF]->(c)",
    );
    ddl(
        &g,
        "MATCH (ci:City) UNWIND range(0, 4) AS k \
         CREATE (:Person {id: ci.id * 10 + k})-[:IS_LOCATED_IN]->(ci)",
    );
    ddl(&g, "UNWIND range(0, 59) AS f CREATE (:Forum {id: f, creationDate: f % 10})");
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE (p.id * 7 + f.id * 13) % 17 < 3 CREATE (f)-[:HAS_MEMBER]->(p)",
    );
    let _ = g.warm();
    g.set_columnar_scans(false);
    g
}

const PATH: &str = "MATCH (country:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(person:Person)\
                    <-[:HAS_MEMBER]-(forum:Forum) WHERE forum.creationDate > 2";

/// The statement, and its control: the same with an always-true WHERE on the
/// downstream clause, which the selection declines.
fn both(breaker: &str, limit: &str) -> (String, String) {
    let q = format!(
        "{PATH} {breaker} WITH DISTINCT forum AS topForum LIMIT {limit} RETURN topForum.id AS f"
    );
    let control = format!(
        "{PATH} {breaker} WITH DISTINCT forum AS topForum LIMIT {limit} WHERE topForum IS NOT NULL \
         RETURN topForum.id AS f"
    );
    (q, control)
}

/// The statement answers as its control does; returns whether it selected.
fn agrees(g: &Graph, breaker: &str, limit: &str) -> bool {
    let (q, control) = both(breaker, limit);
    let (want, c) = run(g, &control);
    assert_eq!(counter(&c, KEPT), 0, "the control selected: {c:?}");
    let (got, c) = run(g, &q);
    assert_eq!(got, want, "`{q}` answered other than its control");
    counter(&c, KEPT) > 0
}

#[test]
fn bi4s_prefix_keeps_the_groups_its_top_forums_are_read_from() {
    let g = world();
    let breaker = "WITH country, forum, count(person) AS numberOfMembers \
                   ORDER BY numberOfMembers DESC, forum.id ASC, country.id";
    for limit in ["1", "5", "20"] {
        assert!(agrees(&g, breaker, limit), "LIMIT {limit} sorted every group");
    }
}

#[test]
fn ties_keep_the_first_seen_order_and_an_unordered_breaker_its_own() {
    let g = world();
    // many groups tie on the one key: the first-seen order decides
    assert!(agrees(
        &g,
        "WITH country, forum, count(person) AS n ORDER BY n DESC",
        "7"
    ));
    // no ORDER BY at all: the groups in first-seen order
    assert!(agrees(&g, "WITH country, forum, count(person) AS n", "7"));
}

#[test]
fn a_limit_past_the_distinct_values_keeps_every_group_and_limit_zero_none() {
    let g = world();
    let breaker = "WITH country, forum, count(person) AS n ORDER BY n DESC, forum.id";
    assert!(agrees(&g, breaker, "1000"));
    let (q, _) = both(breaker, "0");
    let (got, _) = run(&g, &q);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn a_key_no_copy_can_read_declines_and_answers_the_same() {
    let g = world();
    // an average finishes by dividing: it is not read without finishing
    assert!(!agrees(
        &g,
        "WITH country, forum, avg(person.id) AS a ORDER BY a DESC, forum.id, country.id",
        "5"
    ));
}

#[test]
fn on_the_workers_too() {
    // the prefix's grouping runs on the workers (rev44) and its partials
    // merge without an index (rev54); the selection answers as the control
    let g = world();
    g.set_exec(Some(Arc::new(TestExec(4))));
    g.set_parallel_min_rows(2);
    let breaker = "WITH country, forum, count(person) AS numberOfMembers \
                   ORDER BY numberOfMembers DESC, forum.id ASC, country.id";
    let (q, control) = both(breaker, "5");
    let (want, _) = run(&g, &control);
    let (got, c) = run(&g, &q);
    g.set_exec(None);
    assert_eq!(got, want);
    assert!(counter(&c, KEPT) > 0, "{c:?}");
    assert!(
        counter(&c, "interp.stage aggregated its seed shares on the workers") > 0,
        "the grouping stayed on one thread: {c:?}"
    );
}
