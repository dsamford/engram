#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does a filtered LIMIT projection over a cached label spend its
//! time? (fix 40 attribution — local only, not checked in.)
//!
//! On the production mirror `MATCH (s:NewsStory) WHERE s.primaryTopic = $t
//! AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN s.storyId
//! LIMIT 5` answers in 90 ms against Neo4j's 2.6 with every column served
//! from the property-column cache — the cost is the per-member scope bind
//! and `eval_with` over 20k members. The counts over the same columns run
//! column-at-a-time: 15 ms a conjunct, which is ALSO slow for a vector
//! compare over 20k values.
//!
//! ```text
//! projscan [stories=21000] [iters=20]
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn story(i: i64, n: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let s = |k: &str, v: String| (k.to_string(), Value::Str(v));
    // 98% carry the topic; 10% are stale; the RECENT stories (past the
    // cutoff) are the last 8% of the label in id order — where the mirror
    // has them (the loader appended in creation order).
    let recent = i >= n - n / 12;
    for (k, v) in [
        s("storyId", format!("{i:08x}-e45b-4d74-be66-f213{i:012x}")),
        s(
            "primaryTopic",
            if i % 50 == 0 { "Sports".into() } else { "Business and Finance".into() },
        ),
        s("status", if i % 10 == 3 { "stale".into() } else { "active".into() }),
        s(
            "lastUpdatedAt",
            if recent {
                format!("2026-09-0{}T{:02}:{:02}:00.000Z", 1 + (i % 4), i % 24, i % 60)
            } else {
                format!("2026-0{}-{:02}T{:02}:{:02}:00.000Z", 1 + (i % 8), 1 + (i % 28), i % 24, i % 60)
            },
        ),
        s("publishedAt", format!("2026-0{}-{:02}T00:00:00Z", 1 + (i % 8), 1 + (i % 28))),
        s("title", format!("Story {i}: {}", "lorem ipsum ".repeat(6))),
        s("summary", "s".repeat(600)),
    ] {
        m.insert(k, v);
    }
    m.insert("score".to_string(), Value::Float((i % 100) as f64 / 100.0));
    m
}

fn corpus(n: i64) -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX story_id FOR (s:NewsStory) ON (s.storyId)");
    ddl(&g, "CREATE INDEX story_pub FOR (s:NewsStory) ON (s.publishedAt)");
    for i in 0..n {
        g.create_node(&["NewsStory".into()], &story(i, n)).expect("story");
        if i % 3 == 0 {
            let mut f = BTreeMap::new();
            f.insert("eventId".to_string(), Value::Str(format!("evt-{i}")));
            g.create_node(&["GeopoliticalEvent".into()], &f).expect("event");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_projscan_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 256 * 1024 * 1024).expect("into_paged");
    (Graph::new(store.clone(), Realm(1), Namespace(1)), dir)
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("primaryTopic".to_string(), Value::Str("Business and Finance".into()));
    p.insert("cutoff".to_string(), Value::Str("2026-08-31T03:42:27.116Z".into()));
    p.insert(
        "existingIds".to_string(),
        Value::List((0..40).map(|i| Value::Str(format!("{i:08x}-e45b-4d74-be66-f213{i:012x}"))).collect()),
    );
    p
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(21000);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let (g, dir) = corpus(n);
    println!("stories {n}, iters {iters}, paged store {}", dir.display());
    let p = params();
    const PRED: &str = "s.primaryTopic = $primaryTopic AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff";
    let shapes: Vec<(&str, String)> = vec![
        ("topic count", "MATCH (s:NewsStory) WHERE s.primaryTopic = $primaryTopic RETURN count(s) AS n".into()),
        ("cutoff count", "MATCH (s:NewsStory) WHERE s.lastUpdatedAt > $cutoff RETURN count(s) AS n".into()),
        ("3-conjunct count", format!("MATCH (s:NewsStory) WHERE {PRED} RETURN count(s) AS n")),
        ("bare LIMIT 5", format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId LIMIT 5")),
        ("ORDER BY DESC LIMIT 5", format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId ORDER BY s.lastUpdatedAt DESC LIMIT 5")),
        ("NOT IN + ORDER BY LIMIT 5", format!("MATCH (s:NewsStory) WHERE {PRED} AND NOT s.storyId IN $existingIds RETURN s.storyId AS storyId, s.title AS title ORDER BY s.lastUpdatedAt DESC LIMIT 5")),
        ("no-pred LIMIT 5", "MATCH (s:NewsStory) RETURN s.storyId AS storyId LIMIT 5".into()),
        ("all matching (no limit)", format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId")),
    ];
    for (label, src) in &shapes {
        let q = parse_statement(src).expect("parse");
        // Warm: the first run builds the columns and the cache.
        let rows = run_query(&g, &q, p.clone()).expect("run").rows;
        let t0 = Instant::now();
        for _ in 0..iters {
            let _ = run_query(&g, &q, p.clone()).expect("run");
        }
        let per = t0.elapsed().as_secs_f64() * 1e3 / iters as f64;
        let first = rows.first().map(|r| format!("{:?}", r.first())).unwrap_or_default();
        println!("{label:<28} {per:9.3} ms   rows {:>6}   first {}", rows.len(), &first[..first.len().min(60)]);
        let (_, trace) = engram_observe::with_trace(|| {
            let _ = run_query(&g, &q, p.clone());
        });
        let mut cs: Vec<(&String, &u64)> = trace.counters().iter().collect();
        cs.sort_by(|a, b| b.1.cmp(a.1));
        for (k, v) in cs.iter().take(12) {
            println!("      {v:>8}  {k}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
