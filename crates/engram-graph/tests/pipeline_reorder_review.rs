#![allow(non_snake_case)]
//! ADVERSARIAL review of the COUNT-ONLY JOIN REORDER (operator C,
//! `pipeline::reorder_for_count_only`) and the OPTIONAL FOLD (operator D,
//! `pipeline::fold_optional_leg`).
//!
//! The bar is the INTERPRETER as oracle: for every statement here the FOUR
//! columnar variants — the reorder ON / OFF crossed with the fold ON / OFF —
//! must be byte-identical to `set_columnar_scans(false)` (the per-tuple
//! `run_streaming` / `exec_match` general path). A rewrite that changes a single
//! row, in content or in order, is a defect regardless of how much faster it is.
//!
//! WHAT THIS SUITE ATTACKS (each section names its target):
//!   1. the reorder's ADMISSION GATE, on every shape where a row, an order or a
//!      cardinality is observable — a keyed count, ORDER BY / SKIP / LIMIT, a
//!      non-`count(*)` site, DISTINCT, a WITH breaker, a further reading clause;
//!   2. LABEL STAMPING where a var's labels differ per occurrence, over a node
//!      that genuinely carries BOTH labels (so the union is not vacuous);
//!   3. a REVERSED path's relationship isomorphism and its self-loop counting,
//!      including the CLOSE arm (`edge_count_slim`) a reversal can turn an
//!      expand into, and the inline RELATIONSHIP map the refusal list omits;
//!   4. the BARE-PATH DROP where the dropped path is the only constraint on its
//!      var, written before / after / twice / anonymous / unlabelled;
//!   5. the fold's `max(1, ·)` against the interpreter's null-fill under
//!      `count(*)` for legs matching zero / one / many, for two comma paths
//!      where only the SECOND is empty, and for a folded leg's weight crossing a
//!      LATER unfolded clause's ordinary left join;
//!   6. the fold under the multi-OPTIONAL nullable-var decline rules;
//!   7. the WHERE conjuncts the fold moves INLINE onto a hop, whose position is
//!      a function of the binding order the reorder rewrites — including the
//!      edge probe, which reads BOTH endpoints out of the fold's bindings;
//!   8. degenerate seeds (a never-minted label) and contexts the two-clause gate
//!      never sees (UNION);
//!   9. leg shapes at the edge of `leg_hops_foldable`;
//!  10. a RANDOMISED differential — 8 000 generated `count(*)` patterns and
//!      8 000 generated OPTIONAL statements over 40 random multigraphs each,
//!      every one compared against the interpreter;
//!  11. shapes the refusal list does NOT name (a relationship variable, a
//!      multi-type hop, an untyped hop) and the pass's idempotence;
//!  12. the coupling that makes the reorder a no-op while the count fold is off;
//!  13. non-vacuity — a representative of every agreement batch is pinned to
//!      have actually FIRED, so a pass that stopped admitting anything could
//!      not leave this suite green.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const REORDER: &str = "pipeline.count-only reordered";
const FOLD: &str = "interp.pipeline count fold";
const OPT_FOLD: &str = "interp.pipeline optional fold";

// ─── Fixture ─────────────────────────────────────────────────────────────────

/// The hazards the two operators must survive, in ONE graph:
///
///   - `dual` carries BOTH `:Country` and `:Region`, so a label UNION is a real
///     constraint and not a contradiction that trivially counts zero;
///   - pe0 carries TWO KNOWS SELF-LOOPS, the shape where an undirected walk and
///     an undirected edge COUNT can disagree (the O and I sides name one edge);
///   - pe0→pe1 is a PARALLEL pair, so a reversed walk's multiplicity is visible;
///   - places split three ways (`:Country` only, `:Region` only, both), so a
///     stamped label that is dropped or widened changes the count.
///
/// IS_PART_OF: ci0,ci1→co0; ci2→co1; ci3→dual; ci4→rg0.
/// IS_LOCATED_IN: pe0,pe1→ci0; pe2→ci1; pe3→ci2; pe4→ci3; pe5→ci4.
/// KNOWS: pe0→pe0 twice, pe0→pe1 twice, pe1→pe2, pe2→pe0, pe3→pe4.
fn advg() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |labels: &[&str], key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        let ls: Vec<String> = labels.iter().map(|l| (*l).to_string()).collect();
        g.create_node(&ls, &p).expect("node")
    };
    let e = BTreeMap::new();
    let co0 = mk(&["Country"], "ck", 0);
    let co1 = mk(&["Country"], "ck", 1);
    let dual = mk(&["Country", "Region"], "ck", 2);
    let rg0 = mk(&["Region"], "ck", 3);
    let ci: Vec<u64> = (0..5).map(|n| mk(&["City"], "yk", n)).collect();
    let pe: Vec<u64> = (0..6).map(|n| mk(&["Person"], "pk", n)).collect();
    for (city, place) in [(0, co0), (1, co0), (2, co1), (3, dual), (4, rg0)] {
        g.create_rel(ci[city], "IS_PART_OF", place, &e)
            .expect("IS_PART_OF");
    }
    for (person, city) in [(0, 0), (1, 0), (2, 1), (3, 2), (4, 3), (5, 4)] {
        g.create_rel(pe[person], "IS_LOCATED_IN", ci[city], &e)
            .expect("IS_LOCATED_IN");
    }
    for (a, b) in [(0, 0), (0, 0), (0, 1), (0, 1), (1, 2), (2, 0), (3, 4)] {
        g.create_rel(pe[a], "KNOWS", pe[b], &e).expect("KNOWS");
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

/// Run `src` under every combination of the two levers and return the general
/// path's rows once all four columnar variants agree with it. The interpreter is
/// the ORACLE: every columnar variant is compared against IT, never against
/// another columnar variant, so two operators cannot agree their way to a wrong
/// answer.
fn oracle_agrees(g: &Graph, src: &str) -> Rows {
    g.set_columnar_scans(false);
    engram_graph::pipeline::set_count_only_reorder(true);
    engram_graph::pipeline::set_count_fold(true);
    let general = rows(g, src);
    g.set_columnar_scans(true);
    for (reorder, fold) in [(true, true), (true, false), (false, true), (false, false)] {
        engram_graph::pipeline::set_count_only_reorder(reorder);
        engram_graph::pipeline::set_count_fold(fold);
        let got = rows(g, src);
        assert_eq!(
            got, general,
            "reorder={reorder} fold={fold} disagrees with the interpreter: `{src}`"
        );
    }
    engram_graph::pipeline::set_count_only_reorder(true);
    engram_graph::pipeline::set_count_fold(true);
    general
}

/// A counter's value for `src` on the DEFAULT (both levers on) columnar path.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_only_reorder(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// The oracle agrees AND the reorder fired — so what follows is an assertion
/// about a REWRITTEN plan, not about the source order surviving untouched.
fn fires(g: &Graph, src: &str) -> Rows {
    let out = oracle_agrees(g, src);
    assert_eq!(
        counter(g, src, REORDER),
        1,
        "the reorder did not fire: `{src}`"
    );
    out
}

/// The oracle agrees AND the reorder declined.
fn declines(g: &Graph, src: &str) -> Rows {
    let out = oracle_agrees(g, src);
    assert_eq!(
        counter(g, src, REORDER),
        0,
        "the reorder must DECLINE this shape: `{src}`"
    );
    out
}

// ─── 1. The admission gate: nothing observable may be rewritten ─────────────

/// The reorder is unobservable only because the accepted statement is ONE row of
/// pure match count. Every tail that could observe a row, an order, a group or a
/// cardinality must leave the counter at zero — and must still agree with the
/// interpreter, since a decline is only safe if the fallback is right.
#[test]
fn nothing_that_can_observe_a_row_is_rewritten() {
    let g = advg();
    let base = "MATCH (place:Country) \
                MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
                MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) ";
    for tail in [
        // A GROUPING KEY: the ROW COUNT now depends on the bindings.
        "RETURN place.ck AS k, count(*) AS n ORDER BY k",
        "RETURN p1.pk AS a, p2.pk AS b, count(*) AS n ORDER BY a, b",
        // A CONSTANT key still groups.
        "RETURN 1 AS one, count(*) AS n",
        // An order / skip / limit can observe a PRODUCTION order.
        "RETURN count(*) AS n ORDER BY n",
        "RETURN count(*) AS n LIMIT 1",
        "RETURN count(*) AS n LIMIT 0",
        "RETURN count(*) AS n SKIP 1",
        // Sites that are not a bare non-DISTINCT `count(*)`.
        "RETURN count(p1) AS n",
        "RETURN count(DISTINCT p1) AS n",
        "RETURN sum(1) AS n",
        "RETURN min(p1.pk) AS n",
        "RETURN collect(p1.pk) AS n",
        // DISTINCT lifts no site at all.
        "RETURN DISTINCT count(*) AS n",
        // A free pattern var beside the aggregate.
        "RETURN count(*) + p1.pk AS n",
        // A WITH BREAKER between the pattern and the RETURN: three clauses, so
        // the two-clause gate cannot match — and must not, since the WITH is a
        // row-producing boundary the rewrite reasons nothing about.
        "WITH count(*) AS n RETURN n",
        "WITH p1.pk AS k, count(*) AS n RETURN k, n ORDER BY k",
        "WITH count(*) AS n WHERE n > 0 RETURN n",
        // A further READING clause after the counted pattern.
        "WITH DISTINCT p1 MATCH (p1)-[:KNOWS]->(z:Person) RETURN count(*) AS n",
    ] {
        let src = format!("{base}{tail}");
        declines(&g, &src);
    }
}

