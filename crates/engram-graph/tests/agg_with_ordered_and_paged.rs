#![allow(non_snake_case)]
//! Fix 32 (v107): an aggregating WITH that filters, orders and pages its own
//! groups in the where-first form — `WITH s, count(DISTINCT e) AS shared
//! WHERE shared >= 1 ORDER BY shared DESC LIMIT 5 RETURN s.storyId AS
//! storyId, s.title AS title` (the story tracker) — parses as the filtering
//! WITH plus a `WITH *` carrying the tail, a four-clause list no pipeline
//! recogniser matched; the general path paid 291 projected reads per
//! statement where the same chain's count on the pipeline paid none (1.8 ms
//! on the mirror vs Neo4j's 0.8). The tail now fuses into a plain concluding
//! RETURN, which the Form-A tails recognise.
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Str("art-0000".to_string()));
    p.insert("cutoff".to_string(), Value::Int(500));
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

const FUSED: &str = "interp.where-first WITH tail fused into the RETURN";
const AGG_RUNS: &str = "interp.pipeline aggregate runs";
const OPTIONAL_RUNS: &str = "interp.pipeline optional runs";
const HOP_RUNS: &str = "interp.pipeline hop runs";
const FULL: &str = "graph.nodes materialised in full";

/// 400 `:Article {articleId}`, 60 `:Entity`, 40 `:Story {storyId, title,
/// status, lastUpdatedAt}`; each article MENTIONS three entities (cycling) and
/// belongs to one story, so the seed article's entities lead to ~120 other
/// articles over a handful of stories with distinct shared-entity counts.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ents = Vec::new();
    for i in 0..60i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("ent-{i}")));
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    let mut stories = Vec::new();
    for i in 0..40i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("story-{i:03}")));
        m.insert("title".to_string(), Value::Str(format!("Story {i}")));
        m.insert(
            "status".to_string(),
            Value::Str(if i % 9 == 0 { "stale".into() } else { "live".into() }),
        );
        m.insert("lastUpdatedAt".to_string(), Value::Int(300 + i * 20));
        stories.push(g.create_node(&["Story".into()], &m).expect("story"));
    }
    for i in 0..400i64 {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("art-{i:04}")));
        let a = g.create_node(&["Article".into()], &m).expect("article");
        for k in 0..3 {
            let e = ents[((i * 7 + k * 13) % 60) as usize];
            g.create_rel(a, "MENTIONS", e, &BTreeMap::new()).expect("mentions");
        }
        let s = stories[((i * 3) % 40) as usize];
        g.create_rel(a, "PART_OF_STORY", s, &BTreeMap::new()).expect("part");
    }
    g
}

const TRACKER: &str = "MATCH (a:Article {articleId: $id})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF_STORY]->(s:Story) \
    WHERE s.status <> 'stale' AND s.lastUpdatedAt > $cutoff \
    WITH s, count(DISTINCT e) AS sharedEntities WHERE sharedEntities >= 1 \
    ORDER BY sharedEntities DESC LIMIT 5 \
    RETURN s.storyId AS storyId, s.title AS title";

#[test]
fn the_story_tracker_runs_on_the_aggregate_pipeline() {
    let g = corpus();
    let want = general(&g, TRACKER);
    assert!(want.len() >= 3, "fixture: {} rows", want.len());
    let (got, c) = traced(&g, TRACKER);
    assert_eq!(got, want);
    assert!(count_of(&c, FUSED) > 0, "{c:?}");
    assert!(count_of(&c, AGG_RUNS) > 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 0, "the stories are gathered, not decoded in full: {c:?}");
}

/// The tail's forms — SKIP with LIMIT, a filter that empties the groups, an
/// ORDER BY on a property of the key, the OPTIONAL and DISTINCT tails, and
/// a plain WITH without a HAVING.
#[test]
fn where_first_tail_variants_agree() {
    let g = corpus();
    for src in [
        "MATCH (a:Article {articleId: $id})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(DISTINCT e) AS shared WHERE shared >= 1 ORDER BY shared DESC, s.storyId SKIP 1 LIMIT 3 \
         RETURN s.storyId AS id, shared",
        "MATCH (a:Article {articleId: $id})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(DISTINCT e) AS shared WHERE shared > 1000 ORDER BY shared DESC LIMIT 5 \
         RETURN s.storyId AS id",
        "MATCH (a:Article {articleId: $id})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(x) AS n WHERE n >= 2 ORDER BY s.title LIMIT 4 \
         RETURN s.title AS title, n",
        "MATCH (s:Story) OPTIONAL MATCH (s)<-[:PART_OF_STORY]-(x:Article) \
         WITH s, count(x) AS n WHERE n >= 0 ORDER BY n DESC, s.storyId LIMIT 6 \
         RETURN s.storyId AS id, n",
        "MATCH (a:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH DISTINCT s WHERE s.status <> 'stale' ORDER BY s.lastUpdatedAt DESC LIMIT 5 \
         RETURN s.storyId AS id",
        "MATCH (a:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(a) AS n WHERE n > 5 ORDER BY n DESC, s.storyId LIMIT 5 \
         RETURN s AS story, n",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, FUSED) > 0, "`{src}`: {c:?}");
        assert!(
            count_of(&c, AGG_RUNS) + count_of(&c, OPTIONAL_RUNS) + count_of(&c, HOP_RUNS) > 0,
            "`{src}` runs on a pipeline tail: {c:?}"
        );
    }
}

/// CONTROLS: a RETURN with its own ORDER BY or DISTINCT, and a RETURN that
/// aggregates, are not fused — and agree.
#[test]
fn a_return_with_its_own_tail_is_not_fused() {
    let g = corpus();
    for src in [
        "MATCH (a:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(a) AS n WHERE n > 5 ORDER BY n DESC LIMIT 5 \
         RETURN s.storyId AS id, n ORDER BY id",
        "MATCH (a:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(a) AS n WHERE n > 5 ORDER BY n DESC LIMIT 5 \
         RETURN DISTINCT s.status AS st",
        "MATCH (a:Article)-[:PART_OF_STORY]->(s:Story) \
         WITH s, count(a) AS n WHERE n > 5 ORDER BY n DESC LIMIT 5 \
         RETURN count(s) AS stories",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, FUSED), 0, "`{src}`: {c:?}");
    }
}
