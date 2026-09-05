#![allow(non_snake_case)]
//! Fix 74: a hop end that FAILS its one pattern label's membership test is
//! rejected from the membership snapshot — a label-less sentinel the
//! matcher's `node_satisfies` drops — instead of a projected (or full)
//! record read that was decoded only to be dropped. The production
//! `UNWIND $entities AS name MATCH (e:Entity {name: name})<-[:MENTIONS]-
//! (a:NewsArticle) RETURN count(a)` paid 902 projected gets for 183
//! articles on the mirror: 719 of the entities' MENTIONS edges come from
//! emails, and every email was read to learn it is not an article.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the record read for every end, as before).

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
    p.insert(
        "names".to_string(),
        Value::List((0..5).map(|i| s(&format!("entity-{}", i * 37))).collect()),
    );
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    // The first run builds the memberships and indexes; count the second.
    let _ = rows(g, src);
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

/// The control: no columnar paths — every end is a record read.
fn control(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const REJECTED: &str = "interp.matcher rejected a non-member hop end from membership";
const BARE: &str = "interp.matcher bound a hop end bare";
const PROJECTED: &str = "graph.projected node materialisations";
const NODE_FULL: &str = "graph.nodes materialised in full";
const GETS: &str = "store.gets";

/// 200 entities; 600 articles mentioning three entities each (1,800
/// article edges, 9 per entity) and 2,400 emails mentioning three each
/// (7,200 email edges, 36 per entity) — the mirror's 4:1 email-to-article
/// mix on MENTIONS. `Entity.name` is declared, as on the mirror.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX entity_name FOR (n:Entity) ON (n.name)");
    let mut entities = Vec::new();
    for i in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("name".into(), s(&format!("entity-{i}")));
        m.insert("kind".into(), s(if i % 3 == 0 { "org" } else { "person" }));
        entities.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert("articleId".into(), s(&format!("article-{i}")));
        m.insert("title".into(), s(&format!("Article {i}")));
        m.insert("body".into(), s(&"news text ".repeat(30)));
        let a = g.create_node(&["NewsArticle".into()], &m).expect("article");
        for k in 0..3 {
            let e = entities[((i * 3 + k) % 200) as usize];
            g.create_rel(a, "MENTIONS", e, &BTreeMap::new()).expect("rel");
        }
    }
    for i in 0..2_400i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeId".into(), s(&format!("email-{i}")));
        m.insert("nodeType".into(), s("email"));
        m.insert("subject".into(), s(&format!("Subject {i}")));
        m.insert("content".into(), s(&"mail text ".repeat(60)));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        for k in 0..3 {
            let e = entities[((i * 3 + k) % 200) as usize];
            g.create_rel(n, "MENTIONS", e, &BTreeMap::new()).expect("rel");
        }
    }
    g
}

const ORIG: &str = "UNWIND $names AS name \
    MATCH (e:Entity {name: name})<-[:MENTIONS]-(a:NewsArticle) \
    RETURN count(a) AS n";

/// The production shape: five entities, 45 article edges among 225 MENTIONS
/// edges. The 180 email ends are rejected from membership — no record —
/// and the 45 articles bind bare (an empty demand, fix 68).
#[test]
fn a_the_non_member_ends_are_rejected_without_a_record() {
    let g = corpus();
    let want = control(&g, ORIG);
    assert_eq!(want, vec![vec![Value::Int(45)]]);
    let (got, c) = traced(&g, ORIG);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REJECTED), 180, "{c:?}");
    assert_eq!(count_of(&c, BARE), 45, "{c:?}");
    // The five entity seeds are the only projected reads — no end is read.
    assert!(count_of(&c, PROJECTED) <= 5, "no projected read for an end the label rejects: {c:?}");
    assert_eq!(count_of(&c, NODE_FULL), 0, "{c:?}");
    assert!(count_of(&c, GETS) < 45, "fewer record reads than the 45 articles alone: {c:?}");
}

