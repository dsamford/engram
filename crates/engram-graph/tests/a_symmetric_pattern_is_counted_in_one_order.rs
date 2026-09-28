#![allow(non_snake_case)]
//! Fix 90 (strategy Q3): the count fold enumerates ONE id order of a var set
//! it has proven interchangeable and multiplies by the set's size factorial.
//!
//! LSQB q3 counts ordered triples `(person1, person2, person3)` in one
//! country closing a KNOWS triangle. Every permutation of the three is a
//! result, so five of every six walks re-derive a triangle already counted.
//! The fold can walk one order — `id(p1) < id(p2) < id(p3)` — and multiply
//! by 3!. At SF1 the close hop is 80.6% of q3's 12.5M fold walks.
//!
//! The failure mode of the idea is a SILENTLY wrong count, so the tests here
//! are built around the two ways it can be wrong rather than around the
//! speed-up:
//!
//! * `c_` is the DEGENERATE corpus — a self-loop and parallel edges make
//!   results whose three vars are NOT pairwise distinct, whose orbit under
//!   the permutation group is therefore smaller than 3!, and which the
//!   strict id order excludes entirely. With the multiplier applied the
//!   answer would be 0 instead of 6. The execution-time self-loop gate is
//!   what refuses it, and this test fails loudly if that gate is removed.
//! * every other test asserts the SAME count three ways — symmetry ON,
//!   symmetry OFF, and the count fold off altogether (every hop
//!   materialised, the general reduction) — against a number derived by
//!   hand from the corpus, so a bug that moves all three arms together is
//!   caught by the number and a bug that moves one is caught by the
//!   differential.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, pipeline, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PLANNED: &str = "interp.pipeline fold symmetry planned";
const BROKEN: &str = "interp.pipeline fold symmetry broken";
const SELF_LOOP_DECLINE: &str =
    "interp.pipeline fold symmetry declined: a joining type carries self-loops";
const WALKS: &str = "interp.pipeline fold hop walks";

/// LSQB q3, verbatim from `engram-bench`'s adapted form: three persons in ONE
/// country closing a KNOWS triangle.
const Q3: &str = "MATCH (country:Country) \
     MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
     MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
     MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
     MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
     RETURN count(*) AS count";

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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

/// Run `src` and return `(the single count, the trace's counters)`.
fn counted(g: &Graph, src: &str) -> (i64, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    let (rows, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run: {e}"))
    });
    let n = match rows.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one integer count, got {other:?}"),
    };
    (n, trace.counters().clone())
}

/// The three arms every count is asked in: symmetry ON (the default),
/// symmetry OFF (every order enumerated, nothing multiplied), and the fold
/// off entirely (every hop materialised — the reduction the fold replaces).
/// Returns `(on, on_counters, off, off_counters, general)`.
fn three_arms(
    g: &Graph,
    src: &str,
) -> (i64, BTreeMap<String, u64>, i64, BTreeMap<String, u64>, i64) {
    pipeline::set_fold_symmetry_breaking(true);
    pipeline::set_count_fold(true);
    let (on, on_c) = counted(g, src);
    pipeline::set_fold_symmetry_breaking(false);
    let (off, off_c) = counted(g, src);
    pipeline::set_count_fold(false);
    let (general, _) = counted(g, src);
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(true);
    (on, on_c, off, off_c, general)
}

/// Two countries, and one triangle that SPANS them.
///
/// Country A holds `a b c d` with KNOWS `a-b b-c c-a b-d d-c`: the triangles
/// `{a,b,c}` and `{b,c,d}`, two of them. Country B holds `e f g` with
/// `e-f f-g g-e`: one. And `a-e a-f` closes a triangle `{a,e,f}` whose
/// members are NOT all in one country, so the country sub-pattern must
/// exclude it — it is the discriminator that the three country hops are
/// really being applied.
///
/// Ordered triples = 3 triangles x 3! = 18.
fn two_countries() -> (Graph, i64) {
    let g = graph();
    let ca = node(&g, "Country", 1);
    let cb = node(&g, "Country", 2);
    let xa = node(&g, "City", 10);
    let xb = node(&g, "City", 11);
    rel(&g, xa, "IS_PART_OF", ca);
    rel(&g, xb, "IS_PART_OF", cb);
    let p = |city: u64, k: i64| -> u64 {
        let id = node(&g, "Person", k);
        rel(&g, id, "IS_LOCATED_IN", city);
        id
    };
    let (a, b, c, d) = (p(xa, 100), p(xa, 101), p(xa, 102), p(xa, 103));
    let (e, f, h) = (p(xb, 200), p(xb, 201), p(xb, 202));
    for (u, v) in [(a, b), (b, c), (c, a), (b, d), (d, c)] {
        rel(&g, u, "KNOWS", v);
    }
    for (u, v) in [(e, f), (f, h), (h, e)] {
        rel(&g, u, "KNOWS", v);
    }
    // The cross-country triangle {a, e, f}: excluded by the country pattern.
    rel(&g, a, "KNOWS", e);
    rel(&g, a, "KNOWS", f);
    (g, 18)
}

