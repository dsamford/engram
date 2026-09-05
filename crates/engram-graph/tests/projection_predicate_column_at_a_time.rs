#![allow(non_snake_case)]
//! Fix 40: the columnar projection's predicate over ONE label whose columns
//! are cached is evaluated COLUMN-AT-A-TIME, in chunks, stopping at the
//! k-th survivor of a bare LIMIT — no scope bound and no expression walked
//! per member. `MATCH (s:NewsStory) WHERE s.primaryTopic = $t AND s.status
//! <> 'stale' AND s.lastUpdatedAt > $cutoff AND NOT s.storyId IN
//! $existingIds RETURN … LIMIT 5` evaluated ~20k members at ~1 µs each with
//! every column served from the cache: 153–183 ms on the mirror against
//! Neo4j's 3–4, which exits after its first five matches.
//!
//! Every answer is checked against the general path (columnar paths off);
//! the counters are read on the SECOND run, after the first has assembled
//! and kept the columns.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const VECTOR: &str = "interp.columnar projection predicate evaluated column-at-a-time";
const STOPPED: &str = "interp.columnar projection stopped at the limit";
const EXPR: &str = "cypher.expressions evaluated";

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

type Rows = Result<Vec<Vec<Value>>, String>;

fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params()).map(|r| r.rows).map_err(|e| e.to_string())
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

/// 5,000 stories: 98% carry the topic, a tenth are stale, the RECENT ones
/// (past the cutoff) are the last 8% in id order plus every 97th — where
/// the mirror has them; `summary` is present on two thirds; `archivedAt`
/// is never written.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
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
        m.insert("title".to_string(), Value::Str(format!("Story {i}")));
        if i % 3 != 0 {
            m.insert("summary".to_string(), Value::Str("s".repeat(40)));
        }
        m.insert("score".to_string(), Value::Float((i % 100) as f64 / 100.0));
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    g
}

const PRED: &str =
    "s.primaryTopic = $t AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff";

/// The answer equals the general path's and the second run was vectorised.
fn check(g: &Graph, src: &str) -> BTreeMap<String, u64> {
    let want = general(g, src).unwrap_or_else(|e| panic!("general `{src}`: {e}"));
    let first = rows(g, src).unwrap_or_else(|e| panic!("first `{src}`: {e}"));
    assert_eq!(first, want, "first run `{src}`");
    let (got, c) = traced(g, src);
    assert_eq!(got.unwrap(), want, "second run `{src}`");
    c
}

#[test]
fn a_bare_limit_stops_at_its_kth_survivor_without_a_scope_per_member() {
    let g = corpus();
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId LIMIT 5");
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    assert!(count_of(&c, STOPPED) > 0, "{c:?}");
    // The five rows' items and the statement's constants — not a member.
    assert!(count_of(&c, EXPR) < 200, "{c:?}");
}

#[test]
fn skip_and_limit_together_cap_the_survivors() {
    let g = corpus();
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId SKIP 3 LIMIT 4");
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
}

#[test]
fn an_ordered_limit_evaluates_the_whole_population_column_at_a_time() {
    let g = corpus();
    let src = format!(
        "MATCH (s:NewsStory) WHERE {PRED} RETURN s.storyId AS storyId ORDER BY s.lastUpdatedAt DESC, storyId LIMIT 5"
    );
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    assert_eq!(count_of(&c, STOPPED), 0, "{c:?}");
}

#[test]
fn a_constant_string_list_is_hashed_and_the_answer_is_unchanged() {
    let g = corpus();
    let src = format!(
        "MATCH (s:NewsStory) WHERE {PRED} AND NOT s.storyId IN $existingIds RETURN s.storyId AS storyId, s.title AS title LIMIT 5"
    );
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    // A null needle against the list, and a needle that is in it.
    let src2 = "MATCH (s:NewsStory) WHERE s.summary IN $existingIds OR s.storyId IN $existingIds RETURN s.storyId AS storyId ORDER BY storyId";
    let c2 = check(&g, src2);
    assert!(count_of(&c2, VECTOR) > 0, "{c2:?}");
}

#[test]
fn presence_conjuncts_read_the_presence_columns() {
    let g = corpus();
    let src = format!(
        "MATCH (s:NewsStory) WHERE {PRED} AND s.archivedAt IS NULL AND s.summary IS NOT NULL RETURN s.storyId AS storyId LIMIT 7"
    );
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
}

#[test]
fn a_single_phase_projection_binds_only_the_survivors_from_its_walk() {
    let g = corpus();
    // The items read nothing beyond the predicate: one walk binds both.
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN s.primaryTopic AS t, s.status AS st LIMIT 5");
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    assert!(count_of(&c, EXPR) < 200, "{c:?}");
}

#[test]
fn distinct_and_a_predicate_less_scan_are_unchanged() {
    let g = corpus();
    let src = format!("MATCH (s:NewsStory) WHERE {PRED} RETURN DISTINCT s.status AS st");
    let c = check(&g, &src);
    assert!(count_of(&c, VECTOR) > 0, "{c:?}");
    let src2 = "MATCH (s:NewsStory) RETURN s.storyId AS storyId LIMIT 3";
    let c2 = check(&g, src2);
    assert_eq!(count_of(&c2, VECTOR), 0, "{c2:?}");
}

/// CONTROL: a predicate the vectoriser declines (non-constant arithmetic)
/// keeps the per-member walk — the columnar projection still runs it, with
/// its early exit — and answers as the general path does.
#[test]
fn a_predicate_the_vectoriser_declines_keeps_the_walk() {
    let g = corpus();
    let src = "MATCH (s:NewsStory) WHERE s.primaryTopic = $t AND s.score + 0.5 > 1.2 RETURN s.storyId AS storyId LIMIT 5";
    let c = check(&g, src);
    assert_eq!(count_of(&c, VECTOR), 0, "{c:?}");
    assert!(count_of(&c, "interp.columnar projection scans") > 0, "{c:?}");
    assert!(count_of(&c, STOPPED) > 0, "{c:?}");
}

/// A non-boolean predicate is the general path's error on both paths.
#[test]
fn a_non_boolean_predicate_errors_as_the_general_path_does() {
    let g = corpus();
    let src = "MATCH (s:NewsStory) WHERE s.title RETURN s.storyId AS storyId LIMIT 5";
    let want = general(&g, src).expect_err("the general path refuses a string WHERE");
    let _ = rows(&g, src); // warms the columns
    let got = rows(&g, src).expect_err("the columnar path refuses it too");
    assert_eq!(got, want);
}
