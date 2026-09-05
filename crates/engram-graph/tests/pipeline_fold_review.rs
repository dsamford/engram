#![allow(non_snake_case)]
//! ADVERSARIAL review tests for the COUNT FOLD (S1) and the bound-endpoint
//! EDGE PREDICATE / ANTI-JOIN (S2) — written to REFUTE ON == OFF against the
//! interpreter on the shapes the builder's suites did not pin:
//!   - `Dir::Both` self-loops (single AND parallel) under a FOLDED CLOSE, the
//!     chunk FILTER and the INLINE probe, including a close onto the hop's own
//!     source var and a close from a folded var onto a materialised var of an
//!     EARLIER path;
//!   - the memo on chains whose types are disjoint except through a MULTI-TYPE
//!     hop, and an inline `<>`/`=` against a NON-immediate folded ancestor;
//!   - zero-count rows under a CONST group key and Form A;
//!   - overflow END-TO-END: a fan-out ladder whose walk count leaves `u64`
//!     (declines in `fold_tail`) or `i64` (declines in the reducer) must reach
//!     the general path — which REFUSES on the row budget — never a wrapped or
//!     saturated number; the largest fitting count is exact;
//!   - the `sorted_by_peer` canary: with every table's flag cleared the fold
//!     and the counted close must WALK and answer the same;
//!   - the edge predicate through the MULTISTAGE (WITH) runner, on a single
//!     hop under `count(*)` (the census seam), and over `into_paged`.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn run(g: &Graph, src: &str) -> Result<Rows, String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn rows(g: &Graph, src: &str) -> Rows {
    run(g, src).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

/// fold ON / fold OFF / columnar OFF.
fn triple(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let on = rows(g, src);
    engram_graph::pipeline::set_count_fold(false);
    let off = rows(g, src);
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(false);
    let general = rows(g, src);
    g.set_columnar_scans(true);
    (on, off, general)
}

fn counter(g: &Graph, src: &str, key: &str) -> Option<u64> {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied()
}

const FOLD: &str = "interp.pipeline count fold";
const MEMO: &str = "interp.pipeline count fold memo";
const INLINE: &str = "interp.pipeline edge pred inline";
const FILTER: &str = "interp.pipeline edge pred filter";
const CLOSE: &str = "interp.pipeline semijoin counted close";

/// A counter read with the COUNT-ONLY JOIN REORDER (operator C) held OFF.
///
/// A statement that the fold DECLINES is a claim about the pattern AS WRITTEN.
/// Operator C may rewrite a `count(*)`-only pattern into an equivalent one that
/// DOES fold — that is its entire purpose (`pipeline_count_reorder.rs` pins it,
/// and the rewritten plan is admitted only when it materialises strictly fewer
/// columns), so the fold planner's own boundary is read in source order. The
/// ROW agreement above is still taken with every default on.
fn counter_in_source_order(g: &Graph, src: &str, key: &str) -> Option<u64> {
    engram_graph::pipeline::set_count_only_reorder(false);
    let c = counter(g, src, key);
    engram_graph::pipeline::set_count_only_reorder(true);
    c
}

fn agrees(g: &Graph, src: &str) -> Rows {
    let (on, off, general) = triple(g, src);
    assert_eq!(on, general, "fold ON vs general: `{src}`");
    assert_eq!(off, general, "fold OFF vs general: `{src}`");
    on
}

fn agrees_and_folds(g: &Graph, src: &str) -> Rows {
    let on = agrees(g, src);
    assert_eq!(counter(g, src, FOLD), Some(1), "fold must fire: `{src}`");
    on
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── Fixtures ────────────────────────────────────────────────────────────────

/// P{pk} with every self-loop / parallel hazard at once:
///   K: p0->p0 TWICE (parallel self-loops), p0->p1 TWICE, p1->p2, p2->p1,
///      p2->p3, p3->p4, p4->p0, p3->p3 (a single self-loop)
///   M: p0->p2, p2->p0, p1->p1 (self-loop)
///   HI: p1->t0, p3->t0, p3->t1, p0->t1
fn gself() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let p: Vec<u64> = (0..5).map(|i| mk("P", "pk", i)).collect();
    let t: Vec<u64> = (0..2).map(|i| mk("Tag", "tk", i)).collect();
    let e = BTreeMap::new();
    for (s, d) in [
        (0, 0),
        (0, 0),
        (0, 1),
        (0, 1),
        (1, 2),
        (2, 1),
        (2, 3),
        (3, 4),
        (4, 0),
        (3, 3),
    ] {
        g.create_rel(p[s], "K", p[d], &e).expect("K");
    }
    for (s, d) in [(0, 2), (2, 0), (1, 1)] {
        g.create_rel(p[s], "M", p[d], &e).expect("M");
    }
    for (s, d) in [(1, 0), (3, 0), (3, 1), (0, 1)] {
        g.create_rel(p[s], "HI", t[d], &e).expect("HI");
    }
    g
}

