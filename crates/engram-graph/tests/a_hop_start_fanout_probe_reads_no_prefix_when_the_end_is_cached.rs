#![allow(non_snake_case)]
//! Fix 113: the fan-out probe that warms a hop end's columns (fix 83) asks
//! its cheap questions first — when the end's demanded columns are already
//! cached there is nothing to warm and no start's adjacency is walked — and
//! reads a fan-out it does need from the type's adjacency table.
//!
//! The production six-entity story overlap (`UNWIND $entities AS entName
//! MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_
//! STORY]->(s:NewsStory) WHERE … WITH s, count(DISTINCT entName) AS overlap
//! RETURN s.storyId, overlap ORDER BY overlap DESC LIMIT 1`) walked the
//! prefix of every one of its 2,702 article starts — a store scan and, on
//! the paged mirror, a block read each — before expanding it, to decide
//! whether to warm a `storyId` column it had been reading from the cache
//! all along: 173 ms against Neo4j's 26.
//!
//! The rows are pinned against a hand-computed answer; a hop whose end
//! columns are cold still warms them for a wide fan-out (fix 83's case).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SKIPPED: &str = "interp.matcher skipped the fan-out probe: the end's columns are cached";
const WARMED: &str = "interp.matcher warmed a hop end label's columns for a wide fan-out";
const SCANS: &str = "store.visitor scans";

const ARTICLES: i64 = 1_500;
const STORIES: i64 = 300;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert(
        "entities".to_string(),
        Value::List(
            ["The Guardian", "Reuters", "BBC"]
                .iter()
                .map(|s| Value::Str((*s).to_string()))
                .collect::<Vec<_>>()
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

/// Article `i` belongs to story `i % STORIES`; every 10th article mentions
/// one of the three names (i / 10 % 3), every article mentions two tail
/// entities. Paged, so a prefix walk is a block read.
fn corpus(tag: &str) -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut stories = Vec::new();
    for j in 0..STORIES {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("story-{j:04}")));
        m.insert("title".to_string(), Value::Str(format!("Story {j}")));
        stories.push(g.create_node(&["NewsStory".into()], &m).expect("story"));
    }
    let mut entities = Vec::new();
    for e in 0..60i64 {
        let mut m = BTreeMap::new();
        let name = match e {
            0 => "The Guardian".to_string(),
            1 => "Reuters".to_string(),
            2 => "BBC".to_string(),
            _ => format!("Entity {e}"),
        };
        m.insert("name".to_string(), Value::Str(name));
        entities.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..ARTICLES {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("{i:032x}")));
        m.insert(
            "classifiedAt".to_string(),
            Value::Str(format!("2026-08-{:02}", 1 + i % 28)),
        );
        let a = g.create_node(&["NewsArticle".into()], &m).expect("article");
        g.create_rel(
            a,
            "PART_OF_STORY",
            stories[(i % STORIES) as usize],
            &BTreeMap::new(),
        )
        .expect("part");
        if i % 10 == 0 {
            g.create_rel(
                a,
                "MENTIONS",
                entities[(i / 10 % 3) as usize],
                &BTreeMap::new(),
            )
            .expect("mention");
        }
        for k in 0..2 {
            g.create_rel(
                a,
                "MENTIONS",
                entities[(3 + (i * 7 + k * 31) % 57) as usize],
                &BTreeMap::new(),
            )
            .expect("mention");
        }
    }
    let store = g.shared_store();
    drop(g);
    // One directory per TEST: the two tests run in parallel in one process.
    let dir =
        std::env::temp_dir().join(format!("engram_fanout_probe_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 64 * 1024 * 1024)
        .expect("into_paged");
    let g = Graph::new(store, Realm(1), Namespace(1));
    // The adjacency tables are admitted after a handful of probes, as the
    // mirror's are after its thousands, so a warm run's expansion reads
    // them and any prefix walk left is the probe's own.
    g.set_degree_table_after(8);
    (g, dir)
}

/// The story with the most distinct names among its articles' mentions,
/// by hand: article i (i % 10 == 0) mentions name (i / 10 % 3) and belongs
/// to story i % STORIES.
fn expected() -> (String, i64) {
    let mut per_story: BTreeMap<i64, std::collections::BTreeSet<i64>> = BTreeMap::new();
    for i in (0..ARTICLES).step_by(10) {
        per_story.entry(i % STORIES).or_default().insert(i / 10 % 3);
    }
    let best = per_story
        .iter()
        .map(|(s, names)| (names.len() as i64, -*s))
        .max()
        .expect("a story");
    (format!("story-{:04}", -best.1), best.0)
}

const OVERLAP: &str = "UNWIND $entities AS entName MATCH (e:Entity {name: entName})<-[:MENTIONS]-(a:NewsArticle)-[:PART_OF_STORY]->(s:NewsStory) WITH s, count(DISTINCT entName) AS overlap RETURN s.storyId AS storyId, overlap ORDER BY overlap DESC, storyId ASC LIMIT 1";

/// Warm (the story columns cached by the first run), the overlap walks no
/// start's prefix: the probe is skipped for every article, and the store
/// is scanned a handful of times, not once per article.
#[test]
fn a_a_cached_hop_end_skips_the_fanout_probe_for_every_start() {
    let (g, dir) = corpus("a");
    let (want_id, want_n) = expected();
    let want = vec![vec![Value::Str(want_id), Value::Int(want_n)]];
    let (got, _) = traced(&g, OVERLAP);
    assert_eq!(got, want);
    let (got, c) = traced(&g, OVERLAP);
    assert_eq!(got, want);
    let starts = (ARTICLES / 10) as u64; // the articles reached through the three names
    assert_eq!(count_of(&c, SKIPPED), starts, "{c:?}");
    // Before the fix every start walked its prefix here: `starts` scans.
    assert!(count_of(&c, SCANS) < starts / 4, "{c:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// CONTROL (fix 83's case): a hop end whose columns are cold and whose
/// start fans out widely is still warmed, and the fan-out comes from the
/// adjacency table once one is served.
#[test]
fn b_a_wide_fanout_onto_cold_columns_still_warms_them() {
    let (g, dir) = corpus("b");
    // One entity mentioned by every article: a single start with a fan-out
    // of the whole label, onto `title`, which nothing has cached.
    let wide = {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str("Everyone".into()));
        g.create_node(&["Entity".into()], &m).expect("entity")
    };
    let ids = rows(&g, "MATCH (a:NewsArticle) RETURN id(a) AS id");
    for r in &ids {
        if let Value::Int(id) = r[0] {
            g.create_rel(id as u64, "MENTIONS", wide, &BTreeMap::new())
                .expect("mention");
        }
    }
    // A streaming top-k over the hop: `a` is demanded with `articleId`,
    // which nothing has cached, from one start fanning out to the whole
    // label — fix 83's case, still warmed.
    let src = "MATCH (e:Entity {name: 'Everyone'})<-[:MENTIONS]-(a:NewsArticle) WITH a ORDER BY a.articleId LIMIT 3 RETURN a.articleId AS id";
    let (got, c) = traced(&g, src);
    assert_eq!(
        got,
        (0..3)
            .map(|i| vec![Value::Str(format!("{i:032x}"))])
            .collect::<Vec<_>>()
    );
    assert_eq!(count_of(&c, WARMED), 1, "{c:?}");
    let _ = std::fs::remove_dir_all(dir);
}
