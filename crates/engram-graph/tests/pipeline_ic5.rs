#![allow(non_snake_case)]
//! Differential tests for the FULL LDBC SNB IC5 composite columnar pipeline
//! (`pipeline::run_multistage_join`): a two-stage read whose STAGE 2 is itself a
//! two-MATCH conjunctive HASH JOIN —
//!
//! ```cypher
//! MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person)
//! WHERE person <> friend
//! WITH DISTINCT friend
//! MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum)
//! MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend)
//! RETURN forum.id AS forumId, count(post) AS postCount
//! ORDER BY postCount DESC, forumId ASC LIMIT 20
//! ```
//!
//! Before this recognizer the 5-clause shape `[Match, With(distinct), Match,
//! Match, Return]` was unrecognised and the WHOLE statement declined to the
//! nested `run_streaming` path (measured: ON == OFF, the pipeline never fired).
//! This composes the EXISTING pieces — the multistage stage-1 -> WITH boundary
//! (`run_multistage`, rev 113), the set-based two-MATCH hash join (`run_join`,
//! rev 114), the point-gather column loads — the ONE new wiring being that side A
//! (chainA) is SEEDED FROM THE CARRIED SET rather than a fresh label scan.
//!
//! THE CONTRACT (every other `pipeline_*.rs`'s): for every ACCEPTED shape,
//! `set_columnar_scans(true)` (the composite pipeline) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` oracle) — the full
//! ROW SET *and its order*, byte-for-byte — and the composite must FIRE (a
//! distinct 'multistage-join runs' counter) and NOT stream. Every DECLINED shape
//! falls back (ON==OFF) and the composite must NOT fire.
//!
//! WHY the carried-set seeding is LOAD-BEARING (the canary): stage 1's var-length
//! reach set is a PROPER subset of all persons (p3, p4 are isolated in KNOWS).
//! Seeding chainA from a full Person label scan instead of the carried set would
//! let those out-of-reach friends' posts contribute — the count OVER-counts. See
//! `carried_set_restricts_the_join` for the exact before/after.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The IC5 fixture. Persons p0..p4 (id 100..104); Forums f0,f1 (id 200,201);
/// Posts po0..po4 (px 0..4).
///
///   KNOWS (Person->Person, directed): p0->p1, p1->p2. p3 and p4 are ISOLATED
///     (no KNOWS edges) — so an undirected `*1..2` from ANY start never reaches
///     them and they never reach anyone: the DISTINCT-friend reach set is
///     {p0,p1,p2}, a PROPER subset of the five persons (the canary target).
///   HAS_MEMBER (Forum->Person): f0->{p0,p1,p3}, f1->{p2,p4}.
///   CONTAINER_OF (Forum->Post) + HAS_CREATOR (Post->Person):
///     f0: po0(p0), po1(p1), po2(p3)
///     f1: po3(p2), po4(p4)
///
/// IC5 counts posts whose CREATOR is a MEMBER of the containing forum AND is in
/// the carried reach set {p0,p1,p2}:
///   f0 -> 2 (po0 by p0, po1 by p1; po2 by p3 is a member but NOT reached),
///   f1 -> 1 (po3 by p2;            po4 by p4 is a member but NOT reached).
/// A full label scan (the canary) would instead count f0 -> 3, f1 -> 2.
fn gic5() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let p: Vec<u64> = (0..5).map(|k| mk("Person", "id", 100 + k)).collect();
    let f: Vec<u64> = (0..2).map(|k| mk("Forum", "id", 200 + k)).collect();
    let o: Vec<u64> = (0..5).map(|k| mk("Post", "px", k)).collect();
    // KNOWS — p3, p4 deliberately absent (isolated).
    for (s, d) in [(0usize, 1usize), (1, 2)] {
        g.create_rel(p[s], "KNOWS", p[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    // HAS_MEMBER (Forum->Person).
    for (fi, pi) in [(0, 0), (0, 1), (0, 3), (1, 2), (1, 4)] {
        g.create_rel(f[fi], "HAS_MEMBER", p[pi], &BTreeMap::new())
            .expect("HAS_MEMBER");
    }
    // (forum, post, creator).
    for (fi, oi, pi) in [(0, 0, 0), (0, 1, 1), (0, 2, 3), (1, 3, 2), (1, 4, 4)] {
        g.create_rel(f[fi], "CONTAINER_OF", o[oi], &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(o[oi], "HAS_CREATOR", p[pi], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse '{src}': {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run '{src}': {e}"))
        .rows
}

/// Run `src` with the composite pipeline ON, then the general path OFF (oracle).
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

/// The value of a named `counted!` counter after running `src` with columnar ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// Whether the FULL-IC5 composite pipeline fired for `src` (columnar ON).
fn msj_fired(g: &Graph, src: &str) -> bool {
    counter(g, src, "interp.pipeline multistage-join runs") == 1
}

/// Whether `src` fell to the nested `run_streaming` path (columnar ON) — the
/// marker the composite must NOT trip on an accept, and MUST on a full decline.
fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// THE FULL IC5 STATEMENT end-to-end: var-length stage 1 -> WHERE person<>friend
/// -> WITH DISTINCT friend -> two-MATCH stage-2 JOIN -> count(post) -> ORDER BY
/// count DESC, id ASC LIMIT. ON==OFF exactly (rows AND order); the composite
/// fires and does NOT stream.
#[test]
fn full_ic5_on_equals_off_and_fires() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "IC5 ON must equal OFF row-for-row and in order");
    assert_eq!(
        on,
        vec![vec![i(200), i(2)], vec![i(201), i(1)]],
        "IC5 counts posts by a carried friend who is a member of the forum"
    );
    assert!(
        msj_fired(&g, src),
        "the full IC5 composite pipeline must FIRE"
    );
    assert!(
        !streamed(&g, src),
        "the full IC5 statement must NOT fall to run_streaming"
    );
}

/// The var-length stage 1 replaced by a FIXED hop (no BFS) into the SAME two-MATCH
/// stage-2 join — the composite still fires (varlen is not required). Directed
/// KNOWS from all persons reaches {p1,p2}; WITH DISTINCT friend = {p1,p2}.
///   f0: only po1 (by p1, member) kept -> 1; f1: only po3 (by p2, member) -> 1.
#[test]
fn fixed_hop_stage1_into_the_join_fires() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS]->(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "fixed-hop IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)]],
        "fixed-hop stage-1 reach set {{p1,p2}} restricts the join"
    );
    assert!(msj_fired(&g, src), "the fixed-hop composite must FIRE");
    assert!(!streamed(&g, src), "the fixed-hop IC5 must NOT stream");
}

/// MIN / MAX / COUNT over a side-B column, grouped by forum, ordered by the
/// grouping key. All order-insensitive -> ON==OFF, and the composite fires.
/// Carried friends {p0,p1,p2}: f0 keeps po0(px0),po1(px1); f1 keeps po3(px3).
#[test]
fn min_max_count_over_the_composite() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, min(post.px) AS mn, max(post.px) AS mx, \
               count(post) AS c ORDER BY forumId";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "min/max/count IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![
            vec![i(200), i(0), i(1), i(2)],
            vec![i(201), i(3), i(3), i(1)],
        ],
        "min/max/count over the carried-friend posts per forum"
    );
    assert!(msj_fired(&g, src), "min/max/count composite must FIRE");
    assert!(!streamed(&g, src), "min/max/count IC5 must NOT stream");
}