/// A{ak}-R->B{bk}-S->C{ck}-U->B: the third hop lands BACK on B so `b <> d`
/// and closes onto an earlier-path B are non-vacuous.
///   R: a0->b0,b1  a1->b1,b2  a2->b3
///   S: b0->c0,c1  b1->c1     b2->c2  b3->(none)
///   U: c0->b0,b1  c1->b1     c2->b3,b0
///   T: a0->c1, a1->c1, a2->c2 (anti-join targets)
fn gdeep() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let a: Vec<u64> = (0..3).map(|i| mk("A", "ak", i)).collect();
    let b: Vec<u64> = (0..4).map(|i| mk("B", "bk", i)).collect();
    let c: Vec<u64> = (0..3).map(|i| mk("C", "ck", i)).collect();
    let e = BTreeMap::new();
    for (s, d) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 3)] {
        g.create_rel(a[s], "R", b[d], &e).expect("R");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 1), (2, 2)] {
        g.create_rel(b[s], "S", c[d], &e).expect("S");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 1), (2, 3), (2, 0)] {
        g.create_rel(c[s], "U", b[d], &e).expect("U");
    }
    for (s, d) in [(0, 1), (1, 1), (2, 2)] {
        g.create_rel(a[s], "T", c[d], &e).expect("T");
    }
    g
}

/// The overflow LADDER: `levels` ranks of `width` nodes, every node of rank k
/// linked to every node of rank k+1 by type `E<k>` (pairwise-distinct types,
/// so every level memoises). Walks from one rank-0 node = width^(levels-1).
fn gladder(width: usize, levels: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let e = BTreeMap::new();
    let mut ranks: Vec<Vec<u64>> = Vec::new();
    for k in 0..levels {
        let mut ids = Vec::new();
        for j in 0..width {
            let mut p = BTreeMap::new();
            p.insert("k".to_string(), Value::Int(j as i64));
            ids.push(
                g.create_node(&[format!("L{k}")], &p)
                    .expect("node"),
            );
        }
        ranks.push(ids);
    }
    for k in 0..levels - 1 {
        let ty = format!("E{k}");
        for &s in &ranks[k] {
            for &d in &ranks[k + 1] {
                g.create_rel(s, &ty, d, &e).expect("E");
            }
        }
    }
    g
}

fn ladder_query(levels: usize, keyed: bool) -> String {
    let mut s = String::from("MATCH (x0:L0)");
    for k in 1..levels {
        s.push_str(&format!("-[:E{}]->(x{k}:L{k})", k - 1));
    }
    if keyed {
        s.push_str(" RETURN x0.k AS k, count(*) AS n ORDER BY k");
    } else {
        s.push_str(" RETURN count(*) AS n");
    }
    s
}

// ─── Dir::Both self-loops: folded close / filter / inline ───────────────────

