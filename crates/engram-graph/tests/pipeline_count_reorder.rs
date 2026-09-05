#![allow(non_snake_case)]
//! Differential tests for the COUNT-ONLY JOIN REORDER (operator C of
//! `docs/lsqb-completeness-plan.md`, `pipeline::reorder_for_count_only`): a
//! MATCH whose RETURN is nothing but `count(*)` produces ONE row whose content
//! is fixed however the pattern is walked, so its paths may be re-rooted,
//! re-ordered and reversed to reach a plan the count fold can take.
//!
//! The contract is the other `pipeline_*` suites': for every accepted shape the
//! TRIPLE must agree — the reorder ON (the default), the reorder OFF (the
//! pattern planned exactly as written), and columnar OFF (the per-tuple
//! `run_streaming`) — byte-for-byte; and every declined shape falls back and
//! still agrees.
//!
//! WHAT IS PINNED HERE:
//!   - it fires ONLY on the gated shape: a keyed count, an ORDER BY, a LIMIT, a
//!     SKIP, `count(var)`, `count(DISTINCT …)` and a DISTINCT projection must
//!     all leave the counter at zero;
//!   - LABEL STAMPING is load-bearing and semantics-preserving: q3's dropped
//!     `(country:Country)` path is the only place that label is written, and the
//!     count still excludes a same-shaped chain through a NON-Country place;
//!   - the BARE-PATH DROP: a hopless path whose var another path binds is
//!     dropped; one whose var nothing else binds is a cartesian factor and
//!     DECLINES;
//!   - a REVERSED path's relationship isomorphism, over parallel edges and a
//!     self-loop — a reversed walk visits the same relationship SET, so it must
//!     count the same;
//!   - the q2 and q3 shapes both FOLD after the rewrite and agree with the
//!     general path;
//!   - the admission rule: a pattern that already materialises as few columns
//!     keeps its source order (no plan churn), pinned by the counter.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const REORDER: &str = "pipeline.count-only reordered";
const FOLD: &str = "interp.pipeline count fold";

// ─── Fixtures ────────────────────────────────────────────────────────────────

/// The q3 fixture: three persons in ONE country forming a KNOWS triangle, two
/// in another with no triangle, and three in a place that is NOT a Country
/// (a `Region`) that DO form one — the label-stamping canary, since the only
/// place `country` is labelled is the bare `(country:Country)` path the rewrite
/// drops.
///
/// Country co0 ← City ci0, ci1; Country co1 ← City ci2; Region rg0 ← City ci3.
/// Persons: pe0,pe1 → ci0 and pe2 → ci1 (all co0); pe3,pe4 → ci2 (co1);
/// pe5,pe6,pe7 → ci3 (rg0).
/// KNOWS: the co0 triangle pe0-pe1-pe2, the co1 edge pe3-pe4, the rg0 triangle
/// pe5-pe6-pe7, and a cross-country pe0-pe3.
fn gq3() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let e = BTreeMap::new();
    let co: Vec<u64> = (0..2).map(|i| mk("Country", "ck", i)).collect();
    let rg = mk("Region", "ck", 9);
    let ci: Vec<u64> = (0..4).map(|i| mk("City", "yk", i)).collect();
    let pe: Vec<u64> = (0..8).map(|i| mk("Person", "pk", i)).collect();
    for (city, place) in [(0, co[0]), (1, co[0]), (2, co[1]), (3, rg)] {
        g.create_rel(ci[city], "IS_PART_OF", place, &e)
            .expect("IS_PART_OF");
    }
    for (person, city) in [
        (0, 0),
        (1, 0),
        (2, 1),
        (3, 2),
        (4, 2),
        (5, 3),
        (6, 3),
        (7, 3),
    ] {
        g.create_rel(pe[person], "IS_LOCATED_IN", ci[city], &e)
            .expect("IS_LOCATED_IN");
    }
    for (a, b) in [(0, 1), (1, 2), (2, 0), (3, 4), (5, 6), (6, 7), (7, 5), (0, 3)] {
        g.create_rel(pe[a], "KNOWS", pe[b], &e).expect("KNOWS");
    }
    g
}

