#![allow(non_snake_case)]
//! Fix 78: a columnar projection over a population that is a large share
//! of its label reads the label's columns WHOLE — the read the property-
//! column cache keeps — and restricts them to the population, so the next
//! statement over that label reads nothing.
//!
//! The production email classification listing (`MATCH (n:UserDataNode
//! {userId: $userId, nodeType: 'email'}) WHERE n.classified = true AND
//! (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) RETURN
//! n.sentimentLabel AS sentiment, … n.semanticLabels AS semanticLabels`)
//! projects eight properties over 18k of the 38k emails: a population read
//! is never kept, so every run gathered 18k records (1,005 ms against
//! Neo4j's 207 on the mirror) and the next run gathered them again.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF.

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

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".into(), s(user));
    p
}

fn rows(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(user))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, user: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, user));
    (r, trace.counters().clone())
}

fn control(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src, user);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const WHOLE: &str = "interp.columnar population read its label whole to keep the columns";
const KEPT: &str = "graph.property column kept";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const SCANS: &str = "store.column scans";
const GATHER: &str = "graph.column record-gather";
const POINT_GATHER: &str = "graph.column point-gather";
const GETS: &str = "store.gets";

/// 4,000 emails and notes; the big user owns half of them. The mirror's
/// `(userId, nodeType)` composite is declared.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX udn_user_type FOR (n:UserDataNode) ON (n.userId, n.nodeType)");
    for i in 0..4_000i64 {
        let big = i < 2_000;
        let mut m = BTreeMap::new();
        m.insert("nodeId".into(), s(&format!("n-{i}")));
        m.insert("userId".into(), s(if big { "u-big" } else { "u-other" }));
        m.insert("nodeType".into(), s(if i % 20 == 0 { "note" } else { "email" }));
        m.insert("classified".into(), Value::Bool(i % 25 != 0));
        if i % 7 == 0 {
            m.insert("abuseStatus".into(), s(if i % 14 == 0 { "clean" } else { "flagged" }));
        }
        m.insert("sentimentLabel".into(), s(["neutral", "positive", "negative"][(i % 3) as usize]));
        m.insert("sentimentTone".into(), s("automated"));
        m.insert("urgencyScore".into(), Value::Int(i % 5));
        m.insert("contentType".into(), s("newsletter"));
        m.insert("senderType".into(), s("marketing"));
        m.insert("priority".into(), s("informational"));
        m.insert("actionRequired".into(), Value::Bool(i % 9 == 0));
        m.insert("semanticLabels".into(), Value::List(vec![s("newsletter"), s("promotional")]));
        m.insert("subject".into(), s(&format!("Subject line {i}")));
        m.insert("content".into(), s(&"email body text ".repeat(40)));
        g.create_node(&["UserDataNode".into()], &m).expect("node");
    }
    g
}

const LISTING: &str = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'}) \
    WHERE n.classified = true AND (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) \
    RETURN n.sentimentLabel AS sentiment, n.sentimentTone AS tone, n.urgencyScore AS urgency, \
    n.contentType AS contentType, n.senderType AS senderType, n.priority AS priority, \
    n.actionRequired AS actionRequired, n.semanticLabels AS semanticLabels";

/// The first run reads the label whole and keeps its eight columns; the
/// second is served from the cache and touches no record and no column
/// scan. Both agree with the control.
#[test]
fn a_the_listing_keeps_its_columns_and_the_next_run_reads_nothing() {
    let g = corpus();
    let want = control(&g, LISTING, "u-big");
    assert_eq!(want.len(), 1_703, "the big user's classified, non-flagged emails");
    let (first, c1) = traced(&g, LISTING, "u-big");
    assert_eq!(first, want);
    assert_eq!(count_of(&c1, WHOLE), 1, "{c1:?}");
    assert!(count_of(&c1, KEPT) >= 8, "the eight item columns are kept: {c1:?}");
    let (second, c2) = traced(&g, LISTING, "u-big");
    assert_eq!(second, want);
    assert_eq!(count_of(&c2, WHOLE), 0, "nothing to read whole once cached: {c2:?}");
    assert!(count_of(&c2, SERVED) >= 8, "every item column from the cache: {c2:?}");
    assert_eq!(count_of(&c2, SCANS), 0, "{c2:?}");
    assert_eq!(count_of(&c2, GATHER) + count_of(&c2, POINT_GATHER), 0, "{c2:?}");
    assert!(count_of(&c2, GETS) <= 2, "no record per row on the cached run: {c2:?}");
}

/// A population below the share threshold keeps the population read (no
/// whole-label read), and a cached column is still served to it.
#[test]
fn b_a_small_population_is_not_read_whole() {
    let g = corpus();
    // Nine notes of the other user: 100 of 4,000 nodes are notes, 50 of
    // them the other user's — well under an eighth of the label.
    let small = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'note'}) \
        WHERE n.classified = true \
        RETURN n.sentimentLabel AS sentiment, n.priority AS priority";
    let want = control(&g, small, "u-other");
    assert!(!want.is_empty() && want.len() < 100, "{}", want.len());
    let (got, c) = traced(&g, small, "u-other");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WHOLE), 0, "{c:?}");
}

/// A presence-only read (`IS NOT NULL` in the items) rides the same path:
/// the presence column is kept and served next time.
#[test]
fn c_presence_columns_are_kept_too() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'}) \
        RETURN n.nodeId AS id, n.abuseStatus IS NOT NULL AS flagged";
    let want = control(&g, src, "u-big");
    assert_eq!(want.len(), 1_900);
    let (first, c1) = traced(&g, src, "u-big");
    assert_eq!(first, want);
    assert_eq!(count_of(&c1, WHOLE), 1, "{c1:?}");
    let (second, c2) = traced(&g, src, "u-big");
    assert_eq!(second, want);
    assert_eq!(count_of(&c2, WHOLE), 0, "{c2:?}");
    assert_eq!(count_of(&c2, SCANS), 0, "{c2:?}");
}
