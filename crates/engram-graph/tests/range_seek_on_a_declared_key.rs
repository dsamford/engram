#![allow(non_snake_case)]
//! Fix 47: a comparison (`<`, `<=`, `>`, `>=`) on a DECLARED string key —
//! a trailing key of a declared composite included — seeks the scoped
//! range index, as an equality and a prefix do. Neo4j's composite
//! `NewsStory(status, lastUpdatedAt)` index answers `s.status <> 'stale'
//! AND s.lastUpdatedAt > $cutoff … LIMIT 5` in 2 ms from its entries; the
//! mirror declared the same index, which here was only ever its leading
//! key's scoped index, and read the whole label (7.6–11.8 ms on v113
//! after the column-at-a-time predicate, 84–183 before).
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const RANGE_SEEK: &str = "interp.columnar seek probed a declared range";
const COVERED_RANGE: &str = "interp.columnar covered count sought a range";
const EXPR: &str = "cypher.expressions evaluated";

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("t".to_string(), Value::Str("Business and Finance".into()));
    p.insert("cutoff".to_string(), Value::Str("2026-08-31T00:00:00.000Z".into()));
    p.insert(
        "existingIds".to_string(),
        Value::List((4600..4610).map(|i| Value::Str(format!("s-{i:05}"))).collect()),
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

/// 5,000 stories under the mirror's catalogue: `storyId`, `publishedAt`
/// and the COMPOSITE `(status, lastUpdatedAt)`; the recent ones (past the
/// cutoff) are the last 8% in id order plus every 97th; a numeric `score`
/// is declared too (a non-string bound must not seek).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX story_id FOR (s:NewsStory) ON (s.storyId)");
    ddl(&g, "CREATE INDEX story_pub FOR (s:NewsStory) ON (s.publishedAt)");
    ddl(&g, "CREATE INDEX story_status_updated FOR (s:NewsStory) ON (s.status, s.lastUpdatedAt)");
    ddl(&g, "CREATE INDEX story_score FOR (s:NewsStory) ON (s.score)");
    let n = 5000i64;
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("s-{i:05}")));
        m.insert(
            "primaryTopic".to_string(),
            Value::Str(if i % 50 == 0 { "Sports".into() } else { "Business and Finance".into() }),
        );
        m.insert(
            "status".to_string(),
            Value::Str(if i % 10 == 3 { "stale".into() } else { "active".into() }),
        );
        let recent = i >= n - n / 12 || i % 97 == 0;
        m.insert(
            "lastUpdatedAt".to_string(),
            Value::Str(if recent {
                format!("2026-09-0{}T{:02}:{:02}:00.000Z", 1 + (i % 4), i % 24, i % 60)
            } else {
                format!("2026-0{}-{:02}T{:02}:{:02}:00.000Z", 1 + (i % 8), 1 + (i % 28), i % 24, i % 60)
            }),
        );
        m.insert("publishedAt".to_string(), Value::Str(format!("2026-0{}-{:02}", 1 + (i % 8), 1 + (i % 28))));
        m.insert("title".to_string(), Value::Str(format!("Story {i}")));
        m.insert("score".to_string(), Value::Float((i % 100) as f64 / 100.0));
        if i % 11 == 0 {
            // A non-string value under the composite's trailing key: outside
            // every string range, as the comparison answers null for it.
            m.insert("lastUpdatedAt".to_string(), Value::Int(i));
        }
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    g
}

fn check(g: &Graph, src: &str) -> BTreeMap<String, u64> {
    let want = general(g, src);
    let first = rows(g, src);
    assert_eq!(first, want, "first run `{src}`");
    let (got, c) = traced(g, src);
    assert_eq!(got, want, "second run `{src}`");
    c
}

const PRED: &str =
    "s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff";

#[test]
fn the_topic_listing_seeks_the_composite_s_trailing_key() {
    let g = corpus();
    let src = format!(
        "MATCH (s:NewsStory) WHERE {PRED} AND NOT s.storyId IN $existingIds RETURN s.storyId AS storyId, s.title AS title LIMIT 5"
    );
    let c = check(&g, &src);
    assert!(count_of(&c, RANGE_SEEK) > 0, "{c:?}");
    // The sought ids are a few hundred, not the label: the residual runs
    // over them alone.
    assert!(count_of(&c, EXPR) < 2000, "{c:?}");
    let src2 = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId ORDER BY s.lastUpdatedAt DESC, storyId LIMIT 5");
    let c2 = check(&g, &src2);
    assert!(count_of(&c2, RANGE_SEEK) > 0, "{c2:?}");
}

#[test]
fn every_comparison_and_the_mirrored_spelling_seek() {
    let g = corpus();
    for src in [
        "MATCH (s:NewsStory) WHERE s.lastUpdatedAt >= $cutoff RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE s.lastUpdatedAt < '2026-03-01' RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE s.lastUpdatedAt <= '2026-03-01T00:00:00.000Z' RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE $cutoff < s.lastUpdatedAt RETURN s.storyId AS id ORDER BY id LIMIT 7",
        "MATCH (s:NewsStory) WHERE s.status = 'active' AND s.lastUpdatedAt > $cutoff RETURN count(s) AS n",
    ] {
        let c = check(&g, src);
        assert!(count_of(&c, RANGE_SEEK) + count_of(&c, COVERED_RANGE) > 0, "`{src}`: {c:?}");
    }
}

/// CONTROLS: a numeric bound, an undeclared key and a bound reading a
/// variable never seek — and still answer as the general path does.
#[test]
fn a_numeric_bound_an_undeclared_key_and_a_variable_bound_do_not_seek() {
    let g = corpus();
    for src in [
        "MATCH (s:NewsStory) WHERE s.score > 0.5 RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE s.title > 'Story 4' RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE s.lastUpdatedAt > s.publishedAt RETURN count(s) AS n",
        "MATCH (s:NewsStory) WHERE s.lastUpdatedAt > 5 RETURN count(s) AS n",
    ] {
        let c = check(&g, src);
        assert_eq!(count_of(&c, RANGE_SEEK) + count_of(&c, COVERED_RANGE), 0, "`{src}`: {c:?}");
    }
}
