#![allow(non_snake_case)]
//! Differential tests for the two-MATCH SET-BASED HASH-JOIN columnar pipeline
//! (`pipeline::run_join`): `MATCH <chainA> [WHERE] MATCH <chainB> [WHERE]
//! <group-by aggregate> ORDER BY <total> [LIMIT]`, where chainB shares an
//! already-bound variable with chainA (Cypher comma-join semantics). The nested
//! `run_streaming` re-runs chainB per chainA row (O(N*M)); this pipeline runs each
//! side ONCE and hash-joins on the shared id-tuple (O(N+M)) — the LDBC SNB IC5
//! friend/forum shape.
//!
//! THE CONTRACT (every other `pipeline_*.rs`'s): for every ACCEPTED shape,
//! `set_columnar_scans(true)` (the hash-join pipeline) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full
//! ROW SET *and its order*, byte-for-byte — and every DECLINED shape falls back
//! and still agrees. The pipeline must FIRE on accepts (a distinct 'join runs'
//! counter) and must NOT on declines.
//!
//! WHY BYTE-IDENTITY HOLDS despite the hash join's DIFFERENT row order: it fires
//! ONLY when the output does not depend on that order — an ORDER-INSENSITIVE
//! aggregate (count/sum/min/max) under a TOTAL ORDER BY (or a global aggregate,
//! one row). The group SET (key -> aggregate) is identical regardless of join
//! order (count is commutative), and a total ORDER BY fully determines row order.
//!
//! THREE CANARIES (each: break it, the differential FAILS vs the oracle):
//!   1. emit the join as a CARTESIAN product (ignore the key match) -> the count
//!      diverges (too high).
//!   2. let chainA's `used_rels` constrain chainB (seed side B with side A's
//!      traversed rels) -> the shared-edge cycle fixture drops rows the oracle
//!      keeps.
//!   3. drop the total-order tiebreaker (accept `ORDER BY count DESC` alone) on a
//!      TIE-HEAVY fixture -> the row order diverges. This one demonstrates WHY the
//!      total-order decline exists.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The IC5 fixture. Person p0..p3 (id 100..103), Forum f0..f2 (id 200..202),
/// Post o0..o7 (id 300..307).
///   HAS_MEMBER (Forum->Person): f0->{p0,p1}, f1->{p1,p2}, f2->{p0}.
///   CONTAINER_OF (Forum->Post) + HAS_CREATOR (Post->Person):
///     f0: o0(p0), o1(p1), o2(p3)      -- p3 is NOT a member of f0
///     f1: o3(p1), o4(p2), o5(p2)
///     f2: o6(p0), o7(p1)              -- p1 is NOT a member of f2
/// IC5 counts posts whose CREATOR is a MEMBER of the containing forum:
///   f0 -> 2 (o0,o1), f1 -> 3 (o3,o4,o5), f2 -> 1 (o6).
fn gic5() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let p: Vec<u64> = (0..4).map(|k| mk("Person", 100 + k)).collect();
    let f: Vec<u64> = (0..3).map(|k| mk("Forum", 200 + k)).collect();
    let o: Vec<u64> = (0..8).map(|k| mk("Post", 300 + k)).collect();
    let member = |fi: usize, pi: usize| {
        g.create_rel(f[fi], "HAS_MEMBER", p[pi], &BTreeMap::new())
            .expect("HAS_MEMBER");
    };
    for (fi, pi) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 0)] {
        member(fi, pi);
    }
    // (forum, post, creator).
    let post = |fi: usize, oi: usize, pi: usize| {
        g.create_rel(f[fi], "CONTAINER_OF", o[oi], &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(o[oi], "HAS_CREATOR", p[pi], &BTreeMap::new())
            .expect("HAS_CREATOR");
    };
    for (fi, oi, pi) in [
        (0, 0, 0),
        (0, 1, 1),
        (0, 2, 3),
        (1, 3, 1),
        (1, 4, 2),
        (1, 5, 2),
        (2, 6, 0),
        (2, 7, 1),
    ] {
        post(fi, oi, pi);
    }
    g
}

