#![allow(non_snake_case)]
//! `var.prop STARTS WITH 'x'` on a DECLARED key seeks the range index as the
//! range `[x, next(x))` instead of walking the label; a wider seek than the
//! per-id cap is still taken when a WALK over the sought ids halves the
//! label; and the string operators vectorise over cached columns.
//!
//! The production shape (2026-09-04): `MATCH (g:GeopoliticalEvent) WHERE
//! g.eventId STARTS WITH 'edgar-8k-' AND g.startAt IS NOT NULL AND
//! datetime(g.startAt) >= datetime($since) …` walked 44k events and parsed
//! 44k datetimes per statement (31 ms) for the 3.9k the prefix names, which
//! Neo4j read from its index (10 ms).
//!
//! The contract: the seek fires (counted) on a declared key whichever way the
//! result is consumed, never on an undeclared one, and every answer equals the
//! seek-less path's.

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
    p.insert("since".to_string(), Value::Str("2026-08-20".to_string()));
    p.insert("pre".to_string(), Value::Str("edgar-8k-".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_property_seek(true);
    let on = rows(g, src);
    g.set_property_seek(false);
    let off = rows(g, src);
    g.set_property_seek(true);
    (on, off)
}

fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_property_seek(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

const PREFIX: &str = "interp.columnar seek probed a declared prefix";
const PROBES: &str = "graph.index prefix probes";
const WALKED: &str = "interp.columnar aggregate walked its probes over a seek";
const VECTORISED: &str = "interp.columnar aggregate counted over cached columns";

/// 4,000 `:Ev`, dense in id space, above the seek floor: every fourth is
/// `edgar-8k-<i>` (1,000 — past the per-id seek cap of 2,048? no: inside it;
/// see `a_prefix_wider_than_the_per_id_cap_is_walked` for the wide case),
/// the rest `other-<i>`; `startAt` on all but every seventh; `sev` numeric.
fn corpus(declare: bool, per_prefix: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    if declare {
        ddl(&g, "CREATE INDEX ev_id IF NOT EXISTS FOR (n:Ev) ON (n.eventId)");
    }
    for i in 0..4000i64 {
        let mut m = BTreeMap::new();
        let prefixed = (i as usize) % per_prefix == 0;
        m.insert(
            "eventId".to_string(),
            Value::Str(if prefixed {
                format!("edgar-8k-{i:06}")
            } else {
                format!("other-{i:06}")
            }),
        );
        if i % 7 != 0 {
            m.insert("startAt".to_string(), Value::Str(format!("2026-08-{:02}", 1 + i % 28)));
        }
        m.insert("sev".to_string(), Value::Float((i % 10) as f64 / 10.0));
        g.create_node(&["Ev".into()], &m).expect("ev");
    }
    g
}

const COUNT: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH 'edgar-8k-' AND e.startAt IS NOT NULL AND e.startAt >= $since RETURN count(e) AS n";
const COUNT_PARAM: &str = "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND e.sev >= 0.5 RETURN count(e) AS n";
const PROJECT: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH 'edgar-8k-00' RETURN e.eventId AS id ORDER BY id LIMIT 5";

#[test]
fn a_declared_key_seeks_the_prefix_and_agrees() {
    let g = corpus(true, 4);
    for src in [COUNT, COUNT_PARAM, PROJECT] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs walk disagree on `{src}`");
        assert!(counter(&g, src, PREFIX) > 0, "`{src}` must probe the declared prefix");
        assert!(counter(&g, src, PROBES) > 0);
    }
    // Fixture sanity: i % 4 == 0, i % 7 != 0, day >= 20 → i % 28 >= 19.
    let expect = (0..4000i64)
        .filter(|i| i % 4 == 0 && i % 7 != 0 && 1 + i % 28 >= 20)
        .count() as i64;
    assert_eq!(rows(&g, COUNT), vec![vec![Value::Int(expect)]]);
}

/// A prefix naming more ids than the per-id cap (2,048) but fewer than half
/// the label is still sought — as a WALK over the sought ids.
#[test]
fn a_prefix_wider_than_the_per_id_cap_is_walked_over_the_seek() {
    let g = corpus(true, 2); // 2,000 prefixed of 4,000 — 4,000 > 2,048 would not; 2,000 < 4,000/2? no: exactly half
    // Half the label does not halve it: the walk seek must DECLINE here…
    let (on, off) = both(&g, COUNT_PARAM);
    assert_eq!(on, off);
    assert_eq!(counter(&g, COUNT_PARAM, WALKED), 0, "half the label is no reduction");
    // …and a third of it is taken.
    let g = corpus(true, 3); // 1,334 prefixed: past the per-id cap? no (< 2,048): it is per-id sought
    let (on, off) = both(&g, COUNT_PARAM);
    assert_eq!(on, off);
    assert!(counter(&g, COUNT_PARAM, PREFIX) > 0);
    // Force the wide case: 40% prefixed → 1,600; still under the per-id cap, so
    // shrink the label's cap by ADDING nodes: 6,000 nodes at 40% = 2,400 > 2,048.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    ddl(&g, "CREATE INDEX ev_id IF NOT EXISTS FOR (n:Ev) ON (n.eventId)");
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "eventId".to_string(),
            Value::Str(if i % 5 < 2 { format!("edgar-8k-{i:06}") } else { format!("other-{i:06}") }),
        );
        m.insert("sev".to_string(), Value::Float((i % 10) as f64 / 10.0));
        g.create_node(&["Ev".into()], &m).expect("ev");
    }
    // The FIRST read: nothing cached yet, the per-id seek is over its cap,
    // so the aggregate walks over the seek. (Once a whole-label walk has
    // kept the columns, a plain count is answered over them as vectors
    // instead — the cheaper plan, and its own test.)
    g.set_property_seek(true);
    let (on, trace) = engram_observe::with_trace(|| rows(&g, COUNT_PARAM));
    assert!(
        trace.counters().get(WALKED).copied().unwrap_or(0) > 0,
        "2,400 of 6,000: past the per-id cap, walked over the seek; counters: {:?}",
        trace.counters()
    );
    g.set_property_seek(false);
    let off = rows(&g, COUNT_PARAM);
    g.set_property_seek(true);
    assert_eq!(on, off);
    assert_eq!(
        on,
        vec![vec![Value::Int((0..6000i64).filter(|i| i % 5 < 2 && i % 10 >= 5).count() as i64)]]
    );
}

