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

const COLUMNS: &str = "interp.matcher bound a hop end from the label's cached columns";
const PROJECTED: &str = "store.projected gets";
const FULL: &str = "graph.nodes materialised in full";
const VECTORISED: &str = "interp.subquery hop evaluated column-at-a-time";
const LOADED: &str = "interp.subquery hop loaded its far end's column whole";

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
        g.create_rel(
            w,
            "BELONGS_TO_PROJECT",
            projects[(i % 60) as usize],
            &BTreeMap::new(),
        )
        .expect("belongs");
    }
    for (k, p) in projects.iter().enumerate() {
        let mut m = BTreeMap::new();
        m.insert("status".to_string(), Value::Str("open".into()));
        m.insert("title".to_string(), Value::Str(format!("note {k}")));
        let n = g.create_node(&["Note".into()], &m).expect("note");
        g.create_rel(n, "BELONGS_TO_PROJECT", *p, &BTreeMap::new())
            .expect("note edge");
    }
    g
}

/// A whole-label columnar aggregate keeps the label's `status` column. (An
/// equality would build a derived index and seek instead of walking the
/// column, keeping nothing.)
fn warm_status(g: &Graph) {
    let (_, c) = traced(
        g,
        "MATCH (w:KMWorkItem) RETURN w.status AS s, count(*) AS n ORDER BY s",
    );
    // Kept by this walk — or already kept (fix 79 loads a subquery's far-end
    // column whole on first use, so a dashboard run before this warm-up has
    // filed it) and served back.
    assert!(
        count_of(&c, "graph.property column kept")
            + count_of(&c, "graph.property column kept aligned")
            + count_of(&c, "graph.property column served")
            > 0,
        "the warm-up keeps (or finds kept) the status column: {c:?}"
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
    // Before the column is cached: fix 79 loads it WHOLE on the first body
    // that reads it (once, not per body or per project) and answers every
    // body column-at-a-time from then on — where this used to be a
    // projected get per end (12,000 of them) until something else happened
    // to walk the label.
    let (cold, c) = traced(&g, DASHBOARD);
    assert_eq!(cold, want);
    assert_eq!(count_of(&c, LOADED), 1, "{c:?}");
    assert_eq!(count_of(&c, VECTORISED), 120, "{c:?}");
    assert_eq!(
        count_of(&c, COLUMNS),
        0,
        "the matcher never binds an end here: {c:?}"
    );
    assert!(
        count_of(&c, PROJECTED) <= 200,
        "no projected get per end: {c:?}"
    );
    // With it cached by a whole-label walk too: the same answer, no load.
    // (Fix 70 answers these one-hop bodies column-at-a-time from the cached
    // column — 60 rows × 2 counts — before the matcher would bind an end.)
    warm_status(&g);
    let (warm, c) = traced(&g, DASHBOARD);
    assert_eq!(warm, want);
    assert_eq!(count_of(&c, VECTORISED), 120, "{c:?}");
    // What still reads the store: the 60 projects themselves and the one
    // stray Note per project per subquery (a non-member, see below).
    assert!(
        count_of(&c, PROJECTED) <= 200,
        "no projected get for a cached end: {c:?}"
    );
    assert_eq!(count_of(&c, FULL), 0, "{c:?}");
}

/// A pattern comprehension reading two properties needs BOTH columns
/// cached; with one of them a column the label never walked whole, the
/// ends fall back to the projected read — until the statement's
/// sixty-fourth miss reads that column whole and keeps it (fix 87), after
/// which the rest bind from the columns — and agree either way.
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
    assert_eq!(
        count_of(
            &c,
            "interp.matcher warmed a hop end label's columns after repeated misses"
        ),
        1,
        "title is not cached until the sixty-fourth miss: {c:?}"
    );
    assert!(count_of(&c, COLUMNS) >= 5_000, "{c:?}");
    assert!(
        (64..400).contains(&count_of(&c, PROJECTED)),
        "sixty-four misses, then the column: {c:?}"
    );
    // `title` is cached now — the miss-warm kept it — so a whole-label
    // aggregate over it is served from the cache, not walked and kept.
    let (_, c) = traced(&g, "MATCH (w:KMWorkItem) RETURN min(w.title) AS t");
    assert!(
        count_of(
            &c,
            "interp.columnar column read served from the property-column cache"
        ) > 0,
        "{c:?}"
    );
    let (got, c) = traced(&g, two);
    assert_eq!(got, want);
    assert!(count_of(&c, COLUMNS) >= 6_000, "{c:?}");
    // The 60 projects and their 60 stray Notes still read the store.
    assert!(count_of(&c, PROJECTED) <= 130, "{c:?}");
}

