#![allow(non_snake_case)]
//! Differential tests for the COUNT FOLD (operator A of
//! `docs/lsqb-completeness-plan.md`, `pipeline::plan_count_fold` /
//! `fold_tail`): under an all-`count(*)` aggregate, a hop whose end var nothing
//! reads is not expanded — the qualifying walks through it (and its subtree)
//! are COUNTED per driving row and multiplied into the row's weight, and
//! `reduce_agg_groups` adds weights instead of 1s.
//!
//! The contract is the other `pipeline_*` suites': for every accepted shape the
//! TRIPLE must agree — the fold ON (`pipeline::set_count_fold(true)`, the
//! default), the fold OFF (every hop materialises through the same columnar
//! aggregate), and columnar OFF (the per-tuple `run_streaming` aggregation) —
//! the full ROW SET *and its order*, byte-for-byte; and every declined shape
//! falls back and still agrees.
//!
//! WHAT THE FOLD MUST REPRODUCE, each pinned below:
//!   - a CHAIN and a TREE of folded hops (product of sums);
//!   - the per-level MEMO is a pure cache (memo ON == memo OFF, and it fires);
//!   - RELATIONSHIP ISOMORPHISM through a folded walk — a KNOWS-KNOWS walk over
//!     a self-loop and parallel edges, open and closed onto the seed;
//!   - `count(*)` vs `count(var)` / DISTINCT / any other site: no fold;
//!   - a KEYED count whose tail folds keeps FIRST-SEEN group order, and a
//!     group whose fold counts zero VANISHES (as the general path never
//!     produces its rows);
//!   - overflow → decline (a unit test inside `pipeline.rs`, where the
//!     pre-existing weight can be set to `i64::MAX`; no corpus can reach it).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

// ─── Fixtures ────────────────────────────────────────────────────────────────

/// A{ak}-R->B{bk}-S->C{ck}. Creation order is load-bearing (it fixes the
/// reverse-adjacency production order the keyed tests observe).
///   R: a0->b0,b1,b2   a1->b2,b3   a2->(none)   a3->b1
///   S: b0->c0,c1      b1->(none)  b2->c2       b3->c0,c1,c2
/// so per (a,b) the S-fan-out is b0:2 b1:0 b2:1 b3:3 — b1 folds to ZERO (its
/// rows vanish), and the b0/b2 groups TIE at count 2 (first-seen decides).
fn gchain() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let a: Vec<u64> = (0..4).map(|i| mk("A", "ak", i)).collect();
    let b: Vec<u64> = (0..4).map(|i| mk("B", "bk", i)).collect();
    let c: Vec<u64> = (0..3).map(|i| mk("C", "ck", i)).collect();
    for (s, t) in [(0, 0), (0, 1), (0, 2), (1, 2), (1, 3), (3, 1)] {
        g.create_rel(a[s], "R", b[t], &BTreeMap::new()).expect("R");
    }
    for (s, t) in [(0, 0), (0, 1), (2, 2), (3, 0), (3, 1), (3, 2)] {
        g.create_rel(b[s], "S", c[t], &BTreeMap::new()).expect("S");
    }
    g
}

/// The LSQB q4 TREE: `(:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P),
/// (m)<-[:RO]-(c:C)` — three folded legs out of the middle var. Messages with a
/// missing leg (m2 has no LK, m3 no RO) fold to zero.
fn gtree() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let t: Vec<u64> = (0..2).map(|i| mk("T", "tk", i)).collect();
    let m: Vec<u64> = (0..4).map(|i| mk("M", "mk", i)).collect();
    let p: Vec<u64> = (0..3).map(|i| mk("P", "pk", i)).collect();
    let c: Vec<u64> = (0..3).map(|i| mk("C", "ck", i)).collect();
    let e = BTreeMap::new();
    for (s, d) in [(0, 0), (0, 1), (1, 0), (2, 1), (3, 1), (3, 0)] {
        g.create_rel(m[s], "HT", t[d], &e).expect("HT");
    }
    for (s, d) in [(0, 0), (1, 1), (2, 2), (3, 0)] {
        g.create_rel(m[s], "HC", p[d], &e).expect("HC");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 1), (2, 1), (2, 3)] {
        g.create_rel(p[s], "LK", m[d], &e).expect("LK");
    }
    for (s, d) in [(0, 0), (1, 0), (2, 1), (0, 2)] {
        g.create_rel(c[s], "RO", m[d], &e).expect("RO");
    }
    g
}

