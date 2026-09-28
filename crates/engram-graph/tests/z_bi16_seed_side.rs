#![allow(non_snake_case)]
//! Which end does bi16's OPTIONAL MATCH walk from? SNB BI bi16 hit the 900 s
//! ceiling at SF10 (37 s at SF3).
//!
//! `OPTIONAL MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-
//! (message2:Message)-[:HAS_TAG]->(tag)` has BOTH ends bound. From `person1`
//! it is every friend's every message; from `tag` it is the tag's messages,
//! their creators, and one KNOWS probe each.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn traced(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

#[test]
#[ignore = "diagnostic — run with --ignored --nocapture"]
fn is_a_function_predicate_on_a_hop_end_applied_during_the_walk() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 299) AS i CREATE (:Person {id: i})");
    ddl(&g, "UNWIND range(0, 9) AS i CREATE (:Tag {name: 'T' + toString(i)})");
    ddl(
        &g,
        "UNWIND range(0, 299) AS i UNWIND range(1, 20) AS d          MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 300}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 11999) AS m MATCH (p:Person {id: m % 300})          CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m, day: m % 7,            creationDate: datetime('2011-10-0' + toString(1 + m % 7) + 'T12:00:00Z')})",
    );
    ddl(
        &g,
        "MATCH (m:Message) WITH m, CASE WHEN m.id % 97 = 0 THEN 'T0' ELSE 'T' + toString(1 + m.id % 9) END AS tn          MATCH (t:Tag {name: tn}) CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    g.set_path_estimate(true);
    let head = "MATCH (tag:Tag {name: 'T0'})<-[:HAS_TAG]-(message1:Message)-[:HAS_CREATOR]->(person1:Person)                 WHERE message1.day = 3 ";
    let tail = " WITH person1, count(DISTINCT person2) AS cp2 RETURN person1.id AS p, cp2 ORDER BY p";
    for (label, pred) in [
        ("plain property", "message2.day = 3"),
        ("function of a property", "date(message2.creationDate) = date(datetime('2011-10-04T00:00:00Z'))"),
    ] {
        let q = format!(
            "{head}OPTIONAL MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-(message2:Message)-[:HAS_TAG]->(tag) WHERE {pred}{tail}"
        );
        let (rows, c) = traced(&g, &q);
        let mut v: Vec<_> = c.iter().filter(|(_, v)| **v > 0).collect();
        v.sort();
        println!("--- {label}: {} rows", rows.len());
        for (k, n) in v {
            if k.contains("edge") || k.contains("pattern") || k.contains("filter") || k.contains("predicate") || k.contains("expressions") {
                println!("    {n:>8}  {k}");
            }
        }
    }
}

#[test]
#[ignore = "diagnostic — run with --ignored --nocapture"]
fn which_end_does_bi16s_optional_match_walk_from() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // 300 persons, each KNOWS the next 20; 40 messages each; the rare tag is
    // on every 97th message, only a few of them on the chosen day.
    ddl(&g, "UNWIND range(0, 299) AS i CREATE (:Person {id: i})");
    ddl(&g, "UNWIND range(0, 9) AS i CREATE (:Tag {name: 'T' + toString(i)})");
    ddl(
        &g,
        "UNWIND range(0, 299) AS i UNWIND range(1, 20) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 300}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 11999) AS m MATCH (p:Person {id: m % 300}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m, day: m % 7})",
    );
    ddl(
        &g,
        "MATCH (m:Message) WITH m, CASE WHEN m.id % 97 = 0 THEN 'T0' ELSE 'T' + toString(1 + m.id % 9) END AS tn \
         MATCH (t:Tag {name: tn}) CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    if std::env::var("PATH_EST").is_ok() {
        g.set_path_estimate(true);
    }

    let head = "MATCH (tag:Tag {name: 'T0'})<-[:HAS_TAG]-(message1:Message)-[:HAS_CREATOR]->(person1:Person) \
                WHERE message1.day = 3 ";
    let tail = " WITH person1, count(DISTINCT message1) AS cm, count(DISTINCT person2) AS cp2 \
                RETURN person1.id AS p, cm, cp2 ORDER BY p";
    let mut answers = Vec::new();
    for (label, opt) in [
        (
            "as written, from person1",
            "OPTIONAL MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-(message2:Message)-[:HAS_TAG]->(tag) \
             WHERE message2.day = 3",
        ),
        (
            "reversed by hand, from tag",
            "OPTIONAL MATCH (tag)<-[:HAS_TAG]-(message2:Message)-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(person1) \
             WHERE message2.day = 3",
        ),
    ] {
        let (rows, c) = traced(&g, &format!("{head}{opt}{tail}"));
        let pick = |k: &str| c.get(k).copied().unwrap_or(0);
        println!("--- {label}: {} rows", rows.len());
        println!("    store.gets = {}", pick("store.gets"));
        let mut interesting: Vec<_> = c
            .iter()
            .filter(|(_, v)| **v > 0)
            .collect();
        interesting.sort();
        for (k, v) in interesting {
            println!("    {v:>8}  {k}");
        }
        answers.push(rows);
    }
    assert_eq!(answers[0], answers[1], "the two spellings disagree");
    assert!(!answers[0].is_empty(), "the fixture selected nobody");
}

