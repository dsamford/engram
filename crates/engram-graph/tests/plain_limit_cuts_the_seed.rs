#![allow(non_snake_case)]
//! Fix 52: a PLAIN limit (no ORDER BY, no DISTINCT, no aggregate) over a
//! lone, hop-less, map-less, WHERE-less one-label MATCH needs only the first
//! `skip + limit` members of the label — every start is exactly one row for
//! the breaker. The projector already stopped the producer at that cap, but
//! the seed's BATCH path assembled the whole label's columns before the
//! first row reached it, and the column-bound start read whole columns for
//! a handful of rows: `MATCH (s:NewsStory) WITH s LIMIT 200 WITH s WHERE …
//! RETURN count(*)` grew the mirror's pod by 1.8–5.2 GB per statement.
//!
//! Every answer is checked against the values the fixture fixes (the first
//! members in id order are the first created) and against the same statement
//! with the columnar paths off.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("k".to_string(), Value::Int(200));
    p.insert("off".to_string(), Value::Int(10));
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

const CUT: &str = "interp.seed scan cut at the plain limit";
/// The columnar projection recogniser claims the `RETURN … LIMIT k` spelling
/// and cuts its walk the same way.
const PROJ_CUT: &str = "interp.columnar projection walk cut at the plain limit";
const GETS: &str = "store.gets";

/// 20,000 fat stories (a 4 KB body each) — above every batch threshold.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..20_000i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("s-{i:05}")));
        m.insert("topic".to_string(), Value::Str(format!("t{}", i % 7)));
        m.insert(
            "status".to_string(),
            Value::Str(if i % 9 == 0 { "stale" } else { "live" }.to_string()),
        );
        m.insert("content".to_string(), Value::Str("y".repeat(4_000)));
        g.create_node(&["Story".into()], &m).expect("node");
    }
    g
}

#[test]
fn a_plain_limit_over_a_lone_label_scan_reads_only_its_first_members() {
    let g = corpus();
    for (src, want, cap) in [
        (
            "MATCH (s:Story) WITH s LIMIT 200 WITH s WHERE s.topic = 't3' AND s.status <> 'stale' RETURN count(*) AS n",
            // Of s-00000..s-00199: topic t3 ⇔ i % 7 == 3 (29 of them: 3, 10, …, 199 → 29); stale ⇔ i % 9 == 0;
            // both ⇔ i ≡ 3 (mod 7) and i ≡ 0 (mod 9) ⇔ i ≡ 45 (mod 63): 45, 108, 171 → 3 stale → 26 live.
            vec![vec![Value::Int(26)]],
            200,
        ),
        (
            "MATCH (s:Story) WITH s LIMIT toInteger($k) RETURN count(*) AS n",
            vec![vec![Value::Int(200)]],
            200,
        ),
        (
            "MATCH (s:Story) RETURN s.storyId AS id LIMIT 3",
            vec![
                vec![Value::Str("s-00000".into())],
                vec![Value::Str("s-00001".into())],
                vec![Value::Str("s-00002".into())],
            ],
            3,
        ),
        (
            "MATCH (s:Story) WITH s SKIP toInteger($off) LIMIT 2 RETURN s.storyId AS id",
            vec![vec![Value::Str("s-00010".into())], vec![Value::Str("s-00011".into())]],
            12,
        ),
        (
            "MATCH (s:Story) WITH s LIMIT 5 RETURN properties(s) AS s",
            rows(&Graph::new(Store::new(), Realm(1), Namespace(1)), "RETURN 1 LIMIT 0"), // placeholder, checked below
            5,
        ),
    ] {
        let (got, c) = traced(&g, src);
        if !want.is_empty() {
            assert_eq!(got, want, "`{src}`");
        }
        assert_eq!(got, general(&g, src), "general path: `{src}`");
        assert!(
            count_of(&c, CUT) + count_of(&c, PROJ_CUT) > 0,
            "`{src}` cuts the seed (or the projection's walk): {c:?}"
        );
        assert!(
            count_of(&c, GETS) <= cap as u64 + 4,
            "`{src}` reads its {cap} starts, not the label: {c:?}"
        );
    }
    // The bare `properties(s)` page: five full records, every property.
    let got = rows(&g, "MATCH (s:Story) WITH s LIMIT 5 RETURN properties(s) AS s");
    assert_eq!(got.len(), 5);
    let Value::Map(m) = &got[4][0] else {
        panic!("{:?}", got[4][0]);
    };
    assert_eq!(m.get("storyId"), Some(&Value::Str("s-00004".into())));
    assert_eq!(m.get("content").map(|v| matches!(v, Value::Str(s) if s.len() == 4_000)), Some(true));
}

/// CONTROLS: an ORDER BY (a top-k, not a plain limit), an aggregate, a WHERE
/// on the MATCH, an inline map, a two-label start, a hop and an UNWIND ahead
/// of the MATCH each keep the whole-label seed — same rows, no cut.
#[test]
fn ordered_aggregating_filtered_mapped_multi_label_hopped_and_fed_scans_are_not_cut() {
    let g = corpus();
    let mut u = BTreeMap::new();
    u.insert("k".to_string(), Value::Int(1));
    let tag = g.create_node(&["Tag".into()], &u).expect("tag");
    for i in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("z-{i}")));
        let s = g.create_node(&["Story".into(), "Pinned".into()], &m).expect("pinned");
        g.create_rel(s, "TAGGED", tag, &BTreeMap::new()).expect("rel");
    }
    for src in [
        "MATCH (s:Story) WITH s ORDER BY s.storyId DESC LIMIT 3 RETURN s.storyId AS id",
        "MATCH (s:Story) WITH count(s) AS n LIMIT 1 RETURN n",
        "MATCH (s:Story) WHERE s.topic = 't3' WITH s LIMIT 3 RETURN s.storyId AS id ORDER BY id",
        "MATCH (s:Story {topic: 't3'}) WITH s LIMIT 3 RETURN s.storyId AS id ORDER BY id",
        "MATCH (s:Story:Pinned) WITH s LIMIT 2 RETURN s.storyId AS id ORDER BY id",
        "MATCH (s:Story)-[:TAGGED]->(t:Tag) WITH s LIMIT 2 RETURN s.storyId AS id ORDER BY id",
        "UNWIND [1, 2] AS x MATCH (s:Story) WITH x, s LIMIT 3 RETURN x, s.storyId AS id ORDER BY x, id",
        "MATCH (s:Story) WITH DISTINCT s.topic AS t LIMIT 3 RETURN t ORDER BY t",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(
            count_of(&c, CUT) + count_of(&c, PROJ_CUT),
            0,
            "`{src}` is left as written: {c:?}"
        );
    }
}
