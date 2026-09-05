#![allow(non_snake_case)]
//! Differential test for the SPARSE-id-set POINT-GATHER fallback extended to the
//! FULL-POPULATION single-MATCH columnar scan (`load_walk_budgeted`, feeding
//! `try_columnar_projection` / `try_columnar_aggregate` / `try_columnar_stage` /
//! `try_columnar_hop_aggregate`) — site 2 of the IC5-class widening. A single-MATCH
//! filter/project over a whole label loads each referenced VALUE column by a RANGE
//! scan over the label's id span; when the label is SPARSE in the id space (a
//! handful of Forum nodes scattered across a much larger population, all carrying
//! the prop) the span blows the 4×members budget and the scan DECLINED to the
//! general per-tuple path. It now falls back to `column_entries_gather`.
//!
//! (Site 1 — the vectorized HOP-FILTER-COUNT `try_vectorized_hop_filter_count` — is
//! unit-tested directly in `src/vectorized.rs`: the later, more general
//! `plan_and_run_columnar` aggregate SHADOWS it for the `count(*)` shape in the
//! `run_single` dispatch, so no end-to-end query reaches it. The unit test calls
//! the operator directly, where its gather-on-decline is observable and canaryable.)
//!
//! THE CONTRACT (every other `pipeline_*.rs`'s): for the SPARSE fixture the operator
//! must FIRE (previously declined) via the gather (`graph.column point-gather` > 0)
//! AND agree byte-for-byte with the general per-tuple path (`set_columnar_scans`
//! false). The DENSE control takes the range path (gather counter 0) and still
//! agrees — proving the fallback is reached ONLY on decline.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the operator ON, then the general path OFF (the oracle).
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(false);
    let off = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(true);
    (on, off)
}

/// A named counter's value after running `src` once with the operator ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// Whether the single-MATCH full-population columnar scan produced the answer —
/// any of the operators `load_walk_budgeted` feeds bumps one of these on a fire.
fn population_scan_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    let c = trace.counters();
    let hit = |k: &str| c.get(k).copied().unwrap_or(0) > 0;
    hit("interp.columnar projection scans")
        || hit("interp.columnar aggregate scans")
        || hit("interp.columnar stages")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// The `f.val` column is loaded over the Forum population's id span. f0 sits low in
/// id space, then 10 filler nodes that ALSO carry `val`, then f1 high — so the
/// 2-Forum population brackets 12 `val` entries, over the 4×2 = 8 budget. The range
/// scan DECLINES and the point-gather (of exactly {f0, f1}) fires.
fn gpop_sparse() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // This file tests the gather MECHANICS on repeated reads; the
    // property-column cache would serve the second read without one.
    g.set_prop_column_budget(0);
    let mk = |label: &str, val: i64| {
        let mut m = BTreeMap::new();
        m.insert("val".to_string(), Value::Int(val));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let _f0 = mk("Forum", 200);
    for k in 0..10 {
        let _ = mk("Filler", 1000 + k); // fillers between the two forum node ids
    }
    let _f1 = mk("Forum", 900);
    g
}

/// The DENSE twin: two Forums consecutive in id space, no fillers — the `val`
/// column span holds 2 entries, under budget, so the range path is taken.
fn gpop_dense() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // This file tests the gather MECHANICS on repeated reads; the
    // property-column cache would serve the second read without one.
    g.set_prop_column_budget(0);
    let mk = |label: &str, val: i64| {
        let mut m = BTreeMap::new();
        m.insert("val".to_string(), Value::Int(val));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let _f0 = mk("Forum", 200);
    let _f1 = mk("Forum", 900);
    g
}

// A projection with ORDER BY but NO LIMIT: `try_columnar_projection` (dispatched
// before `plan_and_run_columnar`) owns it, and `plan_and_run_columnar`'s
// `recognise_core` declines an ORDER BY without a LIMIT — so if the projection
// declines (e.g. the gather is neutralized) the query falls to the GENERAL path,
// not to another gathering operator. That makes the site-2 canary observable.
const POP_SRC: &str = "MATCH (f:Forum) WHERE f.val > 100 RETURN f.val AS v ORDER BY v";

/// SPARSE full-population scan: the `f.val` value-column range scan over the sparse
/// Forum label DECLINES; the point-gather loads exactly the 2 forum ids. The scan
/// FIRES and the projection is byte-identical to the general path.
#[test]
fn population_scan_sparse_gathers_and_fires() {
    let g = gpop_sparse();
    g.set_columnar_column_budget_factor(1); // force the sparse range-scan decline
    let (on, off) = both(&g, POP_SRC);
    assert_eq!(on, off, "sparse full-population scan vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(200)], vec![i(900)]],
        "sparse full-population scan exact rows + order"
    );
    assert!(
        population_scan_fired(&g, POP_SRC),
        "the sparse full-population scan must FIRE via the point-gather"
    );
    assert!(
        counter(&g, POP_SRC, "graph.column point-gather") > 0,
        "the sparse label value column must fall back to the point-gather"
    );
}

