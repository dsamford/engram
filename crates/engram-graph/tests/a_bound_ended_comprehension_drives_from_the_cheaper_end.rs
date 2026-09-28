//! A pattern comprehension with BOTH ends bound walks from the smaller side.
//!
//! `reverse_bound_end_path` only reverses when the start is genuinely UNBOUND
//! — a bound start already drives from something concrete, so it looks fine.
//! But when both ends are bound the direction still decides the cost.
//!
//! SNB BI bi8:
//! `size([(tag)<-[:HAS_TAG]-(message)-[:HAS_CREATOR]->(person) | message])`
//! with `tag` and `person` both bound, evaluated per person and again per
//! friend. Driven from `tag` it walks every message carrying that tag — tens
//! of thousands at SF3 — to keep the few `person` wrote. Decomposed: the
//! candidate set costs 45 s, and adding this one comprehension took the query
//! past the 300 s ceiling.
//!
//! Reversal changes only the order nodes are DISCOVERED in, never which ones
//! match, so these tests assert answers first and the mechanism second.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, label: &str, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&[label.into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// A "tag" with many messages, one author with two of them, and a decoy
/// author. The asymmetry is the point: walking from the tag is expensive,
/// walking from the author is not.
fn fixture(msgs: usize) -> Graph {
    let g = g();
    let tag = node(&g, "Tag", "t");
    let author = node(&g, "Person", "author");
    let other = node(&g, "Person", "other");
    for i in 0..msgs {
        let m = node(&g, "Message", &format!("m{i}"));
        g.create_rel(m, "HAS_TAG", tag, &BTreeMap::new())
            .expect("rel");
        let who = if i < 2 { author } else { other };
        g.create_rel(m, "HAS_CREATOR", who, &BTreeMap::new())
            .expect("rel");
    }
    g
}

#[test]
fn the_count_is_the_authors_messages_not_the_tags() {
    let g = fixture(40);
    let r = rows(
        &g,
        "MATCH (tag:Tag {name:'t'}), (p:Person {name:'author'}) \
         RETURN size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(p) | m]) AS n",
    );
    assert_eq!(r[0][0], Value::Int(2), "{r:?}");
}

#[test]
fn the_other_author_gets_the_rest() {
    let g = fixture(40);
    let r = rows(
        &g,
        "MATCH (tag:Tag {name:'t'}), (p:Person {name:'other'}) \
         RETURN size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(p) | m]) AS n",
    );
    assert_eq!(r[0][0], Value::Int(38), "{r:?}");
}

#[test]
fn reversing_does_not_change_which_elements_come_back() {
    // The map projects the MESSAGE, so a reversal that mixed up which node
    // binds which variable would show here as different names.
    let g = fixture(6);
    let r = rows(
        &g,
        "MATCH (tag:Tag {name:'t'}), (p:Person {name:'author'}) \
         UNWIND [(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(p) | m.name] AS name \
         RETURN name ORDER BY name",
    );
    let got: Vec<String> = r
        .iter()
        .map(|x| match &x[0] {
            Value::Str(s) => s.to_string(),
            o => format!("{o:?}"),
        })
        .collect();
    assert_eq!(got, vec!["m0", "m1"], "{r:?}");
}

#[test]
fn a_filter_inside_the_comprehension_still_applies() {
    let g = fixture(10);
    let r = rows(
        &g,
        "MATCH (tag:Tag {name:'t'}), (p:Person {name:'other'}) \
         RETURN size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(p) \
                      WHERE m.name = 'm5' | m]) AS n",
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn an_unbound_end_is_left_to_the_existing_reversal() {
    // Only ONE end bound: the existing bound-end reversal owns that case, and
    // the answer must be every message on the tag.
    let g = fixture(5);
    let r = rows(
        &g,
        "MATCH (tag:Tag {name:'t'}) \
         RETURN size([(tag)<-[:HAS_TAG]-(m:Message) | m]) AS n",
    );
    assert_eq!(r[0][0], Value::Int(5), "{r:?}");
}

#[test]
fn a_symmetric_pattern_is_left_alone_and_still_right() {
    // Equal fan-out on both sides: the reversal declines (`>=`), and the
    // answer is unchanged either way.
    let g = g();
    let a = node(&g, "Person", "a");
    let b = node(&g, "Person", "b");
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).expect("rel");
    let r = rows(
        &g,
        "MATCH (x:Person {name:'a'}), (y:Person {name:'b'}) \
         RETURN size([(x)-[:KNOWS]->(y) | y]) AS n",
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn it_engages_when_one_side_is_much_larger() {
    // WARMED FIRST, because the fan-out probe reads a RESIDENT adjacency
    // table and answers `None` when there is none — on a fresh in-memory
    // graph the tables are not admitted yet, so the reversal correctly
    // declines and the test would assert against a decline that production
    // never takes. A served store has these tables: boot warming builds one
    // per type per direction (32 of them at SF3).
    let g = fixture(40);
    let _ = g.warm();
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (tag:Tag {name:'t'}), (p:Person {name:'author'}) \
             RETURN size([(tag)<-[:HAS_TAG]-(m:Message)-[:HAS_CREATOR]->(p) | m]) AS n",
        )
    })
    .1;
    assert!(
        t.counters()
            .contains_key("interp.pattern reversed to drive from the cheaper bound end"),
        "the walk was not reversed: {:?}",
        t.counters()
    );
}
