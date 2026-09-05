#![allow(non_snake_case, clippy::disallowed_methods, clippy::disallowed_types)]
//! THROWAWAY probe: the dashboard base's cost split, locally.
use std::collections::BTreeMap;
use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;
fn s(v: &str) -> Value { Value::Str(v.into()) }
fn params() -> BTreeMap<String, Value> { let mut p = BTreeMap::new(); p.insert("userId".into(), s("u-3")); p }
fn timed(g: &Graph, name: &str, src: &str) {
    let q = parse_statement(src).unwrap();
    let _ = run_query(g, &q, params()).unwrap();
    let n = 10;
    let t = std::time::Instant::now();
    let mut rows = 0;
    for _ in 0..n { rows = run_query(g, &q, params()).unwrap().rows.len(); }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    let (_, trace) = engram_observe::with_trace(|| run_query(g, &q, params()).unwrap());
    let c = trace.counters();
    let k = |key: &str| c.get(key).copied().unwrap_or(0);
    eprintln!("{name:<28} {ms:8.2} ms rows {rows} | full {} proj {} cols {} demand {} expr {} gathered-key {}",
        k("graph.nodes materialised in full"), k("graph.projected node materialisations"),
        k("interp.matcher bound a hop end from the label's cached columns"), k("interp.matcher bound a hop end to its demand"),
        k("cypher.expressions evaluated"), k("interp.agg bare group key gathered for its later reads"));
}
#[test]
fn probe() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut users = Vec::new();
    for ui in 0..10i64 {
        let mut m = BTreeMap::new(); m.insert("userId".into(), s(&format!("u-{ui}")));
        users.push(g.create_node(&["User".into()], &m).unwrap());
    }
    let mut projects = Vec::new();
    for pi in 0..77i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("proj-{pi}")));
        m.insert("name".into(), s(&format!("Project {pi}")));
        m.insert("metadata".into(), s(&"{\"k\": \"v\", \"desc\": \"lorem ipsum dolor sit amet\"}".repeat(60)));
        for k in 0..20 { m.insert(format!("f{k}"), s(&format!("value {k}"))); }
        let p = g.create_node(&["KMProject".into()], &m).unwrap();
        let mut rm = BTreeMap::new(); rm.insert("role".into(), s(if pi % 2 == 0 { "owner" } else { "member" }));
        g.create_rel(users[(pi % 10) as usize], "MEMBER_OF", p, &rm).unwrap();
        projects.push(p);
    }
    for i in 0..1_338i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("wi-{i}")));
        m.insert("updatedAt".into(), s(&format!("2026-08-{:02}T{:02}:00:00Z", 1 + i % 28, i % 24)));
        m.insert("status".into(), s(["open", "done"][(i % 2) as usize]));
        m.insert("content".into(), s(&"body ".repeat(200)));
        let w = g.create_node(&["KMWorkItem".into()], &m).unwrap();
        g.create_rel(w, "BELONGS_TO_PROJECT", projects[(i % 77) as usize], &BTreeMap::new()).unwrap();
    }
    let mid = "MATCH (p:KMProject) WHERE true OPTIONAL MATCH (:User {userId: $userId})-[mm:MEMBER_OF]->(p) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, mm, max(w.updatedAt) AS lastItemAt RETURN ";
    timed(&g, "base properties(p)", &format!("{mid}properties(p) AS p, coalesce(mm.role, 'owner') AS myRole, lastItemAt ORDER BY lastItemAt DESC"));
    timed(&g, "base p (bare)", &format!("{mid}p, coalesce(mm.role, 'owner') AS myRole, lastItemAt ORDER BY lastItemAt DESC"));
    timed(&g, "base p.id", &format!("{mid}p.id AS id, coalesce(mm.role, 'owner') AS myRole, lastItemAt ORDER BY lastItemAt DESC"));
    timed(&g, "no member hop, props", "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, max(w.updatedAt) AS lastItemAt RETURN properties(p) AS p, lastItemAt ORDER BY lastItemAt DESC");
    timed(&g, "no member hop, p.id", "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, max(w.updatedAt) AS lastItemAt RETURN p.id AS id, lastItemAt ORDER BY lastItemAt DESC");
}
