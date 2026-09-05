#![allow(non_snake_case)]
//! Fix 39: a DIRECTED hop of the DataChunk pipeline borrows its adjacency
//! table ONCE for every driving row and walks each row's CSR slice in place,
//! instead of the single-node accessor's per-visit epoch / gate / overlay /
//! memo bookkeeping (~400 ns a row around a ~20 ns lookup). The production
//! `MATCH (n:UserDataNode {userId: $u})-[r:REPLIED_TO]->(t:UserDataNode)
//! RETURN count(r)` expanded a 38k-email seed for 0 edges in 14–18 ms on
//! the mirror against Neo4j's 1.6–1.9.
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const BORROWED: &str = "interp.pipeline hop borrowed its adjacency table once";
const HOP_RUNS: &str = "interp.pipeline hop runs";

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

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

/// 6,000 emails of one user (a DECLARED `userId` index seeds them), 2,000 of
/// another; a handful of REPLIED_TO edges among them, MENTIONS edges to a
/// few entities, and one self-loop so the undirected control has a dedupe
/// to keep.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX udn_user FOR (n:UserDataNode) ON (n.userId)");
    let mut mails = Vec::new();
    for i in 0..8000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str(if i % 4 == 3 { "u2".into() } else { "u1".into() }));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        mails.push(g.create_node(&["UserDataNode".into()], &m).expect("email"));
    }
    let mut ents = Vec::new();
    for k in 0..12i64 {
        let mut e = BTreeMap::new();
        e.insert("name".to_string(), Value::Str(format!("ent-{k}")));
        ents.push(g.create_node(&["Entity".into()], &e).expect("entity"));
    }
    for i in (0..8000usize).step_by(997) {
        g.create_rel(mails[i], "REPLIED_TO", mails[(i * 7 + 13) % 8000], &BTreeMap::new())
            .expect("reply");
    }
    g.create_rel(mails[40], "REPLIED_TO", mails[40], &BTreeMap::new()).expect("self reply");
    for i in (0..8000usize).step_by(53) {
        g.create_rel(mails[i], "MENTIONS", ents[i % 12], &BTreeMap::new()).expect("mention");
    }
    g
}

#[test]
fn a_directed_hop_from_a_wide_seed_borrows_the_table_once() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {userId: $u})-[r:REPLIED_TO]->(t:UserDataNode) RETURN count(r) AS cnt, count(DISTINCT t.nodeId) AS unique";
    let want = general(&g, src);
    let _ = rows(&g, src); // the first run may build the table
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert!(count_of(&c, HOP_RUNS) + count_of(&c, "interp.pipeline aggregate runs") > 0, "{c:?}");
    assert!(count_of(&c, BORROWED) > 0, "{c:?}");
}

/// Every spelling answers as the general path does; the ones the pipeline
/// takes (a reverse-hop count, a count by the far end, a production-order
/// LIMIT cut) borrow the table.
#[test]
fn a_reverse_hop_and_a_projection_keep_their_order() {
    let g = corpus();
    let mut borrowed = 0u64;
    for src in [
        "MATCH (n:UserDataNode {userId: $u})<-[:REPLIED_TO]-(t:UserDataNode) RETURN count(*) AS c",
        "MATCH (n:UserDataNode {userId: $u})<-[:REPLIED_TO]-(t:UserDataNode) RETURN n.nodeId AS a, t.nodeId AS b ORDER BY a, b",
        "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e:Entity) RETURN e.name AS name, count(*) AS c ORDER BY name",
        "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e:Entity) RETURN n.nodeId AS id, e.name AS name LIMIT 20",
        "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e:Entity) RETURN n.nodeId AS id, e.name AS name ORDER BY id DESC, name LIMIT 20",
    ] {
        let want = general(&g, src);
        let _ = rows(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        borrowed += count_of(&c, BORROWED);
    }
    assert!(borrowed >= 3, "the pipeline's hops borrow the table: {borrowed}");
}

/// CONTROL: an undirected hop walks two sides with a self-loop deduped
/// between them — it keeps the per-row accessor and its answer.
#[test]
fn an_undirected_hop_keeps_the_per_row_accessor() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {userId: $u})-[r:REPLIED_TO]-(t:UserDataNode) RETURN count(r) AS cnt";
    let want = general(&g, src);
    let _ = rows(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, BORROWED), 0, "{c:?}");
}
