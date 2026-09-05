#![allow(non_snake_case)]
//! Fix 53: a WHERE operand holding a subquery, a pattern predicate or a
//! pattern comprehension is ordered AFTER the operands that hold none, and
//! the evaluator's AND / OR decide from the cheap side without running the
//! body — `false AND EXISTS {…}`, `true OR EXISTS {…}`. The production
//! viewer-visibility listings spell `(scope-test OR owner-test OR EXISTS
//! {…} OR EXISTS {…}) AND NOT w.status IN […] AND w.assigneeId = $me`, and
//! ran both membership bodies for every row: 3.4 s on the mirror against
//! Neo4j's 113 ms.
//!
//! Every answer is checked against the generating rules (never against the
//! engine) and against the same statement with the columnar paths off.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const ORDERED: &str = "interp.subquery operands ordered last";
const SKIPPED: &str = "cypher.subquery operand skipped by a decided connective";

fn params(me: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("me".to_string(), Value::Str(me.to_string()));
    p.insert("org".to_string(), Value::Str("e2e".to_string()));
    p
}

fn rows(g: &Graph, src: &str, me: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(me))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, me: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, me));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str, me: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    g.set_columnar_scans(false);
    let r = traced(g, src, me);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

/// The generating rules, shared by the fixture and every expectation.
const N: i64 = 400;
fn assignee(i: i64) -> &'static str {
    if i % 4 == 0 { "u1" } else { "u2" }
}
fn done(i: i64) -> bool {
    i % 5 == 0
}
fn org_scoped(i: i64) -> bool {
    i % 2 == 0
}
fn owner(i: i64) -> Option<&'static str> {
    (i % 7 == 0).then_some("u1")
}
fn project(i: i64) -> i64 {
    i % 3
}
/// `u1` is an ACTIVE member of project 0, `u2` of project 1 (no state — the
/// default is active) and a LEFT member of project 2.
fn member(me: &str, p: i64) -> bool {
    matches!((me, p), ("u1", 0) | ("u2", 1))
}

/// 400 items over three projects and two people; a fourth project nobody
/// belongs to keeps the membership test honest.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut projects = Vec::new();
    for p in 0..4i64 {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Int(p));
        projects.push(g.create_node(&["Proj".into()], &m).expect("proj"));
    }
    let mut people = BTreeMap::new();
    for who in ["u1", "u2"] {
        let mut m = BTreeMap::new();
        m.insert("pid".to_string(), Value::Str(who.to_string()));
        people.insert(who, g.create_node(&["Person".into()], &m).expect("person"));
    }
    let mut active = BTreeMap::new();
    active.insert("state".to_string(), Value::Str("active".to_string()));
    let mut left = BTreeMap::new();
    left.insert("state".to_string(), Value::Str("left".to_string()));
    g.create_rel(people["u1"], "MEMBER", projects[0], &active).expect("m");
    g.create_rel(people["u2"], "MEMBER", projects[1], &BTreeMap::new()).expect("m");
    g.create_rel(people["u2"], "MEMBER", projects[2], &left).expect("m");
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        m.insert("assignee".to_string(), Value::Str(assignee(i).to_string()));
        m.insert(
            "status".to_string(),
            Value::Str(if done(i) { "done" } else { "open" }.to_string()),
        );
        if org_scoped(i) {
            m.insert("scope".to_string(), Value::Str("organization".to_string()));
        }
        m.insert("orgId".to_string(), Value::Str("e2e".to_string()));
        if let Some(o) = owner(i) {
            m.insert("owner".to_string(), Value::Str(o.to_string()));
        }
        let w = g.create_node(&["Item".into()], &m).expect("item");
        g.create_rel(w, "IN", projects[project(i) as usize], &BTreeMap::new())
            .expect("in");
    }
    g
}

/// The production listing's visibility rule, from the rules.
fn visible(me: &str, i: i64) -> bool {
    org_scoped(i) || owner(i) == Some(me) || member(me, project(i))
}

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

const LISTING: &str = "MATCH (w:Item) WHERE ( ( coalesce(w.scope,'') IN ['organization'] AND ($org IS NULL OR w.orgId = $org) ) OR w.owner = $me OR EXISTS { MATCH (w)-[:IN]->(:Proj)<-[m:MEMBER]-(:Person {pid: $me}) WHERE coalesce(m.state,'active') = 'active' } ) AND NOT w.status IN ['done','cancelled'] AND w.assignee = $me RETURN w.n AS n ORDER BY n";

