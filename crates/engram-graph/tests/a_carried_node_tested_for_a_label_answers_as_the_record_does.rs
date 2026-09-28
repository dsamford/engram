//! A node CARRIED by a WITH and then tested for a label, counted and read by
//! property — SNB BI bi1's shape — answers as the record-read control does, in
//! the same stage and past a stage boundary.
//!
//! bi1's carried `message` is deliberately still decoded in FULL. Binding it
//! lean was built and measured (rev21, 2026-09-23): the tested label rode from
//! membership and no record was decoded in full, and bi1 got SLOWER at SF3 —
//! 35 s cold and 53 s warm against 33 s / 21 s. The lean bind reads its
//! properties from whole-label COLUMNS, one of them `content` (for `content IS
//! NOT NULL`, which the full WHERE re-evaluates after the seed prefilter), and a
//! string column over 9M messages is larger than the whole property-column
//! budget: `keep_prop_column` can never keep it, so every statement walked all
//! 9M records to build a column it then threw away, evicting the others on the
//! way. Until the column loader knows a column it can never keep, the full
//! decode of the 2.1M filtered messages is the cheaper plan.
//!
//! DIFFERENTIAL against the columnar paths off (every seed read from its
//! record): identical rows.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_query(g, &s, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
        .rows
}

fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = run(g, q);
    g.set_columnar_scans(true);
    r
}

/// 400 messages: every third a `:Comment`, each with a length and a fat body.
fn messages() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 399) AS i CREATE (:Message {id: i, length: i % 170, day: i % 7, \
         content: 'a body long enough that decoding it costs something real'})",
    );
    ddl(&g, "MATCH (m:Message) WHERE m.id % 3 = 0 SET m:Comment");
    let _ = g.warm();
    g
}

/// bi1's own shape, scaled down: a carried node, a label test, a nested count.
const BI1: &str = "MATCH (message:Message) WHERE message.id < 390 AND message.content IS NOT NULL \
     WITH message, message.day AS day \
     WITH day, message:Comment AS isComment, \
          CASE WHEN message.length < 40 THEN 0 WHEN message.length < 80 THEN 1 ELSE 2 END AS lengthCategory, \
          count(message) AS messageCount, \
          sum(message.length) / toFloat(count(message)) AS averageMessageLength \
     RETURN day, isComment, lengthCategory, messageCount, averageMessageLength \
     ORDER BY day, isComment, lengthCategory";

#[test]
fn bi1s_carried_message_answers_as_the_record_does() {
    let g = messages();
    let want = control(&g, BI1);
    assert_eq!(want.len(), 42, "the control's own answer: 7 days x 2 x 3 groups: {want:?}");
    let comments: i64 = want
        .iter()
        .filter(|r| r[1] == Value::Bool(true))
        .map(|r| match r[3] {
            Value::Int(n) => n,
            ref other => panic!("messageCount {other:?}"),
        })
        .sum();
    assert_eq!(comments, 130, "the control counts every third of 390 messages as a comment");
    let _ = run(&g, BI1); // warm
    assert_eq!(run(&g, BI1), want);
}

/// The same label test one stage LATER — past an aggregating or top-k WITH —
/// and a count nested in an expression.
#[test]
fn a_label_tested_past_a_stage_boundary_answers_as_the_record_does() {
    let g = messages();
    for q in [
        "MATCH (m:Message) WHERE m.id < 390 WITH m, count(*) AS one \
         WITH m:Comment AS c, m.length AS len RETURN c, count(*) AS n, sum(len) AS s ORDER BY c",
        "MATCH (m:Message) WITH m ORDER BY m.id LIMIT 90 \
         WITH m:Comment AS c RETURN c, count(*) AS n ORDER BY c",
        "MATCH (m:Message) WITH m, m.day AS day \
         WITH day, sum(m.length) / toFloat(count(m)) AS avg, count(m) + 0 AS n \
         RETURN day, avg, n ORDER BY day",
    ] {
        let want = control(&g, q);
        assert!(!want.is_empty(), "the control answered nothing for `{q}`");
        assert_eq!(run(&g, q), want, "`{q}`");
    }
}
