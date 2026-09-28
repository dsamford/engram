//! A stage whose rows all come from one label scan, ending in an aggregating
//! breaker grouped by the seed node, runs on the workers: each drives the
//! stage over its share of the seed label into its own projector, and the
//! partials merge in share order. Every answer is the serial run's, row for
//! row — including the group order, and the folds that keep arrival order
//! (`collect`, a min or max tie, a buffered percentile).
//!
//! SNB BI bi4's prefix groups 4,982,242 memberships into 1,228,730 `(country,
//! forum)` groups at SF3: 3.1 s of walk on forty workers, then a 6 s drain on
//! the one thread that fed every row to the grouping.

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

const ON_WORKERS: &str = "interp.stage aggregated its seed shares on the workers";

/// The serial answer, then four workers'; returns the parallel run's counters.
fn agrees(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    g.set_exec(None);
    let (want, serial) = run(g, q);
    assert_eq!(counter(&serial, ON_WORKERS), 0, "{serial:?}");
    g.set_exec(Some(Arc::new(TestExec(4))));
    let (got, c) = run(g, q);
    g.set_exec(None);
    assert_eq!(got, want, "the workers' answer differs from the serial one for `{q}`");
    assert!(!want.is_empty(), "vacuous: `{q}` answered nothing");
    c
}

/// Twelve countries of three cities of five people; sixty forums, each with
/// members drawn across the countries, so a forum's members span several
/// countries and a country's rows arrive from many forums. The columnar paths
/// are OFF: the pipeline answers these statements whole, where bi4's (a CALL
/// after its prefix) streams through the stage driver this is about.
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
                    <-[:HAS_MEMBER]-(forum:Forum) WHERE forum.creationDate > 3";

#[test]
fn bi4s_prefix_groups_on_the_workers_in_the_serial_order() {
    let g = world();
    // no ORDER BY: the group order itself is the answer's order
    let c = agrees(
        &g,
        &format!(
            "{PATH} WITH country, forum, count(person) AS n \
             RETURN country.id AS c, forum.id AS f, n"
        ),
    );
    assert!(counter(&c, ON_WORKERS) > 0, "the stage stayed on one thread: {c:?}");
    // and ordered and paged as bi4 does
    let c = agrees(
        &g,
        &format!(
            "{PATH} WITH country, forum, count(person) AS numberOfMembers \
             ORDER BY numberOfMembers DESC, forum.id ASC, country.id \
             WITH DISTINCT forum AS topForum LIMIT 10 RETURN topForum.id AS f"
        ),
    );
    assert!(counter(&c, ON_WORKERS) > 0, "{c:?}");
}

#[test]
fn the_folds_that_keep_arrival_order_merge_as_the_serial_fold_made_them() {
    let g = world();
    for q in [
        // collect's order, and min/max ties between equal dates
        format!(
            "{PATH} WITH country, collect(person.id) AS ps, collect(forum.id) AS fs, \
                 min(forum.creationDate) AS lo, max(forum.creationDate) AS hi, count(*) AS rows \
             RETURN country.id AS c, ps, fs, lo, hi, rows"
        ),
        // a fold buffered to the end
        format!(
            "{PATH} WITH country, percentileDisc(forum.creationDate, 0.5) AS med, \
                 stdev(forum.creationDate) AS sd \
             RETURN country.id AS c, med, sd"
        ),
    ] {
        let c = agrees(&g, &q);
        assert!(counter(&c, ON_WORKERS) > 0, "`{q}` stayed on one thread: {c:?}");
    }
}

#[test]
fn a_fold_that_does_not_merge_exactly_keeps_the_serial_drive() {
    let g = world();
    for q in [
        // not grouped by the seed: a group spans shares
        format!("{PATH} WITH forum, count(person) AS n RETURN forum.id AS f, n"),
        // a sum
        format!("{PATH} WITH country, sum(forum.creationDate) AS s RETURN country.id AS c, s"),
    ] {
        let c = agrees(&g, &q);
        assert_eq!(counter(&c, ON_WORKERS), 0, "`{q}` went to the workers: {c:?}");
    }
}

#[test]
fn a_distinct_fold_goes_to_the_workers() {
    // DISTINCT seen sets merge by union, each partial numbering its NaNs in
    // a range of its own (a group keyed by the seed lies in one share anyway)
    let g = world();
    let c = agrees(
        &g,
        &format!(
            "{PATH} WITH country, count(DISTINCT forum) AS n, collect(DISTINCT forum.id) AS fs \
             RETURN country.id AS c, n, fs"
        ),
    );
    assert!(counter(&c, ON_WORKERS) > 0, "{c:?}");
}

#[test]
fn a_path_turned_round_to_its_indexed_end_never_goes_to_the_workers() {
    // SNB BI bi2's shape: the label scan the plan names is of `Tag`, but the
    // matcher drives from the one `TagClass` its index finds. Dispatched,
    // every worker drove the whole stage without its share, and the serial
    // drive then did it again: bi2 1.4 s -> 9 s.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX tagclass_name FOR (c:TagClass) ON (c.name)");
    ddl(&g, "UNWIND range(0, 4) AS k CREATE (:TagClass {name: 'tc' + toString(k)})");
    ddl(
        &g,
        "MATCH (c:TagClass) UNWIND range(0, 19) AS k \
         CREATE (:Tag {id: c.name + '-' + toString(k)})-[:HAS_TYPE]->(c)",
    );
    ddl(
        &g,
        "MATCH (t:Tag) UNWIND range(0, 2) AS k CREATE (:Message {id: t.id + '/' + toString(k)})-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    g.set_columnar_scans(false);
    let q = "MATCH (tag:Tag)-[:HAS_TYPE]->(:TagClass {name: 'tc2'}) \
             OPTIONAL MATCH (m:Message)-[:HAS_TAG]->(tag) \
             WITH tag, count(m) AS n RETURN tag.id AS t, n ORDER BY t";
    let c = agrees(&g, q);
    assert_eq!(counter(&c, ON_WORKERS), 0, "{c:?}");
    assert_eq!(
        counter(&c, "interp.parallel aggregation declined: a worker's scan was not its share"),
        0,
        "the stage was dispatched to workers that could not take their shares: {c:?}"
    );
}
