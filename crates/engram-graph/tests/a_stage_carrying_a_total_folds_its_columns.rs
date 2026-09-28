//! A columnar stage takes ONE input row carrying values — the total an
//! earlier stage counted — as its walk's variables, evaluates the plain
//! WITHs leading its prefix over that row once, and folds an aggregate
//! NESTED in a breaker item into a hidden column, with the same answer as
//! the general path.
//!
//! SNB BI bi1 is the shape: `WITH count(message) AS totalMessageCountInt
//! WITH toFloat(totalMessageCountInt) AS totalMessageCount MATCH (message:
//! Message) WHERE … WITH totalMessageCount, message, message.creationDate.
//! year AS year WITH totalMessageCount, year, message:Comment AS isComment,
//! CASE … END AS lengthCategory, count(message) AS messageCount,
//! sum(message.length) / toFloat(count(message)) AS averageMessageLength, …`.
//! Either half alone sent the whole stage to the general path, which decoded
//! each of the 2.1M messages it folds in full at SF3.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

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

/// The same statement with every columnar path off: the general path's answer.
fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let (rows, _) = run(g, q);
    g.set_columnar_scans(true);
    rows
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const CARRIED: &str = "interp.columnar stage carried its input row in as variables";
const LIFTED: &str = "interp.columnar stage lifted an aggregate nested in a breaker item";
const STAGES: &str = "interp.columnar stages";
const FULL: &str = "graph.nodes materialised in full";

/// 400 messages three days apart from 2010-01-01: every third a Comment,
/// the rest Posts; every seventh without content; lengths spread over 0-299.
fn messages() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 399) AS i \
         CREATE (:Message {id: i, length: (i * 37) % 300, \
                           creationDate: datetime.fromepochmillis(1262304000000 + i * 259200000)})",
    );
    ddl(&g, "MATCH (m:Message) WHERE m.id % 3 = 0 SET m:Comment");
    ddl(&g, "MATCH (m:Message) WHERE m.id % 3 <> 0 SET m:Post");
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
fn bi1_folds_its_messages_from_columns_with_the_total_carried_in() {
    let g = messages();
    let want = control(&g, BI1);
    let (got, c) = run(&g, BI1);
    assert_eq!(got, want, "the column walk changed bi1's answer");
    // 2010, 2011 and the first half of 2012, Comment or not, four lengths.
    assert!(want.len() >= 12, "vacuous: {want:?}");
    assert!(counter(&c, CARRIED) > 0, "the total was not carried in: {c:?}");
    assert!(counter(&c, LIFTED) > 0, "the nested aggregate was not lifted: {c:?}");
    // The fold's stage ran as a column walk (the total's is the columnar
    // count's), and no message was decoded whole by either.
    assert!(counter(&c, STAGES) >= 1, "the fold fell to the general path: {c:?}");
    assert_eq!(counter(&c, FULL), 0, "a message was decoded whole: {c:?}");
}

#[test]
fn every_neighbouring_shape_answers_as_the_general_path_does() {
    let g = messages();
    for (q, columnar) in [
        // a nested aggregate ordered and paged by its own alias
        (
            "MATCH (m:Message) WITH m.length % 3 AS k, sum(m.length) * 1.0 / count(*) AS avg \
             ORDER BY avg DESC LIMIT 2 RETURN k, avg",
            true,
        ),
        // a carried total read by the nested item through a key column
        (
            "MATCH (m:Message) WITH count(*) AS total \
             MATCH (m:Message) WHERE m.length > 100 \
             WITH total, count(*) * 100 / total AS pct RETURN total, pct",
            true,
        ),
        // the carried total read in the chain and grouped on
        (
            "MATCH (m:Message) WITH count(*) AS total \
             MATCH (m:Message) WITH total, m.length % 2 AS parity, m \
             WITH total, parity, count(m) AS n RETURN total, parity, n ORDER BY parity",
            true,
        ),
        // no key and no surviving member: the nested item over the empty fold
        (
            "MATCH (m:Message) WHERE m.length > 1000 WITH count(*) + 1 AS c, sum(m.length) * 2 AS s \
             RETURN c, s",
            true,
        ),
        // a nested DISTINCT count, and one of the scanned variable itself
        (
            "MATCH (m:Message) WITH m:Comment AS isComment, \
             count(DISTINCT m.length) * 2 AS d, count(DISTINCT m) + 0 AS n \
             RETURN isComment, d, n ORDER BY isComment",
            true,
        ),
        // a leading WITH that filters: declined
        (
            "MATCH (m:Message) WITH count(*) AS total WITH total WHERE total > 0 \
             MATCH (m:Message) WITH total, count(m) AS n RETURN total, n",
            false,
        ),
        // the carried value read by the MATCH's WHERE: declined
        (
            "MATCH (m:Message) WITH max(m.length) AS top \
             MATCH (m:Message) WHERE m.length = top WITH top, count(m) AS n RETURN top, n",
            false,
        ),
        // a node leaving at the statement's RETURN, past a chain WITH: whole
        (
            "MATCH (m:Message) WITH m, m.length AS x RETURN m ORDER BY x DESC, m.id LIMIT 3",
            false,
        ),
        // ...with a total carried in beside it
        (
            "MATCH (m:Message) WITH count(*) AS total \
             MATCH (m:Message) WITH total, m, m.length AS x RETURN total, m ORDER BY x, m.id LIMIT 3",
            false,
        ),
        // ordered by a nested item's alias, then a key's
        (
            "MATCH (m:Message) WITH m.length % 5 AS k, count(*) * 2 AS twice \
             ORDER BY twice DESC, k RETURN k, twice",
            true,
        ),
    ] {
        let want = control(&g, q);
        let (got, c) = run(&g, q);
        assert!(!want.is_empty(), "vacuous: {q}");
        assert_eq!(got, want, "\n  {q}");
        if columnar {
            assert!(counter(&c, FULL) == 0, "decoded whole: {q}\n{c:?}");
        }
    }
}