/// The q1 CHAIN with pairwise-DISTINCT types: `(:X0)<-[:T1]-(:X1)<-[:T2]-(:X2)
/// -[:T3]->(:X3)-[:T4]->(:X4)` where every level fans in/out, so each level's
/// memo is hit many times (x1 nodes are reached from several x0, …).
fn gmemo() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let x0: Vec<u64> = (0..3).map(|i| mk("X0", i)).collect();
    let x1: Vec<u64> = (0..3).map(|i| mk("X1", i)).collect();
    let x2: Vec<u64> = (0..4).map(|i| mk("X2", i)).collect();
    let x3: Vec<u64> = (0..3).map(|i| mk("X3", i)).collect();
    let x4: Vec<u64> = (0..2).map(|i| mk("X4", i)).collect();
    let e = BTreeMap::new();
    // T1: x1 -> x0 (every x1 into two x0s — x1 levels are shared).
    for (s, d) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 0), (2, 2)] {
        g.create_rel(x1[s], "T1", x0[d], &e).expect("T1");
    }
    // T2: x2 -> x1.
    for (s, d) in [(0, 0), (1, 0), (1, 1), (2, 1), (3, 2), (3, 0)] {
        g.create_rel(x2[s], "T2", x1[d], &e).expect("T2");
    }
    // T3: x2 -> x3.
    for (s, d) in [(0, 0), (0, 1), (1, 1), (2, 2), (3, 0), (3, 2)] {
        g.create_rel(x2[s], "T3", x3[d], &e).expect("T3");
    }
    // T4: x3 -> x4 (x3[1] has none — a zero leaf).
    for (s, d) in [(0, 0), (0, 1), (2, 1)] {
        g.create_rel(x3[s], "T4", x4[d], &e).expect("T4");
    }
    g
}

/// The KNOWS fixture: P{pk} with a SELF-LOOP on p0, PARALLEL K edges p0->p1
/// (twice), a RECIPROCAL pair p1->p2 / p2->p1, a chord p2->p3, and HAS_INTEREST
/// into Tag{tk} (p1, p3 each; p2 none). Every rel-iso hazard the fold must
/// reproduce: an undirected walk may not reuse the self-loop or the same
/// parallel edge, but MAY close over the OTHER parallel edge.
fn gknows() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let p: Vec<u64> = (0..4).map(|i| mk("P", "pk", i)).collect();
    let t: Vec<u64> = (0..2).map(|i| mk("Tag", "tk", i)).collect();
    let e = BTreeMap::new();
    g.create_rel(p[0], "K", p[0], &e).expect("self");
    g.create_rel(p[0], "K", p[1], &e).expect("par1");
    g.create_rel(p[0], "K", p[1], &e).expect("par2");
    g.create_rel(p[1], "K", p[2], &e).expect("k12");
    g.create_rel(p[2], "K", p[1], &e).expect("k21");
    g.create_rel(p[2], "K", p[3], &e).expect("k23");
    g.create_rel(p[1], "HI", t[0], &e).expect("hi");
    g.create_rel(p[3], "HI", t[0], &e).expect("hi");
    g.create_rel(p[3], "HI", t[1], &e).expect("hi");
    g
}

// ─── Harness ─────────────────────────────────────────────────────────────────

type Rows = Vec<Vec<Value>>;

fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// The TRIPLE: fold ON (columnar), fold OFF (columnar), columnar OFF (general).
fn triple(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let on = rows(g, src);
    engram_graph::pipeline::set_count_fold(false);
    let fold_off = rows(g, src);
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(false);
    let general = rows(g, src);
    g.set_columnar_scans(true);
    (on, fold_off, general)
}

fn counter(g: &Graph, src: &str, key: &str) -> Option<u64> {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied()
}

fn fold_fired(g: &Graph, src: &str) -> bool {
    counter(g, src, "interp.pipeline count fold") == Some(1)
}

fn agg_fired(g: &Graph, src: &str) -> bool {
    counter(g, src, "interp.pipeline aggregate runs") == Some(1)
}

