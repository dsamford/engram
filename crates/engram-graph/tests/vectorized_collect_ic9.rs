#![allow(non_snake_case)]
//! Collect-fed LDBC IC9 (Layer-4 increment 3c) — the PRODUCTION stage-2 shape,
//! reached through its own stage-1:
//!
//! ```cypher
//! MATCH (person:Person {id: X})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend
//! WITH collect(DISTINCT friend) AS friends
//! UNWIND friends AS friend
//! MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < Y
//! RETURN friend.id, message.id, message.content, message.creationDate
//! ORDER BY message.creationDate DESC, message.id ASC LIMIT k
//! ```
//!
//! The operator runs the PREFIX (through `collect(DISTINCT friend) AS friends`)
//! on the GENERAL path to materialise `friends`, then hands that node list to
//! the SAME stage-2 core the `$param` UNWIND operator uses. So the whole query
//! is exact by composition, and this file proves it by differential: columnar
//! ON (this operator) must equal columnar OFF (the entire query on the general
//! per-tuple path) — the full ROW SET and its ORDER.
//!
//! The tie test pins the load-bearing bit — stage-2's REVERSE adjacency, which
//! decides a `creationDate` tie group cut by LIMIT. Breaking `.rev()` diverges
//! (the canary), which also proves the operator FIRES: a decline would run the
//! general path for ON too, and the vectorized `.rev()` could not matter.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Person p0 is the seed. KNOWS (undirected `*1..2`): p0–p1, p1–p2, p0–p3,
/// p0–p5, so p0's friends = {p1, p2, p3, p5} (p2 at two hops; p0 itself excluded
/// by `NOT person = friend`). p4 is isolated (empty-friends seed). p5 is a
/// friend with NO messages. HAS_CREATOR `(m)-[:HAS_CREATOR]->(person)`:
/// p1 authored m0,m1,m2 — all creationDate 50, distinct content (the tie group
/// cut by LIMIT); p2 authored m3(10),m4(20, DOUBLED),m6(30, null content);
/// p3 authored m5 (null creationDate).
struct G {
    g: Graph,
    #[allow(dead_code)]
    p: [u64; 6],
    #[allow(dead_code)]
    m: [u64; 7],
}

