#![allow(non_snake_case)]
//! ADVERSARIAL review #2 of the COUNT FOLD (S1) and the bound-endpoint EDGE
//! PREDICATE / ANTI-JOIN (S2), written INDEPENDENTLY of
//! `pipeline_fold_review.rs` and of the triage that repaired it.
//!
//! The bar is the same and is not negotiable: for every accepted shape the
//! fold ON, the fold OFF and the general (columnar-off) interpreter must
//! produce the byte-identical row set IN ORDER. Where a number is asserted it
//! is derived by hand AND pinned to the enumerated rows that sum to it, so an
//! arithmetic slip in the comment cannot pass as agreement.
//!
//! What this file attacks that the builder's suites and review #1 did not:
//!   - a REL VARIABLE on a hop the fold absorbs (a folded CLOSE binds a
//!     Rel-kind column that `run_hop_folded` never appends);
//!   - a CONTINUATION root — a folded hop whose source is materialised, so its
//!     isomorphism base is the driving row's `used_rels` — over parallel edges
//!     and self-loops;
//!   - the memo where a tracked subtree hop shares a type with a NON-sibling
//!     hop of the same path (grandparent / uncle), and across path boundaries
//!     where it must NOT be disabled;
//!   - keyed counts with NO `ORDER BY`, so group order itself is the assertion;
//!   - OPTIONAL beside a foldable chain: a leg that matches nothing must keep
//!     its one null-filled row, never be folded away;
//!   - the edge predicate against a labelled far end and against an
//!     OPTIONAL-introduced (nullable) var;
//!   - `sorted_by_peer == false` by CONSTRUCTION (an untyped table) and inside
//!     a transaction with buffered rows, not only via the canary;
//!   - the paged store, on the fold/anti-join shapes review #1 does not run.

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

/// fold ON / fold OFF / columnar OFF (the general per-tuple interpreter).
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

fn agrees(g: &Graph, src: &str) -> Rows {
    let (on, off, general) = triple(g, src);
    assert_eq!(on, general, "fold ON vs general: `{src}`");
    assert_eq!(off, general, "fold OFF vs general: `{src}`");
    on
}

fn counter(g: &Graph, src: &str, key: &str) -> Option<u64> {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied()
}

fn agrees_and_folds(g: &Graph, src: &str) -> Rows {
    let on = agrees(g, src);
    assert_eq!(counter(g, src, FOLD), Some(1), "fold must fire: `{src}`");
    on
}

/// memo ON == memo OFF (the memo is a pure cache, never a semantics change).
fn memo_agrees(g: &Graph, src: &str) {
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold_memo(false);
    let off = run(g, src);
    engram_graph::pipeline::set_count_fold_memo(true);
    let on = run(g, src);
    assert_eq!(on, off, "memo ON vs OFF: `{src}`");
}

const FOLD: &str = "interp.pipeline count fold";
const MEMO: &str = "interp.pipeline count fold memo";
const INLINE: &str = "interp.pipeline edge pred inline";
const CLOSE: &str = "interp.pipeline semijoin counted close";

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── Fixtures ────────────────────────────────────────────────────────────────

/// P{pk} 0..4 with every self-loop / parallel hazard, plus a Tag tail.
///   K (directed, creation order): p0->p0, p0->p0, p0->p1, p0->p1, p1->p2,
///      p2->p1, p2->p3, p3->p4, p4->p0, p3->p3
///   M: p0->p2, p2->p0, p1->p1
///   HI: p1->t0, p3->t0, p3->t1, p0->t1
/// So: p0 carries TWO parallel K self-loops and TWO parallel K edges to p1;
/// p3 carries ONE K self-loop; p1 carries an M self-loop.
fn gself() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let p: Vec<u64> = (0..5).map(|n| mk("P", "pk", n)).collect();
    let t: Vec<u64> = (0..2).map(|n| mk("Tag", "tk", n)).collect();
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

/// A{ak}-R->B{bk}-S->C{ck}-U->B, plus T from A to C (the anti-join target).
///   R: a0->b0,b1  a1->b1,b2  a2->b3
///   S: b0->c0,c1  b1->c1     b2->c2   b3->(none)
///   U: c0->b0,b1  c1->b1     c2->b3,b0
///   T: a0->c1     a1->c1     a2->c2
fn gdeep() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let a: Vec<u64> = (0..3).map(|n| mk("A", "ak", n)).collect();
    let b: Vec<u64> = (0..4).map(|n| mk("B", "bk", n)).collect();
    let c: Vec<u64> = (0..3).map(|n| mk("C", "ck", n)).collect();
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

// ─── (1) A REL VARIABLE on a hop the fold absorbs ───────────────────────────

