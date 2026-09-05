#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The adjacency-snapshot memo must key on the TABLE it holds, not merely on
//! "some table was resolved recently".
//!
//! # What this guards
//!
//! `with_adj_table` resolves a table that is constant for a whole hop, and it
//! did so once per PROBE — allocating its `(tag, types)` map key and walking a
//! `BTreeMap` per row. `adj_snap_memo` re-serves the snapshot the thread just
//! resolved instead. That is sound only while the memo distinguishes every
//! dimension the map key distinguished:
//!
//! * the relationship TYPE set — else `-[:LIKES]-` is answered from `-[:KNOWS]-`;
//! * the DIRECTION tag — else an outgoing probe is answered from the incoming
//!   table, which for a one-way edge is the opposite answer;
//! * the GRAPH — else a second graph on the same thread inherits the first's
//!   adjacency, and the test binaries build graphs in a loop;
//! * FRESHNESS — else a write is invisible to the thread that just wrote it.
//!
//! Each test alternates the two arms of one dimension *on a single thread*,
//! which is the only way the memo is consulted at all. The interleaving is the
//! point: a memo blind to the dimension would have been filled by the previous
//! statement and would answer this one from it.
//!
//! # The fixture has to REACH the memo
//!
//! An adjacency table is only built once `degree_table_after` probes have asked
//! for it — 1,024 by default. A small fixture never gets there, so `with_adj_table`
//! finds no table, the memo is never filled, and the fast path never runs.
//!
//! The first cut of this file did exactly that, and every assertion below
//! passed against a memo deliberately broken to compare only the graph id. That
//! is why `admit_tables` drops the threshold to zero and why each test asserts
//! on `MEMO_SERVED`: a canary that cannot observe the mechanism it guards is
//! not evidence about it.
//!
//! # Proven to bite
//!
//! Checked against a memo whose `adj_snap_memo_get` compares only `graph`:
//! `types_do_not_share_a_snapshot`, `directions_do_not_share_a_snapshot` and
//! `a_second_graph_does_not_inherit` fail on their counts, and
//! `a_write_is_visible_to_the_thread_that_wrote_it` fails on freshness.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The counter the fast path bumps. If this stays zero the test exercised the
/// map walk and says nothing about the memo.
const MEMO_SERVED: &str = "graph.adjacency snapshot re-served to the same thread";

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn count(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one Int from `{src}`, got {other:?}"),
    }
}

fn served(t: &engram_observe::Trace) -> u64 {
    t.counters().get(MEMO_SERVED).copied().unwrap_or(0)
}

/// Every counter the run bumped, biggest first. A "never served" failure is
/// almost always the statement taking a path that never resolves a table at
/// all, and the only way to tell that from a broken memo is to look.
fn dump(t: &engram_observe::Trace) -> String {
    let mut v: Vec<(&String, &u64)> = t.counters().iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    v.iter()
        .map(|(n, c)| format!("\n    {c:>12}  {n}"))
        .collect()
}

/// Build adjacency tables from the first probe, so a fixture this small reaches
/// the code under test at all.
fn admit_tables(g: &Graph) {
    g.set_degree_table_after(0);
}

const N: i64 = 24;

