//! `WHERE x IN <list of nodes>` answers from a set, not a linear scan.
//!
//! The comprehension form of this was hoisted already; this is the same cost
//! in a `WHERE`, which is where SNB BI bi4 pays it. Decomposed at SF3, bi4's
//! `WHERE topForum2 IN topForums` — one hundred nodes, tested once per
//! fanned-out row — accounts for AT LEAST 61 s of the query (b4e 239 s, b4f
//! over 300 s).
//!
//! The transformation is only valid because NODES COMPARE BY IDENTITY, so a
//! set of ids is exactly equivalent to the scan. `eq3` over other types is
//! not: Int and Float coerce, DateTimes compare by instant, Times normalise
//! their offset, and Lists recurse. These tests therefore spend most of their
//! effort on the cases where the hoist must DECLINE and the scan must answer,
//! because a set keyed on anything coarser changes answers silently — which is
//! far worse than being slow.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["N".into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn fixture() -> Graph {
    let g = g();
    for n in ["a", "b", "c", "d"] {
        node(&g, n);
    }
    g
}

#[test]
fn a_node_in_a_collected_node_list_keeps_the_members() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:N) WHERE k.name IN ['a','b'] WITH collect(k) AS keep \
         MATCH (n:N) WHERE n IN keep RETURN n.name AS name ORDER BY name",
    );
    let got: Vec<&Value> = r.iter().map(|x| &x[0]).collect();
    assert_eq!(
        got,
        vec![&Value::Str("a".into()), &Value::Str("b".into())],
        "{r:?}"
    );
}

#[test]
fn the_negated_form_keeps_exactly_the_others() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:N) WHERE k.name IN ['a','b'] WITH collect(k) AS keep \
         MATCH (n:N) WHERE NOT n IN keep RETURN n.name AS name ORDER BY name",
    );
    let got: Vec<&Value> = r.iter().map(|x| &x[0]).collect();
    assert_eq!(
        got,
        vec![&Value::Str("c".into()), &Value::Str("d".into())],
        "the complement, and nothing dropped: {r:?}"
    );
}

#[test]
fn an_empty_list_matches_nothing() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:N) WHERE k.name = 'nobody' WITH collect(k) AS keep \
         MATCH (n:N) WHERE n IN keep RETURN n.name AS name",
    );
    assert!(r.is_empty(), "IN [] is false, never true: {r:?}");
}

#[test]
fn int_and_float_still_coerce_because_the_hoist_declines() {
    // THE CASE THE NARROWNESS EXISTS FOR. 1 IN [1.0] is TRUE — Cypher coerces
    // across the numeric types — and a set of ids could never say so. The
    // needle is not a node, so the scan answers and the coercion survives.
    let g = g();
    let r = rows(
        &g,
        "UNWIND [1, 2, 3] AS x WITH x WHERE x IN [1.0, 3.0] RETURN x ORDER BY x",
    );
    let got: Vec<&Value> = r.iter().map(|v| &v[0]).collect();
    assert_eq!(got, vec![&Value::Int(1), &Value::Int(3)], "{r:?}");
}

#[test]
fn a_node_needle_against_a_list_of_non_nodes_is_false_not_a_set_hit() {
    // The list is not all nodes, so the hoist must decline and the scan must
    // answer — a set built by skipping non-nodes would have been a silent
    // wrong answer here.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (n:N) WHERE n IN [1, 'a', true] RETURN n.name AS name",
    );
    assert!(r.is_empty(), "a node is not any of those: {r:?}");
}

#[test]
fn a_mixed_list_holding_the_node_still_finds_it() {
    // The decisive one: the list HOLDS the node but is not all nodes. If the
    // hoist built a set from just the node elements it would answer correctly
    // here by luck; if it declined to cache but also declined to scan, it
    // would wrongly answer false. It must scan, and find it.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:N) WHERE k.name = 'a' WITH collect(k)[0] AS one \
         MATCH (n:N) WHERE n IN [1, one, 'x'] RETURN n.name AS name",
    );
    let got: Vec<&Value> = r.iter().map(|x| &x[0]).collect();
    assert_eq!(got, vec![&Value::Str("a".into())], "{r:?}");
}

#[test]
fn two_different_lists_in_one_query_each_answer_for_themselves() {
    // The memo holds ONE entry. Alternating between two lists must stay
    // correct however often it is replaced — a stale entry answering for the
    // wrong list is the failure mode a single-slot cache invites.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:N) WHERE k.name IN ['a','b'] WITH collect(k) AS ab \
         MATCH (m:N) WHERE m.name IN ['c'] WITH ab, collect(m) AS c \
         MATCH (n:N) WHERE n IN ab OR n IN c RETURN n.name AS name ORDER BY name",
    );
    let got: Vec<&Value> = r.iter().map(|x| &x[0]).collect();
    assert_eq!(
        got,
        vec![
            &Value::Str("a".into()),
            &Value::Str("b".into()),
            &Value::Str("c".into())
        ],
        "both lists answered for themselves: {r:?}"
    );
}

#[test]
fn the_hoist_actually_engages_for_a_node_list() {
    let g = fixture();
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (k:N) WHERE k.name IN ['a','b'] WITH collect(k) AS keep \
             MATCH (n:N) WHERE n IN keep RETURN n.name AS name",
        )
    })
    .1;
    assert!(
        t.counters()
            .contains_key("cypher.IN hoisted a node list to a set"),
        "the set was never built: {:?}",
        t.counters()
    );
}