/// A folded CLOSE that binds a relationship variable. `semijoin` appends the
/// closing rel's id as an extra Rel-kind COLUMN; `run_hop_folded` appends a
/// column only for an EXPAND (`hop.tgt.is_none()`), so a folded close appends
/// nothing — and every var bound after the rel var then sits one column to the
/// left of the index the plan resolved for it.
/// The SAFE spelling: the rel var is the LAST var in the binding order, so the
/// column the fold never appends shifts nothing and only the count is
/// observable. a=p0 -> b=p0 over loop#1 closes over loop#2 and vice versa (2);
/// a=p2 -> b=p3 closes over p3's single loop (1); a=p4 -> b=p0 arrives over the
/// (p4,p0) edge so BOTH of p0's loops close (2). Total 5.
#[test]
fn rel_var_on_a_folded_close_counts_the_multiplicity() {
    let g = gself();
    let src = "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b) RETURN count(*) AS n";
    let on = agrees(&g, src);
    assert_eq!(on, vec![vec![i(5)]], "rel-var close, count only");
    assert_eq!(counter(&g, src, FOLD), Some(1), "the close is folded away");
    // Enumerated, so the 5 is pinned to its rows.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b) RETURN a.pk AS k, b.pk AS j ORDER BY k, j"
        ),
        vec![
            vec![i(0), i(0)],
            vec![i(0), i(0)],
            vec![i(2), i(3)],
            vec![i(4), i(0)],
            vec![i(4), i(0)]
        ],
        "the five walks"
    );
}

/// THE DEFECT. A var bound AFTER the rel variable of a hop the fold absorbs.
/// `semijoin` appends the closing rel's id as an extra Rel-kind COLUMN;
/// `run_hop_folded` appends a column only for an EXPAND (`hop.tgt.is_none()`),
/// so the folded close appends nothing and `c` — `plan.vars[3]` — lands in
/// chunk column 2. `reduce_agg_groups` indexes the chunk by the PLAN's var
/// index (`pipeline.rs:5760`), so it reads column 3 of a 3-column chunk.
#[test]
fn rel_var_on_a_folded_close_does_not_shift_a_later_var() {
    let g = gself();
    // Path-0 walks per seed are a=p0 -> 2, a=p2 -> 1, a=p4 -> 2 (above).
    // M out-edges: p0->p2, p2->p0, p1->p1. So a=p0 contributes 2 rows with
    // c=p2, a=p2 contributes 1 row with c=p0, a=p4 has no M out-edge.
    let src = "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b), (a)-[:M]->(c:P) RETURN c.pk AS k, count(*) AS n ORDER BY k";
    // The fold OFF / general answers, first — they are the oracle for the row
    // set the fold ON run must reproduce.
    engram_graph::pipeline::set_count_fold(false);
    g.set_columnar_scans(true);
    let off = rows(&g, src);
    g.set_columnar_scans(false);
    let general = rows(&g, src);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(off, general, "fold OFF vs general");
    assert_eq!(
        general,
        vec![vec![i(0), i(1)], vec![i(2), i(2)]],
        "the interpreter's answer"
    );
    // And now with the fold ON.
    assert_eq!(rows(&g, src), general, "fold ON vs general: `{src}`");
}

/// The same defect reached by a close onto the SEED rather than onto the
/// level's own var, and by a projection that READS the dropped rel column.
#[test]
fn rel_var_on_a_folded_close_seed_target() {
    let g = gself();
    agrees(
        &g,
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(a), (a)-[:M]->(c:P) RETURN c.pk AS k, count(*) AS n ORDER BY k",
    );
}

/// The same plan with NOTHING reading the shifted var: no index is taken, so
/// the count is right by accident. Kept so the boundary of the defect is
/// pinned — it is the READ of a later var that panics, not the fold itself.
#[test]
fn rel_var_on_a_folded_close_unread_later_var_is_unharmed() {
    let g = gself();
    let on = agrees(
        &g,
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b), (a)-[:M]->(c:P) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(3)]], "2 rows with c=p2 plus 1 with c=p0");
}

/// THE SILENT FORM of the same defect. With TWO vars bound after the rel
/// variable the shift lands the group key on the NEXT var's column, which
/// exists — so nothing panics and the wrong answer is returned. `d` is folded,
/// so its column is the `NULL_ID` placeholder and the key reads as null.
#[test]
fn rel_var_on_a_folded_close_reads_the_next_vars_column() {
    let g = gself();
    let src = "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b), (a)-[:M]->(c:P), (c)-[:K]->(d:P) RETURN c.pk AS k, count(*) AS n ORDER BY k";
    engram_graph::pipeline::set_count_fold(false);
    g.set_columnar_scans(true);
    let off = rows(&g, src);
    g.set_columnar_scans(false);
    let general = rows(&g, src);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(off, general, "fold OFF vs general");
    assert_eq!(rows(&g, src), general, "fold ON vs general: `{src}`");
}

/// The BOUNDARY, so "a rel var on a folded close is broken" is not read as
/// "always": this spelling adds a THIRD path whose folded root is sourced from
/// the (unshifted) seed, the build declines before the shifted index is taken,
/// and the general path answers correctly. The defect needs a READ of a var
/// bound after the rel variable, or a fold root sourced from one.
#[test]
fn rel_var_on_a_folded_close_third_path_off_the_seed_still_agrees() {
    let g = gself();
    agrees(
        &g,
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b), (a)-[:M]->(c:P), (a)-[:HI]->(t:Tag) RETURN c.pk AS k, count(*) AS n ORDER BY k",
    );
}

