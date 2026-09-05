#![allow(non_snake_case)]
//! Fix 76: a subquery body — a pattern comprehension, an EXISTS / COUNT
//! pattern body — is seeded with every bound NODE trimmed to what the body
//! reads of it, instead of a whole copy of the outer row that the general
//! matcher then clones several more times per evaluation.
//!
//! The production KM work-item listing (`… RETURN properties(w) AS w,
//! [(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id][0] AS projectId,
//! [(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id][0]
//! AS parentId …`) binds `w` in FULL for its 197 survivors, and its two
//! comprehensions per row cost 22 of the statement's 34 ms on the mirror;
//! locally the cost tracked `w`'s property count (7× for 12× the
//! properties), which names the clone.
//!
//! Every answer is checked against the same statement with the lever OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("projectId".into(), s("proj-7"));
    p.insert("limit".into(), Value::Int(200));
    p.insert("offset".into(), Value::Int(0));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let _ = rows(g, src);
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

/// The control: the lever OFF — the whole row seeds every body.
fn control(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_lean_subquery_seed(false);
    let r = rows(g, src);
    g.set_lean_subquery_seed(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const LEAN: &str = "interp.subquery seeded with a lean row";

/// 40 projects × 60 work items with fat records (a 2 KB description and
/// twelve fields); every item after a project's first ten has a parent
/// among those ten. `KMProject.id` is declared, as on the mirror.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX kmp_id FOR (n:KMProject) ON (n.id)");
    let mut projects = Vec::new();
    for pi in 0..40i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("proj-{pi}")));
        m.insert("name".into(), s(&format!("Project {pi}")));
        projects.push(g.create_node(&["KMProject".into()], &m).expect("project"));
    }
    let mut firsts: Vec<Vec<u64>> = vec![Vec::new(); 40];
    for i in 0..2_400i64 {
        let pi = (i % 40) as usize;
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("item-{i}")));
        m.insert("title".into(), s(&format!("Work item {i}")));
        m.insert("description".into(), s(&"lorem ipsum dolor sit amet ".repeat(80)));
        m.insert("status".into(), s(["todo", "doing", "done"][(i % 3) as usize]));
        m.insert("projectRef".into(), s(&format!("proj-{pi}")));
        m.insert("sortOrder".into(), Value::Int((i * 7919) % 2_400));
        for k in 0..12 {
            m.insert(format!("field{k}"), s(&format!("value {k} of {i}")));
        }
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO_PROJECT", projects[pi], &BTreeMap::new()).expect("rel");
        if firsts[pi].len() >= 10 {
            let parent = firsts[pi][(i as usize / 40) % 10];
            g.create_rel(parent, ["HAS_EPIC", "HAS_TASK", "HAS_CHILD"][(i % 3) as usize], w, &BTreeMap::new())
                .expect("rel");
        } else {
            firsts[pi].push(w);
        }
    }
    g
}

const HEAD: &str = "MATCH (w:KMWorkItem) WHERE true AND EXISTS { (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } RETURN ";
const TAIL: &str = " ORDER BY w.sortOrder ASC SKIP toInteger($offset) LIMIT toInteger($limit)";
const A: &str = "[(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id][0] AS projectId";
const B: &str = "[(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id][0] AS parentId";

/// The production listing: both comprehensions seeded lean, once per row
/// each, and the answer byte-identical to the whole-row control.
#[test]
fn a_the_km_listing_seeds_both_comprehensions_lean() {
    let g = corpus();
    let src = format!("{HEAD}properties(w) AS w, {A}, {B}{TAIL}");
    let want = control(&g, &src);
    assert_eq!(want.len(), 60);
    let (got, c) = traced(&g, &src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, LEAN), 120, "two comprehensions per row: {c:?}");
    // The projectId column reads through the comprehension, the parentId
    // one is null for a project's first ten items and set for the rest.
    assert!(got.iter().all(|r| r[1] == s("proj-7")));
    assert_eq!(got.iter().filter(|r| r[2] != Value::Null).count(), 50);
}

