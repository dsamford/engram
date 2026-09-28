#![allow(non_snake_case)]
//! An empty intermediate result is not an undefined variable.
//!
//! `check_where_scope` refuses a WHERE that reads a name nothing binds, and it
//! reads the bound set from `rows.first()` — a SAMPLE ROW. When the incoming
//! result is empty that set is empty too, so every variable the WHERE reads
//! looks undefined and a query which should return zero rows raises
//! `Variable `x` not defined` instead.
//!
//! Found through SNB BI bi13 on 2026-09-15: it returns 100 rows at SF3 and the
//! SAME statement on the SAME binary failed at SF0.1, because SF0.1 has nobody
//! meeting the zombie criteria so the preceding WITH produced nothing. The
//! SCALE FACTOR decided whether a valid query was an error — and the smaller
//! corpus is the one that fails, which is the wrong way round for anyone
//! developing against a sample.
//!
//! The refusal itself is worth keeping: evaluating an unbound name
//! materialises every row before eval discovers it does not exist, and the
//! port benchmark watched the OOM killer answer instead of the error. With no
//! rows there is nothing to materialise, and a genuinely undefined variable is
//! still caught at compile time by `validate_single`.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:P {id: 1}), (:P {id: 2})");
    g
}

#[test]
fn a_where_over_an_empty_intermediate_result_returns_no_rows_not_an_error() {
    // The bi13 shape reduced: collect into a list, UNWIND it, then a leg whose
    // WHERE reads the list — with a filter upstream that matches NOTHING, so
    // the collect yields an empty list and the UNWIND yields no rows.
    let g = graph();
    let r = run(
        &g,
        "MATCH (p:P) WHERE p.id > 1000 \
         WITH collect(p) AS ps \
         UNWIND ps AS one \
         OPTIONAL MATCH (one)-[:KNOWS]-(o:P) WHERE o IN ps \
         RETURN count(*) AS n",
    );
    assert_eq!(
        r.rows.len(),
        1,
        "an aggregate with no grouping returns ONE row even over nothing: {:?}",
        r.rows
    );
    assert_eq!(
        r.rows[0].first(),
        Some(&Value::Int(0)),
        "and that row counts zero: {:?}",
        r.rows
    );
}

#[test]
fn the_same_query_still_answers_when_the_result_is_NOT_empty() {
    // The other side: the guard must not have disabled the path. With rows
    // present the query runs and counts them.
    let g = graph();
    let r = run(
        &g,
        "MATCH (p:P) WHERE p.id > 0 \
         WITH collect(p) AS ps \
         UNWIND ps AS one \
         OPTIONAL MATCH (one)-[:KNOWS]-(o:P) WHERE o IN ps \
         RETURN count(*) AS n",
    );
    assert_eq!(
        r.rows[0].first(),
        Some(&Value::Int(2)),
        "two people, no KNOWS edges, two null-filled optional rows: {:?}",
        r.rows
    );
}

#[test]
fn a_genuinely_undefined_variable_is_still_refused() {
    // The guard must not have opened the door the refusal exists to close. A
    // name nothing binds ANYWHERE is a compile-time UndefinedVariable and is
    // caught without looking at data.
    let g = graph();
    let q = parse_statement("MATCH (n:P) WHERE n.id = nosuchvar RETURN n")
        .expect("this parses; it is the SCOPE that is wrong");
    let e = run_query(&g, &q, BTreeMap::new())
        .expect_err("a WHERE reading a name nothing binds must still be refused");
    let msg = format!("{e}");
    assert!(
        msg.contains("nosuchvar"),
        "the refusal must name the variable: {msg}"
    );
}