/// The fold must FIRE and the triple must agree row-for-row and in order.
fn agrees_and_fires(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let (on, fold_off, general) = triple(g, src);
    assert_eq!(on, general, "fold ON vs general disagree: `{src}`");
    assert_eq!(fold_off, general, "fold OFF vs general disagree: `{src}`");
    assert!(fold_fired(g, src), "the count fold did not fire: `{src}`");
    assert!(
        agg_fired(g, src),
        "the aggregate operator did not fire: `{src}`"
    );
    on
}

/// The fold must DECLINE (the aggregate pipeline still answers) and agree.
///
/// The DECLINE is read with the COUNT-ONLY JOIN REORDER (operator C) held OFF:
/// a decline is a claim about the pattern AS WRITTEN, and operator C exists to
/// rewrite a `count(*)`-only pattern into an equivalent one that DOES fold
/// (pinned by `pipeline_count_reorder.rs`). The ROW agreement is still taken
/// with every default on, so the rewritten plan is still held to the general
/// path's answer.
fn declines_but_agrees(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let (on, fold_off, general) = triple(g, src);
    assert_eq!(on, general, "columnar vs general disagree: `{src}`");
    assert_eq!(fold_off, general, "fold OFF vs general disagree: `{src}`");
    engram_graph::pipeline::set_count_only_reorder(false);
    let fired = counter(g, src, "interp.pipeline count fold");
    engram_graph::pipeline::set_count_only_reorder(true);
    assert_eq!(
        fired, None,
        "the count fold should have DECLINED: `{src}`"
    );
    on
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── Chain ───────────────────────────────────────────────────────────────────

/// A 2-hop CHAIN under `count(*)`: global, keyed by the seed (Form B and A),
/// with a seed WHERE, and with a tie cut by LIMIT. Every tail var folds — the
/// count is Σ_a Σ_b deg_S(b) — and the triple agrees.
#[test]
fn fold_chain_matches_general_across_shapes() {
    let g = gchain();
    let cases: &[&str] = &[
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a.ak AS k, count(*) AS n RETURN k, n ORDER BY k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE a.ak > 0 RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a, count(*) AS n WHERE n > 2 RETURN a.ak AS k, n ORDER BY k",
        // The seed keyed by IDENTITY (the u64 fast path in the reducer).
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH a, count(*) AS n RETURN a.ak AS k, n ORDER BY k",
        // Two count(*) sites in one projection — still all-star.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n, count(*) + 1 AS m",
        // An anonymous seed (`(:A)`), the q1 spelling.
        "MATCH (:A)-[:R]->(:B)-[:S]->(:C) RETURN count(*) AS n",
        // A single hop keyed by the seed: the fold IS the degree sum. (The
        // GLOBAL single-hop `count(*)` is a census shape another operator owns
        // before the pipeline is tried, so it is not listed here.)
        "MATCH (a:A)-[:R]->(b:B) RETURN a.ak AS k, count(*) AS n ORDER BY k",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // The explicit numbers: a0: b0(2)+b1(0)+b2(1) = 3; a1: b2(1)+b3(3) = 4; a3: 0.
    let on = agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(on, vec![vec![i(0), i(3)], vec![i(1), i(4)]], "chain counts");
    let on = agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(7)]], "global chain count");
}

/// A group whose fold counts ZERO must VANISH (a3 → b1 → no S edge: the general
/// path never produces an (a3, b1, c) row, so `ak = 3` has no group), while a
/// global count over an all-zero fold is the single `0` row.
#[test]
fn fold_drops_zero_count_rows_like_the_general_path() {
    let g = gchain();
    let on = agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(*) AS n ORDER BY k",
    );
    assert!(
        !on.iter().any(|r| r[0] == i(3)),
        "a3's only walk folds to zero — its group must be ABSENT: {on:?}"
    );
    // Every walk folds to zero: one global row of 0, never no row.
    let on = agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C)-[:S]->(d:C) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(0)]], "an all-zero fold is the zero row");
}

// ─── Tree ────────────────────────────────────────────────────────────────────