/// A GLOBAL aggregate over the composite (no grouping key) — one row,
/// order-trivial, no ORDER BY. Total kept posts = 2 (f0) + 1 (f1) = 3.
#[test]
fn global_aggregate_over_the_composite() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN count(post) AS c";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "global-aggregate IC5 ON must equal OFF");
    assert_eq!(on, vec![vec![i(3)]], "3 posts by a carried, member friend");
    assert!(msj_fired(&g, src), "global-aggregate composite must FIRE");
    assert!(!streamed(&g, src), "global-aggregate IC5 must NOT stream");
}

// ─── DECLINES (fall back; the composite must NOT fire) ─────────────────────────

/// A stage-2 that is a SINGLE MATCH still routes via the rev-113 single-stage
/// multistage path — the composite must NOT claim it. ON==OFF, and the
/// 'multistage runs' counter fires while 'multistage-join runs' does not.
#[test]
fn single_stage2_match_routes_via_multistage_not_the_join() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               RETURN forum.id AS forumId, count(friend) AS c \
               ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "single-stage-2 ON must equal OFF");
    assert!(
        !msj_fired(&g, src),
        "the composite must DECLINE a single stage-2 MATCH"
    );
    assert_eq!(
        counter(&g, src, "interp.pipeline multistage runs"),
        1,
        "a single stage-2 MATCH must fire the rev-113 multistage path"
    );
}

