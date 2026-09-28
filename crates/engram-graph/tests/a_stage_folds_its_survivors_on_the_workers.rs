//! A columnar stage ending in an aggregating breaker folds its survivors on
//! the WORKERS: one contiguous share of them per worker, each walked from the
//! cached columns into its own partial fold, the partials merged in share
//! order. Every answer is the serial fold's, row for row — its group order,
//! its collect order, its float sums and averages bit for bit (a partial
//! keeps its float addends in arrival order; the finish adds them in the
//! serial order), and its integer overflow, raised exactly where the serial
//! order raises it and nowhere else.
//!
//! SNB BI bi1 folds millions of its 9M messages in this stage at SF3, on one
//! thread: 4.0 s against Neo4j's 2.8.

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

type Counters = BTreeMap<String, u64>;

/// The statement's answer — its rows, or the error it raised — and counters.
fn attempt(g: &Graph, q: &str) -> (Result<Vec<Vec<Value>>, String>, Counters) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (out, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .map(|r| r.rows)
            .map_err(|e| e.to_string())
    });
    (out, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn counter(c: &Counters, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const ON_WORKERS: &str = "interp.columnar stage folded its survivors on the workers";
const SUM_REFUSED: &str =
    "interp.columnar stage fold on the workers declined: a sum the serial order would not reproduce";
const SHARE_FAILED: &str =
    "interp.columnar stage fold on the workers declined: a share did not fold";

/// The serial answer (no executor; the walk keeps the label's columns), then
/// four workers' over the kept columns: the two must agree, rows or error.
/// Returns the four workers' counters.
fn agrees(g: &Graph, q: &str) -> (Result<Vec<Vec<Value>>, String>, Counters) {
    g.set_exec(None);
    let (want, serial) = attempt(g, q);
    assert_eq!(counter(&serial, ON_WORKERS), 0, "{serial:?}");
    g.set_exec(Some(Arc::new(TestExec(4))));
    g.set_parallel_min_rows(2);
    let (got, c) = attempt(g, q);
    g.set_exec(None);
    assert_eq!(got, want, "the workers' answer differs from the serial one for `{q}`");
    (got, c)
}

/// A statement led by a carried constant, as bi1's second stage is led by its
/// carried total: the shape the columnar STAGE takes. A bare `MATCH … WITH
/// <aggregates> RETURN …` is the columnar aggregate's, whole, and never
/// reaches the stage.
fn stage(q: &str) -> String {
    format!("WITH 1 AS one {q}")
}

/// [`agrees`] for a statement that answers rows, and folded on the workers.
fn folds_on_the_workers(g: &Graph, q: &str) {
    let (got, c) = agrees(g, q);
    let rows = got.unwrap_or_else(|e| panic!("`{q}` raised {e}"));
    assert!(!rows.is_empty(), "vacuous: `{q}` answered nothing");
    assert!(counter(&c, ON_WORKERS) > 0, "`{q}` folded on one thread: {c:?}");
}

/// 400 messages three days apart from 2010-01-01: every third a Comment,
/// every seventh without content; lengths spread over 0-299.
fn messages() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 399) AS i \
         CREATE (:Message {id: i, length: (i * 37) % 300, \
                           creationDate: datetime.fromepochmillis(1262304000000 + i * 259200000)})",
    );
    ddl(&g, "MATCH (m:Message) WHERE m.id % 3 = 0 SET m:Comment");
    ddl(
        &g,
        "MATCH (m:Message) WHERE m.id % 7 <> 0 SET m.content = 'c' + toString(m.id)",
    );
    let _ = g.warm();
    g
}

