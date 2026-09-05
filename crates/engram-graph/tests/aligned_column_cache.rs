#![allow(non_snake_case)]
//! A column-at-a-time count reads its cached columns ALIGNED and BORROWED
//! (fix 23, v99): the aligned vector is built once per column and kept in
//! the property-column cache; `eval_column` references an input column
//! instead of copying it; `needle IN coalesce(<column of lists>, <const>)`
//! tests each row's own list in place.
//!
//! The production shape (2026-09-04, v97 on the pod): `MATCH
//! (g:GeopoliticalEvent) WHERE g.startAt IS NOT NULL AND $a IN
//! coalesce(g.affectedCountries, []) RETURN count(g)` — 17.8 ms against
//! Neo4j's 6.9, with 44k lists copied three times per statement (the align,
//! the column reference, the `coalesce` argument).
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("a".to_string(), Value::Str("USA".to_string()));
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

const VECTORISED: &str = "interp.columnar aggregate counted over cached columns";
const ALIGNED: &str = "graph.property column aligned";
const SERVED_ALIGNED: &str = "graph.property column served aligned";
const KEPT_ALIGNED: &str = "graph.property column kept aligned";

/// 3,000 `:Ev`: `countries` is a list on two of three (`['USA', 'CAN']`
/// on every sixth, `['DEU']` on the rest that have one), absent on every
/// third; `startAt` on all but every seventh; `sev` numeric.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        if i % 3 != 0 {
            m.insert(
                "countries".to_string(),
                Value::List(if i % 6 == 1 {
                    vec![Value::Str("USA".into()), Value::Str("CAN".into())]
                } else {
                    vec![Value::Str("DEU".into())]
                }),
            );
        }
        if i % 7 != 0 {
            m.insert("startAt".to_string(), Value::Str(format!("2026-08-{:02}", 1 + i % 28)));
        }
        m.insert("sev".to_string(), Value::Float((i % 10) as f64 / 10.0));
        g.create_node(&["Ev".into()], &m).expect("ev");
    }
    g
}

const COALESCE_IN: &str =
    "MATCH (e:Ev) WHERE e.startAt IS NOT NULL AND $a IN coalesce(e.countries, []) RETURN count(e) AS n";
const IN_COLUMN: &str = "MATCH (e:Ev) WHERE $a IN e.countries RETURN count(e) AS n";
/// A constant list on the right (with the property seek off — see the test).
const IN_CONST: &str =
    "MATCH (e:Ev) WHERE e.sev >= 0.0 AND e.startAt IN ['2026-08-01', '2026-08-05'] RETURN count(e) AS n";

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

#[test]
fn the_vectorised_count_reads_aligned_columns_kept_by_the_cache() {
    let g = corpus();
    let want = general(&g, COALESCE_IN);
    // i % 6 == 1 (USA) and i % 7 != 0 (startAt present).
    let expect = (0..3000i64).filter(|i| i % 6 == 1 && i % 7 != 0).count() as i64;
    assert_eq!(want, vec![vec![Value::Int(expect)]], "fixture");
    // First read: the walk assembles and keeps the columns.
    let (first, c1) = traced(&g, COALESCE_IN);
    assert_eq!(first, want);
    assert_eq!(count_of(&c1, VECTORISED), 0, "nothing cached yet: {c1:?}");
    // Second read: column-at-a-time, aligning the value column ONCE and
    // keeping the aligned vector.
    let (second, c2) = traced(&g, COALESCE_IN);
    assert_eq!(second, want);
    assert!(count_of(&c2, VECTORISED) > 0, "vectorised: {c2:?}");
    assert_eq!(count_of(&c2, ALIGNED), 1, "one value column aligned once: {c2:?}");
    assert_eq!(count_of(&c2, KEPT_ALIGNED), 1, "…and kept: {c2:?}");
    assert_eq!(count_of(&c2, SERVED_ALIGNED), 0, "{c2:?}");
    // Third read: the aligned vector is served — no align at all.
    let (third, c3) = traced(&g, COALESCE_IN);
    assert_eq!(third, want);
    assert!(count_of(&c3, VECTORISED) > 0, "{c3:?}");
    assert_eq!(count_of(&c3, ALIGNED), 0, "served, not re-aligned: {c3:?}");
    assert_eq!(count_of(&c3, SERVED_ALIGNED), 1, "{c3:?}");
}

/// The three-valued rule survives the borrowed path: a needle against a
/// bare column of lists (null where absent ⇒ Null ⇒ not counted), and
/// against a constant list.
#[test]
fn borrowed_membership_keeps_the_three_valued_rule() {
    let g = corpus();
    // The vectorised path is the subject: with the property seek on, a
    // `prop IN [literals]` conjunct is a seek candidate and the per-id
    // seek answers instead (its own tests).
    g.set_property_seek(false);
    for src in [IN_COLUMN, IN_CONST, COALESCE_IN] {
        let want = general(&g, src);
        assert_eq!(rows(&g, src), want, "first (walk) `{src}`");
        let (again, c) = traced(&g, src);
        assert_eq!(again, want, "second (vectorised) `{src}`");
        assert!(count_of(&c, VECTORISED) > 0, "`{src}` vectorises: {c:?}");
    }
    assert_eq!(
        rows(&g, IN_COLUMN),
        vec![vec![Value::Int((0..3000i64).filter(|i| i % 6 == 1).count() as i64)]]
    );
    assert_eq!(
        rows(&g, IN_CONST),
        vec![vec![Value::Int(
            (0..3000i64)
                .filter(|i| i % 7 != 0 && [1, 5].contains(&(1 + i % 28)))
                .count() as i64
        )]]
    );
}

/// A commit retires the column AND its aligned vector: the next read
/// aligns afresh over the new membership, and counts the new node.
#[test]
fn a_commit_retires_the_aligned_vector_with_its_column() {
    let g = corpus();
    let before = rows(&g, COALESCE_IN);
    let (_, c) = traced(&g, COALESCE_IN);
    assert!(count_of(&c, ALIGNED) > 0, "{c:?}");
    let mut m = BTreeMap::new();
    m.insert("countries".to_string(), Value::List(vec![Value::Str("USA".into())]));
    m.insert("startAt".to_string(), Value::Str("2026-09-01".into()));
    g.create_node(&["Ev".into()], &m).expect("ev");
    let (after, c) = traced(&g, COALESCE_IN);
    assert_eq!(count_of(&c, SERVED_ALIGNED), 0, "retired by the commit: {c:?}");
    assert_eq!(after, general(&g, COALESCE_IN));
    let Value::Int(b) = before[0][0] else { panic!() };
    assert_eq!(after, vec![vec![Value::Int(b + 1)]]);
}
