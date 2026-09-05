#![allow(non_snake_case)]
//! IC9's index-ordered top-k (Track B's IC9 lever) must be byte-identical to the
//! row-at-a-time interp it replaces. The query is IC9's exact shape: a KNOWS*1..2
//! neighbourhood collected DISTINCT, then each friend's messages before a date,
//! top-20 by (creationDate DESC, message.id ASC). `on` runs the columnar
//! pipeline (where the index-ordered operator fires in `run_multistage`); `off`
//! runs the interp. They must agree exactly — and the operator must actually
//! fire, else this proves nothing.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const IC9: &str = "MATCH (root:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
     WHERE NOT friend = root \
     WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < 5000 \
     RETURN friend.id AS personId, message.id AS commentOrPostId, \
            message.creationDate AS commentOrPostCreationDate \
     ORDER BY commentOrPostCreationDate DESC, message.id ASC LIMIT 20";

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // 15 people; the query roots at the one whose `id` property is 10.
    let mut person = Vec::new();
    for i in 0..15i64 {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(i));
        person.push(g.create_node(&["Person".into()], &p).expect("person"));
    }
    let empty = BTreeMap::new();
    // A KNOWS ring + chords so root=10's 1..2-hop neighbourhood is most people
    // (a DENSE friend set → the operator fires rather than bailing).
    for i in 0..15usize {
        g.create_rel(person[i], "KNOWS", person[(i + 1) % 15], &empty)
            .expect("knows");
        g.create_rel(person[i], "KNOWS", person[(i + 4) % 15], &empty)
            .expect("chord");
    }
    // ~80 messages: creationDate collides (ties → the message.id tiebreak must
    // decide), message.id is NOT aligned with creation order, spread across
    // authors so > 20 qualify under the date bound.
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(7000 - i)); // unique, anti-aligned
        m.insert(
            "creationDate".to_string(),
            Value::Int(1000 + (i * 37) % 4200),
        );
        let mid = g.create_node(&["Message".into()], &m).expect("message");
        let author = person[(i as usize * 11) % 15];
        g.create_rel(mid, "HAS_CREATOR", author, &empty)
            .expect("has_creator");
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

#[test]
fn ic9_index_ordered_topk_is_byte_identical_to_interp() {
    let g = g();

    g.set_columnar_scans(false);
    let off = rows(&g, IC9);
    g.set_columnar_scans(true);
    let (on, trace) = engram_observe::with_trace(|| rows(&g, IC9));

    assert_eq!(on, off, "index-ordered top-k diverged from the interp");
    assert!(
        !off.is_empty(),
        "the fixture produced no rows — test is vacuous"
    );
    assert!(
        trace
            .counters()
            .get("interp.pipeline index-ordered topk served stage 2")
            .copied()
            .unwrap_or(0)
            > 0,
        "the index-ordered operator did not fire — the differential proves nothing"
    );
}