/// An end whose property is READ: the members bind from the label's
/// cached columns (fix 60) or a projected read; the non-members are still
/// rejected before any read.
#[test]
fn b_a_demanded_end_still_rejects_non_members_first() {
    let g = corpus();
    let src = "UNWIND $names AS name \
        MATCH (e:Entity {name: name})<-[:MENTIONS]-(a:NewsArticle) \
        RETURN a.title AS title ORDER BY title LIMIT 5";
    let want = control(&g, src);
    assert_eq!(want.len(), 5);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REJECTED), 180, "{c:?}");
    // The five seeds and at most the 45 articles are read — no email.
    assert!(count_of(&c, PROJECTED) <= 50, "at most the seeds and the 45 articles are read: {c:?}");
    assert_eq!(count_of(&c, NODE_FULL), 0, "{c:?}");
}

/// Shapes outside the class decline and agree: an UNLABELLED end counts
/// the emails too (nothing to reject); a TWO-label end keeps the record
/// read (the rule is one label); and the columnar paths OFF read every
/// end as before.
#[test]
fn c_shapes_outside_the_class_decline_and_agree() {
    let g = corpus();
    let unlabelled = "UNWIND $names AS name \
        MATCH (e:Entity {name: name})<-[:MENTIONS]-(a) RETURN count(a) AS n";
    let want = control(&g, unlabelled);
    assert_eq!(want, vec![vec![Value::Int(225)]]);
    let (got, c) = traced(&g, unlabelled);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REJECTED), 0, "{c:?}");

    let two_labels = "UNWIND $names AS name \
        MATCH (e:Entity {name: name})<-[:MENTIONS]-(a:NewsArticle:UserDataNode) RETURN count(a) AS n";
    let want = control(&g, two_labels);
    assert_eq!(want, vec![vec![Value::Int(0)]]);
    let (got, c) = traced(&g, two_labels);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REJECTED), 0, "{c:?}");

    g.set_columnar_scans(false);
    let (got, c) = traced(&g, ORIG);
    g.set_columnar_scans(true);
    assert_eq!(got, vec![vec![Value::Int(45)]]);
    assert_eq!(count_of(&c, REJECTED), 0, "{c:?}");
    assert!(count_of(&c, PROJECTED) + count_of(&c, NODE_FULL) >= 225, "the control reads every end: {c:?}");
}

/// A statement's OWN writes are visible to the membership the sentinel
/// reads: an article this statement has just CREATED is a member of the
/// overlaid view (`Graph::members` overlays the transaction's buffered
/// membership changes), so it is bound and counted while the 36 emails
/// are still rejected without a record. Committed, the shared snapshot
/// knows it too.
#[test]
fn d_a_statements_own_created_member_is_seen_by_the_sentinel() {
    let g = corpus();
    let src = "MATCH (e:Entity {name: 'entity-0'})<-[:MENTIONS]-(a:NewsArticle) RETURN count(a) AS n";
    let before = rows(&g, src);
    assert_eq!(before, vec![vec![Value::Int(9)]]);
    let txn = "MATCH (e:Entity {name: 'entity-0'}) \
        CREATE (fresh:NewsArticle {articleId: 'article-fresh'})-[:MENTIONS]->(e) \
        WITH e MATCH (e)<-[:MENTIONS]-(a:NewsArticle) RETURN count(a) AS n";
    let q = parse_statement(txn).expect("parse");
    let (got, trace) = engram_observe::with_trace(|| {
        run_query(&g, &q, params()).expect("run").rows
    });
    let c = trace.counters().clone();
    assert_eq!(got, vec![vec![Value::Int(10)]], "the just-created article counts: {c:?}");
    assert_eq!(count_of(&c, REJECTED), 36, "the 36 emails, not the fresh article: {c:?}");
    // Committed, the new article is a member the snapshot knows.
    assert_eq!(rows(&g, src), vec![vec![Value::Int(10)]]);
}
