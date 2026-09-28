#![allow(non_snake_case)]
//! Fix 107: a hop aggregate whose grouping keys and aggregate arguments all
//! read ONE var folds its rows per DISTINCT id of that var, each id weighted
//! by the rows it stands for; and several primitive keys group on a native
//! key instead of a serialised tuple.
//!
//! The production MENTIONS top-30 (`MATCH (n:UserDataNode {userId: $u})-
//! [:MENTIONS]->(e) RETURN e.name, coalesce(e.type, 'unknown'), count(*) …
//! LIMIT 30`) reduces 95k rows over 37k distinct ends: every row cloned its
//! key string, looked it up among 37k, and — with two keys — built and
//! serialised a value tuple; 110 ms of the shape's 125 on the hop-listing
//! bench once its columns came from the cache.
//!
//! The rows are pinned against hand-computed expectations; the folds that
//! cannot be exact (`collect`, `sum`, an argument over the other var) keep
//! the per-row fold and answer the same rows.

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const FOLDED: &str = "interp.pipeline reduce folded its rows per distinct id";
const MULTIKEY: &str = "interp.pipeline aggregate native multi-key group-by";
const RUNS: &str = "interp.pipeline aggregate runs";

const HEAD: &str = "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e)";
const ENDS: i64 = 150;
const SEEDS: i64 = 300;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("user-1".into()));
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

fn entity_type(e: i64) -> Option<&'static str> {
    (e % 7 != 0).then(|| ["person", "organization", "place", "topic"][(e % 4) as usize])
}

fn name_of(e: i64) -> String {
    format!("Entity {e}")
}

/// Each seed mentions two ends; a fifth of the seeds' first edge lands on
/// the first twenty — 600 rows over 150 ends.
fn targets(i: i64) -> [i64; 2] {
    let first = if i % 5 == 0 {
        (i * 31) % 20
    } else {
        (i * 7) % ENDS
    };
    [first, (i * 7 + 13) % ENDS]
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let ddl =
        parse_any("CREATE INDEX ud_user FOR (n:UserDataNode) ON (n.userId)").expect("parse ddl");
    run_stmt(&g, &ddl, BTreeMap::new()).expect("index");
    let mut ends = Vec::new();
    for e in 0..ENDS {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(name_of(e)));
        if let Some(t) = entity_type(e) {
            m.insert("type".to_string(), Value::Str(t.into()));
        }
        m.insert("mentions".to_string(), Value::Int(e % 50));
        ends.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..SEEDS {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("user-1".into()));
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("nodeId".to_string(), Value::Str(format!("node-{i:04}")));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("seed");
        for e in targets(i) {
            g.create_rel(n, "MENTIONS", ends[e as usize], &BTreeMap::new())
                .expect("mentions");
        }
    }
    let mut other = BTreeMap::new();
    other.insert("userId".to_string(), Value::Str("user-2".into()));
    other.insert("nodeType".to_string(), Value::Str("email".into()));
    let o = g
        .create_node(&["UserDataNode".into()], &other)
        .expect("other");
    g.create_rel(o, "MENTIONS", ends[0], &BTreeMap::new())
        .expect("other mentions");
    g
}

/// Per end: the rows it stands for and the seeds that mention it.
type EndInfo = (i64, BTreeSet<i64>);

fn per_end() -> BTreeMap<i64, EndInfo> {
    let mut m: BTreeMap<i64, EndInfo> = BTreeMap::new();
    for i in 0..SEEDS {
        for e in targets(i) {
            let ent = m.entry(e).or_insert((0, BTreeSet::new()));
            ent.0 += 1;
            ent.1.insert(i);
        }
    }
    m
}

fn top30_by_name_and_type() -> Vec<Vec<Value>> {
    let mut all: Vec<((String, String), i64)> = per_end()
        .into_iter()
        .map(|(e, (mult, _))| {
            (
                (name_of(e), entity_type(e).unwrap_or("unknown").to_string()),
                mult,
            )
        })
        .collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
    all.into_iter()
        .take(30)
        .map(|((name, ty), cnt)| vec![Value::Str(name), Value::Str(ty), Value::Int(cnt)])
        .collect()
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// The production shape (two keys, one a `coalesce`) and its one-key form
/// answer the hand-computed top 30 with the rows folded per end; the two
/// keys group on a native key.
#[test]
fn a_the_two_key_aggregate_folds_per_end_and_groups_on_a_native_key() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, count(*) AS cnt ORDER BY cnt DESC, name ASC LIMIT toInteger(30)"
        ),
    );
    assert_eq!(got, top30_by_name_and_type());
    assert_eq!(count_of(&c, RUNS), 1, "{c:?}");
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(count_of(&c, MULTIKEY), 1, "{c:?}");
    let want: Vec<Vec<Value>> = top30_by_name_and_type()
        .into_iter()
        .map(|r| vec![r[0].clone(), r[2].clone()])
        .collect();
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} RETURN e.name AS name, count(*) AS cnt ORDER BY cnt DESC, name ASC LIMIT toInteger(30)"
        ),
    );
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(count_of(&c, MULTIKEY), 0, "{c:?}");
}

