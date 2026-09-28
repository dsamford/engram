#![allow(non_snake_case)]
//! Fix 108: an aggregate over ONE label whose predicate, grouping keys and
//! arguments read only CACHED columns folds column-at-a-time — no scope
//! bound and no expression walked per member — as the plain count already
//! did.
//!
//! The production NewsArticle classification `MATCH (a:NewsArticle) WHERE
//! a.classifiedAt IS NOT NULL AND a.pubDate >= $c AND (a.abuseStatus IS NULL
//! OR a.abuseStatus IN ['clean', 'approved']) AND a.contentType IS NOT NULL
//! RETURN a.contentType AS key, count(a)` evaluated 165k expressions over its
//! 67k survivors: 111 ms on the mirror where the same predicate's count took
//! 22.
//!
//! The rows are pinned against hand-computed groups; the cold read (the
//! walk that assembles and keeps the columns) and the warm fold answer the
//! same rows.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const FOLDED: &str = "interp.columnar aggregate folded over cached columns";
const COUNTED: &str = "interp.columnar aggregate counted over cached columns";
const EXPRS: &str = "cypher.expressions evaluated";

const N: i64 = 3000;
const CUTOFF: &str = "2026-07-15T00:00:00.000Z";

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("pubDateCutoff".to_string(), Value::Str(CUTOFF.into()));
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

fn pub_date(i: i64) -> String {
    format!(
        "2026-0{}-{:02}T{:02}:00:00.000Z",
        6 + (i % 3),
        1 + (i % 28),
        i % 24
    )
}

fn classified(i: i64) -> bool {
    i % 5 != 0
}

fn abuse(i: i64) -> Option<&'static str> {
    match i % 20 {
        0..=13 => None,
        14..=17 => Some("clean"),
        18 => Some("approved"),
        _ => Some("quarantined"),
    }
}

fn content_type(i: i64) -> Option<&'static str> {
    (i % 11 != 0).then(|| ["breaking", "analysis", "feature", "opinion"][(i % 4) as usize])
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("{i:032x}")));
        m.insert("pubDate".to_string(), Value::Str(pub_date(i)));
        m.insert("words".to_string(), Value::Int(100 + i % 900));
        if classified(i) {
            m.insert("classifiedAt".to_string(), Value::Str(pub_date(i + 1)));
        }
        if let Some(a) = abuse(i) {
            m.insert("abuseStatus".to_string(), Value::Str(a.into()));
        }
        if let Some(t) = content_type(i) {
            m.insert("contentType".to_string(), Value::Str(t.into()));
        }
        g.create_node(&["NewsArticle".into()], &m).expect("article");
    }
    g
}

fn survives(i: i64) -> bool {
    classified(i)
        && pub_date(i).as_str() >= CUTOFF
        && matches!(abuse(i), None | Some("clean") | Some("approved"))
        && content_type(i).is_some()
}

const PRED: &str = "a.classifiedAt IS NOT NULL AND a.pubDate >= $pubDateCutoff AND (a.abuseStatus IS NULL OR a.abuseStatus IN ['clean', 'approved']) AND a.contentType IS NOT NULL";

