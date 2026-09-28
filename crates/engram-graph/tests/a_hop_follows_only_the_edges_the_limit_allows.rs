#![allow(non_snake_case)]
//! LDBC FinBench truncation: follow only the highest-ranked `truncationLimit`
//! edges out of each node on a variable-length hop.
//!
//! The benchmark DEFINES this — "maximum edges traversed at each step" — and
//! introduces it because a hub vertex's degree "may reach million and even
//! billion scales". Nine of the twelve complex reads specify it, and every
//! official implementation applies it through a vendor extension of its own
//! (galaxybase a `CYPHER EXPANDCONFIG` prefix, gpstore a native plugin). Run
//! without it, those nine are a different and unbounded query: from the
//! busiest account at SF10, `-[:transfer*1..3]->` could not even be COUNTED
//! in 600 s, while `count(DISTINCT other)` over the same pattern answered 43
//! in under a second.
//!
//! THIS IS THE ONE THING HERE THAT IS MEANT TO DROP ROWS. Everything else in
//! this engine that skips work must return the same answer; truncation
//! returns a deliberately smaller one. That asymmetry is why it is off unless
//! the server is told, and why the parameters alone do nothing: a stray
//! `truncationLimit` in someone's parameter map must not quietly change what
//! their query means.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const TRUNCATED: &str = "interp.expansion truncated to the step's edge limit";

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

fn params(limit: i64, order: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("truncationLimit".to_string(), Value::Int(limit));
    p.insert("truncationOrder".to_string(), Value::Str(order.to_string()));
    p
}

/// One hub with `width` outgoing transfers, each stamped with a distinct
/// timestamp, so "the most recent N" is a set the test can name exactly.
fn hub(width: i64) -> (Graph, u64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["Account".into()], &m).expect("account")
    };
    let root = mk(0);
    for i in 1..=width {
        let peer = mk(i);
        let mut m = BTreeMap::new();
        // timestamp ASCENDING with id, so the highest ids are the most recent
        m.insert("timestamp".to_string(), Value::Int(1_000 + i));
        g.create_rel(root, "transfer", peer, &m).expect("transfer");
    }
    let _ = g.warm();
    (g, root)
}

const ONE_HOP: &str = "MATCH (:Account {id: 0})-[t:transfer*1..1]->(o:Account) \
                       RETURN o.id AS id ORDER BY id";

/// The parameters alone must do nothing. Truncation drops rows, so it cannot
/// be reachable by a parameter name someone happened to use.
#[test]
fn the_parameters_alone_do_not_truncate() {
    let (g, _) = hub(10);
    g.set_expand_truncation(false);
    let with = rows(&g, ONE_HOP, params(3, "TIMESTAMP_DESCENDING"));
    let without = rows(&g, ONE_HOP, BTreeMap::new());
    assert_eq!(
        with, without,
        "a truncationLimit parameter changed the answer with the server lever off"
    );
    assert_eq!(with.len(), 10, "all ten edges should have been followed");
}

/// Asked for, it keeps exactly the limit — and exactly the RIGHT ones.
#[test]
fn it_keeps_the_most_recent_edges_up_to_the_limit() {
    let (g, _) = hub(10);
    g.set_expand_truncation(true);
    let (out, t) =
        engram_observe::with_trace(|| rows(&g, ONE_HOP, params(3, "TIMESTAMP_DESCENDING")));
    g.set_expand_truncation(false);

    let ids: Vec<i64> = out
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!("id was not an integer"),
        })
        .collect();
    // timestamps ascend with id, so DESCENDING keeps 8, 9, 10
    assert_eq!(ids, vec![8, 9, 10], "kept the wrong three edges");
    assert!(
        counter(&t, TRUNCATED) >= 1,
        "the walk did not report truncating, so the rows were cut somewhere else"
    );
}