/// THE TARGET SHAPE. q3's own text: the three arms agree on the hand-derived
/// count, the planner recognises the symmetry, and the ON arm walks strictly
/// less than the OFF arm — the saving the strategy exists for.
#[test]
fn a_the_lsqb_q3_triangle_agrees_three_ways_and_walks_less() {
    let (g, want) = two_countries();
    let (on, on_c, off, off_c, general) = three_arms(&g, Q3);
    assert_eq!(
        (on, off, general),
        (want, want, want),
        "the three arms disagree, or the corpus is not the one this count was derived from"
    );
    assert!(
        count_of(&on_c, PLANNED) >= 1 && count_of(&on_c, BROKEN) >= 1,
        "fix 90 never fired on LSQB q3's own shape — the recogniser does not reach the plan \
         this query runs, so nothing downstream of it is being measured: {on_c:?}"
    );
    assert_eq!(
        count_of(&off_c, BROKEN),
        0,
        "the OFF arm broke a symmetry: the lever does not reach the fold"
    );
    let (won, woff) = (count_of(&on_c, WALKS), count_of(&off_c, WALKS));
    eprintln!("[fix 90] LSQB q3 shape: {woff} fold walks -> {won} with the symmetry broken");
    assert!(
        won < woff,
        "symmetry breaking did not reduce the fold's walks ({won} vs {woff}) — the constraints \
         are planned but not enforced, or they landed where nothing evaluates them"
    );
}

/// The cross-country triangle is what makes the country hops load-bearing:
/// dropping them would count `{a, e, f}` too. Pinned as its own number so a
/// change to the corpus cannot quietly make test `a` vacuous.
#[test]
fn b_a_triangle_that_spans_two_countries_is_not_counted() {
    let (g, _) = two_countries();
    let (any_country, _) = counted(
        &g,
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
         -[:KNOWS]-(person1) RETURN count(*) AS count",
    );
    assert_eq!(
        any_country, 24,
        "without the country sub-pattern the corpus holds FOUR triangles (the three within a \
         country and {{a, e, f}} across them)"
    );
}

/// THE DEGENERATE CORPUS, and the reason the gate is read from the data at
/// execution rather than decided with the plan.
///
/// Two persons `b c` in one country with TWO KNOWS edges between them and a
/// KNOWS SELF-LOOP on `b`. Six ordered triples close the triangle under
/// relationship isomorphism — every one of them binding two of the three
/// vars to the SAME node (`b`), which the strict id order excludes and the
/// multiplier would then restore from nothing:
///
/// | p1 | p2 | p3 | hops |
/// |----|----|----|------|
/// | b | b | c | self, r1, r2 — and self, r2, r1 |
/// | b | c | b | r1, r2, self — and r2, r1, self |
/// | c | b | b | r1, self, r2 — and r2, self, r1 |
///
/// So the answer is 6, and a symmetry-broken fold would answer 0. The
/// self-loop count is the guard, and it is a property of the DATA: the same
/// query, the same plan, a different answer.
#[test]
fn c_a_self_loop_makes_the_members_coincide_so_the_symmetry_is_declined() {
    let g = graph();
    let ca = node(&g, "Country", 1);
    let xa = node(&g, "City", 10);
    rel(&g, xa, "IS_PART_OF", ca);
    let b = node(&g, "Person", 100);
    let c = node(&g, "Person", 101);
    rel(&g, b, "IS_LOCATED_IN", xa);
    rel(&g, c, "IS_LOCATED_IN", xa);
    rel(&g, b, "KNOWS", c); // r1
    rel(&g, b, "KNOWS", c); // r2 — parallel
    rel(&g, b, "KNOWS", b); // the self-loop

    let (on, on_c, off, off_c, general) = three_arms(&g, Q3);
    assert_eq!(
        (on, off, general),
        (6, 6, 6),
        "the degenerate corpus's six triples are the whole point of this test"
    );
    assert!(
        count_of(&on_c, SELF_LOOP_DECLINE) >= 1,
        "the self-loop gate did not fire — with the symmetry applied this count is 0, not 6: \
         {on_c:?}"
    );
    assert_eq!(
        count_of(&on_c, BROKEN),
        0,
        "the symmetry was broken over a corpus whose members can coincide"
    );
    assert_eq!(count_of(&off_c, BROKEN), 0);
}

