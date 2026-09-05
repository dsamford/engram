#![allow(non_snake_case)]
//! A bare `WITH s` whose every LATER mention is a property read demands only
//! those properties — not the full node.
//!
//! The production story tracker (`MATCH … WITH s, count(DISTINCT e) AS n WHERE
//! n >= 1 ORDER BY n DESC LIMIT 5 RETURN s.storyId, s.title`) paid a FULL
//! `NewsStory` decode per grouped row — every property, summaries included —
//! because the WITH carried `s` bare and the demand analysis treated a bare
//! use as whole-entity. The clauses after the WITH read two properties. The
//! demand is now those two (plus whatever the stage itself reads), and the
//! rows are byte-identical to the full-node form. A bare later use (`RETURN
//! s`), a pattern that reuses the name, or an alias re-read all keep the full
//! demand — asserted here, because a narrowing that fires on a whole-entity
//! use answers with a node missing properties.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |label: &str, props: &[(&str, &str)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), Value::Str((*v).into()));
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let a1 = node("A", &[("name", "a1")]);
    let a2 = node("A", &[("name", "a2")]);
    let a3 = node("A", &[("name", "a3")]);
    let b1 = node(
        "B",
        &[("title", "t1"), ("status", "live"), ("body", "a long body nobody reads")],
    );
    let b2 = node(
        "B",
        &[("title", "t2"), ("status", "live"), ("body", "another long body")],
    );
    let b3 = node("B", &[("title", "t3"), ("status", "stale"), ("body", "stale body")]);
    for (a, b) in [(a1, b1), (a2, b1), (a3, b1), (a1, b2), (a2, b3)] {
        g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

/// (narrowed-fired, full-node-materialisations, rows) for one statement,
/// traced on the STREAMING path (columnar paths off): the claim is about
/// the stage planner, and the pipeline tails have taken these shapes since
/// fix 32 (the where-first WITH tail fuses into the RETURN).
fn traced(g: &Graph, src: &str) -> (u64, u64, Vec<Vec<Value>>) {
    g.set_columnar_scans(false);
    let (out, trace) = engram_observe::with_trace(|| rows(g, src));
    g.set_columnar_scans(true);
    // These shapes must be on the STREAMING path for the claim to be about
    // the stage planner at all — a bare one-hop group-by takes the columnar
    // aggregate, which has its own materialisation rules.
    let mut fired: Vec<String> = trace.sometimes_hit().iter().map(|s| s.to_string()).collect();
    fired.sort();
    assert!(
        trace
            .sometimes_hit()
            .contains("interp.streamed a read-only chain"),
        "the statement must stream: {src}\nfired: {fired:?}"
    );
    let narrowed = trace
        .counters()
        .get("interp.live projection demanded only the properties read after it")
        .copied()
        .unwrap_or(0);
    let mats = trace
        .counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0);
    (narrowed, mats, out)
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

const STORY: &str = "MATCH (a:A)-[:R]->(b:B) WHERE b.status <> 'stale' \
    WITH b, count(DISTINCT a) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5 \
    RETURN b.title AS title, n";

#[test]
fn the_story_shape_reads_two_properties_and_decodes_no_node_in_full() {
    let g = g();
    let (narrowed, mats, out) = traced(&g, STORY);
    assert_eq!(
        out,
        vec![vec![s("t1"), Value::Int(3)], vec![s("t2"), Value::Int(1)]],
        "rows"
    );
    assert!(narrowed >= 1, "the WITH must have demanded only the later-read properties");
    assert_eq!(mats, 0, "no grouped node may be decoded in full: {mats}");
}

/// A bare node carried across the WITH into an OPTIONAL MATCH as an
/// ENDPOINT (`(b)-[:R]-(x)`) is an identity use — the expansion starts from
/// its id — so the WITH still narrows to the properties read after it, and
/// the expansion, the aggregate and the projection answer exactly as the
/// full-node form does. This is the production email listing: `WITH n
/// ORDER BY … SKIP … LIMIT … OPTIONAL MATCH (n)-[:HAS_ASK]->(a:EmailAsk) …
/// RETURN n.nodeId, coalesce(n.sentAt, n.createdAt), …`.
#[test]
fn an_optional_match_endpoint_after_the_with_is_an_identity_use() {
    let g = g();
    let src = "MATCH (b:B) WHERE b.status <> 'stale' \
         WITH b ORDER BY b.title LIMIT 5 \
         OPTIONAL MATCH (b)<-[:R]-(a:A) \
         WITH b, count(a) AS fans \
         RETURN b.title AS title, coalesce(b.status, 'none') AS status, fans ORDER BY title";
    let (narrowed, mats, out) = traced(&g, src);
    assert_eq!(
        out,
        vec![
            vec![s("t1"), s("live"), Value::Int(3)],
            vec![s("t2"), s("live"), Value::Int(1)],
        ]
    );
    assert!(narrowed >= 1, "the endpoint use must not widen the demand to the full node");
    assert_eq!(mats, 0, "no B may be decoded in full: {mats}");
    // An endpoint that RESTATES a props map on the carried name is a
    // whole-entity use and keeps the full node — the narrowing must not fire.
    let restated = "MATCH (b:B) WHERE b.status <> 'stale' \
         WITH b ORDER BY b.title LIMIT 5 \
         OPTIONAL MATCH (b {status: 'live'})<-[:R]-(a:A) \
         RETURN b.title AS title, count(a) AS fans ORDER BY title";
    let (narrowed, _, out) = traced(&g, restated);
    assert_eq!(out, vec![vec![s("t1"), Value::Int(3)], vec![s("t2"), Value::Int(1)]]);
    assert_eq!(narrowed, 0, "a restated map on the endpoint is a whole-entity use");
}

/// `RETURN count(s)` after the WITH is a presence-level use (the same rule
/// the demand walk applies to `count(v)`), not a whole-entity one: the WITH
/// still narrows, and no grouped node is decoded in full.
#[test]
fn a_later_count_of_the_node_is_presence_only() {
    let g = g();
    let (narrowed, mats, out) = traced(
        &g,
        "MATCH (a:A)-[:R]->(b:B) WHERE b.status <> 'stale' \
         WITH b, count(DISTINCT a) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5 \
         RETURN count(b) AS c",
    );
    assert_eq!(out, vec![vec![Value::Int(2)]]);
    assert!(narrowed >= 1, "count(b) reads no property and needs no full node");
    assert_eq!(mats, 0);
}

#[test]
fn the_rows_equal_the_full_node_form() {
    let g = g();
    let narrowed = rows(&g, STORY);
    // The same statement carrying the WHOLE node through the WITH and
    // reading the title off it at the end — the answer must not depend on
    // how much of the node the WITH carried.
    let full = rows(
        &g,
        "MATCH (a:A)-[:R]->(b:B) WHERE b.status <> 'stale' \
         WITH b, count(DISTINCT a) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5 \
         WITH b AS whole, n RETURN whole.title AS title, n",
    );
    assert_eq!(narrowed, full);
}

#[test]
fn an_alias_is_narrowed_under_its_new_name() {
    let g = g();
    // The story shape again (a WHERE on the hop var keeps it on the streaming
    // path — a bare one-hop group-by takes the columnar aggregate, which has
    // its own materialisation rules), with the node carried under an alias.
    let (narrowed, mats, out) = traced(
        &g,
        "MATCH (a:A)-[:R]->(b:B) WHERE b.status <> 'nothing' \
         WITH b AS story, count(DISTINCT a) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5 \
         RETURN story.title AS title, story.status AS st, n",
    );
    assert_eq!(
        out,
        vec![
            vec![s("t1"), s("live"), Value::Int(3)],
            vec![s("t2"), s("live"), Value::Int(1)],
            vec![s("t3"), s("stale"), Value::Int(1)],
        ]
    );
    assert!(narrowed >= 1, "the alias is the name the later clauses read");
    assert_eq!(mats, 0);
}

#[test]
fn a_bare_later_use_keeps_the_full_node() {
    let g = g();
    // The story shape once more, returning the node itself.
    let (narrowed, mats, out) = traced(
        &g,
        "MATCH (a:A)-[:R]->(b:B) WHERE b.status <> 'nothing' \
         WITH b, count(DISTINCT a) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5 \
         RETURN b, n",
    );
    assert_eq!(out.len(), 3);
    // The returned node carries EVERY property — the narrowing did not fire.
    let Value::Node { props, .. } = &out[0][0] else {
        panic!("a node");
    };
    assert_eq!(props.get("body").map(|v| v == &s("a long body nobody reads")), Some(true));
    assert_eq!(narrowed, 0, "a bare RETURN of the node is a whole-entity use");
    assert!(mats >= 1);
}

/// A later pattern REUSING the name as a bare endpoint is an identity use
/// (the expansion starts from the id): the WITH narrows and the rows are
/// the full-node form's. A later pattern restating a props map on the name
/// is the whole-entity case — see
/// `an_optional_match_endpoint_after_the_with_is_an_identity_use`.
#[test]
fn a_pattern_reusing_the_name_as_a_bare_endpoint_still_narrows() {
    let g = g();
    let (narrowed, mats, out) = traced(
        &g,
        "MATCH (a:A)-[:R]->(b:B) WITH b, count(a) AS n \
         MATCH (b)<-[:R]-(x:A) RETURN b.title AS title, count(x) AS m ORDER BY title",
    );
    assert_eq!(
        out,
        vec![
            vec![s("t1"), Value::Int(3)],
            vec![s("t2"), Value::Int(1)],
            vec![s("t3"), Value::Int(1)],
        ]
    );
    assert!(narrowed >= 1, "a bare endpoint reuse reads nothing of the node");
    assert_eq!(mats, 0, "no B may be decoded in full: {mats}");
}
