#![allow(non_snake_case)]
//! A RENAMED collect-unwind — `WITH collect(DISTINCT friend) AS friends UNWIND
//! friends AS f MATCH (f)…` (the collected var `friend` re-bound under a NEW name
//! `f`) — is normalised by the columnar pre-pass (`f → friend` in the clauses
//! after the UNWIND) into the same-name form `recognise_multistage` accepts.
//! Byte-identical to the interp, and FIRES the multistage pipeline. This is the
//! IC6 `UNWIND friends AS f` shape in isolation.

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
    let root = mk("Person", &[("id", Value::Int(1))]);
    let a = mk("Person", &[("id", Value::Int(2))]);
    let b = mk("Person", &[("id", Value::Int(3))]);
    g.create_rel(root, "KNOWS", a, &BTreeMap::new()).unwrap();
    g.create_rel(a, "KNOWS", b, &BTreeMap::new()).unwrap(); // b is 2 hops
    let post = |creator: u64, id: i64, date: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        m.insert("date".to_string(), Value::Int(date));
        let p = g.create_node(&["Post".into()], &m).expect("post");
        g.create_rel(p, "HAS_CREATOR", creator, &BTreeMap::new())
            .unwrap();
        p
    };
    post(a, 100, 50); // a's post, date<100 → keep
    post(a, 101, 150); // a's post, date≥100 → drop
    post(b, 102, 20); // b's post, date<100 → keep
    g
}

const RENAMED: &str = "MATCH (root:Person {id: 1})-[:KNOWS*1..2]-(friend:Person) \
    WHERE NOT friend = root \
    WITH collect(DISTINCT friend) AS friends \
    UNWIND friends AS f \
    MATCH (f)<-[:HAS_CREATOR]-(message:Post) WHERE message.date < 100 \
    RETURN f.id AS pid, message.id AS mid ORDER BY mid ASC LIMIT 20";

// The SAME query written same-name (`UNWIND … AS friend`) — the normalisation
// target; both must give the same rows.
const SAME: &str = "MATCH (root:Person {id: 1})-[:KNOWS*1..2]-(friend:Person) \
    WHERE NOT friend = root \
    WITH collect(DISTINCT friend) AS friends \
    UNWIND friends AS friend \
    MATCH (friend)<-[:HAS_CREATOR]-(message:Post) WHERE message.date < 100 \
    RETURN friend.id AS pid, message.id AS mid ORDER BY mid ASC LIMIT 20";

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

#[test]
fn renamed_unwind_normalises_and_fires() {
    let g = g();
    let (on, off) = both(&g, RENAMED);
    assert_eq!(on, off, "renamed-unwind columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![i(2), i(100)], vec![i(3), i(102)]],
        "a's post 100 (date 50) and b's post 102 (date 20), ordered by mid ASC; post 101 (date 150) drops"
    );
    // And it equals the same-name spelling exactly.
    assert_eq!(on, rows(&g, SAME), "renamed spelling must equal same-name");
    assert!(
        !streamed(&g, RENAMED),
        "the renamed collect-unwind must normalise + fire multistage, not stream"
    );
}
