#![allow(non_snake_case)]
//! The LDBC IC12 shape. Its stage 1 — `MATCH (tag:Tag)-[:HAS_TYPE|IS_SUBCLASS_OF
//! *0..]->(baseTagClass:TagClass) WHERE tag.name='Music' OR baseTagClass.name=
//! 'Music' WITH collect(tag.id) AS tags` — carries a `*0..` multi-type var-length
//! and a spanning-OR WHERE, but it is a GLOBAL collect: the aggregate-list prelude
//! EVALUATES it (via the interp, which handles `*0..`) and injects the id list as
//! `$tags`. Stage 2 is then a fixed chain with `tag.id IN $tags` (a const-param
//! property membership) + a group-by `collect(DISTINCT tag.name)` /
//! `count(DISTINCT comment)` + `toInteger` ORDER BY. Byte-identical to the interp.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    // Tag hierarchy: tag1 (Music) -HAS_TYPE-> tc_music (TagClass); tag2 (Rock)
    // -IS_SUBCLASS_OF-> tc_music. Both qualify (tag.name='Music' OR base='Music').
    let tc_music = mk("TagClass", &[("name", Value::Str("Music".into()))]);
    let tc_other = mk("TagClass", &[("name", Value::Str("Other".into()))]);
    let tag1 = mk(
        "Tag",
        &[
            ("id", Value::Int(100)),
            ("name", Value::Str("Music".into())),
        ],
    );
    let tag2 = mk(
        "Tag",
        &[("id", Value::Int(200)), ("name", Value::Str("Rock".into()))],
    );
    let tag3 = mk(
        "Tag",
        &[("id", Value::Int(300)), ("name", Value::Str("Jazz".into()))],
    );
    g.create_rel(tag1, "HAS_TYPE", tc_other, &BTreeMap::new())
        .unwrap(); // tag.name=Music → qualifies
    g.create_rel(tag2, "IS_SUBCLASS_OF", tc_music, &BTreeMap::new())
        .unwrap(); // base=Music → qualifies
    g.create_rel(tag3, "HAS_TYPE", tc_other, &BTreeMap::new())
        .unwrap(); // neither → excluded
    // People + reply chains.
    let root = mk("Person", &[("id", Value::Int(10))]);
    let person = |id: i64, first: &str, last: &str| {
        mk(
            "Person",
            &[
                ("id", Value::Int(id)),
                ("firstName", Value::Str(first.into())),
                ("lastName", Value::Str(last.into())),
            ],
        )
    };
    let f1 = person(11, "Ana", "One");
    let f2 = person(12, "Bob", "Two");
    g.create_rel(root, "KNOWS", f1, &BTreeMap::new()).unwrap();
    g.create_rel(root, "KNOWS", f2, &BTreeMap::new()).unwrap();
    // A comment by `creator`, replying to a post tagged `tag`.
    let reply = |creator: u64, tag: u64| {
        let post = mk("Post", &[]);
        g.create_rel(post, "HAS_TAG", tag, &BTreeMap::new())
            .unwrap();
        let comment = mk("Comment", &[]);
        g.create_rel(comment, "HAS_CREATOR", creator, &BTreeMap::new())
            .unwrap();
        g.create_rel(comment, "REPLY_OF", post, &BTreeMap::new())
            .unwrap();
    };
    reply(f1, tag1); // f1: post tagged Music (id 100 ∈ tags) → counts
    reply(f1, tag2); // f1: post tagged Rock  (id 200 ∈ tags) → counts
    reply(f2, tag3); // f2: post tagged Jazz  (id 300 ∉ tags) → excluded
    g
}

const IC12: &str = "MATCH (tag:Tag)-[:HAS_TYPE|IS_SUBCLASS_OF*0..]->(baseTagClass:TagClass) \
    WHERE tag.name = 'Music' OR baseTagClass.name = 'Music' \
    WITH collect(tag.id) AS tags \
    MATCH (:Person {id: 10})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(:Post)-[:HAS_TAG]->(tag:Tag) \
    WHERE tag.id IN tags \
    RETURN friend.id AS personId, friend.firstName AS personFirstName, friend.lastName AS personLastName, \
      collect(DISTINCT tag.name) AS tagNames, count(DISTINCT comment) AS replyCount \
    ORDER BY replyCount DESC, toInteger(personId) ASC LIMIT 20";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

#[test]
fn ic12_on_equals_off() {
    let g = g();
    let (on, off) = both(&g, IC12);
    assert_eq!(on, off, "IC12 columnar vs interp disagree");
    // f1 has 2 distinct comments (Music + Rock tags, both in tags); f2's Jazz tag
    // is not in tags, so f2 is absent.
    assert_eq!(on.len(), 1, "only f1 qualifies");
    assert_eq!(on[0][0], Value::Int(11), "personId 11 (Ana)");
    assert_eq!(on[0][4], Value::Int(2), "replyCount = 2 distinct comments");
    // Stage 1 (`*0..`) is evaluated once via the interp under a suppressed trace;
    // stage 2 fires columnar (the anchor+WHERE core chain), so the MAIN query does
    // not stream.
    assert!(
        !streamed(&g, IC12),
        "IC12 stage 2 must fire columnar, not stream"
    );
}

#[test]
fn ic12_stage1_uses_the_anchored_hierarchy_prelude() {
    let g = g();
    g.set_columnar_scans(true);
    let (_rows, trace) = engram_observe::with_trace(|| rows(&g, IC12));
    assert!(
        trace
            .counters()
            .get("interp.pipeline anchored hierarchy collect served a prelude")
            .copied()
            .unwrap_or(0)
            > 0,
        "IC12 stage 1 must be served by the anchored-hierarchy prelude (not the all-tags scan)"
    );
    // And it did NOT fall back to the row-at-a-time stream for stage 2.
    assert!(!streamed(&g, IC12), "IC12 must run columnar, not streamed");
}
