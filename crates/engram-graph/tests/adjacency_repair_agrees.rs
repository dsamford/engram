//! Does a REPAIRED adjacency table answer identically to a rebuilt one?
//!
//! The CSR adjacency table used to be rebuilt in full whenever a relationship
//! changed — a walk of every relationship in the graph, ~62 ms over the 447k of
//! an LDBC SNB corpus, which showed up as a p95 of 62-126 ms on every traversal
//! shape while a write load ran. It is now carried forward by recomputing only
//! the rows of the nodes whose edges moved.
//!
//! That is a correctness claim, and a wrong one fails SILENTLY: a traversal
//! that misses an edge returns fewer rows, and fewer rows is what a query that
//! legitimately matched less also returns.
//!
//! So every assertion here is DIFFERENTIAL, against an oracle that shares no
//! code with the table: `set_adj_table_max_entries(0)` declines tables
//! altogether and the engine walks each node's adjacency from the store
//! directly. The two must agree after every kind of edge change.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Answer `src` with the adjacency table, and again with tables declined, and
/// require agreement.
fn agrees(g: &Graph, src: &str, what: &str) -> usize {
    let mut with_table = run(g, src);
    g.set_adj_table_max_entries(0);
    let mut without = run(g, src);
    g.set_adj_table_max_entries(1 << 20);

    let key = |rows: &mut Vec<Vec<Value>>| -> Vec<String> {
        let mut v: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
        v.sort();
        v
    };
    let (a, b) = (key(&mut with_table), key(&mut without));
    assert_eq!(
        a, b,
        "the adjacency table and the direct walk disagree after {what}\n  `{src}`\n  \
         table returned {} row(s), direct walk returned {} row(s)",
        a.len(),
        b.len()
    );
    a.len()
}

fn seeded(n: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, &format!("UNWIND range(0, {}) AS i CREATE (:N {{k: i}})", n - 1));
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:N {{k: i}}), (b:N {{k: (i + 1) % {n}}}) \
             CREATE (a)-[:R]->(b)",
            n - 1
        ),
    );
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:N {{k: i}}), (b:N {{k: (i * 3 + 7) % {n}}}) \
             CREATE (a)-[:S]->(b)",
            n - 1
        ),
    );
    g
}