#[test]
fn both_self_loops_single_and_parallel_under_fold_filter_and_inline() {
    let g = gself();
    let folds: &[&str] = &[
        // A folded UNTRACKED close over Both onto the hop's own var: the
        // parallel self-loops on p0 count once each, never twice (O then I).
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // A TRACKED close onto the hop's own source var (`used` excludes the
        // self-loop the walk arrived over).
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // Closed onto the seed with parallel self-loops: a=p0 reaches b=p0 over
        // either self-loop and closes over the OTHER.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(a) RETURN count(*) AS n",
        // Inline SELF-EDGE probes at the folded level, both polarities.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (c)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (c)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (c)-[:K]->(c) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // Inline Both probe against the seed where a == c happens via loops.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (a)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)--(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:M]-(c) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // EqBound: closed walks by identity, and b = c only over a self-loop.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE a = c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE b = c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT b = c RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // The seed filter beside the inline conjunct; an ANONYMOUS middle.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE a.pk > 0 AND a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-()-[:K]-(c:P) WHERE a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-()-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) RETURN count(*) AS n",
        // Multi-type overlap with a single-type hop: the memo must stay OFF
        // and the rel-iso through `[:K|M]` then `[:K]` must hold.
        "MATCH (a:P)-[:K|M]-(b:P)-[:K]-(c:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K|M]-(c:P) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K|M]-(b:P)-[:M|K]-(c:P)-[:K]-(a) RETURN count(*) AS n",
        // K, M, K: the outer hops share a type across a disjoint middle.
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K]-(d:P) WHERE a <> d RETURN count(*) AS n",
        // Undirected then directed then HI, q6/q9-shaped with Both loops.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) WHERE a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) WHERE NOT (a)-[:K]-(c) AND a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:M]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) WHERE NOT (a)-[:K]-(c) RETURN count(*) AS n",
        // Two roots off the seed, one of them closing onto the seed.
        "MATCH (a:P)-[:K]-(b:P), (a)-[:M]-(c:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    ];
    for src in folds {
        agrees_and_folds(&g, src);
    }
    // The memo-ON == memo-OFF arm over the same statements.
    for src in folds {
        engram_graph::pipeline::set_count_fold_memo(false);
        let no_memo = run(&g, src);
        engram_graph::pipeline::set_count_fold_memo(true);
        let on = run(&g, src);
        assert_eq!(on, no_memo, "memo ON vs OFF: `{src}`");
    }
    // Hand numbers. `(b)-[:K]-(b)` untracked close: p0 has two self-loops (2),
    // p3 one (1). Undirected K-neighbours (with Both dedup — an undirected hop
    // over a self-loop binds ONCE, so p0's two loops are two rows, not four):
    // p0: {p0,p0,p1,p1,p4} = 5 rows (b=p0 twice, b=p1 twice, b=p4); p1:
    // {p0,p0,p2,p2} = 4; p2: {p1,p1,p3} = 3; p3: {p2,p4,p3} = 3; p4: {p3,p0}
    // = 2. Per row the close multiplies by loops(b): a=p0: 2·2 + 2·0 + 0 = 4;
    // a=p1: 2·2 + 2·0 = 4; a=p2: b=p3 → 1; a=p3: b=p3 → 1; a=p4: b=p3 → 1 AND
    // b=p0 → 2, so 3. Total 13.
    //
    // The close is its own PATH (the comma), so it is `reset`: rel-iso does not
    // reach across it, and the arriving hop's rel is not excluded from the
    // close's — which is why b=p0 contributes 2 per row and not 1. The tracked
    // spelling below (`-[:K]-(b)-[:K]-(b)`, one path) is the case where the
    // arriving self-loop IS excluded.
    let on = agrees_and_folds(
        &g,
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(
        on,
        vec![
            vec![i(0), i(4)],
            vec![i(1), i(4)],
            vec![i(2), i(1)],
            vec![i(3), i(1)],
            vec![i(4), i(3)]
        ],
        "untracked Both self-loop close"
    );
    // The same fixture enumerated, so the count above is pinned to the ROWS it
    // sums and not only to itself: a=p4 has b=p0 twice (p0's two self-loops)
    // and b=p3 once — the row the count's hand-arithmetic must not drop.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) WHERE a.pk = 4 RETURN b.pk AS j ORDER BY j"
        ),
        vec![vec![i(0)], vec![i(0)], vec![i(3)]],
        "a=p4's rows under the untracked self-loop close"
    );
    // The chunk-FILTER form of the Both self-loop: p0 and p3 carry K loops, so
    // the surviving `(a)-[:K]->(b)` edges are (p0,p0)×2, (p2,p3), (p3,p3) and
    // (p4,p0). The filter is a PIPELINE operator, so the statement has to be an
    // AGGREGATE — a plain projection is owned by another runner and fires no
    // `interp.pipeline` counter at all (asserted below). Both `a` and `b` are
    // read here (they are the group keys), so nothing folds and the conjunct
    // stays on the chunk filter rather than being inlined into a level.
    let src = "MATCH (a:P)-[:K]->(b:P) WHERE (b)-[:K]-(b) RETURN a.pk AS ak, b.pk AS bk, count(*) AS n ORDER BY ak, bk";
    let on = agrees(&g, src);
    assert_eq!(counter(&g, src, FILTER), Some(1), "filter form fires");
    assert_eq!(counter(&g, src, FOLD), None, "both group keys are read: no fold");
    assert_eq!(
        on,
        vec![
            vec![i(0), i(0), i(2)],
            vec![i(2), i(3), i(1)],
            vec![i(3), i(3), i(1)],
            vec![i(4), i(0), i(1)]
        ],
        "self-loop filter rows"
    );
    // The same shape as a plain projection, one row per walk — and NOT the
    // pipeline's: no chunk filter fires, which is why the assertion above is on
    // the aggregate spelling.
    let src = "MATCH (a:P)-[:K]->(b:P) WHERE (b)-[:K]-(b) RETURN a.pk AS ak, b.pk AS bk ORDER BY ak, bk";
    assert_eq!(counter(&g, src, FILTER), None, "a plain projection is not the pipeline's");
    assert_eq!(
        agrees(&g, src),
        vec![
            vec![i(0), i(0)],
            vec![i(0), i(0)],
            vec![i(2), i(3)],
            vec![i(3), i(3)],
            vec![i(4), i(0)]
        ],
        "self-loop filter rows, enumerated"
    );
    // The DIRECTED tracked close onto the hop's own var: a-K->b then b-K->b
    // must not reuse the arriving self-loop — but it must not drop a loop it
    // never arrived over either. a=p0 → b=p0 over loop#1, close over loop#2
    // (and vice versa): 2; b=p1 (×2): 0. a=p2 → b=p3, close over p3's loop: 1.
    // a=p3 → b=p3 over p3's ONLY loop, which the close may not reuse: 0.
    // a=p3 → b=p4: 0. a=p4 → b=p0 over the (p4,p0) edge, which is not a loop,
    // so BOTH of p0's loops close: 2. a=p1 → b=p2: 0.
    let on = agrees_and_folds(
        &g,
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(
        on,
        vec![vec![i(0), i(2)], vec![i(2), i(1)], vec![i(4), i(2)]],
        "tracked own-var close"
    );
}