/// The rel variable READ by the projection: the column it names was never
/// appended.
#[test]
fn rel_var_on_a_folded_close_read_by_the_projection() {
    let g = gself();
    for src in [
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b) RETURN type(r) AS t, count(*) AS n ORDER BY t",
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b) RETURN a.pk AS k, type(r) AS t, count(*) AS n ORDER BY k, t",
    ] {
        agrees(&g, src);
    }
}

// ─── (2) A CONTINUATION root over parallel edges and self-loops ─────────────

/// The fold's root hop is not always the first hop: when the middle var is
/// READ, the tail folds from a MATERIALISED source and its isomorphism base is
/// the driving row's `used_rels`, not an empty set. Over `gself` (parallel K
/// edges p0->p1 twice, parallel K self-loops on p0, a single loop on p3) the
/// difference between inheriting and not inheriting is visible in the number.
#[test]
fn a_continuation_root_inherits_the_rows_used_rels() {
    let g = gself();
    for src in [
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) RETURN b.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P) RETURN b.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(d:P) RETURN b.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) RETURN b.pk AS k, count(*) AS n ORDER BY k",
        // The middle var read AND an inline predicate at the folded level.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE a <> c RETURN b.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:M]-(c) RETURN b.pk AS k, count(*) AS n ORDER BY k",
    ] {
        agrees_and_folds(&g, src);
        memo_agrees(&g, src);
    }
    // DIRECTED, keyed by the middle: the arriving K edge must be excluded from
    // the continuation. K out-edges: p0: {p0,p0,p1,p1}; p1: {p2}; p2: {p1,p3};
    // p3: {p4,p3}; p4: {p0}.
    //   b=p0 is reached from a=p0 over each of its two loops, and from a=p4.
    //     - a=p0 over loop#1: c over K-out(p0) minus loop#1 = {loop#2, p1, p1} = 3
    //     - a=p0 over loop#2: likewise 3
    //     - a=p4 over (p4,p0): c over all four K-out(p0) = 4
    //     => 10
    //   b=p1 from a=p0 twice (each -> c=p2, 1) and from a=p2 (-> 1) => 3
    //   b=p2 from a=p1 => K-out(p2) = {p1,p3} => 2
    //   b=p3 from a=p2 (-> K-out(p3) = {p4, loop} = 2) and from a=p3 over its
    //     own loop (-> K-out(p3) minus that loop = {p4} = 1) => 3
    //   b=p4 from a=p3 => K-out(p4) = {p0} => 1
    let on = agrees_and_folds(
        &g,
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P) RETURN b.pk AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(
        on,
        vec![
            vec![i(0), i(10)],
            vec![i(1), i(3)],
            vec![i(2), i(2)],
            vec![i(3), i(3)],
            vec![i(4), i(1)],
        ],
        "directed continuation root, keyed by the materialised middle"
    );
    // Pinned to the rows it sums: b=p0's ten (a, c) pairs, enumerated.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P) WHERE b.pk = 0 RETURN a.pk AS ak, c.pk AS ck ORDER BY ak, ck"
        )
        .len(),
        10,
        "b=p0's rows, enumerated"
    );
}

// ─── (3) The MEMO against non-sibling hops of the same path ─────────────────

/// `memo_ok_for` disables the memo for a level whose tracked subtree hop
/// shares a relationship type with ANY other hop of the same path — not only
/// with its siblings. These shapes put the sharing hop two levels up, and in a
/// sibling BRANCH, and then check the converse: across a path boundary
/// (`reset`) the sets never meet, so the memo may stay on and must still agree.
#[test]
fn the_memo_is_a_pure_cache_across_shared_types_and_path_boundaries() {
    let g = gself();
    let shapes: &[&str] = &[
        // K ... M ... K: the tracked K hops are two apart in ONE path.
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K]-(d:P)-[:M]-(e:P) RETURN count(*) AS n",
        // A multi-type hop overlapping a single-type one two levels away.
        "MATCH (a:P)-[:K|M]-(b:P)-[:HI]->(t:Tag) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K|M]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:M]-(b:P)-[:K|M]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        // The same type on every hop of a three-hop path.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P)-[:K]->(d:P) RETURN count(*) AS n",
        // ACROSS a path boundary: the second path re-seeds `used`, so a shared
        // type there must NOT change the answer (and the memo may stay on).
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P), (a)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (a)-[:K]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(c:P) RETURN count(*) AS n",
        // A branch under one folded level: two children with a shared type.
        "MATCH (a:P)-[:M]-(b:P)-[:K]-(c:P), (b)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:M]-(b:P)-[:K]-(c:P), (b)-[:M]-(d:P) RETURN count(*) AS n",
    ];
    for src in shapes {
        agrees_and_folds(&g, src);
        memo_agrees(&g, src);
    }
    // The memo must actually FIRE on at least one of these, or the whole
    // differential above is vacuous.
    let fired = shapes
        .iter()
        .filter(|s| counter(&g, s, MEMO).is_some())
        .count();
    assert!(fired > 0, "the memo never fired on any shape: differential vacuous");
    // And the same over the deeper, pairwise-disjoint fixture (where the memo
    // is expected ON for every level).
    let gd = gdeep();
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B)-[:S]->(e:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (b)-[:S]->(e:C) RETURN count(*) AS n",
    ] {
        agrees_and_folds(&gd, src);
        memo_agrees(&gd, src);
    }
}