/// The q2 fixture: a social graph with the rel-iso hazards a REVERSED path must
/// still enforce — a KNOWS self-loop on pe0 and PARALLEL KNOWS edges pe0→pe1.
///
/// KNOWS: pe0-pe0 (self), pe0-pe1 TWICE, pe1-pe2, pe2-pe0, pe3-pe4, pe0-pe3.
/// HAS_CREATOR: cm0,cm1 → pe0; cm2 → pe1; cm3 → pe2; po0 → pe1; po1 → pe2;
/// po2 → pe0.
/// REPLY_OF: cm0→po0, cm1→po1, cm2→po2, cm3→po0.
fn gq2() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let e = BTreeMap::new();
    let pe: Vec<u64> = (0..5).map(|i| mk("Person", "pk", i)).collect();
    let cm: Vec<u64> = (0..4).map(|i| mk("Comment", "ck", i)).collect();
    let po: Vec<u64> = (0..3).map(|i| mk("Post", "ok", i)).collect();
    for (a, b) in [(0, 0), (0, 1), (0, 1), (1, 2), (2, 0), (3, 4), (0, 3)] {
        g.create_rel(pe[a], "KNOWS", pe[b], &e).expect("KNOWS");
    }
    for (c, p) in [(0, 0), (1, 0), (2, 1), (3, 2)] {
        g.create_rel(cm[c], "HAS_CREATOR", pe[p], &e)
            .expect("HAS_CREATOR");
    }
    for (o, p) in [(0, 1), (1, 2), (2, 0)] {
        g.create_rel(po[o], "HAS_CREATOR", pe[p], &e)
            .expect("HAS_CREATOR");
    }
    for (c, o) in [(0, 0), (1, 1), (2, 2), (3, 0)] {
        g.create_rel(cm[c], "REPLY_OF", po[o], &e).expect("REPLY_OF");
    }
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

/// The TRIPLE: reorder ON (columnar), reorder OFF (columnar), columnar OFF.
fn triple(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_only_reorder(true);
    let on = rows(g, src);
    engram_graph::pipeline::set_count_only_reorder(false);
    let source_order = rows(g, src);
    engram_graph::pipeline::set_count_only_reorder(true);
    g.set_columnar_scans(false);
    let general = rows(g, src);
    g.set_columnar_scans(true);
    (on, source_order, general)
}

fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_only_reorder(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// Every path agrees, and the reorder FIRED.
fn agrees_and_fires(g: &Graph, src: &str) -> Rows {
    let (on, source_order, general) = triple(g, src);
    assert_eq!(on, general, "reorder ON vs general disagree: `{src}`");
    assert_eq!(
        source_order, general,
        "reorder OFF vs general disagree: `{src}`"
    );
    assert_eq!(counter(g, src, REORDER), 1, "the reorder did not fire: `{src}`");
    on
}

/// Every path agrees, and the reorder DECLINED (the source order stands).
fn declines_but_agrees(g: &Graph, src: &str) -> Rows {
    let (on, source_order, general) = triple(g, src);
    assert_eq!(on, general, "columnar vs general disagree: `{src}`");
    assert_eq!(
        source_order, general,
        "reorder OFF vs general disagree: `{src}`"
    );
    assert_eq!(
        counter(g, src, REORDER),
        0,
        "the reorder should have DECLINED: `{src}`"
    );
    on
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

// ─── q3: the hopless start, the bare-path drop, label stamping ───────────────

/// LSQB q3's shape — four MATCH clauses fused into one five-path pattern whose
/// FIRST path is the hopless `(country:Country)` that `collect_hops` refuses.
/// The rewrite drops that path (its label stamped onto every `country`
/// occurrence), seeds the smallest label, and reaches a plan that FOLDS.
#[test]
fn q3_shape_drops_the_bare_path_and_folds() {
    let g = gq3();
    let q3 = "MATCH (country:Country) \
              MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
              MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
              MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
              MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
              RETURN count(*) AS n";
    // Only the co0 triangle {pe0, pe1, pe2} is a KNOWS triangle whose three
    // members share a COUNTRY: 3! = 6 orderings, each over three distinct
    // relationships. The rg0 triangle {pe5, pe6, pe7} shares a REGION, which
    // the stamped `:Country` excludes; pe3/pe4 have no triangle.
    assert_eq!(agrees_and_fires(&g, q3), vec![vec![i(6)]], "q3 count");
    assert_eq!(counter(&g, q3, FOLD), 1, "the rewritten q3 must FOLD");
    // The same pattern already comma-joined in ONE MATCH takes the same route.
    let q3_commas =
        "MATCH (country:Country), \
         (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country), \
         (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country), \
         (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country), \
         (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
         RETURN count(*) AS n";
    assert_eq!(agrees_and_fires(&g, q3_commas), vec![vec![i(6)]]);
    assert_eq!(counter(&g, q3_commas, FOLD), 1);
}

/// LABEL STAMPING is what makes the bare-path drop safe: `:Country` is written
/// ONLY on the dropped path, so without the stamp the rewritten `(country)`
/// would admit the Region as well. The count proves it does not — and the
/// SAME query with `:Country` written out on every occurrence counts the same.
#[test]
fn label_stamping_keeps_the_dropped_paths_constraint() {
    let g = gq3();
    let stamped_by_hand =
        "MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country:Country), \
         (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country:Country), \
         (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country:Country), \
         (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
         RETURN count(*) AS n";
    let by_hand = triple(&g, stamped_by_hand);
    assert_eq!(by_hand.0, by_hand.2);
    assert_eq!(by_hand.0, vec![vec![i(6)]], "the by-hand stamping counts 6");
    // …and with NO label on the place at all the Region's triangle IS counted,
    // so the label the stamp carries is provably load-bearing (6 + 6 = 12).
    let unlabelled =
        "MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country), \
         (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country), \
         (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country), \
         (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
         RETURN count(*) AS n";
    let (on, source_order, general) = triple(&g, unlabelled);
    assert_eq!(on, general);
    assert_eq!(source_order, general);
    assert_eq!(on, vec![vec![i(12)]], "unlabelled admits the Region triangle");
}

/// A bare path whose var NOTHING else binds is a genuine cartesian factor —
/// dropping it would divide the count by the label's size — so the pass
/// DECLINES and the general path answers.
#[test]
fn a_bare_path_nothing_else_binds_declines() {
    let g = gq3();
    let cartesian = "MATCH (loose:City) \
                     MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country:Country) \
                     RETURN count(*) AS n";
    // 8 persons each in one city, each city in one place, of which 5 are in a
    // Country (pe0,pe1,pe2 → co0; pe3,pe4 → co1), times the 4 loose cities.
    assert_eq!(declines_but_agrees(&g, cartesian), vec![vec![i(20)]]);
}

// ─── q2: the mis-oriented close ──────────────────────────────────────────────

/// LSQB q2's shape: the KNOWS path is written FIRST, so `person2` is bound
/// before the connecting path and the count fold is left with a close onto a
/// sibling branch, which materialises it. The rewrite takes the connecting path
/// first and REVERSES the KNOWS path so it closes from the deeper var onto the
/// seed — then everything but the seed folds.
#[test]
fn q2_shape_reorders_and_folds() {
    let g = gq2();
    let q2 = "MATCH (person1:Person)-[:KNOWS]-(person2:Person), \
              (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
              -[:HAS_CREATOR]->(person2) \
              RETURN count(*) AS n";
    let on = agrees_and_fires(&g, q2);
    assert_eq!(counter(&g, q2, FOLD), 1, "the rewritten q2 must FOLD");
    // By hand, over `gq2`: person1 = pe0 writes cm0 (→po0, by pe1) and cm1
    // (→po1, by pe2); pe0 KNOWS pe1 over TWO parallel edges and pe2 over one →
    // 2 + 1 = 3. person1 = pe1 writes cm2 (→po2, by pe0), and pe1 KNOWS pe0
    // over both parallel edges → 2. person1 = pe2 writes cm3 (→po0, by pe1) and
    // pe2 KNOWS pe1 → 1. Total 6.
    assert_eq!(on, vec![vec![i(6)]], "q2 count");
}

/// The REVERSED path's relationship isomorphism. The connecting path is taken
/// first, so the two-hop SAME-TYPE `(person1)-[:KNOWS]-(mid)-[:KNOWS]-(person2)`
/// is reversed and walked backwards inside the fold — over a self-loop and
/// parallel edges, where reusing a relationship is exactly what rel-iso
/// forbids. A reversed walk visits the same relationship SET, so ON, source
/// order and the general path must all count the same.
#[test]
fn a_reversed_paths_rel_iso_is_the_forward_walks() {
    let g = gq2();
    let two_hop = "MATCH (person1:Person)-[:KNOWS]-(mid:Person)-[:KNOWS]-(person2:Person), \
                   (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
                   -[:HAS_CREATOR]->(person2) \
                   RETURN count(*) AS n";
    let on = agrees_and_fires(&g, two_hop);
    assert_eq!(counter(&g, two_hop, FOLD), 1);
    // The number itself is the general path's; what this pins is that the three
    // paths agree on it over the self-loop + parallel edges.
    assert_eq!(on.len(), 1, "one row");
    // The directed spelling, and the same shape closing onto the seed.
    for src in [
        "MATCH (person1:Person)-[:KNOWS]->(mid:Person)-[:KNOWS]->(person2:Person), \
         (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
         -[:HAS_CREATOR]->(person2) RETURN count(*) AS n",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person1), \
         (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
         -[:HAS_CREATOR]->(person2) RETURN count(*) AS n",
    ] {
        let (on, source_order, general) = triple(&g, src);
        assert_eq!(on, general, "reorder ON vs general: `{src}`");
        assert_eq!(source_order, general, "source order vs general: `{src}`");
    }
}

// ─── The gate ────────────────────────────────────────────────────────────────

/// The reorder is unobservable ONLY because the statement returns one row whose
/// content is the match COUNT. Anything that could observe a row, an order or a
/// grouping must leave the counter at ZERO.
#[test]
fn the_reorder_fires_only_on_the_gated_shape() {
    let g = gq3();
    let base = "MATCH (country:Country) \
                MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
                MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
                MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
                MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) ";
    for tail in [
        // A GROUPING KEY: the row count now depends on the bindings.
        "RETURN country.ck AS k, count(*) AS n ORDER BY k",
        "RETURN person1.pk AS k, count(*) AS n ORDER BY k",
        // An ORDER BY / SKIP / LIMIT can observe a production order.
        "RETURN count(*) AS n ORDER BY n",
        "RETURN count(*) AS n LIMIT 1",
        "RETURN count(*) AS n SKIP 0",
        // Not a bare `count(*)`: the site reads a variable.
        "RETURN count(person1) AS n",
        "RETURN count(DISTINCT person1) AS n",
        "RETURN count(*) AS n, count(person1) AS m",
        // A DISTINCT projection lifts no site at all.
        "RETURN DISTINCT count(*) AS n",
    ] {
        let src = format!("{base}{tail}");
        declines_but_agrees(&g, &src);
    }
    // …and an expression OVER `count(*)` is still one row of pure count, so it
    // is admitted.
    let src = format!("{base}RETURN count(*) + 1 AS n, count(*) AS m");
    assert_eq!(
        agrees_and_fires(&g, &src),
        vec![vec![i(7), i(6)]],
        "an arithmetic wrapper over count(*) is still count-only"
    );
}

/// The admission rule: a pattern whose SOURCE order already materialises as few
/// columns keeps that order — the rewrite is taken only when it strictly
/// improves, so nothing that plans well today churns.
#[test]
fn a_pattern_that_already_folds_keeps_its_source_order() {
    let g = gq3();
    for src in [
        // A single chain from a labelled start: every tail var already folds.
        "MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country:Country) RETURN count(*) AS n",
        // The same chain written from the SMALLEST label already — nothing to do.
        "MATCH (country:Country)<-[:IS_PART_OF]-(city1:City)<-[:IS_LOCATED_IN]-(person1:Person) RETURN count(*) AS n",
        // A tree out of the seed: three legs, all folded in source order.
        "MATCH (country:Country)<-[:IS_PART_OF]-(city1:City)<-[:IS_LOCATED_IN]-(person1:Person), \
         (city1)<-[:IS_LOCATED_IN]-(person2:Person) RETURN count(*) AS n",
    ] {
        declines_but_agrees(&g, src);
        assert_eq!(counter(&g, src, FOLD), 1, "…and it still folds: `{src}`");
    }
}

// ─── Lever ───────────────────────────────────────────────────────────────────

/// Non-vacuity: with the lever OFF the q3 shape is planned as written, the
/// reorder counter is silent, and the fold never fires — so the counter above
/// is measuring the pass and not something else.
#[test]
fn the_reorder_fires_only_when_the_lever_is_on() {
    let g = gq3();
    let q3 = "MATCH (country:Country) \
              MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
              MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
              MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
              MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
              RETURN count(*) AS n";
    assert_eq!(counter(&g, q3, REORDER), 1);
    engram_graph::pipeline::set_count_only_reorder(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, q3));
    engram_graph::pipeline::set_count_only_reorder(true);
    assert_eq!(trace.counters().get(REORDER).copied(), None);
    assert_eq!(
        trace.counters().get(FOLD).copied(),
        None,
        "in source order q3 reaches no columnar plan at all"
    );
}