/// With the whole-path estimate on, bi16's optional leg must be walked from
/// the TAG however it is written, in every run.
///
/// This file found that `shape_tails` could price one statement with an
/// earlier statement's tails (keyed by address and per-hop COUNTS, which this
/// leg shares with its own reversal): run the two spellings back to back and
/// the second turned itself onto the 28,800-probe end. Whether two statements
/// share an address is the allocator's choice, so this test cannot force the
/// collision — `interp::shape_tails_tests` does, deterministically, and fails
/// on the old key. What this one pins is the OUTCOME on the real shape: the
/// cheap end, every time, over repeated alternation.
#[test]
fn bi16s_leg_is_walked_from_the_tag_however_it_is_written() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 299) AS i CREATE (:Person {id: i})");
    ddl(&g, "UNWIND range(0, 9) AS i CREATE (:Tag {name: 'T' + toString(i)})");
    ddl(
        &g,
        "UNWIND range(0, 299) AS i UNWIND range(1, 20) AS d          MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 300}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 11999) AS m MATCH (p:Person {id: m % 300})          CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m, day: m % 7})",
    );
    ddl(
        &g,
        "MATCH (m:Message) WITH m, CASE WHEN m.id % 97 = 0 THEN 'T0' ELSE 'T' + toString(1 + m.id % 9) END AS tn          MATCH (t:Tag {name: tn}) CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    g.set_path_estimate(true);

    let head = "MATCH (tag:Tag {name: 'T0'})<-[:HAS_TAG]-(message1:Message)-[:HAS_CREATOR]->(person1:Person)                 WHERE message1.day = 3 ";
    let tail = " WITH person1, count(DISTINCT message1) AS cm, count(DISTINCT person2) AS cp2                 RETURN person1.id AS p, cm, cp2 ORDER BY p";
    let spellings = [
        "OPTIONAL MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-(message2:Message)-[:HAS_TAG]->(tag)          WHERE message2.day = 3",
        "OPTIONAL MATCH (tag)<-[:HAS_TAG]-(message2:Message)-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(person1)          WHERE message2.day = 3",
    ];
    // Whether two statements' patterns share an address is the allocator's
    // choice, so one pass of each spelling may or may not collide. Alternating
    // them for several rounds makes a collision all but certain, and every
    // run must then price itself: the same probes each time.
    let mut probes = Vec::new();
    let mut answers = Vec::new();
    for round in 0..8 {
        for opt in [spellings[round % 2], spellings[(round + 1) % 2]] {
            let (rows, c) = traced(&g, &format!("{head}{opt}{tail}"));
            probes.push(c.get("graph.edge entries by binary search").copied().unwrap_or(0));
            answers.push(rows);
        }
    }
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "the spellings disagree");
    assert!(!answers[0].is_empty(), "the fixture selected nobody");
    assert!(
        probes.windows(2).all(|w| w[0] == w[1]),
        "the same leg walked from different ends from one run to the next: {probes:?}"
    );
    // 18 rows x 40 friends x 40 messages from person1; ~124 tagged messages per
    // row from the tag. The estimate must pick the tag.
    assert!(probes[0] < 5_000, "walked from person1: {probes:?}");
}