/// SNB BI bi1, as the catalogue spells it, at a cutoff inside the corpus.
const BI1: &str = "MATCH (message:Message) \
    WHERE message.creationDate < datetime('2012-06-01T00:00:00.000') \
    WITH count(message) AS totalMessageCountInt \
    WITH toFloat(totalMessageCountInt) AS totalMessageCount \
    MATCH (message:Message) \
    WHERE message.creationDate < datetime('2012-06-01T00:00:00.000') \
      AND message.content IS NOT NULL \
    WITH totalMessageCount, message, message.creationDate.year AS year \
    WITH totalMessageCount, year, message:Comment AS isComment, \
      CASE WHEN message.length < 40 THEN 0 WHEN message.length < 80 THEN 1 \
           WHEN message.length < 160 THEN 2 ELSE 3 END AS lengthCategory, \
      count(message) AS messageCount, \
      sum(message.length) / toFloat(count(message)) AS averageMessageLength, \
      sum(message.length) AS sumMessageLength \
    RETURN year, isComment, lengthCategory, messageCount, averageMessageLength, \
      sumMessageLength, messageCount / totalMessageCount AS percentageOfMessages \
    ORDER BY year DESC, isComment ASC, lengthCategory ASC";

#[test]
fn bi1_folds_its_survivors_on_the_workers() {
    let g = messages();
    folds_on_the_workers(&g, BI1);
}

#[test]
fn the_groups_and_the_arrival_order_folds_are_the_serial_folds() {
    let g = messages();
    for q in [
        // no ORDER BY: the first-seen group order is the answer's order;
        // collect keeps arrival order, a min or max tie keeps the first
        "MATCH (m:Message) WHERE m.length > 10 \
         WITH m.length % 7 AS k, count(*) AS n, collect(m.id) AS ids, \
              min(m.length % 5) AS lo, max(m.length % 5) AS hi \
         RETURN k, n, ids, lo, hi",
        // a predicate read per row (it tests a label), not column-at-a-time
        "MATCH (m:Message) WHERE m:Comment AND m.length > 5 \
         WITH m.length % 4 AS k, count(*) AS n, sum(m.length) AS s RETURN k, n, s",
        // no key: one group over every share
        "MATCH (m:Message) WHERE m.content IS NOT NULL \
         WITH count(*) AS n, sum(m.length) AS s, collect(m.id) AS ids RETURN n, s, ids",
    ] {
        folds_on_the_workers(&g, &stage(q));
    }
}

#[test]
fn a_float_sum_is_added_in_the_serial_order() {
    // 1e16 + 1.0 rounds back to 1e16 (the doubles there are 2 apart), so the
    // order of the additions is the answer: in the serial order the four 1.0s
    // before -1e16 vanish and the sum is 3.0; summed per share of two and
    // then merged, it is 4.0.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    eight(&g, "F", "[1.0e16, 1.0, 1.0, 1.0, -1.0e16, 1.0, 1.0, 1.0]");
    let _ = g.warm();
    let (got, c) = agrees(&g, &stage("MATCH (n:F) WITH sum(n.v) AS s, avg(n.v) AS a RETURN s, a"));
    let rows = got.expect("sums");
    assert_eq!(rows[0][0], Value::Float(3.0), "{rows:?}");
    assert!(counter(&c, ON_WORKERS) > 0, "{c:?}");
    assert_eq!(counter(&c, SUM_REFUSED), 0, "{c:?}");

    let g = messages();
    // floats in every share, keyed and not
    folds_on_the_workers(
        &g,
        &stage(
            "MATCH (m:Message) \
             WITH m.length % 3 AS k, sum(m.length * 0.1) AS s, avg(m.length * 0.7) AS a \
             RETURN k, s, a",
        ),
    );
    // floats in the first share alone: the later shares sum integers
    folds_on_the_workers(
        &g,
        &stage(
            "MATCH (m:Message) \
             WITH m.length % 3 AS k, \
                  sum(CASE WHEN m.id < 50 THEN m.length * 0.1 ELSE m.length END) AS s \
             RETURN k, s",
        ),
    );
}

/// Eight nodes of `label` whose `v` is `vs`, in id order — two per share
/// under four workers.
fn eight(g: &Graph, label: &str, vs: &str) {
    ddl(
        g,
        &format!("WITH {vs} AS vs UNWIND range(0, 7) AS i CREATE (:{label} {{i: i, v: vs[i]}})"),
    );
}

const MAX: i64 = i64::MAX;

