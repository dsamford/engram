#![allow(non_snake_case)]
//! Fix 36c: a DIRECTED existence probe of a columnar walk — `EXISTS {
//! (n)-[:T]->(:L)}`, `NOT (n)-[:T]->()`, `exists((n)-[:T]->(:L {k: v}))` —
//! is answered for the WHOLE population in one pass over the type's
//! adjacency table (tokens resolved once, the table borrowed once, no
//! per-member Vec), instead of `adjacency_probe_*` per member with its
//! token lookup, membership lookup, allocation and per-visit bookkeeping.
//! The email revival backlog's `NOT EXISTS {(n)-[:MENTIONS_INTEREST]->
//! (:Interest)}` cost 16 ms over 18k emails on the mirror (22 vs Neo4j 11–20
//! for the count; the pick 93 vs 74).
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const POPULATION: &str = "interp.columnar probes answered over the population";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
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

/// 6,000 emails; a third mention an :Interest, a fifth mention a :Topic
/// (the same type, another label — the labelled probe must not count it),
/// and a few interests mention each other (an IN-side edge on the far end).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut interests = Vec::new();
    for k in 0..10i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("i-{k}")));
        interests.push(g.create_node(&["Interest".into()], &m).expect("interest"));
    }
    let mut topics = Vec::new();
    for k in 0..5i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("t-{k}")));
        topics.push(g.create_node(&["Topic".into()], &m).expect("topic"));
    }
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("classified".to_string(), Value::Bool(i % 7 != 0));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        if i % 4 == 0 {
            m.insert("reinforceRevivals".to_string(), Value::Int(i % 5));
        }
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        if i % 3 == 0 {
            g.create_rel(n, "MENTIONS_INTEREST", interests[(i / 3 % 10) as usize], &BTreeMap::new())
                .expect("mention");
        }
        if i % 5 == 0 {
            g.create_rel(n, "MENTIONS_INTEREST", topics[(i / 5 % 5) as usize], &BTreeMap::new())
                .expect("topic mention");
        }
    }
    for k in 0..9usize {
        g.create_rel(interests[k], "MENTIONS_INTEREST", interests[k + 1], &BTreeMap::new())
            .expect("chain");
    }
    g
}

fn check(g: &Graph, src: &str) -> BTreeMap<String, u64> {
    let want = general(g, src);
    let first = rows(g, src);
    assert_eq!(first, want, "first run `{src}`");
    let (got, c) = traced(g, src);
    assert_eq!(got, want, "second run `{src}`");
    c
}

#[test]
fn a_labelled_anti_join_is_answered_over_the_population() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND n.classified = true AND coalesce(n.reinforceRevivals, 0) < 3 AND NOT EXISTS { (n)-[:MENTIONS_INTEREST]->(:Interest) } RETURN count(n) AS n";
    let c = check(&g, src);
    assert!(count_of(&c, POPULATION) > 0, "{c:?}");
}

#[test]
fn the_unlabelled_and_pattern_spellings_too() {
    let g = corpus();
    for src in [
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND NOT EXISTS { (n)-[:MENTIONS_INTEREST]->() } RETURN count(n) AS n",
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND NOT (n)-[:MENTIONS_INTEREST]->() RETURN count(n) AS n",
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND (n)<-[:MENTIONS_INTEREST]-() RETURN count(n) AS n",
    ] {
        let c = check(&g, src);
        assert!(count_of(&c, POPULATION) > 0, "`{src}`: {c:?}");
    }
}

#[test]
fn a_far_end_map_and_a_projection_over_the_survivors() {
    let g = corpus();
    for src in [
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND exists((n)-[:MENTIONS_INTEREST]->(:Interest {name: 'i-3'})) RETURN n.nodeId AS id ORDER BY id LIMIT 10",
        "MATCH (n:UserDataNode) WHERE n.classified = true AND NOT EXISTS { (n)-[:MENTIONS_INTEREST]->(:Interest) } RETURN n.nodeId AS id ORDER BY id DESC LIMIT 7",
        "MATCH (n:UserDataNode) WHERE EXISTS { (n)-[:MENTIONS_INTEREST]->(:Topic) } RETURN count(n) AS n, count(DISTINCT n.classified) AS k",
    ] {
        let c = check(&g, src);
        assert!(count_of(&c, POPULATION) > 0, "`{src}`: {c:?}");
    }
}

/// CONTROL: an UNDIRECTED probe walks two sides — it keeps the per-member
/// probe and its answer.
#[test]
fn an_undirected_probe_keeps_the_per_member_probe() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND NOT EXISTS { (n)-[:MENTIONS_INTEREST]-(:Interest) } RETURN count(n) AS n";
    let c = check(&g, src);
    assert_eq!(count_of(&c, POPULATION), 0, "{c:?}");
}
