//! Fix 90, the RANDOMISED arm: the same count asked three ways over 180
//! generated graphs, so the symmetry-breaking fold has to agree with the two
//! executions it is supposed to be indistinguishable from.
//!
//! `a_symmetric_pattern_is_counted_in_one_order` pins hand-derived numbers on
//! corpora chosen to make each mechanism fire. That is the right shape for
//! proving a mechanism, and the wrong shape for finding the case nobody
//! thought of — every corpus in it is one I designed while holding the
//! implementation in my head, which is exactly the blind spot a symmetry
//! argument has. Fix 90's failure mode is a SILENT WRONG COUNT: it divides
//! the enumeration by |S|! and multiplies the answer back, so an unsound
//! automorphism proof does not crash, it returns a plausible number.
//!
//! So this suite generates graphs instead of choosing them, and asks each
//! pattern under all three executions:
//!
//! * symmetry ON  — the fold enumerates one id order and multiplies,
//! * symmetry OFF — the fold enumerates every order (the fix 84 baseline),
//! * fold OFF     — the general path, which shares no code with either.
//!
//! Any disagreement is a defect, and the third arm is what makes that true:
//! ON and OFF are two configurations of the same fold, so agreement between
//! them alone would only prove the multiplier is self-consistent.
//!
//! The generator deliberately produces the data the exactness argument rests
//! on being able to handle: SELF-LOOPS (the one way two interchangeable vars
//! can bind the same node — the gate must decline), PARALLEL edges, a
//! self-loop that is created and then DELETED (so the `self_loops_by_type`
//! decrement is exercised, not just the increment), members split across two
//! countries, and patterns whose symmetric set is 2, 3 and 4 wide.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, pipeline, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

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

fn one(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let rows = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e}"));
    match rows.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected int, got {other:?} for {src}"),
    }
}

/// The three executions. The levers are thread-locals, so a test binary
/// running these suites in parallel does not race — each thread carries its
/// own, and this one restores both before it returns.
fn arms(g: &Graph, src: &str) -> (i64, i64, i64) {
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(true);
    let on = one(g, src);
    pipeline::set_fold_symmetry_breaking(false);
    let off = one(g, src);
    pipeline::set_count_fold(false);
    let general = one(g, src);
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(true);
    (on, off, general)
}

/// xorshift64. A seeded generator, not `rand`: a failure has to be
/// reproducible from the seed the message prints.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const PATTERNS: &[&str] = &[
    // plain undirected triangle, all Person — the 3-wide symmetric set
    "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p1) RETURN count(*) AS c",
    // triangle with an extra shared neighbour: LSQB q3's country shape
    "MATCH (c:Country) MATCH (p1:Person)-[:LIVES_IN]->(c) MATCH (p2:Person)-[:LIVES_IN]->(c) \
     MATCH (p3:Person)-[:LIVES_IN]->(c) MATCH (p1)-[:KNOWS]-(p2)-[:KNOWS]-(p3)-[:KNOWS]-(p1) \
     RETURN count(*) AS c",
    // two-member symmetric set: p1,p2 swap, p3 is a different label and fixed
    "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Admin)-[:KNOWS]-(p1) RETURN count(*) AS c",
    // 4-clique — a 4-wide set, so the multiplier is 24 and not 6
    "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p4:Person)-[:KNOWS]-(p1) \
     MATCH (p1)-[:KNOWS]-(p3) MATCH (p2)-[:KNOWS]-(p4) RETURN count(*) AS c",
    // untyped joining hop: the self-loop gate has to read EVERY type
    "MATCH (p1:Person)-[]-(p2:Person)-[]-(p3:Person)-[]-(p1) RETURN count(*) AS c",
    // two type alternatives on the join
    "MATCH (p1:Person)-[:KNOWS|LIKES]-(p2:Person)-[:KNOWS|LIKES]-(p3:Person)-[:KNOWS|LIKES]-(p1) \
     RETURN count(*) AS c",
    // anonymous middle nodes — no var to fold, so nothing may be broken
    "MATCH (a:Person)-[:KNOWS]-()-[:KNOWS]-()-[:KNOWS]-(a) RETURN count(*) AS c",
    // triangle with a pendant per member: the sub-patterns must map too
    "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p1) \
     MATCH (p1)-[:LIKES]->(t1:Tag) MATCH (p2)-[:LIKES]->(t2:Tag) MATCH (p3)-[:LIKES]->(t3:Tag) \
     RETURN count(*) AS c",
];

#[test]
fn a_symmetry_broken_count_agrees_with_both_other_executions() {
    let mut bad = Vec::new();
    let mut nonzero = 0usize;
    for seed in 1..=180u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let g = graph();
        let nperson = 3 + r.below(4) as i64; // 3..6
        let nadmin = r.below(3) as i64; // 0..2
        let country = node(&g, "Country", 900);
        let country2 = node(&g, "Country", 901);
        let mut people: Vec<u64> = Vec::new();
        for i in 0..nperson {
            let p = node(&g, "Person", i);
            rel(
                &g,
                p,
                "LIVES_IN",
                if r.below(4) == 0 { country2 } else { country },
            );
            people.push(p);
        }
        for i in 0..nadmin {
            let a = node(&g, "Admin", 500 + i);
            rel(&g, a, "LIVES_IN", country);
            people.push(a);
        }
        let tags: Vec<u64> = (0..2).map(|i| node(&g, "Tag", 700 + i)).collect();
        for &p in &people {
            for &t in &tags {
                if r.below(3) == 0 {
                    rel(&g, p, "LIKES", t);
                }
            }
        }
        // Random KNOWS / LIKES edges, including self-loops and parallels.
        for _ in 0..(4 + r.below(10)) {
            let a = people[r.below(people.len() as u64) as usize];
            let b = people[r.below(people.len() as u64) as usize];
            let ty = if r.below(5) == 0 { "LIKES" } else { "KNOWS" };
            rel(&g, a, ty, b);
        }
        // A deliberate self-loop, sometimes: the gate's whole purpose.
        if r.below(3) == 0 {
            let a = people[r.below(people.len() as u64) as usize];
            rel(&g, a, "KNOWS", a);
        }
        // Sometimes create then DELETE one, so the stats DECREMENT is
        // exercised: a self-loop count that only ever grows would decline
        // symmetry forever after the first loop and hide a wrong answer
        // behind a conservative one.
        if r.below(3) == 0 {
            let a = people[r.below(people.len() as u64) as usize];
            let id = g.create_rel(a, "KNOWS", a, &BTreeMap::new()).expect("loop");
            g.delete_rel(id).expect("delete loop");
        }
        for (pi, src) in PATTERNS.iter().enumerate() {
            let (on, off, general) = arms(&g, src);
            if on != 0 {
                nonzero += 1;
            }
            if !(on == off && off == general) {
                bad.push(format!(
                    "seed {seed} pattern {pi}: ON={on} OFF={off} GENERAL={general}\n  {src}"
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} disagreement(s) between the three executions:\n{}",
        bad.len(),
        bad.join("\n")
    );
    // A sweep of empty graphs would agree three ways and prove nothing —
    // 0 == 0 == 0 is the vacuous pass this whole suite exists to avoid.
    assert!(
        nonzero > 300,
        "only {nonzero} of the {} (seed, pattern) cases counted anything at all — the generator \
         stopped producing matches, so the agreement above is mostly 0 == 0 == 0",
        180 * PATTERNS.len()
    );
}
