#![allow(non_snake_case)]
//! Fix 73: three levers on the production CommunityPost listing
//! (`MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User
//! {userId: $u}) WITH p, r IS NOT NULL AS relevant RETURN properties(p) AS
//! p, relevant ORDER BY relevant DESC, p.createdAt DESC LIMIT 8` — 176 ms
//! against Neo4j's 55 on the mirror; locally 4,000 posts decoded in FULL,
//! 12,100 relationship records read, 12,100 peers projected, for 8 rows):
//!
//! 1. a hop end with a VAR-FREE one-key map on a declared key resolves
//!    ONCE into a sorted id set (`Graph::constant_end_ids`, memoised on the
//!    property's epoch) and every peer is a binary search — no record;
//! 2. a relationship variable read only for its presence (`r IS NOT NULL`,
//!    `count(r)`) binds LEAN from the adjacency entry — no record;
//! 3. `properties(p)` in a top-k RETURN behind a bare `WITH p` carry joins
//!    the late-full class: `p` binds lean, the eight survivors hydrate.
//!
//! Every answer is checked against the same statement with property seeks
//! OFF (no end set) and with the columnar paths OFF.

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
    p.insert("userId".to_string(), s(user));
    p.insert("rank".to_string(), Value::Int(7));
    p.insert("limit".to_string(), Value::Int(8));
    p
}

fn rows(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(user))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, user: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    // The first run resolves and memoises; count the second.
    let _ = rows(g, src, user);
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, user));
    (r, trace.counters().clone())
}

/// The controls: no property seek (no end set), and no columnar paths.
fn controls(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    g.set_property_seek(false);
    let a = rows(g, src, user);
    g.set_property_seek(true);
    g.set_columnar_scans(false);
    let b = rows(g, src, user);
    g.set_columnar_scans(true);
    assert_eq!(a, b, "the two controls disagree on `{src}`");
    a
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const LEAN_REL: &str = "interp.matcher bound a lean relationship";
const RESOLVED: &str = "interp.matcher bound a hop end from the resolved end set";
const SET_RESOLVED: &str = "graph.constant end set resolved";
const SET_MEMO: &str = "graph.constant end set served from the memo";
const REL_FULL: &str = "graph.rels materialised in full";
const NODE_FULL: &str = "graph.nodes materialised in full";
const PROJECTED: &str = "graph.projected node materialisations";
const HYDRATED: &str = "interp.late projection re-materialised a carried node for a survivor";

/// 4,000 posts, 50 users; every post is relevant to three users
/// (`(pi*3+k) % 50`) and every 40th post to u7 as well — u7 is relevant
/// to 340 posts. Those every-40th edges carry `since`. `User.userId`
/// (string) and `User.rank` (int) are declared; `User.name` is read by
/// one variant.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX user_id FOR (n:User) ON (n.userId)");
    ddl(&g, "CREATE INDEX user_rank FOR (n:User) ON (n.rank)");
    let mut users = Vec::new();
    for ui in 0..50i64 {
        let mut um = BTreeMap::new();
        um.insert("userId".into(), s(&format!("u{ui}")));
        um.insert("name".into(), s(&format!("User {ui}")));
        um.insert("rank".into(), Value::Int(ui));
        users.push(g.create_node(&["User".into()], &um).expect("user"));
    }
    for pi in 0..4_000i64 {
        let mut pm = BTreeMap::new();
        pm.insert("id".into(), s(&format!("post-{pi}")));
        pm.insert("title".into(), s(&format!("Post number {pi}")));
        pm.insert("body".into(), s(&format!("body of post {pi}: {}", "lorem ipsum ".repeat(40))));
        pm.insert("author".into(), s(&format!("u{}", pi % 50)));
        pm.insert("createdAt".into(), s(&format!("2026-0{}-{:02}T{:02}:{:02}:00Z", 1 + (pi / 700) % 9, 1 + (pi / 24) % 28, pi % 24, (pi * 7) % 60)));
        let p = g.create_node(&["CommunityPost".into()], &pm).expect("post");
        for k in 0..3 {
            let u = users[((pi * 3 + k) % 50) as usize];
            g.create_rel(p, "RELEVANT_TO", u, &BTreeMap::new()).expect("rel");
        }
        if pi % 40 == 0 {
            let mut rm = BTreeMap::new();
            rm.insert("since".into(), s(&format!("2026-08-{:02}", 1 + (pi / 40) % 28)));
            g.create_rel(p, "RELEVANT_TO", users[7], &rm).expect("rel");
        }
    }
    g
}