/// CONTROL: an undeclared key is never prefix-probed (nothing is built the
/// operator never asked for); the rows agree, and the string operator still
/// vectorises over the cached column on the second read.
#[test]
fn an_undeclared_key_is_not_prefix_probed_but_the_operator_vectorises() {
    let g = corpus(false, 4);
    let (on, off) = both(&g, COUNT);
    assert_eq!(on, off);
    assert_eq!(counter(&g, COUNT, PREFIX), 0);
    assert_eq!(counter(&g, COUNT, PROBES), 0);
    let src = "MATCH (e:Ev) WHERE e.eventId STARTS WITH 'edgar-8k-' AND e.eventId ENDS WITH '0' AND NOT e.eventId CONTAINS '9' RETURN count(e) AS n";
    let first = rows(&g, src); // keeps the column
    assert_eq!(rows(&g, src), first);
    assert!(counter(&g, src, VECTORISED) > 0, "STARTS/ENDS WITH and CONTAINS vectorise");
    assert_eq!(
        first,
        vec![vec![Value::Int(
            (0..4000i64)
                .filter(|i| {
                    let id = format!("edgar-8k-{i:06}");
                    i % 4 == 0 && id.ends_with('0') && !id.contains('9')
                })
                .count() as i64
        )]]
    );
}