/// The q4 TREE: three folded legs out of `m`, whose per-message product is
/// deg_HC × deg_LK × deg_RO; a message missing any leg contributes 0. Keyed by
/// the seed tag and global, Form B and A.
#[test]
fn fold_tree_matches_general() {
    let g = gtree();
    let cases: &[&str] = &[
        "MATCH (:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) RETURN count(*) AS n",
        "MATCH (t:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) RETURN t.tk AS k, count(*) AS n ORDER BY k",
        "MATCH (t:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) WITH t, count(*) AS n RETURN t.tk AS k, n ORDER BY k",
        // The middle var READ (a key): its legs still fold, `m` materialises.
        "MATCH (t:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) RETURN m.mk AS k, count(*) AS n ORDER BY k",
        // Two legs only, and legs in a different textual order.
        "MATCH (t:T)<-[:HT]-(m:M)<-[:RO]-(c:C), (m)<-[:LK]-(l:P) RETURN count(*) AS n",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // Per message: m0: HC1×LK1×RO2 = 2 (tags t0,t1 → 4 rows); m1: 1×3×1 = 3 (t0);
    // m2: LK 0 → 0; m3: RO 0 → 0. Total 4 + 3 = 7.
    let on = agrees_and_fires(
        &g,
        "MATCH (:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(7)]], "tree count");
    let on = agrees_and_fires(
        &g,
        "MATCH (t:T)<-[:HT]-(m:M)-[:HC]->(p:P), (m)<-[:LK]-(l:P), (m)<-[:RO]-(c:C) RETURN m.mk AS k, count(*) AS n ORDER BY k",
    );
    assert_eq!(
        on,
        vec![vec![i(0), i(4)], vec![i(1), i(3)]],
        "keyed-by-middle tree counts; m2/m3 fold to zero and vanish"
    );
}

// ─── Memo ────────────────────────────────────────────────────────────────────

/// The per-level MEMO is a pure cache: on the q1-shaped chain with pairwise
/// disjoint types every level memoises, the memo counter fires, and the count
/// equals the memo-OFF fold and the general path.
#[test]
fn fold_memo_agrees_with_no_memo_and_fires() {
    let g = gmemo();
    let src =
        "MATCH (:X0)<-[:T1]-(:X1)<-[:T2]-(:X2)-[:T3]->(:X3)-[:T4]->(:X4) RETURN count(*) AS n";
    let on = agrees_and_fires(&g, src);
    assert_eq!(
        counter(&g, src, "interp.pipeline count fold memo"),
        Some(1),
        "the shared X1/X2 levels must be served from the memo"
    );
    // Memo OFF: the same fold, every level re-enumerated — identical count and
    // no memo counter.
    engram_graph::pipeline::set_count_fold_memo(false);
    let no_memo = rows(&g, src);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    engram_graph::pipeline::set_count_fold_memo(true);
    assert_eq!(on, no_memo, "memo ON vs memo OFF disagree");
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline count fold memo")
            .copied(),
        None,
        "memo OFF must not report a memo hit"
    );
    assert_eq!(
        trace.counters().get("interp.pipeline count fold").copied(),
        Some(1),
        "memo OFF is still the fold"
    );
    // The number, by hand: per x2: T3-fanout weighted by T4 — x3[0]:2 x3[1]:0
    // x3[2]:1 → x2[0]: 2+0=2, x2[1]: 0, x2[2]: 1, x2[3]: 2+1=3; per x1 (T2 in):
    // x1[0]: x2[0]+x2[1]+x2[3] = 5, x1[1]: x2[1]+x2[2] = 1, x1[2]: x2[3] = 3;
    // per x0 (T1 in): x0[0]: x1[0]+x1[2] = 8, x0[1]: x1[0]+x1[1] = 6,
    // x0[2]: x1[1]+x1[2] = 4 → 18.
    assert_eq!(on, vec![vec![i(18)]], "memo chain count");
}

/// A level that reads an OUTSIDE var (an inline `<>` against the seed) is NOT
/// memoised — the count still agrees, and no memo counter fires on a chain
/// whose only shared level is the one that reads the seed.
#[test]
fn fold_level_reading_the_seed_is_not_memoised() {
    let g = gknows();
    // `p1 <> p3` is inlined at p3's level, which reads the seed: no memo there.
    let src = "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) WHERE a <> c RETURN count(*) AS n";
    agrees_and_fires(&g, src);
    assert_eq!(
        counter(&g, src, "interp.pipeline count fold memo"),
        None,
        "a level reading an outside var is never served from the memo"
    );
}

// ─── Relationship isomorphism ────────────────────────────────────────────────

/// KNOWS-KNOWS over a self-loop and parallel edges. A folded 2-hop undirected
/// walk may not reuse the self-loop or the SAME parallel edge (but may take the
/// other), exactly as `expand`'s per-row `used` set enforces; the types are
/// NOT disjoint so the memo is off and every walk is enumerated with `used`.
#[test]
fn fold_rel_iso_knows_knows_self_loop_and_parallel() {
    let g = gknows();
    let cases: &[&str] = &[
        // Open walks, undirected and directed.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(c:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)<-[:K]-(c:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(d:P) RETURN count(*) AS n",
        // CLOSED onto the seed inside the fold: the closing edge may not reuse
        // the opening one (the self-loop round trip drops; the parallel pair
        // closes over the other edge).
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(a) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(a) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:K]-(a) RETURN count(*) AS n",
        // The close as a SEPARATE 1-hop path re-seeds `used` — the reuse is kept.
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(a) RETURN count(*) AS n",
        // Keyed by the seed, so per-seed rel-iso is observable.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(a) RETURN a.pk AS k, count(*) AS n ORDER BY k",
        // q6 / q9 spellings.
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) WHERE a <> c RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P)-[:HI]->(t:Tag) WHERE NOT (a)-[:K]-(c) AND a <> c RETURN count(*) AS n",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // The single-path close vs the cross-path close differ EXACTLY by the
    // reuse rows (self-loop round trip + each parallel edge reused) — pinned so
    // the rel-iso inside the fold is provably load-bearing.
    let single = agrees_and_fires(&g, "MATCH (a:P)-[:K]-(b:P)-[:K]-(a) RETURN count(*) AS n");
    let cross = agrees_and_fires(
        &g,
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(a) RETURN count(*) AS n",
    );
    let (Value::Int(s), Value::Int(c)) = (&single[0][0], &cross[0][0]) else {
        panic!("counts");
    };
    assert!(
        c > s,
        "the cross-path close must keep the reuse rows: single {s}, cross {c}"
    );
}

