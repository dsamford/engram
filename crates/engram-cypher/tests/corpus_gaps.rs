#![allow(non_snake_case)]
//! The grammar the corpus sweep surfaced — each feature pinned at parse AND
//! evaluation where a graph is not required.

use engram_cypher::stmt::SchemaCmd;
use engram_cypher::{
    Clause, ConstraintKind, EvalError, Expr, Query, Scope, Stmt, Value, eval, parse_any,
    parse_expression, parse_statement,
};

fn v(src: &str) -> Value {
    eval(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &Scope::default(),
    )
    .unwrap_or_else(|e| panic!("eval `{src}`: {e}"))
}

// ─── Label predicates ───────────────────────────────────────────────────────

#[test]
fn label_predicates_evaluate_from_the_bound_node() {
    let mut scope = Scope::default();
    scope.bind(
        "n",
        Value::Node {
            id: 1,
            labels: vec!["Position".into(), "Live".into()],
            props: Default::default(),
        },
    );
    let run = |src: &str| eval(&parse_expression(src).expect("parses"), &scope).expect("evals");
    assert_eq!(run("n:Position"), Value::Bool(true));
    assert_eq!(run("n:Narrative"), Value::Bool(false));
    assert_eq!(
        run("n:Position:Live"),
        Value::Bool(true),
        "multi-label is an AND"
    );
    assert_eq!(run("n:Position:Narrative"), Value::Bool(false));
    assert_eq!(
        run("n:Position OR n:Narrative"),
        Value::Bool(true),
        "the corpus shape verbatim"
    );
    scope.bind("m", Value::Null);
    assert_eq!(
        eval(&parse_expression("m:X").expect("p"), &scope).expect("e"),
        Value::Null,
        "null propagates"
    );
}

#[test]
fn label_predicates_do_not_collide_with_map_keys_or_set_labels() {
    // The colon's other jobs still work.
    assert_eq!(v("{a: 1}.a"), Value::Int(1));
    let clauses = match parse_statement("MATCH (n) SET n:Label RETURN n").expect("parses") {
        Query::Single(q) => q.clauses,
        _ => panic!(),
    };
    assert!(matches!(&clauses[1], Clause::Set { .. }));
}

// ─── Pattern predicates ─────────────────────────────────────────────────────

#[test]
fn a_bare_pattern_is_a_predicate_and_parens_still_group() {
    // Committed only when a relationship follows — arithmetic grouping is
    // untouched.
    assert_eq!(v("(1 + 2) * 3"), Value::Int(9));
    let e = parse_expression("NOT (a)<-[:AUTHORED_BY]-()").expect("parses");
    assert!(matches!(e, Expr::Not(inner) if matches!(*inner, Expr::PatternPredicate(_))));
    // exists(pattern) rewrites to the pattern predicate at parse time, so
    // the boolean-vs-property ambiguity never reaches the evaluator.
    let e = parse_expression("exists((a)-[:R]->(b))").expect("parses");
    assert!(matches!(e, Expr::PatternPredicate(_)));
    // …while exists(property) stays the null test.
    assert_eq!(v("exists(1)"), Value::Bool(true));
    // Without a graph, a pattern predicate refuses by name.
    let err = eval(
        &parse_expression("(a)-[:R]->(b)").expect("parses"),
        &Scope::default(),
    )
    .unwrap_err();
    assert!(matches!(err, EvalError::GraphDependent(_)));
}

// ─── List predicates ────────────────────────────────────────────────────────

#[test]
fn list_predicates_follow_the_three_valued_tables() {
    assert_eq!(v("any(x IN [1, 2, 3] WHERE x > 2)"), Value::Bool(true));
    assert_eq!(v("any(x IN [1, 2] WHERE x > 5)"), Value::Bool(false));
    assert_eq!(
        v("any(x IN [1, null] WHERE x > 5)"),
        Value::Null,
        "an unknown poisons a miss"
    );
    assert_eq!(v("all(x IN [1, 2] WHERE x > 0)"), Value::Bool(true));
    assert_eq!(v("all(x IN [1, null] WHERE x > 0)"), Value::Null);
    assert_eq!(
        v("all(x IN [1, null] WHERE x > 1)"),
        Value::Bool(false),
        "a definite false wins"
    );
    assert_eq!(v("none(x IN [1, 2] WHERE x > 5)"), Value::Bool(true));
    assert_eq!(v("none(x IN [1, 2] WHERE x > 1)"), Value::Bool(false));
    assert_eq!(v("single(x IN [1, 2, 3] WHERE x = 2)"), Value::Bool(true));
    assert_eq!(v("single(x IN [2, 2] WHERE x = 2)"), Value::Bool(false));
    assert_eq!(v("any(x IN null WHERE x)"), Value::Null);
    // ALL is also a keyword (UNION ALL) — both roles coexist.
    assert_eq!(v("ALL(x IN [1] WHERE x = 1)"), Value::Bool(true));
}

