#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Top-k BEFORE projection must be invisible in every answer.
//!
//! # What this guards
//!
//! An aggregating `ORDER BY <agg> LIMIT n` projection used to project EVERY
//! group (template row + an expression evaluation per item) and then let the
//! tail sort and keep n. `agg_topk_survivors` now picks the `skip+limit`
//! survivors off the FINISHED aggregate values with the tail's own comparator
//! and projects only those. The claim is byte-identity, which holds only while:
//!
//! * the selection comparator IS the tail's (`cmp_order_keys` on the same
//!   keys, ASC and DESC);
//! * ties resolve by FIRST-SEEN group order, as the tail's stable sort does;
//! * SKIP counts toward the survivor set (skip+limit, not limit);
//! * shapes the mechanism must decline (a group-key property in ORDER BY, a
//!   limit that does not shrink the set) still answer through the general path.
//!
//! The fixture engineers TIES on purpose: tag `i` receives `(i % 7) + 1`
//! messages, so every count level is shared by ~40 groups and a top-5 is
//! decided by the tie rule, not the key.
//!
//! # Proven to bite — and what it took
//!
//! Checked against a selection whose tie rule is REVERSED (`.then(b.cmp(&a))`
//! on equal keys): `ties_resolve_in_first_seen_order_desc`,
//! `ascending_keys_select_the_smallest`, `skip_counts_toward_the_survivor_set`
//! and `a_node_group_key_projects_only_the_survivors_identically` all fail,
//! while the two decline-path tests stay green — the mechanism is engaged and
//! the canary sees a wrong tie rule.
//!
//! Two earlier cuts of this file passed against that same break, each for a
//! reason worth keeping: (1) a single-hop fixture never reached
//! `project_agg_groups` at all (a vectorized recogniser answered first), and
//! (2) `ORDER BY c` names the ALIAS, which the selector's structural match did
//! not resolve, so it declined on every query and the general path answered.
//! A `sort_unstable_by` break, for the record, did NOT bite on 280 groups —
//! pdqsort preserved the tie order by luck. Unstable is not a proof of stable.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

const TAGS: i64 = 280;

/// ic6's SHAPE, not just its aggregate: one `:P` hub, messages hung off it,
/// tags hung off messages, queried as two MATCH clauses. A single-hop
/// `(m)-[:HAS_TAG]->(t)` is answered by a vectorized recogniser before the
/// columnar pipeline's `project_agg_groups` ever runs — a first cut of this
/// file passed against a deliberately reversed tie rule for exactly that
/// reason. 280 tags; tag i is attached to (i % 7) + 1 messages — dense ties.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    stmt(&g, "CREATE (:P {id: 0})");
    for chunk in 0..(TAGS / 40) {
        let mut q = String::new();
        for i in 0..40 {
            let id = chunk * 40 + i;
            q.push_str(&format!("CREATE (:T {{id: {id}, name: 'tag{id:03}'}}) "));
        }
        stmt(&g, &q);
    }
    for id in 0..TAGS {
        let n = (id % 7) + 1;
        let mut q = format!("MATCH (p:P {{id: 0}}), (t:T {{id: {id}}}) ");
        for k in 0..n {
            q.push_str(&format!(
                "CREATE (p)-[:HAS_M]->(:M {{id: {}}})-[:HAS_TAG]->(t) ",
                id * 10 + k
            ));
        }
        stmt(&g, &q);
    }
    g
}

/// Run `src` with the lever ON and OFF; both must answer identically.
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_agg_topk_before_project(true);
    let on = rows(g, src);
    g.set_agg_topk_before_project(false);
    let off = rows(g, src);
    g.set_agg_topk_before_project(true);
    (on, off)
}

const CHAIN: &str = "MATCH (p:P {id: 0})-[:HAS_M]->(m:M) MATCH (m)-[:HAS_TAG]->(t:T)";

#[test]
fn ties_resolve_in_first_seen_order_desc() {
    let g = fixture();
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t.name AS name, count(*) AS c ORDER BY c DESC LIMIT 5"));
    assert_eq!(on.len(), 5);
    assert_eq!(on, off, "top-5 under dense ties must match the full sort's tie order");
    // Every survivor carries the maximal count (7) — the key was honoured.
    for r in &on {
        assert_eq!(r[1], Value::Int(7), "{r:?}");
    }
}

#[test]
fn ascending_keys_select_the_smallest() {
    let g = fixture();
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t.name AS name, count(*) AS c ORDER BY c ASC LIMIT 4"));
    assert_eq!(on, off);
    for r in &on {
        assert_eq!(r[1], Value::Int(1));
    }
}

#[test]
fn skip_counts_toward_the_survivor_set() {
    let g = fixture();
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t.name AS name, count(*) AS c ORDER BY c DESC SKIP 37 LIMIT 6"));
    // 40 groups carry count 7 (ids 6, 13, …); skipping 37 leaves 3 of them,
    // then the 6-count level starts — a survivor set of `limit` alone would
    // have cut this short.
    assert_eq!(on.len(), 6);
    assert_eq!(on, off);
    assert_eq!(on[2][1], Value::Int(7));
    assert_eq!(on[3][1], Value::Int(6));
}

#[test]
fn a_node_group_key_projects_only_the_survivors_identically() {
    let g = fixture();
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t, count(*) AS c ORDER BY c DESC LIMIT 3"));
    assert_eq!(on, off);
    assert_eq!(on.len(), 3);
}

#[test]
fn a_group_key_property_in_order_by_declines_and_still_answers() {
    let g = fixture();
    // `t.name` is not an aggregate: the mechanism must decline, the general
    // path answers, and the lever is invisible either way.
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t.name AS name, count(*) AS c ORDER BY name ASC LIMIT 3"));
    assert_eq!(on, off);
    assert_eq!(on[0][0], Value::Str("tag000".into()));
}

#[test]
fn a_limit_that_does_not_shrink_the_set_is_identical() {
    let g = fixture();
    let (on, off) = both(&g, &format!("{CHAIN} RETURN t.name AS name, count(*) AS c ORDER BY c DESC LIMIT 1000"));
    assert_eq!(on.len() as i64, TAGS);
    assert_eq!(on, off);
}