/// What the body READS of the bound node survives the trim: a property in
/// the map, the whole node in the map, a property in the filter.
#[test]
fn b_what_the_body_reads_of_the_node_survives() {
    let g = corpus();
    let prop = format!("{HEAD}[(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | w.title + ' in ' + p.name][0] AS label{TAIL}");
    let want = control(&g, &prop);
    let (got, c) = traced(&g, &prop);
    assert_eq!(got, want);
    assert!(got.iter().all(|r| matches!(&r[0], Value::Str(t) if t.starts_with("Work item ") && t.ends_with(" in Project 7"))), "first rows: {:?}", &got[..2]);
    assert_eq!(count_of(&c, LEAN), 60, "{c:?}");

    let whole = format!("{HEAD}[(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | w][0] AS again{TAIL}");
    let want = control(&g, &whole);
    let (got, c) = traced(&g, &whole);
    assert_eq!(got, want);
    assert!(got.iter().all(|r| matches!(&r[0], Value::Node { props, .. } if props.len() == 18)), "the whole node comes back");
    assert_eq!(count_of(&c, LEAN), 0, "a whole use keeps the row whole: {c:?}");

    let filtered = format!("{HEAD}[(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) WHERE w.status = 'todo' | p.id] AS todo{TAIL}");
    let want = control(&g, &filtered);
    let (got, c) = traced(&g, &filtered);
    assert_eq!(got, want);
    assert_eq!(got.iter().filter(|r| r[0] == Value::List(vec![s("proj-7")])).count(), 20);
    assert_eq!(count_of(&c, LEAN), 60, "{c:?}");
}

/// The pattern's OWN inline maps: a bound node carrying a map stays whole
/// (the matcher tests the map on the value), and a map on another node
/// that reads the bound node's property keeps that property.
#[test]
fn c_the_patterns_inline_maps_are_part_of_the_demand() {
    let g = corpus();
    let on_bound = format!("{HEAD}[(w {{status: 'todo'}})-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id] AS todo{TAIL}");
    let want = control(&g, &on_bound);
    let (got, c) = traced(&g, &on_bound);
    assert_eq!(got, want);
    assert_eq!(got.iter().filter(|r| r[0] == Value::List(vec![s("proj-7")])).count(), 20);
    assert_eq!(count_of(&c, LEAN), 0, "a map on the bound node keeps it whole: {c:?}");

    let correlated = format!("{HEAD}[(w)-[:BELONGS_TO_PROJECT]->(p:KMProject {{id: w.projectRef}}) | p.name][0] AS name{TAIL}");
    let want = control(&g, &correlated);
    let (got, c) = traced(&g, &correlated);
    assert_eq!(got, want);
    assert!(got.iter().all(|r| r[0] == s("Project 7")), "first rows: {:?}", &got[..2]);
    assert_eq!(count_of(&c, LEAN), 60, "{c:?}");
}

/// EXISTS and COUNT pattern bodies take the same seed: a WHERE reading the
/// bound node's property, and a bare body, agree with the control.
#[test]
fn d_exists_and_count_bodies_seed_lean_too() {
    let g = corpus();
    let src = format!(
        "{HEAD}properties(w) AS w, \
         COUNT {{ (w)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(c:KMWorkItem) WHERE c.status = w.status }} AS same, \
         EXISTS {{ (w)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(:KMWorkItem) }} AS isParent{TAIL}"
    );
    let want = control(&g, &src);
    let (got, c) = traced(&g, &src);
    assert_eq!(got, want);
    assert_eq!(got.iter().filter(|r| r[2] == Value::Bool(true)).count(), 10, "the ten parents");
    assert!(got.iter().any(|r| matches!(r[1], Value::Int(n) if n > 0)), "some parent has a same-status child");
    assert!(count_of(&c, LEAN) >= 60, "{c:?}");
}