/// Groups keyed by content type in first-seen (member) order.
fn expected() -> Vec<(String, i64, i64)> {
    let mut out: Vec<(String, i64, i64)> = Vec::new();
    for i in 0..N {
        if !survives(i) {
            continue;
        }
        let t = content_type(i).unwrap().to_string();
        match out.iter_mut().find(|(k, _, _)| *k == t) {
            Some(e) => {
                e.1 += 1;
                e.2 += 100 + i % 900;
            }
            None => out.push((t, 1, 100 + i % 900)),
        }
    }
    out
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// The production shape: cold, the walk assembles the columns; warm, the
/// fold runs over them as vectors with no per-member expression, and both
/// answer the hand-computed groups in first-seen order.
#[test]
fn a_the_grouped_count_folds_over_the_cached_columns_warm() {
    let g = corpus();
    let stmt =
        format!("MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(a) AS n");
    let want: Vec<Vec<Value>> = expected()
        .into_iter()
        .map(|(k, n, _)| vec![s(&k), Value::Int(n)])
        .collect();
    let (got, c) = traced(&g, &stmt);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 0, "cold: {c:?}");
    // Warm. The property-column cache is shared by every graph of the test
    // process under one budget, so a neighbouring test can evict a column
    // between runs; the walk then keeps it again, and the fold follows.
    let mut warm = None;
    for _ in 0..3 {
        let (got, c) = traced(&g, &stmt);
        assert_eq!(got, want);
        if count_of(&c, FOLDED) == 1 {
            warm = Some(c);
            break;
        }
    }
    let c = warm.expect("the warm read folds over the cached columns");
    assert!(count_of(&c, EXPRS) < 20, "warm: {c:?}");
    // The plain count keeps its own shortcut (warm: it reads `contentType`
    // as presence alone, a column the keyed read did not keep).
    let count = format!("MATCH (a:NewsArticle) WHERE {PRED} RETURN count(a) AS n");
    let _ = rows(&g, &count);
    let (got, c) = traced(&g, &count);
    let total: i64 = expected().iter().map(|(_, n, _)| n).sum();
    assert_eq!(got, vec![vec![Value::Int(total)]]);
    assert_eq!(count_of(&c, COUNTED), 1, "{c:?}");
    assert_eq!(count_of(&c, FOLDED), 0, "{c:?}");
}

/// A value argument (`sum(a.words)`), a DISTINCT count over the key and a
/// grouping key that is an expression fold over the columns too, with the
/// hand-computed values.
#[test]
fn b_arguments_and_expression_keys_fold_over_the_columns() {
    let g = corpus();
    let stmt = format!(
        "MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, sum(a.words) AS words, count(DISTINCT a.contentType) AS d"
    );
    let want: Vec<Vec<Value>> = expected()
        .into_iter()
        .map(|(k, _, w)| vec![s(&k), Value::Int(w), Value::Int(1)])
        .collect();
    let _ = rows(&g, &stmt);
    let (got, c) = traced(&g, &stmt);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    let stmt = format!(
        "MATCH (a:NewsArticle) WHERE {PRED} RETURN coalesce(a.abuseStatus, 'none') AS key, count(*) AS n"
    );
    let mut want: Vec<(String, i64)> = Vec::new();
    for i in 0..N {
        if !survives(i) {
            continue;
        }
        let k = abuse(i).unwrap_or("none").to_string();
        match want.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 += 1,
            None => want.push((k, 1)),
        }
    }
    let want: Vec<Vec<Value>> = want
        .into_iter()
        .map(|(k, n)| vec![s(&k), Value::Int(n)])
        .collect();
    let _ = rows(&g, &stmt);
    let (got, c) = traced(&g, &stmt);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
}

/// CONTROL: an aggregate with no predicate over a label folds every member;
/// `count(DISTINCT a)` — the production spelling — answers the same groups
/// as `count(a)` whichever path takes it.
#[test]
fn c_no_predicate_and_the_distinct_spelling_answer_the_same_groups() {
    let g = corpus();
    let stmt = "MATCH (a:NewsArticle) RETURN a.contentType AS key, count(a) AS n";
    let mut want: Vec<(Option<String>, i64)> = Vec::new();
    for i in 0..N {
        let k = content_type(i).map(str::to_string);
        match want.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 += 1,
            None => want.push((k, 1)),
        }
    }
    let want: Vec<Vec<Value>> = want
        .into_iter()
        .map(|(k, n)| vec![k.map(|x| s(&x)).unwrap_or(Value::Null), Value::Int(n)])
        .collect();
    let _ = rows(&g, stmt);
    let (got, c) = traced(&g, stmt);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    let distinct = format!(
        "MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(DISTINCT a) AS n"
    );
    let plain =
        format!("MATCH (a:NewsArticle) WHERE {PRED} RETURN a.contentType AS key, count(a) AS n");
    let _ = rows(&g, &distinct);
    assert_eq!(rows(&g, &distinct), rows(&g, &plain));
}
