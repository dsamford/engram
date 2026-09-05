#![allow(non_snake_case)]
//! An EXISTS / COUNT body whose pattern STARTS from a variable the body
//! binds itself — `EXISTS { MATCH (parent:K)-[:HAS]->(w) … }` beside a
//! scanned `w` — survives the columnar `rewrite` untouched ("left for that
//! variable's pass", of which a single-variable stage has none). Three
//! columnar stages then evaluated it hook-less and every spelling errored
//! "EXISTS {} requires a graph context" with the columnar paths on: the
//! label-scan seed filter (`filter_ids_mode`), the WITH-chain stage's MATCH
//! predicate, and the chain's breaker items / ORDER BY / post-WHERE
//! (`RETURN COUNT { … }`). Each now declines to the interpreter, and the
//! answers equal the columnar-off ones.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Value::Str("a".into()));
    m.insert("kind".to_string(), Value::Str("epic".into()));
    let p = g.create_node(&["K".into()], &m).expect("parent");
    let mut m2 = BTreeMap::new();
    m2.insert("id".to_string(), Value::Str("b".into()));
    let w = g.create_node(&["K".into()], &m2).expect("child");
    let mut m3 = BTreeMap::new();
    m3.insert("id".to_string(), Value::Str("c".into()));
    g.create_node(&["K".into()], &m3).expect("orphan");
    g.create_rel(p, "HAS", w, &BTreeMap::new()).expect("edge");
    g
}

fn params() -> BTreeMap<String, Value> {
    let mut params = BTreeMap::new();
    params.insert(
        "ids".to_string(),
        Value::List(vec![Value::Str("b".into()), Value::Str("c".into())]),
    );
    params
}

fn rows(g: &Graph, src: &str, columnar: bool) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}` (columnar {columnar}): {e}"))
        .rows
}

#[test]
fn every_spelling_answers_on_the_columnar_paths_as_it_does_off_them() {
    let g = corpus();
    for src in [
        // The seed filter and the chain's MATCH predicate.
        "MATCH (w:K) WHERE w.id IN $ids AND EXISTS { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } RETURN w.id AS id ORDER BY id",
        "MATCH (w:K) WHERE w.id IN $ids AND EXISTS { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } RETURN w.id AS id",
        "MATCH (w:K) WHERE EXISTS { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } RETURN w.id AS id",
        "MATCH (w:K) WHERE w.id = 'b' AND EXISTS { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } RETURN w.id AS id",
        "MATCH (w:K) WHERE w.id IN $ids AND EXISTS { MATCH (parent:K)-[:HAS]->(w) } RETURN w.id AS id ORDER BY id",
        "MATCH (w:K) WHERE w.id IN $ids AND NOT EXISTS { MATCH (parent:K)-[:HAS]->(w) } RETURN w.id AS id ORDER BY id",
        // The breaker's items, ORDER BY and post-WHERE.
        "MATCH (w:K) WHERE w.id IN $ids RETURN w.id AS id, COUNT { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } AS parents ORDER BY id",
        "MATCH (w:K) WHERE w.id IN $ids RETURN w.id AS id ORDER BY COUNT { MATCH (parent:K)-[:HAS]->(w) } DESC, id",
        "MATCH (w:K) WHERE w.id IN $ids WITH w.id AS id, COUNT { MATCH (parent:K)-[:HAS]->(w) } AS parents WHERE parents > 0 RETURN id",
        "MATCH (w:K) WHERE w.id IN $ids WITH w.id AS id, EXISTS { MATCH (parent:K)-[:HAS]->(w) WHERE parent.kind = 'epic' } AS hasEpic RETURN id, hasEpic ORDER BY id",
        // The spelling written from the bound end never had the problem.
        "MATCH (w:K) WHERE w.id IN $ids AND EXISTS { MATCH (w)<-[:HAS]-(parent:K) WHERE parent.kind = 'epic' } RETURN w.id AS id ORDER BY id",
    ] {
        g.set_columnar_scans(false);
        let want = rows(&g, src, false);
        g.set_columnar_scans(true);
        let got = rows(&g, src, true);
        assert_eq!(got, want, "`{src}`");
        assert!(!want.is_empty(), "fixture: `{src}` answers nothing");
    }
}
