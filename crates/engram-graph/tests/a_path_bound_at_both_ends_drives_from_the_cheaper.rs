//! A path bound at BOTH ends is priced along its WHOLE length, not by its
//! first hop — but only where the cheap rule gives up.
//!
//! `reverse_both_bound_path` already chose a driving end for a path pinned at
//! both ends, by comparing one probe from each end. On a single-hop closure
//! that is exact. On a longer leg the explosion is usually further in, and
//! then the first hop says the OPPOSITE of the truth.
//!
//! Measured on SNB BI16 at SF3: the optional leg
//! `(person1)-[:KNOWS]-(person2)<-[:HAS_CREATOR]-(message2)-[:HAS_TAG]->(tag)`
//! leaves `person1` by 139 KNOWS edges and the tag by 13,512 HAS_TAG edges, so
//! the first-hop rule kept `person1` — and then walked 139 friends × ~370
//! messages each, 14,293,280 expansions per parameter, to produce 90 people.
//! From the tag the same leg reads 13,512 edges. 145 s -> 72 s for the whole
//! query, same 3 rows.
//!
//! TWO traps, both measured, both of which this file pins down:
//!
//! 1. A hop that lands on the opposite bound end costs 1, not its fan-out —
//!    it is an edge-existence test. Price it as a fan-out (Message -> Tag
//!    averages 3.6 at SF3) and the estimate prefers the runaway direction.
//!    But a SINGLE hop is its own cost, so that rule applies only past the
//!    first hop, or bi11's triangle closure loses its decision entirely.
//!
//! 2. The estimate is not free, and this site runs PER PARTIAL. bi8 prices
//!    `size([(tag)<-[:HAS_TAG]-(m)-[:HAS_CREATOR]->(person) | m])` once per
//!    person and once per friend: estimating there cost 146 s -> 199 s to
//!    choose the SAME end the first-hop rule already chose — the trace
//!    counters are identical across both arms. So this ships OFF by default
//!    and these tests turn it on. Two ways out were measured and both are
//!    worse: caching the decision per shape (bi8 past 400 s and killed,
//!    bi16's 76 s back to 154 s — one row's magnitudes do not speak for the
//!    next), and pricing the first hop from the counts so nothing depends on
//!    the row (loses a reversal the measured probe gets right). Only the
//!    shape-only TAIL is cached.
//!
//! And one that is a HANG rather than a slow plan: the two rules must never
//! mix. A reversal re-enters on the reversed path, so running the first-hop
//! rule ahead of the estimate loops — the estimate reverses the leg and the
//! probe, seeing the ends swapped, reverses it back. Each rule alone fires
//! only on a strict improvement and so cannot fire on its own output; the
//! SHAPE picks the rule (one hop: probes; more: the estimate).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PRICED: &str = "interp.pattern priced the whole path, not its first hop";
const KEPT: &str = "interp.pattern kept its bound end: the other is no cheaper";
// THE EXPANSION, not the materialisation: this leg binds its hop ends bare,
// so no record is decoded per end and `graph.projected node materialisations`
// reads the same in both arms while one walks orders of magnitude more edges.
const EDGES: &str = "graph.edge entries by binary search";
const REUSED: &str = "interp.pattern reused its shape's join order";
const ESTIMATED: &str = "interp.pattern estimated a new shape's join order";

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// BI16's shape in miniature. The asymmetry is the whole point: `p1` leaves by
/// FEW edges (`friends`) and the tag by MANY (`tagged`), so the first-hop rule
/// keeps `p1` — and each friend then has `msgs` messages, which is where the
/// cost actually is.
///
/// Warmed, because the fan-out probe reads a RESIDENT adjacency table and
/// answers `None` when there is none; a served store has these (boot warming
/// builds one per type per direction).
fn fixture(friends: i64, msgs: i64, tagged: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |labels: &[&str], k: i64| {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(k));
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        g.create_node(&labels, &m).expect("node")
    };
    let tag = node(&["Tag"], 0);
    let p1 = node(&["Person"], 0);
    let mut people = Vec::new();
    for i in 0..friends {
        let f = node(&["Person"], 1_000 + i);
        g.create_rel(p1, "KNOWS", f, &BTreeMap::new())
            .expect("knows");
        for j in 0..msgs {
            let m = node(&["Message"], 10_000 + i * msgs + j);
            g.create_rel(m, "HAS_CREATOR", f, &BTreeMap::new())
                .expect("creator");
        }
        people.push(f);
    }
    // the tag's entire inbound adjacency, spread over the friends so the
    // answer is non-trivial
    for i in 0..tagged {
        let f = people[(i % people.len() as i64) as usize];
        let m = node(&["Message"], 90_000 + i);
        g.create_rel(m, "HAS_CREATOR", f, &BTreeMap::new())
            .expect("creator");
        g.create_rel(m, "HAS_TAG", tag, &BTreeMap::new())
            .expect("tag");
    }
    // The estimate is OFF by default — `Graph::set_path_estimate` records the
    // measured trade — so the file that tests it turns it on.
    g.set_path_estimate(true);
    let _ = g.warm();
    g
}

