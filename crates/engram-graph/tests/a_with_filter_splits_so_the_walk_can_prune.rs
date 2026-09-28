//! A `WITH … WHERE` is pushed below its projection ONE CONJUNCT AT A TIME, so
//! a per-edge `all(…)` still reaches the walk when a sibling conjunct reads a
//! name the projection invents.
//!
//! LDBC FinBench tcr8 is
//!
//! ```text
//! MATCH p=(src)-[edge:transfer|withdraw*1..3]->(dst:Account)
//! WITH loan, p, dst, edge, [e IN relationships(p) | e.amount] AS amts
//! WHERE all(e IN edge WHERE e.timestamp > $start AND e.timestamp < $end)
//!   AND reduce(curr = head(amts), …) <> -1
//! ```
//!
//! `push_with_filter_before_projection` refused the WHOLE predicate because the
//! `reduce` reads `amts`, which does not exist before the projection. That also
//! kept the `all(…)` half out of the MATCH, where `lift_rel_predicates` would
//! have used it to stop walking through out-of-window edges — so every 1..3-hop
//! path was enumerated and then filtered: 32.5 s at SF10.
//!
//! A row survives `a AND b` only if both are TRUE, so testing `a` before the
//! projection and `b` after it keeps exactly the same rows. The reference
//! spelling in these tests wraps the same conjunction in `(… ) OR false`, which
//! is one conjunct reading the invented name and so cannot be split at all.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SKIPPED: &str = "interp.expansion skipped an edge its own predicate refuses";
const SPLIT: &str = "interp.WITH filter split: part pushed before its projection";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// Root → `width` accounts → `width` more. First-hop edges carry `ts` 0 and an
/// `amount` rising with the target; second-hop edges carry `ts` 100.
fn hub(width: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["Account".into()], &m).expect("account")
    };
    let root = mk(0);
    let first: Vec<u64> = (1..=width as i64).map(&mut mk).collect();
    let second: Vec<u64> = (1001..=1000 + width as i64).map(&mut mk).collect();
    let edge = |a: u64, b: u64, ts: i64, amount: f64| {
        let mut m = BTreeMap::new();
        m.insert("ts".to_string(), Value::Int(ts));
        m.insert("amount".to_string(), Value::Float(amount));
        g.create_rel(a, "transfer", b, &m).expect("transfer");
    };
    for (i, &f) in first.iter().enumerate() {
        edge(root, f, 0, 10.0 + i as f64);
        for (j, &s) in second.iter().enumerate() {
            edge(f, s, 100, 5.0 + 3.0 * j as f64);
        }
    }
    let _ = g.warm();
    g
}

/// The tcr8 shape, with `$window` and `$rest` spliced in. `split` chooses the
/// splittable spelling or the reference one that cannot be split.
fn tcr8_shape(window: &str, rest: &str, tail: &str, split: bool) -> String {
    let pred = if split {
        format!("all(e IN r WHERE {window}) AND {rest}")
    } else {
        format!("(all(e IN r WHERE {window}) AND {rest}) OR false")
    };
    format!(
        "MATCH p=(:Account {{id: 0}})-[r:transfer*1..3]->(o:Account) \
         WITH p, o, r, [e IN relationships(p) | e.amount] AS amts \
         WHERE {pred} {tail}"
    )
}

const MONOTONE: &str = "reduce(curr = head(amts), x IN tail(amts) | \
     CASE WHEN curr <> -1 AND x > curr * 0.5 THEN x ELSE -1 END) <> -1";

/// The point: the window conjunct reaches the walk and stops it at hop one,
/// while the conjunct over `amts` stays behind in the WITH.
#[test]
fn the_window_conjunct_prunes_the_walk() {
    let g = hub(8);
    let q = tcr8_shape("e.ts > 50", MONOTONE, "RETURN count(*) AS n", true);
    let (r, t) = engram_observe::with_trace(|| rows(&g, &q));
    assert_eq!(r, vec![vec![Value::Int(0)]]);
    assert!(counter(&t, SPLIT) >= 1, "the WITH filter was not split");
    assert_eq!(
        counter(&t, SKIPPED),
        8,
        "expected the 8 first-hop edges to be refused during the walk"
    );

    // the reference spelling cannot be split, so nothing reaches the walk —
    // which is what makes it a fair witness for the differential below
    let reference = tcr8_shape("e.ts > 50", MONOTONE, "RETURN count(*) AS n", false);
    let (_, t) = engram_observe::with_trace(|| rows(&g, &reference));
    assert_eq!(counter(&t, SPLIT), 0);
    assert_eq!(counter(&t, SKIPPED), 0);
}

/// Splitting must not change any answer.
#[test]
fn splitting_does_not_change_any_answer() {
    let g = hub(6);
    let cases = [
        // nothing survives the window
        ("e.ts > 50", MONOTONE, "RETURN count(*) AS n"),
        // only the first hop survives the window
        ("e.ts < 50", MONOTONE, "RETURN o.id AS id ORDER BY id"),
        // everything survives the window; the monotone test does the cutting
        ("e.ts >= 0", MONOTONE, "RETURN o.id AS id, length(p) AS d ORDER BY id, d"),
        // tcr8's own tail: distinct last edges, min distance
        (
            "e.ts >= 0",
            MONOTONE,
            "WITH DISTINCT o.id AS id, collect(DISTINCT relationships(p)[-1]) AS edges, \
             min(length(p) + 1) AS d \
             RETURN id, reduce(s = 0.0, e IN edges | s + e.amount) AS inflow, d \
             ORDER BY d DESC, inflow DESC, id",
        ),
        // the conjunct left behind is NULL (one hop), FALSE or TRUE (two hops)
        (
            "e.ts >= 0",
            "amts[1] > 6",
            "RETURN count(*) AS n",
        ),
    ];
    for (i, (window, rest, tail)) in cases.into_iter().enumerate() {
        let split = rows(&g, &tcr8_shape(window, rest, tail, true));
        let whole = rows(&g, &tcr8_shape(window, rest, tail, false));
        assert_eq!(split, whole, "splitting changed the answer: {window} / {rest} / {tail}");
        // only the first case is meant to be empty; the rest must compare REAL rows
        let empty = split.is_empty() || split == vec![vec![Value::Int(0)]];
        assert_eq!(empty, i == 0, "case {i} is vacuous or wrongly empty: {split:?}");
    }
}

/// The conditions on the WITH itself still refuse the push, split or not:
/// an aggregating WITH's WHERE is a HAVING, and a LIMIT picks rows BEFORE its
/// WHERE runs.
#[test]
fn a_with_that_cannot_be_pushed_through_is_still_refused() {
    let g = hub(4);
    for q in [
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WITH o, r, count(*) AS c WHERE all(e IN r WHERE e.ts > 50) AND c > 0 \
         RETURN count(*) AS n",
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WITH o, r, [e IN r | e.ts] AS ts LIMIT 3 WHERE all(e IN r WHERE e.ts > 50) \
         AND size(ts) > 0 RETURN count(*) AS n",
    ] {
        let (_, t) = engram_observe::with_trace(|| rows(&g, q));
        assert_eq!(counter(&t, SPLIT), 0, "`{q}` was split");
        assert_eq!(counter(&t, SKIPPED), 0, "`{q}` reached the walk");
    }
}
