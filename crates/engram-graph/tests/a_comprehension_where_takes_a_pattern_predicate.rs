#![allow(non_snake_case)]
//! Fix 104: the WHERE of a list comprehension, a pattern comprehension or a
//! list predicate is a predicate position wherever the comprehension sits,
//! so a bare relationship pattern is a pattern predicate there — as it is
//! in a clause WHERE. The production chat unread query wrote
//! `size([x IN msgs WHERE (x)-[:MENTIONS]->(u)])` in its RETURN and was
//! refused as UnexpectedSyntax: the one corpus statement engram could not
//! run at all.
//!
//! The answers are pinned by value; a bare pattern in a RETURN item is
//! still refused.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str("user-1".into()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Two users; a channel with six messages by user 2, of which the three
/// even ones mention user 1; a DM with two messages, one mentioning user 1.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let user = |g: &Graph, id: &str| {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(id.into()));
        g.create_node(&["User".into()], &m).expect("user")
    };
    let u1 = user(&g, "user-1");
    let _u2 = user(&g, "user-2");
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Str("chan-1".into()));
    let chan = g.create_node(&["ChatChannel".into()], &m).expect("chan");
    g.create_rel(u1, "MEMBER_OF", chan, &BTreeMap::new())
        .expect("member");
    for i in 0..6i64 {
        let mut mm = BTreeMap::new();
        mm.insert("channelId".to_string(), Value::Str("chan-1".into()));
        mm.insert("authorId".to_string(), Value::Str("user-2".into()));
        mm.insert(
            "createdAt".to_string(),
            Value::Str(format!("2026-09-01T00:00:{i:02}Z")),
        );
        let msg = g.create_node(&["Message".into()], &mm).expect("msg");
        if i % 2 == 0 {
            g.create_rel(msg, "MENTIONS", u1, &BTreeMap::new())
                .expect("mention");
        }
    }
    let mut d = BTreeMap::new();
    d.insert("id".to_string(), Value::Str("dm-1".into()));
    let dm = g.create_node(&["ChatDM".into()], &d).expect("dm");
    g.create_rel(u1, "MEMBER_OF", dm, &BTreeMap::new())
        .expect("member");
    for i in 0..2i64 {
        let mut mm = BTreeMap::new();
        mm.insert("dmId".to_string(), Value::Str("dm-1".into()));
        mm.insert("authorId".to_string(), Value::Str("user-2".into()));
        mm.insert(
            "createdAt".to_string(),
            Value::Str(format!("2026-09-02T00:00:{i:02}Z")),
        );
        let msg = g.create_node(&["Message".into()], &mm).expect("msg");
        if i == 1 {
            g.create_rel(msg, "MENTIONS", u1, &BTreeMap::new())
                .expect("mention");
        }
    }
    g
}

/// The production unread query: per scope, the unread count and the
/// mentions counted by a pattern predicate inside a list comprehension.
#[test]
fn a_the_unread_query_counts_its_mentions() {
    let g = corpus();
    let src = "MATCH (u:User {userId: $userId})-[mem:MEMBER_OF]->(t) \
        WHERE (mem.state IS NULL OR mem.state = 'active') AND (t:ChatChannel OR t:ChatDM) \
        WITH u, t, CASE WHEN t:ChatChannel THEN 'channelId:' + t.id ELSE 'dmId:' + t.id END AS scopeKey \
        OPTIONAL MATCH (u)-[:HAS_READ_STATE]->(rs:ChatReadState {scopeKey: scopeKey}) \
        WITH u, t, rs.lastReadAt AS lastReadAt \
        OPTIONAL MATCH (m:Message) WHERE (m.channelId = t.id OR m.dmId = t.id) AND m.authorId <> $userId \
        AND m.deletedAt IS NULL AND (lastReadAt IS NULL OR m.createdAt > lastReadAt) \
        WITH u, t, collect(m) AS msgs \
        RETURN t.id AS scopeId, [l IN labels(t) WHERE l IN ['ChatChannel','ChatDM']][0] AS scopeKind, \
        size(msgs) AS unread, size([x IN msgs WHERE (x)-[:MENTIONS]->(u)]) AS mentions ORDER BY scopeId";
    let got = rows(&g, src);
    assert_eq!(
        got,
        vec![
            vec![
                Value::Str("chan-1".into()),
                Value::Str("ChatChannel".into()),
                Value::Int(6),
                Value::Int(3)
            ],
            vec![
                Value::Str("dm-1".into()),
                Value::Str("ChatDM".into()),
                Value::Int(2),
                Value::Int(1)
            ],
        ]
    );
}

/// A list predicate and a pattern comprehension take a pattern predicate in
/// their WHERE too — and, since IC7, a bare pattern is a VALUE as well.
#[test]
fn b_list_predicates_and_pattern_comprehensions_take_it_and_a_value_position_does_not() {
    let g = corpus();
    let got = rows(
        &g,
        "MATCH (u:User {userId: $userId}) MATCH (m:Message) WITH u, collect(m) AS msgs \
         RETURN any(x IN msgs WHERE (x)-[:MENTIONS]->(u)) AS anyMention, all(x IN msgs WHERE (x)-[:MENTIONS]->(u)) AS allMention, \
         size([x IN msgs WHERE NOT (x)-[:MENTIONS]->(u) | x.createdAt]) AS quiet",
    );
    assert_eq!(
        got,
        vec![vec![Value::Bool(true), Value::Bool(false), Value::Int(4)]]
    );
    let got = rows(
        &g,
        "MATCH (u:User {userId: $userId})-[:MEMBER_OF]->(t:ChatChannel) \
         RETURN size([(m:Message)-[:MENTIONS]->(u) WHERE (m)-[:MENTIONS]->(u) AND m.channelId = t.id | m.createdAt]) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(3)]]);
    // A bare pattern in a RETURN used to be refused as openCypher
    // `UnexpectedSyntax`. IT IS NOW ACCEPTED, and this assertion inverted with
    // the change rather than surviving it: LDBC SNB Interactive IC7 publishes
    // `not((liker)-[:KNOWS]-(person)) AS isNew` in its RETURN, Neo4j accepts
    // it, and it was the one statement of twenty in the Interactive catalogue
    // this engine could not read. A pattern predicate evaluates to a BOOLEAN
    // through the same `exists` hook wherever it sits, so the refusal was
    // syntactic only.
    //
    // The comprehension and list-predicate cases above are the ones this file
    // exists for and they are unchanged; only the value-position clause moved.
    // See `a_pattern_predicate_is_a_value_not_only_a_filter` for the answers.
    let accepted = parse_statement("MATCH (u:User) RETURN (u)-[:MENTIONS]->() AS v");
    assert!(
        accepted.is_ok(),
        "a bare pattern in a value position is accepted now (IC7): {:?}",
        accepted.err()
    );
}