/// An arithmetic wrapper over `count(*)` is still one row of pure count, so the
/// rewrite IS admitted — the gate must not be so wide that it rewrites a keyed
/// count, nor so narrow that it misses this.
#[test]
fn an_expression_over_count_star_is_still_admitted() {
    let g = advg();
    let src = "MATCH (place:Country) \
               MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
               MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) \
               RETURN count(*) * 2 AS d, count(*) AS n";
    // co0 holds ci0 (pe0, pe1) and ci1 (pe2); co1 holds ci2 (pe3); `dual` is a
    // Country too and holds ci3 (pe4). Ordered pairs: 9 + 1 + 1 = 11.
    assert_eq!(fires(&g, src), vec![vec![i(22), i(11)]]);
}

// ─── 2. Label stamping over a genuinely multi-labelled node ─────────────────

/// The engine's multi-label node pattern is CONJUNCTIVE — the whole premise of
/// stamping a var's label UNION at every occurrence. Pinned directly, so a later
/// change to label matching cannot silently make the union a widening.
#[test]
fn a_multi_label_node_pattern_is_conjunctive() {
    let g = advg();
    for (src, want) in [
        ("MATCH (x:Country:Region) RETURN count(*) AS n", 1),
        ("MATCH (x:Country) RETURN count(*) AS n", 3),
        ("MATCH (x:Region) RETURN count(*) AS n", 2),
        ("MATCH (x:Person:City) RETURN count(*) AS n", 0),
    ] {
        assert_eq!(oracle_agrees(&g, src), vec![vec![i(want)]], "`{src}`");
    }
}

/// Two BARE paths write DIFFERENT labels on the same var; both are dropped and
/// their union stamped onto the hopped occurrence. The union is a real
/// constraint here — `dual` satisfies it and the single-label places do not — so
/// a stamp that lost a label, or that widened to a disjunction, would count 4 or
/// 6 rather than the interpreter's 1.
#[test]
fn a_dropped_paths_labels_intersect_on_the_surviving_occurrence() {
    let g = advg();
    let both = "MATCH (place:Country) \
                MATCH (place:Region) \
                MATCH (city:City)-[:IS_PART_OF]->(place) \
                RETURN count(*) AS n";
    assert_eq!(
        fires(&g, both),
        vec![vec![i(1)]],
        "the label UNION is an INTERSECTION of node sets"
    );
    for (src, want) in [
        (
            "MATCH (place:Country) MATCH (city:City)-[:IS_PART_OF]->(place) \
             RETURN count(*) AS n",
            4,
        ),
        (
            "MATCH (place:Region) MATCH (city:City)-[:IS_PART_OF]->(place) \
             RETURN count(*) AS n",
            2,
        ),
    ] {
        assert_eq!(fires(&g, src), vec![vec![i(want)]], "`{src}`");
    }
    // With NO label the drop would be a pure over-count: all five cities.
    let none = "MATCH (place) MATCH (city:City)-[:IS_PART_OF]->(place) RETURN count(*) AS n";
    assert_eq!(oracle_agrees(&g, none), vec![vec![i(5)]]);
}

/// The stamped label is what lets a var SEED the scan. `Person` is written only
/// on the bare path here, so the rewrite must both drop that path and carry the
/// label onto the endpoint it re-roots at.
#[test]
fn a_label_written_only_on_the_dropped_path_can_seed_the_scan() {
    let g = advg();
    let src = "MATCH (p:Person) \
               MATCH (p)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(place:Country) \
               RETURN count(*) AS n";
    // pe0, pe1 → ci0 → co0; pe2 → ci1 → co0; pe3 → ci2 → co1; pe4 → ci3 → dual.
    // pe5 → ci4 → rg0, which is not a Country.
    assert_eq!(fires(&g, src), vec![vec![i(5)]]);
}

/// A label written ONLY on a MID-CHAIN occurrence must reach the endpoint the
/// rewrite re-roots at, and the other way round: the split spelling counts what
/// the hand-stamped one does.
#[test]
fn a_mid_chain_label_and_an_endpoint_label_agree() {
    let g = advg();
    let split = "MATCH (p:Person)-[:IS_LOCATED_IN]->(c)-[:IS_PART_OF]->(place:Country) \
                 MATCH (c:City)<-[:IS_LOCATED_IN]-(q:Person) \
                 RETURN count(*) AS n";
    let hand = "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(place:Country) \
                MATCH (c:City)<-[:IS_LOCATED_IN]-(q:Person) \
                RETURN count(*) AS n";
    assert_eq!(oracle_agrees(&g, split), oracle_agrees(&g, hand));
}

