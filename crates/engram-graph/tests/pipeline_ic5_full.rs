#![allow(non_snake_case)]
//! Differential test for the REAL 7-clause LDBC SNB IC5 (not the 5-clause
//! two-MATCH-join stand-in in `pipeline_ic5.rs`):
//!
//! ```cypher
//! MATCH (person:Person {id: 10})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend
//! WITH DISTINCT friend
//! MATCH (friend)<-[membership:HAS_MEMBER]-(forum) WHERE membership.joinDate > $cutoff
//! WITH forum, collect(friend) AS friends
//! OPTIONAL MATCH (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum) WHERE friend IN friends
//! WITH forum, count(post) AS postCount
//! RETURN forum.title AS forumName, postCount ORDER BY postCount DESC, forum.id ASC LIMIT 20
//! ```
//!
//! It adds THREE things the 5-clause stand-in lacks and that the recognizer must
//! reproduce byte-identically to `run_streaming`: (1) a rel-property filter
//! `membership.joinDate > cutoff` on the stage-2 HAS_MEMBER edge; (2) OPTIONAL
//! left-join semantics — a forum with member-friends but NO qualifying post is
//! still emitted with postCount 0; (3) `RETURN forum.title` (a non-id group-key
//! property). The fixture is built so ALL THREE are load-bearing: a membership
//! dropped by the cutoff, and a zero-post forum, both change the result.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Persons: me=10; friends f1=11, f2=12 (KNOWS 10->11->12, so undirected *1..2
/// from 10 reaches {11,12}, and `NOT person=friend` drops 10 itself).
/// Forums: FA(id 20,"A"), FB(id 21,"B"), FC(id 22,"C").
/// HAS_MEMBER (forum->friend, joinDate); cutoff is 1000:
///   FA->11 @2000 (kept), FA->12 @500 (DROPPED by cutoff), FB->12 @3000 (kept),
///   FC->11 @2000 (kept, but FC holds NO posts -> zero-count forum).
/// (forum, post, creator):
///   FA: p100 by 11 (11 kept member of FA -> counts), p101 by 12 (12's FA
///       membership was dropped -> NOT counted).
///   FB: p102 by 12 (member -> counts), p103 by 11 (11 not a member of FB -> no).
///   FC: none.
/// So IC5 (cutoff 1000, anchor 10) = FA:1, FB:1, FC:0 ->
///   ORDER BY postCount DESC, forum.id ASC -> [["A",1],["B",1],["C",0]].
fn gic5_full() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let me = mk("Person", &[("id", Value::Int(10))]);
    let f1 = mk("Person", &[("id", Value::Int(11))]);
    let f2 = mk("Person", &[("id", Value::Int(12))]);
    for (s, d) in [(me, f1), (f1, f2)] {
        g.create_rel(s, "KNOWS", d, &BTreeMap::new())
            .expect("KNOWS");
    }
    let fa = mk(
        "Forum",
        &[("id", Value::Int(20)), ("title", Value::Str("A".into()))],
    );
    let fb = mk(
        "Forum",
        &[("id", Value::Int(21)), ("title", Value::Str("B".into()))],
    );
    let fc = mk(
        "Forum",
        &[("id", Value::Int(22)), ("title", Value::Str("C".into()))],
    );
    let member = |forum: u64, friend: u64, jd: i64| {
        let mut e = BTreeMap::new();
        e.insert("joinDate".to_string(), Value::Int(jd));
        g.create_rel(forum, "HAS_MEMBER", friend, &e)
            .expect("HAS_MEMBER");
    };
    member(fa, f1, 2000);
    member(fa, f2, 500); // dropped by cutoff 1000
    member(fb, f2, 3000);
    member(fc, f1, 2000); // FC has a member but no posts
    let post = |id: i64, creator: u64, forum: u64| {
        let p = g
            .create_node(&["Post".into()], &{
                let mut m = BTreeMap::new();
                m.insert("id".to_string(), Value::Int(id));
                m
            })
            .expect("post");
        g.create_rel(p, "HAS_CREATOR", creator, &BTreeMap::new())
            .expect("creator");
        g.create_rel(forum, "CONTAINER_OF", p, &BTreeMap::new())
            .expect("container");
    };
    post(100, f1, fa);
    post(101, f2, fa);
    post(102, f2, fb);
    post(103, f1, fb);
    g
}

fn rows(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, params.clone())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

