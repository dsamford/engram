//! A path whose only bound node is in its MIDDLE was seeded from its unbound
//! start by a label scan, the bound node merely pinning one hop's far end.
//! SNB BI bi17 binds `message1` from the tag and then matches
//! `(person1:Person)<-[:HAS_CREATOR]-(message1)-[:REPLY_OF*0..]->(post1:Post)
//! <-[:CONTAINER_OF]-(forum1:Forum)`: every Person scanned per message, 78 s at
//! SF3 for 4,250 rows. `split_at_bound_interior` walks it from the bound node
//! both ways. The oracle is the same match written from the bound node.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SPLIT: &str = "interp.path driven both ways from its bound interior node";

/// 300 people; person p wrote post 1000+p in forum p % 7 and comment 5000+p
/// replying to post 1000+(p+1)%300, and comment 9000+p replying to that
/// comment. Tag 1 is on every comment whose author is divisible by 3.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |labels: &[&str], id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        let ls: Vec<String> = labels.iter().map(|s| (*s).to_string()).collect();
        g.create_node(&ls, &m).expect("node")
    };
    let rel = |a: u64, t: &str, b: u64| {
        g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
    };
    let tag = node(&["Tag"], 1);
    let forums: Vec<u64> = (0..7).map(|f| node(&["Forum"], f)).collect();
    let people: Vec<u64> = (0..300).map(|p| node(&["Person"], p)).collect();
    let posts: Vec<u64> = (0..300)
        .map(|p| {
            let post = node(&["Message", "Post"], 1000 + p);
            rel(post, "BY", people[p as usize]);
            rel(forums[(p % 7) as usize], "CONTAINS", post);
            post
        })
        .collect();
    for p in 0..300i64 {
        let c = node(&["Message", "Comment"], 5000 + p);
        rel(c, "BY", people[p as usize]);
        rel(c, "REPLY", posts[((p + 1) % 300) as usize]);
        let c2 = node(&["Message", "Comment"], 9000 + p);
        rel(c2, "BY", people[((p + 2) % 300) as usize]);
        rel(c2, "REPLY", c);
        if p % 3 == 0 {
            rel(c, "TAG", tag);
            rel(c2, "TAG", tag);
        }
    }
    g.set_columnar_scans(false);
    g
}

fn run(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

#[test]
fn the_bi17_shape_is_walked_from_its_bound_message_with_the_same_answer() {
    let g = corpus();
    let (rows, c) = run(
        &g,
        "MATCH (t:Tag {id: 1}), (m:Message)-[:TAG]->(t), \
               (p:Person)<-[:BY]-(m)-[:REPLY*0..]->(post:Post)<-[:CONTAINS]-(f:Forum) \
         RETURN m.id, p.id, post.id, f.id ORDER BY m.id",
    );
    let (oracle, _) = run(
        &g,
        "MATCH (t:Tag {id: 1})<-[:TAG]-(m:Message) \
         MATCH (m)-[:BY]->(p:Person) \
         MATCH (m)-[:REPLY*0..]->(post:Post)<-[:CONTAINS]-(f:Forum) \
         RETURN m.id, p.id, post.id, f.id ORDER BY m.id",
    );
    assert_eq!(rows.len(), 200, "100 tagged comments and their 100 tagged replies");
    assert_eq!(rows, oracle, "the split walk binds what the bound-first spelling binds");
    assert!(count_of(&c, SPLIT) >= 1, "the path was driven from its bound interior node");
}

#[test]
fn halves_that_share_a_relationship_type_keep_their_single_walk() {
    let g = corpus();
    // `(a)<-[:REPLY]-(m)<-[:REPLY]-(b)` with m bound: splitting would let
    // the two halves reuse one REPLY edge, so it is not split
    let (rows, c) = run(
        &g,
        "MATCH (m:Comment {id: 5003}) \
         MATCH (a:Message)<-[:REPLY]-(m)<-[:REPLY]-(b:Comment) RETURN a.id, b.id",
    );
    let mut got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1004, 9003)]);
    assert_eq!(count_of(&c, SPLIT), 0, "a shared type keeps the path whole");
}
