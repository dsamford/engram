#![allow(non_snake_case)]
//! Fix 71: an UNDECLARED key's seek probes the PARTITION-WIDE index, and its
//! answer only wins when it is no wider than the label — yet the fallback
//! probe ran UNCAPPED and extracted every id before the label won. The
//! production UserTrack listing (`MATCH (n:UserTrack {userId: $userId})
//! OPTIONAL MATCH (n)-[:PERFORMED_BY]->(a:UserArtist) RETURN properties(n)
//! …`, no index on either engine) pulled the user's ~20k ids across every
//! label out of the `userId` index to keep none of them for an 834-member
//! label: 1.8 ms against Neo4j's 0.5 for 0 rows. The probe is now capped
//! at the label's size + 1.
//!
//! Every answer is checked against the same statement with the property
//! seek OFF (the label scan).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.into()));
    p
}

fn rows(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(user))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, user: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, user));
    (r, trace.counters().clone())
}

fn scanned(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    g.set_property_seek(false);
    let r = rows(g, src, user);
    g.set_property_seek(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const CAPPED: &str = "interp.seed undeclared probe capped at the label";
const GETS: &str = "store.gets";

/// 800 tracks over users u0..u3 (200 each) and 20,000 `Other` nodes over
/// the same users (5,000 each) — the partition-wide `userId` index answers
/// 5,200 ids for u1, the UserTrack label holds 800. User u9 owns three
/// tracks and nothing else. No index is declared anywhere.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut am = BTreeMap::new();
    am.insert("title".into(), s("The Artist"));
    let artist = g.create_node(&["UserArtist".into()], &am).expect("artist");
    for i in 0..800i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("track-{i}")));
        m.insert("userId".into(), s(&format!("u{}", i % 4)));
        m.insert("createdAt".into(), s(&format!("2026-08-{:02}T00:{:02}:{:02}Z", 1 + (i / 60) % 28, (i / 60) % 60, i % 60)));
        let t = g.create_node(&["UserTrack".into()], &m).expect("track");
        if i % 3 == 0 {
            g.create_rel(t, "PERFORMED_BY", artist, &BTreeMap::new()).expect("performed");
        }
    }
    for i in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("track-u9-{i}")));
        m.insert("userId".into(), s("u9"));
        m.insert("createdAt".into(), s(&format!("2026-09-01T00:00:0{i}Z")));
        g.create_node(&["UserTrack".into()], &m).expect("track");
    }
    for i in 0..20_000i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".into(), s(&format!("u{}", i % 4)));
        m.insert("k".into(), Value::Int(i));
        g.create_node(&["Other".into()], &m).expect("other");
    }
    g
}

const ORIG: &str = "MATCH (n:UserTrack {userId: $userId}) \
    OPTIONAL MATCH (n)-[:PERFORMED_BY]->(a:UserArtist) \
    RETURN properties(n) AS n, a.title AS artist ORDER BY n.createdAt DESC";

#[test]
fn a_a_probe_wider_than_the_label_is_capped_and_the_label_scans() {
    let g = corpus();
    let want = scanned(&g, ORIG, "u1");
    assert_eq!(want.len(), 200);
    // The partition-wide index is built on the first probe; count the second run.
    let _ = rows(&g, ORIG, "u1");
    let (got, c) = traced(&g, ORIG, "u1");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, CAPPED), 1, "{c:?}");
    // The label's 803 members are what the scan reads, never the 5,200 ids
    // the uncapped probe extracted (a record read per candidate on the
    // paged mirror).
    assert!(count_of(&c, GETS) <= 1_100, "{c:?}");

    // 0 rows for a user of Others only: the same cap, the same scan.
    let (got, c) = traced(&g, ORIG, "u2");
    assert_eq!(got.len(), 200);
    assert_eq!(count_of(&c, CAPPED), 1, "{c:?}");
}

/// A probe that answers UNDER the label's size still seeks: user u9's three
/// tracks are read alone, not the label.
#[test]
fn b_a_narrow_probe_still_seeks() {
    let g = corpus();
    let want = scanned(&g, ORIG, "u9");
    assert_eq!(want.len(), 3);
    let _ = rows(&g, ORIG, "u9");
    let (got, c) = traced(&g, ORIG, "u9");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, CAPPED), 0, "{c:?}");
    assert!(count_of(&c, GETS) <= 12, "three sought tracks, not a label scan: {c:?}");
}
