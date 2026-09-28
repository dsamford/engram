#![allow(non_snake_case)]
//! Fix 101: a hop over a relationship type with NO live relationship can
//! match nothing, so a MATCH that requires it yields no row before any
//! start is seeded, and an OPTIONAL hop over it never expands. The
//! production thread-depth aggregate, `MATCH (n:UserDataNode {userId,
//! nodeType: 'email'})-[:PART_OF_THREAD]->(t) WHERE … WITH t, count(n) AS
//! depth RETURN avg(depth), max(depth), count(t)`, seeded a user's 38k
//! emails and probed 18k of them for a type that holds no relationship on
//! the mirror: 68–110 ms against Neo4j's 1.3, for one row of nulls.
//!
//! The answers are pinned by value; a zero-length hop, a type with one
//! live relationship, and a relationship created by the same statement
//! keep the seeded walk.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.into()));
    p
}

fn rows(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    p: &BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("stmt");
}

const DEAD: &str =
    "interp.match over a relationship type with no live relationship matched nothing";
const SKIPPED: &str =
    "interp.expansion skipped a hop over a relationship type with no live relationship";
const GETS: &str = "store.gets";
const EXPRS: &str = "cypher.expressions evaluated";

/// The composite index, 3,000 emails of user 1 (`abuseStatus` on every
/// third), two threads of user 2, and PART_OF_THREAD minted by one edge
/// that is then deleted — the type exists and holds no relationship.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "CREATE INDEX udn_user_type FOR (n:UserDataNode) ON (n.userId, n.nodeType)",
    );
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("user-1".into()));
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        if i % 3 == 0 {
            m.insert(
                "abuseStatus".to_string(),
                Value::Str(if i % 9 == 0 { "rejected" } else { "clean" }.into()),
            );
        }
        g.create_node(&["UserDataNode".into()], &m).expect("email");
    }
    for k in ["a", "b"] {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Str(k.into()));
        m.insert("userId".to_string(), Value::Str("user-2".into()));
        g.create_node(&["Thread".into()], &m).expect("thread");
    }
    ddl(
        &g,
        "MATCH (a:Thread {k: 'a'}), (b:Thread {k: 'b'}) CREATE (a)-[:PART_OF_THREAD]->(b)",
    );
    ddl(&g, "MATCH (:Thread)-[r:PART_OF_THREAD]->() DELETE r");
    assert_eq!(
        rows(
            &g,
            "MATCH ()-[r:PART_OF_THREAD]->() RETURN count(r) AS n",
            &params("user-1")
        ),
        vec![vec![Value::Int(0)]],
        "fixture: the type is minted and empty"
    );
    g
}

const ORIG: &str = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:PART_OF_THREAD]->(t) \
    WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
    WITH t, count(n) AS depth RETURN avg(depth) AS avgDepth, max(depth) AS maxDepth, count(t) AS threadCount";

/// The thread-depth aggregate answers its row of nulls without seeding;
/// so does a plain count and a listing over the dead hop.
#[test]
fn a_a_match_over_the_dead_type_answers_before_seeding() {
    let g = corpus();
    let (got, c) = traced(&g, ORIG, &params("user-1"));
    assert_eq!(got, vec![vec![Value::Null, Value::Null, Value::Int(0)]]);
    assert_eq!(count_of(&c, DEAD), 1, "{c:?}");
    assert!(count_of(&c, GETS) < 20, "no seed, no probe: {c:?}");
    assert!(count_of(&c, EXPRS) < 20, "no row evaluated: {c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:PART_OF_THREAD]->(t) RETURN count(*) AS n",
        &params("user-1"),
    );
    assert_eq!(got, vec![vec![Value::Int(0)]]);
    assert!(count_of(&c, GETS) < 20, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:PART_OF_THREAD]->(t) RETURN n.nodeId AS id, t.k AS k ORDER BY id LIMIT 5",
        &params("user-1"),
    );
    assert!(got.is_empty());
    assert_eq!(count_of(&c, DEAD), 1, "{c:?}");
    assert!(count_of(&c, GETS) < 20, "{c:?}");
}

