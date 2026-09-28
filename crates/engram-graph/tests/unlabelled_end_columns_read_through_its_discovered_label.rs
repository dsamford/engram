#![allow(non_snake_case)]
//! Fix 93: a hop end the pattern left unlabelled reads its properties
//! through the label every one of its members carries, discovered from the
//! store — the production MENTIONS aggregate gathered its 37,270 ends' two
//! properties by a record read each on every execution (277 ms against
//! Neo4j's 208 on the mirror) though every end is an `Entity` whose columns
//! the cache serves.
//!
//! The rows are pinned against a hand-computed expectation and against the
//! labelled spelling; a population with a member outside the candidates,
//! and one below the discovery floor, gather as before.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.into()));
    p
}

fn rows(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    p: &BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const DISCOVERED: &str = "interp.pipeline unlabelled var's label discovered from its members";
const FROM_COLUMN: &str = "interp.pipeline bound-var columns read from the label column";

fn type_of(k: i64) -> Option<&'static str> {
    if k % 7 == 0 {
        None
    } else {
        Some(["person", "org", "place"][(k % 3) as usize])
    }
}

/// 3,000 entities (`name`, a `type` on six in seven); user 0 mentions the
/// first 1,500 `(k % 5) + 1` times each, user 1 the first 100 the same way,
/// user 2 the first 1,500 once each AND twenty `Topic` nodes.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ents = Vec::with_capacity(3000);
    for k in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("ent-{k:05}")));
        if let Some(t) = type_of(k) {
            m.insert("type".to_string(), Value::Str(t.into()));
        }
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    let user = |g: &Graph, id: &str| {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(id.into()));
        g.create_node(&["UserDataNode".into()], &m).expect("user")
    };
    let u0 = user(&g, "user-0");
    for k in 0..1500i64 {
        for _ in 0..(k % 5) + 1 {
            g.create_rel(u0, "MENTIONS", ents[k as usize], &BTreeMap::new())
                .expect("m");
        }
    }
    let u1 = user(&g, "user-1");
    for k in 0..100i64 {
        for _ in 0..(k % 5) + 1 {
            g.create_rel(u1, "MENTIONS", ents[k as usize], &BTreeMap::new())
                .expect("m");
        }
    }
    let u2 = user(&g, "user-2");
    for k in 0..1500i64 {
        g.create_rel(u2, "MENTIONS", ents[k as usize], &BTreeMap::new())
            .expect("m");
    }
    for t in 0..20i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("topic-{t:02}")));
        let n = g.create_node(&["Topic".into()], &m).expect("topic");
        g.create_rel(u2, "MENTIONS", n, &BTreeMap::new())
            .expect("m");
    }
    g
}

const STMT: &str = "MATCH (n:UserDataNode {userId: $userId})-[:MENTIONS]->(e) \
    RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, count(*) AS cnt \
    ORDER BY cnt DESC, name LIMIT 30";

/// The thirty best rows of a user mentioning the first `n` entities
/// `(k % 5) + 1` times: the fives by name, then the fours.
fn expected(n: i64) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for want in [5i64, 4] {
        for k in 0..n {
            if (k % 5) + 1 == want && out.len() < 30 {
                out.push(vec![
                    Value::Str(format!("ent-{k:05}")),
                    Value::Str(type_of(k).unwrap_or("unknown").into()),
                    Value::Int(want),
                ]);
            }
        }
    }
    out
}

/// The unlabelled end's rows are the labelled spelling's and the
/// expectation's, read through the discovered label's columns.
#[test]
fn a_the_ends_properties_are_read_through_their_discovered_label() {
    let g = corpus();
    let want = expected(1500);
    assert_eq!(want.len(), 30, "fixture");
    let (got, c) = traced(&g, STMT, &params("user-0"));
    assert_eq!(got, want);
    assert_eq!(count_of(&c, DISCOVERED), 1, "{c:?}");
    assert!(count_of(&c, FROM_COLUMN) >= 1, "{c:?}");
    let labelled = STMT.replace("-[:MENTIONS]->(e)", "-[:MENTIONS]->(e:Entity)");
    let (got, c) = traced(&g, &labelled, &params("user-0"));
    assert_eq!(got, want);
    assert_eq!(
        count_of(&c, DISCOVERED),
        0,
        "the pattern names the label: {c:?}"
    );
}

/// CONTROLS: a population with a member outside every candidate label
/// (user 2's topics) and one below the floor (user 1) gather as before —
/// and answer the same rows.
#[test]
fn b_a_mixed_population_and_a_small_one_gather_as_before() {
    let g = corpus();
    let (got, c) = traced(&g, STMT, &params("user-2"));
    assert_eq!(got.len(), 30);
    assert!(got.iter().all(|r| r[2] == Value::Int(1)));
    assert_eq!(got[0][0], Value::Str("ent-00000".into()));
    assert_eq!(count_of(&c, DISCOVERED), 0, "{c:?}");
    let (got, c) = traced(&g, STMT, &params("user-1"));
    assert_eq!(got, expected(100));
    assert_eq!(count_of(&c, DISCOVERED), 0, "{c:?}");
}
