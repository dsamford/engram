#![allow(non_snake_case)]
//! Fix 48: when the FIRST hop's type has far fewer edges than the seed label
//! has members, the pipeline seeds the chain from the hop table's sources —
//! the only members that can expand to anything — and runs the seed's own
//! predicates over that population, instead of scanning the label, judging
//! every member and expanding every one of them to nothing.
//!
//! On the mirror `MATCH (n:UserDataNode {userId: $u})-[r:REPLIED_TO]->(t)
//! RETURN count(r)`: the `userId` seek named 38,297 of 38,614 emails (over
//! the cap), so the seed scanned the label and evaluated the predicate 38,337
//! times for a REPLIED_TO table holding a handful of edges — 9–14 ms against
//! Neo4j's 1.0–1.7, which drives from the relationship type.
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
    p.insert("u".to_string(), Value::Str("me".to_string()));
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

const SEEDED: &str = "interp.pipeline seed driven from the hop's table";
const SCANNED: &str = "interp.pipeline anchored seed scanned the whole label";
const EXPRS: &str = "cypher.expressions evaluated";

/// 6,000 mails, 5,990 of them `userId: 'me'` (the seek names almost the whole
/// label, as the mirror's did) with a DECLARED `userId` index; a SPARSE
/// `REPLIED_TO` (7 edges, 5 from 'me' mails, 2 from the others) and a DENSE
/// `HAS_TAG` (one per mail) as the control.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX mail_user FOR (n:Mail) ON (n.userId)");
    let mut mails = Vec::new();
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:04}")));
        m.insert(
            "userId".to_string(),
            Value::Str(if i % 600 == 599 { "other" } else { "me" }.to_string()),
        );
        m.insert("subject".to_string(), Value::Str(format!("subject {i}")));
        mails.push(g.create_node(&["Mail".into()], &m).expect("mail"));
    }
    let tag = g
        .create_node(&["Tag".into()], &BTreeMap::new())
        .expect("tag");
    for (i, m) in mails.iter().enumerate() {
        g.create_rel(*m, "HAS_TAG", tag, &BTreeMap::new()).expect("tag rel");
        // 5 replies from 'me' mails (i = 100, 1100, …), 2 from 'other' mails.
        if i % 1000 == 100 && i < 5000 {
            g.create_rel(*m, "REPLIED_TO", mails[i + 1], &BTreeMap::new())
                .expect("reply");
        }
        if i == 599 || i == 1199 {
            g.create_rel(*m, "REPLIED_TO", mails[i - 1], &BTreeMap::new())
                .expect("reply");
        }
    }
    g
}

