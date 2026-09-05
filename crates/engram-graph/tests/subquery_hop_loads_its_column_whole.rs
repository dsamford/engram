#![allow(non_snake_case)]
//! Fix 79: a `COUNT {}` / `EXISTS {}` over one typed hop whose WHERE reads a
//! far-end column that is NOT yet cached loads that column whole (kept by
//! the property-column cache) and evaluates column-at-a-time, instead of
//! handing the body to the matcher — which read a projected record per
//! neighbour, per body, per outer row.
//!
//! The production KMProject dashboard runs nine such bodies per project
//! over `KMWorkItem.status`, a column nothing had ever read as a column:
//! 14k projected record reads per statement, 156 ms against Neo4j's 21.5
//! on the mirror after fix 70 gave the vectorised path to CACHED columns.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn control(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const LOADED: &str = "interp.subquery hop loaded its far end's column whole";
const VECTORISED: &str = "interp.subquery hop evaluated column-at-a-time";
const PROJECTED: &str = "graph.projected node materialisations";
const NODE_FULL: &str = "graph.nodes materialised in full";

/// 40 projects × 60 work items, statuses spread over the seven the
/// dashboard counts; `status` is absent on six items of every project (the
/// `coalesce(status, 'backlog')` default matters).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX kmp_id FOR (n:KMProject) ON (n.id)");
    const STATUSES: [&str; 7] = ["backlog", "todo", "in_progress", "in_review", "blocked", "done", "cancelled"];
    let mut projects = Vec::new();
    for pi in 0..40i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("proj-{pi}")));
        m.insert("name".into(), s(&format!("Project {pi}")));
        projects.push(g.create_node(&["KMProject".into()], &m).expect("project"));
    }
    for i in 0..2_400i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("item-{i}")));
        m.insert("title".into(), s(&format!("Work item {i}")));
        // Item k of a project is i / 40: its status cycles over the seven
        // and is ABSENT for every tenth k (0, 10, 20, ...): six of sixty.
        let k = i / 40;
        if k % 10 != 0 {
            m.insert("status".into(), s(STATUSES[(k % 7) as usize]));
        }
        m.insert("priority".into(), Value::Int(i % 4));
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO_PROJECT", projects[(i % 40) as usize], &BTreeMap::new()).expect("rel");
    }
    g
}

const DASH: &str = "MATCH (p:KMProject) RETURN p.id AS id, \
    COUNT { (a:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE coalesce(a.status, 'backlog') = 'backlog' } AS backlog, \
    COUNT { (b:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE coalesce(b.status, 'backlog') = 'todo' } AS todo, \
    COUNT { (c:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE coalesce(c.status, 'backlog') = 'done' } AS done, \
    COUNT { (d:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE NOT d.status IN ['done', 'cancelled'] } AS open \
    ORDER BY id";

/// The first dashboard loads `status` whole ONCE (not per body or per
/// project) and every body evaluates column-at-a-time; the second loads
/// nothing. No neighbour is read as a record on either run.
#[test]
fn a_the_dashboard_loads_the_status_column_once_and_vectorises_every_body() {
    let g = corpus();
    let want = control(&g, DASH);
    assert_eq!(want.len(), 40);
    let (first, c1) = traced(&g, DASH);
    assert_eq!(first, want);
    assert_eq!(count_of(&c1, LOADED), 1, "{c1:?}");
    assert_eq!(count_of(&c1, VECTORISED), 4 * 40, "four bodies per project: {c1:?}");
    // The forty projected reads are the outer seeds (`p.id`), one per
    // project — no neighbour is read by any body.
    assert!(count_of(&c1, PROJECTED) <= 40, "no neighbour read per body: {c1:?}");
    let (second, c2) = traced(&g, DASH);
    assert_eq!(second, want);
    assert_eq!(count_of(&c2, LOADED), 0, "{c2:?}");
    assert_eq!(count_of(&c2, VECTORISED), 4 * 40, "{c2:?}");
    assert!(count_of(&c2, PROJECTED) <= 40, "{c2:?}");
    // The numbers themselves: 60 items per project, six of them without a
    // status (counted as backlog), the rest spread over seven statuses.
    let row0 = &first[0];
    assert!(matches!(row0[1], Value::Int(n) if n > 6), "backlog counts the absent statuses too: {row0:?}");
    assert!(matches!(row0[4], Value::Int(n) if n > 30), "{row0:?}");
}

/// `EXISTS {}` with the same body shape takes the same load; a presence
/// read (`IS NOT NULL`) rides it too.
#[test]
fn b_exists_and_presence_reads_load_the_column_the_same_way() {
    let g = corpus();
    let src = "MATCH (p:KMProject) RETURN p.id AS id, \
        EXISTS { (a:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE a.status = 'blocked' } AS hasBlocked, \
        COUNT { (b:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE b.status IS NOT NULL } AS withStatus \
        ORDER BY id";
    let want = control(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, LOADED), 1, "{c:?}");
    assert_eq!(count_of(&c, VECTORISED), 2 * 40, "{c:?}");
    assert!(count_of(&c, PROJECTED) + count_of(&c, NODE_FULL) <= 40, "the outer seeds only: {c:?}");
    assert!(got.iter().all(|r| r[1] == Value::Bool(true)), "every project has a blocked item");
    assert!(got.iter().all(|r| matches!(r[2], Value::Int(54))), "54 of 60 carry a status: {:?}", &got[0]);
}