/// A directed 3-cycle over N{k}: n0->n1(e0), n1->n2(e1), n2->n0(e2). A 2-hop
/// chainA records two rels per row; a 2-hop chainB out of the shared end REUSES
/// one of them, exercising the PER-MATCH-CLAUSE rel-iso reset (canary #2: seeding
/// side B with chainA's rels would drop those rows).
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

/// A TIE-HEAVY fixture (canary #3): three forums each containing exactly ONE post
/// by a member, so `count(post)` TIES at 1 across all three. Memberships INVERT
/// the orderings: chainA scans Person p0,p1,p2 (ascending) whose member forums
/// are f2,f1,f0 (so the nested loop first-sees forum ids 202,201,200), while side
/// B seeds from the DISTINCT forum node ids in creation order f0,f1,f2 (ids
/// 200,201,202). So the two strategies first-see the tied groups in OPPOSITE
/// order. With the total `, id ASC` tiebreak both converge to id-ascending;
/// WITHOUT it (canary #3) the row order diverges — proving why the total-order
/// decline exists.
fn gtie() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    // Forum ids ASCEND with creation order; person p_k is a member of forum
    // f_{2-k}, so the Person-scan-driven forum order is the REVERSE of the
    // forum-node-id order side B seeds from.
    let f = [mk("Forum", 200), mk("Forum", 201), mk("Forum", 202)];
    let p = [mk("Person", 100), mk("Person", 101), mk("Person", 102)];
    let o = [mk("Post", 300), mk("Post", 301), mk("Post", 302)];
    for k in 0..3 {
        let fi = 2 - k; // p0->f2, p1->f1, p2->f0
        g.create_rel(f[fi], "HAS_MEMBER", p[k], &BTreeMap::new())
            .expect("HAS_MEMBER");
        g.create_rel(f[fi], "CONTAINER_OF", o[k], &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(o[k], "HAS_CREATOR", p[k], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON, then the general path OFF (the oracle).
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

/// Whether the HASH-JOIN pipeline fired for `src` with columnar ON.
fn join_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline join runs").copied() == Some(1)
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// THE IC5 SHAPE: two MATCHes sharing TWO vars (friend, forum), group by forum,
/// count(post), ORDER BY count DESC + a unique id tiebreak. ON==OFF exactly (rows
/// AND order), and the join fires.
#[test]
fn ic5_two_shared_vars_matches_general() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "IC5 join vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(201), i(3)], vec![i(200), i(2)], vec![i(202), i(1)]],
        "IC5 exact rows + total order"
    );
    assert!(join_fired(&g, src), "the IC5 join must fire");
}

/// The IC5 shape with a LIMIT — the bounded head of the same total order.
#[test]
fn ic5_with_limit() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC LIMIT 2";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "IC5+LIMIT join vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(201), i(3)], vec![i(200), i(2)]],
        "IC5 top-2"
    );
    assert!(join_fired(&g, src), "IC5+LIMIT must fire");
}

/// A join sharing ONE var (forum only): count(post) per forum multiplies by the
/// forum's member count (each member A-row joins each post B-row) — exactly the
/// nested loop's fan-out. ON==OFF, and the join fires.
#[test]
fn one_shared_var_matches_general() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post) \
               RETURN forum.id AS id, count(post) AS c ORDER BY id";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "one-shared-var join vs general disagree");
    // f0: 2 members x 3 posts = 6; f1: 2 x 3 = 6; f2: 1 x 2 = 2.
    assert_eq!(
        on,
        vec![vec![i(200), i(6)], vec![i(201), i(6)], vec![i(202), i(2)]],
        "one-shared-var fan-out multiplicity"
    );
    assert!(join_fired(&g, src), "one-shared-var join must fire");
}

