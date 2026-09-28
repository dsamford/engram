//! An algorithm's ceilings are priced from the maintained counts, not by
//! reading every node's degree.
//!
//! `algo_price` refuses a projection that would be too big BEFORE it is built.
//! It summed `count_adjacent_memo` over every vertex to learn the edge count —
//! and on a cold type that memoised probe builds a whole-store degree table.
//!
//! Measured on the SF3 friendship graph (24,328 people, 1.13M friendships),
//! with the phases timed:
//!
//! ```text
//! price        32.64 s
//! build         0.07 s
//! reverse+ids   0.003 s
//! ```
//!
//! The guard cost 460x the build it guards, and its own comment says a refusal
//! must be cheaper than the thing it refuses. Five other hypotheses were tried
//! and falsified first — the kernel, the projection's edge reads, the stream's
//! per-vertex record reads, execution width, and a shared table build — none of
//! which bumped a counter that named them; only the phase timing did.
//!
//! The bound is exact enough to keep every decision: a per-label degree sum
//! cannot exceed the type's total edge count (doubled for an undirected view,
//! where each edge is seen from both ends), so a bound that clears a ceiling
//! proves the exact figure clears it too. A bound that does NOT clear falls
//! through to the exact pass, which decides as before.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CHEAP: &str = "algo.priced from the maintained counts, without a degree pass";
const EXACT: &str = "algo.priced by counting every node's degree";

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

fn err(g: &Graph, src: &str) -> String {
    match run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new()) {
        Ok(_) => panic!("`{src}` was admitted"),
        Err(e) => format!("{e}"),
    }
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// A ring of `n` people, plus `outside` KNOWS edges between nodes that are
/// NOT in the projection's label. The bound counts the whole TYPE, so those
/// edges inflate it; the exact pass counts only the label's own.
fn ring_plus_outside(n: i64, outside: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(i));
        ids.push(g.create_node(&["Person".into()], &m).expect("node"));
    }
    for i in 0..n as usize {
        g.create_rel(ids[i], "KNOWS", ids[(i + 1) % n as usize], &BTreeMap::new())
            .expect("knows");
    }
    let mut bots = Vec::new();
    for i in 0..outside.max(0) + 1 {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(i));
        bots.push(g.create_node(&["Bot".into()], &m).expect("node"));
    }
    for i in 0..outside.max(0) as usize {
        g.create_rel(bots[i], "KNOWS", bots[i + 1], &BTreeMap::new())
            .expect("knows");
    }
    let _ = g.warm();
    g
}

fn ring(n: i64) -> Graph {
    ring_plus_outside(n, 0)
}

const WCC: &str = "CALL engram.algo.wcc.stream({nodeLabels: ['Person'], \
     relationshipTypes: ['KNOWS'], orientation: 'UNDIRECTED'}) \
     YIELD componentId RETURN count(DISTINCT componentId) AS c, count(*) AS n";

#[test]
fn a_projection_within_its_ceilings_is_priced_in_o_of_one() {
    let g = ring(300);
    let (rows, t) = engram_observe::with_trace(|| run(&g, WCC));
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(300)]], "one ring");
    assert!(
        counter(&t, CHEAP) >= 1,
        "the bound did not decide, so the degree pass ran: {:?}",
        t.counters()
    );
    assert_eq!(
        counter(&t, EXACT),
        0,
        "it counted every node's degree anyway: {:?}",
        t.counters()
    );
}

#[test]
fn a_projection_over_its_edge_ceiling_is_still_refused() {
    // THE GUARD, which the cheap bound must not weaken. The bound is an
    // OVER-estimate, so this is the side that cannot go wrong quietly — but
    // it is the side the whole pass exists for.
    let g = ring(300);
    g.set_algo_edge_ceiling(10);
    let e = err(&g, WCC);
    assert!(
        e.contains("edges") && e.contains("ENGRAM_ALGO_EDGE_CEILING"),
        "the refusal did not name the ceiling it hit: {e}"
    );
}

#[test]
fn a_bound_that_does_not_clear_falls_through_to_the_exact_count() {
    // The bound counts the whole TYPE; a label-scoped projection may hold far
    // fewer edges. So a bound over the ceiling must not refuse on its own —
    // it must hand over to the exact pass, which is what decides.
    // 300 ring edges among Persons and 200 more among Bots: the bound counts
    // all 500 of the type (1,000 undirected), the Person projection holds 600.
    let g = ring_plus_outside(300, 200);
    g.set_algo_edge_ceiling(700);
    let (_, t) = engram_observe::with_trace(|| {
        let _ = run_query(&g, &parse_statement(WCC).expect("parses"), BTreeMap::new());
    });
    assert!(
        counter(&t, EXACT) >= 1,
        "the bound decided alone, so a label-scoped projection could be \
         refused for edges it does not hold: {:?}",
        t.counters()
    );
}

#[test]
fn the_answer_is_the_same_however_it_was_priced() {
    let cheap = ring(300);
    let (rows_cheap, t) = engram_observe::with_trace(|| run(&cheap, WCC));
    assert!(counter(&t, CHEAP) >= 1, "{:?}", t.counters());

    let exact = ring_plus_outside(300, 200);
    exact.set_algo_edge_ceiling(700); // the bound (1,000) fails, the exact (600) passes
    let rows_exact = run(&exact, WCC);
    assert_eq!(rows_cheap, rows_exact, "pricing changed an ANSWER");
}