// ─── 3. Reversal: rel-isomorphism, parallel edges, self-loops ───────────────

/// A reversed walk visits the same relationship SET, so it must count the same —
/// over a node carrying TWO self-loops and a PARALLEL pair, the two shapes where
/// an undirected hop's O and I sides can name one edge twice.
#[test]
fn self_loops_and_parallel_edges_count_the_same_from_either_end() {
    let g = advg();
    for src in [
        "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN count(*) AS n",
        "MATCH (b:Person)-[:KNOWS]-(a:Person) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person) RETURN count(*) AS n",
        "MATCH (c:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(a:Person) RETURN count(*) AS n",
        // A CLOSE onto the start — the arm a reversal can turn an expand into.
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(a) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(a) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(a) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]->(a) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person)-[:KNOWS]-(a) \
         RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
}

/// The reorder is FORCED to reverse EVERY path here: the only labelled endpoint
/// in the pattern is `place:Country` (`City` and `Person` are either mid-chain or
/// larger), so the seed sits at the far end of the connecting chain and each
/// KNOWS path is walked backwards — through the two self-loops and the parallel
/// pair. The counter proves the rewrite fired; the count is derived by hand
/// below, so the interpreter is not merely agreeing with itself.
#[test]
fn a_forced_reversal_over_self_loops_agrees_with_the_interpreter() {
    let g = advg();
    let src = "MATCH (a:Person)-[:KNOWS]-(b:Person) \
               MATCH (c)-[:IS_LOCATED_IN]->(city:City)-[:IS_PART_OF]->(place:Country) \
               MATCH (b)-[:KNOWS]-(c:Person) \
               RETURN count(*) AS n";
    // Undirected KNOWS bindings per person (a self-loop is ONE binding, the
    // parallel pair is two): pe0 5, pe1 3, pe2 2, pe3 1, pe4 1, pe5 0. The two
    // KNOWS paths are separate patterns, so `a` and `c` may reuse one edge, and
    // every neighbour of pe0..pe4 lives in a Country. Summed over `b`:
    // 5·5 + 3·3 + 2·2 + 1·1 + 1·1 = 40.
    assert_eq!(fires(&g, src), vec![vec![i(40)]]);
    // The same forced reversal with the bare-path drop in front of it, and a
    // count that is likewise hand-derived: the 12 undirected KNOWS bindings
    // restricted to pairs whose persons share one Country — every pair among
    // {pe0, pe1, pe2} (2 self + 4 parallel + 2 + 2 = 10), and none of the
    // pe3/pe4 pair, whose countries differ.
    let shared = "MATCH (place:Country) \
                  MATCH (a:Person)-[:KNOWS]-(b:Person) \
                  MATCH (a)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
                  MATCH (b)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) \
                  RETURN count(*) AS n";
    assert_eq!(fires(&g, shared), vec![vec![i(10)]]);
    // A path with BOTH ends bound is oriented to start at the LATER-bound one,
    // so the connecting chain is the reversed one here.
    for src in [
        "MATCH (a:Person)-[:KNOWS]-(b:Person), \
         (a)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(pl:Country)\
         <-[:IS_PART_OF]-(c2:City)<-[:IS_LOCATED_IN]-(b) RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person), \
         (a)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(pl:Country)\
         <-[:IS_PART_OF]-(c2:City)<-[:IS_LOCATED_IN]-(c) RETURN count(*) AS n",
    ] {
        fires(&g, src);
    }
}

/// The same chains where the SOURCE order already materialises as few columns:
/// the admission rule keeps them as written, and they must still agree. A
/// decline is only safe if the fallback is right.
#[test]
fn chains_that_already_plan_well_keep_their_order_and_still_agree() {
    let g = advg();
    for src in [
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person) \
         MATCH (c)-[:IS_LOCATED_IN]->(city:City)-[:IS_PART_OF]->(place:Country) \
         RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
         MATCH (c)-[:IS_LOCATED_IN]->(city:City)-[:IS_PART_OF]->(place:Country) \
         RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(a) \
         MATCH (a)-[:IS_LOCATED_IN]->(city:City)-[:IS_PART_OF]->(place:Country) \
         RETURN count(*) AS n",
        "MATCH (a:Person)-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person)-[:KNOWS]-(a) \
         MATCH (a)-[:IS_LOCATED_IN]->(city:City)-[:IS_PART_OF]->(place:Country) \
         RETURN count(*) AS n",
    ] {
        declines(&g, src);
    }
}

/// An inline map on a RELATIONSHIP is NOT in the reorder's refusal list (only
/// NODE maps are, for the seek `start_prop_anchor` seeds). `reverse_path` copies
/// it onto the flipped hop, so the filter must survive the reversal: a
/// non-matching value counts zero and a matching one counts what the
/// interpreter counts.
#[test]
fn a_relationship_inline_map_survives_a_reversal() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut p = BTreeMap::new();
    p.insert("pk".to_string(), Value::Int(0));
    let a = g.create_node(&["Person".into()], &p).expect("a");
    let b = g.create_node(&["Person".into()], &p).expect("b");
    let city = g.create_node(&["City".into()], &p).expect("city");
    let place = g.create_node(&["Country".into()], &p).expect("place");
    let mut w1 = BTreeMap::new();
    w1.insert("w".to_string(), Value::Int(1));
    let mut w2 = BTreeMap::new();
    w2.insert("w".to_string(), Value::Int(2));
    g.create_rel(a, "KNOWS", b, &w1).expect("k1");
    g.create_rel(a, "KNOWS", b, &w2).expect("k2");
    g.create_rel(b, "IS_LOCATED_IN", city, &BTreeMap::new())
        .expect("loc");
    g.create_rel(city, "IS_PART_OF", place, &BTreeMap::new())
        .expect("part");
    for (src, want) in [
        (
            "MATCH (x:Person)-[:KNOWS {w: 1}]-(y:Person) \
             MATCH (y)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(q:Country) \
             RETURN count(*) AS n",
            1,
        ),
        (
            "MATCH (x:Person)-[:KNOWS {w: 9}]-(y:Person) \
             MATCH (y)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(q:Country) \
             RETURN count(*) AS n",
            0,
        ),
        (
            "MATCH (x:Person)-[:KNOWS]-(y:Person) \
             MATCH (y)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(q:Country) \
             RETURN count(*) AS n",
            2,
        ),
    ] {
        assert_eq!(oracle_agrees(&g, src), vec![vec![i(want)]], "`{src}`");
    }
}

// ─── 4. The bare-path drop ─────────────────────────────────────────────────

