#![allow(non_snake_case)]
//! Fix 103: a relationship variable read BY PROPERTY binds from the slim
//! adjacency entry plus a projected record read, where it used to take
//! `rels_of` — a walk of the start's adjacency prefix and a full decode of
//! every relationship record. The dashboard's membership hop, `MATCH
//! (p:KMProject) OPTIONAL MATCH (:User {userId: $userId})-[mm:MEMBER_OF]->(p)
//! RETURN properties(p), coalesce(mm.role, 'owner')`, paid 77 prefix scans
//! and 77 full decodes for 77 projects (4.9 ms against Neo4j's 2.5 on the
//! mirror). A peer outside the resolved end set is skipped before its frame.
//!
//! The rows are pinned by value; a whole-relationship output and a
//! variable-length hop keep the full walk.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

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

const PROJECTED: &str = "interp.matcher bound a relationship by a projected read";
const FULL_RELS: &str = "graph.rels materialised in full";
const CUT_TO_PEER: &str = "interp.expansion read only the edges to a known peer";
const SKIPPED_END: &str =
    "interp.expansion skipped a peer outside the resolved end set before its frame";

/// Eighty projects; user 1 is a member of the even forty (`role` owner or
/// member, `state` active), user 2 of every one; a third user of none.
/// `User.userId` is indexed as on the mirror — fix 73 resolves the hop's
/// constant end `(:User {userId: $userId})` to a set through it.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX user_id FOR (u:User) ON (u.userId)");
    let user = |g: &Graph, id: &str| {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(id.into()));
        g.create_node(&["User".into()], &m).expect("user")
    };
    let u1 = user(&g, "user-1");
    let u2 = user(&g, "user-2");
    let _u3 = user(&g, "user-3");
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:03}")));
        m.insert("name".to_string(), Value::Str(format!("Project {i}")));
        let p = g.create_node(&["KMProject".into()], &m).expect("project");
        if i % 2 == 0 {
            let mut r = BTreeMap::new();
            r.insert(
                "role".to_string(),
                Value::Str(if i % 4 == 0 { "owner" } else { "member" }.into()),
            );
            r.insert("state".to_string(), Value::Str("active".into()));
            g.create_rel(u1, "MEMBER_OF", p, &r).expect("member");
        }
        let mut r = BTreeMap::new();
        r.insert("role".to_string(), Value::Str("viewer".into()));
        g.create_rel(u2, "MEMBER_OF", p, &r).expect("member");
    }
    g
}

fn expected() -> Vec<Vec<Value>> {
    (0..80i64)
        .map(|i| {
            // No membership (odd) defaults to owner; a membership carries its role.
            let role = if i % 2 != 0 || i % 4 == 0 {
                "owner"
            } else {
                "member"
            };
            vec![Value::Str(format!("proj-{i:03}")), Value::Str(role.into())]
        })
        .collect()
}

const DASH: &str = "MATCH (p:KMProject) OPTIONAL MATCH (:User {userId: $userId})-[mm:MEMBER_OF]->(p) \
    RETURN p.id AS id, coalesce(mm.role, 'owner') AS myRole ORDER BY id";

/// The membership hop binds user 1's forty relationships by a projected
/// read, never touches user 2's eighty, and decodes none in full.
///
/// This assertion CHANGED when the expansion learned to cut the adjacency row
/// to a known peer. It used to require that user 2's eighty peers were
/// enumerated and SKIPPED before their frame
/// (`interp.expansion skipped a peer outside the resolved end set`); they are
/// now never read at all, so that counter reads 0 and
/// `interp.expansion read only the edges to a known peer` reads 80 instead.
/// Strictly less work for the identical answer — which the first assertion in
/// this test, on the rows themselves, is what actually guarantees.
#[test]
fn a_a_property_read_relationship_binds_from_the_adjacency() {
    let g = corpus();
    let (got, c) = traced(&g, DASH, &params("user-1"));
    assert_eq!(got, expected());
    assert_eq!(count_of(&c, PROJECTED), 40, "{c:?}");
    assert_eq!(count_of(&c, FULL_RELS), 0, "{c:?}");
    assert_eq!(
        count_of(&c, SKIPPED_END),
        0,
        "user 2's peers are not skipped any more, they are never read: {c:?}"
    );
    assert_eq!(
        count_of(&c, CUT_TO_PEER),
        80,
        "the row was cut to the known peer, once per project: {c:?}"
    );
    // A user with no membership: every role defaults, nothing is read.
    let (got, c) = traced(&g, DASH, &params("user-3"));
    assert!(got.iter().all(|r| r[1] == Value::Str("owner".into())));
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
    assert_eq!(count_of(&c, FULL_RELS), 0, "{c:?}");
}

/// CONTROLS: a whole-relationship output and a variable-length hop keep
/// the full decode — and answer the same memberships.
#[test]
fn b_a_whole_relationship_output_and_a_var_length_hop_keep_the_full_read() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (:User {userId: $userId})-[mm:MEMBER_OF]->(p:KMProject) RETURN p.id AS id, mm AS m ORDER BY id",
        &params("user-1"),
    );
    assert_eq!(got.len(), 40);
    let Value::Rel {
        props, rel_type, ..
    } = &got[0][1]
    else {
        panic!("{:?}", got[0][1])
    };
    assert_eq!(rel_type, "MEMBER_OF");
    assert_eq!(props.get("role"), Some(&Value::Str("owner".into())));
    assert_eq!(props.get("state"), Some(&Value::Str("active".into())));
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
    assert!(count_of(&c, FULL_RELS) >= 40, "{c:?}");
    let (got, c) = traced(
        &g,
        "MATCH (:User {userId: $userId})-[mm:MEMBER_OF*1..1]->(p:KMProject) RETURN count(*) AS n, count(mm) AS m",
        &params("user-1"),
    );
    assert_eq!(got, vec![vec![Value::Int(40), Value::Int(40)]]);
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
}