/// A WHERE that STRADDLES both sides — a single-var predicate on side A
/// (`friend`) and one on side B (`post`), plus a shared-var predicate. Each is
/// applied to its own side's chunk (equivalent to the general path's per-clause
/// filter since the shared columns coincide). ON==OFF, and the join fires.
#[test]
fn where_straddling_both_sides_matches_general() {
    let g = gic5();
    let cases: &[&str] = &[
        // wA on friend (side A), wB on post (side B).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) WHERE friend.id > 100 \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) WHERE post.id < 306 \
         RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC",
        // wB over the SHARED var forum (constrains which forums survive side B).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) WHERE forum.id >= 201 \
         RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "straddling WHERE vs general disagree: `{src}`");
        assert!(join_fired(&g, src), "straddling WHERE must fire: `{src}`");
    }
}

/// min / max / count aggregates over a side-B column, grouped by forum, ordered
/// by the (single) grouping key. All FULLY order-insensitive (a set's min/max/
/// count do not depend on fold order, any type) -> ON==OFF, and the join fires.
#[test]
fn min_max_count_aggregates_match_general() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, min(post.id) AS mn, max(post.id) AS mx, \
               count(post) AS c ORDER BY id";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "min/max/count join vs general disagree");
    // f0 posts kept: o0(300),o1(301) -> mn300 mx301 c2.
    // f1 posts kept: o3(303),o4(304),o5(305) -> mn303 mx305 c3.
    // f2 posts kept: o6(306) -> mn306 mx306 c1.
    assert_eq!(
        on,
        vec![
            vec![i(200), i(300), i(301), i(2)],
            vec![i(201), i(303), i(305), i(3)],
            vec![i(202), i(306), i(306), i(1)],
        ],
        "min/max/count exact"
    );
    assert!(join_fired(&g, src), "min/max/count join must fire");
}

/// `sum` is DECLINED for the join: a float sum is non-associative, so a different
/// fold order between the hash join and `run_streaming` could diverge, and the
/// recognizer cannot statically prove a column is integer. The join must NOT fire
/// on a `sum` aggregate; the nested fallback answers byte-identically.
#[test]
fn sum_aggregate_declines_join() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, sum(post.id) AS sm ORDER BY id";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "sum join-fallback vs general disagree");
    assert!(!join_fired(&g, src), "sum aggregate must DECLINE the join");
}

/// A GLOBAL aggregate (no grouping key) over the join — one row, order-trivial,
/// so no ORDER BY is needed. ON==OFF, and the join fires. Total joined rows = the
/// sum of the IC5 per-forum counts = 2+3+1 = 6.
#[test]
fn global_aggregate_matches_general() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN count(post) AS c";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "global aggregate join vs general disagree");
    assert_eq!(on, vec![vec![i(6)]], "global count over the join");
    assert!(join_fired(&g, src), "global aggregate join must fire");
}

/// CONVERGENCE despite differing pre-aggregation order. On the tie-heavy fixture
/// the join first-sees the three forums in a DIFFERENT order than the nested
/// loop; the ORDER-INSENSITIVE count + the TOTAL `, id` tiebreak make the output
/// converge. ON==OFF exactly (and canary #3 shows that WITHOUT the tiebreak they
/// would diverge).
#[test]
fn aggregate_and_total_order_converge_despite_join_order() {
    let g = gtie();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "tie-heavy join vs general disagree");
    // All three tie at c=1; the total id tiebreak pins ascending id order.
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)], vec![i(202), i(1)]],
        "the total-order tiebreak pins the tied groups"
    );
    assert!(join_fired(&g, src), "tie-heavy join must fire");
}

/// REL-ISO IS PER-MATCH-CLAUSE (canary #2's fixture). A 2-hop chainA and a 2-hop
/// chainB over the directed 3-cycle share an edge per shared `c`; the chainB edge
/// that also appears in chainA must NOT be excluded (side B is a FRESH clause).
/// ON==OFF keeps one joined row per c -> count 1 for each.
#[test]
fn rel_iso_is_scoped_per_match_clause() {
    let g = gcycle();
    let src = "MATCH (a:N)-[:R]->(b:N)-[:R]->(c:N) \
               MATCH (c)-[:R]->(d:N)-[:R]->(e:N) \
               RETURN c.k AS ck, count(e) AS n ORDER BY ck";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "rel-iso-per-clause join vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(0), i(1)], vec![i(1), i(1)], vec![i(2), i(1)]],
        "a chainB edge reused from chainA must be kept (rel-iso is per-clause)"
    );
    assert!(join_fired(&g, src), "the cycle join must fire");
}

