#![allow(non_snake_case)]
//! Fix 45: a top-level `type(r) IN [...]` / `type(r) = '…'` conjunct over an
//! UNTYPED hop folds into the hop's types before any path sees the
//! statement. `MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r)
//! IN ['FRIEND_OF', 'KNOWS', 'WORKS_WITH'] RETURN type(r), count(r)`
//! expanded a 38k-email seed's whole adjacency on the mirror — 6.8 s and
//! +771 MB against Neo4j's 127 ms.
//!
//! Every folded spelling is checked against the hand-typed hop AND against
//! the general path (columnar paths off); the spellings that must not fold
//! are checked against the general path only.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const FOLDED: &str = "interp.type filter folded into its hop";

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
    p.insert(
        "types".to_string(),
        Value::List(vec![Value::Str("KNOWS".into()), Value::Str("WORKS_WITH".into())]),
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

/// 3,000 emails of one user (a DECLARED `userId` index seeds them), each
/// mentioning three entities; a few dozen FRIEND_OF / KNOWS / WORKS_WITH
/// edges to contacts, and HAS_ASK edges — the untyped hop's adjacency is
/// dominated by the types the filter drops.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX udn_user FOR (n:UserDataNode) ON (n.userId)");
    let mut ents = Vec::new();
    for k in 0..30i64 {
        let mut e = BTreeMap::new();
        e.insert("name".to_string(), Value::Str(format!("ent-{k}")));
        ents.push(g.create_node(&["Entity".into()], &e).expect("entity"));
    }
    let mut contacts = Vec::new();
    for k in 0..25i64 {
        let mut c = BTreeMap::new();
        c.insert("name".to_string(), Value::Str(format!("contact-{k:02}")));
        contacts.push(g.create_node(&["UserDataNode".into()], &c).expect("contact"));
    }
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str(if i % 5 == 4 { "u2".into() } else { "u1".into() }));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        for j in 0..3usize {
            g.create_rel(n, "MENTIONS", ents[(i as usize * 3 + j) % 30], &BTreeMap::new())
                .expect("mention");
        }
        if i % 40 == 0 {
            g.create_rel(n, "FRIEND_OF", contacts[(i / 40 % 25) as usize], &BTreeMap::new())
                .expect("friend");
        }
        if i % 53 == 0 {
            g.create_rel(n, "KNOWS", contacts[(i / 53 % 25) as usize], &BTreeMap::new()).expect("knows");
        }
        if i % 71 == 0 {
            g.create_rel(n, "WORKS_WITH", contacts[(i / 71 % 25) as usize], &BTreeMap::new())
                .expect("works");
        }
        if i % 7 == 0 {
            let mut a = BTreeMap::new();
            a.insert("deadline".to_string(), Value::Str(format!("2026-10-{:02}", 1 + (i % 28))));
            let ask = g.create_node(&["EmailAsk".into()], &a).expect("ask");
            g.create_rel(n, "HAS_ASK", ask, &BTreeMap::new()).expect("has ask");
        }
    }
    g
}

fn check_folded(g: &Graph, written: &str, typed: &str) {
    let want_typed = rows(g, typed);
    let want_general = general(g, written);
    assert_eq!(want_typed, want_general, "the typed spelling and the general path agree: `{written}`");
    let (got, c) = traced(g, written);
    assert_eq!(got, want_typed, "`{written}`");
    assert!(count_of(&c, FOLDED) > 0, "`{written}`: {c:?}");
}

#[test]
fn the_production_count_by_type_folds() {
    let g = corpus();
    check_folded(
        &g,
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) IN ['FRIEND_OF', 'KNOWS', 'WORKS_WITH'] RETURN type(r) AS relType, count(r) AS cnt ORDER BY relType",
        "MATCH (n:UserDataNode {userId: $u})-[r:FRIEND_OF|KNOWS|WORKS_WITH]->(t) RETURN type(r) AS relType, count(r) AS cnt ORDER BY relType",
    );
}

#[test]
fn an_equality_a_reversed_equality_and_a_conjunction_beside_it_fold() {
    let g = corpus();
    check_folded(
        &g,
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) = 'KNOWS' AND t.name STARTS WITH 'contact-0' RETURN n.nodeId AS id, t.name AS name ORDER BY id, name LIMIT 20",
        "MATCH (n:UserDataNode {userId: $u})-[r:KNOWS]->(t) WHERE t.name STARTS WITH 'contact-0' RETURN n.nodeId AS id, t.name AS name ORDER BY id, name LIMIT 20",
    );
    check_folded(
        &g,
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE 'WORKS_WITH' = type(r) RETURN count(*) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r:WORKS_WITH]->(t) RETURN count(*) AS c",
    );
    // A duplicated type in the list is one type.
    check_folded(
        &g,
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) IN ['KNOWS', 'KNOWS', 'FRIEND_OF'] RETURN count(r) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r:KNOWS|FRIEND_OF]->(t) RETURN count(r) AS c",
    );
}

/// CONTROL: the spellings whose complement is unbounded, a parameter list,
/// a disjunction, an empty list and a var-length hop are left as written
/// and still answer as the general path does.
#[test]
fn the_unfoldable_spellings_are_left_as_written() {
    let g = corpus();
    for src in [
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE NOT type(r) IN ['MENTIONS', 'HAS_ASK'] RETURN type(r) AS relType, count(r) AS cnt ORDER BY relType",
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) <> 'MENTIONS' RETURN count(r) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) IN $types RETURN count(r) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE (type(r) = 'KNOWS' OR t.name = 'ent-3') RETURN count(r) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) IN [] RETURN count(r) AS c",
        "MATCH (n:UserDataNode {userId: $u})-[r:MENTIONS]->(t) WHERE type(r) IN ['MENTIONS', 'KNOWS'] RETURN count(r) AS c",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, FOLDED), 0, "`{src}`: {c:?}");
    }
}