/// A NON-order-safe RETURN (a grouped aggregate with a partial ORDER BY — no
/// total tiebreak) declines the COMPOSITE. Since W3's clause fusion, the two
/// trailing MATCHes are re-offered FUSED once every first-pass recogniser
/// declined, and the fused tail is claimed by a pipeline path — with answers
/// (order included) identical to the general path, which is the assertion
/// that matters. The composite's own decline is still pinned.
#[test]
fn non_order_safe_return_declines() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount ORDER BY postCount DESC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "non-order-safe ON must equal OFF");
    assert!(
        !msj_fired(&g, src),
        "a non-total ORDER BY must DECLINE the composite"
    );
    assert_eq!(
        counter(&g, src, "interp.consecutive matches fused for the recognisers"),
        1,
        "the trailing MATCHes are re-offered fused after the composite declines"
    );
}

/// A THREE-stage read (`WITH … MATCH … WITH … MATCH … RETURN`) is out of the
/// 5-clause shape — the composite declines and the whole query streams. ON==OFF.
#[test]
fn three_stage_read_declines() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               WITH DISTINCT forum \
               MATCH (forum)-[:CONTAINER_OF]->(post) \
               RETURN forum.id AS forumId, count(post) AS c \
               ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "three-stage ON must equal OFF");
    assert!(
        !msj_fired(&g, src),
        "a three-stage read must DECLINE the composite"
    );
}

/// A MULTI-VAR carry (`WITH friend, r`) declines — the composite requires exactly
/// ONE carried var (a single-var seed of chainA is byte-identical to the
/// per-carried-tuple general path only when the carried tuple IS that one var).
/// It falls back; ON==OFF; the composite must NOT fire.
#[test]
fn multi_var_carry_declines() {
    let g = gic5();
    let src = "MATCH (person:Person)-[r:KNOWS]->(friend:Person) \
               WHERE person <> friend \
               WITH friend, r \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS c \
               ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "multi-var-carry ON must equal OFF");
    assert!(
        !msj_fired(&g, src),
        "a multi-var carry must DECLINE the composite"
    );
}

/// The single-stage and plain-join shapes must NOT be mis-claimed as the
/// composite (no false firing).
#[test]
fn other_shapes_do_not_fire_the_composite() {
    let g = gic5();
    let cases: &[&str] = &[
        // A plain two-MATCH join (no WITH) — owned by recognise_join.
        "MATCH (friend:Person)<-[:HAS_MEMBER]-(forum:Forum) \
         MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
         RETURN forum.id AS forumId, count(post) AS c ORDER BY c DESC, forumId ASC",
        // A single-stage aggregate.
        "MATCH (forum:Forum)-[:CONTAINER_OF]->(post) RETURN forum.id AS id, count(post) AS c ORDER BY id",
    ];
    for src in cases {
        assert!(
            !msj_fired(&g, src),
            "a non-composite shape must not fire the composite: '{src}'"
        );
    }
}

// ─── CANARY (documented; see the module header) ────────────────────────────────

/// The carried-set restriction is LOAD-BEARING. This asserts the ORACLE result
/// (over the carried reach set {p0,p1,p2}) is STRICTLY LESS than the full-scan
/// result would be — the exact divergence the canary provokes. If
/// `run_multistage_join` seeded chainA from a full Person label scan instead of
/// the carried set, the ON count would be the full-scan value below and this
/// oracle differential (ON==OFF at the carried counts) would FAIL.
#[test]
fn carried_set_restricts_the_join() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "carried-set IC5 ON must equal OFF");
    // The carried reach set is a PROPER subset — the counts are BELOW the full
    // label-scan counts ([[200,3],[201,2]]), which is exactly what seeding chainA
    // from the carried set (not a full scan) buys. The canary flips these to the
    // full-scan values.
    assert_eq!(
        on,
        vec![vec![i(200), i(2)], vec![i(201), i(1)]],
        "the carried set {{p0,p1,p2}} excludes p3, p4 from the count"
    );
    assert!(msj_fired(&g, src), "the carried-set composite must FIRE");
}

