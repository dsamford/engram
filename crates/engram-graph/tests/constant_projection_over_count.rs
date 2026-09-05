#![allow(non_snake_case)]
//! Differential tests for CONSTANT PROJECTION OVER THE COUNT
//! (`pipeline::constant_projection_over_count`): `MATCH … RETURN <literals/
//! params> [SKIP] [LIMIT]` emits the SAME row per match, so the statement is
//! answered from the pattern's match COUNT — the count-only join reorder's
//! plan, which walks the pattern from its selective end — and the one constant
//! row replayed `min(n − skip, limit)` times.
//!
//! The contract is the other differential suites': on every accepted shape the
//! lever ON (`Graph::set_const_projection_fold(true)`, the default) and OFF
//! (the enumerating general path, the pattern walked as written) must agree on
//! the full ROW SET, its order, AND the column names — byte for byte. On every
//! declined shape the counter must stay at zero and the answer must still be
//! the general path's.
//!
//! WHY THIS EXISTS: LSQB q3's existence probe (`… RETURN 1 LIMIT 1`) declined
//! every recogniser and fell to the general path, whose source-order walk is
//! cubic in the persons per country — 180 s at SF1 against a 4.5 s count of
//! the same pattern, on v69 and v71 alike. `lsqb` reports only the count's
//! millis, so the probe hid inside "q3 4.5 s" and surfaced as a hang.
//!
//! The fixture is q3's SHAPE: countries with cities with persons, and KNOWS
//! edges forming triangles inside some countries and NOT others, so that a
//! source-order walk and a reordered walk enumerate the matches in different
//! orders — which is exactly what the constant projection must hide.
use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const COUNTER: &str = "pipeline.constant projection answered from the count";

/// Three countries. Country 0: two cities, persons 0..4, KNOWS triangles
/// (0,1,2) and (1,2,3) plus a chord; country 1: one city, persons 4..6, one
/// triangle (4,5,6); country 2: one city, persons 7..8, a single KNOWS edge
/// (no triangle). Creation order fixes the general path's production order.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let e = BTreeMap::new();
    let node = |label: &str, id: i64| {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let countries: Vec<u64> = (0..3).map(|i| node("Country", i)).collect();
    // (city, country)
    let city_of = [(0usize, 0usize), (1, 0), (2, 1), (3, 2)];
    let cities: Vec<u64> = city_of.iter().map(|(c, _)| node("City", *c as i64)).collect();
    for (ci, (_, co)) in city_of.iter().enumerate() {
        g.create_rel(cities[ci], "IS_PART_OF", countries[*co], &e).expect("part");
    }
    // (person, city)
    let person_city = [0usize, 0, 1, 1, 2, 2, 2, 3, 3];
    let persons: Vec<u64> = (0..person_city.len()).map(|i| node("Person", i as i64)).collect();
    for (pi, ci) in person_city.iter().enumerate() {
        g.create_rel(persons[pi], "IS_LOCATED_IN", cities[*ci], &e).expect("loc");
    }
    for (a, b) in [
        (0usize, 1usize), (1, 2), (2, 0), // triangle 0,1,2
        (2, 3), (3, 1), // triangle 1,2,3 (shares 1-2)
        (0, 3), // chord: 0-3 — triangles 0,1,3? 0-1,1-3,3-0 yes; 0,2,3? 0-2,2-3,3-0 yes
        (4, 5), (5, 6), (6, 4), // triangle 4,5,6
        (7, 8), // no triangle
    ] {
        g.create_rel(persons[a], "KNOWS", persons[b], &e).expect("knows");
    }
    g
}

const Q3_PATTERN: &str = "MATCH (country:Country) \
    MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
    MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
    MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
    MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1)";

