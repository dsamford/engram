#![allow(non_snake_case)]
//! Fix 34 (v109): the columnar projection and aggregate stages' per-id SEEK
//! path decoded every sought record IN FULL to check its labels and bind the
//! two or three properties it read. On a fat label that is the whole cost:
//! the email listing's `{nodeType: 'email', userId: $u}` seek decoded 37
//! UserDataNode records with their raw bodies to project `n.nodeId` for ten
//! rows (2.5 ms on the mirror against Neo4j's 1.0). The hits now decode
//! PROJECTED — the plan's reads, labels always along.
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
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

const FULL: &str = "graph.nodes materialised in full";
const PROJECTED: &str = "graph.projected node materialisations";
const PROJ_SCANS: &str = "interp.columnar projection scans";
const AGG_SCANS: &str = "interp.columnar aggregate scans";

/// 3,000 `:UDN {nodeType, userId, nodeId, classified, score, rawData}` — a
/// 4 KB body on every record — over 40 users; a third are emails. A seek
/// on `userId` hits ~75 records of which ~25 are emails.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(4096);
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "nodeType".to_string(),
            Value::Str(if i % 3 == 0 { "email".into() } else { "note".into() }),
        );
        m.insert("userId".to_string(), Value::Str(format!("u{}", 1 + i % 40)));
        m.insert("nodeId".to_string(), Value::Str(format!("node-{i:05}")));
        if i % 7 != 0 {
            m.insert("classified".to_string(), Value::Bool((i / 120) % 2 == 0));
        }
        m.insert("score".to_string(), Value::Int(i % 11));
        m.insert("rawData".to_string(), Value::Str(body.clone()));
        g.create_node(&["UDN".into()], &m).expect("udn");
    }
    g
}

#[test]
fn a_projection_over_a_seek_decodes_only_what_it_reads() {
    let g = corpus();
    for src in [
        "MATCH (n:UDN {nodeType: 'email', userId: $u}) RETURN n.nodeId AS id ORDER BY id",
        "MATCH (n:UDN {nodeType: 'email', userId: $u}) WHERE n.classified = true \
         RETURN n.nodeId AS id, n.score AS s ORDER BY s DESC, id LIMIT 10",
        "MATCH (n:UDN {userId: $u}) WHERE n.classified IS NULL RETURN n.nodeId AS id ORDER BY id",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, PROJ_SCANS) > 0, "`{src}` seeks on the columnar projection: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}` decodes no record in full: {c:?}");
        assert!(count_of(&c, PROJECTED) > 0, "`{src}` decodes its hits projected: {c:?}");
    }
}

#[test]
fn an_aggregate_over_a_seek_decodes_only_what_it_reads() {
    let g = corpus();
    for src in [
        "MATCH (n:UDN {nodeType: 'email', userId: $u}) WHERE n.classified = true RETURN count(n) AS n",
        "MATCH (n:UDN {userId: $u}) RETURN sum(n.score) AS s, count(n.classified) AS c",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, AGG_SCANS) > 0, "`{src}` seeks on the columnar aggregate: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}` decodes no record in full: {c:?}");
    }
}

/// CONTROL: a bare node in the output still materialises the winners in
/// full — and only the winners.
#[test]
fn a_bare_output_item_materialises_the_winners_only() {
    let g = corpus();
    let src = "MATCH (n:UDN {nodeType: 'email', userId: $u}) RETURN n ORDER BY n.nodeId LIMIT 5";
    let want = general(&g, src);
    assert_eq!(want.len(), 5);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FULL), 5, "{c:?}");
}
