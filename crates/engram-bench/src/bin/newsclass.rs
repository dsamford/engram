#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does the NewsArticle classification aggregate spend its time?
//!
//! On the production mirror `MATCH (a:NewsArticle)-[:PART_OF_STORY]->
//! (s:NewsStory) WHERE a.classifiedAt IS NOT NULL AND a.pubDate >=
//! $pubDateCutoff AND (a.abuseStatus IS NULL OR a.abuseStatus IN ['clean',
//! 'approved']) AND s.status IS NOT NULL RETURN s.status AS key,
//! count(DISTINCT a) AS n` answers five groups in 145 ms (Neo4j 457) with
//! 94,606 expressions evaluated in its trace — per row of something. This
//! bin times the shape and its parts on a paged store.
//!
//! ```text
//! newsclass [articles=60000] [stories=8000] [iters=5]
//! ```
use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn s(k: &str, v: String) -> (String, Value) {
    (k.to_string(), Value::Str(v))
}

fn iso(day: i64, minute: i64) -> String {
    // Days since 2026-05-01, kept lexicographically ordered.
    let month = 5 + day / 30;
    let dom = 1 + day % 30;
    format!(
        "2026-{month:02}-{dom:02}T{:02}:{:02}:00.000Z",
        minute / 60 % 24,
        minute % 60
    )
}

fn article(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    for (k, v) in [
        s("articleId", format!("{i:032x}")),
        s("title", format!("Article {i}: {}", "a headline ".repeat(3))),
        s("url", format!("https://news.example.net/{i}")),
        s("pubDate", iso(i % 120, i % 1440)),
        s(
            "source",
            ["Reuters", "AP", "BBC", "Guardian"][(i % 4) as usize].into(),
        ),
    ] {
        m.insert(k, v);
    }
    if i % 5 != 0 {
        m.insert(
            "classifiedAt".to_string(),
            Value::Str(iso(i % 120 + 1, i % 1440)),
        );
    }
    match i % 20 {
        0..=13 => {}
        14..=17 => {
            m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
        }
        18 => {
            m.insert("abuseStatus".to_string(), Value::Str("approved".into()));
        }
        _ => {
            m.insert("abuseStatus".to_string(), Value::Str("quarantined".into()));
        }
    }
    m.insert(
        "content".to_string(),
        Value::Str("a paragraph of text ".repeat(20)),
    );
    m
}

fn story(j: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("storyId".to_string(), Value::Str(format!("story-{j:08x}")));
    m.insert("title".to_string(), Value::Str(format!("Story {j}")));
    if j % 10 != 0 {
        m.insert(
            "status".to_string(),
            Value::Str(
                [
                    "active",
                    "active",
                    "stale",
                    "developing",
                    "active",
                    "resolved",
                ][(j % 6) as usize]
                    .into(),
            ),
        );
    }
    m
}

fn corpus(articles: i64, stories: i64) -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut story_ids = Vec::with_capacity(stories as usize);
    for j in 0..stories {
        story_ids.push(
            g.create_node(&["NewsStory".into()], &story(j))
                .expect("story"),
        );
    }
    // Entities: six well-known names each mentioned by ~450 articles, plus a
    // long tail — the UNWIND-entities overlap's shape.
    let mut entity_ids = Vec::new();
    for e in 0..400i64 {
        let mut m = BTreeMap::new();
        let name = match e {
            0 => "The Guardian".to_string(),
            1 => "Washington Post".to_string(),
            2 => "Reuters".to_string(),
            3 => "Associated Press".to_string(),
            4 => "BBC".to_string(),
            5 => "CNN".to_string(),
            _ => format!("Entity {e}"),
        };
        m.insert("name".to_string(), Value::Str(name));
        m.insert("type".to_string(), Value::Str("organization".into()));
        entity_ids.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..articles {
        let a = g
            .create_node(&["NewsArticle".into()], &article(i))
            .expect("article");
        if i % 10 != 9 {
            let sid = story_ids[((i * 7919) % stories) as usize];
            g.create_rel(a, "PART_OF_STORY", sid, &BTreeMap::new())
                .expect("part of story");
        }
        // Every 22nd article mentions one of the six names; every article
        // mentions two tail entities.
        if i % 22 == 0 {
            g.create_rel(
                a,
                "MENTIONS",
                entity_ids[(i / 22 % 6) as usize],
                &BTreeMap::new(),
            )
            .expect("mentions");
        }
        for k in 0..2 {
            g.create_rel(
                a,
                "MENTIONS",
                entity_ids[(6 + (i * 13 + k * 101) % 394) as usize],
                &BTreeMap::new(),
            )
            .expect("mentions");
        }
        if i % 3 == 0 {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i));
            g.create_node(&["Filler".into()], &m).expect("filler");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_newsclass_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 256 * 1024 * 1024)
        .expect("into_paged");
    (Graph::new(store, Realm(1), Namespace(1)), dir)
}

