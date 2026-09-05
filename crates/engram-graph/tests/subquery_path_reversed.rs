#![allow(non_snake_case)]
//! A subquery path is driven from its CONSTANT END (fix 23, v99): inside an
//! `EXISTS {}` / `COUNT {}` body, a path whose start is unbound and has no
//! constant declared seek, and whose end is bound or carries one, is
//! reversed — the constant seek runs once per statement, the correlated map
//! is tested at the far end.
//!
//! The production shape (2026-09-04, v98 traced on the pod): `MATCH
//! (o:MarketOrchestrator) WHERE $ticker IN o.watchlist OR EXISTS { MATCH
//! (o)-[:WATCHES]->(wt:Ticker) MATCH (wc:Company {primaryTicker:
//! wt.symbol})-[:SUPPLIES*1..2]-(c:Company {primaryTicker: $ticker}) } …`
//! seeded `wc` 29 times per row and expanded SUPPLIES two hops from each —
//! 3,050 projected node reads to test an end that names zero companies
//! (12.6 ms against Neo4j's 1.8).
//!
//! Every answer is checked against the same body evaluated as written (the
//! index undeclared, so nothing reverses), and against the fixture.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("t".to_string(), Value::Str("T5".to_string()));
    p.insert("t2".to_string(), Value::Str("T7".to_string()));
    p.insert("none".to_string(), Value::Str("TX".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const REVERSED: &str = "interp.subquery path reversed to its constant end";

/// 600 `:Co {primaryTicker: T<i>}` in a SUPPLIES chain (i → i+1), 3
/// `:Orch {id}` each WATCHING ten `:Tk {symbol}`: Orch k watches T<10k>..T<10k+9>.
/// With `$t = T5`, `SUPPLIES*1..2` (undirected) from Co5 reaches Co3, Co4,
/// Co6 and Co7 — never Co5 itself, since a two-hop path back to its start
/// would walk one relationship twice — so exactly Orch 0 watches a
/// supplier-neighbour, through FOUR tickers.
fn corpus(declare: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    if declare {
        ddl(&g, "CREATE INDEX co_ticker IF NOT EXISTS FOR (n:Co) ON (n.primaryTicker)");
    }
    let mut cos = Vec::with_capacity(600);
    for i in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert("primaryTicker".to_string(), Value::Str(format!("T{i}")));
        cos.push(g.create_node(&["Co".into()], &m).expect("co"));
    }
    for i in 0..599usize {
        g.create_rel(cos[i], "SUPPLIES", cos[i + 1], &BTreeMap::new()).expect("supplies");
    }
    for k in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(k));
        let o = g.create_node(&["Orch".into()], &m).expect("orch");
        for j in 0..10i64 {
            let mut t = BTreeMap::new();
            t.insert("symbol".to_string(), Value::Str(format!("T{}", 10 * k + j)));
            let tk = g.create_node(&["Tk".into()], &t).expect("tk");
            g.create_rel(o, "WATCHES", tk, &BTreeMap::new()).expect("watches");
        }
    }
    g
}

const TWO_CLAUSE: &str = "MATCH (o:Orch) WHERE EXISTS { MATCH (o)-[:WATCHES]->(wt:Tk) MATCH (wc:Co {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c:Co {primaryTicker: $t}) } RETURN o.id AS id ORDER BY id";
const PATTERN_BODY: &str = "MATCH (o:Orch)-[:WATCHES]->(wt:Tk) WHERE EXISTS { (wc:Co {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c:Co {primaryTicker: $t}) } RETURN count(*) AS n";
const BOUND_END: &str = "MATCH (c:Co {primaryTicker: $t}) MATCH (o:Orch)-[:WATCHES]->(wt:Tk) WHERE EXISTS { (wc:Co {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c) } RETURN count(*) AS n";
const COUNT_FORM: &str = "MATCH (o:Orch) RETURN o.id AS id, COUNT { MATCH (o)-[:WATCHES]->(wt:Tk) MATCH (wc:Co {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c:Co {primaryTicker: $t}) } AS n ORDER BY id";
/// CONTROL: the start carries a constant declared seek of its own — kept as
/// written (the author's order is the tie-break), the correlated map at the end.
const CONSTANT_START: &str = "MATCH (o:Orch)-[:WATCHES]->(wt:Tk) WHERE EXISTS { (wc:Co {primaryTicker: $t})-[:SUPPLIES*1..2]-(c:Co {primaryTicker: wt.symbol}) } RETURN count(*) AS n";

