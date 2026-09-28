//! A WRITING statement's MATCH holds only the rows its WHERE keeps.
//!
//! The per-row matcher, the one every writing statement takes, bound every
//! start candidate of a label at once and tested the WHERE only after every
//! row was collected. On 2026-09-27 the stress protocol's reset,
//! `MATCH (m:Message) WHERE m.id >= $base DETACH DELETE m` — a delete of
//! NOTHING, after a read-only workload — held ~29M rows at SF10 and took the
//! server from 49 to 131 GB of resident set, where the kernel OOM-killed it.
//!
//! Pinned by the ROW BUDGET, which counts what a statement holds, so the
//! assertion is exact rather than a memory reading: 50,000 candidates under a
//! 5,000-row budget run when the WHERE is tested as rows finish, and the same
//! statement on the old order of work (`set_match_start_chunk(0)`) is
//! refused — the arm that makes the passing one mean something.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, QueryResult, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const N: i64 = 50_000;
const CHUNKED: &str = "interp.matcher carried its starts in chunks";
const NOTHING: &str = "MATCH (m:M) WHERE m.id >= 1000000 DETACH DELETE m";

fn run(g: &Graph, q: &str) -> Result<QueryResult, String> {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).map_err(|e| format!("{e:?}"))
}

fn count(g: &Graph) -> i64 {
    let r = run(g, "MATCH (m:M) RETURN count(m) AS c").expect("count");
    match r.rows[0][0] {
        Value::Int(n) => n,
        ref v => panic!("count answered {v:?}"),
    }
}

#[test]
fn a_where_that_keeps_nothing_holds_nothing_under_a_row_budget() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:M {{id: i}})", N - 1),
    )
    .expect("load");
    g.set_row_budget(Some(5_000));

    // Tested as rows finish: nothing is held, so nothing is refused — and the
    // 50,000 starts were carried through in chunks, not bound at once.
    let (r, trace) = engram_observe::with_trace(|| run(&g, NOTHING));
    r.expect("a delete of nothing runs under a 5,000-row budget");
    assert_eq!(
        trace.counters().get(CHUNKED).copied().unwrap_or(0),
        (N as u64).div_ceil(4096),
        "the starts were carried 4,096 at a time"
    );
    assert_eq!(count(&g), N, "nothing was deleted");

    // Some kept: exactly those are deleted, and they are all it held.
    run(&g, "MATCH (m:M) WHERE m.id >= 49000 DETACH DELETE m")
        .expect("1,000 kept rows fit a 5,000-row budget");
    assert_eq!(count(&g), N - 1_000, "exactly the kept rows were deleted");

    // The old order of work holds every candidate before the WHERE runs.
    g.set_match_start_chunk(0);
    let (r, trace) = engram_observe::with_trace(|| run(&g, NOTHING));
    let err = r.expect_err("49,000 rows held against a 5,000-row budget must refuse");
    assert!(
        err.contains("row budget"),
        "the refusal names the budget: {err}"
    );
    assert_eq!(
        trace.counters().get(CHUNKED).copied().unwrap_or(0),
        0,
        "chunk 0 binds every start at once"
    );
    assert_eq!(count(&g), N - 1_000, "a refused statement deleted nothing");
}
