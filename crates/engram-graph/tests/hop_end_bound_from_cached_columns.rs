#![allow(non_snake_case)]
//! Fix 60: the clause executor's matcher binds each hop end to its demand
//! (fix 51) with a PROJECTED store get per row. When every demanded
//! property is a cached column of the end's one pattern label, the end is
//! built from the columns instead — a membership test and a binary search
//! per property. The KMProject dashboard's eight `COUNT { (w:KMWorkItem)-
//! [:BELONGS_TO_PROJECT]->(p) WHERE w.status = … }` per project read
//! 18,053 work items projected from the store for a `status` the label's
//! column already held (208 ms on the mirror against Neo4j's 22).
//!
//! Every answer is checked against the same statement before the columns
//! were cached (the projected read) and with the columnar paths OFF.

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

const DEMAND: &str = "interp.matcher bound a hop end to its demand";
const COLUMNS: &str = "interp.matcher bound a hop end from the label's cached columns";
const PROJECTED: &str = "store.projected gets";
const FULL: &str = "graph.nodes materialised in full";
const VECTORISED: &str = "interp.subquery hop evaluated column-at-a-time";

/// 60 projects, 6,000 work items (100 per project) with a 2 KB body and a
/// status in {open, done, blocked}; one stray `Note` per project hangs off
/// the same edge type without the KMWorkItem label.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(2048);
    let mut projects = Vec::new();
    for k in 0..60i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{k:03}")));
        projects.push(g.create_node(&["KMProject".into()], &m).expect("project"));
    }
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("wi-{i:05}")));
        m.insert("title".to_string(), Value::Str(format!("Item {i}")));
        // The project is `i % 60`, so the status must vary along `i / 60`
        // or every item of a project would share one status.
        m.insert(
            "status".to_string(),
            Value::Str(match (i / 60) % 5 {
                0 | 1 => "open".into(),
                2 => "blocked".into(),
                _ => "done".into(),
            }),
        );
        m.insert("content".to_string(), Value::Str(body.clone()));
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO_PROJECT", projects[(i % 60) as usize], &BTreeMap::new())
            .expect("belongs");
    }
    for (k, p) in projects.iter().enumerate() {
        let mut m = BTreeMap::new();
        m.insert("status".to_string(), Value::Str("open".into()));
        m.insert("title".to_string(), Value::Str(format!("note {k}")));
        let n = g.create_node(&["Note".into()], &m).expect("note");
        g.create_rel(n, "BELONGS_TO_PROJECT", *p, &BTreeMap::new()).expect("note edge");
    }
    g
}

/// A whole-label columnar aggregate keeps the label's `status` column. (An
/// equality would build a derived index and seek instead of walking the
/// column, keeping nothing.)
fn warm_status(g: &Graph) {
    let (_, c) = traced(g, "MATCH (w:KMWorkItem) RETURN w.status AS s, count(*) AS n ORDER BY s");
    assert!(
        count_of(&c, "graph.property column kept") + count_of(&c, "graph.property column kept aligned") > 0,
        "the warm-up keeps the status column: {c:?}"
    );
}

const DASHBOARD: &str = "MATCH (p:KMProject) \
    RETURN p.id AS id, \
      COUNT { (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' } AS open, \
      COUNT { (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'blocked' } AS blocked \
    ORDER BY id";

#[test]
fn a_count_subquery_binds_its_hop_end_from_the_cached_status_column() {
    let g = corpus();
    let want = general(&g, DASHBOARD);
    assert_eq!(want.len(), 60);
    assert_eq!(want[0][1], Value::Int(40), "open per project");
    assert_eq!(want[0][2], Value::Int(20), "blocked per project");
    // Before the column is cached: the projected read per end, as before.
    let (cold, c) = traced(&g, DASHBOARD);
    assert_eq!(cold, want);
    assert!(count_of(&c, DEMAND) > 0, "{c:?}");
    assert_eq!(count_of(&c, COLUMNS), 0, "nothing cached yet: {c:?}");
    assert!(count_of(&c, PROJECTED) >= 12_000, "a projected get per end: {c:?}");
    // With it cached: no store read for the ends. (Fix 70 answers these
    // one-hop bodies column-at-a-time from the same cached column — 60
    // rows × 2 counts — before the matcher would have bound each end.)
    warm_status(&g);
    let (warm, c) = traced(&g, DASHBOARD);
    assert_eq!(warm, want);
    assert_eq!(count_of(&c, VECTORISED), 120, "{c:?}");
    // What still reads the store: the 60 projects themselves and the one
    // stray Note per project per subquery (a non-member, see below).
    assert!(count_of(&c, PROJECTED) <= 200, "no projected get for a cached end: {c:?}");
    assert_eq!(count_of(&c, FULL), 0, "{c:?}");
}

/// A pattern comprehension reading two properties needs BOTH columns
/// cached; with one of them a body the label never walked whole, the ends
/// fall back to the projected read — and agree.
#[test]
fn a_comprehension_needs_every_demanded_column_cached() {
    let g = corpus();
    warm_status(&g);
    let two = "MATCH (p:KMProject) \
        RETURN p.id AS id, size([(w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' | w.title]) AS n \
        ORDER BY id";
    let want = general(&g, two);
    let (got, c) = traced(&g, two);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, COLUMNS), 0, "title is not cached: {c:?}");
    assert!(count_of(&c, PROJECTED) >= 6_000, "{c:?}");
    // Cache `title` too (a whole-label aggregate walks and keeps it).
    let (_, c) = traced(&g, "MATCH (w:KMWorkItem) RETURN min(w.title) AS t");
    assert!(
        count_of(&c, "graph.property column kept") + count_of(&c, "graph.property column kept aligned") > 0,
        "{c:?}"
    );
    let (got, c) = traced(&g, two);
    assert_eq!(got, want);
    assert!(count_of(&c, COLUMNS) >= 6_000, "{c:?}");
    // The 60 projects and their 60 stray Notes still read the store.
    assert!(count_of(&c, PROJECTED) <= 130, "{c:?}");
}

/// The stray Notes reach the projects over the same edge type but carry no
/// KMWorkItem label: the membership test refuses them to the store read,
/// which the pattern then rejects — never counted, never fabricated.
#[test]
fn a_non_member_end_is_not_built_from_the_label_column() {
    let g = corpus();
    warm_status(&g);
    let src = "MATCH (p:KMProject) \
        RETURN p.id AS id, COUNT { (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' } AS open, \
        COUNT { (x)-[:BELONGS_TO_PROJECT]->(p) WHERE x.status = 'open' } AS any \
        ORDER BY id";
    let want = general(&g, src);
    assert_eq!(want[0][1], Value::Int(40));
    assert_eq!(want[0][2], Value::Int(41), "the note counts on the unlabelled pattern");
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    // The labelled body runs column-at-a-time (fix 70) — the note is not a
    // member and never counted; the unlabelled body has no column to read
    // from and keeps the matcher, which reads the store for its ends.
    assert_eq!(count_of(&c, VECTORISED), 60, "{c:?}");
    assert!(count_of(&c, PROJECTED) > 0, "{c:?}");
}

/// A body that reads the end's labels or the whole node demands it FULL and
/// never takes the column path.
#[test]
fn a_full_demand_never_takes_the_columns() {
    let g = corpus();
    warm_status(&g);
    let src = "MATCH (p:KMProject) \
        RETURN p.id AS id, size([(w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' | labels(w)]) AS n \
        ORDER BY id";
    let want = general(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, COLUMNS), 0, "{c:?}");
}