/// A one-way `KNOWS` ring at stride 1 (out-degree 1), and a `LIKES` ring at
/// strides 1 AND 2 (out-degree 2), over the same nodes.
///
/// The two out-degrees differ so that every query below has a different answer
/// per type — a memo that confused them would still return a plausible count,
/// and the whole point is that it cannot return the RIGHT one.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    admit_tables(&g);
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    let edge = |ty: &str, a: i64, b: i64| {
        stmt(
            &g,
            &format!("MATCH (x:P {{id: {a}}}), (y:P {{id: {b}}}) CREATE (x)-[:{ty}]->(y)"),
        );
    };
    for p in 0..N {
        edge("KNOWS", p, (p + 1) % N);
        edge("LIKES", p, (p + 1) % N);
        edge("LIKES", p, (p + 2) % N);
    }
    // FOLLOWS is deliberately NOT a ring, because a ring cannot detect a memo
    // that confuses the two directions: in-degree and out-degree are both 1, so
    // serving the `I` table for an `O` question returns a different set of
    // peers with the SAME count. Here node 0 reaches 2 nodes forwards in two
    // hops and 3 backwards, so the two directions are distinguishable by count.
    //   forwards : 0 -> 1 -> {5, 6}      (2 is a leaf, 3 is a leaf)
    //   backwards: 0 <- 7 <- {9, 10, 11} (8 is a leaf)
    for (a, b) in [
        (0, 1),
        (0, 2),
        (0, 3),
        (1, 5),
        (1, 6),
        (7, 0),
        (8, 0),
        (9, 7),
        (10, 7),
        (11, 7),
    ] {
        edge("FOLLOWS", a, b);
    }
    // The DELAYED deep branch, for `deep_tables_file_under_an_outer_hit`. One
    // `:Q` node, reachable by LIKES from exactly one P — so in a chain ending
    // `-[:LIKES]->(:Q)-[:FOLLOWS]->` the FOLLOWS table's first probe happens
    // long after the LIKES table started HITTING. Every other P's LIKES
    // targets are `:P` and fail a `:Q` filter, which is precisely the shape
    // that starved the pod's q2: the deep table's first probe always arrives
    // nested under a live serve.
    stmt(&g, "CREATE (:Q {id: 100})");
    stmt(&g, "MATCH (x:P {id: 5}), (y:Q {id: 100}) CREATE (x)-[:LIKES]->(y)");
    stmt(&g, "MATCH (x:Q {id: 100}), (y:P {id: 20}) CREATE (x)-[:FOLLOWS]->(y)");
    g.shared_store().seal();
    g
}

/// Enough repeats that the memo is warm before all but the first probe, which
/// is the state the fast path exists to serve.
const ROUNDS: usize = 40;

/// Two hops, so the fold PROBES PER ROW.
///
/// A single hop is answered by a count fast path that resolves its table once
/// per statement through `adj_table_snapshot_reporting` — a different function
/// that never consults the memo. The first cut of this test used single hops
/// and so measured a path the memo does not sit on at all, which is why every
/// assertion in it passed against a memo broken to compare only the graph id.
#[test]
fn types_do_not_share_a_snapshot() {
    let g = fixture();
    let kk = "MATCH (a:P)-[:KNOWS]->(b:P)-[:KNOWS]->(c:P) RETURN count(*) AS c";
    let ll = "MATCH (a:P)-[:LIKES]->(b:P)-[:LIKES]->(c:P) RETURN count(*) AS c";
    // The INTERLEAVED shape: one statement alternating two type sets, which is
    // how LSQB q2 drives its chain and the reason the memo is associative
    // rather than a single slot.
    let kl = "MATCH (a:P)-[:KNOWS]->(b:P)-[:LIKES]->(c:P) RETURN count(*) AS c";

    let (k, l, m) = (count(&g, kk), count(&g, ll), count(&g, kl));
    assert_eq!(k, N, "out-degree 1 twice: one 2-hop path per node");
    assert_eq!(l, N * 4, "out-degree 2 twice");
    assert_eq!(m, N * 2, "out-degree 1 then 2");
    assert!(
        k != l && l != m && k != m,
        "the three answers must differ or this test cannot detect a memo that \
         confuses the type sets: {k}, {l}, {m}"
    );

    let (_, trace) = engram_observe::with_trace(|| {
        for i in 0..ROUNDS {
            assert_eq!(count(&g, kk), k, "KNOWS-KNOWS, round {i}");
            assert_eq!(count(&g, ll), l, "LIKES-LIKES after KNOWS, round {i}");
            assert_eq!(count(&g, kl), m, "KNOWS-LIKES interleaved, round {i}");
        }
    });
    assert!(
        served(&trace) > 0,
        "the memo never served — this run exercised the map walk and is no \
         evidence about the memo at all. counters:{}",
        dump(&trace)
    );
}

