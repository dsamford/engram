#![allow(non_snake_case)]
//! Fix 82: a CAPPED column-at-a-time scan (a bare LIMIT, no ORDER BY)
//! visits its chunks from both ends, the newest first. Ids are minted in
//! creation order, so a label's newest members sit at the end of id order,
//! and the listings that cap without ordering are recency-filtered: the
//! NewsStory topic listing (`primaryTopic = $t AND status <> 'stale' AND
//! lastUpdatedAt > $cutoff … LIMIT 5`) met its fifth match after the whole
//! label on the mirror (8 ms, every chunk evaluated) where Neo4j's scan of
//! the storyId index meets matches spread at random through UUID order in
//! under 2. A bare LIMIT wants ANY k matches: the k found come back in id
//! order, every one a row the unlimited statement answers.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("t".to_string(), Value::Str("Business and Finance".into()));
    p.insert(
        "cutoff".to_string(),
        Value::Str("2026-08-31T00:00:00.000Z".into()),
    );
    p
}

fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Rows, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str) -> Rows {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const VECTOR: &str = "interp.columnar projection predicate evaluated column-at-a-time";
const STOPPED: &str = "interp.columnar projection stopped at the limit";
const FROM_ENDS: &str =
    "interp.columnar projection scanned its chunks from both ends for the limit";

/// 5,000 stories (two chunks of 4,096): 98% carry the topic, a tenth are
/// stale, the RECENT ones (past the cutoff) are the last 8% in id order
/// plus every 97th — where the mirror has them.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n = 5000i64;
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("s-{i:05}")));
        m.insert(
            "primaryTopic".to_string(),
            Value::Str(if i % 50 == 0 {
                "Sports".into()
            } else {
                "Business and Finance".into()
            }),
        );
        m.insert(
            "status".to_string(),
            Value::Str(if i % 10 == 3 {
                "stale".into()
            } else {
                "active".into()
            }),
        );
        let recent = i >= n - n / 12 || i % 97 == 0;
        m.insert(
            "lastUpdatedAt".to_string(),
            Value::Str(if recent {
                format!(
                    "2026-09-0{}T{:02}:{:02}:00.000Z",
                    1 + (i % 4),
                    i % 24,
                    i % 60
                )
            } else {
                format!(
                    "2026-0{}-{:02}T{:02}:{:02}:00.000Z",
                    1 + (i % 8),
                    1 + (i % 28),
                    i % 24,
                    i % 60
                )
            }),
        );
        m.insert("title".to_string(), Value::Str(format!("Story {i}")));
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    g
}

const PRED: &str = "s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff";

fn story_id(r: &[Value]) -> String {
    match &r[0] {
        Value::Str(s) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// The five rows come from the NEWEST chunk — no story before id 4,096 —
/// and every one is a match; the scan stopped at its fifth survivor.
#[test]
fn a_a_bare_limit_takes_its_matches_from_the_newest_chunk() {
    let g = corpus();
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId LIMIT 5");
    let all = general(&g, &src.replace(" LIMIT 5", ""));
    assert!(all.len() > 300, "fixture: {} matches", all.len());
    let _ = rows(&g, &src); // the first run assembles the columns
    let (got, c) = traced(&g, &src);
    assert_eq!(got.len(), 5, "{got:?}");
    for r in &got {
        assert!(all.contains(r), "not a match: {r:?}");
        assert!(
            story_id(r).as_str() >= "s-04096",
            "from the older chunk: {r:?}"
        );
    }
    assert_eq!(count_of(&c, FROM_ENDS), 1, "{c:?}");
    assert_eq!(count_of(&c, STOPPED), 1, "{c:?}");
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    // In id order, and the same again.
    let ids: Vec<String> = got.iter().map(|r| story_id(r)).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(rows(&g, &src), got);
}

/// SKIP and LIMIT together: the cap is their sum, the rows the ones after
/// the skip, all of them matches.
#[test]
fn b_skip_and_limit_page_the_newest_matches() {
    let g = corpus();
    let src =
        format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId SKIP 3 LIMIT 4");
    let all = general(&g, &src.replace(" SKIP 3 LIMIT 4", ""));
    let _ = rows(&g, &src);
    let (got, c) = traced(&g, &src);
    assert_eq!(got.len(), 4, "{got:?}");
    for r in &got {
        assert!(all.contains(r), "not a match: {r:?}");
    }
    assert_eq!(count_of(&c, FROM_ENDS), 1, "{c:?}");
}

/// A cap the matches never reach returns every match, in id order — the
/// general path's whole answer, whichever chunk was visited first.
#[test]
fn c_a_cap_above_the_matches_answers_them_all_in_id_order() {
    let g = corpus();
    // No equality to seek by: the recent, non-stale stories (a few hundred).
    let src = "MATCH (s:NewsStory) WHERE s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN s.storyId AS storyId LIMIT 5000";
    let want = general(&g, src);
    assert!((300..600).contains(&want.len()), "fixture: {}", want.len());
    let _ = rows(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FROM_ENDS), 1, "{c:?}");
    assert_eq!(count_of(&c, STOPPED), 0, "{c:?}");
}

/// CONTROLS: an ORDERED limit and an uncapped scan evaluate the whole
/// population forward, as before, and answer as the general path does.
#[test]
fn d_an_ordered_limit_and_an_uncapped_scan_keep_the_forward_walk() {
    let g = corpus();
    for src in [
        format!(
            "MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId ORDER BY s.lastUpdatedAt DESC, storyId LIMIT 5"
        ),
        format!("MATCH (s:NewsStory) WHERE {PRED} RETURN DISTINCT s.status AS st"),
        format!("MATCH (s:NewsStory) WHERE {PRED} RETURN count(s) AS n"),
    ] {
        let want = general(&g, &src);
        let _ = rows(&g, &src);
        let (got, c) = traced(&g, &src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, FROM_ENDS), 0, "`{src}`: {c:?}");
    }
}

/// A label of ONE chunk has no other end to start from: the forward scan,
/// the general path's rows.
#[test]
fn e_a_single_chunk_label_scans_forward() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("storyId".to_string(), Value::Str(format!("s-{i:05}")));
        m.insert(
            "primaryTopic".to_string(),
            Value::Str("Business and Finance".into()),
        );
        m.insert("status".to_string(), Value::Str("active".into()));
        m.insert(
            "lastUpdatedAt".to_string(),
            Value::Str(if i % 7 == 0 {
                "2026-09-02T00:00:00.000Z".into()
            } else {
                "2026-03-01T00:00:00.000Z".into()
            }),
        );
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId LIMIT 5");
    let want = general(&g, &src);
    assert_eq!(want.len(), 5, "fixture");
    let _ = rows(&g, &src);
    let (got, c) = traced(&g, &src);
    assert_eq!(got, want);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    assert_eq!(count_of(&c, FROM_ENDS), 0, "{c:?}");
}