/// The same bare path written FIRST, LAST and TWICE must count what writing its
/// label inline counts. A drop that removed a factor, or that forgot a second
/// copy's label, would show here.
#[test]
fn a_bare_path_is_a_factor_of_exactly_one_wherever_it_is_written() {
    let g = advg();
    let inline = "MATCH (city:City)-[:IS_PART_OF]->(place:Country) RETURN count(*) AS n";
    let want = oracle_agrees(&g, inline);
    assert_eq!(want, vec![vec![i(4)]]);
    for src in [
        "MATCH (place:Country) MATCH (city:City)-[:IS_PART_OF]->(place) RETURN count(*) AS n",
        "MATCH (city:City)-[:IS_PART_OF]->(place) MATCH (place:Country) RETURN count(*) AS n",
        "MATCH (place:Country), (place:Country), (city:City)-[:IS_PART_OF]->(place) \
         RETURN count(*) AS n",
        "MATCH (place:Country), (city:City)-[:IS_PART_OF]->(place), (place) \
         RETURN count(*) AS n",
        "MATCH (city:City), (city)-[:IS_PART_OF]->(place:Country) RETURN count(*) AS n",
    ] {
        assert_eq!(oracle_agrees(&g, src), want, "`{src}`");
    }
}

/// A bare path whose var NOTHING with hops binds is a genuine cartesian factor.
/// It must never be dropped — including when it is ANONYMOUS, where there is no
/// var to bind it by at all.
#[test]
fn a_cartesian_bare_path_is_never_dropped() {
    let g = advg();
    for (src, want) in [
        (
            "MATCH (loose:City) MATCH (city:City)-[:IS_PART_OF]->(place:Country) \
             RETURN count(*) AS n",
            20,
        ),
        (
            "MATCH (:Country) MATCH (city:City)-[:IS_PART_OF]->(place:Country) \
             RETURN count(*) AS n",
            12,
        ),
        (
            "MATCH (loose) MATCH (city:City)-[:IS_PART_OF]->(place:Country) \
             RETURN count(*) AS n",
            60,
        ),
    ] {
        assert_eq!(declines(&g, src), vec![vec![i(want)]], "`{src}`");
    }
}

/// A WHERE over a var whose only pattern occurrence is on a DROPPED bare path
/// still filters: the WHERE travels with the rewrite verbatim, and the var is
/// still bound by the surviving occurrence.
#[test]
fn a_where_over_the_dropped_paths_var_still_filters() {
    let g = advg();
    let src = "MATCH (place:Country) \
               MATCH (city:City)-[:IS_PART_OF]->(place) \
               WHERE place.ck = 0 \
               RETURN count(*) AS n";
    assert_eq!(oracle_agrees(&g, src), vec![vec![i(2)]]);
}

/// A pattern the pass cannot connect (two hopped paths sharing no var) and one
/// carrying an inline NODE map must both keep the source order.
#[test]
fn disjoint_and_propertied_patterns_keep_their_source_order() {
    let g = advg();
    for src in [
        "MATCH (a:Person)-[:KNOWS]->(b:Person) MATCH (city:City)-[:IS_PART_OF]->(q:Country) \
         RETURN count(*) AS n",
        "MATCH (place:Country {ck: 0}) MATCH (city:City)-[:IS_PART_OF]->(place) \
         RETURN count(*) AS n",
        "MATCH (place:Country) MATCH (city:City {yk: 0})-[:IS_PART_OF]->(place) \
         RETURN count(*) AS n",
    ] {
        declines(&g, src);
    }
}

// ─── 5. The OPTIONAL fold's max(1, ·) against the null-fill row ─────────────

/// Legs matching ZERO, ONE and MANY, and the leg-count PRODUCT of two comma
/// paths inside one clause where only the SECOND is empty. `max(1, ·)` is the
/// null-fill row, so a leg that misses weighs one and never zero.
#[test]
fn the_fold_weight_is_the_interpreters_null_fill_row() {
    let g = advg();
    for src in [
        // pe0 knows pe1 twice and itself twice; pe5 knows nobody.
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN count(*) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(b:Person) RETURN count(*) AS n",
        // TWO comma paths in ONE clause: the leg count is their product, and a
        // zero factor null-fills the whole leg.
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person), (a)-[:IS_LOCATED_IN]->(c:City) \
         RETURN count(*) AS n",
        // …and with the possibly-empty factor written FIRST.
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City), (a)-[:KNOWS]->(b:Person) \
         RETURN count(*) AS n",
        // TWO clauses where only the second can miss, then only the first.
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN count(*) AS n",
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City) \
         RETURN count(*) AS n",
        // THREE clauses with the middle one empty for EVERY outer row.
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City) \
         OPTIONAL MATCH (a)-[:HAS_INTEREST]->(t:Tag) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN count(*) AS n",
        // A leg that CLOSES onto the outer var, over the self-loops.
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(a) RETURN count(*) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(a) RETURN count(*) AS n",
        // A two-hop leg closing back onto the outer var (rel-iso inside the leg).
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(b:Person)-[:KNOWS]-(a) \
         RETURN count(*) AS n",
        // A leg root re-using a relationship the OUTER walk traversed — each
        // clause is its own pattern, so it must be allowed.
        "MATCH (a:Person)-[:KNOWS]->(b:Person) OPTIONAL MATCH (b)-[:KNOWS]->(d:Person) \
         RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
}

/// The weights a folded leg produces must survive a GROUPED count, an ORDER BY
/// over that count, a LIMIT and a HAVING — the tail reads only outer vars, so
/// the fold is admitted and every group's total is a sum of `max(1, ·)` weights.
#[test]
fn folded_weights_feed_a_grouped_and_ordered_count() {
    let g = advg();
    for src in [
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN a.pk AS k, count(*) AS n ORDER BY k",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN a.pk AS k, count(*) AS n ORDER BY n DESC, k ASC LIMIT 3",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(b:Person) \
         WITH a.pk AS k, count(*) AS n WHERE n > 1 RETURN k, n ORDER BY k",
        "MATCH (a:Person)-[:IS_LOCATED_IN]->(c:City) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN c.yk AS k, count(*) AS n ORDER BY k",
    ] {
        let out = oracle_agrees(&g, src);
        assert!(!out.is_empty(), "`{src}` produced no rows");
    }
    // The keyed shape must ACTUALLY fold, or the assertions above measure the
    // ordinary left join twice.
    let keyed = "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
                 RETURN a.pk AS k, count(*) AS n ORDER BY k";
    assert_eq!(counter(&g, keyed, OPT_FOLD), 1, "the keyed count must fold");
}

/// A folded clause followed by an UNFOLDED one: the second carries a WHERE, so
/// `plan_optional_fold` refuses it and the ordinary left join runs over a chunk
/// that already carries fold WEIGHTS. Dropping them would divide the count.
#[test]
fn a_folded_legs_weight_survives_a_later_unfolded_left_join() {
    let g = advg();
    let src = "MATCH (a:Person) \
               OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
               OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City) WHERE c.yk > 0 \
               RETURN count(*) AS n";
    oracle_agrees(&g, src);
    assert_eq!(
        counter(&g, src, OPT_FOLD),
        1,
        "exactly the first leg folds (the second carries a WHERE)"
    );
    // The mirror: the WHERE-bearing clause FIRST, so a weightless ordinary join
    // feeds the fold.
    let mirror = "MATCH (a:Person) \
                  OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(c:City) WHERE c.yk > 0 \
                  OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
                  RETURN count(*) AS n";
    oracle_agrees(&g, mirror);
    assert_eq!(counter(&g, mirror, OPT_FOLD), 1);
}

