#![allow(non_snake_case)]
//! `ORDER BY toInteger(x.prop)` (and other scalar fns over one var) now vectorize
//! in the columnar top-k — `key_side`/`eval_column` gained a scalar-`Call` arm
//! reusing the SAME registry the per-tuple path uses, so columnar == interp. This
//! is the A1 primitive that unblocks IC11/IC12's `ORDER BY toInteger(personId)`.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Persons with a KNOWS edge each to a friend carrying an id + a rank.
    for id in [3i64, 1, 2, 10, 4] {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(id));
        p.insert("rank".to_string(), Value::Float(id as f64 + 0.5));
        g.create_node(&["Person".into()], &p).expect("person");
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
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

fn fired_core(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    // A core top-k fires the hop-runs counter; the tell we care about is that it
    // did NOT fall to the streaming interp.
    !trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

#[test]
fn order_by_tointeger_is_byte_identical_and_fires() {
    let g = g();
    // ORDER BY toInteger(p.id) ASC — the same order as p.id, but via a scalar fn
    // the old key_side declined.
    let src = "MATCH (p:Person) RETURN p.id AS pid ORDER BY toInteger(p.id) ASC LIMIT 10";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "toInteger ORDER BY columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![i(1)], vec![i(2)], vec![i(3)], vec![i(4)], vec![i(10)]],
        "sorted ascending by the integer id"
    );
    assert!(
        fired_core(&g, src),
        "the toInteger ORDER BY key must NOT fall to the streaming interp"
    );
}

/// `toInteger` on a FLOAT truncates — still order-preserving, and columnar must
/// match the interp's truncation exactly.
#[test]
fn order_by_tointeger_of_float_matches() {
    let g = g();
    let src = "MATCH (p:Person) RETURN p.id AS pid ORDER BY toInteger(p.rank) DESC LIMIT 3";
    let (on, off) = both(&g, src);
    assert_eq!(
        on, off,
        "toInteger(float) ORDER BY columnar vs interp disagree"
    );
    // rank = id+0.5; toInteger truncates to id; DESC → 10,4,3.
    assert_eq!(on, vec![vec![i(10)], vec![i(4)], vec![i(3)]]);
}

/// A scalar fn in a WHERE also vectorizes now (eval_column computes it) — and
/// stays byte-identical.
#[test]
fn where_tointeger_matches() {
    let g = g();
    let src = "MATCH (p:Person) WHERE toInteger(p.rank) >= 4 RETURN p.id AS pid ORDER BY p.id ASC LIMIT 10";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "toInteger WHERE columnar vs interp disagree");
    assert_eq!(on, vec![vec![i(4)], vec![i(10)]], "rank>=4.5 → id 4 and 10");
}