// ─── DECLINES (fall back to the general path; the join must NOT fire) ──────────

/// Shapes the join DECLINES — each must fall back to `run_streaming` (ON==OFF via
/// the general path) and the join operator must NOT fire.
#[test]
fn declines_fall_back_identically_and_do_not_fire() {
    let g = gic5();
    let cases: &[&str] = &[
        // Non-aggregated multi-MATCH (raw rows — order-sensitive).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS id, post.id AS pid ORDER BY id, pid",
        // Group-by with NO ORDER BY (first-seen order is unreproducible).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS id, count(post) AS c",
        // Non-TOTAL ORDER BY (final key is the aggregate — ties break by first-seen).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS id, count(post) AS c ORDER BY c DESC",
        // No shared bound var (chainB's start is not a chainA var — a cartesian).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (x:Person)<-[:HAS_MEMBER]-(y:Forum) \
         RETURN y.id AS id, count(x) AS c ORDER BY id",
        // OPTIONAL second MATCH (claimed by the OPTIONAL recognizer, not the join).
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         OPTIONAL MATCH (forum)-[:CONTAINER_OF]->(post) \
         RETURN forum.id AS id, count(post) AS c ORDER BY id",
        // Variable-length rel in chainB.
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF*1..2]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC",
        // collect() — order-sensitive aggregate.
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS id, collect(post.id) AS ps ORDER BY id",
    ];
    for src in cases {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "declined shape vs general disagree: `{src}`");
        assert!(
            !join_fired(&g, src),
            "the join pipeline must DECLINE: `{src}`"
        );
    }
}

/// A single-MATCH aggregate (and other non-two-MATCH shapes) must NOT fire the
/// join — they are owned by the single-stage recognizers.
#[test]
fn single_match_shapes_do_not_fire_join() {
    let g = gic5();
    let cases: &[&str] = &[
        "MATCH (forum:Forum)-[:CONTAINER_OF]->(post) RETURN forum.id AS id, count(post) AS c ORDER BY id",
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) RETURN forum.id AS id ORDER BY id",
    ];
    for src in cases {
        assert!(
            !join_fired(&g, src),
            "a single-MATCH shape must not fire the join: `{src}`"
        );
    }
}

// ─── SPARSE grouping key: the point-gather fallback (the IC5 15.9s target) ─────
//
// The bug: `load_family_columns` loads a grouping-key column by a RANGE scan over
// `[min_id, max_id]` in NODE-ID space, budgeted at 4×members. When the grouping
// ids are SPARSE — a hash-join grouping by `forum.id` yields forum node ids
// scattered across the whole population, and EVERY node type carries `id` — the
// span holds far more `id` entries than the budget, so the scan DECLINES, the
// aggregate/JOIN returns None, and the whole columnar result is discarded for the
// nested general path. The fix falls back to a POINT-GATHER of exactly the
// grouping ids, byte-identical to the scan. These tests pin: SPARSE fires via the
// gather (byte-identical), DENSE still takes the range path (gather NOT invoked).