/// Anchor-pushdown at composite scale: a source WHERE (`person.id = 0`) MUST
/// prune the var-length BFS's SOURCES before it expands, or stage 1 runs the BFS
/// from EVERY person. In the composite (var-length -> WITH DISTINCT -> join ->
/// agg) that overflows the row budget on ON while the anchored general path
/// (which seeds the anchor) fits — a byte-identity break (ON errors, OFF
/// succeeds) the small differential fixtures cannot show. Reproduced: a 10-person
/// KNOWS clique (whole-label BFS ~90 rows) under a row budget of 40; the single
/// anchored source (9 friends + a small join) fits. Without the pushdown, ON
/// overflows and this test's `.expect` panics.
#[test]
fn anchor_pushdown_keeps_the_composite_under_budget() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, id: i64| {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.to_string()], &p).expect("node")
    };
    let persons: Vec<u64> = (0..10i64).map(|k| mk("Person", k)).collect();
    for &a in &persons {
        for &b in &persons {
            if a != b {
                g.create_rel(a, "KNOWS", b, &BTreeMap::new())
                    .expect("KNOWS");
            }
        }
    }
    for (k, &p) in persons.iter().enumerate() {
        let f = mk("Forum", 200 + k as i64);
        let post = mk("Post", 300 + k as i64);
        g.create_rel(f, "HAS_MEMBER", p, &BTreeMap::new())
            .expect("HAS_MEMBER");
        g.create_rel(f, "CONTAINER_OF", post, &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(post, "HAS_CREATOR", p, &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) WHERE person.id = 0 \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS fid, count(post) AS c ORDER BY c DESC, fid ASC";
    assert!(
        msj_fired(&g, src),
        "the composite must fire (var-length -> WITH DISTINCT -> join)"
    );
    g.set_row_budget(Some(40));
    g.set_columnar_scans(true);
    let on = run_query(&g, &parse_statement(src).unwrap(), BTreeMap::new())
        .expect("ON must not overflow: the anchor is pushed before the BFS")
        .rows;
    g.set_columnar_scans(false);
    let off = run_query(&g, &parse_statement(src).unwrap(), BTreeMap::new())
        .expect("OFF anchors and fits")
        .rows;
    g.set_row_budget(None);
    g.set_columnar_scans(true);
    assert_eq!(on, off, "anchor-pushdown composite ON must equal OFF");
    assert!(
        !on.is_empty(),
        "the anchored person reaches forums with posts"
    );
}

// ─── GAP 1/2: anchored stage-1 seed + conjunctive WHERE ────────────────────────
//
// The LITERAL LDBC IC5 anchors its start — `(person:Person {id:$personId})` — and
// its WHERE is a CONJUNCTION (`person <> friend` alongside that anchor). These
// exercise: (Gap 1) an inline `{prop: val}` OR a `person.prop = val` WHERE
// equality becoming a range-index-seeded scan whose result equals a label scan
// then filter; (Gap 2) a top-level `AND` WHERE split into per-predicate filters,
// each applied as early as its vars bind. The anchor is LOAD-BEARING: on `gic5()`
// the whole-label reach set is {p0,p1,p2} (counts [[200,2],[201,1]], asserted by
// `full_ic5_on_equals_off_and_fires`), while anchoring stage 1 to ONE person is a
// PROPER SUBSET whose counts are strictly below it.

/// Param-aware `msj_fired` — the composite fired for `src` under `params`.
fn msj_fired_p(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .counters()
        .get("interp.pipeline multistage-join runs")
        .copied()
        .unwrap_or(0)
        == 1
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// THE LITERAL IC5 with an INLINE `{id: <lit>}` start anchor + `WHERE person <>
/// friend`. Anchoring stage 1 to p0 restricts the reach set to {p1,p2} (p0 itself
/// dropped by `person <> friend`), a PROPER SUBSET of the whole-label {p0,p1,p2},
/// so f0 counts only po1(p1) not po0(p0): [[200,1],[201,1]] — strictly below the
/// whole-label [[200,2],[201,1]]. ON==OFF exactly; the composite FIRES; no stream.
#[test]
fn full_ic5_inline_anchor_fires() {
    let g = gic5();
    let src = "MATCH (person:Person {id: 100})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "inline-anchor IC5 ON must equal OFF row-for-row and order"
    );
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)]],
        "anchoring stage 1 to p0 restricts the reach set to {{p1,p2}}"
    );
    assert!(
        msj_fired(&g, src),
        "the inline-anchor IC5 composite must FIRE"
    );
    assert!(!streamed(&g, src), "the inline-anchor IC5 must NOT stream");
}

