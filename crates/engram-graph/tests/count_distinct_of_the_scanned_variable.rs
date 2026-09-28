#![allow(non_snake_case)]
//! Fix 86: `count(DISTINCT v)` over the ONE variable a single-source scan
//! binds is `count(*)` — every member is exactly one row. The bare `v`
//! inside the aggregate declined the columnar rewrite and the statement
//! fell to the general path, which materialised every survivor and kept a
//! serialised id per row: the NewsArticle classification aggregate
//! (`MATCH (a:NewsArticle) WHERE a.classifiedAt IS NOT NULL AND a.pubDate >=
//! $cutoff AND (a.abuseStatus IS NULL OR …) AND a.contentType IS NOT NULL
//! RETURN a.contentType AS key, count(DISTINCT a) AS n`) ran 305 ms on the
//! mirror against 128 for the same statement spelt `count(a)`, Neo4j 225.
//! A hop's start repeats across its edges, so a hop scan keeps DISTINCT.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert(
        "cutoff".to_string(),
        Value::Str("2026-07-22T00:00:00Z".into()),
    );
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

const STARRED: &str = "interp.columnar count distinct of the scanned variable counted its members";
const AGG_SCAN: &str = "interp.columnar aggregate scans";
const FULL: &str = "graph.nodes materialised in full";
const PROJECTED: &str = "graph.projected node materialisations";

/// 6,000 articles over seven content types; a third unclassified, a tenth
/// quarantined, half before the cutoff; every third mentions one of 200
/// entities.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let types = [
        "breaking",
        "analysis",
        "opinion",
        "feature",
        "brief",
        "interview",
        "explainer",
    ];
    let mut ents = Vec::new();
    for k in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Entity {k:03}")));
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("art-{i:05}")));
        m.insert(
            "contentType".to_string(),
            Value::Str(types[(i % 7) as usize].into()),
        );
        if i % 3 != 0 {
            m.insert(
                "classifiedAt".to_string(),
                Value::Str("2026-08-01T00:00:00Z".into()),
            );
        }
        if i % 10 == 0 {
            m.insert("abuseStatus".to_string(), Value::Str("quarantined".into()));
        } else if i % 10 == 1 {
            m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
        }
        m.insert(
            "pubDate".to_string(),
            Value::Str(if i % 2 == 0 {
                "2026-08-15T00:00:00Z".into()
            } else {
                "2026-06-01T00:00:00Z".into()
            }),
        );
        let a = g.create_node(&["NewsArticle".into()], &m).expect("article");
        if i % 3 == 0 {
            g.create_rel(
                a,
                "MENTIONS",
                ents[(i / 3 % 200) as usize],
                &BTreeMap::new(),
            )
            .expect("mentions");
        }
    }
    g
}

const PRED: &str = "a.classifiedAt IS NOT NULL AND a.pubDate >= $cutoff \
    AND (a.abuseStatus IS NULL OR a.abuseStatus IN ['clean', 'approved']) AND a.contentType IS NOT NULL";

/// The classification aggregate counts its members per type on the
/// columnar aggregate scan — no node materialised — and answers exactly
/// what the general path and the `count(a)` spelling answer.
#[test]
fn a_count_distinct_of_the_scanned_variable_runs_as_a_star_count() {
    let g = corpus();
    let orig = format!(
        "MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(DISTINCT a) AS n ORDER BY key"
    );
    let plain = format!(
        "MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(a) AS n ORDER BY key"
    );
    let want = general(&g, &orig);
    assert_eq!(want.len(), 7, "fixture");
    assert_eq!(
        rows(&g, &plain),
        want,
        "count(a) and count(DISTINCT a) agree on a single scan"
    );
    let _ = rows(&g, &orig);
    let (got, c) = traced(&g, &orig);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, STARRED), 1, "{c:?}");
    assert!(count_of(&c, AGG_SCAN) >= 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), 0, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
}

/// The ungrouped form and an ORDER BY naming the call.
#[test]
fn b_ungrouped_and_ordered_by_the_call() {
    let g = corpus();
    for src in [
        format!("MATCH (a:NewsArticle) WHERE {PRED} RETURN count(DISTINCT a) AS n"),
        format!(
            "MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(DISTINCT a) AS n ORDER BY count(DISTINCT a) DESC, key"
        ),
    ] {
        let want = general(&g, &src);
        let _ = rows(&g, &src);
        let (got, c) = traced(&g, &src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, STARRED), 1, "`{src}`: {c:?}");
    }
}

/// CONTROLS: a DISTINCT over a PROPERTY keeps its set (two types carry
/// the same value pattern, the counts differ from the member counts); a
/// hop scan's start repeats across its edges, so `count(DISTINCT a)`
/// there stays a distinct count and is smaller than `count(*)`.
#[test]
fn c_a_property_distinct_and_a_hops_distinct_keep_their_sets() {
    let g = corpus();
    let prop =
        format!("MATCH (a:NewsArticle) WHERE {PRED} RETURN count(DISTINCT a.contentType) AS n");
    let want = general(&g, &prop);
    assert_eq!(want, vec![vec![Value::Int(7)]], "fixture");
    let _ = rows(&g, &prop);
    let (got, c) = traced(&g, &prop);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, STARRED), 0, "{c:?}");
    // Two articles mention each entity on average: distinct starts < edges.
    let hop = "MATCH (a:NewsArticle)-[:MENTIONS]->(e:Entity) RETURN count(DISTINCT e) AS ents, count(*) AS edges";
    let want = general(&g, hop);
    let _ = rows(&g, hop);
    let (got, c) = traced(&g, hop);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, STARRED), 0, "{c:?}");
    let (Value::Int(ents), Value::Int(edges)) = (&got[0][0], &got[0][1]) else {
        panic!("{got:?}");
    };
    assert!(*ents < *edges, "{got:?}");
}
