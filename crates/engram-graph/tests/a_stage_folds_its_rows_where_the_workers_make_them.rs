//! A stage whose rows the workers make — its first clause's rows split for
//! the rest of it, or its input rows split — and that ends in an AGGREGATING
//! breaker folds those rows where they are made: each morsel into a partial
//! projector on its worker, the partials merged in morsel order. Every answer
//! is the serial fold's, row for row: the group order, the folds that keep
//! arrival order, and a DISTINCT over NaNs, which never equal each other.
//!
//! SNB BI bi6 makes 8,318,446 rows at SF3 from ~14k first-clause rows and
//! folded them into its `count(DISTINCT like)` groups one at a time, on one
//! thread: most of its 15.4 s against Neo4j's 14.9.

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

/// A width-4 executor that runs every morsel on the calling thread, in order:
/// each morsel finds its predecessor folded and CONTINUES its partial, so a
/// window folds into one partial. Or in REVERSE order: no predecessor has
/// folded when a morsel starts, so every morsel makes its own.
struct InOrderExec {
    reverse: bool,
}

impl ScopedExec for InOrderExec {
    fn width(&self) -> usize {
        4
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        if self.reverse {
            (0..n).rev().for_each(f);
        } else {
            (0..n).for_each(f);
        }
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

const FOLDED: &str = "interp.stage folded its rows where the workers made them";

/// The serial answer, then four workers'; returns the four workers' counters.
fn agrees(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    g.set_exec(None);
    let (want, serial) = run(g, q);
    assert_eq!(counter(&serial, FOLDED), 0, "{serial:?}");
    g.set_exec(Some(Arc::new(TestExec(4))));
    g.set_parallel_min_rows(2);
    let (got, c) = run(g, q);
    g.set_exec(None);
    // compared as rendered: `Value`'s equality is the float's, and a NaN
    // never equals itself — an answer holding one would never equal anything
    assert_eq!(
        format!("{got:?}"),
        format!("{want:?}"),
        "the workers' answer differs from the serial one for `{q}`"
    );
    assert!(!want.is_empty(), "vacuous: `{q}` answered nothing");
    c
}

fn folds(g: &Graph, q: &str) {
    let c = agrees(g, q);
    assert!(counter(&c, FOLDED) > 0, "`{q}` was folded on one thread: {c:?}");
}

/// Two tags over 600 messages by 20 people, and likes by a stride: each
/// like carries a weight, each message a float that is NaN for every
/// thirteenth. 300 first-clause rows for a tag — more than one window of
/// four workers — and many morsels in each. The columnar paths are off:
/// the pipeline answers some of these shapes whole, where bi6's streams
/// through the stage driver this is about.
fn world() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND ['t0', 't1'] AS n CREATE (:Tag {name: n})");
    ddl(&g, "UNWIND range(0, 19) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 599) AS j \
         MATCH (p:Person {id: j % 20}), (t:Tag {name: 't' + toString(j % 2)}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: j, \
             f: CASE WHEN j % 13 = 0 THEN 0.0 / 0.0 ELSE toFloat(j % 5) END})-[:HAS_TAG]->(t)",
    );
    ddl(
        &g,
        "MATCH (p:Person), (m:Message) WHERE (p.id * 7 + m.id * 3) % 17 < 2 \
         CREATE (p)-[:LIKES {w: (p.id + m.id) % 5}]->(m)",
    );
    let _ = g.warm();
    g.set_columnar_scans(false);
    g
}

const BI6: &str = "MATCH (tag:Tag {name: 't1'})<-[:HAS_TAG]-(message1:Message)-[:HAS_CREATOR]->(person1:Person) \
    OPTIONAL MATCH (message1)<-[:LIKES]-(person2:Person) \
    OPTIONAL MATCH (person2)<-[:HAS_CREATOR]-(message2:Message)<-[like:LIKES]-(person3:Person)";

#[test]
fn bi6s_distinct_count_folds_where_its_rows_are_made() {
    let g = world();
    // as the catalogue spells it: ordered and paged
    folds(
        &g,
        &format!(
            "{BI6} RETURN person1.id, count(DISTINCT like) AS authorityScore \
             ORDER BY authorityScore DESC, person1.id ASC LIMIT 100"
        ),
    );
    // unordered: the first-seen group order is the answer's order
    folds(&g, &format!("{BI6} RETURN person1.id AS p, count(DISTINCT like) AS a"));
}

#[test]
fn the_arrival_order_folds_merge_as_the_serial_fold_made_them() {
    let g = world();
    folds(
        &g,
        &format!(
            "{BI6} RETURN person1.id AS p, count(DISTINCT person2) AS fans, \
             collect(DISTINCT person3.id) AS thirds, collect(message2.id) AS ms, \
             min(like.w) AS lo, max(like.w) AS hi, min(DISTINCT like.w) AS dlo, count(*) AS n"
        ),
    );
}

#[test]
fn a_distinct_over_nans_counts_every_nan() {
    let g = world();
    // NaN never equals NaN: each partial numbers its NaNs apart, and the
    // union keeps every one, as the serial fold does
    folds(
        &g,
        &format!(
            "{BI6} RETURN person1.id AS p, count(DISTINCT message2.f) AS fs, \
             collect(DISTINCT message2.f) AS vals"
        ),
    );
}

#[test]
fn an_aggregating_with_folds_too() {
    let g = world();
    folds(
        &g,
        &format!(
            "{BI6} WITH person1, count(DISTINCT like) AS a WHERE a > 0 \
             RETURN person1.id AS p, a ORDER BY p"
        ),
    );
}

#[test]
fn a_stage_split_by_its_input_rows_folds_too() {
    let g = world();
    folds(
        &g,
        "MATCH (p:Person) WITH p ORDER BY p.id \
         MATCH (p)<-[:HAS_CREATOR]-(m:Message)<-[l:LIKES]-(q:Person) \
         RETURN p.id % 4 AS k, count(DISTINCT q) AS fans, collect(DISTINCT m.id) AS ms, count(*) AS n",
    );
}

#[test]
fn a_morsel_continues_its_folded_predecessors_partial() {
    // run in order, every morsel after a window's first continues the partial
    // before it; run in reverse, none can: both answer as the serial fold
    let g = world();
    for q in [
        format!(
            "{BI6} RETURN person1.id AS p, count(DISTINCT like) AS a, collect(message2.id) AS ms, \
             count(DISTINCT message2.f) AS fs"
        ),
        "MATCH (p:Person) WITH p ORDER BY p.id \
         MATCH (p)<-[:HAS_CREATOR]-(m:Message)<-[l:LIKES]-(q:Person) \
         RETURN p.id % 4 AS k, count(DISTINCT q) AS fans, collect(m.id) AS ms"
            .to_string(),
    ] {
        g.set_exec(None);
        let (want, _) = run(&g, &q);
        g.set_parallel_min_rows(2);
        for reverse in [false, true] {
            g.set_exec(Some(Arc::new(InOrderExec { reverse })));
            let (got, c) = run(&g, &q);
            g.set_exec(None);
            assert_eq!(format!("{got:?}"), format!("{want:?}"), "reverse {reverse}: `{q}`");
            assert!(counter(&c, FOLDED) > 0, "reverse {reverse}: `{q}` was not folded: {c:?}");
        }
    }
}

#[test]
fn a_sum_or_an_average_keeps_the_serial_drain() {
    let g = world();
    for q in [
        format!("{BI6} RETURN person1.id AS p, sum(like.w) AS s"),
        format!("{BI6} RETURN person1.id AS p, avg(like.w) AS a"),
    ] {
        let c = agrees(&g, &q);
        assert_eq!(counter(&c, FOLDED), 0, "`{q}` merged partial sums: {c:?}");
    }
}
