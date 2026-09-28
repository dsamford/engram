#![allow(non_snake_case)]
//! Fix 122: a capped scan visits its chunks from BOTH ENDS only when there is
//! a PREDICATE to meet.
//!
//! Both-ends (fix 82) exists because ids are minted in creation order, so a
//! label's newest members sit at the end of id order, and the listings that
//! cap without ordering are recency-filtered. Meeting a selective filter from
//! the far end first can end the scan in one chunk instead of all of them.
//!
//! With NO filter every member matches. The first chunk already answers the
//! whole limit, and reordering the chunks buys nothing while costing
//! locality. `MATCH (p:Person) RETURN p.id, p.firstName LIMIT 5000` — the
//! platform benchmark's `plat-limit-listing`, and one of only two shapes
//! where engram trails Neo4j — took the reordered path for no reason.
//!
//! Canary, run: restoring `cap.is_some() && n.div_ceil(PRED_CHUNK) > 1`
//! fails `a_a_predicate_less_capped_scan_goes_forward` on a fired counter
//! while every answer assertion stays green.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const BOTH_ENDS: &str =
    "interp.columnar projection scanned its chunks from both ends for the limit";

/// More than one 4,096-member chunk, so the both-ends decision is live at all.
const PERSONS: i64 = 12_000;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, t) = engram_observe::with_trace(|| rows(g, src));
    (r, t.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("firstName".to_string(), Value::Str(format!("P{i}")));
        // A minority flag, so the predicate arm is genuinely selective and
        // its matches sit at the END of id order — both-ends' whole reason.
        m.insert("fresh".to_string(), Value::Bool(i >= PERSONS - 500));
        g.create_node(&["Person".into()], &m).expect("person");
    }
    g.shared_store().seal();
    g
}

const LISTING: &str = "MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 5000";
const FILTERED: &str = "MATCH (p:Person) WHERE p.fresh = true RETURN p.id AS id LIMIT 5";

#[test]
fn a_a_predicate_less_capped_scan_goes_forward() {
    let g = corpus();
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got.len(), 5_000, "the listing did not return its limit");
    assert_eq!(
        count_of(&c, BOTH_ENDS),
        0,
        "a listing with no predicate still reordered its chunks: {c:?}"
    );
}

#[test]
fn b_a_capped_scan_WITH_a_predicate_still_goes_from_both_ends() {
    // The control. Without it, deleting both-ends entirely would pass the
    // test above, and fix 82's win would be silently thrown away.
    let g = corpus();
    let (got, c) = traced(&g, FILTERED);
    assert_eq!(
        got.len(),
        5,
        "the filtered listing did not return its limit"
    );
    assert!(
        count_of(&c, BOTH_ENDS) > 0,
        "a CAPPED scan with a selective predicate stopped scanning from both \
         ends — fix 82's win has been thrown away: {c:?}"
    );
}

#[test]
fn c_the_listing_answers_the_first_rows_in_id_order() {
    // Forward is not merely cheaper here, it is the order the rows come back
    // in, so pin it: a bare LIMIT takes the first k members in id order.
    let g = corpus();
    let got = rows(&g, LISTING);
    let want: Vec<Vec<Value>> = (0..5_000)
        .map(|i| vec![Value::Int(i), Value::Str(format!("P{i}"))])
        .collect();
    assert_eq!(
        got, want,
        "the bare listing is not the first 5,000 in id order"
    );
}
