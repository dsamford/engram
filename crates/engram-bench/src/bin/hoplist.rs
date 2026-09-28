#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does a WHOLE-NODE listing reached through a hop spend its time?
//!
//! On the production mirror `MATCH (u:User {userId: $u})-[:OWNS_STUDIO_PROJECT]->
//! (p:StudioProject) OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack)
//! RETURN p, count(t) AS trackCount ORDER BY p.updatedAt DESC` returns 1,144
//! projects in 25.8 ms against Neo4j's 14.4 (v161), 17.4 of it in the engine:
//! per project one store get, one full decode, one folded chain count, five
//! expression evaluations, a group and an order key. This bin times the
//! shape's stages on a paged store so the lever is chosen from a number:
//!
//! | stage         | statement |
//! |---|---|
//! | `hop-count`   | `MATCH (u)-[:OWNS]->(p) RETURN count(p)` — the hop alone |
//! | `hop-lean`    | `… RETURN p.id` — the hop plus one column per end |
//! | `hop-full`    | `… RETURN p` — the hop plus a full decode per end |
//! | `hop-full-ord`| `… RETURN p ORDER BY p.updatedAt DESC` |
//! | `chain-count` | `… OPTIONAL MATCH (p)-[:HAS]->(t) RETURN p.id, count(t)` |
//! | `orig`        | the production shape |
//! | `orig-props`  | the same with `properties(p)` |
//!
//! ```text
//! hoplist [projects=1144] [iters=30]
//! ```
use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn project(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let s = |k: &str, v: String| (k.to_string(), Value::Str(v));
    for (k, v) in [
        s("id", format!("proj_{i:08x}-b338-4020-ba04-31e41079{i:04x}")),
        s("name", format!("Project {i}")),
        s(
            "description",
            format!(
                "Studio project {i}: {}",
                "a line of description ".repeat(3)
            ),
        ),
        s(
            "status",
            if i % 5 == 0 {
                "archived".into()
            } else {
                "active".into()
            },
        ),
        s("ownerId", "84e73028-8f48-4d64-806b-2a320c8ed85c".into()),
        s(
            "createdAt",
            format!("2026-0{}-{:02}T10:00:00.000Z", 1 + (i % 8), 1 + (i % 28)),
        ),
        s(
            "updatedAt",
            format!(
                "2026-09-{:02}T{:02}:{:02}:00.000Z",
                1 + (i % 28),
                (i / 28) % 24,
                i % 60
            ),
        ),
        s(
            "genre",
            ["ambient", "techno", "house", "jazz"][(i % 4) as usize].into(),
        ),
        s(
            "key",
            ["C", "D", "E", "F", "G", "A", "B"][(i % 7) as usize].into(),
        ),
        s(
            "coverUrl",
            format!("https://cdn.example.net/covers/{i:08x}.png"),
        ),
    ] {
        m.insert(k, v);
    }
    m.insert("bpm".to_string(), Value::Int(90 + (i % 60)));
    m.insert("bars".to_string(), Value::Int(16 * (1 + i % 8)));
    m.insert("public".to_string(), Value::Bool(i % 3 == 0));
    m.insert("version".to_string(), Value::Int(1 + i % 4));
    m
}

fn corpus(projects: i64) -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert(
        "userId".to_string(),
        Value::Str("84e73028-8f48-4d64-806b-2a320c8ed85c".into()),
    );
    let u = g.create_node(&["User".into()], &m).expect("user");
    let mut other = BTreeMap::new();
    other.insert("userId".to_string(), Value::Str("other".into()));
    let o = g.create_node(&["User".into()], &other).expect("other");
    for i in 0..projects {
        let p = g
            .create_node(&["StudioProject".into()], &project(i))
            .expect("project");
        g.create_rel(u, "OWNS_STUDIO_PROJECT", p, &BTreeMap::new())
            .expect("owns");
        for k in 0..(i % 4) {
            let mut t = BTreeMap::new();
            t.insert("name".to_string(), Value::Str(format!("Track {k}")));
            t.insert("order".to_string(), Value::Int(k));
            let tr = g.create_node(&["StudioTrack".into()], &t).expect("track");
            g.create_rel(p, "CONTAINS_TRACK", tr, &BTreeMap::new())
                .expect("contains");
        }
        // Fillers so the projects are not contiguous in the id space.
        for f in 0..3 {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i * 3 + f));
            let n = g.create_node(&["Filler".into()], &m).expect("filler");
            if f == 0 {
                g.create_rel(o, "OWNS_STUDIO_PROJECT", n, &BTreeMap::new())
                    .expect("owns");
            }
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_hoplist_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 256 * 1024 * 1024)
        .expect("into_paged");
    (Graph::new(store, Realm(1), Namespace(1)), dir)
}

fn time(label: &str, per: usize, iters: usize, mut f: impl FnMut() -> usize) {
    let rows = f();
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let el = t0.elapsed();
    let per_round = el.as_secs_f64() * 1e3 / iters as f64;
    println!(
        "{label:<14} {per_round:8.3} ms per round   {:8.2} µs per project   rows {rows}",
        per_round * 1e3 / per as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let projects: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1144);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let (g, dir) = corpus(projects);
    let mut params = BTreeMap::new();
    params.insert(
        "u".to_string(),
        Value::Str("84e73028-8f48-4d64-806b-2a320c8ed85c".into()),
    );
    println!(
        "projects {projects}, iters {iters}, paged store {}",
        dir.display()
    );
    let run = |src: &str| -> usize {
        let q = parse_statement(src).expect("parse");
        run_query(&g, &q, params.clone()).expect("run").rows.len()
    };
    let head = "MATCH (u:User {userId: $u})-[:OWNS_STUDIO_PROJECT]->(p:StudioProject)";
    let n = projects as usize;
    let cases: &[(&str, String)] = &[
        ("hop-count", format!("{head} RETURN count(p) AS n")),
        ("hop-lean", format!("{head} RETURN p.id AS id")),
        ("hop-full", format!("{head} RETURN p")),
        (
            "hop-full-ord",
            format!("{head} RETURN p ORDER BY p.updatedAt DESC"),
        ),
        (
            "chain-count",
            format!(
                "{head} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN p.id AS id, count(t) AS trackCount"
            ),
        ),
        (
            "orig",
            format!(
                "{head} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN p, count(t) AS trackCount ORDER BY p.updatedAt DESC"
            ),
        ),
        (
            "orig-props",
            format!(
                "{head} OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:StudioTrack) RETURN properties(p) AS p, count(t) AS trackCount ORDER BY p.updatedAt DESC"
            ),
        ),
    ];
    for (label, src) in cases {
        time(label, n, iters, || run(src));
    }
    // One traced round of the production shape: what runs per project.
    let (_, trace) = engram_observe::with_trace(|| run(&cases[5].1));
    let mut counters: Vec<(&String, &u64)> = trace.counters().iter().collect();
    counters.sort_by(|a, b| b.1.cmp(a.1));
    for (k, v) in counters.iter().take(16) {
        println!("   traced orig:  {v:>8}  {k}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