#[test]
fn a_correlated_start_and_a_constant_declared_end_reverse_and_agree() {
    let on = corpus(true);
    let off = corpus(false);
    for src in [TWO_CLAUSE, PATTERN_BODY, COUNT_FORM] {
        // Warm both (the first statement on a fresh graph builds the label
        // membership and the index — one-time costs, not the shape's).
        let _ = rows(&off, src);
        let _ = rows(&on, src);
        let (want, c_off) = traced(&off, src);
        let (got, c_on) = traced(&on, src);
        assert_eq!(got, want, "reversed vs as written disagree on `{src}`");
        assert!(count_of(&c_on, REVERSED) > 0, "`{src}` must reverse: {c_on:?}");
        assert_eq!(count_of(&c_off, REVERSED), 0, "undeclared: kept as written: {c_off:?}");
    }
    // The production asymmetry: the constant end names NOTHING. Driven from
    // it, the body reads no record at all; as written it seeks and expands
    // from every watched ticker to find that out.
    for src in [TWO_CLAUSE, PATTERN_BODY, COUNT_FORM] {
        let src = src.replace("$t}", "$none}");
        let _ = rows(&off, &src);
        let _ = rows(&on, &src);
        let (want, c_off) = traced(&off, &src);
        let (got, c_on) = traced(&on, &src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c_on, REVERSED) > 0, "{c_on:?}");
        assert!(
            count_of(&c_on, "store.gets") < count_of(&c_off, "store.gets") / 4,
            "`{src}` reads far fewer records from its empty constant end: {} vs {}\nreversed: {c_on:?}\nas written: {c_off:?}",
            count_of(&c_on, "store.gets"),
            count_of(&c_off, "store.gets")
        );
    }
    assert_eq!(rows(&on, TWO_CLAUSE), vec![vec![Value::Int(0)]]);
    assert_eq!(rows(&on, PATTERN_BODY), vec![vec![Value::Int(4)]]);
    assert_eq!(
        rows(&on, COUNT_FORM),
        vec![
            vec![Value::Int(0), Value::Int(4)],
            vec![Value::Int(1), Value::Int(0)],
            vec![Value::Int(2), Value::Int(0)],
        ]
    );
}

/// An end already BOUND in the outer row reverses with or without an index
/// — a bound start is the best seed there is.
#[test]
fn a_bound_end_reverses_regardless_of_the_catalogue() {
    for declare in [true, false] {
        let g = corpus(declare);
        let (got, c) = traced(&g, BOUND_END);
        assert_eq!(got, vec![vec![Value::Int(4)]], "declare={declare}");
        assert!(count_of(&c, REVERSED) > 0, "declare={declare}: {c:?}");
    }
}

/// CONTROL: a start with its own constant declared seek is never reversed;
/// a path with a path variable or `shortestPath` is never reversed either.
#[test]
fn a_constant_start_is_kept_as_written() {
    let g = corpus(true);
    let (got, c) = traced(&g, CONSTANT_START);
    assert_eq!(got, vec![vec![Value::Int(4)]]); // the same four tickers, from the other end
    assert_eq!(count_of(&c, REVERSED), 0, "{c:?}");
    let named = "MATCH (o:Orch)-[:WATCHES]->(wt:Tk) WHERE EXISTS { p = (wc:Co {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c:Co {primaryTicker: $t}) } RETURN count(*) AS n";
    let (got, c) = traced(&g, named);
    assert_eq!(got, vec![vec![Value::Int(4)]]);
    assert_eq!(count_of(&c, REVERSED), 0, "a named path keeps its direction: {c:?}");
}
