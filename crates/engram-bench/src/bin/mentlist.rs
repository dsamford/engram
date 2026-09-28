#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does the MENTIONS top-30 aggregate spend its time?
//!
//! On the production mirror `MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->
//! (e) RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, count(*)
//! AS cnt ORDER BY cnt DESC LIMIT toInteger(30)` runs 250–280 ms against
//! Neo4j's ~210 (v161/v162): 37,273 point gets over the distinct ends (fix
//! 93's covering-label discovery read its three samples and declined), two
//! column scans declined, 135 expressions. This bin times the shape's stages
//! on a paged store whose ids interleave the ends with fillers, with an
//! optional handful of ends OUTSIDE the covering label.
//!
//! ```text
//! mentlist [seeds=38000] [ends=37000] [outliers=0] [fillers_per_end=8] [iters=5]
//! ```
use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const USER: &str = "ae019347-933d-472b-9d3a-5d2408e8a06b";

fn s(k: &str, v: String) -> (String, Value) {
    (k.to_string(), Value::Str(v))
}

fn seed(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let node_type = match i % 20 {
        0..=13 => "email",
        14..=16 => "commit",
        17 => "repository",
        _ => "chat",
    };
    for (k, v) in [
        s("userId", USER.into()),
        s("nodeType", node_type.into()),
        s("nodeId", format!("node-{i:08x}")),
        s(
            "subject",
            format!("Subject {i}: {}", "a line of text ".repeat(4)),
        ),
        s(
            "createdAt",
            format!("2026-0{}-{:02}T10:00:00.000Z", 1 + (i % 8), 1 + (i % 28)),
        ),
    ] {
        m.insert(k, v);
    }
    if i % 9 == 0 {
        m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
    }
    m.insert("classified".to_string(), Value::Bool(i % 3 != 0));
    m
}

fn entity(e: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(format!("Entity {e}")));
    if e % 7 != 0 {
        m.insert(
            "type".to_string(),
            Value::Str(["person", "organization", "place", "topic"][(e % 4) as usize].into()),
        );
    }
    m.insert("entityId".to_string(), Value::Str(format!("ent-{e:08x}")));
    m.insert("mentions".to_string(), Value::Int(e % 50));
    m
}

struct Built {
    graph: Graph,
    dir: std::path::PathBuf,
    edges: usize,
}

