#![allow(non_snake_case)]
//! Fix 68: the existence-probe seed (fix 49) walked its reversed path with
//! the MATERIALISING matcher and no demand — every work item of the
//! project decoded in full and every BELONGS_TO_PROJECT record fetched, to
//! keep 197 ids — and then re-tested the very EXISTS conjunct it had
//! seeded per row (an adjacency read and a projected get each). The
//! production KM listing (`MATCH (w:KMWorkItem) WHERE true AND EXISTS {
//! (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } RETURN
//! properties(w) … ORDER BY w.sortOrder SKIP … LIMIT …`) read 592 records
//! in full, 197 relationship records and 194 projected records for its
//! 197 rows on the mirror (43–78 ms against Neo4j's 23). Now the probe
//! walks lean (an empty demand: bare ends, no relationship record) and the
//! conjunct it satisfied leaves the clause WHERE.
//!
//! Every answer is checked against the same statement with hop reversal
//! OFF — the label scan with the EXISTS evaluated per row.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("projectId".to_string(), s("proj-7"));
    p.insert("limit".to_string(), Value::Int(200));
    p.insert("offset".to_string(), Value::Int(0));
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

/// The label scan with the EXISTS evaluated per row: no probe seed.
fn scanned(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_hop_reversal(false);
    let r = rows(g, src);
    g.set_hop_reversal(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const LEAN: &str = "interp.seed probe walked its path lean";
const PRUNED: &str = "interp.seed probe's conjunct pruned from the WHERE";
const PROBED: &str = "interp.seed driven from an existence probe's constant end";
const FULL: &str = "graph.nodes materialised in full";
const RELS: &str = "graph.rels materialised in full";
const PROJECTED: &str = "store.projected gets";

/// 24 projects × 197 items (4,728 KMWorkItem); every fifth item is the
/// child (HAS_TASK) of the item before it; `id` declared on KMProject.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX kmp_id FOR (n:KMProject) ON (n.id)");
    for p in 0..24i64 {
        let mut pm = BTreeMap::new();
        pm.insert("id".into(), s(&format!("proj-{p}")));
        pm.insert("name".into(), s(&format!("Project {p}")));
        let pn = g.create_node(&["KMProject".into()], &pm).expect("p");
        let mut prev: Option<u64> = None;
        for i in 0..197i64 {
            let mut m = BTreeMap::new();
            m.insert("id".into(), s(&format!("item-{p}-{i}")));
            m.insert("title".into(), s(&format!("Item {i} of project {p} with a title long enough to matter")));
            m.insert("description".into(), s(&"lorem ipsum ".repeat(20)));
            m.insert("status".into(), s(["backlog", "todo", "in_progress", "done"][(i % 4) as usize]));
            m.insert("itemType".into(), s(if i % 10 == 0 { "epic" } else { "task" }));
            m.insert("sortOrder".into(), Value::Int((197 - i) * 10));
            m.insert("updatedAt".into(), s(&format!("2026-08-{:02}T00:00:{:02}Z", 1 + i % 28, i % 60)));
            let w = g.create_node(&["KMWorkItem".into()], &m).expect("w");
            g.create_rel(w, "BELONGS_TO_PROJECT", pn, &BTreeMap::new()).expect("btp");
            if i % 5 == 4 {
                if let Some(parent) = prev {
                    g.create_rel(parent, "HAS_TASK", w, &BTreeMap::new()).expect("has_task");
                }
            }
            prev = Some(w);
        }
    }
    g
}

const PAGE: &str = "MATCH (w:KMWorkItem) WHERE true AND EXISTS { (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } \
    RETURN properties(w) AS w ORDER BY w.sortOrder ASC SKIP toInteger($offset) LIMIT toInteger($limit)";

const ORIG: &str = "MATCH (w:KMWorkItem) WHERE true AND EXISTS { (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } \
    RETURN properties(w) AS w, [(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id][0] AS projectId, \
    [(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id][0] AS parentId \
    ORDER BY w.sortOrder ASC SKIP toInteger($offset) LIMIT toInteger($limit)";

#[test]
fn a_the_page_reads_each_survivor_once_and_nothing_else() {
    let g = corpus();
    let want = scanned(&g, PAGE);
    assert_eq!(want.len(), 197);
    // The index is built on its first probe; count the second run.
    let _ = rows(&g, PAGE);
    let (got, c) = traced(&g, PAGE);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PROBED), 1, "{c:?}");
    assert_eq!(count_of(&c, LEAN), 1, "{c:?}");
    assert_eq!(count_of(&c, PRUNED), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), 197, "one hydration per survivor, no probe decode: {c:?}");
    assert_eq!(count_of(&c, RELS), 0, "the probe walks adjacency, not relationship records: {c:?}");
    // The probe's constant end (`:KMProject {id}`) is one projected read
    // per statement; the per-row EXISTS re-check (197 of them) is gone.
    assert!(count_of(&c, PROJECTED) <= 1, "no per-row EXISTS re-check: {c:?}");
}

/// The production listing with its two comprehensions: byte-identical to
/// the scan, and the probe's reads gone from it too.
#[test]
fn b_the_listing_with_comprehensions_agrees_with_the_scan() {
    let g = corpus();
    let want = scanned(&g, ORIG);
    assert_eq!(want.len(), 197);
    let _ = rows(&g, ORIG);
    let (got, c) = traced(&g, ORIG);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, LEAN), 1, "{c:?}");
    assert_eq!(count_of(&c, PRUNED), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), 197, "{c:?}");
}

/// Only the probe's conjunct leaves the WHERE: a residual beside it still
/// runs, an OPTIONAL MATCH keeps its null row, and with hop reversal off
/// (no probe seed) the counters are silent and the answer identical.
#[test]
fn c_a_residual_conjunct_stays_and_optional_keeps_its_semantics() {
    let g = corpus();
    let residual = PAGE.replace(
        "{id: $projectId}) }",
        "{id: $projectId}) } AND w.status <> 'done' AND w.itemType = 'task'",
    );
    let want = scanned(&g, &residual);
    assert!(want.len() > 100 && want.len() < 197, "{}", want.len());
    let _ = rows(&g, &residual);
    let (got, c) = traced(&g, &residual);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PRUNED), 1, "{c:?}");

    let optional = "MATCH (p:KMProject {id: 'proj-3'}) \
        OPTIONAL MATCH (w:KMWorkItem) WHERE EXISTS { (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } AND w.status = 'nobody' \
        RETURN p.id AS id, count(w) AS n";
    let want = scanned(&g, optional);
    assert_eq!(want, vec![vec![s("proj-3"), Value::Int(0)]]);
    assert_eq!(rows(&g, optional), want);

    g.set_hop_reversal(false);
    let (got, c) = traced(&g, PAGE);
    g.set_hop_reversal(true);
    assert_eq!(got.len(), 197);
    assert_eq!(count_of(&c, PROBED), 0, "{c:?}");
    assert_eq!(count_of(&c, PRUNED), 0, "{c:?}");
}