#[test]
fn the_visibility_listing_answers_the_rules_and_skips_the_bodies_its_cheap_side_decides() {
    let g = corpus();
    for me in ["u1", "u2"] {
        let want: Vec<i64> = (0..N)
            .filter(|&i| assignee(i) == me && !done(i) && visible(me, i))
            .collect();
        assert!(!want.is_empty());
        let (got, c) = traced(&g, LISTING, me);
        assert_eq!(ints(&got), want, "{me}");
        assert!(count_of(&c, ORDERED) >= 1, "{me}: the WHERE was reordered: {c:?}");
        // Every org-scoped survivor decides the OR from its first disjunct;
        // the membership body never runs for it.
        let org_rows = want.iter().filter(|&&i| org_scoped(i)).count() as u64;
        assert!(
            count_of(&c, SKIPPED) >= org_rows,
            "{me}: {org_rows} rows decided the OR without the body: {c:?}"
        );
        let (gp,_) = general(&g, LISTING, me);
        assert_eq!(ints(&gp), want, "general path, {me}");
    }
}

/// The body written FIRST (`EXISTS {…} AND w.assignee = $me`) is still
/// evaluated last: on the general path every item reaches the WHERE, and
/// the three hundred that are not mine decide the AND without the body.
#[test]
fn a_body_written_first_is_evaluated_after_the_cheap_conjunct() {
    let g = corpus();
    let src = "MATCH (w:Item) WHERE EXISTS { MATCH (w)-[:IN]->(:Proj)<-[:MEMBER]-(:Person {pid: $me}) } AND w.assignee = $me RETURN count(w) AS n";
    let want = (0..N)
        .filter(|&i| assignee(i) == "u1" && [0, 1, 2].contains(&project(i)) && matches!(project(i), 0))
        .count() as i64;
    let (got, c) = traced(&g, src, "u1");
    assert_eq!(got, vec![vec![Value::Int(want)]]);
    assert!(count_of(&c, ORDERED) >= 1, "{c:?}");
    let (gp,cg) = general(&g, src, "u1");
    assert_eq!(gp,vec![vec![Value::Int(want)]]);
    let not_mine = (0..N).filter(|&i| assignee(i) != "u1").count() as u64;
    assert!(
        count_of(&cg, SKIPPED) >= not_mine,
        "general path: {not_mine} rows decided the AND without the body: {cg:?}"
    );
}

/// A WITH's WHERE is reordered too, and a NOT EXISTS moves like an EXISTS.
#[test]
fn a_with_where_and_a_negated_body_reorder_alike() {
    let g = corpus();
    let src = "MATCH (w:Item) WITH w WHERE NOT EXISTS { MATCH (w)-[:IN]->(:Proj)<-[:MEMBER]-(:Person {pid: $me}) } AND w.assignee = $me AND w.status = 'open' RETURN count(w) AS n";
    let want = (0..N)
        .filter(|&i| assignee(i) == "u2" && !done(i) && !matches!(project(i), 1 | 2))
        .count() as i64;
    let (got, c) = traced(&g, src, "u2");
    assert_eq!(got, vec![vec![Value::Int(want)]]);
    assert!(count_of(&c, ORDERED) >= 1, "{c:?}");
    let (gp,_) = general(&g, src, "u2");
    assert_eq!(gp,vec![vec![Value::Int(want)]]);
}

/// CONTROLS: a scalar operand is never skipped — `false AND 1` and `true OR
/// 1` raise as they always did — and a WHERE with no body is left as
/// written.
#[test]
fn scalar_operands_still_evaluate_and_a_bodiless_where_is_left_alone() {
    let g = corpus();
    for src in ["RETURN false AND 1 AS x", "RETURN true OR 1 AS x"] {
        let q = parse_statement(src).unwrap();
        assert!(run_query(&g, &q, params("u1")).is_err(), "`{src}` raises");
    }
    let src = "MATCH (w:Item) WHERE w.assignee = $me AND w.status = 'open' RETURN count(w) AS n";
    let want = (0..N).filter(|&i| assignee(i) == "u1" && !done(i)).count() as i64;
    let (got, c) = traced(&g, src, "u1");
    assert_eq!(got, vec![vec![Value::Int(want)]]);
    assert_eq!(count_of(&c, ORDERED), 0, "{c:?}");
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
}