// ─── Declines ────────────────────────────────────────────────────────────────

/// `count(var)`, `count(DISTINCT …)`, a mixed projection, or any other site
/// reads the var (or is not a star count): the aggregate pipeline still
/// answers, but WITHOUT the fold.
#[test]
fn fold_declines_count_var_distinct_and_other_sites() {
    let g = gchain();
    let declines: &[&str] = &[
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(c) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(DISTINCT c) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n, count(c) AS m",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN collect(c.ck) AS cs",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN sum(c.ck) AS s",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN a.ak AS k, count(b) AS n ORDER BY k",
        // Every var is read (grouping key + ORDER BY): nothing to fold.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN b.bk AS k, c.ck AS ck, count(*) AS n ORDER BY k, ck",
        // A property WHERE on the tail var reads it.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE c.ck > 0 RETURN count(*) AS n",
        // A bound rel var on the tail hop.
        "MATCH (a:A)-[:R]->(b:B)-[r:S]->(c:C) RETURN count(*) AS n",
    ];
    for src in declines {
        declines_but_agrees(&g, src);
        assert!(
            agg_fired(&g, src),
            "the aggregate operator must still answer: `{src}`"
        );
    }
    // A DISTINCT projection has NO site — the fold has nothing to weight.
    let (on, _, general) = triple(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN DISTINCT a.ak AS k ORDER BY k",
    );
    assert_eq!(on, general);
    assert_eq!(
        counter(
            &g,
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN DISTINCT a.ak AS k ORDER BY k",
            "interp.pipeline count fold"
        ),
        None
    );
}

/// A tail var read by a WHERE conjunct the fold cannot inline stays
/// materialised; the partial fold (the deeper hop) still fires. A two-var
/// conjunct between SIBLING folded vars cannot be factorised and declines.
#[test]
fn fold_partial_and_sibling_pred_declines() {
    let g = gchain();
    // `b.bk > 0` reads b: b materialises, c still folds.
    agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE b.bk > 0 RETURN count(*) AS n",
    );
    // Siblings under the seed compared: neither can fold (no fold at all).
    declines_but_agrees(
        &g,
        "MATCH (a:A)-[:R]->(b:B), (a)-[:R]->(d:B) WHERE b <> d RETURN count(*) AS n",
    );
    // A conjunct between the seed and the tail var DOES inline (q5's shape).
    agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE a <> c RETURN count(*) AS n",
    );
    let on = agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) WHERE b <> d RETURN count(*) AS n",
    );
    // b <> d with d bound AFTER the fold's root: d materialises... and so must
    // b (the position rule), leaving only c folded — still a fold, still equal.
    let _ = on;
}