// ─── 6. The fold under the nullable-var decline rules ──────────────────────

/// A leg var the tail READS is never folded — the fold has only `NULL_ID` to
/// offer for it — and a non-`count(*)` site is never folded either, since
/// `count(legvar)` counts the null-fill row as ZERO where `max(1, ·)` counts it
/// as one. Both must still answer the interpreter's rows.
#[test]
fn a_read_leg_var_or_a_non_count_star_site_does_not_fold() {
    let g = advg();
    for src in [
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN count(b) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN count(*) AS n, count(b) AS m",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN count(DISTINCT b) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN collect(b.pk) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         WITH a.pk AS k, count(*) AS n, collect(b.pk) AS bs RETURN k, n, bs ORDER BY k",
        // A RELATIONSHIP variable in the leg: the fold appends no column for one.
        "MATCH (a:Person) OPTIONAL MATCH (a)-[r:KNOWS]->(b:Person) RETURN count(*) AS n",
        // A leg var as a GROUPING key is refused by `nullable_agg_ok` outright.
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         RETURN b.pk AS k, count(*) AS n ORDER BY k",
    ] {
        oracle_agrees(&g, src);
        assert_eq!(
            counter(&g, src, OPT_FOLD),
            0,
            "this leg must NOT fold: `{src}`"
        );
    }
}

/// A later clause that re-roots at or closes onto an EARLIER clause's NULLABLE
/// var is declined by the recogniser, folded or not — the sentinel is not a node
/// id. The interpreter answers, and its answer is the one the fold must never
/// claim to have reached.
#[test]
fn a_leg_touching_an_earlier_nullable_var_is_declined() {
    let g = advg();
    for src in [
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         OPTIONAL MATCH (b)-[:IS_LOCATED_IN]->(c:City) \
         RETURN count(*) AS n",
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(d:Person)-[:KNOWS]->(b) \
         RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
        assert_eq!(counter(&g, src, OPT_FOLD), 0, "declined outright: `{src}`");
    }
}

// ─── Lever non-vacuity ─────────────────────────────────────────────────────

/// With the fold lever off the same statement runs the ordinary left join, so
/// every `OPT_FOLD` assertion above is measuring the operator and not the shape.
#[test]
fn the_optional_fold_counter_measures_the_lever() {
    let g = advg();
    let src = "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN count(*) AS n";
    assert_eq!(counter(&g, src, OPT_FOLD), 1);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(trace.counters().get(OPT_FOLD).copied(), None);
}

/// The same for the reorder, and for the count FOLD its rewrite unlocks: in
/// source order the bare-path shape reaches no columnar plan at all.
#[test]
fn the_reorder_counter_measures_the_lever() {
    let g = advg();
    let src = "MATCH (place:Country) \
               MATCH (city:City)-[:IS_PART_OF]->(place) \
               MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
               RETURN count(*) AS n";
    assert_eq!(counter(&g, src, REORDER), 1);
    assert_eq!(counter(&g, src, FOLD), 1);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_only_reorder(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    engram_graph::pipeline::set_count_only_reorder(true);
    assert_eq!(trace.counters().get(REORDER).copied(), None);
    assert_eq!(trace.counters().get(FOLD).copied(), None);
}


// ─── 7. WHERE conjuncts the fold moves INLINE onto a hop ────────────────────

/// The count fold moves a WHERE conjunct onto the hop that binds its last var,
/// and an inline pred reads `bind[o]` for the OTHER var — a position that is a
/// function of the BINDING ORDER, which is exactly what the reorder rewrites. A
/// pred attached at a level where its other var is not yet bound would compare
/// against the `NULL_ID` placeholder and silently drop or admit rows.
#[test]
fn where_conjuncts_that_become_inline_preds_survive_the_reorder() {
    let g = advg();
    let base = "MATCH (place:Country) \
                MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
                MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) ";
    for (tail, want) in [
        ("WHERE p1 <> p2 RETURN count(*) AS n", 6),
        ("WHERE p1.pk <> p2.pk RETURN count(*) AS n", 6),
        ("WHERE p1.pk < p2.pk RETURN count(*) AS n", 3),
        ("WHERE c1 <> c2 RETURN count(*) AS n", 4),
        ("WHERE p1.pk = p2.pk RETURN count(*) AS n", 5),
        ("WHERE place.ck = 0 AND p1 <> p2 RETURN count(*) AS n", 6),
    ] {
        let src = format!("{base}{tail}");
        assert_eq!(oracle_agrees(&g, &src), vec![vec![i(want)]], "`{src}`");
    }
}

/// The EDGE PROBE conjunct (`NOT (x)-[:T]-(y)`) reads BOTH endpoints out of the
/// fold's bindings, so it is the sharpest test of a rewritten binding order.
#[test]
fn an_edge_probe_conjunct_survives_the_reorder() {
    let g = advg();
    let base = "MATCH (place:Country) \
                MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
                MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) ";
    for tail in [
        "WHERE NOT (p1)-[:KNOWS]-(p2) RETURN count(*) AS n",
        "WHERE (p1)-[:KNOWS]-(p2) RETURN count(*) AS n",
        "WHERE p1 <> p2 AND NOT (p1)-[:KNOWS]-(p2) RETURN count(*) AS n",
    ] {
        let src = format!("{base}{tail}");
        oracle_agrees(&g, &src);
    }
}

// ─── 8. Degenerate seeds and contexts the gate never sees ───────────────────

/// A label the graph never minted has ZERO nodes, so it wins the seed choice
/// outright. The scan then finds nothing and the count is zero — on every path.
#[test]
fn a_never_minted_label_seeds_an_empty_scan() {
    let g = advg();
    for src in [
        "MATCH (x:Nonexistent) MATCH (p:Person)-[:KNOWS]->(x) RETURN count(*) AS n",
        "MATCH (x:Nonexistent)<-[:KNOWS]-(p:Person) MATCH (p)-[:IS_LOCATED_IN]->(c:City) \
         RETURN count(*) AS n",
        "MATCH (place:Country) MATCH (c:City)-[:NEVER_MINTED]->(place) RETURN count(*) AS n",
    ] {
        assert_eq!(oracle_agrees(&g, src), vec![vec![i(0)]], "`{src}`");
    }
}

/// A pattern with FOUR paths and several shared vars, so the greedy has real
/// choices to make, and one where two paths share BOTH endpoints (a join over a
/// multigraph). Whatever order the greedy picks, the answer is the oracle's.
#[test]
fn a_multi_path_pattern_with_shared_endpoints_agrees() {
    let g = advg();
    for src in [
        "MATCH (place:Country) \
         MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
         MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) \
         MATCH (p1)-[:KNOWS]-(p2) \
         RETURN count(*) AS n",
        "MATCH (p1:Person)-[:KNOWS]-(p2:Person) \
         MATCH (p1)-[:KNOWS]-(p2) \
         RETURN count(*) AS n",
        "MATCH (p1:Person)-[:KNOWS]-(p2:Person) \
         MATCH (p2)-[:KNOWS]-(p1) \
         MATCH (p1)-[:IS_LOCATED_IN]->(c:City)-[:IS_PART_OF]->(place:Country) \
         RETURN count(*) AS n",
        "MATCH (place:Country) \
         MATCH (c1:City)-[:IS_PART_OF]->(place) \
         MATCH (c2:City)-[:IS_PART_OF]->(place) \
         MATCH (p:Person)-[:IS_LOCATED_IN]->(c1) \
         MATCH (q:Person)-[:IS_LOCATED_IN]->(c2) \
         RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
}