// ─── Map projections ────────────────────────────────────────────────────────

#[test]
fn map_projections_project_nodes_and_maps() {
    let mut scope = Scope::default();
    let mut props = std::collections::BTreeMap::new();
    props.insert("name".to_string(), Value::Str("Ada".into()));
    props.insert("age".to_string(), Value::Int(36));
    scope.bind(
        "n",
        Value::Node {
            id: 1,
            labels: vec![],
            props,
        },
    );
    scope.bind("extra", Value::Int(9));
    let run = |src: &str| eval(&parse_expression(src).expect("parses"), &scope).expect("evals");
    let Value::Map(m) = run("n {.name, doubled: n.age + n.age, extra}") else {
        panic!()
    };
    assert_eq!(m.get("name"), Some(&Value::Str("Ada".into())));
    assert_eq!(m.get("doubled"), Some(&Value::Int(72)));
    assert_eq!(m.get("extra"), Some(&Value::Int(9)));
    let Value::Map(m) = run("n {.*}") else {
        panic!()
    };
    assert_eq!(m.len(), 2, ".* copies every property");
    let Value::Map(m) = run("n {.missing}") else {
        panic!()
    };
    assert_eq!(
        m.get("missing"),
        Some(&Value::Null),
        "a missing property projects null"
    );
    // A map literal stays a map literal (no preceding expression).
    assert_eq!(v("{a: 1}.a"), Value::Int(1));
}

// ─── Soft keywords as variables ─────────────────────────────────────────────

#[test]
fn count_aliases_and_reads_back_as_a_variable() {
    // The corpus shape verbatim: count(*) AS count … WHERE count > 1.
    let clauses = match parse_statement(
        "MATCH (r:Repo) WITH r.orgId AS orgId, count(*) AS count WHERE count > 1 RETURN orgId, count",
    )
    .expect("parses")
    {
        Query::Single(q) => q.clauses,
        _ => panic!(),
    };
    assert_eq!(clauses.len(), 3);
    let mut scope = Scope::default();
    scope.bind("count", Value::Int(3));
    assert_eq!(
        eval(&parse_expression("count > 1").expect("p"), &scope).expect("e"),
        Value::Bool(true)
    );
}

// ─── DDL: composite constraints, NODE KEY, rel indexes, SHOW, CALL () ──────

#[test]
fn composite_require_and_node_key_parse() {
    let Stmt::Schema(SchemaCmd::CreateConstraint { props, kind, .. }) = parse_any(
        "CREATE CONSTRAINT c IF NOT EXISTS FOR (p:Policy) REQUIRE (p.orgId, p.policyId) IS UNIQUE",
    )
    .expect("parses") else {
        panic!()
    };
    assert_eq!(props, vec!["orgId", "policyId"]);
    assert_eq!(kind, ConstraintKind::Unique);
    let Stmt::Schema(SchemaCmd::CreateConstraint { kind, .. }) =
        parse_any("CREATE CONSTRAINT FOR (p:P) REQUIRE (p.a, p.b) IS NODE KEY").expect("parses")
    else {
        panic!()
    };
    assert_eq!(kind, ConstraintKind::NodeKey);
    // IS NOT NULL stays single-property.
    assert!(parse_any("CREATE CONSTRAINT FOR (p:P) REQUIRE (p.a, p.b) IS NOT NULL").is_err());
}

#[test]
fn relationship_indexes_show_and_scoped_call_parse() {
    let Stmt::Schema(SchemaCmd::CreateRangeIndex {
        label,
        on_relationships,
        props,
        ..
    }) = parse_any(
        "CREATE INDEX rel_idx IF NOT EXISTS FOR ()-[r:REGULATES_VIA]-() ON (r.relationId)",
    )
    .expect("parses")
    else {
        panic!()
    };
    assert_eq!(label, "REGULATES_VIA");
    assert!(on_relationships);
    assert_eq!(props, vec!["relationId"]);

    let Stmt::Schema(SchemaCmd::Show { subject, tail }) =
        parse_any("SHOW CONSTRAINTS YIELD name RETURN name").expect("parses")
    else {
        panic!()
    };
    assert_eq!(subject, "CONSTRAINTS");
    // The YIELD tail was swallowed unvalidated — the fact it EXISTED must be
    // recorded, or the executor would answer with the projection ignored.
    assert!(tail);

    let Stmt::Query(Query::Single(q)) =
        parse_any("MATCH (e:UserEmail) CALL (e) { RETURN e.subject AS s } RETURN s")
            .expect("parses")
    else {
        panic!()
    };
    let Clause::CallSubquery { imports, .. } = &q.clauses[1] else {
        panic!()
    };
    assert_eq!(imports, &vec!["e".to_string()]);
}