/// The SAME literal IC5 but with a `$param` inline anchor `{id: $pid}` — the value
/// resolves from `params` at seed time (a param, not a literal). ON==OFF; fires.
#[test]
fn full_ic5_param_anchor_fires() {
    let g = gic5();
    let src = "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let mut params = BTreeMap::new();
    params.insert("pid".to_string(), i(100));
    let (on, off) = both(&g, src, params.clone());
    assert_eq!(on, off, "param-anchor IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)]],
        "the $pid=100 anchor restricts the reach set to {{p1,p2}}"
    );
    assert!(
        msj_fired_p(&g, src, params),
        "the param-anchor IC5 composite must FIRE"
    );
}

/// The WHERE-CONJUNCTION form (no inline prop): `WHERE person.id = X AND person <>
/// friend`. The `person.id = 100` conjunct is a source-var pred applied BEFORE the
/// BFS (same restriction as the inline anchor); `person <> friend` after the hop.
/// Byte-identical to the inline form: ON==OFF at [[200,1],[201,1]]; fires; no stream.
#[test]
fn full_ic5_where_conjunction_anchor_fires() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person.id = 100 AND person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "where-conjunction IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)]],
        "the person.id=100 conjunct anchors stage 1 to {{p1,p2}}"
    );
    assert!(
        msj_fired(&g, src),
        "the where-conjunction IC5 composite must FIRE"
    );
    assert!(
        !streamed(&g, src),
        "the where-conjunction IC5 must NOT stream"
    );
}

// ─── PROPER SUBSET + the anchor-seed canary target ─────────────────────────────

/// The anchored reach is a PROPER SUBSET of the whole label. Anchoring to p1
/// reaches {p0,p2} (with `person <> friend`) — the counts [[200,1],[201,1]] are
/// STRICTLY BELOW the whole-label [[200,2],[201,1]] (asserted by
/// `full_ic5_on_equals_off_and_fires`). A missing anchor would let p1's own post
/// (po1 in f0) and p0/p2's extra posts leak in — a RESULT change, not just speed.
/// THE ANCHOR-SEED CANARY (distinct from the rev-119 BUDGET canary): drop the
/// anchor so stage 1 seeds the WHOLE Person label — ON becomes [[200,2],[201,1]]
/// while OFF stays [[200,1],[201,1]], so this ON==OFF differential FAILS. Verified
/// break/restore during implementation (see the report).
#[test]
fn anchor_seed_is_a_proper_subset_of_the_label() {
    let g = gic5();
    let src = "MATCH (person:Person {id: 101})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "proper-subset IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![vec![i(200), i(1)], vec![i(201), i(1)]],
        "anchoring to p1 reaches only {{p0,p2}} — below the whole-label counts"
    );
    assert!(msj_fired(&g, src), "the proper-subset composite must FIRE");
}