/// A UNION branch and a CALL subquery each wrap the counted statement in a
/// context the two-clause gate never sees. Whatever the reorder does inside, the
/// composed answer must be the interpreter's.
#[test]
fn union_and_subquery_contexts_agree() {
    let g = advg();
    let one = "MATCH (place:Country) \
               MATCH (city:City)-[:IS_PART_OF]->(place) \
               MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
               RETURN count(*) AS n";
    let two = "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN count(*) AS n";
    for src in [
        format!("{one} UNION ALL {two}"),
        format!("{one} UNION {two}"),
    ] {
        oracle_agrees(&g, &src);
    }
}

// ─── 9. Leg shapes the OPTIONAL fold must decline or answer exactly ─────────

/// Leg shapes at the edge of `leg_hops_foldable`: an inline property map, a
/// var-length hop, an empty OUTER, an outer chain that is itself multi-path, and
/// a leg whose only match is a self-loop. Every one must answer the oracle's
/// rows whether it folds or declines.
#[test]
fn leg_shapes_at_the_edge_of_the_fold_agree() {
    let g = advg();
    for src in [
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person {pk: 1}) RETURN count(*) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS*1..2]->(b:Person) RETURN count(*) AS n",
        "MATCH (a:Nonexistent) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN count(*) AS n",
        "MATCH (a:Person)-[:IS_LOCATED_IN]->(c:City), (a)-[:KNOWS]->(x:Person) \
         OPTIONAL MATCH (c)-[:IS_PART_OF]->(place:Country) RETURN count(*) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(a) RETURN count(*) AS n",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person)-[:KNOWS]->(d:Person) \
         RETURN count(*) AS n",
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person)-[:IS_LOCATED_IN]->(c:City)\
         -[:IS_PART_OF]->(place:Country) RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
}

// ─── 10. Randomised differential over generated patterns ────────────────────

/// A deterministic LCG — the suite must reproduce byte-for-byte from its seed,
/// and the engine crates take no new dependencies.
struct Lcg(u64);

impl Lcg {
    fn bits(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.bits() % n as u64) as usize
    }
}

/// A small random multigraph: self-loops, parallel edges, two relationship types
/// and a label assignment that includes a MULTI-labelled and an UNLABELLED node,
/// so a generated pattern can hit the label union, the bare-path drop and
/// relationship isomorphism at once.
fn random_graph(rng: &mut Lcg, nodes: usize, edges: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::with_capacity(nodes);
    for n in 0..nodes {
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(n as i64));
        let labels: Vec<String> = match n % 4 {
            0 => vec!["A".into()],
            1 => vec!["B".into()],
            2 => vec!["A".into(), "B".into()],
            _ => Vec::new(),
        };
        ids.push(g.create_node(&labels, &p).expect("node"));
    }
    let e = BTreeMap::new();
    for _ in 0..edges {
        let s = ids[rng.below(nodes)];
        let d = ids[rng.below(nodes)];
        let t = if rng.below(2) == 0 { "R" } else { "S" };
        g.create_rel(s, t, d, &e).expect("rel");
    }
    g
}

/// One random `count(*)`-only pattern. Variables come from a THREE-name pool so
/// the paths genuinely share endpoints (a disjoint pattern is declined and
/// teaches nothing), and the total hop count is capped so the interpreter — the
/// oracle — stays cheap enough to run on every case.
fn random_count_query(rng: &mut Lcg) -> String {
    const VARS: [&str; 3] = ["u", "v", "w"];
    let node = |rng: &mut Lcg| -> String {
        let v = VARS[rng.below(VARS.len())];
        match rng.below(4) {
            0 => format!("({v})"),
            1 => format!("({v}:A)"),
            2 => format!("({v}:B)"),
            _ => format!("({v}:A:B)"),
        }
    };
    let rel = |rng: &mut Lcg| -> &'static str {
        match rng.below(6) {
            0 => "-[:R]->",
            1 => "<-[:R]-",
            2 => "-[:R]-",
            3 => "-[:S]->",
            4 => "<-[:S]-",
            _ => "-[]-",
        }
    };
    let npaths = 1 + rng.below(3);
    let mut budget = 3usize;
    let mut paths: Vec<String> = Vec::with_capacity(npaths);
    for _ in 0..npaths {
        let hops = rng.below(budget + 1).min(2);
        budget -= hops;
        let mut p = node(rng);
        for _ in 0..hops {
            p.push_str(rel(rng));
            p.push_str(&node(rng));
        }
        paths.push(p);
    }
    format!("MATCH {} RETURN count(*) AS n", paths.join(", "))
}

