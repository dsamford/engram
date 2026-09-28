//! A `CALL` that ends a query is itself the result.
//!
//! Until the procedure catalogue existed, a bare `CALL dbms.components()`
//! produced NOTHING — no columns and no rows — because only a `RETURN` ever
//! set a result and only a `YIELD` ever pushed a column name. Every procedure
//! call therefore needed both, which is neither Cypher's rule nor anything a
//! driver expects.
//!
//! The rule now implemented is Neo4j's and openCypher's: **`YIELD` may be
//! omitted only when the `CALL` is the final clause**, and in that position the
//! procedure's declared output columns become the result columns, in
//! declaration order. The negatives below pin the other half of that rule —
//! that a CALL which is *not* last still requires its `YIELD`, and that the
//! declared columns are not silently bound into scope.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> engram_graph::interp::QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn err(g: &Graph, src: &str) -> String {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    match run_query(g, &q, BTreeMap::new()) {
        Ok(r) => panic!(
            "`{src}` was expected to be refused, but answered {:?}",
            r.columns
        ),
        Err(e) => e.to_string(),
    }
}

fn seed(g: &Graph) {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str("a".into()));
    let a = g.create_node(&["Person".into()], &m).expect("node");
    let b = g.create_node(&["Company".into()], &m).expect("node");
    g.create_rel(a, "WORKS_AT", b, &BTreeMap::new())
        .expect("rel");
}

// ─── The positives ─────────────────────────────────────────────────────────

#[test]
fn a_standalone_call_returns_the_procedures_default_columns() {
    let g = g();
    let r = run(&g, "CALL dbms.components()");
    assert_eq!(
        r.columns,
        vec!["name".to_string(), "versions".into(), "edition".into()],
        "the columns come from the catalogue's declared output signature",
    );
    assert_eq!(r.rows.len(), 1, "one component row");
    assert_eq!(r.rows[0][0], Value::Str("Engram".into()));
}

#[test]
fn a_standalone_call_over_a_multi_row_procedure_returns_every_row() {
    let g = g();
    seed(&g);
    let r = run(&g, "CALL db.labels()");
    assert_eq!(r.columns, vec!["label".to_string()]);
    let mut labels: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Str(s) => s.clone(),
            other => panic!("a label must be a string, got {other:?}"),
        })
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["Company".to_string(), "Person".into()]);
}