fn both(
    g: &Graph,
    src: &str,
    params: &BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params);
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Fell to the per-tuple `run_streaming` interp under columnar ON — the marker the
/// composite must NOT trip when it fires, and MUST when it declines.
fn streamed(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

/// Whether the full IC5 composite pipeline fired (its distinct operator counter).
fn ic5_fired(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .counters()
        .get("interp.pipeline ic5 runs")
        .copied()
        .unwrap_or(0)
        == 1
}

const IC5: &str = "MATCH (person:Person {id: 10})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
    WITH DISTINCT friend \
    MATCH (friend)<-[membership:HAS_MEMBER]-(forum) WHERE membership.joinDate > 1000 \
    WITH forum, collect(friend) AS friends \
    OPTIONAL MATCH (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum) WHERE friend IN friends \
    WITH forum, count(post) AS postCount \
    RETURN forum.title AS forumName, postCount ORDER BY postCount DESC, forum.id ASC LIMIT 20";

fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn i(n: i64) -> Value {
    Value::Int(n)
}

#[test]
fn real_ic5_on_equals_off() {
    let g = gic5_full();
    let (on, off) = both(&g, IC5, &BTreeMap::new());
    assert_eq!(on, off, "real IC5 columnar must equal the interp oracle");
    assert_eq!(
        on,
        vec![vec![s("A"), i(1)], vec![s("B"), i(1)], vec![s("C"), i(0)]],
        "FA:1, FB:1 (joinDate filter drops FA's p101), FC:0 (OPTIONAL emits the zero-count forum)"
    );
}

#[test]
fn real_ic5_fires_columnar() {
    let g = gic5_full();
    assert!(
        ic5_fired(&g, IC5, &BTreeMap::new()),
        "the real 7-clause IC5 must FIRE the composite pipeline"
    );
    assert!(
        !streamed(&g, IC5, &BTreeMap::new()),
        "the real 7-clause IC5 must NOT fall to run_streaming"
    );
}

/// DUP-MEMBERSHIP CANARY: the DEDUP to distinct `(friend, forum)` pairs is
/// load-bearing. A friend joined to the SAME forum by TWO HAS_MEMBER edges (both
/// past the cutoff) must not double-count that friend's posts — `collect`'s
/// `friend IN friends` is set membership, so each post counts ONCE. Without the
/// dedup the outer would carry the pair twice and the OPTIONAL inner would count
/// each post twice, so this ON==OFF differential (and the exact count) would fail.
#[test]
fn real_ic5_dedups_duplicate_memberships() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let me = mk("Person", &[("id", Value::Int(10))]);
    let f1 = mk("Person", &[("id", Value::Int(11))]);
    g.create_rel(me, "KNOWS", f1, &BTreeMap::new())
        .expect("KNOWS");
    let fa = mk(
        "Forum",
        &[("id", Value::Int(20)), ("title", Value::Str("A".into()))],
    );
    // f1 is a member of FA via TWO edges, both past the cutoff.
    for jd in [2000i64, 3000] {
        let mut e = BTreeMap::new();
        e.insert("joinDate".to_string(), Value::Int(jd));
        g.create_rel(fa, "HAS_MEMBER", f1, &e).expect("HAS_MEMBER");
    }
    // f1 authored ONE post in FA.
    let post = g
        .create_node(&["Post".into()], &{
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100));
            m
        })
        .expect("post");
    g.create_rel(post, "HAS_CREATOR", f1, &BTreeMap::new())
        .expect("creator");
    g.create_rel(fa, "CONTAINER_OF", post, &BTreeMap::new())
        .expect("container");

    let (on, off) = both(&g, IC5, &BTreeMap::new());
    assert_eq!(
        on, off,
        "dup-membership IC5 columnar must equal the interp oracle"
    );
    assert_eq!(
        on,
        vec![vec![s("A"), i(1)]],
        "the one post counts ONCE despite the friend's duplicate memberships"
    );
    assert!(
        ic5_fired(&g, IC5, &BTreeMap::new()),
        "the dup-membership IC5 must FIRE"
    );
}

/// MIXED-FORUM CANARY: within ONE forum, a member-friend WITH a post and a
/// member-friend WITHOUT one. The friend-with-post pair produces a real row; the
/// friend-without-post pair NULL-fills (contributing 0). `count(post)` over the
/// forum group must be 1 — not 2 (double-counted), not dropped by the null-fill.
/// This exercises the interleave of matched + null-filled pairs in ONE group.
#[test]
fn real_ic5_mixed_forum_matched_and_nullfilled_pairs() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    // me=10 KNOWS f1=11, f2=12 (both reached at 1 hop).
    let me = mk("Person", &[("id", Value::Int(10))]);
    let f1 = mk("Person", &[("id", Value::Int(11))]);
    let f2 = mk("Person", &[("id", Value::Int(12))]);
    for d in [f1, f2] {
        g.create_rel(me, "KNOWS", d, &BTreeMap::new())
            .expect("KNOWS");
    }
    let fa = mk(
        "Forum",
        &[("id", Value::Int(20)), ("title", Value::Str("A".into()))],
    );
    // BOTH f1 and f2 are kept members of FA.
    for friend in [f1, f2] {
        let mut e = BTreeMap::new();
        e.insert("joinDate".to_string(), Value::Int(2000));
        g.create_rel(fa, "HAS_MEMBER", friend, &e)
            .expect("HAS_MEMBER");
    }
    // Only f1 authored a post in FA; f2 authored none there.
    let post = g
        .create_node(&["Post".into()], &{
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100));
            m
        })
        .expect("post");
    g.create_rel(post, "HAS_CREATOR", f1, &BTreeMap::new())
        .expect("creator");
    g.create_rel(fa, "CONTAINER_OF", post, &BTreeMap::new())
        .expect("container");

    let (on, off) = both(&g, IC5, &BTreeMap::new());
    assert_eq!(
        on, off,
        "mixed-forum IC5 columnar must equal the interp oracle"
    );
    assert_eq!(
        on,
        vec![vec![s("A"), i(1)]],
        "count = 1: f1's post counts, f2's null-fill contributes 0, no double-count"
    );
    assert!(
        ic5_fired(&g, IC5, &BTreeMap::new()),
        "the mixed-forum IC5 must FIRE"
    );
}