/// Every site that folds a multiplicity — `count(*)`, a non-DISTINCT
/// `count`, a DISTINCT count, `min`, `max` — answers the hand-computed
/// values per type group, the null type included.
#[test]
fn b_counts_distincts_and_extrema_fold_exactly() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        &format!(
            "{HEAD} RETURN e.type AS t, count(*) AS c, count(e.type) AS ct, count(DISTINCT e.name) AS dn, min(e.name) AS lo, max(e.name) AS hi"
        ),
    );
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    let mut want: BTreeMap<String, (i64, i64, i64, String, String)> = BTreeMap::new();
    for (e, (mult, _)) in per_end() {
        let t = entity_type(e).map(str::to_string);
        let key = t.clone().unwrap_or_default();
        let name = name_of(e);
        let ent = want
            .entry(key)
            .or_insert((0, 0, 0, name.clone(), name.clone()));
        ent.0 += mult;
        if t.is_some() {
            ent.1 += mult;
        }
        ent.2 += 1;
        if name < ent.3 {
            ent.3 = name.clone();
        }
        if name > ent.4 {
            ent.4 = name;
        }
    }
    assert_eq!(got.len(), want.len(), "{got:?}");
    for r in &got {
        let key = match &r[0] {
            Value::Str(t) => t.clone(),
            Value::Null => String::new(),
            other => panic!("type key {other:?}"),
        };
        let w = &want[&key];
        assert_eq!(
            &r[1..],
            &[
                Value::Int(w.0),
                Value::Int(w.1),
                Value::Int(w.2),
                s(&w.3),
                s(&w.4)
            ],
            "group {key:?}"
        );
    }
}

/// A DISTINCT `collect` folds once per end in the ends' first-seen order —
/// the order the per-row fold gives it (the control reads the other var,
/// so it keeps the per-row fold on the same path).
#[test]
fn c_collect_distinct_folds_in_first_seen_order() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN e.type AS t, collect(DISTINCT e.name) AS names"),
    );
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    let (control, c) = traced(
        &g,
        &format!(
            "{HEAD} RETURN e.type AS t, collect(DISTINCT e.name) AS names, count(DISTINCT n) AS k"
        ),
    );
    assert_eq!(count_of(&c, FOLDED), 0, "{c:?}");
    assert_eq!(count_of(&c, RUNS), 1, "{c:?}");
    let control: Vec<Vec<Value>> = control
        .into_iter()
        .map(|r| vec![r[0].clone(), r[1].clone()])
        .collect();
    assert_eq!(got, control);
}

/// CONTROLS: an argument over the OTHER var, a non-DISTINCT `collect` and a
/// `sum` keep the per-row fold and answer the hand-computed rows.
#[test]
fn d_declines_keep_the_per_row_fold_and_the_rows() {
    let g = corpus();
    let ends = per_end();
    let by_name = |f: &dyn Fn(i64, &EndInfo) -> Value| -> Vec<Vec<Value>> {
        let mut v: Vec<(String, Value)> = ends
            .iter()
            .map(|(&e, ent)| (name_of(e), f(e, ent)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter().map(|(n, x)| vec![Value::Str(n), x]).collect()
    };
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN e.name AS name, count(DISTINCT n) AS k ORDER BY name"),
    );
    assert_eq!(count_of(&c, FOLDED), 0, "{c:?}");
    assert_eq!(got, by_name(&|_, ent| Value::Int(ent.1.len() as i64)));
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN e.name AS name, collect(e.type) AS types ORDER BY name"),
    );
    assert_eq!(count_of(&c, FOLDED), 0, "{c:?}");
    assert_eq!(
        got,
        by_name(&|e, ent| Value::List(
            (entity_type(e)
                .map(|t| vec![s(t); ent.0 as usize])
                .unwrap_or_default())
            .into()
        ))
    );
    let (got, c) = traced(
        &g,
        &format!("{HEAD} RETURN e.name AS name, sum(e.mentions) AS total ORDER BY name"),
    );
    assert_eq!(count_of(&c, FOLDED), 0, "{c:?}");
    assert_eq!(got, by_name(&|e, ent| Value::Int((e % 50) * ent.0)));
}
