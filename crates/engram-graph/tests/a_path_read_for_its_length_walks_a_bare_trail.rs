#![allow(non_snake_case)]
//! A path variable whose every use is `length(p)` or `relationships(p)` (or
//! a bare `WITH p` carry) never exposes a trail NODE, so the walk binds them
//! bare, id only, instead of reading each one's record in full. LDBC FinBench
//! tcr1 walks `p=(account)-[transfer:transfer*1..3]->(other)` and reads
//! `length(p)` and `[e IN relationships(p) | e.timestamp]`: at SF10 that read
//! 23,146 Account records in full, one per trail node, which nothing read.
//!
//! The oracle is the same statement with one more use of the path that DOES
//! see its nodes (`size(nodes(p)) > 0`, true of every path), which keeps the
//! full trail: the rows must agree. The controls are the uses that must keep
//! it — the path returned whole, `nodes(p)`, a renaming carry, a comprehension
//! shadowing the name.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Int(0));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, p.clone())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const BARE: &str = "interp.path walked with a bare trail: nothing reads its nodes";
const FULL_NODES: &str = "graph.nodes materialised in full";

/// Forty accounts; account k transfers to k+1, k+2 and k+5 (mod 40), each
/// transfer stamped `ts` = 10 x k + the step. Accounts carry a `name`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for k in 0..40i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(k));
        m.insert("name".to_string(), Value::Str(format!("account {k}")));
        ids.push(g.create_node(&["Account".into()], &m).expect("account"));
    }
    for k in 0..40i64 {
        for step in [1i64, 2, 5] {
            let mut r = BTreeMap::new();
            r.insert("ts".to_string(), Value::Int(10 * k + step));
            let to = ((k + step) % 40) as usize;
            g.create_rel(ids[k as usize], "transfer", ids[to], &r).expect("transfer");
        }
    }
    g
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    rows
}

/// tcr1's shape: the path read for its length and its relationships'
/// timestamps, carried through a WITH.
#[test]
fn a_a_path_read_for_its_length_and_relationships_walks_a_bare_trail() {
    let g = corpus();
    let lean = "MATCH p=(a:Account {id: $id})-[t:transfer*1..3]->(b:Account) \
        WITH p, [e IN relationships(p) | e.ts] AS ts, b \
        RETURN b.id AS b, length(p) AS len, ts ORDER BY b, len, ts";
    let full = "MATCH p=(a:Account {id: $id})-[t:transfer*1..3]->(b:Account) \
        WHERE size(nodes(p)) > 0 \
        WITH p, [e IN relationships(p) | e.ts] AS ts, b \
        RETURN b.id AS b, length(p) AS len, ts ORDER BY b, len, ts";
    let (want, cf) = traced(&g, full);
    let (got, cl) = traced(&g, lean);
    assert_eq!(want.len(), 3 + 9 + 27, "fixture: every 1-3 hop walk from account 0");
    assert_eq!(sorted(got), sorted(want), "the bare trail changed the rows");
    assert_eq!(count_of(&cf, BARE), 0, "the oracle's trail was bare: {cf:?}");
    assert!(count_of(&cl, BARE) > 0, "the trail was not bare: {cl:?}");
    assert!(
        count_of(&cl, FULL_NODES) + 39 <= count_of(&cf, FULL_NODES),
        "the trail's nodes were still read in full: lean {cl:?} full {cf:?}"
    );
}

/// Shapes that see a trail node keep the full trail, and see its properties.
#[test]
fn b_a_use_that_sees_the_nodes_keeps_the_full_trail() {
    let g = corpus();
    // the path returned whole
    let (got, c) = traced(
        &g,
        "MATCH p=(a:Account {id: $id})-[:transfer*2..2]->(b:Account) RETURN p AS p, b.id AS b ORDER BY b",
    );
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    assert_eq!(got.len(), 9);
    for row in &got {
        let Value::Path(items) = &row[0] else { panic!("{:?}", row[0]) };
        for v in items.iter() {
            if let Value::Node { props, .. } = v {
                assert!(props.contains_key("name"), "a trail node lost its properties: {v:?}");
            }
        }
    }
    // nodes(p)
    let (got, c) = traced(
        &g,
        "MATCH p=(a:Account {id: $id})-[:transfer*2..2]->(b:Account) \
         RETURN [n IN nodes(p) | n.name] AS names ORDER BY names",
    );
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    assert_eq!(got.len(), 9);
    assert!(
        got.iter().all(|r| matches!(&r[0], Value::List(l) if l.iter().all(|n| matches!(n, Value::Str(_))))),
        "{got:?}"
    );
    // a renaming carry
    let (_, c) = traced(
        &g,
        "MATCH p=(a:Account {id: $id})-[:transfer*1..2]->(b:Account) WITH p AS q RETURN length(q) AS len",
    );
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    // a comprehension shadowing the name
    let (_, c) = traced(
        &g,
        "MATCH p=(a:Account {id: $id})-[:transfer*1..2]->(b:Account) \
         RETURN length(p) AS len, [p IN [1, 2] | p] AS xs",
    );
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
}

/// A bare carry through a WITH and a later `length(p)`: bare, same rows.
#[test]
fn c_a_carried_path_read_for_its_length_is_bare() {
    let g = corpus();
    let lean = "MATCH p=(a:Account {id: $id})-[:transfer*1..3]->(b:Account) \
        WITH p, b WHERE b.id < 20 RETURN b.id AS b, length(p) AS len ORDER BY b, len";
    let full = "MATCH p=(a:Account {id: $id})-[:transfer*1..3]->(b:Account) \
        WITH p, b WHERE b.id < 20 AND size(nodes(p)) > 0 RETURN b.id AS b, length(p) AS len ORDER BY b, len";
    let (want, cf) = traced(&g, full);
    let (got, cl) = traced(&g, lean);
    assert!(!want.is_empty());
    assert_eq!(got, want);
    assert_eq!(count_of(&cf, BARE), 0, "{cf:?}");
    assert!(count_of(&cl, BARE) > 0, "{cl:?}");
}