/// The PRODUCTION shape: the fillers between the two Forums do NOT carry `val`.
/// Before the visit budget this span was walked in full — 5,000 rows fetched and
/// decoded to find the 2 that carried the property — and never declined, because
/// the budget counted hits. Now the walk stops at 8×2 rows visited and the
/// point-gather answers from exactly the 2 members. Enough fillers, wide enough,
/// to span many blocks once paged (the paged twin below counts them).
fn gpop_interleaved_bare() -> (Graph, engram_store::Store) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // This file tests the gather MECHANICS on repeated reads; the
    // property-column cache would serve the second read without one.
    g.set_prop_column_budget(0);
    let mk = |label: &str, key: &str, val: Value| {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), val);
        g.create_node(&[label.into()], &m).expect("node")
    };
    let _f0 = mk("Forum", "val", Value::Int(200));
    for k in 0..5_000 {
        let _ = mk(
            "Filler",
            "other",
            Value::Str(format!("filler-{k}-{}", "x".repeat(48))),
        );
    }
    let _f1 = mk("Forum", "val", Value::Int(900));
    let store = g.shared_store();
    (g, store)
}

/// INTERLEAVED, non-carrying fillers (resident), span FAR wider than the
/// budget (5,002 ids for 2 members at factor 8 = 16): the walk is not even
/// started — the span is known from the member ids — and the gather answers
/// directly; rows are byte-identical to the general path. Before the
/// pre-check the walk visited `factor × |members|` rows per column on every
/// call and THEN declined: the production ManagedRepo list walked ~2.3k rows
/// of a 5M-id span per query to gather 143 records.
#[test]
fn population_scan_interleaved_bare_skips_the_walk_and_gathers() {
    let (g, _store) = gpop_interleaved_bare();
    let (on, off) = both(&g, POP_SRC);
    assert_eq!(on, off, "interleaved full-population scan vs general disagree");
    assert_eq!(on, vec![vec![i(200)], vec![i(900)]], "exact rows + order");
    assert!(population_scan_fired(&g, POP_SRC), "the scan must FIRE via the gather");
    assert!(
        counter(&g, POP_SRC, "graph.column point-gather") > 0,
        "a span of 5,002 rows for 2 members must gather"
    );
    assert!(
        counter(&g, POP_SRC, "interp.columnar column read skipped the span walk for a sparse label") > 0,
        "and the walk must be SKIPPED, not started and abandoned"
    );
    assert_eq!(
        counter(&g, POP_SRC, "store.column scan declined on rows visited"),
        0,
        "no row of the span was visited for nothing"
    );
}

/// A PRESENCE read (`IS NOT NULL`) over the sparse population gathers too —
/// it used to take the whole columnar stage to the general path, which
/// materialised every member in full (6.75 GB per execution of the
/// production NewsArticle enrichment count). Rows byte-identical to the
/// general path, the presence gather fires, and no node is decoded in full.
#[test]
fn population_presence_read_over_a_sparse_label_gathers() {
    const SRC: &str = "MATCH (f:Forum) WHERE f.val IS NOT NULL RETURN count(f) AS n";
    let (g, _store) = gpop_interleaved_bare();
    let (on, off) = both(&g, SRC);
    assert_eq!(on, off, "presence count columnar vs general disagree");
    assert_eq!(on, vec![vec![i(2)]], "both forums carry val");
    assert!(population_scan_fired(&g, SRC), "the columnar stage must FIRE");
    assert!(
        counter(&g, SRC, "graph.column presence point-gather") > 0,
        "the presence column must be gathered, not declined to the general path"
    );
    assert_eq!(
        counter(&g, SRC, "graph.nodes materialised in full"),
        0,
        "no forum or filler may be decoded in full for a presence count"
    );
    // And inside eight budgets, where the scan is attempted and declines on
    // rows visited: the gather still answers instead of the general path.
    g.set_columnar_column_budget_factor(500);
    let (on2, off2) = both(&g, SRC);
    assert_eq!(on2, off2);
    assert_eq!(on2, vec![vec![i(2)]]);
    assert!(counter(&g, SRC, "graph.column presence point-gather") > 0);
}

/// The same population with the span INSIDE eight budgets (factor 500 →
/// budget 1,000 < 5,002 < 8,000): the walk starts, stops on rows VISITED at
/// the budget, and the gather answers — the v82 mechanism, still there for
/// spans the pre-check cannot rule out.
#[test]
fn population_scan_interleaved_bare_declines_on_rows_visited_and_gathers() {
    let (g, _store) = gpop_interleaved_bare();
    g.set_columnar_column_budget_factor(500);
    let (on, off) = both(&g, POP_SRC);
    assert_eq!(on, off, "interleaved full-population scan vs general disagree");
    assert_eq!(on, vec![vec![i(200)], vec![i(900)]], "exact rows + order");
    assert!(population_scan_fired(&g, POP_SRC), "the scan must FIRE via the gather");
    assert!(
        counter(&g, POP_SRC, "graph.column point-gather") > 0,
        "a span of 5,002 rows for a budget of 1,000 must decline the walk and gather"
    );
    assert!(
        counter(&g, POP_SRC, "store.column scan declined on rows visited") > 0,
        "and the decline must be the VISIT budget, not the hit budget"
    );
    assert_eq!(
        counter(&g, POP_SRC, "interp.columnar column read skipped the span walk for a sparse label"),
        0,
        "inside eight budgets the pre-check stays out of it"
    );
}