// ─── What the pass will not model ────────────────────────────────────────────

/// Two paths with HOPS that share no variable are a genuine cartesian product.
/// The greedy attaches only a path with a bound endpoint, so nothing attaches
/// after the seed's own path and the whole pass DECLINES rather than emitting a
/// pattern whose second half starts unbound.
#[test]
fn a_disjoint_hopped_path_declines() {
    let g = gq3();
    let disjoint = "MATCH (person1:Person)-[:KNOWS]-(person2:Person) \
                    MATCH (city1:City)-[:IS_PART_OF]->(country:Country) \
                    RETURN count(*) AS n";
    // 8 KNOWS edges, each matched in both orientations by the undirected hop
    // (there is no KNOWS self-loop in this fixture) = 16; three of the four
    // cities are IS_PART_OF a Country (the fourth is in the Region). 16 × 3.
    assert_eq!(declines_but_agrees(&g, disjoint), vec![vec![i(48)]]);
}

/// An INLINE PROPERTY MAP anywhere in the pattern declines the whole pass:
/// moving a propertied node off the scan start would give up the index seek
/// `start_prop_anchor` seeds it with, so the source order — which put it there
/// — is kept even though the hopless path would otherwise be dropped.
#[test]
fn an_inline_property_map_declines() {
    let g = gq3();
    let anchored = "MATCH (country:Country {ck: 0}) \
                    MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)\
                    -[:IS_PART_OF]->(country) \
                    RETURN count(*) AS n";
    // co0 holds ci0 (pe0, pe1) and ci1 (pe2).
    assert_eq!(declines_but_agrees(&g, anchored), vec![vec![i(3)]]);
    // The same map written on a MID-CHAIN node declines too.
    let mid = "MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City {yk: 0})\
               -[:IS_PART_OF]->(country:Country) \
               RETURN count(*) AS n";
    assert_eq!(declines_but_agrees(&g, mid), vec![vec![i(2)]]);
}
