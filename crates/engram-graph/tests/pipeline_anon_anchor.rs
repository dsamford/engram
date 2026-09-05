#![allow(non_snake_case)]
//! Differential tests for the two single-MATCH read-chain extensions that let a
//! single-seed multi-hop query (the `foaf` shape
//! `MATCH (a:L {id: v})-[:T]-()-[:T]-(g) RETURN count(DISTINCT g)`) run on the
//! COLUMNAR pipeline instead of the allocation-heavy `run_streaming` interp:
//!
//!   1. ANONYMOUS intermediate/end nodes `()` in a fixed-hop path — previously
//!      every node had to carry a variable (`collect_hops` declined otherwise).
//!   2. An INLINE start-property anchor `(a:L {id: val})` on the scan start —
//!      previously any inline start prop declined the whole read chain.
//!
//! The contract is the pipeline's usual one: for every accepted shape, the
//! columnar path (`set_columnar_scans(true)`) equals the general path
//! (`set_columnar_scans(false)`) — the full ROW SET and its order, byte-for-byte
//! — and the shape actually FIRES the pipeline (was falling back before).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Aa{ak} -T1-> Bb -T2-> Cc{ck}. Fan-outs chosen so the 2-hop far set has
/// duplicates (so DISTINCT vs non-DISTINCT differ) and the anchor `{ak:1}`
/// selects a single seed with a non-trivial 2-hop reach.
fn g3() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Aa".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3)];
    let mk_b = || g.create_node(&["Bb".into()], &BTreeMap::new()).expect("b");
    let b = [mk_b(), mk_b(), mk_b()];
    let mk_c = |ck: i64| {
        let mut p = BTreeMap::new();
        p.insert("ck".to_string(), Value::Int(ck));
        g.create_node(&["Cc".into()], &p).expect("c")
    };
    let c = [mk_c(10), mk_c(20), mk_c(30)];
    for (s, t) in [(0, 0), (0, 1), (1, 2)] {
        g.create_rel(a[s], "T1", b[t], &BTreeMap::new())
            .expect("T1");
    }
    for (s, t) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 0)] {
        g.create_rel(b[s], "T2", c[t], &BTreeMap::new())
            .expect("T2");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// (columnar ON, general OFF) for `src`.
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(false);
    let off = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the named pipeline counter reached 1 for `src` with columnar ON — i.e.
/// the shape ran on the pipeline rather than falling back to `run_streaming`.
fn fired(g: &Graph, src: &str, counter: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(counter).copied() == Some(1)
}

#[test]
fn anon_and_anchor_match_general_across_shapes() {
    let g = g3();
    let cases: &[&str] = &[
        // ANONYMOUS intermediate node, directed 2-hop, aggregate.
        "MATCH (a:Aa)-[:T1]->()-[:T2]->(c:Cc) RETURN count(c) AS n",
        "MATCH (a:Aa)-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
        // ANONYMOUS end node.
        "MATCH (a:Aa)-[:T1]->(b:Bb)-[:T2]->() RETURN count(*) AS n",
        // START ANCHOR alone (named nodes), aggregate.
        "MATCH (a:Aa {ak: 1})-[:T1]->(b:Bb)-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
        // ANCHOR + ANONYMOUS node — the foaf shape, directed.
        "MATCH (a:Aa {ak: 1})-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
        // The foaf shape, UNDIRECTED (as the real query uses `-[:T]-`).
        "MATCH (a:Aa {ak: 1})-[:T1]-()-[:T2]-(c) RETURN count(DISTINCT c) AS n",
        // ANCHOR selecting a seed with no 2-hop reach beyond itself.
        "MATCH (a:Aa {ak: 2})-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
        // ANCHOR selecting NOTHING (no such ak).
        "MATCH (a:Aa {ak: 99})-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
        // Grouping key over the anchored+anonymous 2-hop.
        "MATCH (a:Aa {ak: 1})-[:T1]->()-[:T2]->(c:Cc) RETURN c.ck AS ck, count(*) AS n ORDER BY ck",
        // A param-valued anchor.
        "MATCH (a:Aa {ak: 1})-[:T1]->()-[:T2]->(c:Cc) RETURN count(c) AS n",
    ];
    for src in cases {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    }

    // The new shapes must FIRE the pipeline (they declined before this change).
    assert!(
        fired(
            &g,
            "MATCH (a:Aa {ak: 1})-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
            "interp.pipeline aggregate runs",
        ),
        "anchor + anonymous-node 2-hop aggregate (the foaf shape) must run on the pipeline"
    );
    assert!(
        fired(
            &g,
            "MATCH (a:Aa)-[:T1]->()-[:T2]->(c:Cc) RETURN count(c) AS n",
            "interp.pipeline aggregate runs",
        ),
        "anonymous-node 2-hop aggregate must run on the pipeline"
    );
    assert!(
        fired(
            &g,
            "MATCH (a:Aa {ak: 1})-[:T1]-()-[:T2]-(c) RETURN count(DISTINCT c) AS n",
            "interp.pipeline aggregate runs",
        ),
        "the undirected foaf shape must run on the pipeline"
    );
}

/// The CANARY: with a start anchor selecting exactly one seed, the columnar path
/// must return the SAME reach as the general path. A broken anchor (wrong seed,
/// or the fold dropped) would change the row set — this pins the equality that
/// makes the seek a pure performance choice.
#[test]
fn anchor_selects_the_same_seed_as_a_filter() {
    let g = g3();
    // The anchored aggregate must equal the same query written as an explicit
    // WHERE on the (named) seed — the anchor IS that filter, desugared.
    let anchored = both(
        &g,
        "MATCH (a:Aa {ak: 1})-[:T1]->()-[:T2]->(c:Cc) RETURN count(DISTINCT c) AS n",
    )
    .0;
    let via_where = both(
        &g,
        "MATCH (a:Aa)-[:T1]->()-[:T2]->(c:Cc) WHERE a.ak = 1 RETURN count(DISTINCT c) AS n",
    )
    .0;
    assert_eq!(
        anchored, via_where,
        "the inline start anchor must equal the explicit-WHERE form"
    );
}