// ─── Memo with a non-immediate folded ancestor read ─────────────────────────

#[test]
fn memo_with_inline_pred_against_a_grandparent_level() {
    let g = gdeep();
    // `b <> d` is inlined at d's level; b is two levels up. c's level reads b
    // (not memoised), b's level is a pure function of b (memoised, types
    // R/S/U pairwise disjoint): memo ON == OFF == general.
    let src = "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b <> d RETURN count(*) AS n";
    let on = agrees_and_folds(&g, src);
    engram_graph::pipeline::set_count_fold_memo(false);
    let no_memo = rows(&g, src);
    engram_graph::pipeline::set_count_fold_memo(true);
    assert_eq!(on, no_memo);
    // By hand: walks (a,b,c,d) with d != b: a0: b0→c0→{b0✗,b1✓}=1, b0→c1→{b1}=1,
    // b1→c1→{b1✗}=0 → 2; a1: b1→c1→0, b2→c2→{b3,b0}=2 → 2; a2: b3→ none → 0.
    assert_eq!(on, vec![vec![i(4)]]);
    // The same with an edge probe against the grandparent (`(a)-[:T]->(c)`
    // with `a` the seed) beside it, and `b = d` (the round trip).
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b <> d AND NOT (a)-[:T]->(c) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b = d RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(b) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        // A close from the folded level onto a var of an EARLIER path. In the
        // first spelling `d` is the group key, so the READ SET materialises it;
        // in the second nothing reads it, and `plan_count_fold` un-folds the
        // close's TARGET (never the level) so the two spellings reach the SAME
        // plan. Both must therefore fold — the second is what the un-fold rule
        // in `plan_count_fold` buys.
        "MATCH (a:A)-[:R]->(d:B), (a)-[:R]->(b:B)-[:S]->(c:C), (c)-[:U]->(d) RETURN d.bk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(d:B), (a)-[:R]->(b:B)-[:S]->(c:C), (c)-[:U]->(d) RETURN count(*) AS n",
        // Two group keys, a tie, the key bound AFTER the root.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) RETURN d.bk AS k, a.ak AS j, count(*) AS n ORDER BY n DESC LIMIT 2",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) RETURN d.bk AS k, a.ak AS j, count(*) AS n ORDER BY n DESC, k, j",
    ] {
        agrees_and_folds(&g, src);
    }
    // `b = d` round trips: a0: b0→c0→b0 (1), b1→c1→b1 (1) → 2; a1: b1→c1→b1
    // (1), b2→c2→{b3,b0} (0) → 1; a2: 0.
    let on = agrees_and_folds(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(b) RETURN a.ak AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(on, vec![vec![i(0), i(2)], vec![i(1), i(1)]]);
    // The BOUNDARY of the un-fold-the-target rule, so "it folds" is not read as
    // "it always folds": spell the same three patterns with `d` bound AFTER the
    // fold's root and materialising `d` cannot satisfy the position rule
    // (`d`'s index is above the root's end var) — the level un-folds and the
    // statement DECLINES, still agreeing with the interpreter on the number.
    let boundary =
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B), (c)-[:U]->(d) RETURN count(*) AS n";
    let declined = agrees(&g, boundary);
    assert_eq!(
        counter_in_source_order(&g, boundary, FOLD),
        None,
        "the position rule still bites"
    );
    assert_eq!(
        declined,
        rows(
            &g,
            "MATCH (a:A)-[:R]->(d:B), (a)-[:R]->(b:B)-[:S]->(c:C), (c)-[:U]->(d) RETURN count(*) AS n"
        ),
        "the folded and declined spellings count the same walks"
    );
}