// ─── Keyed count with a folded tail ──────────────────────────────────────────

/// FIRST-SEEN group order under a folded tail. Grouping by `b` (materialised)
/// with `c` folded: b0 and b2 TIE at 2 walks each, so `ORDER BY n DESC LIMIT 2`
/// keeps b3 (3) plus whichever tied group was seen FIRST in production order —
/// which the fold must not disturb; and b1 (fold 0) never appears.
#[test]
fn fold_keyed_count_keeps_first_seen_order() {
    let g = gchain();
    let full =
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN b.bk AS k, count(*) AS n ORDER BY n DESC";
    let on = agrees_and_fires(&g, full);
    assert!(
        !on.iter().any(|r| r[0] == i(1)),
        "b1 folds to 0 — no group: {on:?}"
    );
    assert_eq!(on.len(), 3, "b0, b2, b3 groups");
    for lim in 1..=3 {
        let src = format!(
            "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN b.bk AS k, count(*) AS n ORDER BY n DESC LIMIT {lim}"
        );
        let cut = agrees_and_fires(&g, &src);
        assert_eq!(
            cut,
            on[..lim].to_vec(),
            "the LIMIT {lim} prefix is first-seen decided"
        );
    }
    // Node-identity key (the reducer's u64 fast path), same tie.
    agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WITH b, count(*) AS n RETURN b.bk AS k, n ORDER BY n DESC LIMIT 2",
    );
    // The tie in ASCENDING order: the first-seen of {b0, b2} leads.
    agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN b.bk AS k, count(*) AS n ORDER BY n LIMIT 1",
    );
}

/// A MATERIALISED hop AFTER the fold's root (a second path off the seed whose
/// end is a grouping key): the fold runs at its root's position, the later hop
/// multiplies the weighted rows, and the group order is the general path's.
#[test]
fn fold_root_before_a_materialised_hop() {
    let g = gchain();
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) RETURN d.bk AS k, count(*) AS n ORDER BY n DESC, k",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B) RETURN d.bk AS k, count(*) AS n ORDER BY n DESC LIMIT 1",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C), (a)-[:R]->(d:B)-[:S]->(e:C) RETURN d.bk AS k, count(*) AS n ORDER BY k",
    ] {
        agrees_and_fires(&g, src);
    }
}

// ─── A close onto the level's OWN var ────────────────────────────────────────

/// A SELF-CLOSE — `(b)-[:K]-(b)`, whose target IS the var the level binds — is
/// foldable: `hop_sum`'s close arm reads `bind[b]`, and the expand that entered
/// the level set `bind[b] = peer` before calling `level`, so the target is
/// bound by construction. (It is the hop twin of the inline `(x)-[:T]->(x)`
/// probe, which `plan_count_fold` has always admitted as `other == level`.)
///
/// Rel-iso is the discriminator and both sides are pinned: spelled as ONE path
/// the close inherits the level's `used`, so a walk that arrived over p0's
/// self-loop may not close over it; spelled as TWO paths (the comma) the close
/// re-seeds `used` empty and the same loop closes.
#[test]
fn fold_self_close_onto_the_levels_own_var() {
    let g = gknows();
    // ONE path: `a-K-b-K-b`. The only closable level is b=p0 (the sole K
    // self-loop). a=p1 reaches b=p0 over each of the two PARALLEL edges, and
    // neither is the loop, so both close: 2. a=p0 reaches b=p0 over the loop
    // itself, which rel-iso then forbids reusing: 0.
    assert_eq!(
        agrees_and_fires(&g, "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN count(*) AS n"),
        vec![vec![i(2)]]
    );
    assert_eq!(
        agrees_and_fires(
            &g,
            "MATCH (a:P)-[:K]-(b:P)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k"
        ),
        vec![vec![i(1), i(2)]],
        "only p1's two parallel edges reach a closable level"
    );
    // TWO paths: the close re-seeds `used`, so the arriving rel is not
    // excluded and b=p0's loop closes from every a that reaches it. p0's
    // undirected K-neighbours are {p0,p1,p1} (the loop offered once) and p1's
    // are {p0,p0,p2,p2}: a=p0 → 1, a=p1 → 2, a=p2/p3 → 0.
    assert_eq!(
        agrees_and_fires(&g, "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN count(*) AS n"),
        vec![vec![i(3)]]
    );
    assert_eq!(
        agrees_and_fires(
            &g,
            "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b) RETURN a.pk AS k, count(*) AS n ORDER BY k"
        ),
        vec![vec![i(0), i(1)], vec![i(1), i(2)]]
    );
    // The DIRECTED spellings, and the self-close beside a further folded leg.
    for src in [
        "MATCH (a:P)-[:K]->(b:P)-[:K]->(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]->(b:P), (b)-[:K]->(b) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P), (b)-[:K]-(b), (b)-[:HI]->(t:Tag) RETURN count(*) AS n",
        "MATCH (a:P)-[:K]-(b:P)-[:K]-(c:P), (c)-[:K]-(c) RETURN a.pk AS k, count(*) AS n ORDER BY k",
    ] {
        agrees_and_fires(&g, src);
    }
}