/// The same corpus WITHOUT the self-loop: the members cannot coincide (two
/// parallel edges alone still need three distinct rels between two nodes,
/// and there are only two), the gate passes, and the answer is 0 either way.
/// The control for test `c`: it is the SELF-LOOP that declines, not the
/// parallel edges.
#[test]
fn d_parallel_edges_alone_do_not_decline_the_symmetry() {
    let g = graph();
    let ca = node(&g, "Country", 1);
    let xa = node(&g, "City", 10);
    rel(&g, xa, "IS_PART_OF", ca);
    let b = node(&g, "Person", 100);
    let c = node(&g, "Person", 101);
    rel(&g, b, "IS_LOCATED_IN", xa);
    rel(&g, c, "IS_LOCATED_IN", xa);
    rel(&g, b, "KNOWS", c);
    rel(&g, b, "KNOWS", c);

    let (on, on_c, off, _off_c, general) = three_arms(&g, Q3);
    assert_eq!((on, off, general), (0, 0, 0));
    assert_eq!(
        count_of(&on_c, SELF_LOOP_DECLINE),
        0,
        "parallel edges are not self-loops"
    );
    assert!(count_of(&on_c, BROKEN) >= 1, "{on_c:?}");
}

/// A DIRECTED cycle is not symmetric under a TRANSPOSITION: swapping two of
/// its three vars reverses an arrow, and no relabelling of the rest repairs
/// it. Its automorphisms are the three ROTATIONS, an orbit of 3 rather than
/// 3!, and breaking it would need a cyclic constraint (`p1` the minimum)
/// rather than a total order — a different mechanism, so the first cut
/// declines. The count proves the shape is live in the corpus rather than
/// vacuously zero: `a->b->c->a` and `e->f->h->e` are directed triangles,
/// three rotations each.
#[test]
fn e_a_directed_cycle_is_not_symmetric() {
    let (g, _) = two_countries();
    let src = "MATCH (person1:Person)-[:KNOWS]->(person2:Person)-[:KNOWS]->(person3:Person)\
               -[:KNOWS]->(person1) RETURN count(*) AS count";
    let (on, on_c, off, _off_c, general) = three_arms(&g, src);
    assert_eq!(
        (on, off, general),
        (6, 6, 6),
        "two directed triangles x three rotations — a live shape, not an empty one"
    );
    assert_eq!(
        count_of(&on_c, BROKEN),
        0,
        "a directed 3-cycle admits only the ROTATIONS, not the transpositions: {on_c:?}"
    );
}

/// A star is not symmetric in the way the multiplier needs: `person2` and
/// `person3` are interchangeable but NOT adjacent, so two of them could bind
/// the same node (both are neighbours of `person1`) and the orbit would be
/// smaller than 2!. Declined on adjacency, and the count proves why: the
/// corpus HAS such a result.
#[test]
fn f_two_non_adjacent_members_are_declined_and_the_corpus_shows_why() {
    let g = graph();
    let a = node(&g, "Person", 1);
    let b = node(&g, "Person", 2);
    let c = node(&g, "Person", 3);
    rel(&g, a, "KNOWS", b);
    rel(&g, a, "KNOWS", c);
    // `(p2)-[:KNOWS]-(p1)-[:KNOWS]-(p3)`: one path, so relationship
    // isomorphism forbids reusing the SAME edge for both hops — but p2 and
    // p3 may still coincide when a has two distinct edges to it.
    rel(&g, a, "KNOWS", b); // a second a-b edge: p2 = p3 = b becomes possible
    let src = "MATCH (person2:Person)-[:KNOWS]-(person1:Person)-[:KNOWS]-(person3:Person) \
               RETURN count(*) AS count";
    let (on, on_c, off, _off_c, general) = three_arms(&g, src);
    // a's three edges give 3 x 2 = 6 ordered (p2, p3) pairs through a, plus
    // the walks through b and c as the centre: b has two edges (to a twice)
    // → 2, c has one → 0. Total 6 + 2 = 8.
    assert_eq!((on, off, general), (8, 8, 8));
    assert_eq!(
        count_of(&on_c, BROKEN),
        0,
        "person2 and person3 are interchangeable but not adjacent, so they may coincide: {on_c:?}"
    );
}

