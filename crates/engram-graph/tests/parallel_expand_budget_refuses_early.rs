#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The PARALLEL expand must refuse the row budget WHILE producing, exactly as
//! the serial loop does — not after materialising the over-budget output.
//!
//! # What this guards
//!
//! `expand_row_slice` workers share a produced-rows account; any worker that
//! pushes the combined total past the budget trips a flag every worker checks
//! per driving row, and `expand_parallel` refuses without merging. Before the
//! account existed, each worker materialised its whole partial and the only
//! check ran after the merge — "identical pass/fail", priced at exactly the
//! memory the budget exists to prevent: LSQB q2's existence probe grew ONE
//! worker's partial past 1.6 GiB (`memory allocation of 1610612736 bytes
//! failed`, diag3) and, uncapped, OOM-killed the 40 Gi bench pod.
//!
//! # The bite
//!
//! The early-stop's memory effect is not observable from the public surface
//! (the refusal TEXT is identical either way), so the mechanism's
//! proven-to-bite is the POD diagnostic: on the unfixed binary the capped
//! server ABORTS in `expand_row_slice`; on the fixed one the same probe must
//! REFUSE. This suite pins the contract around it: refusal parity with
//! serial, byte-identity at the exact budget boundary, and no refusal when
//! no budget is set.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, ScopedExec, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

#[derive(Debug)]
struct TestExec {
    width: usize,
}

impl ScopedExec for TestExec {
    fn width(&self) -> usize {
        self.width
    }
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.width.min(n).max(1);
        if threads <= 1 {
            for i in 0..n {
                f(i);
            }
            return;
        }
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(i);
                    }
                });
            }
        });
    }
}

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

/// 200 `:B` nodes fanned out of one `:H` hub — the two-hop chain below
/// produces 200 × 199 = 39,800 rows (relationship isomorphism excludes the
/// `a == b` pairs: one edge cannot serve both hops), far past a budget of
/// 1,000.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    stmt(&g, "CREATE (:H {id: 0})");
    for chunk in 0..8 {
        let mut q = String::from("MATCH (h:H) ");
        for i in 0..25 {
            let id = chunk * 25 + i;
            q.push_str(&format!("CREATE (h)-[:E]->(:B {{id: {id}}}) "));
        }
        stmt(&g, &q);
    }
    g
}

const CHAIN: &str = "MATCH (a:B)<-[:E]-(h:H)-[:E]->(b:B) RETURN a.id AS x, b.id AS y";

fn run(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e:?}"))
}

fn go_parallel(g: &Graph, width: usize) {
    g.set_exec(Some(Arc::new(TestExec { width })));
    g.set_parallel_expand(true);
    g.set_parallel_min_rows(1);
}

#[test]
fn over_budget_refuses_in_parallel_exactly_as_serial() {
    let g = fixture();
    g.set_row_budget(Some(1_000));
    let serial = run(&g, CHAIN).expect_err("40,000 rows over a 1,000 budget must refuse");
    assert!(serial.contains("row budget exceeded"), "{serial}");
    go_parallel(&g, 4);
    let parallel = run(&g, CHAIN).expect_err("the parallel path must refuse the same statement");
    assert_eq!(serial, parallel, "one refusal, either path");
}

#[test]
fn at_the_exact_budget_both_paths_pass_byte_identically() {
    let g = fixture();
    // The chain's full output is exactly 39,800 rows: AT the budget is not
    // over it, so both paths must answer — and identically, row for row.
    g.set_row_budget(Some(39_800));
    let serial = run(&g, CHAIN).expect("at-budget passes serially");
    go_parallel(&g, 3); // 3 does not divide the driving rows: the merge-boundary case
    let parallel = run(&g, CHAIN).expect("at-budget passes in parallel");
    assert_eq!(serial.len(), 39_800);
    assert_eq!(serial, parallel, "byte-identical at the boundary");
}

#[test]
fn with_no_budget_the_parallel_path_never_refuses() {
    let g = fixture();
    assert!(g.row_budget().is_none(), "no budget by default");
    go_parallel(&g, 4);
    let rows = run(&g, CHAIN).expect("no budget, no refusal");
    assert_eq!(rows.len(), 39_800);
}
