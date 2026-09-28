#![allow(non_snake_case)]
//! `all(x IN <var-length rels> WHERE p)` is applied DURING the expansion, not
//! to the finished paths.
//!
//! A variable-length pattern was enumerated first and filtered afterwards, so a
//! predicate false of the very first edge still paid for every path that edge
//! led to. On LDBC FinBench at SF10 that is the difference between answering
//! and not answering at all. From the busiest account,
//! `-[:transfer*1..3]->` is 815 edges at one hop and 265,860 paths at two, and
//! counting the three-hop paths did not finish in 600 s:
//!
//! ```text
//! count(*)          over *1..3            KILLED at 600 s
//! count(DISTINCT o) over *1..3            43, under 1 s
//! ```
//!
//! The second is fast because it is reachability rather than enumeration. Then
//! the telling measurement: adding a predicate that NO edge can satisfy — a
//! timestamp later than any in the corpus — changed nothing. Still killed at
//! 300 s, in every spelling:
//!
//! ```text
//! no predicate                             KILLED at 300 s
//! all(e IN r            WHERE e.ts > max)  KILLED at 300 s
//! all(e IN relationships(p) WHERE …)       KILLED at 300 s
//! all(e IN relationships(p) WHERE startNode(e)… )  KILLED at 300 s
//! ```
//!
//! The predicate was being asked of rows, and the rows were exactly what could
//! not be produced. The third spelling is the one LDBC's own portable
//! truncation rewrite uses, so without this the documented way to run FinBench
//! on an engine without native truncation does not work either.
//!
//! THE LOAD-BEARING TEST IS DIFFERENTIAL. Pushdown may not change an answer:
//! the lift COPIES the conjunct and leaves the WHERE in place, so the lever ON
//! and OFF must return identical rows on every query here. The counter test
//! then proves the work was actually avoided rather than merely repeated.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SKIPPED: &str = "interp.expansion skipped an edge its own predicate refuses";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// The same query with the lever on and off.
fn both_ways(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_rel_predicate_pushdown(true);
    let on = rows(g, src);
    g.set_rel_predicate_pushdown(false);
    let off = rows(g, src);
    g.set_rel_predicate_pushdown(true);
    (on, off)
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// A hub that fans out to `width` accounts, each of which fans out again — so
/// the walk is wide enough that pruning at the FIRST hop is visible, and the
/// edges carry a `ts` the predicate can select on.
///
/// The first hop's edges are stamped 0, the rest 100. A predicate demanding
/// `ts > 50` is therefore false of every first-hop edge and true of everything
/// beyond it: pruning correctly returns nothing, and pruning at the right
/// moment means the second hop is never walked at all.
fn hub(width: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut mk = |i: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        g.create_node(&["Account".into()], &m).expect("account")
    };
    let root = mk(0);
    let first: Vec<u64> = (1..=width as i64).map(&mut mk).collect();
    let second: Vec<u64> = (1001..=1000 + width as i64).map(&mut mk).collect();
    let edge = |a: u64, b: u64, ts: i64| {
        let mut m = BTreeMap::new();
        m.insert("ts".to_string(), Value::Int(ts));
        g.create_rel(a, "transfer", b, &m).expect("transfer");
    };
    for &f in &first {
        edge(root, f, 0);
        for &s in &second {
            edge(f, s, 100);
        }
    }
    let _ = g.warm();
    g
}

/// The whole point: a predicate no first-hop edge satisfies must stop the walk
/// there, so the `width * width` second-hop edges are never considered.
#[test]
fn an_edge_that_fails_the_predicate_is_not_walked_through() {
    let g = hub(8);
    let q = "MATCH p=(:Account {id: 0})-[r:transfer*1..3]->(o:Account) \
             WHERE all(e IN r WHERE e.ts > 50) RETURN count(*) AS n";

    let (_, t) = engram_observe::with_trace(|| rows(&g, q));
    let skipped = counter(&t, SKIPPED);

    // Every first-hop edge is refused, and nothing beyond them is reached, so
    // the count is the hub's own out-degree and NOT the whole fan-out.
    assert_eq!(
        skipped, 8,
        "expected the 8 first-hop edges to be refused and the walk to stop \
         there; {skipped} edges were refused, which means it kept walking"
    );
}

/// Pushdown must not change an answer — the WHERE is still there.
#[test]
fn the_lever_does_not_change_any_answer() {
    let g = hub(6);
    for q in [
        // nothing survives
        "MATCH p=(:Account {id: 0})-[r:transfer*1..3]->(o:Account) \
         WHERE all(e IN r WHERE e.ts > 50) RETURN count(*) AS n",
        // everything survives
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE all(e IN r WHERE e.ts >= 0) RETURN count(*) AS n",
        // only the first hop survives, so the 2-hop paths are cut
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE all(e IN r WHERE e.ts < 50) RETURN count(*) AS n",
        // the endpoints themselves, not just a count
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE all(e IN r WHERE e.ts < 50) RETURN o.id AS id ORDER BY id",
        // the `relationships(p)` spelling over a single-hop path
        "MATCH p=(:Account {id: 0})-[:transfer*1..2]->(o:Account) \
         WHERE all(e IN relationships(p) WHERE e.ts < 50) RETURN o.id AS id ORDER BY id",
        // length(p) still reads the real path
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE all(e IN r WHERE e.ts >= 0) RETURN length(p) AS d, count(*) AS n \
         ORDER BY d",
    ] {
        let (on, off) = both_ways(&g, q);
        assert_eq!(on, off, "pushdown changed the answer to `{q}`");
    }
}

/// A predicate that mentions another variable cannot be lifted — the expansion
/// has no binding for it — and the query must still answer correctly.
#[test]
fn a_predicate_that_reads_another_variable_is_left_alone() {
    let g = hub(4);
    let q = "MATCH (o:Account {id: 1}) \
             MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(x:Account) \
             WHERE all(e IN r WHERE e.ts < o.id + 50) RETURN count(*) AS n";
    let (on, off) = both_ways(&g, q);
    assert_eq!(on, off, "a non-liftable predicate changed the answer");

    let (_, t) = engram_observe::with_trace(|| rows(&g, q));
    assert_eq!(
        counter(&t, SKIPPED),
        0,
        "a predicate reading an outer variable must not be pushed into the walk"
    );
}

/// `any`/`none` are false of a path for reasons that are not true of its
/// individual edges, so they must not be lifted.
#[test]
fn only_all_is_lifted() {
    let g = hub(4);
    for q in [
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE any(e IN r WHERE e.ts > 50) RETURN count(*) AS n",
        "MATCH p=(:Account {id: 0})-[r:transfer*1..2]->(o:Account) \
         WHERE none(e IN r WHERE e.ts > 50) RETURN count(*) AS n",
    ] {
        let (on, off) = both_ways(&g, q);
        assert_eq!(on, off, "`{q}` changed under the lever");

        let (_, t) = engram_observe::with_trace(|| rows(&g, q));
        assert_eq!(
            counter(&t, SKIPPED),
            0,
            "`{q}` was pushed into the walk; only `all` may be"
        );
    }
}
