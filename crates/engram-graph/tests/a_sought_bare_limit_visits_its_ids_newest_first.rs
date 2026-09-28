#![allow(non_snake_case)]
//! Fix 114: a bare `LIMIT` over a SOUGHT population (an index seek's ids)
//! visits the ids newest first — fix 82's rule for the label scan — so a
//! recency-filtered pick finds its k survivors at the end of id order
//! instead of reading almost every candidate.
//!
//! The production story pick `MATCH (s:NewsStory) WHERE s.primaryTopic =
//! $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN … LIMIT
//! 5` read 1,324 sought stories ascending — a projected record each — to
//! answer five recent ones: 9 ms on the mirror against Neo4j's 1.1.
//!
//! The rows are pinned against a hand-computed answer, in id order as the
//! label scan's are; an ordered pick still visits everything. One story in
//! thirty-two carries the topic: the per-id seek admits a probe naming fewer
//! than a sixteenth of the label.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const NEWEST: &str = "interp.columnar projection sought ids visited newest first for the limit";
const GETS: &str = "store.projected gets";

const N: i64 = 2_000;

fn stamp(i: i64) -> String {
    format!("2026-{:02}-{:02}", 1 + (i / 250) % 12, 1 + i % 28)
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("t".to_string(), Value::Str("crime".into()));
    p.insert("cutoff".to_string(), Value::Str(stamp(1_600)));
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn survives(i: i64) -> bool {
    i % 32 == 0 && i % 5 != 0 && stamp(i).as_str() > stamp(1_600).as_str()
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ddl =
        parse_any("CREATE INDEX story_topic FOR (s:Story) ON (s.primaryTopic)").expect("parse ddl");
    run_stmt(&g, &ddl, BTreeMap::new()).expect("index");
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("s-{i:04}")));
        m.insert(
            "primaryTopic".to_string(),
            Value::Str(if i % 32 == 0 {
                "crime".into()
            } else {
                "other".into()
            }),
        );
        m.insert(
            "status".to_string(),
            Value::Str(if i % 5 == 0 {
                "stale".into()
            } else {
                "active".into()
            }),
        );
        m.insert("lastUpdatedAt".to_string(), Value::Str(stamp(i)));
        m.insert("summary".to_string(), Value::Str("a summary ".repeat(10)));
        g.create_node(&["Story".into()], &m).expect("story");
    }
    g
}

const PICK: &str = "MATCH (s:Story) WHERE s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN s.storyId AS id LIMIT 5";

/// The pick answers the five NEWEST survivors, in id order, reading a
/// handful of sought records instead of the whole seek.
#[test]
fn a_a_bare_limit_over_sought_ids_reads_the_newest_survivors() {
    let g = corpus();
    let mut newest: Vec<i64> = (0..N).rev().filter(|&i| survives(i)).take(5).collect();
    newest.sort_unstable();
    let want: Vec<Vec<Value>> = newest
        .iter()
        .map(|i| vec![Value::Str(format!("s-{i:04}"))])
        .collect();
    for pass in 0..2 {
        let (got, c) = traced(&g, PICK);
        assert_eq!(got, want, "pass {pass}: {c:?}");
        assert_eq!(count_of(&c, NEWEST), 1, "pass {pass}: {c:?}");
        assert!(count_of(&c, GETS) <= 20, "pass {pass}: {c:?}");
    }
}

/// CONTROLS: an ordered pick (no early cap) still visits every candidate
/// and answers the five OLDEST survivors; the count is unchanged.
#[test]
fn b_an_ordered_pick_and_a_count_visit_everything() {
    let g = corpus();
    let oldest: Vec<i64> = (0..N).filter(|&i| survives(i)).take(5).collect();
    let want: Vec<Vec<Value>> = oldest
        .iter()
        .map(|i| vec![Value::Str(format!("s-{i:04}"))])
        .collect();
    let (got, c) = traced(
        &g,
        "MATCH (s:Story) WHERE s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN s.storyId AS id ORDER BY id LIMIT 5",
    );
    assert_eq!(got, want);
    assert_eq!(count_of(&c, NEWEST), 0, "{c:?}");
    let total = (0..N).filter(|&i| survives(i)).count() as i64;
    let (got, c) = traced(
        &g,
        "MATCH (s:Story) WHERE s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN count(s) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(total)]]);
    assert_eq!(count_of(&c, NEWEST), 0, "{c:?}");
}
