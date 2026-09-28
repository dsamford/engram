//! Two costs of a statement that reads many records one at a time.
//!
//! An UNLABELLED hop end read from its record counts the miss against the
//! labels the read found on it, as a labelled end counts its own: at the
//! batch floor the label's demanded columns are read whole and kept, and the
//! next statement binds its ends from them. SNB Interactive IS3's `(:Person
//! {id})-[r:KNOWS]-(friend)` read 1,193 friends' names by projected record
//! reads on every run — 12 ms of engine against Neo4j's 10 — because nothing
//! ever kept the Person columns an unlabelled end demands.
//!
//! A LARGE record-gather splits across the executor: one contiguous run of
//! the ids per worker, the runs' columns concatenated in run order. IC8's
//! pipeline gathered 3,110 comments' dates by point reads on one thread.

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

const WARMED: &str = "interp.matcher warmed a hop end label's columns after repeated misses";
const FROM_COLUMNS: &str = "interp.matcher bound an unlabelled hop end from a label's cached columns";
const SPLIT: &str = "graph.column record-gather split across the executor";

/// IS3's shape: one person, 100 friends through KNOWS both ways, the friend
/// unlabelled in the pattern.
const IS3: &str = "MATCH (n:Person {id: 0})-[r:KNOWS]-(friend) \
    RETURN friend.id AS personId, friend.firstName AS firstName, friend.lastName AS lastName, \
           r.creationDate AS friendshipCreationDate \
    ORDER BY friendshipCreationDate DESC, toInteger(personId) ASC";

#[test]
fn an_unlabelled_end_warms_its_labels_columns_after_repeated_misses() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE (:Person {id: 0, firstName: 'p', lastName: 'zero'})");
    ddl(
        &g,
        "MATCH (n:Person {id: 0}) UNWIND range(1, 100) AS i \
         CREATE (n)-[:KNOWS {creationDate: i % 7}]->(:Person {id: i, firstName: 'f' + toString(i), \
                                                           lastName: 'l' + toString(i % 9)})",
    );
    ddl(&g, "CREATE INDEX person_id FOR (p:Person) ON (p.id)");
    let _ = g.warm();
    // the record reads, every one of them a miss against Person
    let (first, c) = run(&g, IS3);
    assert_eq!(first.len(), 100, "{first:?}");
    assert_eq!(counter(&c, FROM_COLUMNS), 0, "{c:?}");
    assert!(counter(&c, WARMED) > 0, "no Person column was warmed: {c:?}");
    // the next statement binds its friends from the kept columns
    let (second, c) = run(&g, IS3);
    assert_eq!(second, first);
    assert!(counter(&c, FROM_COLUMNS) >= 100, "{c:?}");
}

#[test]
fn a_labelled_end_is_unchanged() {
    // the labelled end warmed its label before (fix 87); the unlabelled count
    // adds nothing to it
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE (:Person {id: 0, firstName: 'p', lastName: 'zero'})");
    ddl(
        &g,
        "MATCH (n:Person {id: 0}) UNWIND range(1, 100) AS i \
         CREATE (n)-[:KNOWS {creationDate: i % 7}]->(:Person {id: i, firstName: 'f' + toString(i), \
                                                           lastName: 'l' + toString(i % 9)})",
    );
    let q = IS3.replace("(friend)", "(friend:Person)");
    let (first, _) = run(&g, &q);
    let (second, _) = run(&g, &q);
    assert_eq!(first.len(), 100);
    assert_eq!(second, first);
}

/// 1,200 `:F` nodes, each followed in id order by ten `:X` fillers that also
/// carry `val`: the population's id span holds eleven times its members, past
/// the range scan's budget, so its `val` column is point-gathered — 1,200
/// ids, over the split floor. No column is cached: every read gathers.
fn sparse() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_prop_column_budget(0);
    ddl(
        &g,
        "UNWIND range(0, 1199) AS i \
         CREATE (:F {val: (i * 7919) % 1201}), (:X {val: i}), (:X {val: i}), (:X {val: i}), \
                (:X {val: i}), (:X {val: i}), (:X {val: i}), (:X {val: i}), (:X {val: i}), \
                (:X {val: i}), (:X {val: i})",
    );
    let _ = g.warm();
    g
}

#[test]
fn a_large_gather_splits_across_the_executor_and_answers_as_one_run() {
    let g = sparse();
    for q in [
        "MATCH (f:F) RETURN f.val AS v ORDER BY v",
        "MATCH (f:F) WHERE f.val % 3 = 1 RETURN count(f) AS n, sum(f.val) AS s",
    ] {
        g.set_exec(None);
        let (want, serial) = run(&g, q);
        assert!(counter(&serial, "graph.column point-gather") > 0, "`{q}` never gathered: {serial:?}");
        assert_eq!(counter(&serial, SPLIT), 0, "{serial:?}");
        g.set_exec(Some(Arc::new(TestExec(4))));
        let (got, c) = run(&g, q);
        g.set_exec(None);
        assert_eq!(got, want, "`{q}`");
        assert!(!want.is_empty());
        assert!(counter(&c, SPLIT) > 0, "`{q}` gathered on one thread: {c:?}");
    }
}