#[test]
fn directions_do_not_share_a_snapshot() {
    let g = fixture();
    // Two hops on the ASYMMETRIC type, so the two directions differ by count
    // and not merely by which peers they name.
    let fwd = "MATCH (a:P)-[:FOLLOWS]->(b:P)-[:FOLLOWS]->(c:P) WHERE a.id = 0 RETURN count(*) AS c";
    let back = "MATCH (a:P)<-[:FOLLOWS]-(b:P)<-[:FOLLOWS]-(c:P) WHERE a.id = 0 RETURN count(*) AS c";

    let (f, b) = (count(&g, fwd), count(&g, back));
    assert_eq!(f, 2, "0 -> 1 -> {{5, 6}}");
    assert_eq!(b, 3, "0 <- 7 <- {{9, 10, 11}}");
    assert_ne!(
        f, b,
        "the two directions must differ by COUNT or this test cannot detect a \
         memo that serves one table for the other"
    );

    let (_, trace) = engram_observe::with_trace(|| {
        for i in 0..ROUNDS {
            assert_eq!(count(&g, fwd), f, "forwards after backwards, round {i}");
            assert_eq!(count(&g, back), b, "backwards after forwards, round {i}");
        }
    });
    assert!(
        served(&trace) > 0,
        "the memo never served. counters:{}",
        dump(&trace)
    );
}

#[test]
fn a_second_graph_does_not_inherit() {
    // Two graphs alive at once, queried alternately on ONE thread. The second
    // has strictly more edges, so serving the first's table under-counts.
    // The DENSE graph is built first and therefore carries the higher store
    // clock. That ordering is the whole test: query it first, and a memo blind
    // to the graph offers its table to the sparse graph with a stamp that
    // clears the sparse graph's epoch, so the freshness check does not save it.
    //
    // Built the other way round — sparse first — the dense graph's newer epoch
    // declines the stale entry and a graph-blind memo passes anyway. That is
    // fixture luck, not a property, and the first cut of this test had it.
    let dense = fixture();
    for p in 0..N {
        let q = (p + 2) % N;
        stmt(
            &dense,
            &format!("MATCH (x:P {{id: {p}}}), (y:P {{id: {q}}}) CREATE (x)-[:KNOWS]->(y)"),
        );
    }
    dense.shared_store().seal();
    let sparse = fixture();

    // Two hops, for the reason given on `types_do_not_share_a_snapshot`.
    let src = "MATCH (x:P)-[:KNOWS]->(y:P)-[:KNOWS]->(z:P) RETURN count(*) AS c";
    let (cd, cs) = (count(&dense, src), count(&sparse, src));
    assert_eq!(cd, N * 4, "dense: out-degree 2 twice");
    assert_eq!(cs, N, "sparse: out-degree 1 twice");
    let (_, trace) = engram_observe::with_trace(|| {
        for i in 0..ROUNDS {
            assert_eq!(count(&dense, src), cd, "dense, round {i}");
            assert_eq!(count(&sparse, src), cs, "sparse after dense, round {i}");
        }
    });
    assert!(
        served(&trace) > 0,
        "the memo never served. counters:{}",
        dump(&trace)
    );
}

#[test]
fn a_write_is_visible_to_the_thread_that_wrote_it() {
    // The freshness dimension. Read (warming the memo), write, read again: the
    // remembered snapshot is now too old for the new epoch and must not be
    // served in place of one that sees the write.
    let g = fixture();
    // Two hops, so the read goes through the memo at all. A single hop is
    // answered by the count fast path, and this test passed against a memo with
    // its freshness check deleted until it was written this way.
    let src = "MATCH (a:P)-[:KNOWS]->(b:P)-[:KNOWS]->(c:P) RETURN count(*) AS c";
    let mut prev = count(&g, src);
    assert_eq!(prev, N, "the ring has one 2-hop path per node");

    for i in 0..12 {
        let p = i;
        let q = (i + 7) % N;
        stmt(
            &g,
            &format!("MATCH (a:P {{id: {p}}}), (b:P {{id: {q}}}) CREATE (a)-[:KNOWS]->(b)"),
        );
        // Every new edge closes at least one new 2-hop path at each end, so the
        // count MUST rise. A memo serving its pre-write snapshot returns `prev`.
        let now = count(&g, src);
        assert!(
            now > prev,
            "the edge just written must be visible to the very next read on \
             this thread — read {i} returned {now}, unchanged from {prev}"
        );
        prev = now;
    }
}