#[test]
fn an_integer_sum_overflows_exactly_where_the_serial_order_does() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // The serial order's running total passes i64::MAX at the third value,
    // inside the second share — though each share's own total fits, and so
    // does the whole: [MAX, -1] then [2, -3]. A merge of totals answers
    // MAX - 2; the serial fold raises.
    eight(&g, "Over", &format!("[{MAX}, -1, 2, -3, 0, 0, 0, 0]"));
    // A share overflows on its own — [MAX, 3] — where the serial order never
    // does: -5, -5, MAX - 5, MAX - 2.
    eight(&g, "Under", &format!("[-5, 0, {MAX}, 3, 0, 0, 0, 0]"));
    // Mixed signs within ten of the edge and never over it: merged exactly.
    eight(
        &g,
        "Edge",
        &format!("[{}, -20, 15, 5, -3, 1, 2, 3]", MAX - 10),
    );
    let _ = g.warm();

    let (got, c) = agrees(&g, &stage("MATCH (n:Over) WITH sum(n.v) AS s RETURN s"));
    let err = got.expect_err("the serial order overflows");
    assert!(err.to_lowercase().contains("overflow"), "{err}");
    assert!(counter(&c, SUM_REFUSED) > 0, "the merge took the totals: {c:?}");

    let (got, c) = agrees(&g, &stage("MATCH (n:Under) WITH sum(n.v) AS s RETURN s"));
    assert_eq!(got, Ok(vec![vec![Value::Int(MAX - 2)]]));
    assert!(counter(&c, SHARE_FAILED) > 0, "{c:?}");

    let (got, c) = agrees(&g, &stage("MATCH (n:Edge) WITH sum(n.v) AS s RETURN s"));
    assert_eq!(got, Ok(vec![vec![Value::Int(MAX - 7)]]));
    assert!(counter(&c, ON_WORKERS) > 0, "{c:?}");
}

#[test]
fn the_error_raised_is_the_serial_orders_first() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // a string in the third share, an overflow in the fourth: the serial
    // fold meets the string first
    eight(&g, "Bad", &format!("[1, 2, 3, 4, 'x', 6, {MAX}, 1]"));
    let _ = g.warm();
    let (got, c) = agrees(&g, &stage("MATCH (n:Bad) WITH sum(n.v) AS s RETURN s"));
    let err = got.expect_err("sum() over a string");
    assert!(err.contains("over a string"), "{err}");
    assert!(counter(&c, SHARE_FAILED) > 0, "{c:?}");
}

#[test]
fn what_cannot_merge_exactly_stays_on_one_thread() {
    let g = messages();
    for q in [
        "MATCH (m:Message) WITH m.length % 3 AS k, count(DISTINCT m.length % 11) AS d RETURN k, d",
        "MATCH (m:Message) WITH m.length % 3 AS k, sum(DISTINCT m.length) AS s RETURN k, s",
    ] {
        let q = stage(q);
        let (got, c) = agrees(&g, &q);
        assert!(got.is_ok_and(|r| !r.is_empty()), "{q}");
        assert_eq!(counter(&c, ON_WORKERS), 0, "`{q}` merged partials: {c:?}");
    }
}

#[test]
fn a_column_not_yet_kept_is_assembled_on_one_thread_first() {
    let g = messages();
    let q = stage(
        "MATCH (m:Message) WHERE m.length > 10 WITH m.length % 7 AS k, count(*) AS n RETURN k, n",
    );
    g.set_exec(Some(Arc::new(TestExec(4))));
    g.set_parallel_min_rows(2);
    // nothing kept yet: the serial walk assembles the columns and keeps them
    let (first, c) = attempt(&g, &q);
    assert_eq!(counter(&c, ON_WORKERS), 0, "{c:?}");
    // kept: the next run shares them out
    let (second, c) = attempt(&g, &q);
    assert!(counter(&c, ON_WORKERS) > 0, "{c:?}");
    g.set_exec(None);
    assert_eq!(first, second);
    assert!(first.is_ok_and(|r| !r.is_empty()));
}
