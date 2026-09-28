#![allow(non_snake_case)]
//! A hop that binds a FEW relationships lean (fewer than a batch) reads each
//! one's demanded properties when its frame completes, and only if its end
//! passes. They used to be read at the push, before the end was tested, so a
//! relationship whose end then failed paid a projected record read for
//! properties nothing looked at. LDBC FinBench tcr1 walks
//! `(other)<-[signIn:signIn]-(medium:Medium {isBlocked: true})` once per
//! transfer path, about one edge each: at SF10 that read 25,993 signIn records
//! one by one for their timestamps, and only 8% of media are blocked.
//!
//! The oracle is the same pattern with the relationship returned WHOLE, which
//! takes the full walk: the rows must agree, OPTIONAL rows and a WHERE on the
//! relationship's property included.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn traced(g: &Graph, src: &str, n: i64) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut p = BTreeMap::new();
    p.insert("n".to_string(), Value::Int(n));
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

const PROJECTED: &str = "interp.matcher bound a relationship by a projected read";
const DEFERRED: &str =
    "interp.matcher deferred a lean relationship's properties until its end passed";
const FULL_RELS: &str = "graph.rels materialised in full";

/// Thirty accounts and two hundred media, a quarter of them blocked. Each
/// account is signed in by five media (`timestamp` = 100 x account + j);
/// accounts 20-29 only by unblocked ones. No index on `isBlocked`, so the end
/// map is tested per peer, as tcr1's is.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut media = Vec::new();
    for i in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(1000 + i));
        m.insert("isBlocked".to_string(), Value::Bool(i % 4 == 0));
        media.push(g.create_node(&["Medium".into()], &m).expect("medium"));
    }
    for k in 0..30i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(k));
        let a = g.create_node(&["Account".into()], &m).expect("account");
        for j in 0..5i64 {
            let mut i = (k * 7 + j * 13) % 200;
            if k >= 20 && i % 4 == 0 {
                i += 1; // never blocked
            }
            let mut r = BTreeMap::new();
            r.insert("timestamp".to_string(), Value::Int(100 * k + j));
            g.create_rel(media[i as usize], "signIn", a, &r).expect("signIn");
        }
    }
    g
}

/// The statement with the relationship returned whole, projected in Rust as
/// the lean statement projects it.
fn oracle(g: &Graph, whole: &str, n: i64) -> Vec<Vec<Value>> {
    let (rows, c) = traced(g, whole, n);
    assert!(count_of(&c, FULL_RELS) > 0, "the oracle did not take the full walk: {c:?}");
    let mut out: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|r| {
            let t = match &r[2] {
                Value::Rel { props, .. } => props.get("timestamp").cloned().unwrap_or(Value::Null),
                Value::Null => Value::Null,
                other => panic!("{other:?}"),
            };
            vec![r[0].clone(), r[1].clone(), t]
        })
        .collect();
    out.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    out
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    rows
}

/// tcr1's shape: the end fails for three edges in four, and only the edges
/// whose medium is blocked are read.
#[test]
fn a_a_relationship_whose_end_fails_is_never_read() {
    let g = corpus();
    let lean = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) \
        RETURN a.id AS a, m.id AS m, s.timestamp AS t ORDER BY a, t, m";
    let whole = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) \
        RETURN a.id AS a, m.id AS m, s AS s";
    let want = oracle(&g, whole, 30);
    let (got, c) = traced(&g, lean, 30);
    assert_eq!(sorted(got.clone()), want, "the lean rows differ from the full walk's");
    assert!(!got.is_empty() && got.len() < 150, "fixture: {} rows", got.len());
    assert!(got.iter().all(|r| r[2] != Value::Null), "a timestamp went missing: {got:?}");
    assert_eq!(count_of(&c, FULL_RELS), 0, "{c:?}");
    assert_eq!(count_of(&c, DEFERRED), 150, "every edge was deferred: {c:?}");
    assert_eq!(
        count_of(&c, PROJECTED),
        got.len() as u64,
        "one read per edge whose end passed, none for the rest: {c:?}"
    );
}

/// A WHERE on the relationship's property, decided at the hop (`early_hop_filters`)
/// after the end passed: the property is there when it is asked.
#[test]
fn b_a_where_on_the_relationship_sees_its_property() {
    let g = corpus();
    let lean = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) WHERE s.timestamp % 2 = 0 \
        RETURN a.id AS a, m.id AS m, s.timestamp AS t ORDER BY a, t, m";
    let whole = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) WHERE s.timestamp % 2 = 0 \
        RETURN a.id AS a, m.id AS m, s AS s";
    let want = oracle(&g, whole, 30);
    let (got, c) = traced(&g, lean, 30);
    assert_eq!(sorted(got.clone()), want);
    assert!(!got.is_empty(), "fixture");
    assert!(count_of(&c, DEFERRED) > 0, "{c:?}");
    assert!(
        got.iter().all(|r| matches!(r[2], Value::Int(t) if t % 2 == 0)),
        "{got:?}"
    );
}

/// OPTIONAL: an account none of whose media is blocked keeps its null row,
/// and reads nothing for it.
#[test]
fn c_an_optional_hop_keeps_its_null_rows() {
    let g = corpus();
    let lean = "MATCH (a:Account) WHERE a.id < $n \
        OPTIONAL MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) \
        RETURN a.id AS a, m.id AS m, s.timestamp AS t ORDER BY a, t, m";
    let whole = "MATCH (a:Account) WHERE a.id < $n \
        OPTIONAL MATCH (a)<-[s:signIn]-(m:Medium {isBlocked: true}) \
        RETURN a.id AS a, m.id AS m, s AS s";
    let want = oracle(&g, whole, 30);
    let (got, c) = traced(&g, lean, 30);
    assert_eq!(sorted(got.clone()), want);
    let nulls = got.iter().filter(|r| r[1] == Value::Null).count();
    assert_eq!(nulls, 10, "accounts 20-29 have no blocked medium: {got:?}");
    let matched = got.len() - nulls;
    assert_eq!(count_of(&c, PROJECTED), matched as u64, "{c:?}");
}

/// CONTROL: an end every edge passes reads every edge, once.
#[test]
fn d_an_end_every_edge_passes_reads_each_once() {
    let g = corpus();
    let lean = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium) \
        RETURN a.id AS a, m.id AS m, s.timestamp AS t ORDER BY a, t, m";
    let whole = "MATCH (a:Account) WHERE a.id < $n \
        MATCH (a)<-[s:signIn]-(m:Medium) \
        RETURN a.id AS a, m.id AS m, s AS s";
    let want = oracle(&g, whole, 30);
    let (got, c) = traced(&g, lean, 30);
    assert_eq!(sorted(got), want);
    assert_eq!(want.len(), 150, "fixture");
    assert_eq!(count_of(&c, PROJECTED), 150, "{c:?}");
}