fn gt() -> G {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_p = |id: i64| {
        let mut pr = BTreeMap::new();
        pr.insert("id".to_string(), Value::Int(id));
        g.create_node(&["Person".into()], &pr).expect("p")
    };
    let p = [mk_p(0), mk_p(1), mk_p(2), mk_p(3), mk_p(4), mk_p(5)];
    let mk_m = |id: i64, cd: Option<i64>, content: Option<&str>| {
        let mut pr = BTreeMap::new();
        pr.insert("id".to_string(), Value::Int(id));
        if let Some(v) = cd {
            pr.insert("creationDate".to_string(), Value::Int(v));
        }
        if let Some(s) = content {
            pr.insert("content".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["Message".into()], &pr).expect("m")
    };
    let m = [
        mk_m(0, Some(50), Some("p")), // p1 — tie group
        mk_m(1, Some(50), Some("q")), // p1 — tie group
        mk_m(2, Some(50), Some("r")), // p1 — tie group
        mk_m(3, Some(10), Some("a")), // p2
        mk_m(4, Some(20), Some("b")), // p2 — DOUBLED edge
        mk_m(5, None, Some("z")),     // p3 — null creationDate
        mk_m(6, Some(30), None),      // p2 — null content
    ];
    // KNOWS (created directed; queried undirected `-[:KNOWS*1..2]-`).
    for (s, d) in [(0, 1), (1, 2), (0, 3), (0, 5)] {
        g.create_rel(p[s], "KNOWS", p[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    // HAS_CREATOR: (message)-[:HAS_CREATOR]->(person).
    for (msg, person) in [
        (0, 1),
        (1, 1),
        (2, 1),
        (3, 2),
        (4, 2),
        (4, 2), // doubled
        (6, 2),
        (5, 3),
    ] {
        g.create_rel(m[msg], "HAS_CREATOR", p[person], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    G { g, p, m }
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Vectorized operator ON vs the whole query on the general per-tuple path OFF.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// The production IC9 body, seeded on person `$pid` with threshold `$cd` and a
/// given ORDER BY / LIMIT tail.
fn ic9(order_limit: &str) -> String {
    format!(
        "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
         WITH collect(DISTINCT friend) AS friends \
         UNWIND friends AS friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < $cd \
         RETURN friend.id AS fid, message.id AS mid, message.content AS content, \
         message.creationDate AS cd {order_limit}"
    )
}

fn params(pid: i64, cd: i64) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("pid".to_string(), Value::Int(pid));
    p.insert("cd".to_string(), Value::Int(cd));
    p
}

#[test]
fn collect_ic9_matches_general_across_shapes() {
    let G { g, .. } = gt();
    // The production ORDER BY is strict (creationDate DESC, id ASC), so the
    // result is order-independent — but every LIMIT depth, direction and NULL
    // case must still match the general path row-for-row.
    let tails: &[&str] = &[
        "ORDER BY message.creationDate DESC, message.id ASC LIMIT 20",
        "ORDER BY message.creationDate DESC, message.id ASC LIMIT 1",
        "ORDER BY message.creationDate DESC, message.id ASC LIMIT 3",
        "ORDER BY message.creationDate DESC, message.id ASC SKIP 2 LIMIT 3",
        // ASC — the (dropped-by-WHERE) null creationDate never appears here.
        "ORDER BY message.creationDate ASC, message.id ASC LIMIT 100",
        // A friend-side key in the ORDER BY.
        "ORDER BY friend.id ASC, message.id ASC LIMIT 5",
    ];
    for t in tails {
        let src = ic9(t);
        // Threshold 100 admits every non-null creationDate (10..50); the null
        // (m5) is dropped by `< 100`, matching the general path.
        let (on, off) = both(&g, &src, params(0, 100));
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
        // A tighter threshold that excludes the cd=50 tie group entirely.
        let (on2, off2) = both(&g, &src, params(0, 40));
        assert_eq!(on2, off2, "columnar vs general disagree (cd<40): `{src}`");
    }
}

#[test]
fn collect_ic9_empty_friends_and_no_message_friend() {
    let G { g, .. } = gt();
    // Seed p4 is isolated → friends = [] → UNWIND yields nothing → empty result.
    let src = ic9("ORDER BY message.creationDate DESC, message.id ASC LIMIT 20");
    let (on, off) = both(&g, &src, params(4, 100));
    assert_eq!(on, off, "empty friends must match (and be empty)");
    assert!(
        on.is_empty(),
        "isolated seed must yield no rows, got {on:?}"
    );
    // Seed p0 includes p5 (a friend with no messages); it simply contributes no
    // rows — covered by the main test, asserted non-empty here as a sanity check.
    let (on0, _) = both(&g, &src, params(0, 100));
    assert!(!on0.is_empty(), "p0 has message-bearing friends");
}

#[test]
fn collect_ic9_tie_group_resolves_like_the_general_path() {
    // ORDER BY creationDate DESC ONLY — no id tiebreak — so the cd=50 trio
    // (m0,m1,m2, all under friend p1) ties, and which survive under LIMIT is
    // decided PURELY by production order (friends-list order × stage-2 REVERSE
    // adjacency). This is the canary target: break stage-2 `.rev()` and ON
    // diverges from the general path here.
    let G { g, .. } = gt();
    for k in 1..=3 {
        let src = ic9(&format!("ORDER BY message.creationDate DESC LIMIT {k}"));
        let (on, off) = both(&g, &src, params(0, 100));
        assert_eq!(
            on, off,
            "tie group under LIMIT {k} must match the general path"
        );
    }
}

#[test]
fn collect_ic9_declines_and_falls_back_identically() {
    let G { g, .. } = gt();
    let tail = "RETURN friend.id AS fid, message.id AS mid, message.creationDate AS cd \
                ORDER BY message.creationDate DESC, message.id ASC LIMIT 20";
    // Each of these DECLINES to the general path; ON must still equal OFF
    // (both run the whole query on the general path).
    let declines: &[String] = &[
        // A grouping key beside the collect → prefix is multi-row → decline.
        format!(
            "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
             WITH count(friend) AS c, collect(DISTINCT friend) AS friends \
             UNWIND friends AS friend \
             MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < $cd {tail}"
        ),
        // A non-DISTINCT collect → decline.
        format!(
            "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
             WITH collect(friend) AS friends \
             UNWIND friends AS friend \
             MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < $cd {tail}"
        ),
        // WHERE over the friend (f) side in stage-2 → decline.
        format!(
            "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
             WITH collect(DISTINCT friend) AS friends \
             UNWIND friends AS friend \
             MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE friend.id > 0 {tail}"
        ),
        // Aggregation in the RETURN → decline. (Its own RETURN, so no `tail`.)
        "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
         WITH collect(DISTINCT friend) AS friends \
         UNWIND friends AS friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < $cd \
         RETURN friend.id AS fid, count(message) AS c ORDER BY friend.id LIMIT 20"
            .to_string(),
        // A labelled UNWIND-bound start → decline.
        format!(
            "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
             WITH collect(DISTINCT friend) AS friends \
             UNWIND friends AS friend \
             MATCH (friend:Person)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < $cd {tail}"
        ),
        // A second pattern after UNWIND (two paths) → decline.
        format!(
            "MATCH (person:Person {{id: $pid}})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend \
             WITH collect(DISTINCT friend) AS friends \
             UNWIND friends AS friend \
             MATCH (friend)<-[:HAS_CREATOR]-(message:Message), (message)-[:HAS_CREATOR]->(z) \
             WHERE message.creationDate < $cd {tail}"
        ),
    ];
    for src in declines {
        let (on, off) = both(&g, src, params(0, 100));
        assert_eq!(on, off, "decline must fall back identically: `{src}`");
    }
}