// ─── (4) Group ORDER with zero-count rows dropped, no ORDER BY ──────────────

/// A folded row whose count is zero is DROPPED. When the keyed count has no
/// `ORDER BY`, the group order is itself observable — so a drop that changes
/// which driving row is seen FIRST for a key would show here and nowhere else.
#[test]
fn keyed_group_order_survives_the_dropped_zero_rows() {
    let g = gdeep();
    for src in [
        // b3 has no S edge: a2's only walk folds to zero and its group vanishes.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a AS a, count(*) AS n",
        // Keyed by a var bound AFTER the zero-folding one.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) RETURN a.ak AS k, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b <> d RETURN a.ak AS k, count(*) AS n",
        // Two keys, ties between them.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) RETURN a.ak AS k, b.bk AS j, count(*) AS n",
        // A const key and a WITH form.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN 7 AS k, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a.ak AS k, count(*) AS n RETURN k, n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a.ak AS k, count(*) AS n WHERE n > 1 RETURN k, n",
        // LIMIT without ORDER BY: the first-seen prefix is the assertion.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n LIMIT 1",
    ] {
        agrees_and_folds(&g, src);
    }
    // The unordered keyed count, pinned: a2's group must be ABSENT, not zero.
    let on = agrees_and_folds(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n",
    );
    assert_eq!(
        on,
        vec![vec![i(0), i(3)], vec![i(1), i(2)]],
        "a2 folds to zero: its group vanishes, and the order is first-seen"
    );
}

// ─── (5) OPTIONAL beside a foldable chain ───────────────────────────────────

/// An OPTIONAL leg that matches nothing contributes exactly ONE null-filled
/// row. A fold applied to such a leg would drop it (the fold's zero-count rule
/// is the non-OPTIONAL semantics). `plan_count_fold` runs only on the single
/// MATCH aggregate's plan, so no leg may fold — pinned here by the numbers and
/// by the counter.
#[test]
fn an_optional_leg_that_matches_nothing_keeps_its_row() {
    let g = gdeep();
    for src in [
        "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(x:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C)-[:U]->(d:B) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) OPTIONAL MATCH (c)-[:U]->(d:B) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) OPTIONAL MATCH (c)-[:U]->(d:B) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        // A leg whose WHERE removes every match: still one null row.
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) WHERE c.ck > 100 RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) WHERE NOT (c)-[:U]->(b) RETURN count(*) AS n",
    ] {
        let on = agrees(&g, src);
        assert_eq!(
            counter(&g, src, FOLD),
            None,
            "an OPTIONAL plan must never fold a leg: `{src}` -> {on:?}"
        );
    }
    // b3 has no S edge. R gives 5 (a,b) rows: (a0,b0)(a0,b1)(a1,b1)(a1,b2)(a2,b3).
    // S fan-out: b0 -> 2, b1 -> 1, b2 -> 1, b3 -> 0 (one null row).
    // So 2 + 1 + 1 + 1 + 1 = 6.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (b)-[:S]->(c:C) RETURN count(*) AS n"
        ),
        vec![vec![i(6)]],
        "the empty leg keeps its null row"
    );
}

// ─── (6) The edge predicate: labelled far end, nullable var ─────────────────

/// `edge_pred_of` refuses a labelled endpoint (the probe does not re-verify
/// the label) and the OPTIONAL admission refuses a probe against an
/// optional-introduced var (the sentinel `NULL_ID` would be handed to the
/// adjacency read). Both are DECLINES, so the general path must answer them —
/// and answer them the same.
#[test]
fn the_edge_predicate_declines_a_labelled_end_and_a_nullable_var() {
    let g = gdeep();
    // A LABELLED bound far end. `c` is a :C, never an :A, so the second and
    // fourth forms must be empty / zero — a probe that ignored the label would
    // count the T edges anyway.
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c:A) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c:A) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a:A)-[:T]->(c) RETURN count(*) AS n",
        // An anonymous labelled far end (a one-sided existence probe).
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(:C) RETURN count(*) AS n",
    ] {
        agrees(&g, src);
    }
    // `(a)-[:T]->(c:A)` cannot hold — no A carries a T edge to another A.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c:A) RETURN count(*) AS n"
        ),
        vec![vec![i(0)]],
        "a relabelled bound far end must be re-verified"
    );
    // The unlabelled twin is the reviewer's 3.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c) RETURN count(*) AS n"
        ),
        vec![vec![i(3)]],
        "the unlabelled probe still answers 3"
    );
    // A probe against an OPTIONAL-introduced var, in the same clause and in a
    // later one, both polarities.
    for src in [
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (a)-[:T]->(x:C) WHERE (x)-[:U]->(b) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (a)-[:T]->(x:C) WHERE NOT (x)-[:U]->(b) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (a)-[:T]->(x:C) OPTIONAL MATCH (b)-[:S]->(y:C) WHERE NOT (x)-[:U]->(b) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (a)-[:T]->(x:C) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        // The nullable var probed AFTER the OPTIONAL, in the tail's WHERE.
        "MATCH (a:A)-[:R]->(b:B) OPTIONAL MATCH (a)-[:T]->(x:C) WITH a, x WHERE NOT (x)-[:U]->(a) RETURN count(*) AS n",
    ] {
        agrees(&g, src);
    }
}