/// The anchor SEEDS the range index (not a whole-label scan) when the label is
/// above the seek floor (600 > 512 Persons). Only persons 3,4,5 have KNOWS edges
/// (a short chain); the rest are isolated, so `{id: 3}` reaches {4,5} and each
/// carried friend contributes its own self-forum post: [[1004,1],[1005,1]].
/// ON==OFF, the composite fires, AND the property-index seed counter fired.
#[test]
fn anchor_seeds_the_index_above_the_floor() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let persons: Vec<u64> = (0..600i64).map(|k| mk("Person", "id", k)).collect();
    for (s, d) in [(3usize, 4usize), (4, 5)] {
        g.create_rel(persons[s], "KNOWS", persons[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    for (k, &p) in persons.iter().enumerate() {
        let f = mk("Forum", "id", 1000 + k as i64);
        let post = mk("Post", "px", k as i64);
        g.create_rel(f, "HAS_MEMBER", p, &BTreeMap::new())
            .expect("HAS_MEMBER");
        g.create_rel(f, "CONTAINER_OF", post, &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(post, "HAS_CREATOR", p, &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    let src = "MATCH (person:Person {id: 3})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "index-seeded anchor IC5 ON must equal OFF");
    assert_eq!(
        on,
        vec![vec![i(1004), i(1)], vec![i(1005), i(1)]],
        "person 3 reaches {{4,5}}, each contributing its own self-forum post"
    );
    assert!(msj_fired(&g, src), "the index-seeded composite must FIRE");
    assert!(
        counter(
            &g,
            src,
            "interp.pipeline anchored seed sought a property index"
        ) >= 1,
        "above the seek floor the anchor must SEED the range index, not scan"
    );
}

// ─── DECLINES (fall back; the composite must NOT fire) ─────────────────────────

/// A TWO-PROP inline anchor `{id: 100, px: 0}` is not a single point equality this
/// seeds and filters byte-identically — `collect_hops` declines the whole query to
/// the general path. ON==OFF; the composite must NOT fire.
#[test]
fn two_prop_inline_anchor_declines() {
    let g = gic5();
    let src = "MATCH (person:Person {id: 100, px: 0})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS c ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "two-prop-anchor ON must equal OFF (both fall back)"
    );
    assert!(
        !msj_fired(&g, src),
        "a two-prop inline anchor must DECLINE the composite"
    );
}

/// An inline anchor on a NON-SCALAR value `{id: [100]}` — the index cannot serve a
/// list value and this recognizer does not promise byte-identity for it — declines
/// the whole query. ON==OFF; the composite must NOT fire.
#[test]
fn non_scalar_inline_anchor_declines() {
    let g = gic5();
    let src = "MATCH (person:Person {id: [100]})-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS c ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "non-scalar-anchor ON must equal OFF (both fall back)"
    );
    assert!(
        !msj_fired(&g, src),
        "a non-scalar inline anchor must DECLINE the composite"
    );
}

/// A top-level `OR` WHERE (`person.id = 100 OR person <> friend`) is not a
/// conjunction of tractable predicates — `recognise_where_preds` declines the
/// whole query. ON==OFF; the composite must NOT fire.
#[test]
fn or_where_declines() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person.id = 100 OR person <> friend \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS c ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "or-where ON must equal OFF (both fall back)");
    assert!(
        !msj_fired(&g, src),
        "a top-level OR WHERE must DECLINE the composite"
    );
}

/// A conjunct that is NEITHER a single-var prop pred NOR a two-var id pred — here a
/// two-var ARITHMETIC predicate `person.id + friend.id > 0` — declines the whole
/// conjunction. ON==OFF; the composite must NOT fire.
#[test]
fn neither_shape_conjunct_declines() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person.id = 100 AND person.id + friend.id > 0 \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS c ORDER BY c DESC, forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "neither-shape-conjunct ON must equal OFF (both fall back)"
    );
    assert!(
        !msj_fired(&g, src),
        "a conjunct that is neither shape must DECLINE the composite"
    );
}

// ─── GROUP-KEY PROPERTY GATHER (materialise the props, not the group entity) ────

