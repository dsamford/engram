#![allow(non_snake_case)]
//! Fix 35 (v110): a MULTI-LABEL node source (`(r:Repo:ManagedRepo)`) reads
//! its property columns through its SMALLEST label's property-column cache
//! entry, restricted to the intersection, and a whole walk KEEPS what it
//! gathered when the intersection is that label whole. Before this the
//! cache was consulted only for a single-label source: the two-label
//! ManagedRepo listing re-gathered its 143 records on every statement
//! (`store.gets` 143 per run, 1.8 ms against Neo4j's 1.7) while the
//! one-label spelling of the same list was served from the cache.
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("limit".to_string(), Value::Int(200));
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

const SERVED: &str = "interp.columnar column read served from the property-column cache";
const THROUGH_SMALLEST: &str = "interp.columnar multi-label column read through its smallest label";
const KEPT: &str = "graph.property column kept";
const GATHER_OR_WALK: &[&str] = &[
    "graph.column point-gather",
    "graph.column record-gather",
    "graph.column range scans",
];

fn reads(c: &BTreeMap<String, u64>) -> u64 {
    GATHER_OR_WALK.iter().map(|k| count_of(c, k)).sum()
}

/// 240 `:Repo:ManagedRepo` (every ManagedRepo is a Repo) beside 80 plain
/// `:Repo`, each followed by a `:Filler` so the labels interleave; and a
/// STRICT-SUBSET pair — 50 `:Tagged` of which 30 are also `:Thing`, beside
/// 200 `:Thing` — where the smallest label is NOT the intersection.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let filler = |g: &Graph, i: i64| {
        let mut f = BTreeMap::new();
        f.insert("other".to_string(), Value::Str(format!("filler-{i}")));
        g.create_node(&["Filler".into()], &f).expect("filler");
    };
    for i in 0..320i64 {
        let mut m = BTreeMap::new();
        m.insert("repoId".to_string(), Value::Str(format!("repo-{i:04}")));
        m.insert("fullName".to_string(), Value::Str(format!("org{}/name-{i}", i % 9)));
        m.insert("orgId".to_string(), Value::Str(format!("org{}", i % 9)));
        m.insert(
            "provider".to_string(),
            Value::Str(if i % 4 == 0 { "lore".into() } else { "github".into() }),
        );
        if i % 3 != 0 {
            m.insert(
                "autonomyLevel".to_string(),
                Value::Str(if i % 6 == 1 { "unmanaged".into() } else { "assisted".into() }),
            );
        }
        let labels: Vec<String> = if i % 4 == 3 {
            vec!["Repo".into()]
        } else {
            vec!["Repo".into(), "ManagedRepo".into()]
        };
        g.create_node(&labels, &m).expect("repo");
        filler(&g, i);
    }
    for i in 0..230i64 {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(i));
        let labels: Vec<String> = if i < 30 {
            vec!["Thing".into(), "Tagged".into()]
        } else if i < 50 {
            vec!["Tagged".into()]
        } else {
            vec!["Thing".into()]
        };
        g.create_node(&labels, &m).expect("thing");
        filler(&g, 1000 + i);
    }
    g
}