/// The broad refutation: 8 000 generated `count(*)` patterns over 40 random
/// multigraphs, each run on all four lever combinations and compared against the
/// interpreter. A pattern the interpreter REFUSES (a row-budget or unsupported
/// construct) is skipped, since there is no oracle for it; a pattern the
/// interpreter answers must be answered identically by every columnar variant.
/// The reorder-fired tally is asserted non-zero, so a gate that silently stopped
/// admitting anything could not pass this as a green run.
#[test]
fn randomised_count_patterns_match_the_interpreter() {
    let mut rng = Lcg(0x5EED_1234_ABCD_0001);
    let mut checked = 0usize;
    let mut fired = 0usize;
    let mut skipped = 0usize;
    for _ in 0..40 {
        let g = random_graph(&mut rng, 10, 16);
        for _ in 0..200 {
            let src = random_count_query(&mut rng);
            let q = parse_statement(&src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
            g.set_columnar_scans(false);
            engram_graph::pipeline::set_count_only_reorder(true);
            engram_graph::pipeline::set_count_fold(true);
            let Ok(general) = run_query(&g, &q, BTreeMap::new()) else {
                skipped += 1;
                g.set_columnar_scans(true);
                continue;
            };
            g.set_columnar_scans(true);
            for (reorder, fold) in [(true, true), (true, false), (false, true), (false, false)] {
                engram_graph::pipeline::set_count_only_reorder(reorder);
                engram_graph::pipeline::set_count_fold(fold);
                let got = run_query(&g, &q, BTreeMap::new())
                    .unwrap_or_else(|e| panic!("columnar refused what the interpreter answered: `{src}`: {e}"));
                assert_eq!(
                    got.rows, general.rows,
                    "reorder={reorder} fold={fold} disagrees with the interpreter: `{src}`"
                );
            }
            engram_graph::pipeline::set_count_only_reorder(true);
            engram_graph::pipeline::set_count_fold(true);
            let (_, trace) = engram_observe::with_trace(|| {
                run_query(&g, &q, BTreeMap::new()).expect("rerun")
            });
            fired += trace.counters().get(REORDER).copied().unwrap_or(0) as usize;
            checked += 1;
        }
    }
    println!("checked={checked} reorder-fired={fired} skipped={skipped}");
    assert!(checked > 1_000, "too few cases reached the oracle: {checked}");
    assert!(
        fired > 50,
        "the reorder fired on only {fired} of {checked} cases ({skipped} skipped) — the fuzz is not exercising it"
    );
}

/// One random OPTIONAL statement: an outer chain, one or two OPTIONAL clauses
/// each of one or two comma paths rooted at an already-bound var, and a
/// `count(*)` tail (sometimes keyed on an OUTER var, which the fold still
/// admits). Legs are built to root at a bound var so a useful share of the
/// generated statements reach the operator rather than being declined outright.
fn random_optional_query(rng: &mut Lcg) -> String {
    let rel = |rng: &mut Lcg| -> &'static str {
        match rng.below(5) {
            0 => "-[:R]->",
            1 => "<-[:R]-",
            2 => "-[:R]-",
            3 => "-[:S]->",
            _ => "-[]-",
        }
    };
    let label = |rng: &mut Lcg| -> &'static str {
        match rng.below(3) {
            0 => "",
            1 => ":A",
            _ => ":B",
        }
    };
    let mut out = format!("MATCH (u{})", label(rng));
    let mut bound: Vec<&'static str> = vec!["u"];
    if rng.below(2) == 0 {
        out.push_str(rel(rng));
        out.push_str(&format!("(v{})", label(rng)));
        bound.push("v");
    }
    let fresh: [&str; 3] = ["x", "y", "z"];
    let mut next_fresh = 0usize;
    for _ in 0..(1 + rng.below(2)) {
        out.push_str(" OPTIONAL MATCH ");
        let paths = 1 + rng.below(2);
        for pi in 0..paths {
            if pi > 0 {
                out.push_str(", ");
            }
            let root = bound[rng.below(bound.len())];
            out.push_str(&format!("({root})"));
            for _ in 0..(1 + rng.below(2)) {
                out.push_str(rel(rng));
                // A CLOSE onto the root, or a fresh nullable var.
                if rng.below(4) == 0 || next_fresh >= fresh.len() {
                    out.push_str(&format!("({root})"));
                } else {
                    out.push_str(&format!("({}{})", fresh[next_fresh], label(rng)));
                    next_fresh += 1;
                }
            }
        }
    }
    if rng.below(3) == 0 {
        out.push_str(" RETURN u.k AS k, count(*) AS n ORDER BY k");
    } else {
        out.push_str(" RETURN count(*) AS n");
    }
    out
}