fn run(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

/// ON and OFF, each under a trace: the results and how often the path fired.
fn arms(g: &Graph, src: &str, params: &BTreeMap<String, Value>) -> (QueryResult, u64, QueryResult, u64) {
    g.set_const_projection_fold(true);
    let (on, t_on) = engram_observe::with_trace(|| run(g, src, params.clone()));
    g.set_const_projection_fold(false);
    let (off, t_off) = engram_observe::with_trace(|| run(g, src, params.clone()));
    g.set_const_projection_fold(true);
    let fired = |t: &engram_observe::Trace| t.counters().get(COUNTER).copied().unwrap_or(0);
    (on, fired(&t_on), off, fired(&t_off))
}

fn assert_identical_and_fired(g: &Graph, src: &str, params: BTreeMap<String, Value>) {
    let (on, fired_on, off, fired_off) = arms(g, src, &params);
    assert!(fired_on > 0, "`{src}`: the ON arm never took the path — this test proved nothing");
    assert_eq!(fired_off, 0, "`{src}`: the lever is off and the path still ran");
    assert_eq!(on.columns, off.columns, "`{src}`: column names differ");
    assert_eq!(on.rows, off.rows, "`{src}`: rows differ");
}

fn assert_declined(g: &Graph, src: &str, params: BTreeMap<String, Value>) {
    let (on, fired_on, off, fired_off) = arms(g, src, &params);
    assert_eq!(fired_on, 0, "`{src}`: the path must decline this shape");
    assert_eq!(fired_off, 0);
    assert_eq!((on.columns, on.rows), (off.columns, off.rows));
}

// ─── The probe, and its neighbours ──────────────────────────────────────────

#[test]
fn the_existence_probe_is_byte_identical_and_takes_the_path() {
    let g = fixture();
    let src = format!("{Q3_PATTERN} RETURN 1 LIMIT 1");
    assert_identical_and_fired(&g, &src, BTreeMap::new());
    let r = run(&g, &src, BTreeMap::new());
    assert_eq!(r.columns, vec!["1".to_string()], "an unaliased literal is named by its source text");
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn every_match_is_one_copy_of_the_constant_row() {
    // No LIMIT: one row per match. The fixture's triangles, each counted
    // over the undirected KNOWS walk from every rotation and direction, are
    // exactly what `count(*)` reports — pin the two agree.
    let g = fixture();
    let n = match run(&g, &format!("{Q3_PATTERN} RETURN count(*) AS c"), BTreeMap::new()).rows[0][0] {
        Value::Int(n) => n as usize,
        ref v => panic!("count returned {v:?}"),
    };
    assert!(n > 1, "the fixture must have several matches, got {n}");
    let src = format!("{Q3_PATTERN} RETURN true AS present, 'q3' AS tag");
    assert_identical_and_fired(&g, &src, BTreeMap::new());
    let r = run(&g, &src, BTreeMap::new());
    assert_eq!(r.columns, vec!["present".to_string(), "tag".to_string()]);
    assert_eq!(r.rows.len(), n);
    assert!(r.rows.iter().all(|row| row == &vec![Value::Bool(true), Value::Str("q3".into())]));
}

#[test]
fn skip_and_limit_cut_the_run_of_identical_rows() {
    let g = fixture();
    let n = run(&g, &format!("{Q3_PATTERN} RETURN 1"), BTreeMap::new()).rows.len();
    for (skip, limit) in [(0usize, 3usize), (2, 4), (n - 1, 5), (n, 1), (n + 7, 2), (0, 0)] {
        let src = format!("{Q3_PATTERN} RETURN 1 SKIP {skip} LIMIT {limit}");
        assert_identical_and_fired(&g, &src, BTreeMap::new());
        let got = run(&g, &src, BTreeMap::new()).rows.len();
        assert_eq!(got, n.saturating_sub(skip).min(limit), "`{src}`");
    }
    // SKIP alone.
    let src = format!("{Q3_PATTERN} RETURN 1 SKIP 2");
    assert_identical_and_fired(&g, &src, BTreeMap::new());
    assert_eq!(run(&g, &src, BTreeMap::new()).rows.len(), n - 2);
}

#[test]
fn parameters_are_constants_too_in_the_items_and_in_the_cut() {
    let g = fixture();
    let mut params = BTreeMap::new();
    params.insert("marker".to_string(), Value::Str("hit".into()));
    params.insert("k".to_string(), Value::Int(2));
    let src = format!("{Q3_PATTERN} RETURN $marker AS m, 42 LIMIT $k");
    assert_identical_and_fired(&g, &src, params.clone());
    let r = run(&g, &src, params);
    assert_eq!(r.columns, vec!["m".to_string(), "42".to_string()]);
    assert_eq!(r.rows, vec![vec![Value::Str("hit".into()), Value::Int(42)]; 2]);
}

#[test]
fn zero_matches_give_zero_rows_with_the_columns_still_named() {
    // Persons 7 and 8 share one KNOWS edge and no triangle: a WHERE pinning
    // the country to theirs matches nothing.
    let g = fixture();
    let src = format!("{Q3_PATTERN} WHERE country.id = 2 RETURN 1 AS one LIMIT 1");
    assert_identical_and_fired(&g, &src, BTreeMap::new());
    let r = run(&g, &src, BTreeMap::new());
    assert_eq!(r.columns, vec!["one".to_string()]);
    assert!(r.rows.is_empty());
}

#[test]
fn a_where_on_the_pattern_is_honoured() {
    // Only country 1's triangle (persons 4,5,6) — the count must fall with it.
    let g = fixture();
    let all = run(&g, &format!("{Q3_PATTERN} RETURN 1"), BTreeMap::new()).rows.len();
    let src = format!("{Q3_PATTERN} WHERE country.id = 1 RETURN 1");
    assert_identical_and_fired(&g, &src, BTreeMap::new());
    let some = run(&g, &src, BTreeMap::new()).rows.len();
    assert!(some > 0 && some < all, "WHERE must select a strict subset: {some} of {all}");
}

#[test]
fn a_single_path_pattern_is_accepted_too() {
    let g = fixture();
    let src = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN 1 LIMIT 1";
    assert_identical_and_fired(&g, src, BTreeMap::new());
}

// ─── Declines: anything a row could make differ ─────────────────────────────

#[test]
fn a_projected_variable_or_property_declines() {
    let g = fixture();
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN person1.id LIMIT 1"), BTreeMap::new());
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN 1, country LIMIT 1"), BTreeMap::new());
}

#[test]
fn a_function_call_declines_even_when_pure() {
    // `toUpper('x')` is constant, but the admission is literals/params only:
    // `rand()` and `timestamp()` look the same to a shape check and would
    // replicate one draw across every row.
    let g = fixture();
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN toUpper('x') LIMIT 1"), BTreeMap::new());
    // `rand()` cannot be compared run-to-run (that is the point); only the
    // decline is checkable.
    let (_, fired_on, _, fired_off) = arms(&g, &format!("{Q3_PATTERN} RETURN rand() LIMIT 1"), &BTreeMap::new());
    assert_eq!((fired_on, fired_off), (0, 0), "rand() must never ride the replay");
}

#[test]
fn distinct_order_by_and_star_decline() {
    let g = fixture();
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN DISTINCT 1"), BTreeMap::new());
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN 1 ORDER BY 1 LIMIT 1"), BTreeMap::new());
    assert_declined(&g, &format!("{Q3_PATTERN} RETURN *, 1 LIMIT 1"), BTreeMap::new());
}

#[test]
fn an_aggregate_is_not_a_constant() {
    // `count(*)` is the count-only reorder's own shape; it must not re-enter
    // this pass (termination) and must still answer.
    let g = fixture();
    let src = format!("{Q3_PATTERN} RETURN count(*) AS c");
    let (on, fired, _, _) = arms(&g, &src, &BTreeMap::new());
    assert_eq!(fired, 0);
    assert!(matches!(on.rows[0][0], Value::Int(n) if n > 0));
}

#[test]
fn a_limited_probe_stops_the_fold_at_its_cap_and_a_count_never_does() {
    // The existence probe walks only as far as its first matches: with
    // LIMIT 1 the fold stops after the first driving row whose weight is
    // ≥ 1 and leaves the other countries unwalked. Measured reason on the
    // pod: a probe that folds the WHOLE relation set for q1/q6/q7/q9
    // displaced enough block cache to cost the next q3 count +12%.
    let g = fixture();
    let cap_hits = |src: &str| {
        let (_, t) = engram_observe::with_trace(|| run(&g, src, BTreeMap::new()));
        t.counters().get("pipeline.count fold stopped at the probe cap").copied().unwrap_or(0)
    };
    assert!(cap_hits(&format!("{Q3_PATTERN} RETURN 1 LIMIT 1")) > 0, "LIMIT 1 must stop the fold early");
    assert_eq!(cap_hits(&format!("{Q3_PATTERN} RETURN count(*) AS c")), 0, "a count sums every row");
    assert_eq!(cap_hits(&format!("{Q3_PATTERN} RETURN 1")), 0, "no LIMIT, no cap: every copy is owed");
    // A SKIP past every match leaves the cap unreached: the exact total, zero rows.
    let n = run(&g, &format!("{Q3_PATTERN} RETURN 1"), BTreeMap::new()).rows.len();
    assert_eq!(cap_hits(&format!("{Q3_PATTERN} RETURN 1 SKIP {} LIMIT 2", n + 3)), 0);
    assert!(run(&g, &format!("{Q3_PATTERN} RETURN 1 SKIP {} LIMIT 2", n + 3), BTreeMap::new()).rows.is_empty());
    // And the capped answer is still byte-identical to the general path's.
    assert_identical_and_fired(&g, &format!("{Q3_PATTERN} RETURN 1 LIMIT 1"), BTreeMap::new());
}

#[test]
fn the_canary_bites_on_a_miscounted_cut() {
    // The differential's teeth: a replay that produced one row too many or
    // too few would differ from the general path's row count. Prove the
    // comparison is live by checking the two arms disagree when the
    // statement is changed between them.
    let g = fixture();
    g.set_const_projection_fold(true);
    let on = run(&g, &format!("{Q3_PATTERN} RETURN 1 LIMIT 3"), BTreeMap::new());
    g.set_const_projection_fold(false);
    let off = run(&g, &format!("{Q3_PATTERN} RETURN 1 LIMIT 2"), BTreeMap::new());
    g.set_const_projection_fold(true);
    assert_ne!(on.rows, off.rows, "the arms must be able to disagree, or the equality above is vacuous");
}

// ─── OPTIONAL MATCH legs (the q7 shape) ─────────────────────────────────────

/// The q7 shape over the fixture's own vocabulary: every person's KNOWS
/// partners, then OPTIONAL legs whose multiplicity varies per outer row —
/// a second KNOWS hop (0..n matches) and a leg no node satisfies (0 matches,
/// so the row survives once). Persons 7 and 8 have exactly one KNOWS
/// partner each and no second hop past it; the triangles fan out.
const Q7_SHAPE: &str = "MATCH (a:Person)-[:KNOWS]-(b:Person) \
    OPTIONAL MATCH (b)-[:KNOWS]-(c:Person) \
    OPTIONAL MATCH (b)-[:IS_LOCATED_IN]->(:Country)";

#[test]
fn optional_legs_ride_along_and_the_probe_no_longer_enumerates() {
    // lsqb derives q7's probe as `MATCH … OPTIONAL MATCH … OPTIONAL MATCH …
    // RETURN 1 LIMIT 1`; at SF3 the general path materialised the
    // HAS_TAG × LIKES fan-out past the 20M-row budget before LIMIT could
    // stop it, while the count of the same clauses answered in 6 s through
    // the optional fold. Same rows as the general path, and the path fires.
    let g = fixture();
    assert_identical_and_fired(&g, &format!("{Q7_SHAPE} RETURN 1 LIMIT 1"), BTreeMap::new());
    assert_identical_and_fired(&g, &format!("{Q7_SHAPE} RETURN 1"), BTreeMap::new());
    assert_identical_and_fired(&g, &format!("{Q7_SHAPE} RETURN 'x' AS k SKIP 2 LIMIT 5"), BTreeMap::new());
    // The optional legs' multiplicity is part of the count: the row set is
    // larger than the leading MATCH alone, or the legs were dropped.
    let with = run(&g, &format!("{Q7_SHAPE} RETURN 1"), BTreeMap::new()).rows.len();
    let without = run(&g, "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN 1", BTreeMap::new()).rows.len();
    assert!(with > without, "optional legs multiplied nothing: {with} vs {without}");
}

#[test]
fn an_optional_match_before_the_leading_match_declines() {
    // Only trailing OPTIONAL clauses are recognised: a leading one is a
    // different statement (its rows survive an empty pattern).
    let g = fixture();
    assert_declined(
        &g,
        "OPTIONAL MATCH (a:Person)-[:KNOWS]-(b:Person) MATCH (b)-[:IS_LOCATED_IN]->(:City) RETURN 1",
        BTreeMap::new(),
    );
}
