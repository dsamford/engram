#![allow(non_snake_case)]
//! Fix 41: a LEAN start population — a seed the demand analysis binds by a
//! few properties — is bound from the label's columns in one read instead
//! of a projected store get per seed. The inbox listing (`MATCH
//! (n:UserDataNode {nodeType, userId}) WHERE … WITH n ORDER BY n.createdAt
//! DESC SKIP … LIMIT 1000 …`) seeded 18,111 emails through the column filter
//! and then read every one of them back (18,111 projected gets, 9,787 of
//! them block-cache misses — the records are fat) to bind one sort key:
//! 1,251 ms for page one against Neo4j's 104, which reads the key from its
//! index and never touches a record.
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
    p.insert("skip".to_string(), Value::Int(200));
    p.insert("pageSize".to_string(), Value::Int(50));
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

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const BOUND: &str = "interp.seed starts bound from the label column";
const PROJECTED: &str = "graph.projected node materialisations";
const FULL: &str = "graph.nodes materialised in full";

/// 4,000 emails over 8 users (500 each) with 3 KB bodies, a DECLARED
/// `userId` index; a third of each user's emails carry an EmailAsk.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX udn_user FOR (n:UserDataNode) ON (n.userId)");
    let body: String = "b".repeat(3072);
    for i in 0..4000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str(format!("u{}", i % 8)));
        m.insert("classified".to_string(), Value::Bool(i % 5 != 0));
        if i % 4 == 0 {
            m.insert("abuseStatus".to_string(), Value::Str(if i % 8 == 0 { "clean".into() } else { "quarantined".into() }));
        }
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        m.insert("createdAt".to_string(), Value::Str(format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", 1 + (i % 12), 1 + (i % 28), i % 24, i % 60)));
        m.insert("rawData".to_string(), Value::Str(body.clone()));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        if i % 3 == 0 {
            let mut a = BTreeMap::new();
            a.insert("resolved".to_string(), Value::Bool(i % 6 == 0));
            a.insert("deadline".to_string(), Value::Str(format!("2026-10-{:02}", 1 + (i % 28))));
            let ask = g.create_node(&["EmailAsk".into()], &a).expect("ask");
            g.create_rel(n, "HAS_ASK", ask, &BTreeMap::new()).expect("has ask");
        }
    }
    g
}

const PAGE: &str = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
    WHERE n.classified = true AND (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) \
    WITH n ORDER BY n.createdAt DESC SKIP toInteger($skip) LIMIT toInteger($pageSize) \
    RETURN n.nodeId AS nodeId";

const PAGE_WITH_ASKS: &str = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
    WHERE n.classified = true AND (n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved']) \
    WITH n ORDER BY n.createdAt DESC SKIP toInteger($skip) LIMIT toInteger($pageSize) \
    OPTIONAL MATCH (n)-[:HAS_ASK]->(a:EmailAsk) \
    WITH n, count(CASE WHEN a IS NOT NULL AND coalesce(a.resolved, false) = false THEN a END) AS openAskCount, \
         min(CASE WHEN a IS NOT NULL AND coalesce(a.resolved, false) = false THEN a.deadline END) AS askDeadline \
    RETURN n.nodeId AS nodeId, coalesce(n.sentAt, n.createdAt) AS createdAt, askDeadline, openAskCount";

#[test]
fn a_lean_seed_population_is_bound_from_the_label_column() {
    let g = corpus();
    for src in [PAGE, PAGE_WITH_ASKS] {
        let want = general(&g, src);
        assert_eq!(want.len(), 50, "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, BOUND) > 0, "`{src}` binds its seeds from the column: {c:?}");
        assert!(
            count_of(&c, PROJECTED) < 64 && count_of(&c, FULL) < 64,
            "`{src}` reads no record per seed: {c:?}"
        );
    }
}

/// A seed's WHERE and map are still judged per candidate on the lean
/// binding: the map's keys ride along, and a predicate the column filter did
/// not take (`n.nodeId STARTS WITH …`) is evaluated on the bound node.
#[test]
fn the_map_and_the_residual_where_are_judged_on_the_lean_binding() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
        WHERE n.classified = true AND toUpper(n.nodeId) STARTS WITH 'MAIL-00' \
        RETURN n.nodeId AS nodeId ORDER BY nodeId";
    let want = general(&g, src);
    assert!(!want.is_empty());
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert!(count_of(&c, FULL) < 64, "{c:?}");
}

/// CONTROL: a small population (under the batch floor) keeps the per-id read.
#[test]
fn a_small_population_keeps_the_per_id_read() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) \
        WHERE n.classified = true AND n.nodeId IN ['mail-00001', 'mail-00009', 'mail-00017', 'mail-00033', 'mail-00041', 'mail-00049'] \
        WITH n ORDER BY n.createdAt DESC LIMIT 5 RETURN n.nodeId AS nodeId";
    let want = general(&g, src);
    assert_eq!(want.len(), 5);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, BOUND), 0, "{c:?}");
}