#[test]
fn a_call_with_yield_as_the_last_clause_returns_the_yielded_columns() {
    let g = g();
    seed(&g);
    // YIELD, no RETURN — which also produced nothing before.
    let r = run(&g, "CALL db.labels() YIELD label");
    assert_eq!(r.columns, vec!["label".to_string()]);
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn a_yield_alias_renames_the_column() {
    let g = g();
    let r = run(&g, "CALL dbms.components() YIELD name AS product");
    assert_eq!(r.columns, vec!["product".to_string()]);
    assert_eq!(r.rows[0][0], Value::Str("Engram".into()));
}

#[test]
fn a_yield_selects_a_subset_in_the_order_written() {
    let g = g();
    let r = run(&g, "CALL dbms.components() YIELD edition, name");
    assert_eq!(r.columns, vec!["edition".to_string(), "name".into()]);
    assert_eq!(r.rows[0][0], Value::Str("engram".into()));
    assert_eq!(r.rows[0][1], Value::Str("Engram".into()));
}

#[test]
fn the_long_form_still_works_unchanged() {
    // Every query written against the old rule must keep working; this is the
    // form the book told people to use.
    let g = g();
    let r = run(
        &g,
        "CALL dbms.components() YIELD name, versions, edition RETURN name, edition",
    );
    assert_eq!(r.columns, vec!["name".to_string(), "edition".into()]);
    assert_eq!(r.rows[0][0], Value::Str("Engram".into()));
}

#[test]
fn a_trailing_where_still_filters_a_yield_less_call() {
    let g = g();
    seed(&g);
    let r = run(&g, "CALL db.labels() YIELD label WHERE label = 'Person'");
    assert_eq!(r.rows, vec![vec![Value::Str("Person".into())]]);
}

#[test]
fn await_indexes_answers_immediately_in_both_arities() {
    // A driver calls this on connect. There is nothing to await here — index
    // builds are single-flight on the read path — so `true` is the honest
    // answer, and the optional timeout is accepted rather than refused.
    let g = g();
    for src in ["CALL db.awaitIndexes()", "CALL db.awaitIndexes(5)"] {
        let r = run(&g, src);
        assert_eq!(r.columns, vec!["ok".to_string()], "{src}");
        assert_eq!(r.rows, vec![vec![Value::Bool(true)]], "{src}");
    }
}

#[test]
fn db_labels_over_an_empty_graph_still_names_its_column() {
    // The old code derived its field names from the FIRST catalog row and
    // needed a hand-maintained fallback list for when there were none. The
    // catalogue has no such gap.
    let g = g();
    let r = run(&g, "CALL db.labels()");
    assert_eq!(r.columns, vec!["label".to_string()]);
    assert!(r.rows.is_empty());
}

// ─── The negatives ─────────────────────────────────────────────────────────

#[test]
fn a_call_that_is_not_the_last_clause_without_yield_is_refused() {
    let g = g();
    seed(&g);
    let e = err(&g, "CALL db.labels() RETURN 1 AS one");
    assert!(
        e.contains("YIELD is required when CALL is not the last clause"),
        "the refusal must name the rule, got: {e}",
    );
    assert!(
        e.contains("db.labels"),
        "the refusal must name the procedure, got: {e}",
    );
}

#[test]
fn a_bare_call_then_return_names_an_unbound_variable() {
    // THE HALF OF THE RULE THAT IS DELIBERATELY NOT CLOSED. Binding a
    // procedure's declared outputs into scope without a YIELD would let a
    // later clause capture a name the user never wrote — `CALL p() MATCH
    // (score)` being the shape that bites. Neo4j forbids it for that reason
    // and so do we.
    let g = g();
    seed(&g);
    let e = err(&g, "CALL db.labels() RETURN label");
    assert!(
        !e.is_empty(),
        "`CALL db.labels() RETURN label` must be refused, not answered",
    );
}

#[test]
fn a_yield_of_a_field_the_procedure_does_not_declare_is_refused() {
    let g = g();
    let e = err(&g, "CALL db.labels() YIELD labels");
    assert!(
        e.contains("does not yield `labels`"),
        "the refusal must name the field, got: {e}",
    );
    assert!(
        e.contains("label"),
        "the refusal must list what the procedure DOES yield, got: {e}",
    );
}

#[test]
fn a_procedure_called_with_the_wrong_arity_is_refused_before_it_runs() {
    let g = g();
    let e = err(&g, "CALL db.labels(1)");
    assert!(
        e.contains("takes 0 arguments, got 1"),
        "the refusal must state both counts, got: {e}",
    );
    let e = err(&g, "CALL db.awaitIndexes(1, 2)");
    assert!(
        e.contains("0 to 1"),
        "an optional argument makes arity a range, got: {e}",
    );
}

#[test]
fn an_unknown_procedure_still_says_unsupported_not_unknown() {
    // Drivers switch on the error VARIANT to decide whether to fall back, so
    // this must keep producing `Unsupported` and not become some new class the
    // callers have never seen. The catalogue moved the decision, not the
    // diagnostic.
    let g = g();
    let q = parse_statement("CALL db.nosuchthing()").expect("parses");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(engram_graph::interp::RunError::Unsupported(m)) => {
            assert!(m.contains("db.nosuchthing"), "must name it, got: {m}");
        }
        Err(other) => panic!("expected Unsupported, got {other:?}"),
        Ok(_) => panic!("an unknown procedure must not answer"),
    }
}

#[test]
fn an_unknown_procedure_keeps_its_error_variant_even_when_it_is_not_last() {
    // DRIVERS SWITCH ON THE VARIANT. `Unsupported` becomes
    // `Neo.ClientError.Statement.NotSupported`, which tells a client to fall
    // back; a semantic error does not. Checking the clause position before the
    // name reported the wrong one of the statement's two problems and changed
    // the status code — an audit caught it.
    let g = g();
    let q = parse_statement("CALL db.nosuchthing() RETURN 1 AS one").expect("parses");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(engram_graph::interp::RunError::Unsupported(m)) => {
            assert!(m.contains("db.nosuchthing"), "must name it, got: {m}");
        }
        Err(other) => panic!("expected Unsupported for an unknown name, got {other:?}"),
        Ok(_) => panic!("an unknown procedure must not answer"),
    }
    // And a KNOWN procedure in the same position still reports the YIELD rule.
    let e = err(&g, "CALL db.labels() RETURN 1 AS one");
    assert!(e.contains("YIELD is required"), "got: {e}");
}

#[test]
fn an_unknown_procedure_that_is_not_last_reports_only_one_thing() {
    // Two refusals for one mistake is worse than one. A CALL of a procedure
    // that does not exist AND has no YIELD must say the procedure does not
    // exist — the YIELD rule is not the user's problem here.
    let g = g();
    let e = err(&g, "CALL db.nosuchthing() RETURN 1 AS one");
    assert!(
        e.contains("db.nosuchthing"),
        "the unknown name is the useful diagnostic, got: {e}",
    );
}
