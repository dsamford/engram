#![allow(non_snake_case)]
//! Differential tests for the MULTI-STAGE `MATCH … WITH [DISTINCT] <vars> MATCH
//! … RETURN …` columnar pipeline (`pipeline::run_multistage`): a TWO-stage read
//! — stage-1 MATCH+WHERE, a WITH carrying pattern variables forward (optionally
//! DISTINCT), a stage-2 MATCH+WHERE continuing from a carried var, then a RETURN
//! the single-stage tail already handles. This is LDBC SNB IC5's
//! stage-1->stage-2 shape.
//!
//! THE CONTRACT (`pipeline_semijoin`/`pipeline_optional`'s): for every accepted
//! shape, `set_columnar_scans(true)` (the multi-stage pipeline) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full
//! ROW SET *and its order*, byte-for-byte — and every declined shape falls back
//! and still agrees. The pipeline must FIRE on accepts (a distinct 'multistage
//! runs' counter) and must NOT on declines. Most accepts compare with NO ORDER
//! BY, so the raw PRODUCTION ORDER is itself under test (canary #3 breaks it).
//!
//! WHAT THE MULTI-STAGE PATH REPRODUCES from `run_streaming`:
//!   - PRODUCTION ORDER across the boundary: stage-1 rows in production order,
//!     and for each, its stage-2 rows in expansion order.
//!   - WITH DISTINCT: dedup the carried tuples FIRST-SEEN *before* stage 2, so a
//!     duplicate carried tuple does not multiply stage-2 fan-out.
//!   - RELATIONSHIP ISOMORPHISM RESETS AT THE WITH: Cypher rel-uniqueness is
//!     per-MATCH-clause, so a stage-2 edge may reuse an edge stage 1 walked.
//!
//! THREE CANARIES (each: break it, the differential FAILS vs the oracle):
//!   1. carry stage-1 `used_rels` across the WITH (no rel-iso reset) -> the
//!      shared-edge test drops rows the oracle keeps.
//!   2. skip the WITH DISTINCT dedup at the boundary -> stage-2 fan-out
//!      multiplies and the count/row test diverges.
//!   3. reverse stage-1 production order -> the order-pinned test diverges.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The MAIN fixture. P{k,name} people, Po{pk} posts.
///   KNOWS (P->P, prop w): p0->p1(100), p0->p2(200), p1->p2(300).
///   LIKES (P->Po):        p1->po0, p2->po1, p2->po2.
/// Two `a`s reach `b=p2` over KNOWS (p0->p2 and p1->p2), so `WITH b` carries a
/// DUPLICATE p2 while `WITH DISTINCT b` collapses it — the dedup fixture. p2
/// LIKES two posts, so the duplicate multiplies stage-2 fan-out unless deduped.
fn gmain() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_p = |k: i64, name: &str| {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(k));
        p.insert("name".to_string(), Value::Str(name.to_string()));
        g.create_node(&["P".into()], &p).expect("p")
    };
    let p = [mk_p(0, "a"), mk_p(1, "b"), mk_p(2, "c"), mk_p(3, "d")];
    let mk_po = |pk: i64| {
        let mut m = BTreeMap::new();
        m.insert("pk".to_string(), Value::Int(pk));
        g.create_node(&["Po".into()], &m).expect("po")
    };
    let po = [mk_po(10), mk_po(11), mk_po(12)];

    // KNOWS with weight (creation order is load-bearing for reverse adjacency).
    for (src, dst, w) in [(0usize, 1usize, 100i64), (0, 2, 200), (1, 2, 300)] {
        let mut m = BTreeMap::new();
        m.insert("w".to_string(), Value::Int(w));
        g.create_rel(p[src], "KNOWS", p[dst], &m).expect("KNOWS");
    }
    // LIKES P->Po.
    for (src, dst) in [(1usize, 0usize), (2, 1), (2, 2)] {
        g.create_rel(p[src], "LIKES", po[dst], &BTreeMap::new())
            .expect("LIKES");
    }
    g
}