/// The OPTIONAL fold's broad refutation: 8 000 generated left-join statements
/// over 40 random multigraphs, each compared against the interpreter with the
/// fold ON and OFF. `max(1, ·)` is the only thing standing between a folded leg
/// and a lost null-fill row, so the tally of statements that ACTUALLY folded is
/// asserted, not assumed.
#[test]
fn randomised_optional_statements_match_the_interpreter() {
    let mut rng = Lcg(0x0F01_D5EE_D000_7A31);
    let mut checked = 0usize;
    let mut folded = 0usize;
    let mut skipped = 0usize;
    for _ in 0..40 {
        let g = random_graph(&mut rng, 10, 16);
        for _ in 0..200 {
            let src = random_optional_query(&mut rng);
            let q = parse_statement(&src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
            g.set_columnar_scans(false);
            engram_graph::pipeline::set_count_fold(true);
            let Ok(general) = run_query(&g, &q, BTreeMap::new()) else {
                skipped += 1;
                g.set_columnar_scans(true);
                continue;
            };
            g.set_columnar_scans(true);
            for fold in [true, false] {
                engram_graph::pipeline::set_count_fold(fold);
                let got = run_query(&g, &q, BTreeMap::new()).unwrap_or_else(|e| {
                    panic!("columnar refused what the interpreter answered: `{src}`: {e}")
                });
                assert_eq!(
                    got.rows, general.rows,
                    "fold={fold} disagrees with the interpreter: `{src}`"
                );
            }
            engram_graph::pipeline::set_count_fold(true);
            let (_, trace) =
                engram_observe::with_trace(|| run_query(&g, &q, BTreeMap::new()).expect("rerun"));
            folded += trace.counters().get(OPT_FOLD).copied().unwrap_or(0) as usize;
            checked += 1;
        }
    }
    println!("checked={checked} legs-folded={folded} skipped={skipped}");
    assert!(checked > 1_000, "too few cases reached the oracle: {checked}");
    assert!(
        folded > 50,
        "only {folded} legs folded over {checked} cases — the fuzz is not exercising the operator"
    );
}

// ─── 11. Shapes the refusal list does NOT name ──────────────────────────────

/// `reorder_pattern` refuses a named path, a `shortestPath`, a var-length hop
/// and any inline NODE map — but NOT a RELATIONSHIP VARIABLE, which
/// `reverse_path` carries onto the flipped hop. A rel var is a binding the
/// statement could in principle observe, so the count must be the interpreter's
/// with the var present, absent, and read by a WHERE.
#[test]
fn a_relationship_variable_survives_the_reorder() {
    let g = advg();
    for src in [
        "MATCH (place:Country) \
         MATCH (city:City)-[r:IS_PART_OF]->(place) \
         MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
         RETURN count(*) AS n",
        "MATCH (place:Country) \
         MATCH (city:City)-[r:IS_PART_OF]->(place) \
         MATCH (p:Person)-[s:IS_LOCATED_IN]->(city) \
         RETURN count(*) AS n",
        "MATCH (place:Country) \
         MATCH (a:Person)-[r:KNOWS]-(b:Person) \
         MATCH (a)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
         RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
    // A named PATH and a shortestPath are refused outright; both must still
    // answer, through whichever path claims them.
    for src in [
        "MATCH pth = (city:City)-[:IS_PART_OF]->(place:Country) RETURN count(*) AS n",
        "MATCH (place:Country) MATCH pth = (city:City)-[:IS_PART_OF]->(place) \
         RETURN count(*) AS n",
    ] {
        declines(&g, src);
    }
}

/// A MULTI-TYPE hop (`-[:R|S]-`) and an UNTYPED hop (`-[]-`) reverse like any
/// other: `flip_dir` is an involution and the type list is copied. An untyped
/// hop is also the one shape `memo_ok_for` must treat as disjoint from NOTHING,
/// so a two-hop untyped chain is the memo's own hazard.
#[test]
fn multi_type_and_untyped_hops_reverse_and_fold_correctly() {
    let g = advg();
    for src in [
        "MATCH (place:Country) \
         MATCH (city:City)-[:IS_PART_OF|IS_LOCATED_IN]-(place) \
         MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
         RETURN count(*) AS n",
        "MATCH (place:Country) MATCH (city:City)-[]-(place) MATCH (p:Person)-[]->(city) \
         RETURN count(*) AS n",
        "MATCH (a:Person)-[]-(b)-[]-(c:Country) RETURN count(*) AS n",
        "MATCH (c:Country)-[]-(b)-[]-(a:Person) RETURN count(*) AS n",
        "MATCH (a:Person)-[]-(b)-[]-(a) RETURN count(*) AS n",
    ] {
        oracle_agrees(&g, src);
    }
}

/// The pass claims to be IDEMPOTENT — its output's seed is its first var, its
/// paths are already oriented, and a rewrite equal to its input returns `None`,
/// which is what stops `plan_and_run_columnar` from re-planning forever. Running
/// the rewritten form of a fired shape (the same pattern written the way the
/// pass leaves it) must therefore DECLINE, and count the same.
#[test]
fn the_rewritten_form_of_a_fired_shape_declines() {
    let g = advg();
    let fired = "MATCH (place:Country) \
                 MATCH (city:City)-[:IS_PART_OF]->(place) \
                 MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
                 RETURN count(*) AS n";
    let want = fires(&g, fired);
    let rewritten = "MATCH (place:Country)<-[:IS_PART_OF]-(city:City), \
                     (city)<-[:IS_LOCATED_IN]-(p:Person) \
                     RETURN count(*) AS n";
    assert_eq!(declines(&g, rewritten), want);
    assert_eq!(counter(&g, rewritten, FOLD), 1, "and it still folds");
}

// ─── 12. The two levers are NOT independent ────────────────────────────────

/// The reorder's admission rule counts MATERIALISED vars, and
/// `materialised_var_count` re-runs `recognise_aggregate`, which sets `Hop.fold`
/// only while `count_fold_enabled()`. So with the COUNT FOLD off, a pattern the
/// recognisers already claim can never "materialise strictly fewer" and the
/// reorder silently declines it — the pass fires with the fold off ONLY for a
/// source order that is declined outright (q3's hopless path).
///
/// Pinned because it is a hazard for anyone measuring these operators: toggling
/// the FOLD lever also switches most of the reorder off, so a differential that
/// varies only the fold is not exercising the rewrite it appears to.
#[test]
fn the_reorder_is_admitted_only_while_the_count_fold_is_on() {
    let g = advg();
    // A source order the recognisers CLAIM: the rewrite only saves columns, so
    // with the fold off there is nothing to save and the pass declines.
    let claimed = "MATCH (a:Person)-[:KNOWS]-(b:Person), \
                   (a)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(pl:Country)\
                   <-[:IS_PART_OF]-(c2:City)<-[:IS_LOCATED_IN]-(b) RETURN count(*) AS n";
    assert_eq!(counter(&g, claimed, REORDER), 1);
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(false);
    let (_, off) = engram_observe::with_trace(|| rows(&g, claimed));
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(
        off.counters().get(REORDER).copied(),
        None,
        "with the fold off the admission rule can never improve on a claimed source order"
    );
    // A source order the recognisers DECLINE (the hopless path) is improved by
    // any recognised rewrite, so the pass fires with the fold off too.
    let declined = "MATCH (place:Country) \
                    MATCH (city:City)-[:IS_PART_OF]->(place) \
                    MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
                    RETURN count(*) AS n";
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(false);
    let (_, still) = engram_observe::with_trace(|| rows(&g, declined));
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(still.counters().get(REORDER).copied(), Some(1));
}

// ─── 13. Non-vacuity of the batches above ──────────────────────────────────

/// The agreement batches in sections 3, 7, 8 and 11 pass whether or not the
/// rewrite ever ran, so a representative of each family is pinned here to have
/// FIRED. Without this the suite could go green against a pass that had stopped
/// admitting anything at all.
#[test]
fn a_representative_of_every_agreement_batch_actually_fires() {
    let g = advg();
    let base = "MATCH (place:Country) \
                MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
                MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) ";
    let mut fired: Vec<(&str, u64)> = Vec::new();
    let mut probe = |label: &'static str, src: &str| {
        fired.push((label, counter(&g, src, REORDER)));
    };
    probe(
        "two-var inequality conjunct",
        &format!("{base}WHERE p1 <> p2 RETURN count(*) AS n"),
    );
    probe(
        "edge-probe conjunct",
        &format!("{base}WHERE NOT (p1)-[:KNOWS]-(p2) RETURN count(*) AS n"),
    );
    probe(
        "relationship variable",
        "MATCH (place:Country) \
         MATCH (city:City)-[r:IS_PART_OF]->(place) \
         MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
         RETURN count(*) AS n",
    );
    probe(
        "multi-type hop",
        "MATCH (place:Country) \
         MATCH (city:City)-[:IS_PART_OF|IS_LOCATED_IN]-(place) \
         MATCH (p:Person)-[:IS_LOCATED_IN]->(city) \
         RETURN count(*) AS n",
    );
    probe(
        "four paths sharing endpoints",
        "MATCH (place:Country) \
         MATCH (p1:Person)-[:IS_LOCATED_IN]->(c1:City)-[:IS_PART_OF]->(place) \
         MATCH (p2:Person)-[:IS_LOCATED_IN]->(c2:City)-[:IS_PART_OF]->(place) \
         MATCH (p1)-[:KNOWS]-(p2) \
         RETURN count(*) AS n",
    );
    let silent: Vec<&str> = fired
        .iter()
        .filter(|(_, n)| *n == 0)
        .map(|(l, _)| *l)
        .collect();
    assert!(silent.is_empty(), "these families never fired: {silent:?}");
    // The UNTYPED family (`-[]-`) is the one the reorder declines on this
    // fixture — its source order already reaches a recognised plan and the
    // rewrite saves no column. Its non-vacuity comes from
    // `randomised_count_patterns_match_the_interpreter`, whose generator emits
    // `-[]-` among its six hop spellings across ~2 000 firing cases.
}

/// The same for the OPTIONAL fold's own inline-map / self-loop family.
#[test]
fn a_representative_optional_leg_of_every_batch_actually_folds() {
    let g = advg();
    let mut folded: Vec<(&str, u64)> = Vec::new();
    let mut probe = |label: &'static str, src: &str| {
        folded.push((label, counter(&g, src, OPT_FOLD)));
    };
    probe(
        "one-hop leg",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN count(*) AS n",
    );
    probe(
        "two comma paths in one clause",
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person), (a)-[:IS_LOCATED_IN]->(c:City) \
         RETURN count(*) AS n",
    );
    probe(
        "close onto the outer var over a self-loop",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(a) RETURN count(*) AS n",
    );
    probe(
        "two-hop leg closing back onto the outer var",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]-(b:Person)-[:KNOWS]-(a) \
         RETURN count(*) AS n",
    );
    probe(
        "three-hop leg",
        "MATCH (a:Person) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b:Person)-[:IS_LOCATED_IN]->(c:City)\
         -[:IS_PART_OF]->(place:Country) RETURN count(*) AS n",
    );
    let silent: Vec<&str> = folded
        .iter()
        .filter(|(_, n)| *n == 0)
        .map(|(l, _)| *l)
        .collect();
    assert!(silent.is_empty(), "these legs never folded: {silent:?}");
}