// ─── Zero-count rows under a CONST key / Form A ─────────────────────────────

#[test]
fn zero_count_rows_vanish_under_const_key_and_with_form() {
    let g = gdeep();
    // b3 has no S edge, and no C has a U edge to a C: every walk folds to 0.
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) RETURN 1 AS k, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) WITH 1 AS k, count(*) AS n RETURN k, n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) WITH a, count(*) AS n RETURN a.ak AS k, n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) RETURN count(*) AS n, 'x' AS s",
        // Only a2's walk (b3) folds to zero: the const-keyed group keeps the
        // other seeds' weight.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN 1 AS k, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a.ak AS k, count(*) AS n WHERE n >= 1 RETURN k, n ORDER BY k",
    ] {
        agrees_and_folds(&g, src);
    }
    let on = agrees_and_folds(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) RETURN 1 AS k, count(*) AS n",
    );
    assert!(on.is_empty(), "a keyed count over zero rows has no group: {on:?}");
    let on = agrees_and_folds(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:C) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(0)]]);
}

// ─── Overflow end-to-end ────────────────────────────────────────────────────

#[test]
fn overflow_declines_to_the_general_path_and_the_largest_fit_is_exact() {
    // width 16: 16 hops = 16^16 = 2^64 walks per seed (past u64 — `fold_tail`
    // declines); 15 hops = 2^60 per seed (fits; the global sum 2^64 leaves i64
    // — the reducer declines). The budget makes the general path REFUSE
    // instead of enumerating 2^60 rows.
    let g = gladder(16, 17);
    g.set_row_budget(Some(100_000));
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(true);

    // 15 hops, keyed by the seed: 16 groups of exactly 2^60.
    let fit = ladder_query(16, true);
    let (on, trace) = engram_observe::with_trace(|| rows(&g, &fit));
    assert_eq!(on.len(), 16);
    for (j, r) in on.iter().enumerate() {
        assert_eq!(r, &vec![i(j as i64), i(1 << 60)], "row {j}");
    }
    assert_eq!(trace.counters().get(FOLD).copied(), Some(1));
    assert_eq!(trace.counters().get(MEMO).copied(), Some(1));

    // (The 16-hop / global forms overflow u64 / i64 and DECLINE to the general
    // path — which, being a streaming per-tuple count, never materialises a row
    // the budget could refuse and simply never finishes: 2^60 walks. That arm
    // is pinned by the in-crate unit tests on `fold_tail` / `fold_row_weighted`
    // and by the `Ok(None)` plumbing, not runnable end-to-end.)

    // With the fold OFF the same fitting statement also refuses (the budget),
    // which is exactly why the fold exists — and proves the number above came
    // from the fold, not from a materialisation that happened to fit.
    engram_graph::pipeline::set_count_fold(false);
    let res = run(&g, &fit);
    engram_graph::pipeline::set_count_fold(true);
    assert!(res.is_err(), "fold OFF cannot materialise 2^60 rows");
}

