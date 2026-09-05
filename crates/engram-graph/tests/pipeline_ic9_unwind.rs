#![allow(non_snake_case)]
//! Differential test for the IC9 shape: a var-length friends-of-friends match,
//! `WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend` (normalised
//! to `WITH DISTINCT friend`), then a per-friend expand + top-k. The contract is
//! the pipeline's usual one: columnar (`set_columnar_scans(true)`) equals the
//! general `run_streaming` path byte-for-byte, and the shape FIRES the multistage
//! operator (it fell to the interp before the collect-unwind normalisation).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Persons with `id`, a KNOWS graph 1–2 hops deep from the anchor (id 10), and
/// Messages with `id`/`creationDate` created by the friends — enough that the
/// 2-hop reach has duplicates (so DISTINCT matters) and several friends have
/// multiple messages (so the top-k over a total order is non-trivial).
fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_p = |id: i64| {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(id));
        p.insert("firstName".to_string(), Value::Str(format!("f{id}")));
        p.insert("lastName".to_string(), Value::Str(format!("l{id}")));
        g.create_node(&["Person".into()], &p).expect("person")
    };
    // p[0] is the anchor (id 10); the rest are ids 11..=16.
    let p: Vec<u64> = (0..7)
        .map(|i| mk_p(if i == 0 { 10 } else { 10 + i }))
        .collect();
    // KNOWS: 10—11, 10—12 ; 11—13, 12—13 (13 reached two ways → DISTINCT),
    // 12—14 ; 13—15 (3 hops from 10, must NOT appear at *1..2).
    for (a, b) in [(0, 1), (0, 2), (1, 3), (2, 3), (2, 4), (3, 5)] {
        g.create_rel(p[a], "KNOWS", p[b], &BTreeMap::new())
            .expect("knows");
    }
    let mk_m = |id: i64, date: i64, creator: u64| {
        let mut mp = BTreeMap::new();
        mp.insert("id".to_string(), Value::Int(id));
        mp.insert("creationDate".to_string(), Value::Int(date));
        mp.insert("content".to_string(), Value::Str(format!("c{id}")));
        let m = g.create_node(&["Message".into()], &mp).expect("message");
        // HAS_CREATOR points message -> person.
        g.create_rel(m, "HAS_CREATOR", creator, &BTreeMap::new())
            .expect("creator");
    };
    // Messages by the friends-of-10 (11,12,13,14 are within 2 hops; 15 is 3 hops).
    mk_m(100, 500, p[1]);
    mk_m(101, 900, p[1]);
    mk_m(102, 700, p[2]);
    mk_m(103, 1400, p[3]); // creationDate 1400 — above the query's cutoff, dropped
    mk_m(104, 300, p[3]);
    mk_m(105, 800, p[4]);
    mk_m(106, 600, p[5]); // creator 15 is 3 hops out → never reached
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn fired(g: &Graph, src: &str, counter: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(counter).copied().unwrap_or(0) > 0
}

const IC9: &str = "MATCH (root:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
    WHERE NOT friend = root \
    WITH collect(DISTINCT friend) AS friends \
    UNWIND friends AS friend \
    MATCH (friend)<-[:HAS_CREATOR]-(message:Message) WHERE message.creationDate < 1356998400000 \
    RETURN friend.id AS personId, friend.firstName AS fn, message.id AS mid, \
    message.creationDate AS d \
    ORDER BY d DESC, mid ASC LIMIT 20";

#[test]
fn ic9_collect_unwind_matches_general() {
    let g = g();
    let (on, off) = both(&g, IC9);
    assert_eq!(on, off, "IC9 columnar vs general disagree");
    // Non-empty result (the graph is built so the query returns rows), so the
    // equality above is meaningful, not two empties.
    assert!(!on.is_empty(), "IC9 should return rows on this graph");
    assert!(
        fired(&g, IC9, "interp.pipeline multistage runs"),
        "the collect(DISTINCT)+UNWIND IC9 shape must run on the multistage pipeline"
    );
}

/// The plain `WITH DISTINCT friend` form must still match (the normalisation is
/// additive, not a replacement) and equal the collect-unwind form.
#[test]
fn ic9_plain_distinct_equals_collect_unwind() {
    let g = g();
    let plain = IC9.replace(
        "WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend ",
        "WITH DISTINCT friend ",
    );
    assert_eq!(
        both(&g, &plain).0,
        both(&g, IC9).0,
        "WITH DISTINCT and collect+UNWIND must produce the same result"
    );
    assert!(fired(&g, &plain, "interp.pipeline multistage runs"));
}
