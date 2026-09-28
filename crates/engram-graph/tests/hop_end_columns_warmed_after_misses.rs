#![allow(non_snake_case)]
//! Fix 87: the KMProject dashboard's work items — `MATCH (p:KMProject)
//! OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p,
//! max(w.updatedAt) AS last …` — reach 1,338 items through 77 starts of
//! about 17 ends each. No single start's fan-out is worth a whole-label
//! column (fix 83's rule), yet the STATEMENT read a tenth of the label's
//! `updatedAt` by projected record reads (1,338 gets, ~11 ms of the
//! dashboard's 36 against Neo4j's 23 on the mirror). Now every projected
//! read of a labelled end whose demanded columns are not cached is a MISS
//! against its label, and at the batch floor (64) in one statement the
//! label's uncached demanded columns are read whole and kept: the ends
//! after the sixty-fourth bind from the columns, this statement and the
//! next. A statement that touches fewer ends keeps its projected reads.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

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

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const WARMED_MISSES: &str = "interp.matcher warmed a hop end label's columns after repeated misses";
const WARMED_FANOUT: &str = "interp.matcher warmed a hop end label's columns for a wide fan-out";
const COLUMNS: &str = "interp.matcher bound a hop end from the label's cached columns";
const PROJECTED: &str = "store.projected gets";

/// 80 projects; 12,000 work items of which the first 1,360 belong to the
/// projects, 17 each — a tenth of the label, no start past 17 ends.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut projects = Vec::new();
    for k in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{k:03}")));
        m.insert("name".to_string(), Value::Str(format!("Project {k}")));
        projects.push(g.create_node(&["KMProject".into()], &m).expect("project"));
    }
    for i in 0..12_000i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("wi-{i:05}")));
        m.insert(
            "status".to_string(),
            Value::Str(if i % 4 == 0 {
                "done".into()
            } else {
                "open".into()
            }),
        );
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!(
                "2026-{:02}-{:02}T{:02}:{:02}:00Z",
                1 + (i % 12),
                1 + (i % 28),
                i % 24,
                i % 60
            )),
        );
        m.insert("blob".to_string(), Value::Str("b".repeat(300)));
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        if i < 1360 {
            g.create_rel(
                w,
                "BELONGS_TO_PROJECT",
                projects[(i / 17) as usize],
                &BTreeMap::new(),
            )
            .expect("rel");
        }
    }
    g
}

const DASH: &str = "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
    WITH p, max(w.updatedAt) AS last RETURN p.id AS id, last ORDER BY last DESC, id";

/// The dashboard's 1,360 ends: sixty-four projected reads, then the label's
/// `updatedAt` column read whole and every later end bound from it; the
/// next run binds them all from the cache with no warm.
#[test]
fn a_the_dashboards_ends_warm_the_label_after_sixty_four_misses() {
    let g = corpus();
    let want = general(&g, DASH);
    assert_eq!(want.len(), 80, "fixture");
    let (got, c) = traced(&g, DASH);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED_MISSES), 1, "{c:?}");
    assert_eq!(
        count_of(&c, WARMED_FANOUT),
        0,
        "no start fans out enough: {c:?}"
    );
    assert!(count_of(&c, COLUMNS) >= 1_200, "{c:?}");
    assert!(
        (60..120).contains(&count_of(&c, PROJECTED)),
        "sixty-four misses, then the columns: {c:?}"
    );
    let (got, c) = traced(&g, DASH);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED_MISSES), 0, "{c:?}");
    assert!(count_of(&c, COLUMNS) >= 1_300, "{c:?}");
    assert!(count_of(&c, PROJECTED) < 20, "{c:?}");
}

/// Three projects' fifty-one items stay under the floor: projected reads,
/// nothing warmed, the same answer.
#[test]
fn b_under_the_floor_the_projected_reads_stay() {
    let g = corpus();
    let src = "MATCH (p:KMProject) WHERE p.id IN ['proj-000', 'proj-001', 'proj-002'] \
        OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
        WITH p, max(w.updatedAt) AS last RETURN p.id AS id, last ORDER BY id";
    let want = general(&g, src);
    assert_eq!(want.len(), 3, "fixture");
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED_MISSES), 0, "{c:?}");
    assert_eq!(count_of(&c, COLUMNS), 0, "{c:?}");
    assert!((45..60).contains(&count_of(&c, PROJECTED)), "{c:?}");
}

/// The misses are counted per STATEMENT: two statements of thirty-two ends
/// each never warm, the sixty-fourth end of one statement does.
#[test]
fn c_the_misses_are_counted_per_statement() {
    let g = corpus();
    let two = "MATCH (p:KMProject) WHERE p.id IN ['proj-010', 'proj-011'] \
        OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
        WITH p, max(w.updatedAt) AS last RETURN p.id AS id, last ORDER BY id";
    for _ in 0..2 {
        let (_, c) = traced(&g, two);
        assert_eq!(count_of(&c, WARMED_MISSES), 0, "{c:?}");
    }
    let four = "MATCH (p:KMProject) WHERE p.id IN ['proj-010', 'proj-011', 'proj-012', 'proj-013'] \
        OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
        WITH p, max(w.updatedAt) AS last RETURN p.id AS id, last ORDER BY id";
    let want = general(&g, four);
    let (got, c) = traced(&g, four);
    assert_eq!(got, want);
    assert_eq!(
        count_of(&c, WARMED_MISSES),
        1,
        "sixty-eight ends in one statement: {c:?}"
    );
}