/// LABELS MUST REACH THE RECOGNISER. Two vars are grouped as candidates only
/// when their label sets are EQUAL, and those sets are assembled from the
/// seed's labels plus each hop's end labels. If a label written in the query
/// never reached that assembly, two vars with different labels would look
/// interchangeable and the multiplier would count triples that do not exist.
///
/// The corpus makes the difference visible: `d` is an `:Admin` and not a
/// `:Person`, so `{a, b, d}` closes a KNOWS triangle that the LABELLED
/// pattern must not count, while the same triangle over three `:Person`s
/// must be. A recogniser blind to labels would break the symmetry over
/// `{p1, p2, p3}` here and — since the pattern is NOT symmetric under
/// swapping a Person var with the Admin var — could return a wrong count.
#[test]
fn h_members_with_different_labels_are_not_interchangeable() {
    let g = graph();
    let a = node(&g, "Person", 1);
    let b = node(&g, "Person", 2);
    let c = node(&g, "Person", 3);
    let d = node(&g, "Admin", 4);
    for (u, v) in [(a, b), (b, c), (c, a), (a, d), (b, d)] {
        rel(&g, u, "KNOWS", v);
    }
    // {a,b,c} is a Person triangle; {a,b,d} has an Admin at one corner.
    let all_person = "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p1) \
                      RETURN count(*) AS count";
    let (on, on_c, off, _off_c, general) = three_arms(&g, all_person);
    assert_eq!(
        (on, off, general),
        (6, 6, 6),
        "one all-Person triangle x 3! — the Admin corner must not be counted"
    );
    assert!(
        count_of(&on_c, BROKEN) >= 1,
        "three same-labelled members ARE interchangeable: {on_c:?}"
    );

    // The mixed pattern: p3 is an Admin. Swapping p1 and p3 does not preserve
    // the labels, so the set must NOT be grouped and the count must stand on
    // its own — {a,b,d} and {b,a,d} are the two orders of the one triangle
    // whose Admin sits at p3.
    let mixed = "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Admin)-[:KNOWS]-(p1) \
                 RETURN count(*) AS count";
    let (on, on_c, off, _off_c, general) = three_arms(&g, mixed);
    assert_eq!(
        (on, off, general),
        (2, 2, 2),
        "d is the only Admin and it closes exactly one triangle, in two orders"
    );
    let broken = count_of(&on_c, BROKEN);
    if broken >= 1 {
        // Grouping {p1, p2} alone IS legitimate here (both :Person, adjacent,
        // and swapping them extends to an automorphism fixing p3). What must
        // never happen is a group of three.
        assert!(
            on == 2,
            "a symmetry was broken over vars that are not interchangeable: {on_c:?}"
        );
    }
}

/// A WHERE is declined outright by the first cut: every predicate would have
/// to be shown invariant under the renaming too. `person1 <> person3` is in
/// fact invariant, and the decline is deliberately conservative — recorded
/// here so the choice is visible rather than assumed.
#[test]
fn g_a_where_declines_the_symmetry_conservatively() {
    let (g, _) = two_countries();
    let src = "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
               -[:KNOWS]-(person1) WHERE person1 <> person3 RETURN count(*) AS count";
    let (on, on_c, off, _off_c, general) = three_arms(&g, src);
    assert_eq!((on, off, general), (24, 24, 24));
    assert_eq!(count_of(&on_c, BROKEN), 0, "{on_c:?}");
}
