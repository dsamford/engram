#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The per-type adjacency EPOCH must key on the TYPE.
//!
//! # What this guards
//!
//! `adjacency_epoch` answers "how fresh must a table for these types be" and
//! is the clock the hop-count memo's validity keys on. Epochs move
//! independently per type: a type whose counter lags would let a genuinely
//! stale table pass `at >= epoch` and serve rows from before a write.
//!
//! This file previously also guarded a thread-local memo of the epoch CELLS
//! (`adj_epoch_value`, v60). P-6 measured that memo as a straight loss —
//! slower on every heavy LSQB query (q9 +20.6%, q3 +15.5%, q8 +15.2%,
//! q5 +9.2%; N=3 interleaved, throttle flat) — and it was DELETED. The epoch
//! keying it sat on top of remains load-bearing, so these tests remain.
//!
//! # Why the direct epoch read exists (`adjacency_epoch_for_test`)
//!
//! The keying is not observable through query answers: a relationship write
//! calls `retract_adj_tables`, which drops the type's table outright, so the
//! next read rebuilds and is correct whatever epoch it asked for. Retraction
//! masks the fault. Two query-level canaries were built against a deliberately
//! token-blind implementation and both passed; reading the epochs directly is
//! what made the property bite.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

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

const N: i64 = 24;

/// `KNOWS` at stride 1, `LIKES` at strides 1 and 2 — different out-degrees, so
/// the two types cannot be confused by count.
fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0); // admit tables at once; a small fixture builds none otherwise
    for p in 0..N {
        stmt(&g, &format!("CREATE (:P {{id: {p}}})"));
    }
    for p in 0..N {
        for (ty, q) in [
            ("KNOWS", (p + 1) % N),
            ("LIKES", (p + 1) % N),
            ("LIKES", (p + 2) % N),
        ] {
            stmt(
                &g,
                &format!("MATCH (x:P {{id: {p}}}), (y:P {{id: {q}}}) CREATE (x)-[:{ty}]->(y)"),
            );
        }
    }
    g.shared_store().seal();
    g
}

const KK: &str = "MATCH (a:P)-[:KNOWS]->(b:P)-[:KNOWS]->(c:P) RETURN count(*) AS c";
const LL: &str = "MATCH (a:P)-[:LIKES]->(b:P)-[:LIKES]->(c:P) RETURN count(*) AS c";

/// THE test for the keying, read straight off `adjacency_epoch`.
///
/// The two types are written at different times, so their counters hold
/// different values, and an implementation that serves one type's counter for
/// another reports the wrong one here in one line.
#[test]
fn each_type_gets_its_own_epoch() {
    let g = fixture();
    let knows = g.type_token_peek("KNOWS").expect("KNOWS minted");
    let likes = g.type_token_peek("LIKES").expect("LIKES minted");

    // Move ONLY the KNOWS epoch, so the two counters cannot coincide.
    stmt(
        &g,
        "MATCH (x:P {id: 0}), (y:P {id: 11}) CREATE (x)-[:KNOWS]->(y)",
    );

    for i in 0..20 {
        let k = g.adjacency_epoch_for_test(&[knows]);
        let l = g.adjacency_epoch_for_test(&[likes]);
        assert!(
            k > l,
            "KNOWS was written after LIKES, so its epoch must be strictly \
             greater — round {i} saw KNOWS={k} LIKES={l}. Equal values mean one \
             counter is being served for both types."
        );
    }

    // And a write to LIKES must move LIKES' epoch, not KNOWS'.
    let before = g.adjacency_epoch_for_test(&[knows]);
    stmt(
        &g,
        "MATCH (x:P {id: 1}), (y:P {id: 12}) CREATE (x)-[:LIKES]->(y)",
    );
    let l_after = g.adjacency_epoch_for_test(&[likes]);
    let k_after = g.adjacency_epoch_for_test(&[knows]);
    assert!(
        l_after > before,
        "the LIKES write must raise the LIKES epoch: {l_after} vs {before}"
    );
    assert_eq!(
        k_after, before,
        "the LIKES write must NOT raise the KNOWS epoch — a shared counter \
         would move both"
    );
}

#[test]
fn a_write_to_a_type_survives_reading_another_type() {
    let g = fixture();
    assert_eq!(count(&g, KK), N, "out-degree 1 twice");
    assert_eq!(count(&g, LL), N * 4, "out-degree 2 twice");

    let mut prev = count(&g, KK);
    for i in 0..10 {
        // Read the OTHER type between the write and the re-read, so any
        // freshness answer confused across types has its chance to serve a
        // stale KNOWS table.
        assert_eq!(count(&g, LL), N * 4, "LIKES unchanged, round {i}");
        let p = i;
        let q = (i + 7) % N;
        stmt(
            &g,
            &format!("MATCH (x:P {{id: {p}}}), (y:P {{id: {q}}}) CREATE (x)-[:KNOWS]->(y)"),
        );
        let now = count(&g, KK);
        assert!(
            now > prev,
            "the KNOWS edge just written must be visible to the next KNOWS read, \
             even though a LIKES read came between — round {i} returned {now}, \
             unchanged from {prev}"
        );
        prev = now;
    }
}