/// The aggregating RETURN reads its group key ONLY through a property
/// (`forum.id`), so the group projection now GATHERS that column instead of
/// materialising the whole Forum node per group. Before this, the grouped RETURN
/// decoded one full Forum node per group purely to read the group key — the
/// identical GLOBAL-count query decodes ZERO, which is the gap this closes.
/// Asserts ON==OFF, that full-node materialisation dropped to the global
/// aggregate's own ZERO baseline, and that the group-key gather fired.
#[test]
fn group_key_props_gathered_not_the_whole_node() {
    let g = gic5();
    let grouped = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY postCount DESC, forumId ASC LIMIT 20";
    let global = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN count(post) AS c";
    let (on, off) = both(&g, grouped, BTreeMap::new());
    assert_eq!(on, off, "gathered group-key IC5 ON must equal OFF");
    assert_eq!(on, vec![vec![i(200), i(2)], vec![i(201), i(1)]]);
    // The GLOBAL aggregate of the identical chain decodes ZERO full nodes — the
    // baseline the grouped projection must now match (the whole rest of the
    // pipeline reads through columns, never full-node decodes).
    assert_eq!(
        counter(&g, global, "graph.nodes materialised in full"),
        0,
        "the global aggregate decodes no full nodes"
    );
    // The grouped projection now decodes ZERO too — the per-group full-node
    // 'template' is gone (it was one Forum decode per group before this change).
    assert_eq!(
        counter(&g, grouped, "graph.nodes materialised in full"),
        0,
        "the grouped projection gathers forum.id, it never decodes the Forum node"
    );
    // ... and the group-key column gather is what ran in its place.
    assert_eq!(
        counter(&g, grouped, "interp.agg group-key props gathered"),
        1,
        "the group-key property gather fired for the group projection"
    );
}

/// FALLBACK: a RETURN that carries the group-key NODE as a WHOLE entity
/// (`RETURN forum`) cannot be served from a property Map — it MUST still
/// materialise the Forum node once per group, byte-identically to before.
/// ON==OFF; the per-group full-node decode is RETAINED (two forums -> two
/// decodes); and the gather does NOT fire.
#[test]
fn whole_node_group_key_still_materialises() {
    let g = gic5();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person <> friend WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum AS f, count(post) AS c ORDER BY c DESC, f ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "whole-node group-key ON must equal OFF");
    assert_eq!(
        counter(&g, src, "graph.nodes materialised in full"),
        2,
        "'RETURN forum' keeps the per-group full-node materialisation (one per forum)"
    );
    assert_eq!(
        counter(&g, src, "interp.agg group-key props gathered"),
        0,
        "a whole-entity group-key use must NOT gather"
    );
}

// ─── JOIN-ORDERING: chainB re-rooted at the carried var ─────────────────────────
//
// ChainB `(forum)-CONTAINER_OF->(post)-HAS_CREATOR->(friend)` seeded from the
// shared start (forum) materialises EVERY post of every member forum, then the
// hash join drops the posts whose creator is not carried. Because the carried var
// (`friend`) ALSO appears in chainB (its terminal), chainB is re-rooted there —
// `(friend)<-HAS_CREATOR-(post)<-CONTAINER_OF-(forum)`, seeded from the DISTINCT
// carried set — expanding O(carried friends' posts) instead of O(member-forum
// posts). The hash join on the shared `(friend, forum)` is unchanged, so the row
// set is byte-identical; only chainB's seed + expansion order changed. The reroot
// counter distinguishes the two paths.

/// The reroot-counter key: incremented once when the carried-var-rooted chainB
/// path runs (0 when the forward forum-rooted chainB runs).
const REROOT_KEY: &str = "interp.pipeline join rerooted from carried";