// ─── (7) sorted_by_peer false BY CONSTRUCTION, and inside a transaction ─────

/// The counted close and the fold's close both read `edge_count_slim`, which
/// binary-searches only a table whose `sorted_by_peer` was ESTABLISHED. Three
/// ways it is not: a MULTI-TYPE token list (the row is ordered by type first),
/// an id above the table bound, and a transaction with buffered rows for the
/// node and side. Each must fall back to the walk and answer identically.
#[test]
fn multi_type_and_txn_buffered_closes_fall_back_to_the_walk() {
    let g = gself();
    g.set_degree_table_after(0);
    // Multi-type closes and probes: `edge_count_slim` must never binary-search.
    let multi: &[&str] = &[
        "MATCH (a:P)-[:K]-(b:P)-[:K|M]-(a) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K|M]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K|M]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K|M]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
    ];
    for src in multi {
        agrees(&g, src);
    }
    let (_, trace) = engram_observe::with_trace(|| {
        multi.iter().map(|s| rows(&g, s)).collect::<Vec<Rows>>()
    });
    assert!(
        trace
            .counters()
            .get("graph.edge probe walked")
            .copied()
            .unwrap_or(0)
            > 0,
        "a multi-type close must WALK: {:?}",
        trace.counters()
    );

    // Inside a TRANSACTION with buffered rows for the nodes the closes read:
    // `edge_count_slim` sees `txn_pending` and walks the overlay. The answer
    // must move by exactly the buffered edges and stay ON == OFF == general.
    let base: Vec<Rows> = multi.iter().map(|s| rows(&g, s)).collect();
    g.begin_txn().expect("begin");
    let ids = rows(&g, "MATCH (p:P) RETURN p.pk AS k ORDER BY k");
    assert_eq!(ids.len(), 5, "five P nodes visible inside the transaction");
    let in_txn: Vec<Rows> = multi.iter().map(|s| rows(&g, s)).collect();
    assert_eq!(base, in_txn, "an open transaction with no writes changes nothing");
    g.rollback_txn();

    // A transaction that DOES buffer rows on the closing nodes.
    g.begin_txn().expect("begin 2");
    rows(
        &g,
        "MATCH (x:P) WHERE x.pk = 3 CREATE (x)-[:K]->(x) RETURN count(*) AS n",
    );
    let (buffered, trace) = engram_observe::with_trace(|| {
        multi.iter().map(|s| agrees(&g, s)).collect::<Vec<Rows>>()
    });
    assert!(
        trace
            .counters()
            .get("graph.edge probe binary search")
            .copied()
            .unwrap_or(0)
            == 0
            || trace
                .counters()
                .get("graph.edge probe walked")
                .copied()
                .unwrap_or(0)
                > 0,
        "a buffered node must not be answered from the shared table alone"
    );
    assert_ne!(
        base, buffered,
        "the buffered self-loop must change at least one count"
    );
    g.rollback_txn();
    let after: Vec<Rows> = multi.iter().map(|s| agrees(&g, s)).collect();
    assert_eq!(base, after, "rollback restores the counts");
}

// ─── (8) The canary, independently, over the fold AND the counted close ─────

#[test]
fn the_sorted_flag_canary_moves_no_number() {
    let g = gself();
    g.set_degree_table_after(0);
    let stmts: &[&str] = &[
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(a) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)-[:M]->(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:M]->(a) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (a)-[:M]-(c) RETURN count(*) AS n",
    ];
    let before: Vec<Rows> = stmts.iter().map(|s| agrees(&g, s)).collect();
    let (_, warm) = engram_observe::with_trace(|| {
        stmts.iter().map(|s| rows(&g, s)).collect::<Vec<Rows>>()
    });
    assert!(
        warm.counters()
            .get("graph.edge probe binary search")
            .copied()
            .unwrap_or(0)
            > 0,
        "the warm run never binary-searched"
    );
    assert!(
        warm.counters().get(FOLD).copied().unwrap_or(0) > 0,
        "no fold in the canary set"
    );
    assert!(
        warm.counters().get(CLOSE).copied().unwrap_or(0) > 0,
        "no counted close in the canary set"
    );
    assert!(
        warm.counters().get(INLINE).copied().unwrap_or(0) > 0,
        "no inline edge probe in the canary set"
    );
    let flipped = g.clear_adjacency_sorted_flags();
    assert!(flipped > 0, "the canary cleared no table");
    let (after, trace) = engram_observe::with_trace(|| {
        stmts.iter().map(|s| rows(&g, s)).collect::<Vec<Rows>>()
    });
    assert_eq!(before, after, "the walk must answer what the search did");
    assert_eq!(
        trace
            .counters()
            .get("graph.edge probe binary search")
            .copied(),
        None,
        "a cleared flag was consulted anyway"
    );
    // And with the flags still cleared, ON == OFF == general.
    for s in stmts {
        agrees(&g, s);
    }
}