// ─── The sorted_by_peer canary ──────────────────────────────────────────────

#[test]
fn cleared_sorted_flags_force_the_walk_and_agree() {
    let g = gself();
    g.set_degree_table_after(0);
    let stmts: &[&str] = &[
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P) WHERE NOT (a)-[:K]->(c) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:M]->(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P), (b)-[:K]->(a) RETURN a.pk AS k, b.pk AS j ORDER BY k, j",
        "MATCH (a:P)-[:K]->(b:P) WHERE NOT (b)-[:K]->(a) RETURN a.pk AS k, b.pk AS j ORDER BY k, j",
        // The MATERIALISED counted close over a Both self-loop: `b` is the group
        // key, so it is read and the fold declines — `DataChunk::semijoin` takes
        // its `edge_count_slim` fast path instead. Without this statement the
        // list has no counted close at all (the fold absorbs every other one),
        // and the CLOSE assertion below would be vacuous.
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
    ];
    // Warm: tables are built on first use.
    let before: Vec<Rows> = stmts.iter().map(|s| rows(&g, s)).collect();
    let (again, trace) = engram_observe::with_trace(|| {
        stmts.iter().map(|s| rows(&g, s)).collect::<Vec<Rows>>()
    });
    assert_eq!(before, again);
    let searched = trace
        .counters()
        .get("graph.edge probe binary search")
        .copied()
        .unwrap_or(0);
    assert!(searched > 0, "the warm run never binary-searched: {:?}", trace.counters());
    let flipped = g.clear_adjacency_sorted_flags();
    assert!(flipped > 0, "the canary cleared no table");
    let (walked, trace) = engram_observe::with_trace(|| {
        stmts.iter().map(|s| rows(&g, s)).collect::<Vec<Rows>>()
    });
    assert_eq!(before, walked, "the walk must answer what the search did");
    assert_eq!(
        trace
            .counters()
            .get("graph.edge probe binary search")
            .copied(),
        None,
        "a cleared flag was not consulted"
    );
    assert!(
        trace
            .counters()
            .get("graph.edge probe walked")
            .copied()
            .unwrap_or(0)
            > 0
    );
    // Four of the seven statements fold: the three `count(*)` chains whose tail
    // var is unread, plus the self-close `(b)-[:K]-(b)` (folded at b's own
    // level — `bind[b]` is the level's node). The two plain-projection
    // statements are not aggregates, and the seventh reads `b`.
    assert_eq!(trace.counters().get(FOLD).copied(), Some(4), "the folds still ran");
    assert!(trace.counters().get(CLOSE).copied().unwrap_or(0) > 0, "the counted close still ran");
    // And the general path, for the record.
    g.set_columnar_scans(false);
    let general: Vec<Rows> = stmts.iter().map(|s| rows(&g, s)).collect();
    g.set_columnar_scans(true);
    assert_eq!(before, general);
}

// ─── The edge predicate on other runners ────────────────────────────────────

