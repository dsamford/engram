#![allow(non_snake_case)]
//! Fix 57: a WITH breaker that carries the scanned node ITSELF — `MATCH (n:L)
//! WHERE … WITH n ORDER BY n.x DESC SKIP … LIMIT …` — used to send the whole
//! stage to the general path, which built a row per member: the production
//! inbox listing paged 1,000 of ~38k emails through 125k expression
//! evaluations and an 11k-deep top-k (294 ms against Neo4j's 113). The
//! columnar stage now admits the bare carry: the node is a placeholder
//! column, the member id rides as a trailing column through the ordering
//! and paging, and only the survivors are materialised — then the later
//! clauses read the node as they always did.
//!
//! Every answer is checked against the generating rules and against the
//! same statement with the columnar paths off.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const STAGES: &str = "interp.columnar stages";
const HYDRATED: &str = "interp.columnar stage hydrated a bare node for a survivor";
const FULL: &str = "graph.nodes materialised in full";

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("offset".to_string(), Value::Int(5));
    p.insert("limit".to_string(), Value::Int(10));
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

const N: i64 = 3_000;
fn open(i: i64) -> bool {
    i % 5 != 0
}
fn updated(i: i64) -> i64 {
    (i * 7919) % N // a permutation: the ORDER BY is not the id order
}
fn project(i: i64) -> i64 {
    i % 3
}
/// Every third item carries an Ask; every other Ask is resolved.
fn ask(i: i64) -> Option<bool> {
    (i % 3 == 0).then_some(i % 6 == 0)
}

/// 3,000 fat items (a 2 KB body each) over three projects, a third of them
/// with an Ask.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut projects = Vec::new();
    for p in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Int(p));
        projects.push(g.create_node(&["Proj".into()], &m).expect("proj"));
    }
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        m.insert("updated".to_string(), Value::Int(updated(i)));
        m.insert(
            "status".to_string(),
            Value::Str(if open(i) { "open" } else { "done" }.to_string()),
        );
        m.insert("body".to_string(), Value::Str("b".repeat(2_000)));
        let w = g.create_node(&["Item".into()], &m).expect("item");
        g.create_rel(w, "IN", projects[project(i) as usize], &BTreeMap::new())
            .expect("in");
        if let Some(resolved) = ask(i) {
            let mut a = BTreeMap::new();
            a.insert("resolved".to_string(), Value::Bool(resolved));
            let ask = g.create_node(&["Ask".into()], &a).expect("ask");
            g.create_rel(w, "HAS", ask, &BTreeMap::new()).expect("has");
        }
    }
    g
}

/// The page the rules give: open items by `updated` DESC, then the window.
fn page(skip: usize, limit: usize) -> Vec<i64> {
    let mut all: Vec<i64> = (0..N).filter(|&i| open(i)).collect();
    all.sort_by_key(|&i| std::cmp::Reverse(updated(i)));
    all.into_iter().skip(skip).take(limit).collect()
}

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

const PAGED_HOP: &str = "MATCH (n:Item) WHERE n.status = 'open' WITH n ORDER BY n.updated DESC SKIP toInteger($offset) LIMIT toInteger($limit) OPTIONAL MATCH (n)-[:IN]->(p:Proj) RETURN n.n AS n, p.pid AS pid";

/// The production inbox shape: a paged bare carry, an OPTIONAL hop, an
/// aggregating WITH over the carry and a RETURN reading it by property.
const INBOX: &str = "MATCH (n:Item {status: 'open'}) WITH n ORDER BY n.updated DESC SKIP toInteger($offset) LIMIT toInteger($limit) OPTIONAL MATCH (n)-[:HAS]->(a:Ask) WITH n, count(CASE WHEN a IS NOT NULL AND coalesce(a.resolved, false) = false THEN a END) AS openAsks RETURN n.n AS item, openAsks ORDER BY n.updated DESC";

#[test]
fn a_paged_bare_carry_runs_on_the_stage_and_hydrates_its_page() {
    let g = corpus();
    let want = page(5, 10);
    let (got, c) = traced(&g, PAGED_HOP);
    assert_eq!(ints(&got), want);
    for (r, &i) in got.iter().zip(&want) {
        assert_eq!(r[1], Value::Int(project(i)));
    }
    assert!(count_of(&c, STAGES) >= 1, "the stage claimed it: {c:?}");
    assert_eq!(count_of(&c, HYDRATED), 10, "one hydration per survivor: {c:?}");
    assert!(count_of(&c, FULL) <= 10 + 4, "{c:?}");
    assert_eq!(ints(&general(&g, PAGED_HOP)), want, "general path");
}

#[test]
fn the_inbox_shape_answers_the_rules_through_the_stage() {
    let g = corpus();
    let want = page(5, 10);
    let (got, c) = traced(&g, INBOX);
    assert_eq!(ints(&got), want);
    for (r, &i) in got.iter().zip(&want) {
        let open_asks = match ask(i) {
            Some(false) => 1,
            _ => 0,
        };
        assert_eq!(r[1], Value::Int(open_asks), "item {i}");
    }
    assert!(count_of(&c, STAGES) >= 1, "{c:?}");
    assert_eq!(count_of(&c, HYDRATED), 10, "{c:?}");
    assert_eq!(got, general(&g, INBOX), "general path");
}

/// CONTROLS: a post-WHERE over the carry, an aggregate beside it and a
/// carry an earlier WITH dropped each stay on the general path — same rows.
#[test]
fn a_post_where_an_aggregate_and_a_dropped_carry_stay_as_written() {
    let g = corpus();
    for src in [
        "MATCH (n:Item) WHERE n.status = 'open' WITH n ORDER BY n.updated DESC LIMIT 10 WHERE n.n > 100 RETURN n.n AS n",
        "MATCH (n:Item) WHERE n.status = 'open' WITH n, count(*) AS c ORDER BY n.updated DESC LIMIT 3 RETURN n.n AS n, c",
        "MATCH (n:Item) WHERE n.status = 'open' WITH n.n AS k WITH k ORDER BY k DESC LIMIT 3 RETURN k",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, HYDRATED), 0, "`{src}` hydrates nothing: {c:?}");
    }
}
