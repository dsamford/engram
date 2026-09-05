#![allow(non_snake_case)]
//! The IC4 shape: `MATCH (p {id})-[:KNOWS]-(friend), (friend)<-[:HAS_CREATOR]-
//! (post)-[:HAS_TAG]->(tag) WITH DISTINCT tag, post WITH tag, CASE… AS valid,
//! CASE… AS inValid WITH tag, sum(valid) AS postCount, sum(inValid) AS
//! inValidPostCount WHERE postCount > 0 AND inValidPostCount = 0 RETURN
//! tag.name, postCount ORDER BY postCount DESC, tagName ASC LIMIT 10`.
//!
//! Exercises the A2 primitives together: a DISTINCT relational stage, a fused
//! CASE-projection stage (`sum(valid)` → `sum(CASE…)`), a chained-range CASE, a
//! group-by `sum` and a HAVING — all with NO further graph traversal after the
//! WITH. Byte-identical to the interp, and FIRES the multistage pipeline.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let person = mk("Person", &[("id", Value::Int(10))]);
    let f1 = mk("Person", &[("id", Value::Int(11))]);
    let f2 = mk("Person", &[("id", Value::Int(12))]);
    g.create_rel(person, "KNOWS", f1, &BTreeMap::new()).unwrap();
    g.create_rel(person, "KNOWS", f2, &BTreeMap::new()).unwrap();
    let tag_a = mk("Tag", &[("name", Value::Str("Alpha".into()))]);
    let tag_b = mk("Tag", &[("name", Value::Str("Beta".into()))]);
    let tag_c = mk("Tag", &[("name", Value::Str("Gamma".into()))]);
    // A post created by `friend`, tagged `tag`, with a creationDate.
    let post = |friend: u64, tag: u64, date: i64| {
        let p = {
            let mut m = BTreeMap::new();
            m.insert("date".to_string(), Value::Int(date));
            g.create_node(&["Post".into()], &m).expect("post")
        };
        g.create_rel(p, "HAS_CREATOR", friend, &BTreeMap::new())
            .unwrap();
        g.create_rel(p, "HAS_TAG", tag, &BTreeMap::new()).unwrap();
        p
    };
    // Range [1000, 2000): valid. date < 1000: inValid.
    // Alpha: two valid posts, zero invalid → KEEP, postCount 2.
    post(f1, tag_a, 1500);
    post(f1, tag_a, 1800);
    // Beta: one valid + one invalid → dropped (inValidPostCount = 1).
    post(f1, tag_b, 1200);
    post(f2, tag_b, 500);
    // Gamma: one valid, zero invalid → KEEP, postCount 1.
    post(f2, tag_c, 1100);
    // A post ≥ upper bound is neither valid nor invalid — Gamma stays postCount 1,
    // inValid 0 (still kept).
    post(f2, tag_c, 2500);
    g
}

const IC4: &str = "MATCH (person:Person {id: 10})-[:KNOWS]-(friend:Person), \
    (friend)<-[:HAS_CREATOR]-(post:Post)-[:HAS_TAG]->(tag) \
    WITH DISTINCT tag, post \
    WITH tag, \
      CASE WHEN 1000 <= post.date < 2000 THEN 1 ELSE 0 END AS valid, \
      CASE WHEN post.date < 1000 THEN 1 ELSE 0 END AS inValid \
    WITH tag, sum(valid) AS postCount, sum(inValid) AS inValidPostCount \
    WHERE postCount > 0 AND inValidPostCount = 0 \
    RETURN tag.name AS tagName, postCount ORDER BY postCount DESC, tagName ASC LIMIT 10";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
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

fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn ic4_on_equals_off_and_fires() {
    let g = g();
    let (on, off) = both(&g, IC4);
    assert_eq!(on, off, "IC4 columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Alpha"), i(2)], vec![s("Gamma"), i(1)]],
        "Alpha (2 valid, 0 invalid) then Gamma (1 valid, 0 invalid); Beta has an invalid post"
    );
    assert!(
        !streamed(&g, IC4),
        "IC4 must run the multistage pipeline (DISTINCT + fused CASE-sum + HAVING), not stream"
    );
}