// ─── (9) Dir::Both self-loops, verified independently of the triage ─────────

/// A node with THREE parallel self-loops and a node with none, under: an
/// untracked close (its own path), a tracked close (same path), a close onto
/// the seed, the inline self-edge probe, and the materialised counted close.
/// Every number is pinned to its enumerated rows.
#[test]
fn both_self_loops_counted_once_per_relationship() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |v: i64| {
        let mut p = BTreeMap::new();
        p.insert("pk".to_string(), Value::Int(v));
        g.create_node(&["P".into()], &p).expect("node")
    };
    let p: Vec<u64> = (0..4).map(mk).collect();
    let e = BTreeMap::new();
    // p0: three parallel K self-loops. p1: none. p2: one. p3: none.
    for (s, d) in [(0, 0), (0, 0), (0, 0), (2, 2), (0, 1), (1, 2), (2, 3), (3, 0)] {
        g.create_rel(p[s], "K", p[d], &e).expect("K");
    }
    // The bare self-loop degree, undirected: p0 -> 3, p2 -> 1, others 0.
    assert_eq!(
        agrees(&g, "MATCH (x:P)-[:K]-(x) RETURN x.pk AS k, count(*) AS n ORDER BY k"),
        vec![vec![i(0), i(3)], vec![i(2), i(1)]],
        "an undirected self-loop binds ONCE per relationship"
    );
    assert_eq!(
        agrees(&g, "MATCH (x:P)-[:K]->(x) RETURN x.pk AS k, count(*) AS n ORDER BY k"),
        vec![vec![i(0), i(3)], vec![i(2), i(1)]],
        "the directed spelling agrees"
    );
    for src in [
        // UNTRACKED close (its own path): the arriving rel is not excluded.
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        // TRACKED close onto the level's own var.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        // Close onto the SEED through a self-loop.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(a) RETURN count(*) AS n",
        // Inline self-edge probes at a folded level, both polarities.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (c)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (c)-[:K]-(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (b)-[:K]-(b) RETURN count(*) AS n",
        // Three self-loops behind a WHERE that keeps only the loop rows.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE a = c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE b = c RETURN count(*) AS n",
    ] {
        agrees(&g, src);
        memo_agrees(&g, src);
    }
    // The tracked own-var close, DIRECTED, enumerated then counted.
    // K out-edges: p0: {p0,p0,p0,p1}; p1: {p2}; p2: {p2,p3}; p3: {p0}.
    //   a=p0 -> b=p0 over each of its three loops; each close then has the
    //     OTHER two loops => 3 x 2 = 6. a=p0 -> b=p1: p1 has no loop => 0.
    //   a=p1 -> b=p2: p2's single loop, not the arriving rel => 1.
    //   a=p2 -> b=p2 over its own loop: no other loop => 0. b=p3: none => 0.
    //   a=p3 -> b=p0: arriving rel is (p3,p0), so all three loops close => 3.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k"
        ),
        vec![vec![i(0), i(6)], vec![i(1), i(1)], vec![i(3), i(3)]],
        "tracked own-var close over three parallel loops"
    );
    // The same, materialised (b is the group key) — the counted-close path.
    let src = "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN b.pk AS j, count(*) AS n ORDER BY j";
    assert_eq!(
        agrees(&g, src),
        vec![vec![i(0), i(9)], vec![i(2), i(1)]],
        "the same walks keyed by b: p0 gets 6+3, p2 gets 1"
    );
    assert_eq!(counter(&g, src, FOLD), None, "b is read: no fold");

    // UNTYPED probes (`(x)--(y)`): `type_tokens_peek` answers `None`, which is
    // never the single-type case, so `edge_count_slim` always WALKS — over a
    // table whose `sorted_by_peer` is false by construction.
    for src in [
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)--(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE (c)--(c) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (c)--(c) RETURN count(*) AS n",
    ] {
        agrees(&g, src);
    }

    // And the whole set again with every table's sorted flag CLEARED, so the
    // counted close and the fold's close both take the walk.
    g.set_degree_table_after(0);
    let warm: Vec<Rows> = [
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    ]
    .iter()
    .map(|s| rows(&g, s))
    .collect();
    let flipped = g.clear_adjacency_sorted_flags();
    assert!(flipped > 0, "the canary cleared no table");
    let walked: Vec<Rows> = [
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    ]
    .iter()
    .map(|s| agrees(&g, s))
    .collect();
    assert_eq!(warm, walked, "three parallel loops, searched vs walked");
}