/// The same population on a PAGED store — the backing the production mirror
/// serves from, where a walked row is a block fetched, verified and decoded.
/// Same rows, gather fires, and the walk stops early: the query fetches a
/// fraction of the blocks a forced whole-span walk fetches.
#[test]
fn population_scan_interleaved_bare_on_a_paged_store_stops_fetching_at_the_budget() {
    let (resident, store) = gpop_interleaved_bare();
    let want = rows(&resident, POP_SRC, BTreeMap::new());
    drop(resident);
    let dir = std::env::temp_dir().join(format!(
        "engram_sparse_gather_paged_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A cache smaller than the span, so blocks a walk crosses are preads.
    let _cache = store.into_paged(&dir, 16 * 1024).expect("into_paged");
    let g = Graph::new(store.clone(), Realm(1), Namespace(1));
    g.set_prop_column_budget(0);

    let preads_of = |factor: usize| -> (Vec<Vec<Value>>, u64) {
        g.set_columnar_column_budget_factor(factor);
        g.set_columnar_scans(true);
        let (got, trace) = engram_observe::with_trace(|| rows(&g, POP_SRC, BTreeMap::new()));
        (
            got,
            trace.counters().get("paged.pread").copied().unwrap_or(0),
        )
    };
    // The default budget (8 × 2 members): declines, gathers, few blocks.
    let (got, few) = preads_of(8);
    assert_eq!(got, want, "paged interleaved scan vs resident disagree");
    assert!(
        counter(&g, POP_SRC, "graph.column point-gather") > 0,
        "the paged span must decline to the gather"
    );
    // A budget wide enough to admit the whole span: the walk runs to the end
    // and crosses every block — the cost this fix removes.
    let (walked, many) = preads_of(1 << 20);
    assert_eq!(walked, want, "the whole-span walk still agrees");
    assert!(many >= 8, "fixture: the span must cover many blocks ({many})");
    assert!(
        few * 4 < many,
        "the budgeted read must fetch a fraction of the span: {few} preads vs {many}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// TWO properties over the interleaved population: the declined columns are
/// gathered TOGETHER — one record read per member — not one gather per
/// property. On the paged production mirror a 143-node label read four
/// properties as four gathers (572 point reads for 143 records).
#[test]
fn population_scan_gathers_every_declined_column_in_one_record_pass() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // This file tests the gather MECHANICS on repeated reads; the
    // property-column cache would serve the second read without one.
    g.set_prop_column_budget(0);
    let forum = |val: i64, name: &str| {
        let mut m = BTreeMap::new();
        m.insert("val".to_string(), Value::Int(val));
        m.insert("name".to_string(), Value::Str(name.to_string()));
        g.create_node(&["Forum".into()], &m).expect("node")
    };
    let _f0 = forum(200, "alpha");
    for k in 0..500 {
        let mut m = BTreeMap::new();
        m.insert("other".to_string(), Value::Str(format!("filler-{k}")));
        g.create_node(&["Filler".into()], &m).expect("node");
    }
    let _f1 = forum(900, "beta");
    const SRC: &str = "MATCH (f:Forum) WHERE f.val > 100 RETURN f.val AS v, f.name AS name ORDER BY v";
    let (on, off) = both(&g, SRC);
    assert_eq!(on, off, "two-column interleaved scan vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![i(200), Value::Str("alpha".into())],
            vec![i(900), Value::Str("beta".into())]
        ]
    );
    assert!(population_scan_fired(&g, SRC), "the scan must FIRE via the gather");
    // A filtered projection is TWO-PHASE — the predicate's column over the
    // population, then the items' columns over the survivors — so the bound
    // is one gather per phase, never one per column (three, before).
    let gathers = counter(&g, SRC, "graph.column record-gather");
    assert!(
        (1..=2).contains(&gathers),
        "the declined columns must be gathered per PHASE, not per column: {gathers} gathers"
    );
}

/// DENSE control: contiguous Forums → the range scan FITS its budget and the
/// point-gather is NOT invoked, yet the projection still fires and agrees.
#[test]
fn population_scan_dense_uses_range_scan_not_gather() {
    let g = gpop_dense();
    let (on, off) = both(&g, POP_SRC);
    assert_eq!(on, off, "dense full-population scan vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(200)], vec![i(900)]],
        "dense full-population scan exact rows + order"
    );
    assert!(
        population_scan_fired(&g, POP_SRC),
        "the dense full-population scan must still fire"
    );
    assert_eq!(
        counter(&g, POP_SRC, "graph.column point-gather"),
        0,
        "the dense label value column must use the range scan, not the gather"
    );
}