#[test]
fn a_sparse_first_hop_seeds_the_chain_from_its_table() {
    let g = corpus();
    for (src, want) in [
        ("MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]->(t:Mail) RETURN count(r) AS cnt", vec![vec![Value::Int(5)]]),
        ("MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]->(t:Mail) RETURN count(DISTINCT t.nodeId) AS unique", vec![vec![Value::Int(5)]]),
        ("MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]->(t) RETURN count(r) AS cnt", vec![vec![Value::Int(5)]]),
        ("MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]->(t:Mail) RETURN count(r) AS cnt, count(DISTINCT t.nodeId) AS unique", vec![vec![Value::Int(5), Value::Int(5)]]),
        // The other direction reads the in-table.
        ("MATCH (t:Mail {userId: $u})<-[r:REPLIED_TO]-(n:Mail) RETURN count(r) AS cnt", vec![vec![Value::Int(7)]]),
        // A WHERE-form predicate on the seed, and the 'other' user.
        ("MATCH (n:Mail)-[r:REPLIED_TO]->(t:Mail) WHERE n.userId = 'other' RETURN count(r) AS cnt", vec![vec![Value::Int(2)]]),
        // A grouped aggregate: the seven sources, one row each.
        ("MATCH (n:Mail)-[r:REPLIED_TO]->(t:Mail) RETURN n.nodeId AS id, count(r) AS c ORDER BY id", vec![
            vec![Value::Str("mail-0100".into()), Value::Int(1)],
            vec![Value::Str("mail-0599".into()), Value::Int(1)],
            vec![Value::Str("mail-1100".into()), Value::Int(1)],
            vec![Value::Str("mail-1199".into()), Value::Int(1)],
            vec![Value::Str("mail-2100".into()), Value::Int(1)],
            vec![Value::Str("mail-3100".into()), Value::Int(1)],
            vec![Value::Str("mail-4100".into()), Value::Int(1)],
        ]),
    ] {
        assert_eq!(general(&g, src), want, "general path: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, SEEDED) > 0, "`{src}` seeds from the table: {c:?}");
        assert_eq!(count_of(&c, SCANNED), 0, "`{src}` scans no label: {c:?}");
        assert!(
            count_of(&c, EXPRS) < 100,
            "`{src}` judges the table's sources, not 6,000 members: {c:?}"
        );
    }
}

/// A type with NO edge seeds nothing: the mirror's REPLIED_TO holds zero
/// edges and the seed still scanned 38k emails for it on v116.
#[test]
fn an_edgeless_type_seeds_nothing() {
    let g = corpus();
    // Mint the type on an unrelated pair, then delete the edge: the type
    // exists with zero edges (an unminted type takes the empty-chunk route
    // before any seed and proves nothing here).
    let a = g.create_node(&["Tag".into()], &BTreeMap::new()).expect("a");
    let b = g.create_node(&["Tag".into()], &BTreeMap::new()).expect("b");
    let r = g.create_rel(a, "FORWARDED_TO", b, &BTreeMap::new()).expect("rel");
    g.delete_rel(r).expect("delete");
    for src in [
        "MATCH (n:Mail {userId: $u})-[r:FORWARDED_TO]->(t:Mail) RETURN count(r) AS cnt",
        "MATCH (n:Mail {userId: $u})-[r:FORWARDED_TO]->(t) RETURN count(r) AS cnt, count(DISTINCT t.nodeId) AS unique",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, "interp.pipeline seed emptied by an edgeless type") > 0, "{c:?}");
        assert_eq!(count_of(&c, SCANNED), 0, "`{src}` scans no label: {c:?}");
        assert!(count_of(&c, EXPRS) < 10, "`{src}` judges nothing: {c:?}");
    }
}

/// CONTROLS: a DENSE type (one edge per member) keeps the label seed; an
/// undirected, an untyped and a variable-length first hop are left alone.
#[test]
fn a_dense_untyped_undirected_or_variable_length_first_hop_keeps_the_label_seed() {
    let g = corpus();
    for src in [
        "MATCH (n:Mail {userId: $u})-[r:HAS_TAG]->(t:Tag) RETURN count(r) AS cnt",
        "MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]-(t:Mail) RETURN count(r) AS cnt",
        "MATCH (n:Mail {userId: $u})-[r]->(t:Mail) RETURN count(r) AS cnt",
        "MATCH (n:Mail {userId: $u})-[:REPLIED_TO*1..2]->(t:Mail) RETURN count(t) AS cnt",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 0, "`{src}` is left as written: {c:?}");
    }
}

/// The table seed is a population, never an answer: a relationship deleted
/// (or added) in the same transaction is seen exactly as the general path
/// sees it, because a writing transaction keeps the tables out and the
/// label seed answers.
#[test]
fn a_write_in_flight_keeps_the_label_seed() {
    let g = corpus();
    let src = "MATCH (n:Mail {userId: $u})-[r:REPLIED_TO]->(t:Mail) RETURN count(r) AS cnt";
    // Warm the table on a committed graph first.
    let (before, c) = traced(&g, src);
    assert_eq!(before, vec![vec![Value::Int(5)]]);
    assert!(count_of(&c, SEEDED) > 0);
    g.begin_txn().expect("begin");
    ddl(&g, "MATCH (n:Mail {nodeId: 'mail-0100'})-[r:REPLIED_TO]->() DELETE r");
    let (during, c) = traced(&g, src);
    assert_eq!(during, vec![vec![Value::Int(4)]], "the deleted edge is gone inside the txn");
    assert_eq!(count_of(&c, SEEDED), 0, "a writing txn keeps the tables out: {c:?}");
    g.rollback_txn();
    let (after, _) = traced(&g, src);
    assert_eq!(after, vec![vec![Value::Int(5)]]);
}
