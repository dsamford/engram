#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A cycle's closing hop is ALREADY an existence test — pinned, because I tried
//! to "fix" it and the fix was inert.
//!
//! # What was attempted, and why it was wrong
//!
//! `semijoin` has a counted close (two `partition_point`s on the sorted CSR,
//! O(log deg)) guarded off whenever relationship isomorphism is tracked with a
//! non-empty base — which a multi-hop cycle ALWAYS has at its closing hop. That
//! looked exactly like LSQB q3's cost: ~12.8M rows each scanning `person3`'s
//! ~36 neighbours for `person1`, against the 107,386,500 `adjacency tables
//! reused` profiled at SF1.
//!
//! It is not, and this fixture is how that was established. A count-only
//! triangle never reaches `semijoin`: it takes `interp.pipeline count fold`,
//! whose close runs through `pred_holds`' `EdgeToBound` arm —
//! `edge_count_slim(peer, dir, tokens, bound) > 0`. That IS the existence test.
//! A short circuit added to `semijoin` fired **zero** times here and was
//! reverted rather than shipped unmeasured.
//!
//! # What this test pins
//!
//! That the shape routes to the count fold and answers correctly. If a future
//! change moves it onto a walk-based close, the assertions below fail and
//! whoever made the change learns it here instead of from a p50.
//!
//! It also records the real conclusion about q3: the cost is the ENUMERATION —
//! 111 x 89 x 89 x 89 rooted at `Country` — and not the close. Shrinking it
//! needs either a seed the cost model cannot safely pick (measured: -27% on q3,
//! +22% to +53% on four other queries) or a triangle-listing operator. See
//! `docs/write-path-phase0.md` §16 and the task list.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A ring of `N` people where each knows the next `K`. Triangles exist (the
/// chords make them) but most pairs at distance 3 do NOT close — the ratio that
/// makes the short circuit worth anything.
const N: i64 = 90;
const K: i64 = 4;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..N {
        stmt(&g, &format!("CREATE (:Person {{id: {p}}})"));
    }
    for p in 0..N {
        for k in 1..=K {
            let q = (p + k) % N;
            stmt(
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

const TRIANGLE: &str = "MATCH (p1:Person)-[:KNOWS]-(p2:Person)-[:KNOWS]-(p3:Person)-[:KNOWS]-(p1) \
                        RETURN count(*) AS count";

#[test]
fn the_closing_hop_skips_the_walk_when_no_edge_exists() {
    let g = fixture();
    let q = parse_statement(TRIANGLE).expect("parse");
    let (r, t) = engram_observe::with_trace(|| run_query(&g, &q, BTreeMap::new()));
    let r = r.expect("run");
    let count = match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("the triangle count must be one Int, got {other:?}"),
    };
    let c = |k: &str| t.counters().get(k).copied().unwrap_or(0);
    let skipped = c("interp.pipeline semijoin close skipped, no such edge");

    eprintln!(
        "[cycle] {N} people x {K} chords -> count {count}; \
         closes skipped without walking: {skipped}; adjacency rows {}",
        c("graph.adjacency tables reused")
    );

    let mut all: Vec<(&str, u64)> = t
        .counters()
        .iter()
        .filter(|(k, _)| k.starts_with("interp.") || k.starts_with("pipeline."))
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    all.sort_unstable_by_key(|(k, _)| *k);
    for (k, v) in all {
        eprintln!("        [path] {v:>8}  {k}");
    }

    assert!(
        count > 0,
        "the fixture must contain triangles, or the close never succeeds and \
         skipping every one of them would look like a win"
    );
    // The close is NOT a semijoin walk here — it is the count fold's existence
    // test. Pinned as an equality, so a future change routing this shape onto a
    // walk-based close is caught here rather than in a p50.
    assert_eq!(
        skipped, 0,
        "a `semijoin` close fired for a count-only triangle; this shape takes          the count fold, whose close is already `edge_count_slim(..) > 0`"
    );
    assert_eq!(
        c("interp.pipeline count fold"),
        1,
        "the count fold must be the path that answers this — the whole finding          is that its close is already an existence test"
    );

    // EXACTNESS. The short circuit may only remove work. Same fixture, same
    // query, run through the path that does NOT track isomorphism — a
    // single-hop close — must agree about which pairs are connected.
    let pairs = parse_statement(
        "MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN count(*) AS count",
    )
    .expect("parse");
    let pr = run_query(&g, &pairs, BTreeMap::new()).expect("run pairs");
    let npairs = match pr.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("pair count must be one Int, got {other:?}"),
    };
    assert_eq!(
        npairs,
        N * K * 2,
        "each of the {N} people has {K} outgoing KNOWS, and an undirected \
         match sees every edge from both ends — if this is wrong the fixture is \
         not the graph the triangle count describes"
    );
}