/// A named counter's value after running `src` once with the pipeline ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// A SPARSE-grouping-key IC5 fixture: the same friend/forum/post shape as
/// `gic5`, but the two forums are created FAR APART in NODE-ID space (the range
/// scan's axis) with 10 filler nodes between them. Every node carries `id`, so
/// the `[min_forum, max_forum]` NODE-ID span holds 12 `id` entries — over the
/// 2-forum grouping key's budget (4×2 = 8) — forcing `load_family_columns`' range
/// scan to DECLINE and the point-gather fallback to fire. The `id` PROPERTY
/// values (200, 900) drive the RESULT; the CREATION ORDER drives the SPARSITY
/// (node ids are assigned sequentially).
///   HAS_MEMBER (Forum->Person): f0->{p0,p1}, f1->{p1,p2}.
///   posts (forum, post, creator):
///     f0: (p0),(p1),(p2 non-member) -> count 2
///     f1: (p1),(p2),(p0 non-member) -> count 2
fn gic5_sparse() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    // Creation order sets the NODE ids. Persons, then forum f0, then a run of 10
    // fillers, then forum f1 — so the two forum node ids bracket 12 id-carrying
    // nodes and the grouping-key range scan blows its 4×2 budget.
    let p: Vec<u64> = (0..3).map(|k| mk("Person", 100 + k)).collect();
    let f0 = mk("Forum", 200);
    for k in 0..10 {
        let _ = mk("Filler", 500 + k); // fillers between the two forum node ids
    }
    let f1 = mk("Forum", 900);
    let f = [f0, f1];
    // Connected posts are created AFTER f1, so their node ids sit ABOVE the forum
    // span and do not inflate it further (the fillers already carry it over).
    let o: Vec<u64> = (0..6).map(|k| mk("Post", 1000 + k)).collect();
    let member = |fi: usize, pi: usize| {
        g.create_rel(f[fi], "HAS_MEMBER", p[pi], &BTreeMap::new())
            .expect("HAS_MEMBER");
    };
    for (fi, pi) in [(0, 0), (0, 1), (1, 1), (1, 2)] {
        member(fi, pi);
    }
    let post = |fi: usize, oi: usize, pi: usize| {
        g.create_rel(f[fi], "CONTAINER_OF", o[oi], &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(o[oi], "HAS_CREATOR", p[pi], &BTreeMap::new())
            .expect("HAS_CREATOR");
    };
    // f0: o0(p0), o1(p1), o2(p2 non-member); f1: o3(p1), o4(p2), o5(p0 non-member).
    for (fi, oi, pi) in [
        (0, 0, 0),
        (0, 1, 1),
        (0, 2, 2),
        (1, 3, 1),
        (1, 4, 2),
        (1, 5, 0),
    ] {
        post(fi, oi, pi);
    }
    g
}

/// SPARSE grouping key over the IC5 join: the two forums are far apart in node-id
/// space with fillers between, so the `forum.id` grouping-key column's range scan
/// exceeds its budget and DECLINES. The point-gather fallback then loads exactly
/// the 2 forum ids — the join FIRES and the result is byte-identical to the
/// general path. Before the fallback existed this shape declined and the whole
/// columnar join was discarded (the measured IC5 15.9s regression).
#[test]
fn ic5_sparse_grouping_key_gathers_and_fires() {
    let g = gic5_sparse();
    g.set_columnar_column_budget_factor(1); // force the sparse range-scan decline
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "sparse IC5 join vs general disagree");
    assert_eq!(
        on,
        vec![vec![i(200), i(2)], vec![i(900), i(2)]],
        "sparse IC5 exact rows + total order"
    );
    assert!(
        join_fired(&g, src),
        "the sparse IC5 join must FIRE via the point-gather fallback"
    );
    assert!(
        counter(&g, src, "graph.column point-gather") > 0,
        "the sparse grouping-key column must fall back to the point-gather"
    );
}

/// DENSE control: `gic5`'s three forums are contiguous in node-id space, so the
/// grouping-key range scan FITS its budget and the range path is taken — the
/// point-gather is NOT invoked — yet the join still fires and agrees. Pins that
/// the fallback is reached ONLY when the range scan declines.
#[test]
fn ic5_dense_grouping_key_uses_range_scan_not_gather() {
    let g = gic5();
    let src = "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS id, count(post) AS c ORDER BY c DESC, id ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "dense IC5 join vs general disagree");
    assert!(join_fired(&g, src), "the dense IC5 join must fire");
    assert_eq!(
        counter(&g, src, "graph.column point-gather"),
        0,
        "the dense grouping-key column must use the range scan, not the gather"
    );
}
