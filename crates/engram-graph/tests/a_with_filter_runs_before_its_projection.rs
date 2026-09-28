//! A `WITH`'s `WHERE` is evaluated before the projection when that is sound.
//!
//! A `WITH` is a stage boundary and a stage boundary MATERIALISES its rows;
//! a predicate that then discards most of them has paid for every one. SNB BI
//! bi4, decomposed at SF3: the boundary ALONE — the same `WITH` carrying no
//! predicate — took the query from 234 s past the 300 s ceiling. Rows the
//! filter would reject are rows that never needed building.
//!
//! Most of this file is about when the move is NOT sound. Each refusal guards
//! a different wrong answer, and a pushdown that fires once too often is a
//! silent change of results — strictly worse than the slow plan it replaced.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn person(g: &Graph, name: &str, age: i64) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    m.insert("age".to_string(), Value::Int(age));
    g.create_node(&["P".into()], &m).expect("node")
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn names(r: &[Vec<Value>]) -> Vec<String> {
    r.iter()
        .map(|x| match &x[0] {
            Value::Str(s) => s.to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn fixture() -> Graph {
    let g = g();
    person(&g, "a", 10);
    person(&g, "b", 20);
    person(&g, "c", 30);
    person(&g, "d", 40);
    g
}

#[test]
fn the_filter_still_selects_the_same_rows() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p, p.age AS age WHERE p.age > 20 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["c", "d"], "{r:?}");
}

#[test]
fn it_engages_on_that_shape() {
    let g = fixture();
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            "MATCH (p:P) WITH p, p.age AS age WHERE p.age > 20 \
             RETURN p.name AS name ORDER BY name",
        )
    })
    .1;
    assert!(
        t.sometimes_hit()
            .contains("interp.WITH filter pushed before its projection"),
        "the pushdown never fired: {:?}",
        t.sometimes_hit()
    );
}

#[test]
fn a_predicate_reading_a_name_the_projection_invents_stays_put() {
    // `doubled` does not exist before the projection. Moving this predicate
    // would read an unbound name, or worse, an unrelated one.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p, p.age * 2 AS doubled WHERE doubled > 40 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["c", "d"], "{r:?}");
}

#[test]
fn a_having_over_an_aggregate_stays_put() {
    // The classic HAVING: the count does not exist until the projection has
    // run, so this predicate cannot move at all.
    let g = g();
    let a = person(&g, "a", 1);
    let b = person(&g, "b", 1);
    let c = person(&g, "c", 1);
    for (from, to) in [(a, b), (a, c), (b, c)] {
        g.create_rel(from, "KNOWS", to, &BTreeMap::new())
            .expect("rel");
    }
    let r = rows(
        &g,
        "MATCH (p:P)-[:KNOWS]->(q:P) WITH p, count(q) AS n WHERE n > 1 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["a"], "only a knows two: {r:?}");
}

#[test]
fn a_filter_after_a_limit_keeps_the_limits_rows() {
    // THE ONE THAT CHANGES ANSWERS IF PUSHED. `LIMIT 2` takes the two
    // youngest; the filter then rejects both, so the answer is EMPTY. Pushed
    // before the projection the filter would run first and the limit would
    // take two of the survivors — two rows instead of none.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p ORDER BY p.age ASC LIMIT 2 WHERE p.age > 20 \
         RETURN p.name AS name ORDER BY name",
    );
    assert!(
        r.is_empty(),
        "the limit picks a and b, and neither is over 20: {r:?}"
    );
}

#[test]
fn a_filter_after_a_skip_keeps_the_skips_rows() {
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WITH p ORDER BY p.age ASC SKIP 2 WHERE p.age > 10 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["c", "d"], "{r:?}");
}

#[test]
fn a_filter_over_distinct_keeps_its_rows() {
    let g = g();
    let a = person(&g, "a", 10);
    let b = person(&g, "b", 20);
    // two edges to the same pair, so DISTINCT has something to collapse
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).expect("rel");
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).expect("rel");
    let r = rows(
        &g,
        "MATCH (p:P)-[:KNOWS]->(q:P) WITH DISTINCT q WHERE q.age > 10 \
         RETURN q.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["b"], "{r:?}");
}

#[test]
fn an_optional_match_before_it_does_not_absorb_the_filter() {
    // Pushing into an OPTIONAL MATCH makes the predicate part of the PATTERN:
    // non-matching rows would come back with nulls instead of being removed.
    // Here every `p` survives the OPTIONAL MATCH, and the filter must then
    // remove the ones with no qualifying friend — not null them.
    let g = g();
    let a = person(&g, "a", 10);
    let b = person(&g, "b", 20);
    person(&g, "lonely", 30);
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).expect("rel");
    let r = rows(
        &g,
        "MATCH (p:P) OPTIONAL MATCH (p)-[:KNOWS]->(f:P) WITH p, f WHERE f.age > 15 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["a"], "only a has a friend over 15: {r:?}");
}

#[test]
fn the_predicate_may_read_a_name_the_projection_drops() {
    // bi4's actual shape: the predicate reads `keep`, which the projection
    // does NOT carry forward. That is still sound — `keep` is in scope BEFORE
    // the projection, which is exactly where the filter is moving to.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (k:P) WHERE k.age >= 30 WITH collect(k) AS keep \
         MATCH (p:P) WITH p, p.name AS nm WHERE p IN keep \
         RETURN nm AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["c", "d"], "{r:?}");
}

#[test]
fn an_existing_match_filter_is_kept_alongside_the_pushed_one() {
    // The MATCH already carries a WHERE; the pushed predicate must AND with
    // it, not replace it.
    let g = fixture();
    let r = rows(
        &g,
        "MATCH (p:P) WHERE p.age < 40 WITH p, p.age AS age WHERE p.age > 10 \
         RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(names(&r), vec!["b", "c"], "both bounds held: {r:?}");
}
