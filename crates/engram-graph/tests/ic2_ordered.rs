//! IC2's date-ordered k-way-merge fast path proven byte-identical to the ordinary
//! expand + gather + top-k AND to the interp — on the shape it targets
//! (`(:Person{id})-[:KNOWS]-(friend)<-[:HAS_CREATOR]-(message) WHERE
//! message.creationDate <= T … ORDER BY creationDate DESC, id ASC LIMIT k`),
//! including a NON-friend creator (must be excluded), a date > T (WHERE-excluded),
//! a date TIE broken by message id, and more candidates than the LIMIT.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn node(g: &Graph, label: &str, props: &[(&str, Value)]) -> u64 {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    g.create_node(&[label.into()], &m).expect("node")
}
fn rel(g: &Graph, s: u64, t: &str, d: u64) {
    g.create_rel(s, t, d, &BTreeMap::new()).expect("rel");
}
fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap();
    run_query(g, &q, BTreeMap::new()).unwrap().rows
}
fn i(n: i64) -> Value {
    Value::Int(n)
}

/// (IC2 ordered ON, ordinary expand+gather+top-k, interp) — all three must agree.
fn three(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_ic2_ordered(true);
    let ordered = rows(g, src);
    g.set_ic2_ordered(false);
    let general = rows(g, src);
    g.set_ic2_ordered(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (ordered, general, interp)
}
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_ic2_ordered(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline ic2 ordered merge")
        .copied()
        .unwrap_or(0)
        > 0
}

fn ic2_graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p0 = node(&g, "Person", &[("id", i(10))]);
    let p1 = node(&g, "Person", &[("id", i(1))]);
    let p2 = node(&g, "Person", &[("id", i(2))]);
    let p3 = node(&g, "Person", &[("id", i(3))]);
    let p4 = node(&g, "Person", &[("id", i(4))]); // NOT a friend
    // KNOWS is undirected in the query; p3 is a friend via the reverse edge.
    rel(&g, p0, "KNOWS", p1);
    rel(&g, p0, "KNOWS", p2);
    rel(&g, p3, "KNOWS", p0);
    // messages: (creator, creationDate, id). A message HAS_CREATOR its creator.
    let mk = |g: &Graph, creator: u64, date: i64, id: i64| {
        let m = node(g, "Message", &[("creationDate", i(date)), ("id", i(id))]);
        rel(g, m, "HAS_CREATOR", creator);
    };
    mk(&g, p1, 100, 101);
    mk(&g, p1, 90, 102);
    mk(&g, p1, 80, 103);
    mk(&g, p2, 95, 201);
    mk(&g, p2, 85, 202);
    mk(&g, p3, 100, 301); // date TIE with p1's 101 → id 101 sorts before 301
    mk(&g, p4, 99, 401); // non-friend → excluded
    mk(&g, p1, 200, 199); // date > T → WHERE-excluded
    g
}

#[test]
fn ic2_ordered_matches_general_and_interp() {
    let g = ic2_graph();
    let src = "MATCH (:Person {id: 10})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Message) \
        WHERE message.creationDate <= 150 \
        RETURN friend.id AS pid, message.id AS mid, message.creationDate AS mdate \
        ORDER BY mdate DESC, toInteger(mid) ASC LIMIT 4";
    let (ordered, general, interp) = three(&g, src);
    assert_eq!(ordered, general, "ic2 ordered vs general disagree");
    assert_eq!(ordered, interp, "ic2 ordered vs interp disagree");
    // top-4 by (date DESC, id ASC): (p1,101,100),(p3,301,100),(p2,201,95),(p1,102,90).
    assert_eq!(
        ordered,
        vec![
            vec![i(1), i(101), i(100)],
            vec![i(3), i(301), i(100)],
            vec![i(2), i(201), i(95)],
            vec![i(1), i(102), i(90)],
        ],
        "IC2 top-4: friend excluded, date>T excluded, tie broken by id"
    );
    assert!(fired(&g, src), "the IC2 shape must take the ordered merge");
}

#[test]
fn ic2_ordered_mutual_knows_preserves_duplicate_rows() {
    // A MUTUAL friend — KNOWS in BOTH directions (SNB's storage) — is matched TWICE
    // by the undirected `(person)-[:KNOWS]-(friend)`, so each of its messages appears
    // TWICE. The ordered merge must reproduce that multiplicity, not dedup it (the
    // real-corpus bug: `friends.dedup()` dropped the duplicate rows at the LIMIT).
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p0 = node(&g, "Person", &[("id", i(10))]);
    let p1 = node(&g, "Person", &[("id", i(1))]);
    let p2 = node(&g, "Person", &[("id", i(2))]);
    rel(&g, p0, "KNOWS", p1); // mutual: both directions to p1
    rel(&g, p1, "KNOWS", p0);
    rel(&g, p0, "KNOWS", p2); // p2: single edge
    let mk = |g: &Graph, creator: u64, date: i64, id: i64| {
        let m = node(g, "Message", &[("creationDate", i(date)), ("id", i(id))]);
        rel(g, m, "HAS_CREATOR", creator);
    };
    mk(&g, p1, 100, 101);
    mk(&g, p2, 90, 201);

    let src = "MATCH (:Person {id: 10})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Message) \
        WHERE message.creationDate <= 150 \
        RETURN friend.id AS pid, message.id AS mid, message.creationDate AS mdate \
        ORDER BY mdate DESC, toInteger(mid) ASC LIMIT 10";
    let (ordered, general, interp) = three(&g, src);
    assert_eq!(
        ordered, general,
        "ic2 ordered vs general disagree on mutual KNOWS"
    );
    assert_eq!(
        ordered, interp,
        "ic2 ordered vs interp disagree on mutual KNOWS"
    );
    // p1's message appears TWICE (two KNOWS edges), p2's once.
    assert_eq!(
        ordered,
        vec![
            vec![i(1), i(101), i(100)],
            vec![i(1), i(101), i(100)],
            vec![i(2), i(201), i(90)],
        ],
        "the mutual friend's message is duplicated, matching the undirected expand"
    );
    assert!(fired(&g, src), "the IC2 shape must take the ordered merge");
}

#[test]
fn ic2_ordered_full_result_no_limit_pressure() {
    // LIMIT larger than the candidate set — every friend message ≤ T, ordered.
    let g = ic2_graph();
    let src = "MATCH (:Person {id: 10})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Message) \
        WHERE message.creationDate <= 150 \
        RETURN friend.id AS pid, message.id AS mid, message.creationDate AS mdate \
        ORDER BY mdate DESC, toInteger(mid) ASC LIMIT 100";
    let (ordered, general, interp) = three(&g, src);
    assert_eq!(ordered, general);
    assert_eq!(ordered, interp);
    assert!(fired(&g, src));
}
