//! A both-bound path evaluated once per row with the SAME far end — SNB BI
//! bi17's `(forum1)<-[:HAS_MEMBER]->(person2)<-[:HAS_CREATOR]-(comment)
//! -[:HAS_TAG]->(tag)`, 4,250 rows and one tag at SF3 — is answered as a join
//! at its first interior node (`join_at_middle`): the far half once per end,
//! then each row's first hop looked up in it. The oracle spells the same
//! question with no both-bound path at all, the tag test in the WHERE.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const BUILT: &str = "interp.middle join built its far half once for a repeated end";
const ANSWERED: &str = "interp.middle join answered a row from its far half";

/// Tag 1 and tag 2. Ten forums of thirty members, a member in forum f also a
/// member of forum f+1. Each forum holds five posts carrying both tags. Each
/// person wrote twelve comments; every seventh carries tag 1, every eleventh
/// tag 2.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let rel = |a: u64, t: &str, b: u64| {
        g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
    };
    let t1 = node("Tag", 1);
    let t2 = node("Tag", 2);
    let forums: Vec<u64> = (0..10).map(|f| node("Forum", f)).collect();
    let mut cid = 10_000;
    for p in 0..300i64 {
        let person = node("Person", p);
        let f = (p / 30) as usize;
        rel(forums[f], "MEMBER", person);
        rel(forums[(f + 1) % 10], "MEMBER", person);
        for k in 0..12 {
            cid += 1;
            let c = node("Comment", cid);
            rel(c, "BY", person);
            if (p * 12 + k) % 7 == 0 {
                rel(c, "TAG", t1);
            }
            if (p * 12 + k) % 11 == 0 {
                rel(c, "TAG", t2);
            }
        }
    }
    for (f, forum) in forums.iter().enumerate() {
        for j in 0..5 {
            let post = node("Post", (f * 100 + j) as i64);
            rel(*forum, "CONTAINS", post);
            rel(post, "TAG", t1);
            rel(post, "TAG", t2);
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
fn the_repeated_tag_is_walked_once_and_every_row_answers_as_the_oracle_does() {
    let g = corpus();
    for tag in [1, 2] {
        let (rows, c) = run(
            &g,
            &format!(
                "MATCH (t:Tag {{id: {tag}}}), (t)<-[:TAG]-(m1:Post)<-[:CONTAINS]-(f:Forum), \
                       (f)-[:MEMBER]->(p2:Person)<-[:BY]-(c:Comment)-[:TAG]->(t) \
                 RETURN m1.id, p2.id, c.id ORDER BY m1.id, p2.id, c.id"
            ),
        );
        let (oracle, _) = run(
            &g,
            &format!(
                "MATCH (t:Tag {{id: {tag}}}) MATCH (t)<-[:TAG]-(m1:Post)<-[:CONTAINS]-(f:Forum) \
                 MATCH (f)-[:MEMBER]->(p2:Person)<-[:BY]-(c:Comment) WHERE (c)-[:TAG]->(t) \
                 RETURN m1.id, p2.id, c.id ORDER BY m1.id, p2.id, c.id"
            ),
        );
        assert!(!oracle.is_empty(), "the corpus answers tag {tag}");
        assert_eq!(rows, oracle, "tag {tag}: the join answers what the walk answers");
        assert!(count_of(&c, BUILT) >= 1, "tag {tag}: the far half was built");
        assert!(
            count_of(&c, ANSWERED) >= 40,
            "tag {tag}: rows after the first were answered from it: {}",
            count_of(&c, ANSWERED)
        );
    }
}

#[test]
fn an_end_seen_once_is_walked_as_before() {
    let g = corpus();
    // one forum, one post: the both-bound leg runs for one row only
    let (rows, c) = run(
        &g,
        "MATCH (t:Tag {id: 1}), (t)<-[:TAG]-(m1:Post {id: 300})<-[:CONTAINS]-(f:Forum), \
               (f)-[:MEMBER]->(p2:Person)<-[:BY]-(c:Comment)-[:TAG]->(t) \
         RETURN count(*)",
    );
    let (oracle, _) = run(
        &g,
        "MATCH (t:Tag {id: 1}) MATCH (t)<-[:TAG]-(m1:Post {id: 300})<-[:CONTAINS]-(f:Forum) \
         MATCH (f)-[:MEMBER]->(p2:Person)<-[:BY]-(c:Comment) WHERE (c)-[:TAG]->(t) \
         RETURN count(*)",
    );
    assert_eq!(rows, oracle);
    assert_eq!(count_of(&c, BUILT), 0, "a single sight builds nothing");
}
