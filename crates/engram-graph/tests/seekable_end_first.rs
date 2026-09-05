#![allow(non_snake_case)]
//! Fix 38: a statement's FIRST path written from an unbound, unseekable
//! start toward an end whose inline map is a constant on a DECLARED key
//! runs reversed — seeded by the end's seek and walked toward the start.
//! `MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory
//! {storyId: $storyId}) RETURN properties(g) … LIMIT 1` scanned all 43,822
//! events on the mirror and expanded each (57–65 ms against Neo4j's 1.7)
//! for the one story the seek names.
//!
//! Every answer is checked against the spelling written from the seekable
//! end, and against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("storyId".to_string(), Value::Str("story-0042".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const REVERSED: &str = "interp.top-level path reversed to its seekable end";
const SCANNED: &str = "interp.pipeline anchored seed scanned the whole label";

/// 1,000 stories (above the seek's label floor) with a DECLARED `storyId`
/// index; 6,000 events, each deriving from one story (6 per story), with a
/// few properties.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX story_id FOR (n:NewsStory) ON (n.storyId)");
    let mut stories = Vec::new();
    for i in 0..1000i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("story-{i:04}")));
        m.insert("title".to_string(), Value::Str(format!("story {i}")));
        stories.push(g.create_node(&["NewsStory".into()], &m).expect("story"));
    }
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("eventId".to_string(), Value::Str(format!("story-{:04}-ev-{i}", i % 1000)));
        m.insert("severity".to_string(), Value::Float(0.1 * ((i % 9) as f64)));
        m.insert("createdAt".to_string(), Value::Str(format!("2026-08-{:02}T{:02}:{:02}:00Z", 1 + (i % 28), i % 24, i % 60)));
        let e = g.create_node(&["GeopoliticalEvent".into()], &m).expect("event");
        g.create_rel(e, "DERIVES_FROM_STORY", stories[(i % 1000) as usize], &BTreeMap::new())
            .expect("derives");
    }
    g
}

#[test]
fn the_first_path_runs_from_its_seekable_end() {
    let g = corpus();
    for (written, from_end) in [
        (
            "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN count(g) AS n",
            "MATCH (s:NewsStory {storyId: $storyId})<-[:DERIVES_FROM_STORY]-(g:GeopoliticalEvent) RETURN count(g) AS n",
        ),
        (
            "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN g.eventId AS id ORDER BY id",
            "MATCH (s:NewsStory {storyId: $storyId})<-[:DERIVES_FROM_STORY]-(g:GeopoliticalEvent) RETURN g.eventId AS id ORDER BY id",
        ),
        (
            "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN properties(g) AS g ORDER BY g.createdAt ASC LIMIT 1",
            "MATCH (s:NewsStory {storyId: $storyId})<-[:DERIVES_FROM_STORY]-(g:GeopoliticalEvent) RETURN properties(g) AS g ORDER BY g.createdAt ASC LIMIT 1",
        ),
        (
            "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) WHERE g.severity > 0.5 RETURN g.eventId AS id, s.title AS title ORDER BY id",
            "MATCH (s:NewsStory {storyId: $storyId})<-[:DERIVES_FROM_STORY]-(g:GeopoliticalEvent) WHERE g.severity > 0.5 RETURN g.eventId AS id, s.title AS title ORDER BY id",
        ),
    ] {
        let want = rows(&g, from_end);
        assert!(!want.is_empty(), "fixture: `{from_end}`");
        assert_eq!(general(&g, written), want, "general path: `{written}`");
        let (got, c) = traced(&g, written);
        assert_eq!(got, want, "`{written}`");
        assert!(count_of(&c, REVERSED) > 0, "`{written}` reverses: {c:?}");
        assert_eq!(count_of(&c, SCANNED), 0, "`{written}` scans no label: {c:?}");
    }
}

/// CONTROLS: a start that seeks on its own is left as written; a bare LIMIT
/// without ORDER BY is left as written (the pick would move); a second
/// MATCH is not the first path.
#[test]
fn seekable_starts_and_bare_limits_are_left_as_written() {
    let g = corpus();
    for src in [
        "MATCH (s:NewsStory {storyId: $storyId})-[:DERIVES_FROM_STORY]-(g:GeopoliticalEvent) RETURN count(g) AS n",
        "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN g.eventId AS id LIMIT 3",
        "MATCH (g:GeopoliticalEvent) WHERE g.severity > 0.7 RETURN count(g) AS n",
    ] {
        let (_, c) = traced(&g, src);
        assert_eq!(count_of(&c, REVERSED), 0, "`{src}` is left as written: {c:?}");
    }
    let bare = "MATCH (g:GeopoliticalEvent)-[:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN g.eventId AS id LIMIT 3";
    assert_eq!(rows(&g, bare).len(), 3);
}