/// The BI16 leg: both ends bound, written from the end whose FIRST hop is
/// smaller and whose whole path is far larger.
const LEG: &str = "MATCH (p1:Person {k: 0}), (tag:Tag {k: 0}) \
     WITH p1, tag \
     MATCH (p1)-[:KNOWS]-(p2:Person)<-[:HAS_CREATOR]-(m2:Message)-[:HAS_TAG]->(tag) \
     RETURN count(DISTINCT p2) AS cp2";

/// The same question from the other end — the hand rewrite that measured 11 s
/// against 93 s for BI16's leg on the bench corpus.
const LEG_REWRITTEN: &str = "MATCH (p1:Person {k: 0}), (tag:Tag {k: 0}) \
     WITH p1, tag \
     MATCH (tag)<-[:HAS_TAG]-(m2:Message)-[:HAS_CREATOR]->(p2:Person)-[:KNOWS]-(p1) \
     RETURN count(DISTINCT p2) AS cp2";

#[test]
fn the_written_end_is_abandoned_when_the_whole_path_is_dearer() {
    // 5 friends (p1's first hop) against 50 tagged messages (the tag's): the
    // first-hop rule keeps p1, and p1's second hop is 5 x 400 messages.
    let g = fixture(5, 400, 50);
    let (rows, t) = engram_observe::with_trace(|| run(&g, LEG));
    assert_eq!(rows, vec![vec![Value::Int(5)]], "the tagged friends");
    assert!(
        counter(&t, PRICED) >= 1,
        "the first hop decided, so the leg kept its expensive end: {:?}",
        t.counters()
    );
}

#[test]
fn the_answer_is_the_same_from_either_end() {
    // A join order may change what is READ, never what is RETURNED — and the
    // rewrite is the independent witness: it asks the same question from the
    // end the estimate now picks on its own.
    let g = fixture(5, 400, 50);
    assert_eq!(run(&g, LEG), run(&g, LEG_REWRITTEN));
    assert_eq!(run(&g, LEG), vec![vec![Value::Int(5)]]);
}

#[test]
fn pricing_the_whole_path_reads_far_less() {
    // The claim is a COST claim, so measure the cost: without the reversal the
    // leg walks every friend's every message; with it, the tag's own 50.
    let g = fixture(5, 400, 50);
    g.set_hop_reversal(false);
    let (off_rows, off) = engram_observe::with_trace(|| run(&g, LEG));
    g.set_hop_reversal(true);
    let (on_rows, on) = engram_observe::with_trace(|| run(&g, LEG));

    assert_eq!(off_rows, on_rows, "the reversal changed an ANSWER");
    assert!(
        counter(&on, EDGES) * 4 < counter(&off, EDGES),
        "driving from the cheap end walked {} edges against {} — no material \
         saving: {:?}",
        counter(&on, EDGES),
        counter(&off, EDGES),
        on.counters()
    );
}

#[test]
fn a_path_already_driving_from_its_cheap_end_is_left_alone() {
    // THE CONTROL. The estimate must not simply reverse everything. Here the
    // written start (the tag, 50 edges) is the cheap end and the far end is
    // dearer (200 friends), so the cheap rule declines, the estimate runs —
    // and keeps what the query wrote.
    let g = fixture(200, 1, 50);
    let (rows, t) = engram_observe::with_trace(|| run(&g, LEG_REWRITTEN));
    assert_eq!(rows, vec![vec![Value::Int(50)]]);
    assert_eq!(
        counter(&t, PRICED),
        0,
        "it reversed a path that was already cheapest: {:?}",
        t.counters()
    );
    assert!(
        counter(&t, KEPT) >= 1,
        "the estimate never ran, so this control proves nothing: {:?}",
        t.counters()
    );
}

#[test]
fn a_cheap_leg_is_not_reversed_on_a_near_tie() {
    // The MARGIN. Past the first hop the estimate is averages, so a near-tie
    // is not evidence: here 200 friends of one message each price about the
    // same either way, and flipping on that would trade this plan's error for
    // another's.
    let g = fixture(200, 1, 50);
    let (_, t) = engram_observe::with_trace(|| run(&g, LEG));
    assert_eq!(
        counter(&t, PRICED),
        0,
        "the estimate ran although the first hop already decided: {:?}",
        t.counters()
    );
}

#[test]
fn the_join_order_is_decided_once_and_then_reused() {
    // What makes the estimate affordable at a site that runs per partial.
    // The shape here is reached once per row, as bi8 reaches its scoring
    // comprehension once per person and once per friend — and pricing it at
    // every one of those cost bi8 53 s to reach the end it had already chosen.
    let g = fixture(5, 400, 50);
    let per_row = "MATCH (tag:Tag {k: 0}) MATCH (p1:Person) WITH p1, tag          RETURN sum(size([(p1)-[:KNOWS]-(p2:Person)<-[:HAS_CREATOR]-(m2:Message)-[:HAS_TAG]->(tag) | m2])) AS n";
    let (rows, t) = engram_observe::with_trace(|| run(&g, per_row));
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        counter(&t, REUSED) >= 2,
        "every row priced the shape again, which is what cost bi8 53 s: {:?}",
        t.counters()
    );
    // PRICED counts the DECISION, which the caller takes on every partial;
    // what must not repeat is the estimate behind it.
    assert_eq!(
        counter(&t, ESTIMATED),
        1,
        "the shape was estimated once per row rather than once: {:?}",
        t.counters()
    );
}
