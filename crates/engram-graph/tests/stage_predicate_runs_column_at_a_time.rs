#![allow(non_snake_case)]
//! Fix 84: the columnar STAGE (a WITH chain over one scanned label, ordered
//! and paged at its breaker) bound a scope and walked its predicate per
//! member. The inbox page (`MATCH (n:UserDataNode {nodeType: 'email',
//! userId: $userId}) WHERE n.classified = true AND (n.abuseStatus IS NULL OR
//! n.abuseStatus IN ['clean', 'approved']) WITH n ORDER BY n.createdAt DESC
//! SKIP … LIMIT 1000 …`) evaluated every one of its user's 18k emails per
//! page — 92k expressions, 154 ms on the mirror against Neo4j's 107. Now a
//! one-label stage whose predicate reads only value / presence columns is
//! judged column-at-a-time over the walk's members (the projection scan's
//! evaluator since fix 40), and only the survivors are bound.
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
    p.insert("userId".to_string(), Value::Str("u-main".into()));
    p.insert("skip".to_string(), Value::Int(0));
    p.insert("pageSize".to_string(), Value::Int(100));
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

const STAGES: &str = "interp.columnar stages";
const VECTOR: &str = "interp.columnar stage predicate evaluated column-at-a-time";
const EXPRS: &str = "cypher.expressions evaluated";

/// 6,000 emails, 5,500 of them one user's (the stage's case: a start no
/// selective seek answers); one in eight unclassified, one in ten
/// quarantined, `createdAt` spread over a year.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert(
            "userId".to_string(),
            Value::Str(if i % 12 == 0 {
                format!("u-{}", i % 5)
            } else {
                "u-main".into()
            }),
        );
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        if i % 8 != 0 {
            m.insert("classified".to_string(), Value::Bool(true));
        }
        if i % 10 == 0 {
            m.insert("abuseStatus".to_string(), Value::Str("quarantined".into()));
        } else if i % 10 == 1 {
            m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
        }
        m.insert(
            "createdAt".to_string(),
            Value::Str(format!(
                "2026-{:02}-{:02}T{:02}:{:02}:00Z",
                1 + (i % 12),
                1 + (i % 28),
                i % 24,
                i % 60
            )),
        );
        m.insert("score".to_string(), Value::Float((i % 100) as f64 / 100.0));
        m.insert("body".to_string(), Value::Str("b".repeat(200)));
        g.create_node(&["UserDataNode".into()], &m).expect("email");
    }
    g
}

const PAGE: &str = "MATCH (n:UserDataNode {nodeType: 'email', userId: $userId}) \
    WHERE n.classified = true AND (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) \
    WITH n ORDER BY n.createdAt DESC SKIP toInteger($skip) LIMIT toInteger($pageSize) \
    RETURN n.nodeId AS nodeId";

/// The page's predicate is judged column-at-a-time: the same hundred rows,
/// a few hundred expressions where the per-member walk evaluated one per
/// email.
#[test]
fn a_the_inbox_pages_predicate_runs_column_at_a_time() {
    let g = corpus();
    let want = general(&g, PAGE);
    assert_eq!(want.len(), 100, "fixture");
    let _ = rows(&g, PAGE); // the first run assembles and keeps the columns
    let (got, c) = traced(&g, PAGE);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, STAGES), 1, "{c:?}");
    assert_eq!(count_of(&c, VECTOR), 1, "{c:?}");
    // Two evaluations per SURVIVOR (its carried item and its order key)
    // and the page's items — the predicate itself walked for nobody. A
    // per-member walk would add one per email (6,000).
    let survivors = match general(
        &g,
        "MATCH (n:UserDataNode {nodeType: 'email', userId: $userId}) \
         WHERE n.classified = true AND (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) \
         RETURN count(n) AS n",
    )[0][0]
    {
        Value::Int(n) => n as u64,
        ref other => panic!("{other:?}"),
    };
    assert!((3_000..5_000).contains(&survivors), "fixture: {survivors}");
    let exprs = count_of(&c, EXPRS);
    assert!(
        exprs <= 2 * survivors + 300,
        "{exprs} expressions for {survivors} survivors: {c:?}"
    );
}

/// An aggregating breaker over the same predicate folds the survivors alone.
#[test]
fn b_an_aggregating_stage_folds_the_survivors_alone() {
    let g = corpus();
    // A WITH chain into an aggregating breaker — the stage's shape (a single
    // aggregating WITH is the pipeline aggregate's).
    let src = "MATCH (n:UserDataNode {nodeType: 'email'}) \
        WHERE n.classified = true AND n.abuseStatus IS NULL \
        WITH n.userId AS u WITH u, count(*) AS c RETURN u, c ORDER BY u";
    let want = general(&g, src);
    assert!(want.len() >= 5, "fixture: {want:?}");
    let _ = rows(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, STAGES), 1, "{c:?}");
    assert_eq!(count_of(&c, VECTOR), 1, "{c:?}");
}

/// CONTROLS: a predicate with a pattern probe (a read beyond the columns)
/// keeps the per-member walk, and a stage without a predicate has nothing
/// to judge; both answer as the general path does.
#[test]
fn c_a_probing_predicate_and_no_predicate_keep_the_walk() {
    let g = corpus();
    for src in [
        "MATCH (n:UserDataNode {nodeType: 'email', userId: $userId}) \
         WHERE n.classified = true AND NOT EXISTS { (n)-[:HAS_ASK]->() } \
         WITH n ORDER BY n.createdAt DESC LIMIT 20 RETURN n.nodeId AS nodeId",
        // No map either: an inline map is a predicate the stage judges.
        "MATCH (n:UserDataNode) WITH n ORDER BY n.createdAt DESC LIMIT 20 RETURN n.nodeId AS nodeId",
    ] {
        let want = general(&g, src);
        assert_eq!(want.len(), 20, "fixture `{src}`");
        let _ = rows(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, VECTOR), 0, "`{src}`: {c:?}");
    }
}

/// A non-boolean predicate is the general path's error on both paths.
#[test]
fn d_a_non_boolean_predicate_errors_as_the_general_path_does() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode) WHERE n.nodeId \
        WITH n ORDER BY n.createdAt DESC LIMIT 20 RETURN n.nodeId AS nodeId";
    let q = parse_statement(src).expect("parse");
    g.set_columnar_scans(false);
    let want = run_query(&g, &q, params()).expect_err("the general path refuses a string WHERE");
    g.set_columnar_scans(true);
    let _ = run_query(&g, &q, params());
    let got = run_query(&g, &q, params()).expect_err("the columnar stage refuses it too");
    assert_eq!(format!("{got:?}"), format!("{want:?}"));
}