// ─── A close onto a folded NON-ancestor ──────────────────────────────────────

/// A close whose target is a folded var that is NOT on the level's ancestor
/// chain (a SIBLING branch under the seed) un-folds the TARGET, not the level:
/// the target becomes a real column, and the position rule then admits the
/// close. Un-folding the level instead would have materialised that same target
/// anyway PLUS the level's whole chain, so this is never the worse plan.
///
/// The boundary is the position rule, and it is pinned both ways: reorder the
/// pattern so the target is bound AFTER the fold's root and materialising it
/// cannot help — the level un-folds and the statement declines, still agreeing.
#[test]
fn fold_un_folds_a_close_target_that_is_not_an_ancestor() {
    let g = gtree();
    // c -RO-> m (the close's target, a sibling branch), c -RO-> m2 -HC-> p,
    // p -LK-> m. Target `m` is bound by the FIRST hop, below the fold root's
    // end var, so it materialises and `p`'s level closes onto the column.
    let folds =
        "MATCH (c:C)-[:RO]->(m:M), (c)-[:RO]->(m2:M)-[:HC]->(p:P), (p)-[:LK]->(m) RETURN count(*) AS n";
    assert_eq!(agrees_and_fires(&g, folds), vec![vec![i(3)]]);
    // The SAME three patterns with the target bound last: c -RO-> m2 -HC-> p
    // first, then c -RO-> m, then the close. `m` is above the root's end var,
    // the position rule fails, and the whole chain materialises.
    let declines =
        "MATCH (c:C)-[:RO]->(m2:M)-[:HC]->(p:P), (c)-[:RO]->(m:M), (p)-[:LK]->(m) RETURN count(*) AS n";
    assert_eq!(declines_but_agrees(&g, declines), vec![vec![i(3)]]);
    // Keyed by the seed, and with the target itself read (which materialises it
    // for the other reason — the read set — and must reach the same plan).
    for src in [
        "MATCH (c:C)-[:RO]->(m:M), (c)-[:RO]->(m2:M)-[:HC]->(p:P), (p)-[:LK]->(m) RETURN c.ck AS k, count(*) AS n ORDER BY k",
        "MATCH (c:C)-[:RO]->(m:M), (c)-[:RO]->(m2:M)-[:HC]->(p:P), (p)-[:LK]->(m) RETURN m.mk AS k, count(*) AS n ORDER BY k",
    ] {
        agrees_and_fires(&g, src);
    }
}

// ─── Lever ───────────────────────────────────────────────────────────────────

/// Non-vacuity: with the fold lever OFF the same statement is answered by the
/// aggregate operator WITHOUT the fold counter; with columnar OFF neither fires.
#[test]
fn fold_fires_only_when_the_lever_is_on() {
    let g = gchain();
    let src = "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) RETURN count(*) AS n";
    assert!(fold_fired(&g, src));
    engram_graph::pipeline::set_count_fold(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(
        trace.counters().get("interp.pipeline count fold").copied(),
        None
    );
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline aggregate runs")
            .copied(),
        Some(1)
    );
    g.set_columnar_scans(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    g.set_columnar_scans(true);
    assert_eq!(
        trace.counters().get("interp.pipeline count fold").copied(),
        None
    );
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline aggregate runs")
            .copied(),
        None
    );
}
