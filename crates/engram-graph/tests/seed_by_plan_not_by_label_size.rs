#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The scan root must be chosen by the PLAN it leads to, not by which label is
//! smallest.
//!
//! # The defect this pins
//!
//! `reorder_pattern` picked the endpoint var whose label has the fewest nodes.
//! On LSQB q3 that is `Country` (~111 at SF1, against 9,892 `Person`), and it
//! is the worst possible root:
//!
//! ```cypher
//! MATCH (country:Country)
//! MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country)
//! MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country)
//! MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country)
//! MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1)
//! RETURN count(*)
//! ```
//!
//! From `country`, the three location paths MULTIPLY — each country reaches
//! ~89 people, so 111 x 89 x 89 x 89 is ~78M rows before the triangle closes.
//! Profiled at SF1: **107,386,500** `graph.adjacency tables reused`.
//!
//! Seeding at `person1` and taking the KNOWS triangle FIRST peaks at ~12.8M and
//! then closes each location path as a semijoin onto the bound country. Same
//! answer, ~6x less work — and it is an ordering the peak search could already
//! express. It was simply never offered that starting point.
//!
//! # What this test can and cannot show
//!
//! It ASSERTS the correctness property: the seed and ordering are a plan
//! choice, so the count may not move. That holds at any scale.
//!
//! It only REPORTS the work. This fixture measurably does NOT pose the q3
//! question — both arms do ~24k adjacency rows on it, and the per-hop
//! arithmetic predicting 41,472 against 7,200 matches neither. A 72-person
//! graph is dense enough that the two plans cost about the same, and tuning the
//! fixture until it agreed with the prediction would be fitting the instrument
//! to the answer.
//!
//! **The gate for the work claim is LSQB q3 on the real SF1 corpus**, and it
//! was run: N=3 medians, searching seeds freely, gave q3 21,727 -> **15,870 ms**
//! (-27%) — the seed WAS the defect — while costing q4 +53%, q5 +32%, q2 +25%
//! and q8 +22%. Gating the swap behind a 4.0 margin on the predicted peak
//! recovered NEITHER: q3 went back to 21,204 and the other four stayed
//! regressed, which also proves the four were never about the seed CHOICE.
//!
//! The seed search is therefore REVERTED and `reorder_pattern` keeps the
//! smallest-label rule. This test remains because its assertion — the plan
//! choice may not move the count — holds either way, and because the next
//! person to have this idea should find the measurement rather than repeat it.
//! Closing q3 properly needs the triangle-listing operator, not a seed rule.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const Q3: &str = "MATCH (country:Country) \
                  MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
                  MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
                  MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
                  MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
                  RETURN count(*) AS count";

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

/// Few countries, more cities, many people — SF1's ratio, which is what makes
/// `Country` the smallest label and the worst seed.
const COUNTRIES: i64 = 3;
const CITIES_PER_COUNTRY: i64 = 4;
const PEOPLE_PER_CITY: i64 = 6;
const FRIENDS: i64 = 5;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for c in 0..COUNTRIES {
        run(&g, &format!("CREATE (:Country {{id: {c}}})"));
        for t in 0..CITIES_PER_COUNTRY {
            let city = c * CITIES_PER_COUNTRY + t;
            run(&g, &format!("CREATE (:City {{id: {city}}})"));
            run(
                &g,
                &format!(
                    "MATCH (x:City {{id: {city}}}), (y:Country {{id: {c}}}) \
                     CREATE (x)-[:IS_PART_OF]->(y)"
                ),
            );
        }
    }
    let people = COUNTRIES * CITIES_PER_COUNTRY * PEOPLE_PER_CITY;
    for p in 0..people {
        let city = p / PEOPLE_PER_CITY;
        run(&g, &format!("CREATE (:Person {{id: {p}}})"));
        run(
            &g,
            &format!(
                "MATCH (x:Person {{id: {p}}}), (y:City {{id: {city}}}) \
                 CREATE (x)-[:IS_LOCATED_IN]->(y)"
            ),
        );
    }
    // A ring with chords — dense enough that triangles exist.
    for p in 0..people {
        for k in 1..=FRIENDS {
            let q = (p + k) % people;
            run(
                &g,
                &format!(
                    "MATCH (a:Person {{id: {p}}}), (b:Person {{id: {q}}}) CREATE (a)-[:KNOWS]->(b)"
                ),
            );
        }
    }
    g.shared_store().seal();
    g
}

fn count_and_work(g: &Graph) -> (i64, u64) {
    let q = parse_statement(Q3).expect("parse");
    let (r, t) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
    let r = r.expect("run q3");
    let count = match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("q3 must answer one Int, got {other:?}"),
    };
    let work = t
        .counters()
        .get("graph.adjacency tables reused")
        .copied()
        .unwrap_or(0);
    let c = |k: &str| t.counters().get(k).copied().unwrap_or(0);
    eprintln!(
        "        [why] count-only reordered={} peak-search={} pipeline-agg={} fold={}",
        c("pipeline.count-only reordered"),
        c("pipeline.ordering chosen by peak search"),
        c("interp.pipeline aggregate runs"),
        c("interp.pipeline count fold"),
    );
    (count, work)
}

#[test]
fn the_triangle_is_not_rooted_at_the_smallest_label() {
    let g = fixture();

    engram_graph::pipeline::set_order_peak_search(false);
    let (greedy_count, greedy_work) = count_and_work(&g);
    engram_graph::pipeline::set_order_peak_search(true);
    let (search_count, search_work) = count_and_work(&g);

    eprintln!(
        "[q3-shape] smallest-label seed: count {greedy_count}, {greedy_work} adjacency rows\n\
         [q3-shape] searched seed:       count {search_count}, {search_work} adjacency rows"
    );

    // THE ASSERTION THAT MATTERS: the seed and ordering are a PLAN choice, so
    // the answer may not move. A difference here is a wrong answer, not a
    // slower one.
    assert_eq!(
        search_count, greedy_count,
        "searching over seeds changed the ANSWER; it may only change the plan"
    );
    assert!(
        greedy_count > 0,
        "the fixture must produce triangles inside a country, or both arms \
         trivially agree and this test poses no question"
    );

    // Both arms must actually DO the traversal, or the counts above agree
    // vacuously. The work RATIO is reported, not asserted: see the module docs
    // for why this fixture cannot carry that claim.
    assert!(
        greedy_work > 0 && search_work > 0,
        "both arms must traverse adjacency, or neither plan ran"
    );
    eprintln!(
        "[q3-shape] work ratio search/greedy = {:.2} (REPORTED, not asserted          — this fixture does not reproduce SF1's ratios)",
        search_work as f64 / greedy_work as f64
    );
}