/// A table first probed NESTED — inside an outer hit's serve — must still get
/// filed, and then served.
///
/// # Proven to bite
///
/// The first per-way-less build of the borrow-held memo could not file under
/// an outer hit at all: a table whose first probe only ever happens nested
/// NEVER filed and missed forever. The pod named the victims on LSQB q2 —
/// hop 2's table and both close tables, `declined to file` × 3,029,508 on one
/// execution, every one a full-cost map walk. This test fails against that
/// build with `holds no entry` still firing on the second run.
#[test]
fn deep_tables_file_under_an_outer_hit() {
    let g = fixture();
    // Three DISTINCT types, and the deep one DELAYED: `:Q` is reachable only
    // through P node 5, and every other P's LIKES targets fail the `:Q`
    // filter, so the FOLLOWS table's first probe happens nested under a LIKES
    // serve that is already hitting. A fixture whose first path reaches full
    // depth files every table on the all-miss cascade and cannot detect the
    // starvation — the first cut of this test passed against the starved
    // build for exactly that reason.
    let chain =
        "MATCH (a:P)-[:KNOWS]->(b:P)-[:LIKES]->(c:Q)-[:FOLLOWS]->(d:P) RETURN count(*) AS c";
    let warm = count(&g, chain);
    assert!(warm > 0, "the fixture must give this chain rows to walk");

    let (_, trace) = engram_observe::with_trace(|| {
        assert_eq!(count(&g, chain), warm, "the warm repeat must agree");
    });
    let miss = |name: &str| trace.counters().get(name).copied().unwrap_or(0);
    assert!(
        served(&trace) > 0,
        "the repeat never served from the memo. counters:{}",
        dump(&trace)
    );
    assert_eq!(
        miss("graph.adjacency memo holds no entry for this table"),
        0,
        "every table this chain touches was resolved on the first run, so a \
         second-run miss means a nested table could not FILE. counters:{}",
        dump(&trace)
    );
    assert_eq!(
        miss("graph.adjacency memo found no writable way"),
        0,
        "eight ways against a three-deep recursion must always leave one \
         writable. counters:{}",
        dump(&trace)
    );
}

/// The lever's two arms must agree on every answer; it may only change what the
/// lookup costs.
#[test]
fn the_lever_changes_cost_and_not_answers() {
    let g = fixture();
    let queries = [
        "MATCH (a:P)-[:KNOWS]->(b:P) RETURN count(*) AS c",
        "MATCH (a:P)-[:LIKES]->(b:P) RETURN count(*) AS c",
        "MATCH (a:P)-[:KNOWS]-(b:P) RETURN count(*) AS c",
        "MATCH (a:P)<-[:KNOWS]-(b:P) WHERE a.id = 3 RETURN count(*) AS c",
    ];

    g.set_adj_snap_memo(true);
    let (on, on_trace) = engram_observe::with_trace(|| {
        queries.iter().map(|q| count(&g, q)).collect::<Vec<i64>>()
    });
    g.set_adj_snap_memo(false);
    let (off, off_trace) = engram_observe::with_trace(|| {
        queries.iter().map(|q| count(&g, q)).collect::<Vec<i64>>()
    });
    g.set_adj_snap_memo(true);

    assert_eq!(
        on, off,
        "the memo must be invisible in the answers — if these differ it is not \
         a cost lever, it is a behaviour change"
    );
    assert!(
        served(&on_trace) > 0,
        "the ON arm never served from the memo, so this compares two runs of \
         the same path"
    );
    assert_eq!(
        served(&off_trace),
        0,
        "the OFF arm must never serve from the memo — that is what makes it the \
         control"
    );
}
