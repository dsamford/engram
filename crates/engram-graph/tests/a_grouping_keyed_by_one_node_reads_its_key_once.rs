//! An aggregation whose grouping keys all read ONE node finds a node's group by
//! its id after the node's first row, and counts DISTINCT nodes by id — with
//! the answer byte-identical to grouping and deduplicating by value.
//!
//! SNB BI bi9 grouped 4.8M rows by `person.id, person.firstName,
//! person.lastName` and folded `count(DISTINCT post)` and `count(DISTINCT
//! reply)`: per ROW, three property reads, an encoded group key and a map
//! lookup, and an encoded key per DISTINCT site — ~13 s of serial fold at any
//! width. The key is a function of the person within a statement, and a
//! node's canonical key IS its id.
//!
//! The one thing the memo must not change: grouping is by VALUE. Two
//! different people with equal key values are ONE group, because each one's
//! first row finds the group through the value key; the memo only remembers
//! where that led.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

const MEMO: &str = "interp.agg groups memoised by their one key node";

/// Four people — two of them (ids 1 and 1) with EQUAL key values — each the
/// author of messages, every message tagged by a kind, and replies between
/// messages so one message is reached from several rows.
fn people() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "CREATE (:P {id: 1, name: 'Ann'}), (:P {id: 1, name: 'Ann'}), \
                (:P {id: 2, name: 'Bo'}), (:P {id: 3, name: 'Cy'})",
    );
    ddl(
        &g,
        "MATCH (p:P) WITH p ORDER BY p.id, id(p) \
         UNWIND range(1, 5) AS i \
         CREATE (:M {n: i, kind: CASE WHEN i % 2 = 0 THEN 'even' ELSE 'odd' END})-[:BY]->(p)",
    );
    ddl(&g, "MATCH (a:M), (b:M) WHERE a.n = b.n + 1 CREATE (a)-[:RE]->(b)");
    let _ = g.warm();
    g
}

#[test]
fn equal_keys_on_different_nodes_are_still_one_group() {
    let g = people();
    // bi9's shape: a variable-length walk. `r` is each message itself (0 hops)
    // and every message replying to it (1 hop): a reply to message n is each
    // person's message n + 1. The streaming projector folds it when the
    // columnar paths are off — the memo's path; since rev32 the pipeline
    // walks it mid-path when they are on, and must group by value alike.
    let q = "MATCH (p:P)<-[:BY]-(m:M)<-[:RE*0..1]-(r:M) \
             RETURN p.id AS id, p.name AS name, count(DISTINCT m) AS msgs, \
                    count(DISTINCT r) AS rs \
             ORDER BY id, name";
    // Ann is TWO people with one key — ONE group: her 10 messages; and r is
    // those 10 plus every message with n in 2..5 (16), less her own 8 of
    // them: 18. Bo and Cy: 5 messages, 5 + 16 - 4 = 17.
    let want = vec![
        vec![Value::Int(1), Value::Str("Ann".into()), Value::Int(10), Value::Int(18)],
        vec![Value::Int(2), Value::Str("Bo".into()), Value::Int(5), Value::Int(17)],
        vec![Value::Int(3), Value::Str("Cy".into()), Value::Int(5), Value::Int(17)],
    ];
    g.set_columnar_scans(false);
    let (streamed, c) = run(&g, q);
    g.set_columnar_scans(true);
    assert!(c.get(MEMO).copied().unwrap_or(0) > 0, "the memo never engaged: {c:?}");
    assert_eq!(streamed, want, "the streaming projector's groups");
    let (piped, _) = run(&g, q);
    assert_eq!(piped, want, "the pipeline's groups");
}

#[test]
fn distinct_counts_nodes_values_and_a_mix_as_the_canonical_key_does() {
    let g = people();
    let q = "MATCH (p:P)<-[:BY]-(m:M) \
             RETURN p.id AS id, \
                    count(DISTINCT m.kind) AS kinds, \
                    count(DISTINCT CASE WHEN m.n % 2 = 0 THEN m ELSE m.n END) AS mixed, \
                    count(DISTINCT m.n) AS ns \
             ORDER BY id";
    let (got, _) = run(&g, q);
    // Ann (two people): kinds {odd, even}; mixed = her 4 even-n NODES plus
    // the odd n VALUES {1, 3, 5}; ns = {1..5}.
    assert_eq!(
        got[0],
        vec![Value::Int(1), Value::Int(2), Value::Int(4 + 3), Value::Int(5)],
        "{got:?}"
    );
    // Bo and Cy (one person each): 2 even-n nodes plus 3 odd values.
    assert_eq!(got[1], vec![Value::Int(2), Value::Int(2), Value::Int(2 + 3), Value::Int(5)]);
    assert_eq!(got[2], vec![Value::Int(3), Value::Int(2), Value::Int(2 + 3), Value::Int(5)]);
}

#[test]
fn keys_reading_two_variables_are_not_memoised() {
    let g = people();
    let (_, c) = run(
        &g,
        "MATCH (p:P)<-[:BY]-(m:M) RETURN p.id AS id, m.kind AS kind, count(*) AS n",
    );
    assert_eq!(c.get(MEMO).copied().unwrap_or(0), 0, "two key variables memoised: {c:?}");
}

#[test]
fn distinct_relationships_count_as_their_ids_do() {
    // `count(DISTINCT r)` keeps relationships as ids (SNB BI bi6's `like`),
    // where each one was a byte key allocated per row: the same count as
    // `count(DISTINCT id(r))`, beside a DISTINCT over nodes and over values.
    let g = people();
    let q = "MATCH (p:P)<-[:BY]-(m:M)<-[r:RE*0..1]-(x:M) \
             UNWIND (CASE WHEN size(r) = 0 THEN [null] ELSE r END) AS e \
             RETURN p.id AS id, count(DISTINCT e) AS rels, count(DISTINCT id(e)) AS ids, \
                    count(DISTINCT x) AS xs, count(DISTINCT x.n) AS ns \
             ORDER BY id";
    let (got, _) = run(&g, q);
    assert!(!got.is_empty());
    for row in &got {
        assert_eq!(row[1], row[2], "DISTINCT over relationships differs from over their ids: {got:?}");
    }
    assert!(
        got.iter().any(|r| matches!(r[1], Value::Int(n) if n > 1)),
        "vacuous: no group saw two relationships: {got:?}"
    );
}