fn corpus(seeds: i64, ends: i64, outliers: i64, fillers_per_end: i64) -> Built {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for src in [
        "CREATE INDEX ud_user FOR (n:UserDataNode) ON (n.userId)",
        "CREATE INDEX ud_user_type FOR (n:UserDataNode) ON (n.userId, n.nodeType)",
    ] {
        let ddl = parse_any(src).expect("parse index");
        run_stmt(&g, &ddl, BTreeMap::new()).expect("index");
    }
    // The ends, interleaved with fillers so their ids are sparse in the span.
    let mut end_ids: Vec<u64> = Vec::with_capacity(ends as usize);
    for e in 0..ends {
        end_ids.push(
            g.create_node(&["Entity".into()], &entity(e))
                .expect("entity"),
        );
        for f in 0..fillers_per_end {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(e * fillers_per_end + f));
            g.create_node(&["Filler".into()], &m).expect("filler");
        }
    }
    // Ends outside the covering label: a `Person` the discovery cannot cover.
    let mut outlier_ids: Vec<u64> = Vec::new();
    for o in 0..outliers {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Person {o}")));
        m.insert("type".to_string(), Value::Str("person".into()));
        outlier_ids.push(g.create_node(&["Person".into()], &m).expect("person"));
    }
    let mut other = BTreeMap::new();
    other.insert("userId".to_string(), Value::Str("other".into()));
    other.insert("nodeType".to_string(), Value::Str("email".into()));
    let mut edges = 0usize;
    for i in 0..seeds {
        let n = g
            .create_node(&["UserDataNode".into()], &seed(i))
            .expect("seed");
        let k = 1 + (i % 4);
        for j in 0..k {
            // A fifth of the edges land on the first hundred ends: a top-30
            // that means something.
            let target = if (i + j) % 5 == 0 {
                end_ids[((i * 31 + j) % 100) as usize]
            } else {
                end_ids[(((i * 7919) ^ (j * 104_729)) % ends) as usize]
            };
            g.create_rel(n, "MENTIONS", target, &BTreeMap::new())
                .expect("mentions");
            edges += 1;
        }
        if !outlier_ids.is_empty() && i % 997 == 0 {
            let t = outlier_ids[(i / 997) as usize % outlier_ids.len()];
            g.create_rel(n, "MENTIONS", t, &BTreeMap::new())
                .expect("mentions outlier");
            edges += 1;
        }
        if i % 19 == 0 {
            let o = g
                .create_node(&["UserDataNode".into()], &other)
                .expect("other seed");
            g.create_rel(
                o,
                "MENTIONS",
                end_ids[(i % ends) as usize],
                &BTreeMap::new(),
            )
            .expect("other mentions");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_mentlist_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 256 * 1024 * 1024)
        .expect("into_paged");
    Built {
        graph: Graph::new(store, Realm(1), Namespace(1)),
        dir,
        edges,
    }
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
        "{label:<16} {per_round:9.3} ms per round   {:8.2} µs per edge   rows {rows}",
        per_round * 1e3 / per as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: i64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let seeds = arg(1, 38_000);
    let ends = arg(2, 37_000);
    let outliers = arg(3, 0);
    let fillers = arg(4, 8);
    let iters = arg(5, 5) as usize;
    let t0 = Instant::now();
    let Built {
        graph: g,
        dir,
        edges,
    } = corpus(seeds, ends, outliers, fillers);
    println!(
        "seeds {seeds}, ends {ends}, outliers {outliers}, fillers/end {fillers}, edges {edges}, iters {iters}, built in {:.1} s, paged store {}",
        t0.elapsed().as_secs_f64(),
        dir.display()
    );
    let mut params = BTreeMap::new();
    params.insert("u".to_string(), Value::Str(USER.into()));
    let run = |src: &str| -> usize {
        let q = parse_statement(src).expect("parse");
        run_query(&g, &q, params.clone()).expect("run").rows.len()
    };
    let head = "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e)";
    let tail = "count(*) AS cnt ORDER BY cnt DESC LIMIT toInteger(30)";
    let cases: &[(&str, String)] = &[
        (
            "seed-count",
            "MATCH (n:UserDataNode {userId: $u}) RETURN count(n) AS n".to_string(),
        ),
        ("hop-count", format!("{head} RETURN count(*) AS n")),
        (
            "distinct-ends",
            format!("{head} RETURN count(DISTINCT e) AS n"),
        ),
        ("count-e", format!("{head} RETURN count(e) AS n")),
        (
            "group-node",
            format!("{head} RETURN e AS e, count(*) AS cnt ORDER BY cnt DESC LIMIT toInteger(30)"),
        ),
        (
            "one-key-all",
            format!("{head} RETURN e.name AS name, count(*) AS cnt"),
        ),
        ("one-key", format!("{head} RETURN e.name AS name, {tail}")),
        (
            "plain-keys",
            format!("{head} RETURN e.name AS name, e.type AS type, {tail}"),
        ),
        (
            "orig",
            format!("{head} RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, {tail}"),
        ),
        (
            "labelled-end",
            format!(
                "MATCH (n:UserDataNode {{userId: $u}})-[:MENTIONS]->(e:Entity) RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, {tail}"
            ),
        ),
        (
            "email-seeds",
            format!(
                "MATCH (n:UserDataNode {{userId: $u, nodeType: 'email'}})-[:MENTIONS]->(e) RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, {tail}"
            ),
        ),
    ];
    for (label, src) in cases {
        time(label, edges, iters, || run(src));
    }
    let traced: Vec<usize> = std::env::var("MENTLIST_TRACE")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![5, 6, 7]);
    for which in traced {
        let (_, trace) = engram_observe::with_trace(|| run(&cases[which].1));
        let mut counters: Vec<(&String, &u64)> = trace.counters().iter().collect();
        counters.sort_by(|a, b| b.1.cmp(a.1));
        let top: usize = std::env::var("MENTLIST_TOP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(18);
        for (k, v) in counters.iter().take(top) {
            println!("   traced {}:  {v:>8}  {k}", cases[which].0);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
