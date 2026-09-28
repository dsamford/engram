//! A hop end constrained by `WHERE v IN <list of nodes>` is confined DURING
//! expansion, so a peer outside the list never becomes a row.
//!
//! The engine already skipped a peer outside a RESOLVED END SET before
//! building its frame, but that set could only come from an inline map
//! (`(:Forum {id: $f})`). A membership constraint normally arrives in a
//! `WHERE`, and then the entire fan-out was materialised and filtered
//! afterwards.
//!
//! SNB BI bi4 is exactly that: `(person)<-[:HAS_MEMBER]-(topForum2:Forum)`
//! with `topForum2 IN topForums`. Decomposed at SF3 over four runs, adding
//! that one hop took the query from 41 s to 248 s, and three separate attempts
//! to make the AFTERWARDS cheaper — sharing list values, hoisting the
//! membership test to a set, moving the filter onto the MATCH — each measured
//! as no change, because every one of them acts on rows that already exist.
//!
//! These tests pin the ANSWER, because confining a hop too aggressively is
//! silent: the rows simply never appear.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, label: &str, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&[label.into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn names(r: &[Vec<Value>]) -> Vec<String> {
    let mut v: Vec<String> = r
        .iter()
        .map(|x| match &x[0] {
            Value::Str(s) => s.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    v.sort();
    v
}

/// One person in four forums; two of them are "top".
fn fixture() -> Graph {
    let g = g();
    let p = node(&g, "Person", "p");
    for f in ["top1", "top2", "other1", "other2"] {
        let fid = node(&g, "Forum", f);
        g.create_rel(fid, "HAS_MEMBER", p, &BTreeMap::new())
            .expect("rel");
    }
    g
}

#[test]
fn only_the_listed_forums_come_back() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name STARTS WITH 'top' WITH collect(t) AS tops \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN tops \
         RETURN f.name AS name",
    );
    assert_eq!(names(&r), vec!["top1", "top2"], "{r:?}");
}

#[test]
fn it_engages_on_bi4s_shape() {
    // THE SHAPE MATTERS, and the first version of this test got it wrong. A
    // simple one-hop `MATCH` is accepted by the VECTORISED PIPELINE, which is
    // a different executor with no end-set mechanism at all — so the test
    // asserted engagement on a path the code under test does not run on, and
    // failed while the implementation was fine.
    //
    // bi4's subquery is `UNWIND` plus a multi-hop pattern, which the pipeline
    // declines, so it reaches the general matcher. That is the path this
    // optimisation lives on, and the one worth asserting.
    let g = g();
    let p = node(&g, "Person", "p");
    let top = node(&g, "Forum", "top1");
    let other = node(&g, "Forum", "other1");
    let msg = node(&g, "Message", "m");
    g.create_rel(top, "CONTAINER_OF", msg, &BTreeMap::new())
        .expect("rel");
    g.create_rel(msg, "HAS_CREATOR", p, &BTreeMap::new())
        .expect("rel");
    g.create_rel(top, "HAS_MEMBER", p, &BTreeMap::new())
        .expect("rel");
    g.create_rel(other, "HAS_MEMBER", p, &BTreeMap::new())
        .expect("rel");

    let src = "MATCH (t:Forum) WHERE t.name = 'top1' WITH collect(t) AS tops \
               UNWIND tops AS t1 \
               MATCH (t1)-[:CONTAINER_OF]->(m:Message)-[:HAS_CREATOR]->(q:Person)<-[:HAS_MEMBER]-(f:Forum) \
               WHERE f IN tops \
               RETURN f.name AS name";
    let (r, t) = engram_observe::with_trace(|| rows(&g, src));
    assert_eq!(
        names(&r),
        vec!["top1"],
        "the other forum is excluded: {r:?}"
    );
    assert!(
        t.counters()
            .contains_key("interp.hop end confined by a WHERE membership test"),
        "the hop was not confined on bi4's shape: {:?}",
        t.counters()
    );
}

#[test]
fn a_list_holding_a_non_node_confines_nothing() {
    // An id set would answer a different question than `eq3` does, so the
    // whole optimisation must stand down — and the answer must still be right.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name = 'top1' WITH collect(t)[0] AS one \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN [one, 7, 'x'] \
         RETURN f.name AS name",
    );
    assert_eq!(names(&r), vec!["top1"], "{r:?}");
}

#[test]
fn an_empty_list_matches_nothing() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name = 'nobody' WITH collect(t) AS tops \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN tops \
         RETURN f.name AS name",
    );
    assert!(r.is_empty(), "IN [] is false for every peer: {r:?}");
}

#[test]
fn a_negated_membership_is_not_a_confinement() {
    // `NOT f IN tops` selects the COMPLEMENT. Confining the hop to `tops`
    // would return exactly the wrong half — the sharpest failure this
    // optimisation could have.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name STARTS WITH 'top' WITH collect(t) AS tops \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE NOT f IN tops \
         RETURN f.name AS name",
    );
    assert_eq!(names(&r), vec!["other1", "other2"], "{r:?}");
}

#[test]
fn a_membership_under_or_does_not_confine_the_hop() {
    // Under `OR` the predicate does not constrain the hop at all: a peer
    // outside the list can still satisfy the other arm.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name = 'top1' WITH collect(t) AS tops \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN tops OR f.name = 'other2' \
         RETURN f.name AS name",
    );
    assert_eq!(names(&r), vec!["other2", "top1"], "{r:?}");
}

#[test]
fn a_list_correlated_with_the_hops_own_binding_is_left_alone() {
    // The haystack is evaluated against the SEED, before the hop runs. A list
    // that reads what the hop itself binds cannot be evaluated then, so the
    // confinement must decline and the ordinary filter must answer.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN [f] \
         RETURN f.name AS name",
    );
    assert_eq!(
        names(&r),
        vec!["other1", "other2", "top1", "top2"],
        "every forum is trivially in [itself]: {r:?}"
    );
}

#[test]
fn the_other_end_of_the_hop_is_unaffected() {
    // Confining the FAR end must not drop rows at the near end: two people in
    // one top forum must both come back.
    let g = g();
    let f = node(&g, "Forum", "top1");
    for who in ["a", "b"] {
        let p = node(&g, "Person", who);
        g.create_rel(f, "HAS_MEMBER", p, &BTreeMap::new())
            .expect("rel");
    }
    let other = node(&g, "Forum", "other");
    let c = node(&g, "Person", "c");
    g.create_rel(other, "HAS_MEMBER", c, &BTreeMap::new())
        .expect("rel");

    let r = rows(
        &g,
        "MATCH (t:Forum) WHERE t.name = 'top1' WITH collect(t) AS tops \
         MATCH (p:Person)<-[:HAS_MEMBER]-(f:Forum) WHERE f IN tops \
         RETURN p.name AS name",
    );
    assert_eq!(names(&r), vec!["a", "b"], "{r:?}");
}
