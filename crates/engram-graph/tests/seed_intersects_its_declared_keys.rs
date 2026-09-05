#![allow(non_snake_case)]
//! Fix 66: a start sought on TWO OR MORE declared keys — `{userId: $userId,
//! status: 'open'}` over the production `(userId, status)` composite — took
//! the most selective probe alone and re-verified every candidate by a
//! record read. On the mirror the Commitment listing read 10 records to
//! answer 0 rows and the repository listing 37 for 0, while Neo4j answered
//! both from the composite index. Now every other declared key is probed
//! too and the candidate set is the intersection: the 0-row case reads no
//! record at all.
//!
//! Every answer is checked against the same statement with the property
//! seek OFF (the label scan).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.into()));
    p
}

fn rows(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(user))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, user: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, user));
    (r, trace.counters().clone())
}

/// The statement over the label scan (no seek at all).
fn scanned(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    g.set_property_seek(false);
    let r = rows(g, src, user);
    g.set_property_seek(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const INTERSECTED: &str = "interp.seed intersected a second declared key";
const GETS: &str = "store.gets";
const FULL: &str = "graph.nodes materialised in full";

/// 2,000 commitments over 50 users (40 each); every fourth commitment of
/// users u0..u24 is `open`, users u25..u49 have none open. The production
/// composite `(userId, status)` is declared.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX commitment_user_status FOR (n:Commitment) ON (n.userId, n.status)");
    for i in 0..2000i64 {
        let user = i % 50;
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("c-{i}")));
        m.insert("userId".into(), s(&format!("u{user}")));
        m.insert("title".into(), s(&format!("Commitment {i}")));
        // `i % 4` would never be 0 for an odd user (i = user + 50k): open
        // status follows the user's k-th commitment instead.
        let open = user < 25 && (i / 50) % 4 == 0;
        m.insert("status".into(), s(if open { "open" } else { "done" }));
        m.insert("dueAt".into(), s(&format!("2026-09-{:02}T00:00:{:02}Z", 1 + (i / 60) % 28, i % 60)));
        g.create_node(&["Commitment".into()], &m).expect("commitment");
    }
    g
}

const ORIG: &str = "MATCH (c:Commitment {userId: $userId, status: 'open'}) \
    RETURN properties(c) AS c ORDER BY c.dueAt ASC LIMIT 200";

#[test]
fn a_two_key_map_answers_zero_rows_without_a_record_read() {
    let g = corpus();
    // u30 has 40 commitments, none open: the userId probe alone would read
    // all 40 records to reject them.
    assert_eq!(scanned(&g, ORIG, "u30"), Vec::<Vec<Value>>::new());
    // The indexes are built on their first probe; count the second run.
    let _ = rows(&g, ORIG, "u30");
    let (got, c) = traced(&g, ORIG, "u30");
    assert!(got.is_empty());
    assert_eq!(count_of(&c, INTERSECTED), 1, "{c:?}");
    assert_eq!(count_of(&c, GETS), 0, "no record read for an empty intersection: {c:?}");
}

/// The rows of a user WITH open commitments: byte-identical to the scan,
/// only the survivors read.
#[test]
fn b_the_intersection_keeps_exactly_the_survivors() {
    let g = corpus();
    let want = scanned(&g, ORIG, "u1");
    assert_eq!(want.len(), 10);
    let _ = rows(&g, ORIG, "u1");
    let (got, c) = traced(&g, ORIG, "u1");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, INTERSECTED), 1, "{c:?}");
    assert!(
        count_of(&c, GETS) <= 20,
        "the 10 survivors bound lean and hydrated, not the 40 candidates: {c:?}"
    );
    assert_eq!(count_of(&c, FULL), 10, "{c:?}");
}

/// The WHERE spelling of the second key intersects the same way; a single
/// declared key has nothing to intersect.
#[test]
fn c_the_where_spelling_and_a_single_key() {
    let g = corpus();
    let where_form = "MATCH (c:Commitment {userId: $userId}) WHERE c.status = 'open' \
        RETURN properties(c) AS c ORDER BY c.dueAt ASC LIMIT 200";
    let want = scanned(&g, where_form, "u1");
    assert_eq!(want.len(), 10);
    let _ = rows(&g, where_form, "u1");
    let (got, c) = traced(&g, where_form, "u1");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, INTERSECTED), 1, "{c:?}");

    let single = "MATCH (c:Commitment {userId: $userId}) RETURN count(c) AS n";
    let want = scanned(&g, single, "u30");
    let _ = rows(&g, single, "u30");
    let (got, c) = traced(&g, single, "u30");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, INTERSECTED), 0, "{c:?}");
}
