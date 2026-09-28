#![allow(non_snake_case)]
//! Fix 110: `x IN $list` and `x IN xs` test the held list in place — the
//! evaluator cloned the whole list for every evaluation.
//!
//! The production NOT-IN story pick (`… AND NOT s.storyId IN $existingIds
//! … LIMIT 5`) cloned its `$existingIds` for each candidate: on the mirror
//! a 3,000-id list added 73 ms to an 85 ms statement, ~24 µs per id.
//!
//! The rows are pinned against hand-computed answers under the
//! three-valued rule (a null needle, a null in the list, an empty list); a
//! list built by the statement (`collect`) is tested the same way.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const IN_PLACE: &str = "cypher.IN tested a held list in place";

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, params));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// Forty stories, `storyId` "s-00".."s-39", `topic` crime/business by
/// parity, `rank` 0..39; three carry no storyId.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..40i64 {
        let mut m = BTreeMap::new();
        if i % 13 != 12 {
            m.insert("storyId".to_string(), Value::Str(format!("s-{i:02}")));
        }
        m.insert(
            "topic".to_string(),
            Value::Str(if i % 2 == 0 {
                "crime".into()
            } else {
                "business".into()
            }),
        );
        m.insert("rank".to_string(), Value::Int(i));
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    g
}

fn ids(v: &[&str]) -> Value {
    Value::List(v.iter().map(|x| s(x)).collect::<Vec<_>>().into())
}

fn p(list: Value) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("existingIds".to_string(), list);
    p.insert("topic".to_string(), s("crime"));
    p
}

/// The production pick over a parameter list: the excluded ids are gone,
/// a story without a storyId is Unknown (dropped by WHERE), and the list is
/// tested in place per candidate.
#[test]
fn a_a_param_list_is_tested_in_place_with_the_three_valued_rule() {
    let g = corpus();
    let src = "MATCH (st:NewsStory) WHERE st.topic = $topic AND NOT st.storyId IN $existingIds RETURN st.storyId AS id ORDER BY st.rank";
    let (got, c) = traced(&g, src, p(ids(&["s-00", "s-04", "s-38", "nope"])));
    let want: Vec<Vec<Value>> = (0..40i64)
        .filter(|i| i % 2 == 0 && i % 13 != 12 && ![0, 4, 38].contains(i))
        .map(|i| vec![Value::Str(format!("s-{i:02}"))])
        .collect();
    assert_eq!(got, want);
    assert!(count_of(&c, IN_PLACE) >= 20, "{c:?}");
    // A null in the list: a needle not found is Unknown, so NOT … IN drops
    // every candidate but the ones found are still excluded — nothing stays.
    let (got, _) = traced(
        &g,
        src,
        p(Value::List((vec![s("s-02"), Value::Null]).into())),
    );
    assert_eq!(got, Vec::<Vec<Value>>::new());
    // An empty list keeps every crime story — a null needle against `[]`
    // is false, not Unknown, so NOT keeps the stories without a storyId too.
    let (got, _) = traced(&g, src, p(Value::List((Vec::new()).into())));
    assert_eq!(got.len(), 20);
    // A null list is Unknown for every candidate.
    let (got, _) = traced(&g, src, p(Value::Null));
    assert_eq!(got, Vec::<Vec<Value>>::new());
}

/// A list the statement built (`collect`) is tested in place too; an
/// unknown parameter still raises.
#[test]
fn b_a_collected_list_is_tested_in_place_and_an_unknown_param_raises() {
    let g = corpus();
    let src = "MATCH (b:NewsStory {topic: 'business'}) WITH collect(b.rank) AS ranks MATCH (st:NewsStory) WHERE st.rank + 1 IN ranks RETURN count(st) AS n";
    let (got, c) = traced(&g, src, BTreeMap::new());
    assert_eq!(got, vec![vec![Value::Int(20)]]);
    assert!(count_of(&c, IN_PLACE) >= 40, "{c:?}");
    let q =
        parse_statement("MATCH (st:NewsStory) WHERE st.storyId IN $missing RETURN count(st) AS n")
            .expect("parse");
    let err = run_query(&g, &q, BTreeMap::new()).expect_err("an unknown parameter raises");
    assert!(format!("{err}").contains("missing"), "{err}");
}