/// The rel-iso fixture: a directed 3-cycle over N{k}. R: n0->n1(e0), n1->n2(e1),
/// n2->n0(e2). A 2-hop stage 1 records two rels per row; a 2-hop stage 2 out of
/// the carried end REUSES one of them, so it exercises the per-clause rel-iso
/// reset (and canary #1: carrying stage-1 `used_rels` would drop those rows).
fn gcycle() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |k: i64| {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(k));
        g.create_node(&["N".into()], &m).expect("n")
    };
    let n = [mk(0), mk(1), mk(2)];
    for (src, dst) in [(0usize, 1usize), (1, 2), (2, 0)] {
        g.create_rel(n[src], "R", n[dst], &BTreeMap::new())
            .expect("R");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON and the general path OFF.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the MULTI-STAGE pipeline fired for `src` with columnar ON.
fn multistage_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline multistage runs")
        .copied()
        == Some(1)
}

/// Content assert that is robust to row order (the ORDER is checked separately by
/// the ON==OFF differential on the same, unordered, query). `Value` is not `Ord`,
/// so sort by a stable debug rendering — enough for the small Int/Str fixtures.
fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    v.sort_by_key(|row| format!("{row:?}"));
    v
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// Pass-through WITH then a second hop. `MATCH (a)-[:KNOWS]->(b) WITH b MATCH
/// (b)-[:LIKES]->(c) RETURN c` — the plain two-stage read. ON==OFF (production
/// order under test — no ORDER BY), and the pipeline fires.
#[test]
fn passthrough_with_then_second_hop_matches_general() {
    let g = gmain();
    let cases: &[&str] = &[
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN b.k AS bk, c.pk AS pk",
        // A stage-2 top-k: ORDER BY a bound-var expr + LIMIT.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk ORDER BY c.pk SKIP 1 LIMIT 2",
        // `b AS b` — the accepted same-name alias.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b AS b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "passthrough vs general disagree: `{src}`");
    }
    for src in cases {
        assert!(multistage_fired(&g, src), "must fire: `{src}`");
    }
    // The non-distinct carry keeps the DUPLICATE b=p2 → p2's two posts appear
    // TWICE, p1's post once: [10,11,11,12,12] as a multiset.
    let src = "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk";
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        sorted(on),
        vec![
            vec![i(10)],
            vec![i(11)],
            vec![i(11)],
            vec![i(12)],
            vec![i(12)]
        ],
        "non-distinct carry must keep the duplicate fan-out"
    );
}

/// ORDER-FIDELITY (canary #3's target). A stage-1 WHERE pins the carried order
/// to a NON-palindromic sequence (from p0: b=[p2,p1]), so the raw production
/// order of stage 2 is observable: [12,11,10]. Reversing stage-1 order (canary
/// #3) would make ON diverge from this OFF oracle.
#[test]
fn production_order_is_pinned_across_the_boundary() {
    let g = gmain();
    let src = "MATCH (a:P)-[:KNOWS]->(b:P) WHERE a.k = 0 WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "production order vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(12)], vec![i(11)], vec![i(10)]],
        "stage-1 x stage-2 production order must be exactly reproduced"
    );
    assert!(multistage_fired(&g, src), "the order-pinned read must fire");
}