const ORIG: &str = "MATCH (p:CommunityPost) \
    OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: $userId}) \
    WITH p, r IS NOT NULL AS relevant \
    RETURN properties(p) AS p, relevant ORDER BY relevant DESC, p.createdAt DESC LIMIT toInteger($limit)";

#[test]
fn a_the_listing_resolves_its_end_once_and_binds_lean() {
    let g = corpus();
    let want = controls(&g, ORIG, "u7");
    assert_eq!(want.len(), 8);
    let (got, c) = traced(&g, ORIG, "u7");
    assert_eq!(got, want);
    // Every top row is a relevant post with its full property map.
    for r in &got {
        assert_eq!(r[1], Value::Bool(true), "{r:?}");
        let Value::Map(m) = &r[0] else { panic!("not a map: {r:?}") };
        assert!(m.contains_key("body") && m.contains_key("title"), "{m:?}");
    }
    // 12,100 edges, each a lean relationship; 340 of their ends are the
    // sought user, bound from the resolved set; the set was memoised by
    // the first run; the eight survivors are the only full decodes.
    assert_eq!(count_of(&c, LEAN_REL), 12_100, "{c:?}");
    assert_eq!(count_of(&c, RESOLVED), 340, "{c:?}");
    assert!(count_of(&c, SET_MEMO) >= 1, "{c:?}");
    assert!(count_of(&c, SET_RESOLVED) <= 1, "{c:?}");
    assert_eq!(count_of(&c, REL_FULL), 0, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
    assert_eq!(count_of(&c, HYDRATED), 8, "{c:?}");
    assert!(count_of(&c, NODE_FULL) <= 8, "{c:?}");
    assert!(count_of(&c, "store.gets") <= 12, "{c:?}");

    // A user nobody is relevant to: an empty set, every row `false`.
    let src = ORIG;
    let want = controls(&g, src, "u-none");
    let (got, c) = traced(&g, src, "u-none");
    assert_eq!(got, want);
    assert_eq!(got.len(), 8);
    assert!(got.iter().all(|r| r[1] == Value::Bool(false)), "{got:?}");
    assert_eq!(count_of(&c, RESOLVED), 0, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
}

/// A relationship or end that IS read keeps its record (or its columns):
/// `r.since` reads the relationship, `u.name` reads the end.
#[test]
fn b_a_read_relationship_or_end_keeps_the_record() {
    let g = corpus();
    let src = "MATCH (p:CommunityPost) \
        OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: $userId}) \
        WITH p, r IS NOT NULL AS relevant, r.since AS since \
        RETURN p.id AS id, relevant, since ORDER BY relevant DESC, since ASC, id ASC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want);
    assert_eq!(got.len(), 8);
    assert_eq!(count_of(&c, LEAN_REL), 0, "a read relationship is never lean: {c:?}");
    assert!(count_of(&c, REL_FULL) >= 12_100, "{c:?}");
    // The end set still resolves the map (the relationship is what is read).
    assert_eq!(count_of(&c, RESOLVED), 340, "{c:?}");
    assert!(got[0][2] != Value::Null, "the top rows carry `since`: {got:?}");

    let src = "MATCH (p:CommunityPost) \
        OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(u:User {userId: $userId}) \
        WITH p, r IS NOT NULL AS relevant, u.name AS who \
        RETURN p.id AS id, relevant, who ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want);
    assert_eq!(got[0][2], s("User 7"), "{got:?}");
    assert_eq!(count_of(&c, LEAN_REL), 12_100, "{c:?}");
    // The set proves the map; the name is read through `mat_end` (a
    // projected get or the cached column), never a bare bind.
    assert_eq!(count_of(&c, RESOLVED), 0, "{c:?}");
    assert!(count_of(&c, PROJECTED) <= 340, "{c:?}");
}

/// Shapes outside the class keep the per-peer test and agree: an integer
/// map value, a correlated map, a two-key map, an undeclared key, an
/// undirected hop.
#[test]
fn c_shapes_outside_the_class_decline_and_agree() {
    let g = corpus();
    for (src, lean_expected) in [
        // An integer value: the index probe and `=` disagree on 7 vs 7.0.
        ("MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {rank: $rank}) \
          WITH p, r IS NOT NULL AS relevant RETURN p.id AS id, relevant ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)", 12_100),
        // A correlated map: the value differs per row.
        ("MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: p.author}) \
          WITH p, r IS NOT NULL AS relevant RETURN p.id AS id, relevant ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)", 12_100),
        // Two keys.
        ("MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: $userId, rank: $rank}) \
          WITH p, r IS NOT NULL AS relevant RETURN p.id AS id, relevant ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)", 12_100),
        // An undeclared key.
        ("MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {name: 'User 7'}) \
          WITH p, r IS NOT NULL AS relevant RETURN p.id AS id, relevant ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)", 12_100),
    ] {
        let want = controls(&g, src, "u7");
        let (got, c) = traced(&g, src, "u7");
        assert_eq!(got, want, "{src}");
        assert_eq!(got.len(), 8, "{src}");
        assert_eq!(count_of(&c, RESOLVED), 0, "{src}: {c:?}");
        assert_eq!(count_of(&c, SET_RESOLVED) + count_of(&c, SET_MEMO), 0, "{src}: {c:?}");
        assert_eq!(count_of(&c, LEAN_REL), lean_expected, "{src}: {c:?}");
    }
    // An undirected hop: the adjacency entry does not say which side it
    // came from, so the relationship keeps its record — the end set (a
    // property of the peer, not of the direction) still applies.
    let src = "MATCH (p:CommunityPost) OPTIONAL MATCH (p)-[r:RELEVANT_TO]-(:User {userId: $userId}) \
        WITH p, r IS NOT NULL AS relevant RETURN p.id AS id, relevant ORDER BY relevant DESC, id ASC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want, "{src}");
    assert_eq!(count_of(&c, LEAN_REL), 0, "{c:?}");
    assert_eq!(count_of(&c, RESOLVED), 340, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
}

/// A relationship property read ONLY by the top-k RETURN: the late
/// projection must not defer it (the projector re-materialises nodes, never
/// relationships), so the relationship keeps its record and the property
/// arrives. Found by `pipeline_relvar` on the first whole-crate run.
#[test]
fn e_a_relationship_property_read_by_the_top_k_is_not_deferred() {
    let g = corpus();
    let src = "MATCH (p:CommunityPost) \
        OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: $userId}) \
        RETURN p.id AS id, r.since AS since ORDER BY since ASC, id ASC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want);
    assert_eq!(got.len(), 8);
    assert!(got.iter().all(|r| r[1] != Value::Null), "every top row carries `since`: {got:?}");
    assert_eq!(count_of(&c, LEAN_REL), 0, "{c:?}");
    // The general path alone (no columnar stage) answers the same.
    g.set_columnar_scans(false);
    let general = rows(&g, src, "u7");
    g.set_columnar_scans(true);
    assert_eq!(general, want);
}

/// The `EXISTS` and `count(r)` spellings take the same levers and agree.
#[test]
fn d_the_exists_and_count_spellings_agree() {
    let g = corpus();
    let src = "MATCH (p:CommunityPost) \
        WITH p, EXISTS { (p)-[:RELEVANT_TO]->(:User {userId: $userId}) } AS relevant \
        RETURN properties(p) AS p, relevant ORDER BY relevant DESC, p.createdAt DESC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESOLVED), 340, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
    assert!(count_of(&c, NODE_FULL) <= 8, "{c:?}");
    // The same eight posts as the OPTIONAL spelling.
    let (orig, _) = traced(&g, ORIG, "u7");
    assert_eq!(got, orig);

    let src = "MATCH (p:CommunityPost) \
        OPTIONAL MATCH (p)-[r:RELEVANT_TO]->(:User {userId: $userId}) \
        WITH p, count(r) AS n \
        RETURN p.id AS id, n ORDER BY n DESC, id ASC LIMIT toInteger($limit)";
    let want = controls(&g, src, "u7");
    let (got, c) = traced(&g, src, "u7");
    assert_eq!(got, want);
    assert_eq!(got[0][1], Value::Int(1), "{got:?}");
    assert_eq!(count_of(&c, REL_FULL), 0, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "{c:?}");
}