/// A walk's ends share one label and one demand, so the walk fetches their
/// columns ONCE and binds each end by a binary search. The fetch takes the
/// column cache's global lock and bumps the column's shared refcount; per
/// END, from forty workers, it serialised SNB BI bi4's grouping stage (4.98M
/// forum ends, one fetch per demanded property each).
#[test]
fn a_walk_fetches_its_ends_columns_once() {
    let g = corpus();
    warm_status(&g);
    let two = "MATCH (p:KMProject) \
        RETURN p.id AS id, size([(w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' | w.title]) AS n \
        ORDER BY id";
    let want = general(&g, two);
    // the first run's misses keep `title` (fix 87)
    let _ = traced(&g, two);
    let (got, c) = traced(&g, two);
    assert_eq!(got, want);
    let bound = count_of(&c, COLUMNS);
    assert!(bound >= 6_000, "{c:?}");
    // 60 walks, two columns each, asked twice a walk — once by the fan-out
    // check that the end's columns are cached, once to bind from them: 240,
    // where each of the 6,000 ends fetched both before (12,000)
    let served = count_of(&c, "graph.property column served");
    assert!(
        served <= 60 * 4 + 8,
        "{served} column fetches for {bound} ends bound from them: {c:?}"
    );
}

/// An UNLABELLED end binds from the columns of a label it carries: the
/// column cache is keyed by label, so an end the pattern names no label for
/// was a projected record read per end however warm its label's columns —
/// SNB Interactive IS2's `(message)` read 5,916 messages from the store. The
/// stray Note is no work item: it takes the record read, and still counts.
#[test]
fn an_unlabelled_end_binds_from_a_labels_columns() {
    const ANY: &str = "interp.matcher bound an unlabelled hop end from a label's cached columns";
    let g = corpus();
    warm_status(&g);
    // a whole-label walk keeps `title` too
    let _ = traced(&g, "MATCH (w:KMWorkItem) RETURN min(w.title) AS t");
    let q = "MATCH (p:KMProject) \
        RETURN p.id AS id, size([(w)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = 'open' | w.title]) AS n \
        ORDER BY id";
    let want = general(&g, q);
    let (got, c) = traced(&g, q);
    assert_eq!(got, want);
    assert_eq!(want[0][1], Value::Int(41), "40 open work items and the open Note");
    assert!(count_of(&c, ANY) >= 6_000, "{c:?}");
}

/// An end whose demand holds a column that cannot be cached asks for the
/// columns only up to that one: SNB BI bi12 demands a message's `content` —
/// larger than the whole column budget, and first of its three — and asking
/// for the two cached ones as well at each of 9M messages took it from 8.4 s
/// to 18.3 s. Here `content` (2 KB an item) outgrows a 2 MB budget that
/// holds `status`.
#[test]
fn an_uncacheable_column_ends_the_asking_at_each_end() {
    let g = corpus();
    g.set_prop_column_budget(2 * 1024 * 1024);
    warm_status(&g);
    let q = "MATCH (p:KMProject) \
        RETURN p.id AS id, size([(w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
            WHERE w.status = 'open' AND w.content STARTS WITH 'b' | w.id]) AS n \
        ORDER BY id";
    let want = general(&g, q);
    let (got, c) = traced(&g, q);
    assert_eq!(got, want);
    assert_eq!(want[0][1], Value::Int(40), "{want:?}");
    assert_eq!(count_of(&c, COLUMNS), 0, "`content` was bound from a column: {c:?}");
    // `content` sorts first of the demand, and is never there: no end goes
    // on to fetch `status`
    let served = count_of(&c, "graph.property column served");
    assert!(
        served < 600,
        "{served} column fetches over 6,000 ends none of which could bind: {c:?}"
    );
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
    assert_eq!(
        want[0][2],
        Value::Int(41),
        "the note counts on the unlabelled pattern"
    );
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