/// WITH DISTINCT then a second hop. The duplicate carried `b=p2` collapses, so
/// p2's two posts appear ONCE: {10,11,12}. ON==OFF (production order), and the
/// dedup is BEFORE stage 2 (contrast the non-distinct case above). The COUNT
/// differential is what canary #2 breaks.
#[test]
fn with_distinct_collapses_stage2_fanout() {
    let g = gmain();
    let src =
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "WITH DISTINCT vs general disagree");
    assert_eq!(
        sorted(on),
        vec![vec![i(10)], vec![i(11)], vec![i(12)]],
        "WITH DISTINCT must collapse the duplicate carried tuple's fan-out"
    );
    assert!(multistage_fired(&g, src), "WITH DISTINCT must fire");
    // The COUNT differential the dedup canary breaks: DISTINCT counts 3, the
    // non-distinct carry counts 5.
    let cd = "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) RETURN count(*) AS n";
    let (on, off) = both(&g, cd, BTreeMap::new());
    assert_eq!(on, off, "DISTINCT count vs general disagree");
    assert_eq!(on, vec![vec![i(3)]], "DISTINCT collapses to 3 stage-2 rows");
    assert!(multistage_fired(&g, cd), "DISTINCT count must fire");
    let cn = "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN count(*) AS n";
    assert_eq!(
        rows(&g, cn, BTreeMap::new()),
        vec![vec![i(5)]],
        "non-distinct fan-out is 5"
    );
}

/// REL-ISO RESETS AT THE WITH (and canary #1's fixture). A 2-hop stage 1 and a
/// 2-hop stage 2 over the directed 3-cycle share an edge per row; the stage-2
/// edge that also appears in stage 1 must NOT be excluded. ON==OFF keeps all
/// three rows: e.k in {1,2,0}.
#[test]
fn rel_iso_resets_across_the_with() {
    let g = gcycle();
    let src = "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N) WITH c MATCH (c)-[:R]->(d:N)-[:R]->(e:N) RETURN e.k AS ek";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "rel-iso reset vs general disagree");
    assert_eq!(
        sorted(on),
        vec![vec![i(0)], vec![i(1)], vec![i(2)]],
        "a stage-2 edge reused from stage 1 must be kept (rel-iso resets at WITH)"
    );
    assert!(
        multistage_fired(&g, src),
        "the two-hop/two-hop read must fire"
    );
}

/// STAGE-2 WHERE. A single-var predicate over a stage-2 var filters the second
/// hop; ON==OFF and the pipeline fires.
#[test]
fn stage2_where_matches_general() {
    let g = gmain();
    let cases: &[&str] = &[
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) WHERE c.pk > 10 RETURN c.pk AS pk",
        // A WHERE over a CARRIED var after stage 2.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) WHERE b.k = 2 RETURN c.pk AS pk",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "stage-2 WHERE vs general disagree: `{src}`");
        assert!(
            multistage_fired(&g, src),
            "stage-2 WHERE must fire: `{src}`"
        );
    }
}

/// STAGE-2 ORDER BY + LIMIT — a bounded top-k after two stages, keyed on a
/// bound-var expr. ON==OFF (including the production-order tiebreak the reverse
/// canary breaks).
#[test]
fn stage2_order_by_limit_topk() {
    let g = gmain();
    let cases: &[&str] = &[
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN b.k AS bk, c.pk AS pk ORDER BY c.pk DESC LIMIT 3",
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk ORDER BY c.pk LIMIT 2",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "stage-2 top-k vs general disagree: `{src}`");
        assert!(
            multistage_fired(&g, src),
            "stage-2 top-k must fire: `{src}`"
        );
    }
}

/// STAGE-2 AGGREGATE — a global `count` and a grouped `count`, over the stage-2
/// chunk. ON==OFF, exact rows.
#[test]
fn stage2_aggregate_matches_general() {
    let g = gmain();
    let cases: &[(&str, Vec<Vec<Value>>)] = &[
        (
            "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) RETURN count(c) AS n",
            vec![vec![i(3)]],
        ),
        (
            "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:LIKES]->(c:Po) RETURN b.k AS bk, count(*) AS n ORDER BY bk",
            vec![vec![i(1), i(1)], vec![i(2), i(2)]],
        ),
    ];
    for (src, want) in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "stage-2 aggregate vs general disagree: `{src}`");
        assert_eq!(&on, want, "stage-2 aggregate rows wrong: `{src}`");
        assert!(
            multistage_fired(&g, src),
            "stage-2 aggregate must fire: `{src}`"
        );
    }
}