#[test]
fn edge_pred_through_multistage_census_seam_and_declines() {
    let g = gdeep();
    // Through the WITH runner: src carried, dst bound in stage 2; and the
    // reverse spelling (src bound LAST).
    for src in [
        "MATCH (a:A)-[:R]->(b:B) WITH a, b MATCH (b)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck",
        "MATCH (a:A)-[:R]->(b:B) WITH a, b MATCH (b)-[:S]->(c:C) WHERE NOT (c)<-[:T]-(a) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck",
        "MATCH (a:A)-[:R]->(b:B) WITH a, b MATCH (b)-[:S]->(c:C) WHERE (c)<-[:T]-(a) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) WITH DISTINCT b MATCH (b)-[:S]->(c:C)-[:U]->(d:B) WHERE NOT (d)-[:S]->(c) RETURN b.bk AS bk, count(*) AS n ORDER BY bk",
        // A single hop under count(*) with the probe (the census seam).
        "MATCH (a:A)-[:R]->(b:B) WHERE NOT (a)-[:R]->(b) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) WHERE (a)-[:R]->(b) RETURN count(*) AS n",
        "MATCH (b:B)-[:S]->(c:C) WHERE NOT (c)-[:U]->(b) RETURN count(*) AS n",
        "MATCH (b:B)-[:S]->(c:C) WHERE NOT (c)-[:U]->(b) RETURN b.bk AS k, count(*) AS n ORDER BY k",
        // The probe inline where the level var is the pattern's FAR end and
        // the arrow flips, both polarities, keyed and global.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (c)<-[:T]-(a) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (c)<-[:T]-(a) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (c)-[:T]->(a) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE NOT (d)<-[:R]-(a) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:R]->(d) AND b <> d RETURN count(*) AS n",
        // OPTIONAL beside a foldable chain: the OPTIONAL plan owns it (no fold)
        // and must still agree.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) OPTIONAL MATCH (a)-[:T]->(x:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) OPTIONAL MATCH (a)-[:T]->(x:C) WHERE NOT (x)-[:U]->(b) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) OPTIONAL MATCH (a)-[:T]->(x:C) OPTIONAL MATCH (c)-[:U]->(y:B) RETURN count(*) AS n",
    ] {
        agrees(&g, src);
    }
    // Hand numbers: (a,b,c) walks: a0: (0,0,0)(0,0,1)(0,1,1); a1: (1,1,1)(1,2,2);
    // a2: none. T: a0->c1, a1->c1, a2->c2. `(c)<-[:T]-(a)` keeps (0,0,1)(0,1,1)
    // (1,1,1) = 3.
    let src = "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (c)<-[:T]-(a) RETURN count(*) AS n";
    assert_eq!(agrees_and_folds(&g, src), vec![vec![i(3)]]);
    assert_eq!(counter(&g, src, INLINE), Some(1));
    // `(a)-[:R]->(d) AND b <> d`: (a,b,c,d): a0: b0→c0→{b0,b1}: d=b1 (R a0->b1 ✓, b<>d ✓) 1;
    // b0→c1→b1 ✓ 1; b1→c1→b1 (b=d ✗) → 2. a1: b1→c1→b1 ✗; b2→c2→{b3 (R a1->b3? no), b0 (no)} → 0.
    assert_eq!(
        agrees_and_folds(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:R]->(d) AND b <> d RETURN count(*) AS n"
        ),
        vec![vec![i(2)]]
    );
}

// ─── Paged ───────────────────────────────────────────────────────────────────

#[test]
fn fold_and_anti_join_over_the_paged_store_equal_resident() {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = gself();
    let stmts: &[&str] = &[
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) AND a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:HI]->(t:Tag) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P) WHERE NOT (b)-[:K]-(a) RETURN a.pk AS k, b.pk AS j ORDER BY k, j",
    ];
    let resident: Vec<Rows> = stmts.iter().map(|s| agrees(&g, s)).collect();
    g.shared_store().seal();
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join("engram_fold_review_paged");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 8 * 1024).expect("into_paged");
    let paged = Graph::new(store.clone(), realm, ns);
    paged.set_degree_table_after(0);
    let (got, trace) = engram_observe::with_trace(|| {
        stmts.iter().map(|s| agrees(&paged, s)).collect::<Vec<Rows>>()
    });
    assert_eq!(resident, got, "paged vs resident");
    assert!(trace.counters().get("paged.pread").copied().unwrap_or(0) > 0);
    assert!(trace.counters().get(FOLD).copied().unwrap_or(0) >= 5);
    drop(paged);
    let (reopened, _cache2) =
        engram_store::Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir");
    let g2 = Graph::new(reopened, realm, ns);
    g2.set_degree_table_after(0);
    let got2: Vec<Rows> = stmts.iter().map(|s| agrees(&g2, s)).collect();
    assert_eq!(resident, got2, "open_paged_dir vs resident");
    drop(g2);
    let _ = std::fs::remove_dir_all(&dir);
}
