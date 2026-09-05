#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A hop's label filter is a MEMBERSHIP TEST, and a membership test must not
//! materialise the label.
//!
//! # The defect this pins
//!
//! `MATCH (p:Person {id: K})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message)`
//! plans the `m:Message` filter by fetching the label's membership and asking,
//! per candidate peer, "is this id in it?". The membership arrives as a
//! `MembersView` — a base vector plus an `added`/`removed` overlay accumulated
//! by O(delta) catch-up. The pipeline used to call `to_arc_vec()` on it and
//! `binary_search` the result.
//!
//! `to_arc_vec()` with an overlay present MERGES: it walks the whole base,
//! applies the overlay and allocates a fresh vector — O(|label|). It is
//! memoised, but per PUBLISHED SNAPSHOT, and under a concurrent write stream a
//! new snapshot is published tens of times a second. So a query that needs
//! `contains()` — O(log n) — paid a multi-million-id merge instead, once per
//! read, on the largest label in the corpus.
//!
//! On the stress harness this was measurable as a whole profile: `balanced`
//! (50/50, writes create `:Message:Comment`) ran ~30% below both
//! `balanced-nolabels` (identical writes, no labels) and `balanced-disjoint`
//! (writes on a relationship type no read traverses), and the materialisation
//! counter fired ONE-FOR-ONE with membership catch-ups.
//!
//! # What this test asserts
//!
//! With an overlay present and the ic6 shape running, `derived.members view
//! materialised` must be ZERO — the filter is answered by `contains()`.
//!
//! # Why it can be trusted
//!
//! A zero is the absence of a signal, so the counter is proven live in the same
//! test: after the query, `members_all(["Message"]).to_arc_vec()` is called
//! directly and the counter must go to exactly 1. Without that leg the
//! assertion would also pass if the counter were never wired, if the fixture
//! never built an overlay, or if the query never consulted the membership at
//! all — and each of those is asserted separately below.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g
            .commit_owned(txn)
            .unwrap_or_else(|e| panic!("commit {src}: {e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            panic!("run {src}: {e:?}");
        }
    }
}

/// The hub, eight friends, twelve messages each, three tags per message from a
/// vocabulary of ten — the ic6 fan-out shape at a size a unit test can hold.
fn build(g: &Graph) {
    g.set_degree_table_after(0);
    stmt(g, "CREATE (:Person {id: 0, name: 'hub'})");
    for f in 1..=8i64 {
        stmt(g, &format!("CREATE (:Person {{id: {f}, name: 'f{f}'}})"));
        stmt(
            g,
            &format!("MATCH (a:Person {{id: 0}}), (b:Person {{id: {f}}}) CREATE (a)-[:KNOWS]->(b)"),
        );
    }
    for t in 0..10i64 {
        stmt(g, &format!("CREATE (:Tag {{id: {t}, name: 'tag{t}'}})"));
    }
    for f in 1..=8i64 {
        for j in 0..12i64 {
            let mid = f * 100 + j;
            stmt(g, &format!("CREATE (:Message {{id: {mid}}})"));
            stmt(
                g,
                &format!(
                    "MATCH (m:Message {{id: {mid}}}), (p:Person {{id: {f}}}) \
                     CREATE (m)-[:HAS_CREATOR]->(p)"
                ),
            );
            for k in 0..3i64 {
                let tag = (mid + k) % 10;
                stmt(
                    g,
                    &format!(
                        "MATCH (m:Message {{id: {mid}}}), (t:Tag {{id: {tag}}}) \
                         CREATE (m)-[:HAS_TAG]->(t)"
                    ),
                );
            }
        }
    }
}

const IC6: &str = "MATCH (p:Person {id: 0})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
                   MATCH (m)-[:HAS_TAG]->(t:Tag) \
                   RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10";

#[test]
fn a_hop_label_filter_answers_from_the_overlay_without_materialising_it() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    build(&g);

    // Publish a BASE membership for :Message, then write past it so the next
    // reader catches up into an OVERLAY. Without an overlay `to_arc_vec()` is
    // an `Arc` clone and the whole question is moot — so the overlay is
    // asserted, not assumed.
    let baseline = run_query(&g, &parse_statement(IC6).expect("parse"), BTreeMap::new());
    baseline.expect("warm the membership");
    stmt(&g, "CREATE (:Message:Comment {id: 999999})");

    let (res, trace) = engram_observe::with_trace(|| {
        let q = parse_statement(IC6).expect("parse");
        run_query(&g, &q, BTreeMap::new())
    });
    let res = res.expect("run");
    let c = |k: &str| trace.counters().get(k).copied().unwrap_or(0);

    eprintln!(
        "[hop-filter] {} row(s); caught_up={} folded={} materialised={}",
        res.rows.len(),
        c("derived.members view caught up"),
        c("derived.members view folded"),
        c("derived.members view materialised"),
    );

    assert!(
        !res.rows.is_empty(),
        "the fixture must answer rows, or the counters describe nothing"
    );
    assert!(
        c("graph.membership snapshots caught up") > 0,
        "the read must have caught the :Message membership up into an OVERLAY — \
         with no overlay `to_arc_vec` is a pointer copy and a zero below proves nothing"
    );
    assert_eq!(
        c("derived.members view materialised"),
        0,
        "the `m:Message` hop filter is a membership TEST; it must be answered by \
         `MembersView::contains` in O(log n), not by merging the whole label"
    );

    // THE CANARY, and the lever's ON/OFF differential in one place. A zero is
    // the absence of a signal; flipping `hop_membership_contains` OFF restores
    // the pre-fix path on the SAME binary and the SAME fixture, so the zero
    // above is the absence of WORK rather than the absence of instrumentation,
    // of an overlay, or of a filter. The rows must not move either — this was a
    // performance change and the two paths answer the same question.
    g.set_hop_membership_contains(false);
    let (arm, trace2) = engram_observe::with_trace(|| {
        let q = parse_statement(IC6).expect("parse");
        run_query(&g, &q, BTreeMap::new())
    });
    let arm = arm.expect("run the A/B arm");
    let c2 = |k: &str| trace2.counters().get(k).copied().unwrap_or(0);
    g.set_hop_membership_contains(true);

    eprintln!(
        "[hop-filter] arm OFF: materialised={} via {} filtered probe(s)",
        c2("derived.members view materialised"),
        c2("graph.hop label filter materialised the label"),
    );
    assert!(
        c2("graph.hop label filter materialised the label") > 0,
        "the lever must reach the hop filter — an A/B arm nothing consults is inert"
    );
    assert_eq!(
        c2("derived.members view materialised"),
        1,
        "and materialising is what it costs: the whole :Message label merged once          for this snapshot, which under a write stream is once per query"
    );
    assert_eq!(
        arm.rows, res.rows,
        "the two paths must answer identically: `contains` tests `added` and          `base` minus `removed`, which is exactly the set the merge produces"
    );
}