// ─── (10) Overflow: the largest fitting count is exact ──────────────────────

/// `levels` ranks of `width` nodes, rank k fully connected to rank k+1 by the
/// pairwise-distinct type `E<k>` — so every level memoises and one seed's walk
/// count is `width^(levels-1)`.
fn gladder(width: usize, levels: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let e = BTreeMap::new();
    let mut ranks: Vec<Vec<u64>> = Vec::new();
    for k in 0..levels {
        let mut ids = Vec::new();
        for j in 0..width {
            let mut p = BTreeMap::new();
            p.insert("k".to_string(), Value::Int(j as i64));
            ids.push(g.create_node(&[format!("L{k}")], &p).expect("node"));
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

#[test]
fn the_largest_fitting_count_is_exact_and_the_budget_cannot_reach_it() {
    let g = gladder(16, 17);
    g.set_row_budget(Some(100_000));
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(true);
    // 15 hops from a rank-0 seed = 16^15 = 2^60 walks; keyed by the seed, 16
    // groups of exactly that.
    let fit = ladder_query(16, true);
    let (on, trace) = engram_observe::with_trace(|| rows(&g, &fit));
    assert_eq!(on.len(), 16);
    for (j, r) in on.iter().enumerate() {
        assert_eq!(r, &vec![i(j as i64), i(1i64 << 60)], "row {j}");
    }
    assert_eq!(trace.counters().get(FOLD).copied(), Some(1));
    assert_eq!(trace.counters().get(MEMO).copied(), Some(1));
    // The GLOBAL form of the same 15 hops sums to 16 x 2^60 = 2^64, which
    // leaves both `u64` and `i64` — the fold must DECLINE, never wrap. It
    // declines to a path that cannot finish, so the only assertion available
    // without running forever is on the shorter ladder either side of the
    // boundary: 14 hops global = 16^15 = 2^60 fits and is exact.
    let short = ladder_query(15, false);
    let got = rows(&g, &short);
    assert_eq!(got, vec![vec![i(1i64 << 60)]], "16 x 16^14 = 2^60, exact");
    // Fold OFF the same statement cannot materialise 2^60 rows.
    engram_graph::pipeline::set_count_fold(false);
    let res = run(&g, &short);
    engram_graph::pipeline::set_count_fold(true);
    assert!(res.is_err(), "fold OFF must refuse on the budget");
}

// ─── (11) Paged: the fold and the anti-join over pread ──────────────────────

#[test]
fn paged_agrees_with_resident_on_the_fold_and_the_anti_join() {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = gself();
    let stmts: &[&str] = &[
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE NOT (a)-[:K]-(c) AND a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) RETURN b.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN b.pk AS j, count(*) AS n ORDER BY j",
        "MATCH (a:P)-[:K]-(b:P)-[:M]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K|M]-(c:P)-[:HI]->(t:Tag) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P) WHERE NOT (b)-[:K]-(a) RETURN a.pk AS k, b.pk AS j ORDER BY k, j",
        "MATCH (a:P)-[:K]->(b:P)-[r:K]->(b) RETURN count(*) AS n",
    ];
    let resident: Vec<Rows> = stmts.iter().map(|s| agrees(&g, s)).collect();
    g.shared_store().seal();
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join("engram_fold_review2_paged");
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
    assert!(
        trace.counters().get(FOLD).copied().unwrap_or(0) >= 4,
        "the fold must still fire over the paged store: {:?}",
        trace.counters().get(FOLD)
    );
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

// ─── (12) The un-fold-the-target rule, over a single shared type ────────────

/// The triage's new rule un-folds a close TARGET that is not on the level's
/// ancestor chain, then lets the fixpoint re-decide. Over `gself` every hop is
/// `:K`, so relationship isomorphism (per PATH) is live on every one of these
/// and a plan that moved a hop across a path boundary would show. The pairs
/// below are the SAME patterns written in two orders — the rule's whole claim
/// is that both orders reach the same answer.
#[test]
fn sibling_closes_agree_in_both_pattern_orders() {
    let g = gself();
    let pairs: &[(&str, &str)] = &[
        (
            "MATCH (a:P)-[:K]-(d:P), (a)-[:K]-(b:P)-[:K]-(c:P), (c)-[:K]-(d) RETURN count(*) AS n",
            "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P), (a)-[:K]-(d:P), (c)-[:K]-(d) RETURN count(*) AS n",
        ),
        (
            "MATCH (a:P)-[:M]-(d:P), (a)-[:K]-(b:P)-[:K]-(c:P), (c)-[:M]-(d) RETURN count(*) AS n",
            "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P), (a)-[:M]-(d:P), (c)-[:M]-(d) RETURN count(*) AS n",
        ),
        (
            "MATCH (a:P)-[:K]->(d:P), (a)-[:K]->(b:P)-[:K]->(c:P), (c)-[:K]->(d) RETURN a.pk AS k, count(*) AS n ORDER BY k",
            "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P), (a)-[:K]->(d:P), (c)-[:K]->(d) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        ),
    ];
    for (first, second) in pairs {
        let x = agrees(&g, first);
        let y = agrees(&g, second);
        assert_eq!(x, y, "the two orders count the same walks:\n  {first}\n  {second}");
        memo_agrees(&g, first);
        memo_agrees(&g, second);
    }
    // The keyed spelling of the same shape (which materialises the target for
    // the OTHER reason) must reach the same plan and the same number.
    for src in [
        "MATCH (a:P)-[:K]-(d:P), (a)-[:K]-(b:P)-[:K]-(c:P), (c)-[:K]-(d) RETURN d.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(d:P), (a)-[:K]-(b:P)-[:K]-(c:P), (c)-[:K]-(d) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    ] {
        agrees(&g, src);
    }
}

// ─── (13) Sites, stages, seeds and UNWIND around the fold ───────────────────

/// The fold is admitted only under an all-`count(*)`, non-DISTINCT projection.
/// Every neighbour of that boundary must DECLINE and still agree.
#[test]
fn non_star_sites_and_distinct_decline_and_agree() {
    let g = gdeep();
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(c) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(DISTINCT c) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n, count(c) AS m",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN collect(c.ck) AS xs",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN min(c.ck) AS lo, count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN DISTINCT a.ak AS k",
    ] {
        let on = agrees(&g, src);
        assert_eq!(
            counter(&g, src, FOLD),
            None,
            "a non-star site must not fold: `{src}` -> {on:?}"
        );
    }
}

/// The fold beside the other pipeline machinery: an anchored seed, a WITH
/// stage, UNWIND, ORDER BY/LIMIT on the count, and a post-aggregate WHERE.
#[test]
fn the_fold_composes_with_seeds_stages_and_unwind() {
    let g = gdeep();
    for src in [
        // A seed anchored by a property (the index seek path).
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE a.ak = 0 RETURN count(*) AS n",
        "MATCH (a:A {ak: 0})-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE a.ak IN [0, 1] RETURN a.ak AS k, count(*) AS n ORDER BY k",
        // ORDER BY / LIMIT over the folded count.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY n DESC, k LIMIT 1",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY n ASC, k",
        // Form A with a post-aggregate WHERE.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a.ak AS k, count(*) AS n WHERE n = 2 RETURN k, n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a, count(*) AS n WHERE n > 2 RETURN a.ak AS k, n",
        // A second MATCH stage after the aggregate.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a, count(*) AS n MATCH (a)-[:T]->(x:C) RETURN a.ak AS k, n, x.ck AS j ORDER BY k, j",
    ] {
        agrees(&g, src);
    }
    for src in [
        "UNWIND [0, 1, 2] AS z MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE a.ak = z RETURN z, count(*) AS n ORDER BY z",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) UNWIND [1, 2] AS z RETURN z, count(*) AS n ORDER BY z",
    ] {
        agrees(&g, src);
    }
}

// ─── (14) The inline predicate against a MATERIALISED var (position rule) ───

/// `NeBound` / `EqBound` / `EdgeToBound` against a var that materialises: the
/// position rule admits it only when that var is bound BEFORE the fold's root.
/// Both sides of that boundary, and both polarities, over `gdeep` where the
/// answer moves.
#[test]
fn inline_predicates_against_a_materialised_var_both_sides_of_the_position_rule() {
    let g = gdeep();
    for src in [
        // The other var is the SEED — always before the root.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE NOT (a)-[:R]->(d) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:R]->(d) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:T]->(c) RETURN count(*) AS n",
        // The other var is a MATERIALISED middle bound before the root.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b <> d RETURN b.bk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE b = d RETURN b.bk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE NOT (d)-[:S]->(c) RETURN b.bk AS k, count(*) AS n ORDER BY k",
        // The other var is bound AFTER the root: the level must un-fold.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) WHERE c <> d RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) WHERE NOT (c)-[:U]->(d) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) WHERE b <> d RETURN count(*) AS n",
    ] {
        agrees(&g, src);
        memo_agrees(&g, src);
    }
    // `(a)-[:R]->(d) AND b <> d` = 2 (review #1's number, re-derived): the (a,b,c,d)
    // walks are a0: b0->c0->{b0,b1} keeps d=b1 (R a0->b1, b0<>b1) = 1; b0->c1->b1
    // keeps 1; b1->c1->b1 dropped by b=d. a1: b1->c1->b1 dropped; b2->c2->{b3,b0}
    // neither is an R target of a1. a2: b3 has no S edge.
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:R]->(d) AND b <> d RETURN count(*) AS n"
        ),
        vec![vec![i(2)]],
        "the inline pair"
    );
    assert_eq!(
        agrees(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:U]->(d:B) WHERE (a)-[:R]->(d) AND b <> d RETURN a.ak AS k, b.bk AS j, c.ck AS m, d.bk AS q ORDER BY k, j, m, q"
        ),
        vec![vec![i(0), i(0), i(0), i(1)], vec![i(0), i(0), i(1), i(1)]],
        "the two rows the 2 sums"
    );
}