#[test]
fn a_two_label_projection_reads_through_its_smallest_label_and_is_served_next() {
    let g = corpus();
    for src in [
        "MATCH (r:Repo:ManagedRepo) RETURN r.repoId AS id ORDER BY id",
        "MATCH (r:Repo:ManagedRepo) WHERE r.provider = 'lore' RETURN count(r) AS n",
        "MATCH (r:Repo:ManagedRepo) WHERE r.autonomyLevel IS NOT NULL RETURN count(r) AS n",
        "MATCH (r:Repo:ManagedRepo) WHERE r.provider = 'lore' AND coalesce(r.autonomyLevel, 'unmanaged') <> 'unmanaged' \
         RETURN properties(r) AS r ORDER BY r.orgId, r.fullName LIMIT toInteger($limit)",
    ] {
        let g2 = corpus();
        let want = general(&g2, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (first, c1) = traced(&g, src);
        assert_eq!(first, want, "first `{src}`");
        assert!(count_of(&c1, THROUGH_SMALLEST) > 0, "`{src}` reads through the smallest label: {c1:?}");
        let (second, c2) = traced(&g, src);
        assert_eq!(second, want, "second `{src}`");
        assert!(count_of(&c2, SERVED) > 0, "`{src}` second run is served: {c2:?}");
        assert_eq!(reads(&c2), 0, "`{src}` second run reads no column: {c2:?}");
    }
}

/// The two spellings share the cache: the one-label list keeps the column
/// and the two-label list is served from it — and the other way round.
#[test]
fn the_one_label_and_two_label_spellings_share_the_cache() {
    let g = corpus();
    let one = "MATCH (r:ManagedRepo) RETURN r.repoId AS id ORDER BY id";
    let two = "MATCH (r:Repo:ManagedRepo) RETURN r.repoId AS id ORDER BY id";
    let want = general(&g, one);
    assert_eq!(general(&g, two), want, "every ManagedRepo is a Repo");
    let (r1, c1) = traced(&g, one);
    assert_eq!(r1, want);
    assert!(count_of(&c1, KEPT) > 0, "{c1:?}");
    let (r2, c2) = traced(&g, two);
    assert_eq!(r2, want);
    assert!(count_of(&c2, SERVED) > 0, "two-label served by the one-label keep: {c2:?}");
    assert_eq!(reads(&c2), 0, "{c2:?}");

    let g = corpus();
    let (r2, c2) = traced(&g, two);
    assert_eq!(r2, want);
    assert!(count_of(&c2, KEPT) > 0, "the two-label whole walk keeps: {c2:?}");
    let (r1, c1) = traced(&g, one);
    assert_eq!(r1, want);
    assert!(count_of(&c1, SERVED) > 0, "one-label served by the two-label keep: {c1:?}");
}

/// A STRICT-SUBSET intersection reads through the smallest label restricted
/// to its members, and its walk is NOT filed as that label's whole column.
#[test]
fn a_strict_subset_intersection_reads_through_but_is_not_kept_as_the_label() {
    let src = "MATCH (t:Thing:Tagged) RETURN t.k AS k ORDER BY k";
    let g = corpus();
    let want = general(&g, src);
    assert_eq!(want.len(), 30, "the intersection is a strict subset of :Tagged");
    let (r1, c1) = traced(&g, src);
    assert_eq!(r1, want);
    assert!(count_of(&c1, THROUGH_SMALLEST) > 0, "{c1:?}");
    assert_eq!(count_of(&c1, KEPT), 0, "a strict subset is not the label's column: {c1:?}");
    let (r2, c2) = traced(&g, src);
    assert_eq!(r2, want);
    assert_eq!(count_of(&c2, SERVED), 0, "nothing was kept to serve from: {c2:?}");

    // Warm the smallest label whole; the intersection is then served from it.
    let (all, ca) = traced(&g, "MATCH (t:Tagged) RETURN t.k AS k ORDER BY k");
    assert_eq!(all.len(), 50);
    assert!(count_of(&ca, KEPT) > 0, "{ca:?}");
    let (r3, c3) = traced(&g, src);
    assert_eq!(r3, want, "served through :Tagged, restricted to the intersection");
    assert!(count_of(&c3, SERVED) > 0, "{c3:?}");
    assert_eq!(reads(&c3), 0, "{c3:?}");
}

/// A commit retires the kept column: a repo created after the keep is in
/// the next answer.
#[test]
fn a_commit_retires_the_two_label_keep() {
    let src = "MATCH (r:Repo:ManagedRepo) WHERE r.provider = 'lore' RETURN r.repoId AS id ORDER BY id";
    let g = corpus();
    let (before, c1) = traced(&g, src);
    assert!(count_of(&c1, THROUGH_SMALLEST) > 0, "{c1:?}");
    let mut m = BTreeMap::new();
    m.insert("repoId".to_string(), Value::Str("repo-9999".to_string()));
    m.insert("provider".to_string(), Value::Str("lore".to_string()));
    g.create_node(&["Repo".into(), "ManagedRepo".into()], &m).expect("new repo");
    let want = general(&g, src);
    assert_eq!(want.len(), before.len() + 1);
    let (after, _) = traced(&g, src);
    assert_eq!(after, want, "the new repo is in the answer");
}