/// A fixture where a carried friend authored a post in a forum they ARE a member
/// of AND in a forum they are NOT a member of, and forums also hold posts by a
/// NON-friend member — the three cases the join must resolve.
///
///   Persons (id): me=10, F1=11, F2=12 (carried friends); NF=13 (non-friend).
///   Forums (id): FA=20, FB=21.  Posts (px): X=0, Y=1, Z=2, W=3.
///   KNOWS (directed): me->F1, F1->F2 — undirected *1..2 from `me` reaches
///     {F1,F2} (me itself is excluded: the only path back reuses the me-F1 edge).
///   HAS_MEMBER (Forum->Person): FA->{F1,NF}, FB->{F2}. (F1 is NOT a member of FB.)
///   (forum, post, creator): X=(FA,F1), Y=(FB,F1), Z=(FA,NF), W=(FB,F2).
///
/// IC5 counts posts whose CREATOR is a carried friend AND a MEMBER of the forum:
///   FA -> 1 (X by F1, a member; Z by NF is a member but NOT a friend),
///   FB -> 1 (W by F2, a member; Y by F1 is a friend but NOT a member of FB).
fn gic5_reroot() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let me = mk("Person", "id", 10);
    let f1 = mk("Person", "id", 11);
    let f2 = mk("Person", "id", 12);
    let nf = mk("Person", "id", 13);
    let fa = mk("Forum", "id", 20);
    let fb = mk("Forum", "id", 21);
    let po: Vec<u64> = (0..4).map(|k| mk("Post", "px", k)).collect();
    for (s, d) in [(me, f1), (f1, f2)] {
        g.create_rel(s, "KNOWS", d, &BTreeMap::new())
            .expect("KNOWS");
    }
    for (fo, pe) in [(fa, f1), (fa, nf), (fb, f2)] {
        g.create_rel(fo, "HAS_MEMBER", pe, &BTreeMap::new())
            .expect("HAS_MEMBER");
    }
    // (forum, post, creator): X=(FA,F1), Y=(FB,F1), Z=(FA,NF), W=(FB,F2).
    for (fo, oi, cr) in [(fa, 0, f1), (fb, 1, f1), (fa, 2, nf), (fb, 3, f2)] {
        g.create_rel(fo, "CONTAINER_OF", po[oi], &BTreeMap::new())
            .expect("CONTAINER_OF");
        g.create_rel(po[oi], "HAS_CREATOR", cr, &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

/// The literal (anchored) IC5 on `gic5_reroot`: the carried var is chainB's
/// terminal, so the carried-var-rooted chainB runs. ON==OFF exactly; the composite
/// fires; the reroot counter is 1; the result EXCLUDES the non-member-forum post
/// (Y: F1 authored it in FB but is not a member of FB) and the non-friend post
/// (Z: NF is a member of FA but is not carried).
#[test]
fn reroot_at_carried_var_excludes_nonmember_and_nonfriend_posts() {
    let g = gic5_reroot();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person.id = 10 \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post)-[:HAS_CREATOR]->(friend) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "reroot IC5 ON must equal OFF row-for-row and order"
    );
    assert_eq!(
        on,
        vec![vec![i(20), i(1)], vec![i(21), i(1)]],
        "each forum counts ONLY its carried-member creators' posts (Y and Z excluded)"
    );
    assert!(msj_fired(&g, src), "the reroot composite must FIRE");
    assert!(!streamed(&g, src), "the reroot IC5 must NOT stream");
    assert_eq!(
        counter(&g, src, REROOT_KEY),
        1,
        "the carried-var-rooted chainB path must be taken (anchored + expressible)"
    );
}

/// ChainB that does NOT reference the carried var — `(forum)-CONTAINER_OF->(post)`
/// counts every post in each member forum, sharing only `forum` with chainA. The
/// carried var is not re-rootable in chainB, so the recognizer falls back to the
/// forward forum-rooted chainB: ON==OFF, the composite fires, and the reroot
/// counter is 0.
#[test]
fn chainb_without_carried_var_falls_back_to_forum_root() {
    let g = gic5_reroot();
    let src = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
               WHERE person.id = 10 \
               WITH DISTINCT friend \
               MATCH (friend)<-[:HAS_MEMBER]-(forum:Forum) \
               MATCH (forum)-[:CONTAINER_OF]->(post) \
               RETURN forum.id AS forumId, count(post) AS postCount \
               ORDER BY forumId ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "fallback IC5 ON must equal OFF");
    // FA holds X,Z (2 posts); FB holds Y,W (2 posts) — every post in each member
    // forum, since chainB no longer restricts by creator.
    assert_eq!(
        on,
        vec![vec![i(20), i(2)], vec![i(21), i(2)]],
        "without the friend join key every post in a member forum is counted"
    );
    assert!(msj_fired(&g, src), "the fallback composite must FIRE");
    assert_eq!(
        counter(&g, src, REROOT_KEY),
        0,
        "chainB lacks the carried var, so the forward forum-rooted path runs"
    );
}
