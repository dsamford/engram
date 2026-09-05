#![allow(non_snake_case)]
//! Fix 80: a bare NODE group key of an aggregating WITH is carried LEAN
//! through the stage's hops and hydrated once per group at the RETURN —
//! for any concluding RETURN that outputs it whole and otherwise reads it
//! by property, not only a top-k one. Lever G' (fixes 27/31) hydrated the
//! k survivors of a top-k; after an aggregation every output row IS a
//! group, so hydrating each is the one full read per group the seed paid
//! anyway, while the hops between no longer copy the whole node per
//! emitted row.
//!
//! The production KMProject dashboard's base (`MATCH (p:KMProject) WHERE
//! true OPTIONAL MATCH (:User {userId: $u})-[mm:MEMBER_OF]->(p) OPTIONAL
//! MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, mm,
//! max(w.updatedAt) AS lastItemAt RETURN properties(p) AS p, coalesce(
//! mm.role, 'owner') AS myRole, lastItemAt ORDER BY lastItemAt DESC`, no
//! LIMIT) carried 77 fat projects through 1,338 work-item rows: 14.6 ms
//! against Neo4j's 6.1 on the mirror; locally the same statement with
//! `p.id` in place of `properties(p)` ran in half.
//!
//! Every answer is checked against the same statement with the late
//! projection OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".into(), s("u-3"));
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

fn control(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_late_projection(false);
    let r = rows(g, src);
    g.set_late_projection(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const LEAN_AFTER_AGG: &str = "interp.breaker bound a bare group key lean for the RETURN after its aggregation";
const LEAN_TOPK: &str = "interp.breaker bound a bare carry lean for the RETURN's top-k";
const HYDRATED_EAGERLY: &str = "interp.late full carry hydrated eagerly";
const HYDRATED_SURVIVOR: &str = "interp.late projection re-materialised a carried node for a survivor";
const FULL: &str = "graph.nodes materialised in full";

/// 77 projects with a fat metadata string and twenty fields, 1,338 work
/// items over them, ten users owning memberships.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut users = Vec::new();
    for ui in 0..10i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".into(), s(&format!("u-{ui}")));
        users.push(g.create_node(&["User".into()], &m).expect("user"));
    }
    let mut projects = Vec::new();
    for pi in 0..77i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("proj-{pi:02}")));
        m.insert("name".into(), s(&format!("Project {pi}")));
        m.insert("metadata".into(), s(&"{\"k\": \"v\", \"desc\": \"lorem ipsum dolor sit amet\"}".repeat(60)));
        for k in 0..20 {
            m.insert(format!("f{k}"), s(&format!("value {k}")));
        }
        let p = g.create_node(&["KMProject".into()], &m).expect("project");
        let mut rm = BTreeMap::new();
        rm.insert("role".into(), s(if pi % 2 == 0 { "owner" } else { "member" }));
        g.create_rel(users[(pi % 10) as usize], "MEMBER_OF", p, &rm).expect("membership");
        projects.push(p);
    }
    for i in 0..1_338i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("wi-{i}")));
        m.insert("updatedAt".into(), s(&format!("2026-08-{:02}T{:02}:00:00Z", 1 + i % 28, i % 24)));
        m.insert("content".into(), s(&"body ".repeat(200)));
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO_PROJECT", projects[(i % 77) as usize], &BTreeMap::new()).expect("rel");
    }
    g
}

const BASE: &str = "MATCH (p:KMProject) WHERE true \
    OPTIONAL MATCH (:User {userId: $userId})-[mm:MEMBER_OF]->(p) \
    OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
    WITH p, mm, max(w.updatedAt) AS lastItemAt \
    RETURN properties(p) AS p, coalesce(mm.role, 'owner') AS myRole, lastItemAt ORDER BY lastItemAt DESC";

/// The dashboard base: the group key carried lean, hydrated once per group
/// (77 full reads, one per output row), the answer byte-identical.
#[test]
fn a_the_dashboard_base_carries_its_group_key_lean_and_hydrates_per_group() {
    let g = corpus();
    let want = control(&g, BASE);
    assert_eq!(want.len(), 77);
    let (got, c) = traced(&g, BASE);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, LEAN_AFTER_AGG), 1, "{c:?}");
    assert_eq!(count_of(&c, LEAN_TOPK), 0, "not a top-k: {c:?}");
    // The projector hydrates each output row (no top-k to defer past),
    // counting the eager path once per row and the hydration once per row.
    assert_eq!(count_of(&c, HYDRATED_EAGERLY), 77, "one hydration per group: {c:?}");
    assert_eq!(count_of(&c, HYDRATED_SURVIVOR), 77, "{c:?}");
    assert_eq!(count_of(&c, FULL), 77, "the hydrations are the only full reads: {c:?}");
    // The output is the whole node's map, ordered by the aggregate.
    assert!(matches!(&got[0][0], Value::Map(m) if m.len() == 23), "{:?}", got[0][0]);
    let keys: Vec<String> = got
        .iter()
        .map(|r| match &r[2] {
            Value::Str(t) => t.clone(),
            other => panic!("lastItemAt: {other:?}"),
        })
        .collect();
    assert!(keys.windows(2).all(|w| w[0] >= w[1]), "ordered by lastItemAt DESC");
}

/// The bare spelling (`RETURN p`) and an unordered RETURN take the same
/// lever; a top-k RETURN keeps lever G' (its own counter, k hydrations).
#[test]
fn b_bare_unordered_and_topk_spellings() {
    let g = corpus();
    let bare = "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
        WITH p, max(w.updatedAt) AS lastItemAt RETURN p, lastItemAt";
    let want = control(&g, bare);
    let (got, c) = traced(&g, bare);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, LEAN_AFTER_AGG), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), 77, "{c:?}");
    assert!(got.iter().all(|r| matches!(&r[0], Value::Node { props, .. } if props.len() == 23)));

    let topk = "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
        WITH p, max(w.updatedAt) AS lastItemAt RETURN properties(p) AS p, lastItemAt \
        ORDER BY lastItemAt DESC LIMIT 5";
    let want = control(&g, topk);
    let (got, c) = traced(&g, topk);
    assert_eq!(got, want);
    assert_eq!(got.len(), 5);
    assert_eq!(count_of(&c, LEAN_AFTER_AGG), 0, "a top-k keeps lever G': {c:?}");
    assert_eq!(count_of(&c, LEAN_TOPK), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), 5, "five survivors hydrated: {c:?}");
}

/// Outside the class: a group key read WHOLE by something other than the
/// RETURN's output (a function over it), a DISTINCT RETURN, and a
/// non-aggregating breaker all keep the full carry — and agree.
#[test]
fn c_shapes_outside_the_class_keep_the_full_carry() {
    let g = corpus();
    for src in [
        "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
         WITH p, max(w.updatedAt) AS lastItemAt RETURN size(keys(p)) AS n, lastItemAt ORDER BY lastItemAt DESC",
        "MATCH (p:KMProject) OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) \
         WITH p, max(w.updatedAt) AS lastItemAt RETURN DISTINCT p, lastItemAt",
        "MATCH (p:KMProject) WITH p ORDER BY p.name RETURN properties(p) AS p",
    ] {
        let want = control(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "{src}");
        assert_eq!(count_of(&c, LEAN_AFTER_AGG), 0, "{src}: {c:?}");
    }
}
