#![allow(non_snake_case)]
//! Fix 58: the hop-bearing aggregate scan (`MATCH (a)-[:T]->(b) [WHERE …]
//! RETURN <aggregates>`) walked EVERY relationship of the type and gathered
//! both ends' columns over every distinct end, whoever the statement asked
//! about: the mentioned-entity aggregate over one user's emails walked all
//! 84k MENTIONS and gathered 38k emails' columns for a user who owns twenty
//! (2.5 s on the production mirror against Neo4j's 2 ms). A SOUGHT end —
//! a declared-key equality selecting under half its label — now drives the
//! population through its typed adjacency, and each end's columns are
//! served through its label's property-column cache, restricted to the
//! ends it reaches.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the general path).

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
    p.insert("userId".to_string(), Value::Str(user.to_string()));
    p
}

fn rows(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src, p);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const HOP_SCAN: &str = "interp.columnar hop aggregate scans";
const SEEDED: &str = "interp.columnar hop scan seeded from a sought end";
const FULL: &str = "graph.nodes materialised in full";
const GETS: &str = "store.gets";

/// 6,000 emails over 12 users (500 each — the label is well past the seek
/// floor, each user is a twelfth of it) with 2 KB bodies and a DECLARED
/// `userId` index; every third email mentions two of 600 entities (a label
/// past the seek floor too); one email in eight is quarantined.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX udn_user FOR (n:UserDataNode) ON (n.userId)");
    ddl(&g, "CREATE INDEX udn_type FOR (n:UserDataNode) ON (n.nodeType)");
    let body: String = "b".repeat(2048);
    let mut ents = Vec::new();
    for k in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Entity {k:03}")));
        m.insert(
            "type".to_string(),
            Value::Str(if k % 3 == 0 { "org".into() } else { "person".into() }),
        );
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str(format!("u{:02}", i % 12)));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        if i % 8 == 0 {
            m.insert("abuseStatus".to_string(), Value::Str("quarantined".into()));
        } else if i % 8 == 1 {
            m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
        }
        m.insert("rawData".to_string(), Value::Str(body.clone()));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        if i % 3 == 0 {
            for k in 0..2 {
                let e = ents[((i / 3 + k * 7) % 600) as usize];
                g.create_rel(n, "MENTIONS", e, &BTreeMap::new()).expect("mentions");
            }
        }
    }
    g
}

/// The production statement: the mentioned-entity aggregate over ONE
/// user's emails.
const ORIG: &str = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
    WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
    RETURN e.name AS name, e.type AS type, count(*) AS cnt ORDER BY cnt DESC, name LIMIT 20";

#[test]
fn a_sought_end_drives_the_hop_aggregate() {
    let g = corpus();
    let p = params("u03");
    let want = general(&g, ORIG, &p);
    assert_eq!(want.len(), 20, "fixture");
    let (got, c) = traced(&g, ORIG, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, HOP_SCAN), 1, "the hop scan ran: {c:?}");
    assert_eq!(count_of(&c, SEEDED), 1, "seeded from the sought user: {c:?}");
    assert_eq!(count_of(&c, FULL), 0, "no node decoded in full: {c:?}");
    // The user's 500 emails and the entities they reach — never the 6,000.
    assert!(
        count_of(&c, GETS) < 1200,
        "reads bounded by the sought end, not the label: {c:?}"
    );
}

/// The count forms the diagnostics decomposed the statement into — the
/// map-seeded hop count and the labelled far end — seed the same way.
/// (The WHERE-form spelling of the seed is the pipeline aggregate's, not
/// this scan's, and is not asserted here.)
#[test]
fn the_count_forms_seed_too() {
    let g = corpus();
    let p = params("u07");
    for src in [
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) RETURN count(*) AS n",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e:Entity) RETURN count(*) AS n",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e:Entity) \
         WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean'] \
         RETURN e.type AS type, count(*) AS n ORDER BY type",
    ] {
        let want = general(&g, src, &p);
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, HOP_SCAN), 1, "`{src}` ran the hop scan: {c:?}");
        assert_eq!(count_of(&c, SEEDED), 1, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}`: {c:?}");
    }
}

/// A user who owns NOTHING seeds an empty population and answers empty —
/// no walk of the type at all.
#[test]
fn an_unknown_user_answers_empty_without_the_walk() {
    let g = corpus();
    let p = params("nobody");
    // The general path first: it builds the declared index on its first
    // probe (a read per member), which is the index's cost, not the walk's.
    assert!(general(&g, ORIG, &p).is_empty());
    let (got, c) = traced(&g, ORIG, &p);
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(count_of(&c, SEEDED), 1, "{c:?}");
    assert_eq!(count_of(&c, GETS), 0, "nothing read for an empty seek: {c:?}");
}

/// The whole-type forms never seed: no equality on a declared key (the
/// all-users aggregate), an equality that selects most of the label
/// (`nodeType = 'email'` names every email), and a relationship the
/// statement reads (`type(r)`, `r.x`) — the adjacency carries no
/// relationship record. (Which columnar path answers each is that path's
/// business; none of them is the seeded walk.)
#[test]
fn the_whole_type_forms_never_seed() {
    let g = corpus();
    let p = params("u01");
    for src in [
        "MATCH (n:UserDataNode)-[:MENTIONS]->(e:Entity) RETURN e.type AS type, count(*) AS n ORDER BY type",
        "MATCH (n:UserDataNode {nodeType: 'email'})-[:MENTIONS]->(e) RETURN count(*) AS n",
        "MATCH (n:UserDataNode {nodeType: 'email'})-[:MENTIONS]->(e) RETURN e.type AS type, count(*) AS n ORDER BY type",
        "MATCH (n:UserDataNode {userId: $userId})-[r:MENTIONS]->(e) RETURN type(r) AS t, count(*) AS n",
        "MATCH (n:UserDataNode {userId: $userId})-[r:MENTIONS]->(e) RETURN count(r.since) AS n",
    ] {
        let want = general(&g, src, &p);
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 0, "`{src}` keeps the type walk: {c:?}");
    }
}

/// A FAR end sought by name is the pipeline aggregate's statement — its
/// anchored seed drives the reversed path — and never reaches this scan
/// (with or without a label on the start); the answer agrees either way.
#[test]
fn a_sought_far_end_is_the_pipelines() {
    let g = corpus();
    ddl(&g, "CREATE INDEX ent_name FOR (e:Entity) ON (e.name)");
    let mut p = BTreeMap::new();
    p.insert("name".to_string(), Value::Str("Entity 005".into()));
    for src in [
        "MATCH (n:UserDataNode)-[:MENTIONS]->(e:Entity {name: $name}) \
         WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
         RETURN n.userId AS userId, count(*) AS cnt ORDER BY userId",
        "MATCH (n)-[:MENTIONS]->(e:Entity {name: $name}) \
         WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
         RETURN n.userId AS userId, count(*) AS cnt ORDER BY userId",
    ] {
        let want = general(&g, src, &p);
        assert!(!want.is_empty(), "`{src}`");
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 0, "`{src}`: {c:?}");
        assert!(count_of(&c, "interp.pipeline aggregate runs") > 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}`: {c:?}");
    }
}