fn time(label: &str, iters: usize, mut f: impl FnMut() -> usize) {
    let rows = f();
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let per_round = t0.elapsed().as_secs_f64() * 1e3 / iters as f64;
    println!("{label:<14} {per_round:9.3} ms per round   rows {rows}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: i64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let articles = arg(1, 60_000);
    let stories = arg(2, 8_000);
    let iters = arg(3, 5) as usize;
    let t0 = Instant::now();
    let (g, dir) = corpus(articles, stories);
    println!(
        "articles {articles}, stories {stories}, iters {iters}, built in {:.1} s, paged store {}",
        t0.elapsed().as_secs_f64(),
        dir.display()
    );
    let mut params = BTreeMap::new();
    params.insert("pubDateCutoff".to_string(), Value::Str(iso(45, 0)));
    params.insert(
        "entities".to_string(),
        Value::List(
            [
                "The Guardian",
                "Washington Post",
                "Reuters",
                "Associated Press",
                "BBC",
                "CNN",
            ]
            .iter()
            .map(|s| Value::Str((*s).to_string()))
            .collect::<Vec<_>>()
            .into(),
        ),
    );
    let run = |src: &str| -> usize {
        let q = parse_statement(src).expect("parse");
        run_query(&g, &q, params.clone()).expect("run").rows.len()
    };
    let a_pred = "a.classifiedAt IS NOT NULL AND a.pubDate >= $pubDateCutoff AND (a.abuseStatus IS NULL OR a.abuseStatus IN ['clean', 'approved'])";
    let cases: &[(&str, String)] = &[
        ("seed-count", format!("MATCH (a:NewsArticle) WHERE {a_pred} RETURN count(a) AS n")),
        ("label-group", format!("MATCH (a:NewsArticle) WHERE {a_pred} RETURN a.source AS key, count(a) AS n")),
        ("label-group-dist", format!("MATCH (a:NewsArticle) WHERE {a_pred} RETURN a.source AS key, count(DISTINCT a) AS n")),
        ("label-group-coal", format!("MATCH (a:NewsArticle) WHERE {a_pred} RETURN coalesce(a.abuseStatus, 'none') AS key, count(*) AS n")),
        ("hop-count", format!("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE {a_pred} RETURN count(*) AS n")),
        ("hop-s-pred", format!("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE {a_pred} AND s.status IS NOT NULL RETURN count(*) AS n")),
        ("group-count", format!("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE {a_pred} AND s.status IS NOT NULL RETURN s.status AS key, count(a) AS n")),
        ("orig", format!("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE {a_pred} AND s.status IS NOT NULL RETURN s.status AS key, count(DISTINCT a) AS n")),
        ("no-or", String::from("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE a.classifiedAt IS NOT NULL AND a.pubDate >= $pubDateCutoff AND s.status IS NOT NULL RETURN s.status AS key, count(DISTINCT a) AS n")),
        ("global-distinct", format!("MATCH (a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE {a_pred} AND s.status IS NOT NULL RETURN count(DISTINCT a) AS n")),
        ("overlap-6", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE a.classifiedAt IS NOT NULL AND a.classifiedAt >= $pubDateCutoff WITH s, count(DISTINCT entName) AS overlap RETURN s.storyId AS storyId, overlap ORDER BY overlap DESC LIMIT 1".to_string()),
        ("overlap-hop", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle) RETURN count(*) AS n".to_string()),
        ("overlap-2hop", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) RETURN count(*) AS n".to_string()),
        ("overlap-where-null", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE a.classifiedAt IS NOT NULL RETURN count(*) AS n".to_string()),
        ("overlap-where-cmp", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WHERE a.classifiedAt >= $pubDateCutoff RETURN count(*) AS n".to_string()),
        ("overlap-agg-only", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s, count(DISTINCT entName) AS overlap RETURN s.storyId AS storyId, overlap ORDER BY overlap DESC LIMIT 1".to_string()),
        ("overlap-agg-count", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s, count(*) AS overlap RETURN s.storyId AS storyId, overlap ORDER BY overlap DESC LIMIT 1".to_string()),
        ("overlap-agg-noorder", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s, count(*) AS overlap RETURN s.storyId AS storyId, overlap".to_string()),
        ("overlap-agg-withorder", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s, count(*) AS overlap ORDER BY overlap DESC LIMIT 1 RETURN s.storyId AS storyId, overlap".to_string()),
        ("overlap-agg-idkey", "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s.storyId AS storyId, count(*) AS overlap RETURN storyId, overlap ORDER BY overlap DESC LIMIT 1".to_string()),
    ];
    for (label, src) in cases {
        time(label, iters, || run(src));
    }
    let traced: Vec<usize> = std::env::var("NEWSCLASS_TRACE")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![4]);
    let top: usize = std::env::var("NEWSCLASS_TOP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    for which in traced {
        let (_, trace) = engram_observe::with_trace(|| run(&cases[which].1));
        let mut counters: Vec<(&String, &u64)> = trace.counters().iter().collect();
        counters.sort_by(|a, b| b.1.cmp(a.1));
        for (k, v) in counters.iter().take(top) {
            println!("   traced {}:  {v:>8}  {k}", cases[which].0);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