/// STAGE-2 DISTINCT RETURN — the final clause dedups its projected items. ON==OFF.
#[test]
fn stage2_distinct_return() {
    let g = gmain();
    // Non-distinct carry (b=p2 duplicated) then a DISTINCT RETURN over c.pk: the
    // duplicate posts collapse at the RETURN. {10,11,12}.
    let src =
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN DISTINCT c.pk AS pk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "stage-2 DISTINCT RETURN vs general disagree");
    assert_eq!(
        sorted(on),
        vec![vec![i(10)], vec![i(11)], vec![i(12)]],
        "the DISTINCT RETURN must dedup its projected items"
    );
    assert!(
        multistage_fired(&g, src),
        "stage-2 DISTINCT RETURN must fire"
    );
}

/// INCOMING and UNDIRECTED stage-2 hops route through `Dir::In` / `Dir::Both`,
/// byte-identical to the general path (neighbour order included).
#[test]
fn stage2_incoming_and_undirected_hop() {
    let g = gmain();
    let cases: &[&str] = &[
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)<-[:KNOWS]-(x:P) RETURN x.k AS xk",
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (b)-[:KNOWS]-(x:P) RETURN x.k AS xk",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "incoming/undirected stage-2 hop disagree: `{src}`");
        assert!(multistage_fired(&g, src), "must fire: `{src}`");
    }
}

/// A CARRIED RELATIONSHIP VAR composes: stage 1 binds `r`, the WITH carries `r`
/// (and `b`), stage 2 drives from `b`, and the RETURN reads `r.w`. ON==OFF.
#[test]
fn carried_rel_var_composes() {
    let g = gmain();
    let src = "MATCH (a:P)-[r:KNOWS]->(b:P) WITH r, b MATCH (b)-[:LIKES]->(c:Po) RETURN r.w AS w, c.pk AS pk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "carried rel var vs general disagree");
    assert!(multistage_fired(&g, src), "carried rel var must fire");
}

// ─── DECLINES ───────────────────────────────────────────────────────────────

/// Shapes the multi-stage pipeline DECLINES — each must fall back to the general
/// path (ON==OFF) and NOT fire the multistage counter.
#[test]
fn declines_out_of_scope_shapes() {
    let g = gmain();
    let declines: &[&str] = &[
        // aggregate-in-WITH.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b, count(*) AS n MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // ORDER BY on the WITH itself.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b ORDER BY b.k MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // LIMIT on the WITH itself.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b LIMIT 1 MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // a computed expression in the WITH (carried alongside the bare var).
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b, b.k AS bk MATCH (b)-[:LIKES]->(c:Po) RETURN bk AS bk, c.pk AS pk",
        // an alias-rename to a different name.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b AS bb MATCH (bb)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // a WITH dropping the var chain2 continues from (b is re-scanned).
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH a MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // three stages (WITH … WITH …).
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b WITH b MATCH (b)-[:LIKES]->(c:Po) RETURN c.pk AS pk",
        // a chain2 disconnected from the carried var.
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH DISTINCT b MATCH (y:P)-[:LIKES]->(z:Po) RETURN z.pk AS pk",
    ];
    for src in declines {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(
            sorted(on),
            sorted(off),
            "declined shape must still agree via general path: `{src}`"
        );
        assert!(
            !multistage_fired(&g, src),
            "the multistage pipeline must DECLINE: `{src}`"
        );
    }
}

/// The single-stage shapes must NOT be claimed as multi-stage (no false firing).
#[test]
fn single_stage_shapes_do_not_fire_multistage() {
    let g = gmain();
    let singles: &[&str] = &[
        "MATCH (a:P)-[:KNOWS]->(b:P) RETURN b.k AS bk",
        "MATCH (a:P)-[:KNOWS]->(b:P) RETURN count(*) AS n",
        "MATCH (a:P)-[:KNOWS]->(b:P) WITH b, count(*) AS n RETURN n ORDER BY n",
    ];
    for src in singles {
        assert!(
            !multistage_fired(&g, src),
            "a single-stage shape must not fire multistage: `{src}`"
        );
    }
}