/// An OPTIONAL hop over the dead type keeps every row and never reads an
/// adjacency: the same rows as with a live type, whose walk reads one per
/// row. (The null-row path still costs two store reads per row either
/// way — pre-existing, and not this fix's.)
#[test]
fn b_an_optional_hop_over_the_dead_type_keeps_its_rows_and_never_expands() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'}) OPTIONAL MATCH (n)-[:PART_OF_THREAD]->(t) \
         RETURN count(n) AS emails, count(t) AS threads";
    let (got, c) = traced(&g, src, &params("user-1"));
    assert_eq!(got, vec![vec![Value::Int(3000), Value::Int(0)]]);
    assert_eq!(
        count_of(&c, DEAD),
        0,
        "an OPTIONAL hop is not a requirement: {c:?}"
    );
    assert_eq!(count_of(&c, SKIPPED), 3000, "{c:?}");
    let adjacency = |c: &BTreeMap<String, u64>| {
        c.iter()
            .filter(|(k, _)| k.starts_with("graph.adjacency"))
            .map(|(_, v)| *v)
            .sum::<u64>()
    };
    assert_eq!(adjacency(&c), 0, "no adjacency read for a dead type: {c:?}");
    ddl(
        &g,
        "MATCH (a:Thread {k: 'a'}), (b:Thread {k: 'b'}) CREATE (a)-[:PART_OF_THREAD]->(b)",
    );
    let (got, c) = traced(&g, src, &params("user-1"));
    assert_eq!(got, vec![vec![Value::Int(3000), Value::Int(0)]]);
    assert_eq!(count_of(&c, SKIPPED), 0, "a live type walks: {c:?}");
    assert!(
        adjacency(&c) >= 3000,
        "the live walk reads the adjacency per row: {c:?}"
    );
}

/// CONTROLS: a zero-length hop matches its start; a type with one live
/// relationship seeds as before; a relationship the statement itself
/// creates is matched by its later MATCH.
#[test]
fn c_a_zero_length_hop_a_live_type_and_a_same_statement_create_keep_the_walk() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:PART_OF_THREAD*0..1]->(t) RETURN count(*) AS n",
        &params("user-1"),
    );
    assert_eq!(got, vec![vec![Value::Int(3000)]]);
    assert_eq!(count_of(&c, DEAD) + count_of(&c, SKIPPED), 0, "{c:?}");
    ddl(
        &g,
        "MATCH (a:Thread {k: 'a'}), (b:Thread {k: 'b'}) CREATE (a)-[:PART_OF_THREAD]->(b)",
    );
    assert_eq!(
        rows(
            &g,
            "MATCH ()-[r:PART_OF_THREAD]->() RETURN count(r) AS n",
            &params("user-1")
        ),
        vec![vec![Value::Int(1)]],
        "fixture: one live relationship"
    );
    let (got, c) = traced(&g, ORIG, &params("user-1"));
    assert_eq!(got, vec![vec![Value::Null, Value::Null, Value::Int(0)]]);
    assert_eq!(
        count_of(&c, DEAD) + count_of(&c, SKIPPED),
        0,
        "one live relationship: {c:?}"
    );
    assert!(count_of(&c, EXPRS) >= 3000, "the seeds are walked: {c:?}");
    let got = rows(
        &g,
        "MATCH (a:Thread {k: 'a'})-[:PART_OF_THREAD]->(b) RETURN b.k AS k",
        &params("user-2"),
    );
    assert_eq!(got, vec![vec![Value::Str("b".into())]]);
    // Dead again, then created and matched inside one statement.
    ddl(&g, "MATCH (:Thread)-[r:PART_OF_THREAD]->() DELETE r");
    let (got, c) = traced(
        &g,
        "MATCH (a:Thread {k: 'a'}), (b:Thread {k: 'b'}) CREATE (a)-[:PART_OF_THREAD]->(b) \
         WITH a MATCH (a)-[:PART_OF_THREAD]->(x) RETURN x.k AS k",
        &params("user-2"),
    );
    assert_eq!(got, vec![vec![Value::Str("b".into())]]);
    assert_eq!(count_of(&c, DEAD) + count_of(&c, SKIPPED), 0, "{c:?}");
}