/// The order is the parameter's, not a fixed one.
#[test]
fn ascending_keeps_the_other_end_of_the_range() {
    let (g, _) = hub(10);
    g.set_expand_truncation(true);
    let out = rows(&g, ONE_HOP, params(3, "TIMESTAMP_ASCENDING"));
    g.set_expand_truncation(false);
    let ids: Vec<i64> = out
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!("id was not an integer"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "ASCENDING should keep the oldest three");
}

/// A node with fewer edges than the limit is untouched, and says so rather
/// than paying to rank a list it is going to keep whole.
#[test]
fn a_node_under_the_limit_is_not_ranked() {
    let (g, _) = hub(3);
    g.set_expand_truncation(true);
    let (out, t) =
        engram_observe::with_trace(|| rows(&g, ONE_HOP, params(10, "TIMESTAMP_DESCENDING")));
    g.set_expand_truncation(false);
    assert_eq!(
        out.len(),
        3,
        "all three edges should survive a limit of ten"
    );
    assert_eq!(
        counter(&t, TRUNCATED),
        0,
        "nothing was cut, so nothing should have reported a truncation"
    );
}

/// It applies at EVERY step, which is what "at each step" means — a cap on
/// the finished walk would be a different benchmark.
#[test]
fn the_limit_applies_at_every_step_not_once_per_walk() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["Account".into()], &m).expect("account")
    };
    let root = mk(0);
    let mut ts = 1_000i64;
    // root -> 4 accounts, and each of those -> 4 more: 4 + 16 = 20 paths
    // untruncated; with a limit of 2 at each step, 2 + 4 = 6.
    for a in 1..=4i64 {
        let mid = mk(a);
        ts += 1;
        let mut m = BTreeMap::new();
        m.insert("timestamp".to_string(), Value::Int(ts));
        g.create_rel(root, "transfer", mid, &m).expect("rel");
        for b in 1..=4i64 {
            let leaf = mk(a * 100 + b);
            ts += 1;
            let mut m2 = BTreeMap::new();
            m2.insert("timestamp".to_string(), Value::Int(ts));
            g.create_rel(mid, "transfer", leaf, &m2).expect("rel");
        }
    }
    let _ = g.warm();
    let q = "MATCH (:Account {id: 0})-[t:transfer*1..2]->(o:Account) RETURN count(*) AS n";

    let plain = rows(&g, q, BTreeMap::new());
    assert_eq!(plain[0][0], Value::Int(20), "untruncated path count");

    g.set_expand_truncation(true);
    let cut = rows(&g, q, params(2, "TIMESTAMP_DESCENDING"));
    g.set_expand_truncation(false);
    assert_eq!(
        cut[0][0],
        Value::Int(6),
        "a limit of 2 at each step gives 2 one-hop plus 2x2 two-hop paths; \
         a once-per-walk cap would have given a different number"
    );
}

/// A PROPERTY NOTHING CARRIES MUST NOT PRODUCE AN ARBITRARY CUT.
///
/// Truncation exists to drop rows, so the one thing it must never do is drop
/// them by accident. Ranking on a property no edge has leaves every candidate
/// with the same (absent) key, and a cut over that keeps whichever `limit`
/// happened to be enumerated first — a wrong answer wearing the shape of a
/// fast one. It keeps them all instead, and says so.
#[test]
fn a_property_no_edge_carries_does_not_truncate() {
    let (g, _) = hub(10);
    g.set_expand_truncation(true);
    let mut p = params(3, "TIMESTAMP_DESCENDING");
    p.insert(
        "truncationProperty".to_string(),
        Value::Str("no_such_property".to_string()),
    );
    let (out, t) = engram_observe::with_trace(|| rows(&g, ONE_HOP, p));
    g.set_expand_truncation(false);

    assert_eq!(
        out.len(),
        10,
        "ranking on a property nothing carries cut the walk arbitrarily"
    );
    assert_eq!(
        counter(&t, TRUNCATED),
        0,
        "it reported truncating on a property that does not exist"
    );
    assert!(
        counter(
            &t,
            "interp.expansion declined to truncate on a property nothing carries"
        ) >= 1,
        "the decline was not recorded, so a future change could start cutting here silently"
    );
}
