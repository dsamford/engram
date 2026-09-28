#![allow(non_snake_case)]
//! Fix 96: a fixed hop completing under one pattern label with a lean
//! demand skips a peer outside the label BEFORE its stack frame — the
//! story match's reverse MENTIONS hop from an entity reached 901 peers of
//! which 718 were emails under a `(a:NewsArticle)` end, about 3 µs each on
//! the mirror (2.65 ms for a bare count) for frames the label test threw
//! away one by one.
//!
//! The counts are pinned against the fixture; a var-length hop, an
//! unlabelled end and a whole-node demand keep the per-frame test.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert(
        "entities".to_string(),
        Value::List(
            (vec![
                Value::Str("ent-03".into()),
                Value::Str("ent-07".into()),
                Value::Str("ent-11".into()),
            ])
            .into(),
        ),
    );
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const SKIPPED: &str = "interp.expansion skipped a non-member peer before its frame";
const REJECTED: &str = "interp.matcher rejected a non-member hop end from membership";

/// Twenty entities; every entity is mentioned by twenty emails and five
/// articles, each article part of one of three stories.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ents = Vec::new();
    for k in 0..20i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("ent-{k:02}")));
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    let mut stories = Vec::new();
    for s in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("story-{s}")));
        stories.push(g.create_node(&["NewsStory".into()], &m).expect("story"));
    }
    for i in 0..400i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("subject".to_string(), Value::Str(format!("mail {i}")));
        let e = g.create_node(&["UserDataNode".into()], &m).expect("email");
        g.create_rel(e, "MENTIONS", ents[(i % 20) as usize], &BTreeMap::new())
            .expect("m");
    }
    for i in 0..100i64 {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("art-{i}")));
        m.insert(
            "classifiedAt".to_string(),
            Value::Str("2026-09-01T00:00:00Z".into()),
        );
        let a = g.create_node(&["NewsArticle".into()], &m).expect("article");
        g.create_rel(a, "MENTIONS", ents[(i % 20) as usize], &BTreeMap::new())
            .expect("m");
        g.create_rel(
            a,
            "PART_OF_STORY",
            stories[(i % 3) as usize],
            &BTreeMap::new(),
        )
        .expect("p");
    }
    g
}

/// The three entities' sixty email mentions are skipped before a frame;
/// the fifteen articles bind, and the story match still answers.
#[test]
fn a_non_member_peers_are_skipped_before_their_frame() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle) RETURN count(*) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(15)]]);
    assert_eq!(count_of(&c, SKIPPED), 60, "{c:?}");
    assert_eq!(count_of(&c, REJECTED), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) \
         WHERE a.classifiedAt IS NOT NULL WITH s, count(DISTINCT entName) AS overlap RETURN s.storyId AS storyId, overlap ORDER BY overlap DESC, storyId LIMIT 1",
    );
    assert_eq!(got, vec![vec![Value::Str("story-0".into()), Value::Int(3)]]);
    assert_eq!(count_of(&c, SKIPPED), 60, "{c:?}");
}

/// CONTROLS: a var-length hop, an unlabelled end and a whole-node demand
/// keep the per-frame test — and answer the same counts.
#[test]
fn b_a_var_length_hop_an_unlabelled_end_and_a_whole_node_demand_keep_the_frames() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS*1..1]-(a:NewsArticle) RETURN count(*) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(15)]]);
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
    assert!(count_of(&c, REJECTED) >= 60, "{c:?}");
    let (got, c) = traced(
        &g,
        "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a) RETURN count(*) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(75)]]);
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
    let (got, c) = traced(
        &g,
        "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle) RETURN a ORDER BY a.articleId",
    );
    assert_eq!(got.len(), 15);
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
}