/// Canary: the two arms must actually take different paths.
///
/// If `set_adj_table_max_entries(0)` did not really decline the table, both
/// arms would run the identical plan and every comparison in this file would
/// be a result compared with itself.
#[test]
fn the_oracle_really_declines_the_table() {
    let g = seeded(400);
    let ((), with) = engram_observe::with_trace(|| {
        run(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c");
    });
    g.set_adj_table_max_entries(0);
    let ((), without) = engram_observe::with_trace(|| {
        run(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c");
    });
    g.set_adj_table_max_entries(1 << 20);

    let used = |t: &engram_observe::Trace| -> u64 {
        let c = t.counters();
        c.get("graph.adjacency tables built").copied().unwrap_or(0)
            + c.get("graph.adjacency tables reused").copied().unwrap_or(0)
            + c.get("graph.adjacency tables repaired").copied().unwrap_or(0)
    };
    assert!(
        used(&with) >= 1,
        "the table arm never touched an adjacency table — this file proves nothing"
    );
    assert_eq!(
        used(&without),
        0,
        "the oracle arm still used an adjacency table, so both arms are the same plan"
    );
}

#[test]
fn agrees_after_adding_edges() {
    let g = seeded(600);
    agrees(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c", "the initial load");
    for i in 0..30 {
        run(
            &g,
            &format!("MATCH (a:N {{k: {i}}}), (b:N {{k: {}}}) CREATE (a)-[:R]->(b)", i + 100),
        );
        agrees(
            &g,
            &format!("MATCH (a:N {{k: {i}}})-[:R]->(b:N) RETURN b.k"),
            &format!("adding edge {i} (the changed row)"),
        );
    }
    agrees(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c", "30 added edges");
}

#[test]
fn agrees_after_deleting_edges() {
    let g = seeded(600);
    for i in 0..20 {
        run(&g, &format!("MATCH (a:N {{k: {i}}})-[r:R]->() DELETE r"));
        let n = agrees(
            &g,
            &format!("MATCH (a:N {{k: {i}}})-[:R]->(b:N) RETURN b.k"),
            &format!("deleting node {i}'s R edge"),
        );
        assert_eq!(n, 0, "node {i} still has an :R edge after deleting it");
    }
    agrees(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c", "20 deleted edges");
}

/// The INCOMING direction, which is a different table (`b'I'`) and a different
/// dirty row than the outgoing one.
#[test]
fn agrees_on_incoming_edges() {
    let g = seeded(600);
    for i in 0..20 {
        run(
            &g,
            &format!("MATCH (a:N {{k: {}}}), (b:N {{k: {i}}}) CREATE (a)-[:R]->(b)", i + 300),
        );
        agrees(
            &g,
            &format!("MATCH (a:N)-[:R]->(b:N {{k: {i}}}) RETURN a.k"),
            &format!("adding an incoming edge to {i}"),
        );
    }
}

/// A write to ONE type must not disturb a table over ANOTHER.
///
/// This is what makes the per-type epochs safe: `:S` tables are reused across
/// `:R` writes rather than rebuilt. If the type scoping were wrong in the
/// direction of reuse, the `:S` answers would go stale here.
#[test]
fn agrees_on_an_untouched_type_while_another_is_written() {
    let g = seeded(600);
    agrees(&g, "MATCH (a:N)-[:S]->(b:N) RETURN count(*) AS c", "the initial load");
    for i in 0..25 {
        // Write :R ...
        run(
            &g,
            &format!("MATCH (a:N {{k: {i}}}), (b:N {{k: {}}}) CREATE (a)-[:R]->(b)", i + 50),
        );
        // ... and read :S, which must be unaffected AND correct.
        agrees(
            &g,
            &format!("MATCH (a:N {{k: {i}}})-[:S]->(b:N) RETURN b.k"),
            &format!(":S read after :R write {i}"),
        );
    }
    agrees(&g, "MATCH (a:N)-[:S]->(b:N) RETURN count(*) AS c", "25 :R writes");
    // And :S must still be right after :S itself is written.
    run(&g, "MATCH (a:N {k: 3}), (b:N {k: 400}) CREATE (a)-[:S]->(b)");
    agrees(&g, "MATCH (a:N {k: 3})-[:S]->(b:N) RETURN b.k", "an :S write");
}

/// The UNTYPED table (`MATCH (a)-[]->(b)`) covers every type, so a change to
/// any type must reach it.
#[test]
fn agrees_on_the_untyped_table() {
    let g = seeded(600);
    agrees(&g, "MATCH (a:N)-[]->(b:N) RETURN count(*) AS c", "the initial load");
    for i in 0..20 {
        run(
            &g,
            &format!("MATCH (a:N {{k: {i}}}), (b:N {{k: {}}}) CREATE (a)-[:T]->(b)", i + 200),
        );
        agrees(
            &g,
            &format!("MATCH (a:N {{k: {i}}})-[]->(b:N) RETURN b.k"),
            &format!("adding a :T edge from {i}, read untyped"),
        );
    }
    agrees(&g, "MATCH (a:N)-[]->(b:N) RETURN count(*) AS c", "20 :T edges");
}

/// A detach delete removes a node AND all of its edges — many rows at once,
/// on both sides, and the peers' rows change too.
#[test]
fn agrees_after_detach_deletes() {
    let g = seeded(600);
    for i in 0..15 {
        run(&g, &format!("MATCH (n:N {{k: {}}}) DETACH DELETE n", i * 7));
        agrees(
            &g,
            "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c",
            &format!("detach delete {i}"),
        );
        agrees(
            &g,
            "MATCH (a:N)-[:S]->(b:N) RETURN count(*) AS c",
            &format!("detach delete {i} (:S)"),
        );
    }
}

/// A WARMED graph must answer exactly like an unwarmed one.
///
/// `Graph::warm` fills every adjacency table — untyped and one per relationship
/// type — from a single pass that buckets rows by type, rather than from one
/// scan per table. That is a second construction path for the same structure,
/// and a second construction path is a second chance to be subtly wrong: a
/// misplaced offset, a row filed under the wrong type, a bucket truncated at a
/// budget. Any of those produces a table that answers, and answers short.
///
/// So: warm one graph, leave another cold, and require identical answers to
/// every shape — typed both directions, untyped, and multi-hop across types.
#[test]
fn warming_does_not_change_any_answer() {
    let warmed = seeded(600);
    let cold = seeded(600);
    let report = warmed.warm();
    eprintln!(
        "warmed: {} nodes, {} out-edges, {} in-edges, {} tables",
        report.nodes, report.out_edges, report.in_edges, report.tables
    );
    assert!(
        report.tables >= 6,
        "warming cached only {} adjacency tables — with two relationship types \
         and two directions there should be at least 6 (untyped + :R + :S, each \
         way). A warm that builds nothing cannot be distinguished from one that \
         works, except by this count.",
        report.tables
    );

    for q in [
        "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c",
        "MATCH (a:N)-[:S]->(b:N) RETURN count(*) AS c",
        "MATCH (a:N)-[]->(b:N) RETURN count(*) AS c",
        "MATCH (a:N {k: 7})-[:R]->(b:N) RETURN b.k",
        "MATCH (a:N)-[:R]->(b:N {k: 7}) RETURN a.k",
        "MATCH (a:N)-[:R]->()-[:S]->(c:N) RETURN count(*) AS c",
    ] {
        let mut w = run(&warmed, q);
        let mut c = run(&cold, q);
        let key = |rows: &mut Vec<Vec<Value>>| -> Vec<String> {
            let mut v: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
            v.sort();
            v
        };
        assert_eq!(
            key(&mut w),
            key(&mut c),
            "a warmed graph and a cold one disagree on `{q}` — the one-pass \
             bucketed table build does not reproduce the per-table build"
        );
    }

    // And it must stay right once the warmed tables start being repaired.
    for i in 0..10 {
        for g in [&warmed, &cold] {
            run(
                g,
                &format!("MATCH (a:N {{k: {i}}}), (b:N {{k: {}}}) CREATE (a)-[:R]->(b)", i + 77),
            );
        }
        let mut w = run(&warmed, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c");
        let mut c = run(&cold, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c");
        assert_eq!(
            format!("{w:?}"),
            format!("{c:?}"),
            "warmed and cold diverged after {} edge insert(s)",
            i + 1
        );
        let _ = (&mut w, &mut c);
    }
}

/// After warming, a query must not BUILD anything.
///
/// The point of warming is that the first query is served, not that a warm
/// routine ran. Those are different claims, and only this one is worth having:
/// a warm that builds tables into a cache that then evicts them reports
/// success and changes nothing.
///
/// Which is exactly what happened. The adjacency-table cache dropped everything
/// once it held 32 entries, and an LDBC SNB schema needs precisely 32 — 15
/// relationship types plus the untyped table, in two directions — so warming
/// evicted its own work on the final insert. A 1.48M-node graph warmed for six
/// seconds and still stalled five seconds on its first query.
///
/// This fixture uses 20 types (42 tables) so it sits above the old limit and
/// would fail against it.
#[test]
fn after_warming_a_query_builds_nothing() {
    const TYPES: usize = 20;
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "UNWIND range(0, 399) AS i CREATE (:N {k: i})");
    for t in 0..TYPES {
        run(
            &g,
            &format!(
                "UNWIND range(0, 399) AS i MATCH (a:N {{k: i}}), (b:N {{k: (i + {}) % 400}}) \
                 CREATE (a)-[:T{t}]->(b)",
                t + 1
            ),
        );
    }

    let report = g.warm();
    eprintln!("warmed {} adjacency tables for {TYPES} types", report.tables);
    assert!(
        report.tables >= (TYPES + 1) * 2,
        "warming cached {} tables for {TYPES} relationship types — expected at least \
         {} (untyped + one per type, each direction)",
        report.tables,
        (TYPES + 1) * 2
    );

    // Now read every type. None of them may build.
    let ((), trace) = engram_observe::with_trace(|| {
        for t in 0..TYPES {
            run(&g, &format!("MATCH (a:N)-[:T{t}]->(b:N) RETURN count(*) AS c"));
            run(&g, &format!("MATCH (a:N)<-[:T{t}]-(b:N) RETURN count(*) AS c"));
        }
        run(&g, "MATCH (a:N)-[]->(b:N) RETURN count(*) AS c");
    });
    let built = trace
        .counters()
        .get("graph.adjacency tables built")
        .copied()
        .unwrap_or(0);
    let reused = trace
        .counters()
        .get("graph.adjacency tables reused")
        .copied()
        .unwrap_or(0);
    eprintln!("after warming: {built} table(s) built, {reused} reuse(s)");
    assert_eq!(
        built, 0,
        "after warming, reading every relationship type still BUILT {built} table(s) — \
         the warm did not survive to serve a query. Check `ADJ_TABLE_CACHE_MAX` against \
         the number of tables warming inserts: a cache that clears at exactly that many \
         evicts the warm as it finishes."
    );
    assert!(
        reused >= 1,
        "no adjacency table was reused, so this test did not observe the cache at all"
    );
}

/// A long interleaving that never lets the table settle — the state a mixed
/// workload is always in, and where a small per-repair error would accumulate.
#[test]
fn agrees_across_a_long_interleaving() {
    let g = seeded(800);
    for round in 0..40i64 {
        run(
            &g,
            &format!("MATCH (a:N {{k: {round}}}), (b:N {{k: {}}}) CREATE (a)-[:R]->(b)", round + 11),
        );
        if round % 3 == 0 {
            run(&g, &format!("MATCH (a:N {{k: {}}})-[r:S]->() DELETE r", round + 1));
        }
        if round % 4 == 0 {
            run(
                &g,
                &format!("MATCH (a:N {{k: {}}}), (b:N {{k: {round}}}) CREATE (a)-[:S]->(b)", round + 5),
            );
        }
        agrees(
            &g,
            &format!("MATCH (a:N {{k: {round}}})-[:R]->(b:N) RETURN b.k"),
            &format!("round {round} out"),
        );
        agrees(
            &g,
            &format!("MATCH (a:N)-[:R]->(b:N {{k: {}}}) RETURN a.k", round + 11),
            &format!("round {round} in"),
        );
    }
    agrees(&g, "MATCH (a:N)-[:R]->(b:N) RETURN count(*) AS c", "the whole interleaving");
    agrees(&g, "MATCH (a:N)-[:S]->(b:N) RETURN count(*) AS c", "the whole interleaving (:S)");
    agrees(
        &g,
        "MATCH (a:N)-[:R]->()-[:S]->(c:N) RETURN count(*) AS c",
        "the whole interleaving (two hops, two types)",
    );
}
