#![allow(non_snake_case)]
//! The IC6 CORE (post-scalar-prelude, `knownTagId` inlined as a literal):
//! `MATCH (person {id})-[:KNOWS*1..2]-(friend) WHERE NOT person=friend
//! WITH DISTINCT friend MATCH (friend)<-[:HAS_CREATOR]-(post),
//! (post)-[:HAS_TAG]->(t:Tag {id: LIT}), (post)-[:HAS_TAG]->(tag:Tag)
//! WHERE NOT t=tag WITH tag.name AS tagName, count(post) AS postCount
//! RETURN tagName, postCount ORDER BY postCount DESC, tagName ASC LIMIT 10`.
//!
//! Exercises the stage-2 Form-A AGGREGATE tail (a `WITH <agg>` before the
//! RETURN, over a multistage stage-2 chunk), a multi-path stage-2 with a
//! mid-chain literal `{id}` anchor and a two-var `NOT t=tag` WHERE. Byte-
//! identical to the interp, and FIRES the multistage pipeline. The scalar
//! prelude that turns `knownTagId` into that literal is the remaining IC6 piece.

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
    // f1 is a direct friend; f2 is 2 hops (via f1) — both in KNOWS*1..2.
    g.create_rel(person, "KNOWS", f1, &BTreeMap::new()).unwrap();
    g.create_rel(f1, "KNOWS", f2, &BTreeMap::new()).unwrap();
    let known = mk(
        "Tag",
        &[
            ("id", Value::Int(100)),
            ("name", Value::Str("Music".into())),
        ],
    );
    let rock = mk(
        "Tag",
        &[("id", Value::Int(200)), ("name", Value::Str("Rock".into()))],
    );
    let jazz = mk(
        "Tag",
        &[("id", Value::Int(300)), ("name", Value::Str("Jazz".into()))],
    );
    // A post created by `friend`, tagged with `known` (id 100) AND `other`.
    let post = |friend: u64, other: u64, with_known: bool| {
        let p = g
            .create_node(&["Post".into()], &BTreeMap::new())
            .expect("post");
        g.create_rel(p, "HAS_CREATOR", friend, &BTreeMap::new())
            .unwrap();
        if with_known {
            g.create_rel(p, "HAS_TAG", known, &BTreeMap::new()).unwrap();
        }
        g.create_rel(p, "HAS_TAG", other, &BTreeMap::new()).unwrap();
        p
    };
    post(f1, rock, true); // Rock
    post(f1, jazz, true); // Jazz
    post(f2, rock, true); // Rock
    // A post tagged only with `rock` (no known tag) — must NOT match (no `t`).
    post(f1, rock, false);
    g
}

// The FULL LDBC IC6: the scalar prelude resolves `knownTagId` from the Music
// tag, then the collect-unwind (renamed `AS f`) and the anchor `{id: knownTagId}`
// all normalise + fire. Must equal IC6_CORE exactly (same graph, same answer).
const IC6_FULL: &str = "MATCH (knownTag:Tag {name: 'Music'}) \
    WITH knownTag.id AS knownTagId \
    MATCH (person:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) WHERE NOT person = friend \
    WITH knownTagId, collect(DISTINCT friend) AS friends \
    UNWIND friends AS f \
    MATCH (f)<-[:HAS_CREATOR]-(post:Post), \
      (post)-[:HAS_TAG]->(t:Tag {id: knownTagId}), \
      (post)-[:HAS_TAG]->(tag:Tag) \
    WHERE NOT t = tag \
    WITH tag.name AS tagName, count(post) AS postCount \
    RETURN tagName, postCount ORDER BY postCount DESC, tagName ASC LIMIT 10";

const IC6_CORE: &str = "MATCH (person:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
    WHERE NOT person = friend \
    WITH DISTINCT friend \
    MATCH (friend)<-[:HAS_CREATOR]-(post:Post), \
      (post)-[:HAS_TAG]->(t:Tag {id: 100}), \
      (post)-[:HAS_TAG]->(tag:Tag) \
    WHERE NOT t = tag \
    WITH tag.name AS tagName, count(post) AS postCount \
    RETURN tagName, postCount ORDER BY postCount DESC, tagName ASC LIMIT 10";

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
fn ic6_core_on_equals_off_and_fires() {
    let g = g();
    let (on, off) = both(&g, IC6_CORE);
    assert_eq!(on, off, "IC6-core columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Rock"), i(2)], vec![s("Jazz"), i(1)]],
        "Rock from 2 posts (f1, f2), Jazz from 1 (f1); the un-known-tagged post drops"
    );
    assert!(
        !streamed(&g, IC6_CORE),
        "IC6-core must run the multistage pipeline (Form-A stage-2 aggregate tail), not stream"
    );
}

#[test]
fn ic6_full_prelude_on_equals_off_and_fires() {
    let g = g();
    let (on, off) = both(&g, IC6_FULL);
    assert_eq!(on, off, "IC6-full columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("Rock"), i(2)], vec![s("Jazz"), i(1)]],
        "full IC6 (scalar prelude + renamed unwind + carried anchor) equals IC6-core"
    );
    assert!(
        !streamed(&g, IC6_FULL),
        "IC6-full must normalise (prelude + rename) then fire the multistage pipeline, not stream"
    );
}
