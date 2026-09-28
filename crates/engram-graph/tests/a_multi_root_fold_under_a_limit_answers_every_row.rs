//! A count fold with MORE THAN ONE ROOT, under a LIMIT, must answer the same
//! rows as the general path.
//!
//! `FoldPlan::reached` is ONE probe-cap accumulator shared by every root, and
//! `fold_rows` charges each root's CUMULATIVE weight into it while checking it
//! before every driving row. Roots' weights MULTIPLY to make the answer
//! (`FoldPlan::root` says so), so a single root's partial is not the count —
//! but the cap is judged against it anyway. Once the first root's partial
//! crosses the cap, the second root's pass breaks on its first row with
//! nothing kept, the chunk's selection is empty, and
//! `constant_projection_over_count` reads that as zero: a query whose true
//! answer is five rows returns none, with no error.
//!
//! Fix 90 did not create this — it makes it far easier to reach. The
//! multiplier is applied at `multiplier_root`, the LOWEST-indexed root, so the
//! first root's partial is inflated by |S|! and crosses the cap |S|! times
//! sooner, on exactly the symmetric shapes the recogniser targets.
//!
//! Found by an adversarial review of fix 90 and reproduced here before being
//! fixed. The three arms are asked at many LIMITs because the defect is a
//! THRESHOLD: it appears only where the cap falls between the first root's
//! partial and the true count, so a single k can pass while its neighbours
//! fail.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, pipeline, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, label: &str, key: i64) -> u64 {
    let mut p = BTreeMap::new();
    p.insert("k".to_string(), Value::Int(key));
    g.create_node(&[label.into()], &p).expect("node")
}

fn rel(g: &Graph, a: u64, t: &str, b: u64) {
    g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
}

fn rows(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e}"))
        .rows
        .len()
}

fn count(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e}"));
    match r.rows.first().and_then(|x| x.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected int, got {other:?}"),
    }
}

/// Three persons, three tags. `a` reaches three tags and `b` one, and `a` and
/// `b` are joined by TWO parallel `K` edges — so the pattern has 3 * 1 * 2 * 2
/// = 12 matches (the `K` hop is undirected, so each parallel edge matches from
/// either side).
fn corpus() -> Graph {
    let g = graph();
    let x1 = node(&g, "X", 1);
    let x2 = node(&g, "X", 2);
    let x3 = node(&g, "X", 3);
    let a = node(&g, "P", 10);
    let b = node(&g, "P", 11);
    let _c = node(&g, "P", 12);
    rel(&g, a, "T", x1);
    rel(&g, a, "T", x2);
    rel(&g, a, "T", x3);
    rel(&g, b, "T", x3);
    rel(&g, a, "K", b);
    rel(&g, a, "K", b);
    g
}

/// Two independent `T` roots joined by a `K` hop — `p1` and `p2` are
/// interchangeable, so fix 90 plans a symmetry over them, and the plan has TWO
/// fold roots.
const Q: &str = "MATCH (p1:P)-[:T]->(y1:X) MATCH (p2:P)-[:T]->(y2:X) \
                 MATCH (p1)-[:K]-(p2) RETURN 1 AS c LIMIT ";

#[test]
fn a_multi_root_fold_under_a_limit_answers_the_same_rows_as_the_general_path() {
    let g = corpus();

    // The uncapped count first: if the multiplier arithmetic itself were
    // wrong, every LIMIT below would be wrong for a different reason and the
    // finding would be misattributed.
    let total = "MATCH (p1:P)-[:T]->(y1:X) MATCH (p2:P)-[:T]->(y2:X) \
                 MATCH (p1)-[:K]-(p2) RETURN count(*) AS c";
    pipeline::set_count_fold(false);
    let general_total = count(&g, total);
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(false);
    let off_total = count(&g, total);
    pipeline::set_fold_symmetry_breaking(true);
    let on_total = count(&g, total);
    assert_eq!(
        (general_total, off_total, on_total),
        (12, 12, 12),
        "the UNCAPPED count must agree three ways before any LIMIT is judged — \
         if it does not, the defect below is in the multiplier, not the cap"
    );

    let mut bad = Vec::new();
    for k in [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13] {
        let src = format!("{Q}{k}");
        pipeline::set_count_fold(false);
        let general = rows(&g, &src);
        pipeline::set_count_fold(true);
        pipeline::set_fold_symmetry_breaking(false);
        let off = rows(&g, &src);
        pipeline::set_fold_symmetry_breaking(true);
        let on = rows(&g, &src);
        // The general path is the reference: LIMIT k over 12 matches is
        // min(k, 12) rows, and nothing about a fold may change that.
        let want = k.min(12);
        if general != want || off != want || on != want {
            bad.push(format!(
                "LIMIT {k}: want {want}, general {general}, fold(sym off) {off}, fold(sym on) {on}"
            ));
        }
    }
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(true);
    assert!(
        bad.is_empty(),
        "a multi-root count fold under a LIMIT dropped rows — the shared probe-cap \
         accumulator is charged with one root's partial product, which is not the \
         count:\n  {}",
        bad.join("\n  ")
    );
}
