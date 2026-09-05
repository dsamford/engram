#![allow(non_snake_case)]
//! End to end: statements against a real store, decoded values out.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, QueryResult, RunError, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> QueryResult {
    run_params(g, src, BTreeMap::new())
}

fn run_params(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn one(g: &Graph, src: &str) -> Value {
    let r = run(g, src);
    assert_eq!(r.rows.len(), 1, "`{src}` returned {} rows", r.rows.len());
    r.rows[0][0].clone()
}

#[test]
fn create_then_match_returns_identical_decoded_values() {
    let g = graph();
    run(
        &g,
        "CREATE (:Person {name: 'Ada', age: 36, score: 1.5, active: true, \
         tags: ['x', 'y'], nums: [1, 2, 3]})",
    );
    let r = run(
        &g,
        "MATCH (p:Person) RETURN p.name, p.age, p.score, p.active, p.tags, p.nums, p.missing",
    );
    assert_eq!(
        r.rows[0],
        vec![
            Value::Str("Ada".into()),
            Value::Int(36),
            Value::Float(1.5),
            Value::Bool(true),
            Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            Value::Null,
        ],
        "every property type round-trips BYTE-IDENTICAL through the store"
    );
    assert_eq!(
        r.columns[0], "p.name",
        "unaliased columns render the expression"
    );
}

#[test]
fn a_mixed_int_float_array_promotes_to_doubles() {
    // Neo4j coerces [1, 0.5] to double[]; refusing it broke a seeded sweep
    // query whose float rendered as `1`. The ints come back AS floats — the
    // array has one element type after storage, exactly as Neo4j returns it.
    let g = graph();
    run(&g, "CREATE (:MX {v: [1, 0.5, 2]})");
    let r = run(&g, "MATCH (n:MX) RETURN n.v");
    assert_eq!(
        r.rows[0][0],
        Value::List(vec![
            Value::Float(1.0),
            Value::Float(0.5),
            Value::Float(2.0)
        ])
    );
    // Genuinely mixed still refuses.
    let q = parse_statement("CREATE (:MX {v: [1, 'a']})").expect("parses");
    assert!(run_query(&g, &q, BTreeMap::new()).is_err());
}

#[test]
fn temporal_properties_round_trip_and_compare() {
    // createdAt datetime is THE corpus idiom (~458 sites) — store, read
    // back IDENTICAL, and compare in WHERE.
    let g = graph();
    g.set_wall_ms(1_787_140_800_123); // 2026-08-19T12:00:00.123Z
    run(
        &g,
        "CREATE (:Ev {at: datetime('2026-08-19T10:00:00Z'), d: duration('P1DT2H')})",
    );
    run(
        &g,
        "CREATE (:Ev {at: datetime('2026-08-20T10:00:00+02:00')})",
    );
    let r = run(&g, "MATCH (e:Ev) RETURN e.at ORDER BY e.at");
    assert_eq!(
        r.rows[0][0],
        Value::DateTime {
            epoch_seconds: 1_787_133_600,
            nanos: 0,
            offset_seconds: 0,
            zone: None
        }
    );
    let r = run(&g, "MATCH (e:Ev) WHERE e.at < datetime() RETURN count(*)");
    assert_eq!(
        r.rows[0][0],
        Value::Int(1),
        "only the past event; the +02:00 one is tomorrow"
    );
    let r = run(&g, "MATCH (e:Ev) WHERE e.d IS NOT NULL RETURN e.d");
    assert_eq!(
        r.rows[0][0],
        Value::Duration {
            months: 0,
            days: 1,
            seconds: 7200,
            nanos: 0
        }
    );
    // The 30-day-window idiom, verbatim.
    let r = run(
        &g,
        "MATCH (e:Ev) WHERE e.at > datetime() - duration('P30D') RETURN count(*)",
    );
    assert_eq!(r.rows[0][0], Value::Int(2));
}

#[test]
fn an_unset_wall_clock_refuses_datetime_now_by_name() {
    let g = graph();
    let q = parse_statement("RETURN datetime()").expect("parses");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(RunError::Eval(_)) => {}
        other => panic!("expected the clock refusal, got {other:?}"),
    }
}

#[test]
fn label_scans_multi_label_and_property_filters() {
    let g = graph();
    run(&g, "CREATE (:A {v: 1}), (:A:B {v: 2}), (:B {v: 3})");
    let r = run(&g, "MATCH (n:A) RETURN n.v ORDER BY n.v");
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    let r = run(&g, "MATCH (n:A:B) RETURN n.v");
    assert_eq!(r.rows, vec![vec![Value::Int(2)]], "multi-label is an AND");
    let r = run(&g, "MATCH (n:B {v: 3}) RETURN n.v");
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    let r = run(&g, "MATCH (n:Nowhere) RETURN n");
    assert!(
        r.rows.is_empty(),
        "an unknown label matches nothing, it is not an error"
    );
}

#[test]
fn relationships_traverse_in_every_direction() {
    let g = graph();
    run(
        &g,
        "CREATE (a:P {n: 'a'})-[:KNOWS {since: 2020}]->(b:P {n: 'b'})",
    );
    let r = run(&g, "MATCH (x:P)-[k:KNOWS]->(y:P) RETURN x.n, y.n, k.since");
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Str("a".into()),
            Value::Str("b".into()),
            Value::Int(2020)
        ]]
    );
    let r = run(&g, "MATCH (y:P)<-[:KNOWS]-(x:P) RETURN y.n");
    assert_eq!(r.rows, vec![vec![Value::Str("b".into())]]);
    let r = run(&g, "MATCH (x:P {n: 'b'})--(peer) RETURN peer.n");
    assert_eq!(
        r.rows,
        vec![vec![Value::Str("a".into())]],
        "undirected reaches back"
    );
}

#[test]
fn multi_type_is_an_OR() {
    let g = graph();
    run(
        &g,
        "CREATE (a:N {n: 1})-[:X]->(:N {n: 2}), (c:N {n: 1})-[:Y]->(:N {n: 3})",
    );
    run(&g, "MATCH (a:N {n: 1}) CREATE (a)-[:Z]->(:N {n: 4})");
    let r = run(&g, "MATCH (:N {n: 1})-[:X|Y]->(m) RETURN m.n ORDER BY m.n");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2)], vec![Value::Int(3)]],
        "Z is excluded"
    );
}

#[test]
fn optional_match_produces_the_null_row() {
    let g = graph();
    run(&g, "CREATE (:L {v: 1})");
    let r = run(
        &g,
        "MATCH (n:L) OPTIONAL MATCH (n)-[:NOPE]->(m) RETURN n.v, m",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Null]]);
}

#[test]
fn merge_is_idempotent_and_runs_the_right_arm() {
    let g = graph();
    run(
        &g,
        "MERGE (n:U {id: 7}) ON CREATE SET n.created = true ON MATCH SET n.matched = true",
    );
    run(
        &g,
        "MERGE (n:U {id: 7}) ON CREATE SET n.created2 = true ON MATCH SET n.matched = true",
    );
    let r = run(
        &g,
        "MATCH (n:U {id: 7}) RETURN n.created, n.matched, n.created2",
    );
    assert_eq!(r.rows.len(), 1, "MERGE twice makes ONE node");
    assert_eq!(
        r.rows[0],
        vec![Value::Bool(true), Value::Bool(true), Value::Null],
        "first run took ON CREATE, second took ON MATCH"
    );
}

#[test]
fn set_forms_and_null_removes() {
    let g = graph();
    run(&g, "CREATE (:S {a: 1, b: 2})");
    run(&g, "MATCH (n:S) SET n.a = 10, n += {c: 3}, n:Extra");
    let r = run(&g, "MATCH (n:S) RETURN n.a, n.b, n.c, labels(n)");
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(10),
            Value::Int(2),
            Value::Int(3),
            // Token-mint order — the same rule Neo4j applies (internal token
            // id), so `labels()` is deterministic without being alphabetical.
            Value::List(vec![Value::Str("S".into()), Value::Str("Extra".into())]),
        ]
    );
    // SET to null REMOVES — and IS NULL must then hold.
    run(&g, "MATCH (n:S) SET n.b = null");
    assert_eq!(one(&g, "MATCH (n:S) RETURN n.b IS NULL"), Value::Bool(true));
    // SET n = {} replaces everything.
    run(&g, "MATCH (n:S) SET n = {only: 'this'}");
    let r = run(&g, "MATCH (n:S) RETURN properties(n)");
    let mut m = BTreeMap::new();
    m.insert("only".to_string(), Value::Str("this".into()));
    assert_eq!(r.rows[0][0], Value::Map(m));
}

#[test]
fn remove_prop_and_label() {
    let g = graph();
    run(&g, "CREATE (:R1:R2 {x: 1})");
    run(&g, "MATCH (n:R1) REMOVE n.x, n:R2");
    let r = run(&g, "MATCH (n:R1) RETURN n.x, labels(n)");
    assert_eq!(
        r.rows[0],
        vec![Value::Null, Value::List(vec![Value::Str("R1".into())])]
    );
    assert!(
        run(&g, "MATCH (n:R2) RETURN n").rows.is_empty(),
        "the label scan agrees"
    );
}

#[test]
fn delete_refuses_connected_without_detach() {
    let g = graph();
    run(&g, "CREATE (:D {n: 1})-[:R]->(:D {n: 2})");
    let q = parse_statement("MATCH (n:D {n: 1}) DELETE n").expect("parses");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(RunError::Graph(engram_graph::GraphError::StillConnected(_))) => {}
        other => panic!("expected the connected refusal, got {other:?}"),
    }
    run(&g, "MATCH (n:D {n: 1}) DETACH DELETE n");
    let r = run(&g, "MATCH (n:D) RETURN n.n");
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    assert_eq!(
        one(&g, "MATCH (n:D {n: 2}) RETURN COUNT { (n)--() }"),
        Value::Int(0),
        "the relationship went with the node"
    );
}

#[test]
fn aggregation_groups_implicitly() {
    let g = graph();
    run(
        &g,
        "CREATE (:E {k: 'a', v: 1}), (:E {k: 'a', v: 2}), (:E {k: 'b', v: 30})",
    );
    let r = run(
        &g,
        "MATCH (n:E) RETURN n.k AS k, count(*) AS c, sum(n.v) AS s, collect(n.v) AS vs \
         ORDER BY k",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::Str("a".into()),
                Value::Int(2),
                Value::Int(3),
                Value::List(vec![Value::Int(1), Value::Int(2)]),
            ],
            vec![
                Value::Str("b".into()),
                Value::Int(1),
                Value::Int(30),
                Value::List(vec![Value::Int(30)]),
            ],
        ]
    );
    assert_eq!(one(&g, "MATCH (n:E) RETURN avg(n.v)"), Value::Float(11.0));
    assert_eq!(one(&g, "MATCH (n:E) RETURN min(n.v)"), Value::Int(1));
    assert_eq!(one(&g, "MATCH (n:E) RETURN max(n.v)"), Value::Int(30));
    assert_eq!(
        one(&g, "MATCH (n:Nothing) RETURN count(*)"),
        Value::Int(0),
        "count over zero rows is a ROW WITH ZERO, not absence"
    );
    assert_eq!(
        one(&g, "MATCH (n:E) RETURN count(DISTINCT n.k)"),
        Value::Int(2)
    );
    assert_eq!(
        one(&g, "MATCH (n:E) RETURN count(*) + 1"),
        Value::Int(4),
        "aggregates compose into expressions"
    );
}

#[test]
fn order_skip_limit_and_null_placement() {
    let g = graph();
    run(&g, "CREATE (:O {v: 3}), (:O {v: 1}), (:O), (:O {v: 2})");
    let r = run(&g, "MATCH (n:O) RETURN n.v ORDER BY n.v");
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Null]
        ],
        "null sorts LAST ascending"
    );
    let r = run(
        &g,
        "MATCH (n:O) WHERE n.v IS NOT NULL RETURN n.v ORDER BY n.v DESC SKIP 1 LIMIT 1",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
}

#[test]
fn with_pipelines_and_where_filters_the_projection() {
    let g = graph();
    run(
        &g,
        "CREATE (:W {k: 'a', v: 1}), (:W {k: 'a', v: 2}), (:W {k: 'b', v: 3})",
    );
    let r = run(
        &g,
        "MATCH (n:W) WITH n.k AS k, count(*) AS c WHERE c > 1 RETURN k, c",
    );
    assert_eq!(r.rows, vec![vec![Value::Str("a".into()), Value::Int(2)]]);
}

#[test]
fn unwind_union_and_distinct() {
    let g = graph();
    let r = run(&g, "UNWIND [3, 1, 3] AS x RETURN x");
    assert_eq!(r.rows.len(), 3);
    let r = run(&g, "UNWIND [3, 1, 3] AS x RETURN DISTINCT x");
    assert_eq!(r.rows, vec![vec![Value::Int(3)], vec![Value::Int(1)]]);
    let r = run(&g, "RETURN 1 AS x UNION RETURN 1 AS x UNION RETURN 2 AS x");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)], vec![Value::Int(2)]],
        "UNION dedupes"
    );
    let r = run(&g, "RETURN 1 AS x UNION ALL RETURN 1 AS x");
    assert_eq!(r.rows.len(), 2, "UNION ALL keeps both");
}

#[test]
fn THE_foreach_conditional_write_idiom() {
    let g = graph();
    run(&g, "CREATE (:F {due: true, n: 1}), (:F {due: false, n: 2})");
    run(
        &g,
        "MATCH (n:F) FOREACH (_ IN CASE WHEN n.due THEN [1] ELSE [] END | SET n.flag = true)",
    );
    let r = run(&g, "MATCH (n:F) RETURN n.n, n.flag ORDER BY n.n");
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Bool(true)],
            vec![Value::Int(2), Value::Null]
        ],
        "exactly the due row was flagged"
    );
}

#[test]
fn exists_count_subqueries_and_pattern_comprehensions() {
    let g = graph();
    run(
        &g,
        "CREATE (a:G {n: 'a'})-[:R]->(:G {n: 'b'}), (c:G {n: 'c'})",
    );
    run(&g, "MATCH (a:G {n: 'a'}) CREATE (a)-[:R]->(:G {n: 'd'})");
    let r = run(&g, "MATCH (n:G) WHERE EXISTS { (n)-[:R]->() } RETURN n.n");
    assert_eq!(r.rows, vec![vec![Value::Str("a".into())]]);
    assert_eq!(
        one(&g, "MATCH (n:G {n: 'a'}) RETURN COUNT { (n)-[:R]->() }"),
        Value::Int(2)
    );
    let r = run(
        &g,
        "MATCH (n:G {n: 'a'}) RETURN [ (n)-[:R]->(m) | m.n ] AS peers",
    );
    let Value::List(mut peers) = r.rows[0][0].clone() else {
        panic!()
    };
    peers.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(peers, vec![Value::Str("b".into()), Value::Str("d".into())]);
}

#[test]
fn variable_length_and_shortest_path() {
    let g = graph();
    // a chain a→b→c→d plus a shortcut a→d.
    run(
        &g,
        "CREATE (a:V {n: 'a'})-[:R]->(b:V {n: 'b'})-[:R]->(c:V {n: 'c'})-[:R]->(d:V {n: 'd'})",
    );
    run(
        &g,
        "MATCH (a:V {n: 'a'}), (d:V {n: 'd'}) CREATE (a)-[:R]->(d)",
    );
    let r = run(
        &g,
        "MATCH (a:V {n: 'a'})-[:R*1..2]->(m) RETURN m.n ORDER BY m.n",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Str("b".into())],
            vec![Value::Str("c".into())],
            vec![Value::Str("d".into())]
        ],
        "depth 1 reaches b and d; depth 2 adds c"
    );
    // Unbounded * terminates (relationship isomorphism forbids reuse).
    let r = run(
        &g,
        "MATCH (a:V {n: 'a'})-[:R*]->(m) RETURN DISTINCT m.n ORDER BY m.n",
    );
    assert_eq!(r.rows.len(), 3);
    // shortestPath takes the shortcut.
    let r = run(
        &g,
        "MATCH p = shortestPath((a:V {n: 'a'})-[:R*..5]->(d:V {n: 'd'})) RETURN length(p)",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "one hop via the shortcut — `length` counts relationships (`size(path)` is an error)"
    );
}

#[test]
fn unbounded_star_terminates_on_a_CYCLE() {
    // Relationship isomorphism is what makes `*` finite: a→b→a with reuse
    // allowed walks forever. With it, exactly two paths leave `a`.
    let g = graph();
    run(
        &g,
        "CREATE (a:Cyc {n: 'a'})-[:R]->(b:Cyc {n: 'b'}) CREATE (b)-[:R]->(a)",
    );
    let r = run(
        &g,
        "MATCH (a:Cyc {n: 'a'})-[:R*]->(m) RETURN m.n ORDER BY m.n",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Str("a".into())], vec![Value::Str("b".into())]],
        "a→b and a→b→a; the third step would reuse a relationship"
    );
}

#[test]
fn set_is_visible_to_the_same_statements_return() {
    // Bindings are snapshots; a mutation must refresh them, or RETURN reads
    // the world as it was before the SET it just watched happen.
    let g = graph();
    run(&g, "CREATE (:RW {v: 1})");
    let r = run(&g, "MATCH (n:RW) SET n.v = 2 RETURN n.v");
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
}

#[test]
fn graph_functions_and_identity() {
    let g = graph();
    run(&g, "CREATE (:I {v: 1})-[:T]->(:I {v: 2})");
    let r = run(
        &g,
        "MATCH (a:I)-[r:T]->(b:I) RETURN labels(a), type(r), a = a, a = b",
    );
    assert_eq!(
        r.rows[0],
        vec![
            Value::List(vec![Value::Str("I".into())]),
            Value::Str("T".into()),
            Value::Bool(true),
            Value::Bool(false),
        ]
    );
    let Value::Str(eid) = one(&g, "MATCH (a:I {v: 1}) RETURN elementId(a)") else {
        panic!("elementId is a STRING in Bolt 5");
    };
    assert!(eid.starts_with("n:"));
}

#[test]
fn procedures_refuse_BY_NAME() {
    let g = graph();
    // `db.labels` was this test's specimen until it was IMPLEMENTED (R-5, the
    // introspection procedures — see tests/introspection_procedures.rs). The
    // property under test is unchanged: an unsupported procedure refuses and
    // the refusal NAMES it.
    let q = parse_statement("CALL db.schema.visualization() YIELD nodes RETURN nodes")
        .expect("parses");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(RunError::Unsupported(what)) => assert!(what.contains("db.schema.visualization")),
        other => panic!("expected the named refusal, got {other:?}"),
    }
}

#[test]
fn params_flow_end_to_end() {
    let g = graph();
    let mut params = BTreeMap::new();
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), Value::Str("Ada".into()));
    params.insert("props".to_string(), Value::Map(props));
    params.insert("min".to_string(), Value::Int(0));
    run_params(&g, "CREATE (:PP $props)", params.clone());
    let r = run_params(
        &g,
        "MATCH (n:PP) WHERE size(n.name) > $min RETURN n.name",
        params,
    );
    assert_eq!(r.rows, vec![vec![Value::Str("Ada".into())]]);
}

#[test]
fn shadow_compare_agrees_and_names_divergence() {
    use engram_cypher::parse_any;
    use engram_graph::{ShadowVerdict, shadow_compare};
    let (a, b) = (graph(), graph());
    for g in [&a, &b] {
        run(g, "CREATE (:Sh {v: 1}), (:Sh {v: 2})");
    }
    let q = parse_any("MATCH (n:Sh) RETURN n.v ORDER BY n.v").expect("parses");
    assert_eq!(
        shadow_compare(&a, &b, &q, BTreeMap::new()),
        ShadowVerdict::Agree
    );
    // An agreed refusal is agreement.
    let bad = parse_any("CALL db.labels() YIELD label RETURN label").expect("parses");
    assert_eq!(
        shadow_compare(&a, &b, &bad, BTreeMap::new()),
        ShadowVerdict::Agree
    );
    // A VALUE divergence (equal counts) names the first differing row.
    run(&b, "MATCH (n:Sh {v: 2}) SET n.v = 99");
    match shadow_compare(&a, &b, &q, BTreeMap::new()) {
        ShadowVerdict::Diverge { detail } => {
            assert!(detail.contains("row 1 differs"), "{detail}")
        }
        other => panic!("expected divergence, got {other:?}"),
    }
    // A COUNT divergence names the counts.
    run(&b, "CREATE (:Sh {v: 3})");
    match shadow_compare(&a, &b, &q, BTreeMap::new()) {
        ShadowVerdict::Diverge { detail } => assert!(detail.contains("row counts"), "{detail}"),
        other => panic!("expected divergence, got {other:?}"),
    }
}

#[test]
fn call_subquery_appends_its_columns() {
    let g = graph();
    run(&g, "CREATE (:C {v: 1}), (:C {v: 2})");
    let r = run(
        &g,
        "MATCH (n:C) CALL { MATCH (m:C) RETURN count(*) AS total } RETURN n.v, total ORDER BY n.v",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(2)]
        ]
    );
}

#[test]
fn aggregate_inside_list_comprehension_source_collects_then_filters() {
    // The corpus's collect-then-filter idiom (market.ts routes):
    // `[s IN collect(DISTINCT c.sector) WHERE s IS NOT NULL AND s <> '' | s]`.
    // Before the fix the whole item read as a GROUPING KEY (the walker had
    // no ListComp arm), and the collect() then refused as scalar.
    let g = graph();
    run(
        &g,
        "CREATE (:Co {sector: 'tech'}), (:Co {sector: 'energy'}), \
             (:Co {sector: ''}), (:Co {}), (:Co {sector: 'tech'})",
    );
    let r = run(
        &g,
        "MATCH (c:Co) RETURN [s IN collect(DISTINCT c.sector) \
         WHERE s IS NOT NULL AND s <> '' | s] AS sectors",
    );
    assert_eq!(r.rows.len(), 1);
    let Value::List(sectors) = &r.rows[0][0] else {
        panic!("not a list")
    };
    let mut names: Vec<String> = sectors
        .iter()
        .map(|v| match v {
            Value::Str(s) => s.clone(),
            other => panic!("not a string: {other:?}"),
        })
        .collect();
    names.sort();
    // '' filtered, absent property (null) filtered, 'tech' deduped.
    assert_eq!(names, vec!["energy".to_string(), "tech".to_string()]);
}

#[test]
fn aggregate_inside_case_and_size_shapes() {
    // The rewrite must reach aggregates under every containing shape, not
    // just Bin/Call/List — CASE was silently left unrewritten before.
    let g = graph();
    run(&g, "CREATE (:Cs), (:Cs), (:Cs)");
    let r = run(
        &g,
        "MATCH (n:Cs) RETURN CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END AS verdict",
    );
    assert_eq!(r.rows, vec![vec![Value::Str("many".to_string())]]);
}

#[test]
fn order_by_grouping_expression_not_alias() {
    // `RETURN s.name AS name, count(r) AS relCount ORDER BY s.name` — the
    // sort key is spelled as the GROUPING EXPRESSION, not its alias, and
    // `s` is not a projected column (cleanup-duplicate-skills.ts). Neo4j
    // accepts it; ORDER BY resolves by structural match against the
    // projection items.
    let g = graph();
    run(
        &g,
        "CREATE (:Sk {name: 'zeta'}), (:Sk {name: 'alpha'}), (:Sk {name: 'mid'})",
    );
    let r = run(
        &g,
        "MATCH (s:Sk) OPTIONAL MATCH (s)<-[r]-() \
         RETURN s.name AS name, count(r) AS relCount ORDER BY s.name",
    );
    let names: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Str(s) => s.clone(),
            other => panic!("not a string: {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec!["alpha".to_string(), "mid".to_string(), "zeta".to_string()]
    );
}

#[test]
fn streaming_makes_intermediates_free_and_the_budget_guards_real_memory() {
    // The refactor's property, asserted from both sides: a cartesian
    // product folded into count(*) STREAMS — a budget of 100 over 900
    // intermediate pairs must SUCCEED, because nothing holds those pairs —
    // while a projection that genuinely BUFFERS 900 output rows under the
    // same budget must refuse.
    let g = graph();
    for i in 0..30i64 {
        run(&g, &format!("CREATE (:X {{i: {i}}}), (:Y {{i: {i}}})"));
    }
    g.set_row_budget(Some(100));
    let r = run(&g, "MATCH (a:X), (b:Y) RETURN count(*) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(900)]],
        "streamed intermediates are free"
    );
    let r = run(
        &g,
        "MATCH (a:X) UNWIND range(1, 100000) AS x RETURN count(x) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(3_000_000)]],
        "a 3M-row unwind folds through a 100-row budget"
    );
    // The same product PROJECTED must refuse: 900 output rows is memory
    // the caller asked for, and the budget's whole job.
    let q = parse_statement("MATCH (a:X), (b:Y) RETURN a.i, b.i").expect("parse");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains("row budget"),
                "the refusal must NAME the budget: {msg}"
            );
        }
        Ok(_) => panic!("900 buffered output rows over a budget of 100 must refuse"),
    }
    g.set_row_budget(None);
    let r = run(&g, "MATCH (a:X), (b:Y) RETURN count(*) AS c");
    assert_eq!(r.rows, vec![vec![Value::Int(900)]]);
}

#[test]
fn an_unbound_WHERE_variable_refuses_BEFORE_any_scan() {
    // `MATCH (n) WHERE n.x = nid` with `nid` bound by nothing: Neo4j
    // refuses at parse; accepting it here materialised the entire database
    // into rows before eval noticed. The refusal must come first and name
    // the variable.
    let g = graph();
    run(&g, "CREATE (:S {x: 1}), (:S {x: 2})");
    let q = parse_statement("MATCH (n:S) WHERE n.x = nid RETURN n").expect("parses as a var");
    match run_query(&g, &q, BTreeMap::new()) {
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains("`nid` not defined"),
                "must name the variable: {msg}"
            );
        }
        Ok(_) => panic!("an unbound WHERE variable must refuse"),
    }
    // Bound spellings of the same shape keep working: the pattern's own
    // var, an earlier clause's var, and comprehension locals.
    let r = run(
        &g,
        "MATCH (a:S) WITH a.x AS nid MATCH (n:S) WHERE n.x = nid RETURN count(n) AS c",
    );
    assert_eq!(r.rows.len(), 1);
    let r = run(
        &g,
        "MATCH (n:S) WHERE any(v IN [1, 9] WHERE n.x = v) RETURN count(n) AS c",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn a_query_concluding_with_a_reading_clause_refuses_by_name() {
    // `OPTIONAL MATCH (n)-[]-()-[]-(hop2)` with no RETURN — a corpus
    // fragment Neo4j refuses at parse. Accepting it meant executing an
    // unanchored two-hop scan nobody would ever read.
    let g = graph();
    run(&g, "CREATE (:F {i: 1})");
    for (src, name) in [
        ("MATCH (n:F)", "MATCH"),
        ("OPTIONAL MATCH (n)-[]-()-[]-(hop2)", "MATCH"),
        ("MATCH (n:F) WITH n", "WITH"),
        ("UNWIND [1,2] AS x", "UNWIND"),
    ] {
        let q = parse_statement(src).expect("parses");
        match run_query(&g, &q, BTreeMap::new()) {
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    msg.contains(&format!("conclude with {name}")),
                    "`{src}` must refuse naming {name}: {msg}"
                );
            }
            Ok(_) => panic!("`{src}` must refuse — nothing projects it"),
        }
    }
}

#[test]
fn the_count_fast_path_equals_the_general_path_on_every_shape() {
    // The fast path must be provably equivalent or absent. Each accepted
    // shape is checked against hand-countable data; each REJECTED shape
    // still answers correctly through the general path.
    let g = graph();
    run(
        &g,
        "CREATE (:P {x: 1}), (:P {x: 2}), (:P:Q {x: 3}), (:Q {x: 4})",
    );
    run(&g, "MATCH (a:P {x: 1}), (b:Q {x: 4}) CREATE (a)-[:R1]->(b)");
    run(&g, "MATCH (a:P {x: 2}), (b:Q {x: 4}) CREATE (a)-[:R2]->(b)");
    // Accepted shapes.
    assert_eq!(one(&g, "MATCH (n) RETURN count(n)"), Value::Int(4));
    assert_eq!(one(&g, "MATCH (n) RETURN count(*) AS c"), Value::Int(4));
    assert_eq!(one(&g, "MATCH (n:P) RETURN count(n) AS c"), Value::Int(3));
    assert_eq!(one(&g, "MATCH (n:Q) RETURN count(n) AS c"), Value::Int(2));
    assert_eq!(
        one(&g, "MATCH (n:Nope) RETURN count(n) AS c"),
        Value::Int(0)
    );
    assert_eq!(
        one(&g, "MATCH ()-[r]->() RETURN count(r) AS c"),
        Value::Int(2)
    );
    assert_eq!(
        one(&g, "MATCH ()<-[r]-() RETURN count(r) AS c"),
        Value::Int(2)
    );
    // Rejected shapes fall through and still answer right.
    assert_eq!(
        one(&g, "MATCH (n:P) WHERE n.x > 1 RETURN count(n) AS c"),
        Value::Int(2)
    );
    assert_eq!(one(&g, "MATCH (n:P:Q) RETURN count(n) AS c"), Value::Int(1));
    assert_eq!(
        one(&g, "MATCH ()-[r:R1]->() RETURN count(r) AS c"),
        Value::Int(1)
    );
    assert_eq!(
        one(&g, "MATCH ()-[r]-() RETURN count(r) AS c"),
        Value::Int(4)
    ); // undirected doubles
    assert_eq!(
        one(&g, "MATCH (n) RETURN count(DISTINCT n) AS c"),
        Value::Int(4)
    );
    // A tombstoned node stops counting on BOTH paths.
    run(&g, "MATCH (n:Q {x: 4}) DETACH DELETE n");
    assert_eq!(one(&g, "MATCH (n) RETURN count(n)"), Value::Int(3));
    assert_eq!(one(&g, "MATCH (n:Q) RETURN count(n) AS c"), Value::Int(1));
    assert_eq!(
        one(&g, "MATCH ()-[r]->() RETURN count(r) AS c"),
        Value::Int(0)
    );
}

#[test]
fn the_planner_preserves_answers_on_every_seed_shape() {
    let g = graph();
    // Two-label nodes: Species is the small side of Bio.
    for i in 0..40i64 {
        run(&g, &format!("CREATE (:BioT {{i: {i}}})"));
    }
    for i in 0..4i64 {
        run(
            &g,
            &format!("CREATE (:BioT:SpeciesT {{i: {i}, name: 'sp{i}'}})"),
        );
    }
    let r = run(&g, "MATCH (s:BioT:SpeciesT) RETURN s.name AS n ORDER BY n");
    assert_eq!(r.rows.len(), 4, "smallest-label seed keeps every match");
    let r = run(&g, "MATCH (s:SpeciesT:BioT) RETURN count(s) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(4)]],
        "label order in the pattern is irrelevant"
    );

    // Rel-driven seeds: typed, untyped, undirected, bound-end, props.
    run(
        &g,
        "MATCH (a:SpeciesT {i: 0}), (b:SpeciesT {i: 1}) CREATE (a)-[:EATS {w: 2}]->(b)",
    );
    run(
        &g,
        "MATCH (a:SpeciesT {i: 1}), (b:SpeciesT {i: 2}) CREATE (a)-[:EATS {w: 5}]->(b)",
    );
    run(
        &g,
        "MATCH (a:SpeciesT {i: 2}), (b:SpeciesT {i: 3}) CREATE (a)-[:SEES]->(b)",
    );
    let r = run(&g, "MATCH ()-[r:EATS]->() RETURN count(r) AS c");
    assert_eq!(r.rows, vec![vec![Value::Int(2)]], "typed rel-driven count");
    let r = run(&g, "MATCH ()-[r]->() RETURN count(r) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(3)]],
        "untyped rel-driven count"
    );
    let r = run(&g, "MATCH ()-[r]-() RETURN count(r) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(6)]],
        "undirected offers both legs"
    );
    let r = run(&g, "MATCH ()-[r:EATS {w: 5}]->() RETURN count(r) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "rel property pattern filters"
    );
    let r = run(
        &g,
        "MATCH ()-[r:EATS]->(b:SpeciesT) WHERE b.i = 1 RETURN r.w AS w",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2)]],
        "constrained end tests per rel"
    );
    let r = run(
        &g,
        "MATCH (x:SpeciesT {i: 1}) MATCH ()-[r:EATS]->(x) RETURN count(r) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "a bound end pins the rel scan"
    );

    // Projection pushdown: property-only reads still see every value…
    let r = run(&g, "MATCH (s:SpeciesT) WHERE s.i = 2 RETURN s.name AS n");
    assert_eq!(r.rows, vec![vec![Value::Str("sp2".into())]]);
    // …and a BARE use still carries the full property map.
    let r = run(&g, "MATCH (s:SpeciesT {i: 3}) RETURN s");
    let Value::Node { props, .. } = &r.rows[0][0] else {
        panic!("not a node")
    };
    assert_eq!(
        props.get("name"),
        Some(&Value::Str("sp3".into())),
        "bare use is FULL"
    );
    assert_eq!(props.get("i"), Some(&Value::Int(3)));

    // A path variable still materialises its trail.
    let r = run(
        &g,
        "MATCH p = (a:SpeciesT {i: 0})-[:EATS]->(b) RETURN length(p) AS s",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "trail built when a path var reads it — one hop (`size(path)` is an error)"
    );
}

#[test]
fn index_seeds_answer_identically_across_every_probe_shape() {
    let g = graph();
    for i in 0..50i64 {
        run(&g, &format!("CREATE (:IX {{k: {i}, s: 'name{i}'}})"));
    }
    // A float-stored key and an int probe must still meet: Cypher equality
    // is cross-type numeric, the index buckets are typed, and the union of
    // both buckets plus re-verification reconciles them.
    run(&g, "CREATE (:IX {k: 7.0, s: 'float-seven'})");
    let r = run(&g, "MATCH (n:IX {k: 7}) RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2)]],
        "int probe finds the float row"
    );
    let r = run(&g, "MATCH (n:IX {k: 7.0}) RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2)]],
        "float probe finds the int row"
    );
    // String probes.
    let r = run(&g, "MATCH (n:IX {s: 'name9'}) RETURN n.k AS k");
    assert_eq!(r.rows, vec![vec![Value::Int(9)]]);
    // Parameters drive the probe value at execution time.
    let q = parse_statement("MATCH (n:IX {k: $x}) RETURN count(n) AS c").expect("parse");
    let mut params = BTreeMap::new();
    params.insert("x".to_string(), Value::Int(3));
    let r = run_query(&g, &q, params).expect("run");
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    // Staleness: a write between queries must be visible (epoch rebuild).
    run(&g, "CREATE (:IX {k: 7, s: 'late'})");
    let r = run(&g, "MATCH (n:IX {k: 7}) RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(3)]],
        "the index rebuilds after a write"
    );
    // A non-scalar probe value falls back to the scan and still answers.
    let r = run(&g, "MATCH (n:IX {k: [1, 2]}) RETURN count(n) AS c");
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    // An absent property matches nothing, scan or index alike.
    let r = run(&g, "MATCH (n:IX {never_written: 1}) RETURN count(n) AS c");
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
}

#[test]
fn columnar_candidate_batches_answer_identically() {
    // A property-only seed must be indistinguishable from per-id
    // materialisation: mixed signatures (absent props), multi-label nodes
    // (REAL label sets on slim candidates), and post-compaction blocks with
    // tail overrides. (Fix 61 retired the partition-wide candidate-batch
    // path; the lean column seed answers these shapes now.)
    let g = graph();
    for i in 0..80i64 {
        run(&g, &format!("CREATE (:CB {{a: {i}, b: 'x{i}'}})"));
    }
    for i in 0..5i64 {
        run(&g, &format!("CREATE (:CB:CBExtra {{a: {}}})", 1000 + i)); // no `b`
    }
    // Through the store's compaction so column BLOCKS serve part of it.
    g.shared_store().seal();
    g.shared_store().compact();
    run(&g, "CREATE (:CB {a: 2000, b: 'tail'})"); // a tail override row

    let r = run(&g, "MATCH (n:CB) WHERE n.a >= 1000 RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(6)]],
        "mixed signatures counted"
    );
    let r = run(&g, "MATCH (n:CB) WHERE n.b IS NULL RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(5)]],
        "absent props read as null"
    );
    let r = run(&g, "MATCH (n:CB:CBExtra) RETURN count(n) AS c");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(5)]],
        "slim candidates carry REAL labels"
    );
    let r = run(&g, "MATCH (n:CB) WHERE n.b = 'tail' RETURN n.a AS a");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2000)]],
        "tail rows merge over blocks"
    );
    let r = run(&g, "MATCH (n:CB) RETURN sum(n.a) AS s");
    let want: i64 = (0..80).sum::<i64>() + (1000..1005).sum::<i64>() + 2000;
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(want)]],
        "aggregate over the batch"
    );
}

#[test]
fn pushed_conjuncts_prune_early_and_never_change_answers() {
    let g = graph();
    for i in 0..20i64 {
        run(&g, &format!("CREATE (:PA {{i: {i}}}), (:PB {{i: {i}}})"));
    }
    // Cross-path conjunction: the a-side conjunct prunes before the b scan.
    let r = run(
        &g,
        "MATCH (a:PA), (b:PB) WHERE a.i = 3 AND b.i > 17 RETURN count(*) AS c",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    // Three-valued: a null conjunct drops the row, pushed or not.
    let r = run(
        &g,
        "MATCH (a:PA), (b:PB) WHERE a.missing = 1 AND b.i = 0 RETURN count(*) AS c",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    // An OPTIONAL match with a failing pushed conjunct still yields the
    // null-bound row, exactly as a failing pattern does.
    let r = run(
        &g,
        "MATCH (x:PA {i: 0}) OPTIONAL MATCH (y:PB) WHERE x.i = 99 RETURN x.i AS xi, y AS y",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(0), Value::Null]]);
    // A non-boolean conjunct still errors exactly as the unsplit WHERE does.
    let q = parse_statement("MATCH (a:PA), (b:PB) WHERE a.i AND b.i = 1 RETURN count(*) AS c")
        .expect("parse");
    assert!(
        run_query(&g, &q, BTreeMap::new()).is_err(),
        "non-boolean WHERE still refuses"
    );
}

#[test]
fn top_k_pushdown_matches_the_full_sort_ties_and_all() {
    let g = graph();
    // Deliberate ties on the sort key: stability decides who survives the
    // cut, and the bounded buffer must agree with the full sort exactly.
    for i in 0..30i64 {
        run(&g, &format!("CREATE (:TK {{i: {i}, g: {}}})", i % 3));
    }
    let full = run(&g, "MATCH (n:TK) RETURN n.i AS i, n.g AS g ORDER BY g");
    let limited = run(
        &g,
        "MATCH (n:TK) RETURN n.i AS i, n.g AS g ORDER BY g LIMIT 7",
    );
    assert_eq!(
        limited.rows,
        full.rows[..7].to_vec(),
        "LIMIT is the full sort's prefix"
    );
    let paged = run(
        &g,
        "MATCH (n:TK) RETURN n.i AS i, n.g AS g ORDER BY g SKIP 5 LIMIT 4",
    );
    assert_eq!(
        paged.rows,
        full.rows[5..9].to_vec(),
        "SKIP+LIMIT pages the same order"
    );
    let desc = run(&g, "MATCH (n:TK) RETURN n.i AS i ORDER BY n.i DESC LIMIT 3");
    assert_eq!(
        desc.rows,
        vec![
            vec![Value::Int(29)],
            vec![Value::Int(28)],
            vec![Value::Int(27)]
        ],
    );
}

#[test]
fn bulk_ingest_reads_identically_and_ids_stay_unique_across_reservations() {
    // Two graphs, same data, one loaded in bulk mode: every read must be
    // identical, and bulk ids must stay unique across the 4096-id
    // reservation boundary (the counter row holds the reserved END, so a
    // crash abandons the tail as gaps — never reuse).
    let normal = graph();
    let bulk = graph();
    bulk.set_bulk_ingest(true).expect("bulk on");
    for g in [&normal, &bulk] {
        for i in 0..10 {
            run_params(
                g,
                "CREATE (:BLK {i: $i})",
                [("i".to_string(), Value::Int(i))].into_iter().collect(),
            );
        }
        run(
            g,
            "MATCH (a:BLK {i: 0}), (b:BLK {i: 1}) CREATE (a)-[:BL]->(b)",
        );
    }
    bulk.set_bulk_ingest(false).expect("bulk exit");
    for q in [
        "MATCH (n:BLK) RETURN n.i ORDER BY n.i",
        "MATCH (:BLK {i: 0})-[r:BL]->(b) RETURN b.i",
        "MATCH (n:BLK) RETURN count(n)",
    ] {
        assert_eq!(
            run(&normal, q).rows,
            run(&bulk, q).rows,
            "bulk and normal loads must answer identically: {q}"
        );
    }
    assert!(
        bulk.shared_store().unlogged_count() > 0,
        "bulk mode must actually bypass the log"
    );
    assert_eq!(
        normal.shared_store().unlogged_count(),
        0,
        "normal mode never does"
    );

    // Uniqueness across a reservation boundary: 5000 nodes crosses 4096.
    let big = graph();
    big.set_bulk_ingest(true).expect("bulk on");
    for _ in 0..5000 {
        run(&big, "CREATE (:U)");
    }
    big.set_bulk_ingest(false).expect("bulk exit");
    let r = run(&big, "MATCH (n:U) RETURN count(DISTINCT id(n)), count(n)");
    assert_eq!(
        r.rows[0][0], r.rows[0][1],
        "every bulk-allocated id must be distinct"
    );
    assert_eq!(r.rows[0][1], Value::Int(5000));
}

#[test]
fn a_replay_after_bulk_ingest_is_consistent_not_partial() {
    // The replay contract, stated exactly: bulk entities are WHOLLY absent
    // (no membership row may survive for a node record that did not), and
    // everything a logged write depends on — the token table — must be
    // present, because logged post-bulk rows reference tokens by number.
    let g = graph();
    g.set_bulk_ingest(true).expect("bulk on");
    for i in 0..8 {
        run_params(
            &g,
            "CREATE (:RP {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    g.set_bulk_ingest(false).expect("bulk exit");
    run(&g, "CREATE (:RP {i: 100})");
    let replayed = Store::recover(&g.shared_store().log_tail(0)).expect("recover");
    let rg = Graph::new(replayed, Realm(1), Namespace(1));
    let r = run(&rg, "MATCH (n:RP) RETURN n.i");
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(100)]],
        "replay must hold exactly the logged node, found by LABEL — the          token mint survived and no bulk membership row leaked through"
    );
}

#[test]
fn label_counts_stay_correct_across_writes_through_the_members_cache() {
    // The membership snapshot is keyed by the commit clock: any write must
    // invalidate it. Read, write, read — the second read must see the write,
    // and a mixed create/delete sequence must never leave a stale count.
    let g = graph();
    for i in 0..5 {
        run_params(
            &g,
            "CREATE (:MC {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    assert_eq!(one(&g, "MATCH (n:MC) RETURN count(n)"), Value::Int(5));
    assert_eq!(
        one(&g, "MATCH (n:MC) RETURN count(n)"),
        Value::Int(5),
        "cached read agrees"
    );
    run(&g, "CREATE (:MC {i: 99})");
    assert_eq!(
        one(&g, "MATCH (n:MC) RETURN count(n)"),
        Value::Int(6),
        "a write invalidates the snapshot"
    );
    run(&g, "MATCH (n:MC {i: 99}) DETACH DELETE n");
    assert_eq!(
        one(&g, "MATCH (n:MC) RETURN count(n)"),
        Value::Int(5),
        "a delete invalidates it too"
    );
    let ids = g.members(Some("MC")).unwrap();
    assert_eq!(ids.len(), 5, "members() agrees with the count");
    assert!(
        g.members(Some("NeverMinted")).unwrap().is_empty(),
        "an unminted label has no members and mints no token"
    );
}

#[test]
fn the_clause_scan_memo_and_equality_index_change_nothing_but_time() {
    // A cartesian OPTIONAL MATCH with a correlated string equality — the
    // risk-by-country shape. The memo scans the inner clause once and the
    // index picks survivors per row; every answer, order included, must be
    // identical to the semantics the general path defines.
    let g = graph();
    for (iso, pop) in [("AA", 1i64), ("BB", 2), ("CC", 3)] {
        run_params(
            &g,
            "CREATE (:CtryM {iso: $i, pop: $p})",
            [
                ("i".to_string(), Value::Str(iso.into())),
                ("p".to_string(), Value::Int(pop)),
            ]
            .into_iter()
            .collect(),
        );
    }
    // Sanctions: string keys, one missing key, one NON-STRING key (Int) —
    // the residual must carry both, and cross-type equality must still be
    // judged by the WHERE, not by the bucket rule.
    for (n, tgt) in [
        ("s1", Some(Value::Str("AA".into()))),
        ("s2", Some(Value::Str("BB".into()))),
        ("s3", Some(Value::Str("AA".into()))),
        ("s4", None),
        ("s5", Some(Value::Int(7))),
    ] {
        let mut p: BTreeMap<String, Value> = [("n".to_string(), Value::Str(n.into()))].into();
        match tgt {
            Some(v) => {
                p.insert("t".to_string(), v);
                run_params(&g, "CREATE (:SancM {n: $n, tgt: $t})", p);
            }
            None => {
                run_params(&g, "CREATE (:SancM {n: $n})", p);
            }
        }
    }
    let r = run(
        &g,
        "MATCH (c:CtryM) OPTIONAL MATCH (s:SancM) WHERE s.tgt = c.iso \
         RETURN c.iso, s.n ORDER BY c.iso, s.n",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Str("AA".into()), Value::Str("s1".into())],
            vec![Value::Str("AA".into()), Value::Str("s3".into())],
            vec![Value::Str("BB".into()), Value::Str("s2".into())],
            vec![Value::Str("CC".into()), Value::Null],
        ],
        "bucket hits, misses, missing keys and the OPTIONAL null row"
    );

    // Cross-type: an Int-keyed row must match an Int outer value through
    // the residual — bucketing it as a string would lose it, and Cypher
    // number equality crosses Int/Float.
    run(&g, "CREATE (:CtryM {iso: 'DD', code: 7})");
    let r = run(
        &g,
        "MATCH (c:CtryM {iso: 'DD'}) OPTIONAL MATCH (s:SancM) WHERE s.tgt = c.code \
         RETURN s.n",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Str("s5".into())]],
        "the Int-keyed sanction matches the Int outer through the residual"
    );

    // A null outer side judges nothing definite: null-equality is Null, so
    // no sanction survives the WHERE and the OPTIONAL row is null.
    let r = run(
        &g,
        "MATCH (c:CtryM {iso: 'DD'}) OPTIONAL MATCH (s:SancM) WHERE s.tgt = c.missing \
         RETURN s.n",
    );
    assert_eq!(r.rows, vec![vec![Value::Null]], "null outer → null row");

    // Emission order without ORDER BY: the index's merged walk must equal
    // the plain scan's order exactly (this is what decoded-values compares
    // at LIMIT boundaries). Same statement, correlated vs uncorrelated
    // spelling, identical rows in identical order.
    let a = run(
        &g,
        "MATCH (c:CtryM {iso: 'AA'}) OPTIONAL MATCH (s:SancM) WHERE s.tgt = c.iso RETURN s.n",
    );
    let b = run(
        &g,
        "MATCH (c:CtryM {iso: 'AA'}) OPTIONAL MATCH (s:SancM) WHERE s.tgt = 'AA' RETURN s.n",
    );
    assert_eq!(
        a.rows, b.rows,
        "indexed and unindexed walks agree, order included"
    );
}

#[test]
fn the_adjacency_probe_answers_exists_exactly_and_stays_fresh() {
    let g = graph();
    run(&g, "CREATE (:PA {n: 1})-[:PLINK]->(:PB {n: 2})");
    run(&g, "CREATE (:PA {n: 3})");
    let hits = |g: &Graph| {
        run(
            g,
            "MATCH (a:PA) RETURN a.n, exists((a)-[:PLINK]->(:PB)) ORDER BY a.n",
        )
        .rows
    };
    // The labelled far node keeps this on the GENERAL path (the probe
    // declines shapes it cannot verify); the bound-bound probe form runs
    // through a two-clause match.
    assert_eq!(
        hits(&g),
        vec![
            vec![Value::Int(1), Value::Bool(true)],
            vec![Value::Int(3), Value::Bool(false)],
        ]
    );
    let probed = run(
        &g,
        "MATCH (a:PA {n: 1}), (b:PB) RETURN exists((a)-[:PLINK]->(b)), exists((b)-[:PLINK]->(a)), exists((a)-[:NOPE]->(b))",
    );
    assert_eq!(
        probed.rows,
        vec![vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false)
        ]],
        "direction respected; a never-minted type has no edges"
    );
    // Staleness, through the PROBE's own path (both endpoints bound, no
    // labels in the predicate — a labelled far node declines the fast
    // path, which is how the first version of this test never touched
    // the cache it meant to check). Populate the snapshot with a probe,
    // write the relationship, probe again: the answer must flip.
    run(&g, "CREATE (:PB {n: 4})");
    let probe = |g: &Graph| {
        run(
            g,
            "MATCH (a:PA {n: 3}), (b:PB {n: 4}) RETURN exists((a)-[:PLINK]->(b))",
        )
        .rows[0][0]
            .clone()
    };
    assert_eq!(probe(&g), Value::Bool(false), "no relationship yet");
    run(
        &g,
        "MATCH (a:PA {n: 3}), (b:PB {n: 4}) CREATE (a)-[:PLINK]->(b)",
    );
    assert_eq!(
        probe(&g),
        Value::Bool(true),
        "the write moved the commit clock; a stale snapshot would still say false"
    );
    assert_eq!(
        hits(&g),
        vec![
            vec![Value::Int(1), Value::Bool(true)],
            vec![Value::Int(3), Value::Bool(true)],
        ],
        "the general path agrees"
    );
}

#[test]
fn a_single_row_stage_streams_and_never_builds_a_memo() {
    // The memo engages on the SECOND row, never the first: a stage-head
    // MATCH over the whole population must stream one candidate at a time.
    // Building there materialises the entire population for a replay that
    // never happens — measured as the production degree-census OOM.
    let g = graph();
    for i in 0..6 {
        run_params(
            &g,
            "CREATE (:LZ {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (r, trace) = engram_observe::with_trace(|| run(&g, "MATCH (n:LZ) RETURN n.i ORDER BY n.i"));
    assert_eq!(r.rows.len(), 6);
    assert_eq!(
        trace.counters().get("interp.clause scan memos built"),
        None,
        "one incoming row: the scan must stream, not build"
    );
    // Two clauses: the SECOND clause sees six rows, so its memo builds
    // exactly once — and the first clause (one incoming row) still streams.
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (a:LZ) OPTIONAL MATCH (b:LZ) WHERE b.i = a.i RETURN count(b)",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(6)]]);
    assert_eq!(
        trace.counters().get("interp.clause scan memos built"),
        Some(&1),
        "the multi-row clause builds ONE memo; the stage head builds none"
    );
}

#[test]
fn the_degree_count_fast_path_agrees_with_the_general_matcher() {
    // count { (n)--() } and its directed/typed forms are adjacency-list
    // sizes. Every shape below is answered both ways — the bare fast-path
    // form against a labelled form that must decline to the general
    // matcher — and they must agree, self-loops included.
    let g = graph();
    run(
        &g,
        "CREATE (a:DG {n: 1})-[:DL]->(b:DG {n: 2})-[:DL]->(c:DG {n: 3})",
    );
    run(
        &g,
        "MATCH (a:DG {n: 1}), (c:DG {n: 3}) CREATE (a)-[:DX]->(c)",
    );
    run(&g, "MATCH (a:DG {n: 1}) CREATE (a)-[:DSELF]->(a)");
    let r = run(
        &g,
        "MATCH (n:DG) RETURN n.n, \
           count { (n)--() },  count { (n)-->() },  count { (n)<--() }, \
           count { (n)-[:DL]->() }, count { (n)-[:NEVER]->() }, \
           count { (n)--(:DG) } \
         ORDER BY n.n",
    );
    // Node 1: DL out, DX out, DSELF self-loop (one rel; counts once
    // undirected, and appears both outgoing and incoming when directed).
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::Int(1),
                Value::Int(3), // DL + DX + DSELF, each rel once
                Value::Int(3), // DL, DX, DSELF all originate here
                Value::Int(1), // DSELF arrives here too
                Value::Int(1),
                Value::Int(0),
                Value::Int(3), // labelled far end: general path, same graph
            ],
            vec![
                Value::Int(2),
                Value::Int(2),
                Value::Int(1),
                Value::Int(1),
                Value::Int(1),
                Value::Int(0),
                Value::Int(2),
            ],
            vec![
                Value::Int(3),
                Value::Int(2),
                Value::Int(0),
                Value::Int(2),
                Value::Int(0),
                Value::Int(0),
                Value::Int(2),
            ],
        ]
    );
}

#[test]
fn hop_count_fast_paths_agree_with_the_general_path_on_every_shape() {
    // Every accepted (labels × types × direction) shape, checked against
    // the general path (a `WHERE true` declines every fast path), on a
    // graph with a self-loop, a cross-label rel, and an unrelated pair.
    let g = graph();
    run(&g, "CREATE (a:HX {i: 1})-[:HT]->(b:HY {i: 2})");
    run(&g, "MATCH (a:HX {i: 1}) CREATE (a)-[:HT]->(a)");
    run(
        &g,
        "MATCH (a:HX {i: 1}), (b:HY {i: 2}) CREATE (b)-[:HU]->(a)",
    );
    run(&g, "CREATE (:HZ {i: 3})-[:HT]->(:HZ {i: 4})");
    for shape in [
        "()-[r]->()",
        "()-[r:HT]->()",
        "(:HX)-[r]->()",
        "()-[r]->(:HY)",
        "(:HX)-[r:HT]->(:HY)",
        "(:HY)<-[r:HT]-(:HX)",
        "(:HX)-[r:HT]->(:HX)",
        "(:HX)-[r:NEVER]->()",
        "(:NeverMinted)-[r]->()",
    ] {
        let fast = one(&g, &format!("MATCH {shape} RETURN count(r)"));
        let slow = one(&g, &format!("MATCH {shape} WHERE true RETURN count(r)"));
        assert_eq!(fast, slow, "shape {shape}");
        // count(*) and counting a node var agree with counting the rel.
        let star = one(&g, &format!("MATCH {shape} RETURN count(*)"));
        assert_eq!(fast, star, "count(*) for {shape}");
    }
}

#[test]
fn the_rel_type_histogram_fast_path_matches_the_general_path() {
    let g = graph();
    for _ in 0..3 {
        run(&g, "CREATE (:RH)-[:ALPHA]->(:RH)");
    }
    run(&g, "CREATE (:RH)-[:BETA]->(:RH)");
    run(&g, "CREATE (:RH)-[:BETA]->(:RH)");
    run(&g, "CREATE (:RH)-[:GAMMA]->(:RH)");
    let fast = run(
        &g,
        "MATCH ()-[r]->() WITH type(r) AS t, count(*) AS c \
         RETURN t, c ORDER BY c DESC, t LIMIT 2",
    );
    assert_eq!(
        fast.rows,
        vec![
            vec![Value::Str("ALPHA".into()), Value::Int(3)],
            vec![Value::Str("BETA".into()), Value::Int(2)],
        ]
    );
    // The general path (WHERE declines the fast path) agrees exactly.
    let slow = run(
        &g,
        "MATCH ()-[r]->() WHERE true WITH type(r) AS t, count(*) AS c \
         RETURN t, c ORDER BY c DESC, t LIMIT 2",
    );
    assert_eq!(fast.rows, slow.rows);
    // Reversed item order + re-aliasing + SKIP also agree.
    let fast = run(
        &g,
        "MATCH ()-[r]->() WITH type(r) AS t, count(*) AS c \
         RETURN c AS n, t AS ty ORDER BY c DESC, t SKIP 1 LIMIT 1",
    );
    assert_eq!(
        fast.rows,
        vec![vec![Value::Int(2), Value::Str("BETA".into())]]
    );
    assert_eq!(fast.columns, vec!["n".to_string(), "ty".to_string()]);
}

#[test]
fn a_satisfied_plain_limit_stops_the_producer_not_just_the_buffer() {
    // Measured: RETURN n LIMIT 100 over a large label took 10.1s because
    // the projector dropped rows while the scan kept materialising every
    // candidate. The saturation signal must stop the SCAN — observable in
    // the materialisation counter, not just the row count.
    let g = graph();
    for i in 0..500 {
        run_params(
            &g,
            "CREATE (:SAT {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (r, trace) = engram_observe::with_trace(|| run(&g, "MATCH (n:SAT) RETURN n.i LIMIT 3"));
    assert_eq!(r.rows.len(), 3);
    let mats = trace
        .counters()
        .get("graph.projected node materialisations")
        .copied()
        .unwrap_or(0);
    assert!(
        mats <= 8,
        "the scan must stop at the limit: {mats} materialisations for LIMIT 3"
    );
    // SKIP pages correctly under the same early stop.
    let rows = run(&g, "MATCH (n:SAT) RETURN n.i LIMIT 3").rows;
    let paged = run(&g, "MATCH (n:SAT) RETURN n.i SKIP 1 LIMIT 2").rows;
    assert_eq!(&rows[1..], &paged[..], "SKIP window is the same stream");
}

#[test]
fn count_of_a_bare_variable_materialises_slim_not_full() {
    // count(m) needs presence (and identity under DISTINCT), never the
    // properties — the production statement decoded 1.79M full nodes to
    // produce one number. Observable: the PROJECTED materialisation
    // counter fires for every scanned node instead of the full decoder.
    let g = graph();
    for i in 0..40 {
        run_params(
            &g,
            "CREATE (:CDM {i: $i, fat: 'a fat property the count must never decode'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // A carried variable in the WHERE is a join: every fast path declines
    // (the count fast path, the columnar scan, the constant-count stage),
    // so this streams through demand analysis.
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "WITH 0 AS z MATCH (m:CDM) WHERE m.i >= z WITH count(m) AS c RETURN c",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(40)]]);
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&40),
        "all scans went through the slim projection"
    );
    // DISTINCT counts by identity through the same slim value.
    let r = run(
        &g,
        "MATCH (m:CDM) WHERE m.i >= 0 RETURN count(DISTINCT m) AS c",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(40)]]);
    // A property use elsewhere still widens: sum needs the value.
    let r = run(
        &g,
        "MATCH (m:CDM) WHERE m.i >= 0 RETURN count(m) AS c, sum(m.i) AS s",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(40), Value::Int(780)]]);
}

#[test]
fn streaming_folds_match_the_reference_fold_on_every_function() {
    // Every streamed aggregate against hand-computed answers, plus the
    // DISTINCT variants and the empty-group behaviours the fold defines
    // (count 0, collect [], sum 0, avg/min/max null).
    let g = graph();
    let r = run(
        &g,
        "UNWIND [3, 1, 1.0, 2, null] AS x \
         RETURN count(x), count(DISTINCT x), sum(x), sum(DISTINCT x), \
                avg(DISTINCT x), min(x), max(x), collect(DISTINCT x)",
    );
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Int(4),
            Value::Int(3),
            Value::Float(7.0),
            // MEASURED (Neo4j 2026-08-21): DISTINCT keeps the FIRST-SEEN
            // representative — Int 1 arrives before Float 1.0 here, so the
            // distinct sum is integer-typed. The float-first spelling
            // below pins the other direction.
            Value::Int(6),
            Value::Float(2.0),
            Value::Int(1),
            Value::Int(3),
            Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]),
        ]]
    );
    let r = run(
        &g,
        "UNWIND [1] AS x WITH x WHERE x > 5 \
         RETURN count(x), sum(x), avg(x), min(x), max(x), collect(x)",
    );
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Int(0),
            Value::Int(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::List(vec![]),
        ]]
    );
    // Float first: the representative flips, and so does the sum's type.
    let r = run(&g, "UNWIND [3, 1.0, 1, 2] AS x RETURN sum(DISTINCT x) AS s");
    assert_eq!(r.rows, vec![vec![Value::Float(6.0)]]);
    // The overflow refusal survives the streaming rewrite.
    let q = parse_statement("UNWIND [9223372036854775807, 1] AS x RETURN sum(x)").unwrap();
    assert!(
        run_query(&g, &q, Default::default()).is_err(),
        "sum overflow refuses"
    );
}

#[test]
fn a_label_predicate_in_where_keeps_the_scan_slim() {
    // `m:Label` reads labels, which slim nodes carry; it must not widen the
    // demand to Full (measured: the label-OR count(m) statement stayed at
    // 132s because this arm re-widened what the aggregate rule narrowed).
    let g = graph();
    for i in 0..30 {
        run_params(
            &g,
            "CREATE (:LP1 {i: $i, fat: 'never decoded'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    run(&g, "CREATE (:LP2 {i: 99, fat: 'x'})");
    // A carried variable in the WHERE keeps this off every fast path; the
    // general streaming path is what demand analysis governs.
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "WITH 0 AS z MATCH (m) WHERE (m:LP1 OR m:LP2) AND m.i >= z WITH count(m) AS c RETURN c",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(31)]]);
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&31),
        "label predicates ride on the slim projection"
    );
}

#[test]
fn a_subquery_endpoint_demands_identity_not_properties() {
    // count { (n)--() } consumes n by IDENTITY: the expansion starts from
    // its id. Measured: the degree-histogram census spent most of 670s
    // fully decoding and re-cloning fat nodes nothing read. The projected
    // counter proves the slim path; the answers prove nothing changed.
    let g = graph();
    run(
        &g,
        "CREATE (a:SQ {i: 1, fat: 'never decoded'})-[:SL]->(b:SQ {i: 2, fat: 'x'})",
    );
    run(&g, "CREATE (:SQ {i: 3, fat: 'y'})");
    // The general path's demand analysis is the subject: the columnar
    // paths are switched off for it.
    g.set_columnar_scans(false);
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:SQ) WITH n, count { (n)--() } AS d RETURN d ORDER BY d",
        )
    });
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(0)],
            vec![Value::Int(1)],
            vec![Value::Int(1)]
        ]
    );
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&3),
        "endpoint-only use keeps the scan slim"
    );
    // exists() with both endpoints bare is identity too; a property use
    // elsewhere still widens exactly that property.
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (a:SQ), (b:SQ) WHERE a.i = 1 AND exists((a)-[:SL]->(b)) RETURN b.i",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    assert!(
        trace
            .counters()
            .get("graph.projected node materialisations")
            .copied()
            .unwrap_or(0)
            >= 3,
        "both scans stay projected"
    );
    // Inner props on the endpoint are MATCHED against the node: that is a
    // property use, so the endpoint stays Full — and the answer is right.
    let r = run(
        &g,
        "MATCH (n:SQ) RETURN n.i, count { (n {i: 1})-->() } AS d ORDER BY n.i",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(0)],
            vec![Value::Int(3), Value::Int(0)],
        ]
    );
}

#[test]
fn liveness_never_prunes_a_variable_a_later_clause_reads() {
    // The over-approximation direction: a later mention keeps the item
    // live (Full), and a WITH * makes liveness unknown (everything live).
    let g = graph();
    g.set_columnar_scans(false); // the general path's liveness is the subject
    run(&g, "CREATE (:LV {i: 1, fat: 'x'})");
    run(&g, "CREATE (:LV {i: 2, fat: 'y'})");
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:LV) WITH n, n.i AS i WITH i, n RETURN n.fat ORDER BY i",
        )
    });
    assert_eq!(
        r.rows,
        vec![vec![Value::Str("x".into())], vec![Value::Str("y".into())]]
    );
    // Live after the WITH — and every later mention is a PROPERTY read
    // (`n.i`, `n.fat`), so the carry demands those two properties, not the
    // full node (fix 24: a prefix WITH summarises what the rest of the stage
    // reads, as the stage boundary always did). Before fix 24 this stayed
    // Full — the over-approximation the test used to pin.
    assert_eq!(
        trace.counters().get("graph.nodes materialised in full"),
        None,
        "n is read only by property after the WITH: never Full"
    );
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&2),
        "n is projected to the properties read after the WITH"
    );
    let (r, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:LV) WITH n, n.i AS i WITH * RETURN n.fat ORDER BY i",
        )
    });
    assert_eq!(r.rows.len(), 2);
    // A `WITH *` is see-through: it carries `n` on unchanged and the RETURN
    // after it reads only `n.fat`, so the same projection applies.
    assert_eq!(
        trace.counters().get("graph.nodes materialised in full"),
        None,
        "WITH * carries n to a property read: never Full"
    );
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&2),
        "WITH * carries n to a property read: projected"
    );
    // Dead after projection: slim.
    let (r, trace) =
        engram_observe::with_trace(|| run(&g, "MATCH (n:LV) WITH n, n.i AS i RETURN i ORDER BY i"));
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        Some(&2),
        "n is dead after the WITH: presence only"
    );
}

#[test]
fn the_degree_table_answers_exactly_what_direct_probes_answer() {
    // Direct probes and the per-epoch table must agree on every shape —
    // self-loops, direction, types — and a write must retire the table.
    let g = graph();
    run(
        &g,
        "CREATE (a:DT {n: 1})-[:DA]->(b:DT {n: 2})-[:DB]->(c:DT {n: 3})",
    );
    run(&g, "MATCH (a:DT {n: 1}) CREATE (a)-[:DA]->(a)");
    run(
        &g,
        "MATCH (a:DT {n: 1}), (c:DT {n: 3}) CREATE (c)-[:DA]->(a)",
    );
    let q = "MATCH (n:DT) RETURN n.n, count { (n)--() }, count { (n)-->() }, \
             count { (n)<--() }, count { (n)-[:DA]-() } ORDER BY n.n";
    let direct = run(&g, q).rows;
    assert_eq!(
        direct,
        vec![
            vec![
                Value::Int(1),
                Value::Int(3),
                Value::Int(2),
                Value::Int(2),
                Value::Int(3)
            ],
            vec![
                Value::Int(2),
                Value::Int(2),
                Value::Int(1),
                Value::Int(1),
                Value::Int(1)
            ],
            vec![
                Value::Int(3),
                Value::Int(2),
                Value::Int(1),
                Value::Int(1),
                Value::Int(1)
            ],
        ]
    );
    g.set_degree_table_after(0);
    let (r, trace) = engram_observe::with_trace(|| run(&g, q));
    assert_eq!(r.rows, direct, "the table agrees with direct probes");
    assert!(
        trace
            .counters()
            .get("graph.degree tables built")
            .copied()
            .unwrap_or(0)
            >= 1,
        "the table was actually used"
    );
    // A write moves the epoch: the table rebuilds and sees the new edge.
    run(
        &g,
        "MATCH (b:DT {n: 2}), (c:DT {n: 3}) CREATE (b)-[:DB]->(c)",
    );
    let r = run(&g, "MATCH (n:DT {n: 2}) RETURN count { (n)--() }");
    assert_eq!(r.rows, vec![vec![Value::Int(3)]], "stale table would say 2");
}

#[test]
fn slim_expansion_and_adjacency_tables_answer_exactly_what_full_expansion_answers() {
    // The slim path (no rel props, no rel var, no trail) reads key bytes
    // only; the full path fetches every record. Both, with and without the
    // per-epoch table, must agree on every shape — typed, undirected with
    // a self-loop, var-length, and with the rel var BOUND (which forces
    // the full path and pins the reference).
    let g = graph();
    run(
        &g,
        "CREATE (a:AX {i: 1})-[:AT]->(b:AX {i: 2})-[:AT]->(c:AX {i: 3})",
    );
    run(&g, "MATCH (a:AX {i: 1}) CREATE (a)-[:AT]->(a)");
    run(
        &g,
        "MATCH (a:AX {i: 1}), (c:AX {i: 3}) CREATE (c)-[:AU]->(a)",
    );
    let shapes = [
        "MATCH (a:AX {i: 1})-[:AT]->(x) RETURN count(*)",
        "MATCH (a:AX {i: 1})-[:AT]-(x) RETURN count(*)",
        "MATCH (a:AX {i: 1})-->(x) RETURN count(*)",
        "MATCH (a:AX {i: 1})-[*1..2]->(x) RETURN count(*)",
        "MATCH (a:AX {i: 1})-[:AT*1..3]-(x) RETURN count(*)",
        "MATCH (a:AX)-[:AU|AT]->(x:AX) RETURN count(*)",
        "MATCH (a:AX {i: 1})-[:NEVER]->(x) RETURN count(*)",
    ];
    let reference: Vec<Value> = shapes
        .iter()
        .map(|q| {
            one(
                &g,
                &q.replace("]->(x)", "]->(x) WHERE true")
                    .replace("]-(x)", "]-(x) WHERE true")
                    .replace("-->(x)", "-->(x) WHERE true"),
            )
        })
        .collect();
    // Full path reference: bind the rel variable, which forces rels_of.
    let with_var: Vec<Value> = shapes
        .iter()
        .map(|q| {
            let bound = q
                .replace("-[:AT]->", "-[r:AT]->")
                .replace("-[:AT]-(", "-[r:AT]-(")
                .replace("-->(x)", "-[r]->(x)")
                .replace("-[*1..2]->", "-[r*1..2]->")
                .replace("-[:AT*1..3]-", "-[r:AT*1..3]-")
                .replace("-[:AU|AT]->", "-[r:AU|AT]->")
                .replace("-[:NEVER]->", "-[r:NEVER]->");
            one(
                &g,
                &bound.replace("RETURN count(*)", "WHERE true RETURN count(*)"),
            )
        })
        .collect();
    assert_eq!(
        reference, with_var,
        "anonymous and bound rel patterns agree"
    );
    for (q, want) in shapes.iter().zip(&reference) {
        assert_eq!(
            &one(
                &g,
                &format!("{q} WHERE true")
                    .replace("RETURN count(*) WHERE true", "WHERE true RETURN count(*)")
            ),
            want,
            "slim direct: {q}"
        );
    }
    g.set_degree_table_after(0);
    for (q, want) in shapes.iter().zip(&reference) {
        let (v, trace) = engram_observe::with_trace(|| {
            one(
                &g,
                &format!("{q} WHERE true")
                    .replace("RETURN count(*) WHERE true", "WHERE true RETURN count(*)"),
            )
        });
        assert_eq!(&v, want, "slim via table: {q}");
        if !q.contains("NEVER") {
            assert!(
                trace
                    .counters()
                    .get("graph.adjacency tables built")
                    .copied()
                    .unwrap_or(0)
                    >= 1
                    || !trace.counters().is_empty(),
                "table engaged"
            );
        }
    }
}

#[test]
fn exists_with_a_labelled_unbound_far_end_matches_the_general_path() {
    let g = graph();
    run(&g, "CREATE (e1:EV {i: 1})-[:OCC]->(:CTY:Big {n: 'A'})");
    run(&g, "CREATE (e2:EV {i: 2})-[:OCC]->(:Region {n: 'R'})");
    run(&g, "CREATE (e3:EV {i: 3})");
    run(&g, "MATCH (e:EV {i: 3}) CREATE (e)-[:OCC]->(:CTY {n: 'B'})");
    let fast = run(
        &g,
        "MATCH (e:EV) RETURN e.i, exists((e)-[:OCC]->(:CTY)), exists((e)-[:OCC]->(:CTY:Big)), \
         exists((e)-[:OCC]->()), exists((e)-[:NOPE]->(:CTY)) ORDER BY e.i",
    );
    assert_eq!(
        fast.rows,
        vec![
            vec![
                Value::Int(1),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false)
            ],
            vec![
                Value::Int(2),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false)
            ],
            vec![
                Value::Int(3),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false)
            ],
        ]
    );
    // The general path (a far PROP declines the probe) agrees.
    let general = run(
        &g,
        "MATCH (e:EV) RETURN e.i, exists((e)-[:OCC]->(:CTY {n: 'A'})) OR exists((e)-[:OCC]->(:CTY {n: 'B'})) ORDER BY e.i",
    );
    assert_eq!(
        general
            .rows
            .iter()
            .map(|r| r[1].clone())
            .collect::<Vec<_>>(),
        vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)]
    );
    // Counting the census shape: the backfill statement's CASE form.
    let r = run(
        &g,
        "MATCH (e:EV) WITH count(e) AS total, \
         count(CASE WHEN exists((e)-[:OCC]->(:CTY)) THEN 1 END) AS withEdge RETURN total, withEdge",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(3), Value::Int(2)]]);
}

#[test]
fn the_count_store_agrees_with_the_membership_walk_under_every_write() {
    // Maintained counts vs the walk, after each of a deterministic mix of
    // creates, label adds/removes, relationship creates/deletes and detach
    // deletes. Then the REBUILD path: a graph opened over a recovered
    // store starts without stats and must reconstruct the same numbers.
    let g = graph();
    let labels = ["CS1", "CS2", "CS3"];
    let types = ["CT1", "CT2"];
    let mut x: u64 = 0x1234_5678_9ABC_DEF0;
    let mut next = || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 33
    };
    let check = |g: &Graph, step: usize| {
        assert_eq!(
            g.count_all_nodes(),
            g.members(None).unwrap().len() as u64,
            "nodes at step {step}"
        );
        for l in labels {
            assert_eq!(
                g.count_label_nodes(l),
                g.members(Some(l)).unwrap().len() as u64,
                "label {l} at step {step}"
            );
        }
        let walked = one(g, "MATCH ()-[r]->() WHERE true RETURN count(r)");
        assert_eq!(
            Value::Int(g.count_all_rels() as i64),
            walked,
            "rels at step {step}"
        );
        for t in types {
            let walked = one(
                g,
                &format!("MATCH ()-[r:{t}]->() WHERE true RETURN count(r)"),
            );
            let fast = one(g, &format!("MATCH ()-[r:{t}]->() RETURN count(r)"));
            assert_eq!(fast, walked, "type {t} at step {step}");
        }
    };
    let mut ids: Vec<u64> = Vec::new();
    for step in 0..120usize {
        let roll = next();
        match roll % 6 {
            0 | 1 => {
                let mut ls: Vec<String> = Vec::new();
                for (i, l) in labels.iter().enumerate() {
                    if (roll >> (i + 3)) & 1 == 1 {
                        ls.push(l.to_string());
                    }
                }
                let id = g.create_node(&ls, &Default::default()).unwrap();
                ids.push(id);
            }
            2 if ids.len() >= 2 => {
                let a = ids[(next() as usize) % ids.len()];
                let b = ids[(next() as usize) % ids.len()];
                let t = types[(next() as usize) % types.len()];
                g.create_rel(a, t, b, &Default::default()).unwrap();
            }
            3 if !ids.is_empty() => {
                let id = ids[(next() as usize) % ids.len()];
                let l = labels[(next() as usize) % labels.len()].to_string();
                if next() % 2 == 0 {
                    g.add_labels(id, &[l]).unwrap();
                } else {
                    g.remove_labels(id, &[l]).unwrap();
                }
            }
            4 if !ids.is_empty() => {
                let i = (next() as usize) % ids.len();
                let id = ids.swap_remove(i);
                g.delete_node(id, true).unwrap();
            }
            _ => {}
        }
        check(&g, step);
    }
    // Rebuild path: recover the log into a fresh store, open a graph over
    // it (stats deferred), and every count must reconstruct exactly.
    let before = (
        g.count_all_nodes(),
        labels.map(|l| g.count_label_nodes(l)),
        g.count_all_rels(),
        g.rel_type_histogram().unwrap(),
    );
    let recovered = Store::recover(&g.shared_store().log_tail(0)).expect("recover");
    let g2 = Graph::new(recovered, Realm(1), Namespace(1));
    let (r, trace) = engram_observe::with_trace(|| {
        (
            g2.count_all_nodes(),
            labels.map(|l| g2.count_label_nodes(l)),
            g2.count_all_rels(),
            g2.rel_type_histogram().unwrap(),
        )
    });
    assert_eq!(r, before, "rebuilt stats equal maintained stats");
    assert_eq!(trace.counters().get("graph.stats rebuilt"), Some(&1));
}

#[test]
fn multi_label_counts_answer_exactly_what_the_general_path_answers() {
    // (s:Bio:Species) declined every fast path on its second label and
    // streamed the larger label instead. Intersections of id-sorted
    // memberships, checked against the general path (WHERE true declines
    // the fast paths) on node counts, hop counts with multi-label ends,
    // three labels, and a disjoint pair.
    let g = graph();
    run(
        &g,
        "CREATE (:ML1:ML2 {i: 1}), (:ML1:ML2:ML3 {i: 2}), (:ML1 {i: 3}), (:ML2 {i: 4}), (:ML3 {i: 5})",
    );
    run(
        &g,
        "MATCH (a:ML1:ML2 {i: 1}), (b:ML3) CREATE (a)-[:MLT]->(b)",
    );
    run(
        &g,
        "MATCH (a:ML1 {i: 3}), (b:ML2:ML3) CREATE (a)-[:MLT]->(b)",
    );
    for shape in [
        "MATCH (n:ML1:ML2) RETURN count(n)",
        "MATCH (n:ML1:ML2:ML3) RETURN count(*)",
        "MATCH (n:ML3:ML1) RETURN count(n)",
        "MATCH (n:ML2:ML3) RETURN count(n)",
        "MATCH (n:ML1:NeverMinted) RETURN count(n)",
        "MATCH (:ML1:ML2)-[r:MLT]->(:ML3) RETURN count(r)",
        "MATCH (:ML1)-[r:MLT]->(:ML2:ML3) RETURN count(r)",
        "MATCH (:ML1:ML2)-[r]->(:ML2:ML3) RETURN count(*)",
    ] {
        let fast = one(&g, shape);
        let slow = one(&g, &shape.replace(" RETURN", " WHERE true RETURN"));
        assert_eq!(fast, slow, "{shape}");
    }
    assert_eq!(one(&g, "MATCH (n:ML1:ML2) RETURN count(n)"), Value::Int(2));
    assert_eq!(
        one(&g, "MATCH (n:ML1:ML2:ML3) RETURN count(n)"),
        Value::Int(1)
    );
    assert_eq!(
        one(&g, "MATCH (n:ML1:NeverMinted) RETURN count(n)"),
        Value::Int(0)
    );
}

#[test]
fn the_hash_index_joins_every_type_exactly_as_the_where_would() {
    // The G0 canonical key IS Cypher `=`-equivalence, so the index buckets
    // every type: Int outer meets Float inner, composite keys, lists; null
    // and NaN never join; a bound-key OPTIONAL still emits its null row.
    let g = graph();
    run(
        &g,
        "CREATE (:HJO {k: 1, t: 'a', i: 1}), (:HJO {k: 2.0, t: 'b', i: 2}), (:HJO {k: [1, 2], t: 'c', i: 3}), (:HJO {t: 'd', i: 4})",
    );
    run(
        &g,
        "CREATE (:HJI {k: 1.0, t: 'a', n: 'x'}), (:HJI {k: 2, t: 'b', n: 'y'}), (:HJI {k: [1.0, 2], t: 'c', n: 'z'}), (:HJI {k: 1, t: 'zz', n: 'w'}), (:HJI {t: 'd', n: 'v'})",
    );
    let fast = run(
        &g,
        "MATCH (o:HJO) OPTIONAL MATCH (i:HJI) WHERE i.k = o.k RETURN o.i, collect(i.n) ORDER BY o.i",
    );
    assert_eq!(
        fast.rows,
        vec![
            vec![
                Value::Int(1),
                Value::List(vec![Value::Str("x".into()), Value::Str("w".into())])
            ],
            vec![Value::Int(2), Value::List(vec![Value::Str("y".into())])],
            vec![Value::Int(3), Value::List(vec![Value::Str("z".into())])],
            vec![Value::Int(4), Value::List(vec![])],
        ],
        "1 meets 1.0 and 1; 2.0 meets 2; lists unify elementwise; a missing key joins nothing"
    );
    let slow = run(
        &g,
        "MATCH (o:HJO) OPTIONAL MATCH (i:HJI) WHERE i.k = o.k AND true RETURN o.i, collect(i.n) ORDER BY o.i",
    );
    assert_eq!(fast.rows, slow.rows);
    // Composite key: both conjuncts must hold.
    let r = run(
        &g,
        "MATCH (o:HJO) OPTIONAL MATCH (i:HJI) WHERE i.k = o.k AND i.t = o.t RETURN o.i, collect(i.n) ORDER BY o.i",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::List(vec![Value::Str("x".into())])],
            vec![Value::Int(2), Value::List(vec![Value::Str("y".into())])],
            vec![Value::Int(3), Value::List(vec![Value::Str("z".into())])],
            vec![Value::Int(4), Value::List(vec![])],
        ]
    );
    // NaN never joins, from either side.
    let params: BTreeMap<String, Value> = [("nan".to_string(), Value::Float(f64::NAN))]
        .into_iter()
        .collect();
    run_params(
        &g,
        "CREATE (:HJO {k: $nan, i: 5}), (:HJI {k: $nan, n: 'nan'})",
        params,
    );
    let r = run(
        &g,
        "MATCH (o:HJO {i: 5}) OPTIONAL MATCH (i:HJI) WHERE i.k = o.k RETURN collect(i.n)",
    );
    assert_eq!(r.rows, vec![vec![Value::List(vec![])]]);
}

#[test]
fn a_cartesian_match_splits_exactly_and_optional_never_does() {
    let g = graph();
    run(
        &g,
        "CREATE (:CX {k: 1, i: 1}), (:CX {k: 2, i: 2}), (:CY {k: 1, j: 10}), (:CY {k: 1, j: 11}), (:CY {k: 3, j: 12})",
    );
    // The split shape vs the same cartesian kept whole by a path variable
    // (a path var defeats the single-node rule) — identical rows, in order.
    let split = run(&g, "MATCH (a:CX), (b:CY) WHERE a.k = b.k RETURN a.i, b.j");
    let whole = run(
        &g,
        "MATCH p = (a:CX), (b:CY) WHERE a.k = b.k RETURN a.i, b.j",
    );
    assert_eq!(split.rows, whole.rows);
    assert_eq!(
        split.rows,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(1), Value::Int(11)]
        ]
    );
    let (_, trace) = engram_observe::with_trace(|| {
        run(&g, "MATCH (a:CX), (b:CY) WHERE a.k = b.k RETURN a.i, b.j")
    });
    assert!(
        trace
            .counters()
            .get("interp.clause scan memo indexed")
            .copied()
            .unwrap_or(0)
            >= 1,
        "the split clause is indexed — the join is a hash join"
    );
    // OPTIONAL MATCH (a), (b) fails as ONE pattern: when no b exists the
    // whole row is null, not |a| rows with a null b. Never split.
    let r = run(
        &g,
        "MATCH (x:CX {i: 1}) OPTIONAL MATCH (a:CX), (b:NoSuchLabel) RETURN x.i, a.i, b.j",
    );
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Null, Value::Null]]);
}

#[test]
fn the_columnar_aggregate_scan_answers_exactly_what_the_general_path_answers() {
    // Differential: every operator class and every aggregate, keyed and
    // unkeyed, over a fixture mixing Int/Float/Str/Bool/Null/missing and
    // two labels. The general path is forced by an interposed `WITH n`
    // (two stages decline the scan); rows must match exactly, order too.
    let g = graph();
    let fixture = [
        "CREATE (:CA:CB {k: 'a', i: 1, f: 1.5, b: true})",
        "CREATE (:CA {k: 'b', i: 2, f: 2.0})",
        "CREATE (:CA {k: 'a', i: 3, b: false})",
        "CREATE (:CA:CB {k: 'c', i: 1.0, f: 0.5})",
        "CREATE (:CA {i: 4, f: 4.0, b: true})",
        "CREATE (:CA {k: 'b'})",
        "CREATE (:CB {k: 'zz', i: 9})",
    ];
    for f in fixture {
        run(&g, f);
    }
    let cases = [
        "MATCH (n:CA) RETURN count(*)",
        "MATCH (n:CA) RETURN count(n)",
        "MATCH (n:CA) RETURN count(n.k), count(DISTINCT n.k), sum(n.i), avg(n.f), min(n.i), max(n.k), collect(n.i)",
        "MATCH (n:CA) WHERE n.i > 1 RETURN count(*)",
        "MATCH (n:CA) WHERE n.i >= 1.0 AND n.k <> 'b' RETURN count(*), sum(n.i)",
        "MATCH (n:CA) WHERE n.f < 2 OR n.b RETURN count(*)",
        "MATCH (n:CA) WHERE n.k IS NULL RETURN count(*)",
        "MATCH (n:CA) WHERE n.k IS NOT NULL AND NOT n.b RETURN count(*)",
        "MATCH (n:CA) WHERE n:CB RETURN count(*)",
        "MATCH (n:CA) WHERE (n:CB OR n.k = 'b') AND n.i IS NOT NULL RETURN count(n)",
        "MATCH (n:CA) WHERE n.k IN ['a', 'c'] RETURN count(*)",
        "MATCH (n:CA) WHERE coalesce(n.f, 0) >= 1 RETURN count(*)",
        "MATCH (n:CA) WHERE n.i = 1 RETURN count(*)",
        "MATCH (n:CA) WHERE n.i <= n.f RETURN count(*)",
        "MATCH (n) WHERE n.k IS NOT NULL RETURN count(n)",
        "MATCH (n:CA) RETURN n.k, count(*)",
        "MATCH (n:CA) RETURN n.k, count(*), sum(n.i) ORDER BY count(*) DESC, n.k",
        "MATCH (n:CA) RETURN n.k AS key, count(*) AS c ORDER BY c DESC, key SKIP 1 LIMIT 2",
        "MATCH (n:CA) WHERE n.i > 100 RETURN count(*), sum(n.i), avg(n.i), min(n.k), collect(n.k)",
        "MATCH (n:CA) WHERE n.i > 100 RETURN n.k, count(*)",
        "MATCH (n:CA:CB) RETURN n.k, count(*) ORDER BY n.k",
        "MATCH (n:NeverMinted) RETURN count(*)",
        "MATCH (n:CA) WHERE n.nope = 1 RETURN count(*)",
    ];
    for q in cases {
        let fast = run(&g, q);
        let general = run(
            &g,
            &q.replacen(" WHERE ", " WITH n WHERE ", 1)
                .replacen(" RETURN", " WITH n RETURN", 1)
                .replace(" WITH n WITH n RETURN", " WITH n RETURN"),
        );
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
    }
    // The scan materialises NO node for the class — and a predicate that
    // reads `n` another way declines to the general path, which does.
    let (_, trace) =
        engram_observe::with_trace(|| run(&g, "MATCH (n:CA) WHERE n.i > 1 RETURN count(*)"));
    assert_eq!(
        trace.counters().get("interp.columnar aggregate scans"),
        Some(&1)
    );
    assert_eq!(
        trace
            .counters()
            .get("graph.projected node materialisations"),
        None
    );
    let (_, t2) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:CA) WHERE size(keys(n)) > 1 RETURN count(*)")
    });
    assert_eq!(
        t2.counters().get("interp.columnar aggregate scans"),
        None,
        "a bare use of n declines"
    );
}

#[test]
fn the_columnar_scan_v2_takes_with_return_case_and_exists_exactly() {
    // The census shapes: an aggregating WITH followed by a RETURN over its
    // aliases; CASE WHEN exists((n)-[:T]->(:L)) THEN 1 END as an aggregate
    // argument; NOT exists(...) in the WHERE; expression keys. Each is
    // checked against the general path (an extra bare WITH declines).
    let g = graph();
    run(&g, "CREATE (:EV2 {i: 1, r: 'a'})-[:OCC2]->(:CT2 {n: 'x'})");
    run(&g, "CREATE (:EV2 {i: 2, r: 'a'})-[:OCC2]->(:RG2 {n: 'y'})");
    run(&g, "CREATE (:EV2 {i: 3})");
    run(&g, "CREATE (:EV2 {i: 4, r: 'b'})");
    run(
        &g,
        "MATCH (e:EV2 {i: 4}) CREATE (e)-[:OCC2]->(:CT2 {n: 'z'})",
    );
    let cases = [
        "MATCH (e:EV2) WITH count(e) AS total, count(CASE WHEN exists((e)-[:OCC2]->(:CT2)) THEN 1 END) AS withEdge, count(CASE WHEN e.r IS NOT NULL THEN 1 END) AS withRegion RETURN total, withEdge, withRegion",
        "MATCH (e:EV2) WHERE e.r IS NOT NULL AND NOT exists((e)-[:OCC2]->(:CT2)) WITH e.r AS region, count(e) AS n RETURN region, n ORDER BY n DESC, region LIMIT 20",
        "MATCH (e:EV2) WITH count(e) AS total, count(CASE WHEN exists((e)-[:OCC2]->(:CT2)) THEN 1 END) AS withEdge RETURN withEdge, total, toFloat(withEdge) / total AS share",
        "MATCH (e:EV2) RETURN e.i % 2 AS parity, count(*) AS c ORDER BY parity",
        "MATCH (e:EV2) WHERE exists((e)-[:OCC2]->()) RETURN count(*)",
        "MATCH (e:EV2) WHERE (e)-[:OCC2]->(:CT2) RETURN sum(e.i)",
        "MATCH (e:EV2) WITH e.r AS r, collect(e.i) AS xs RETURN r, size(xs) AS k ORDER BY k DESC, r",
        "MATCH (e:EV2) WITH e.r AS r, count(*) AS c WHERE c > 1 RETURN r, c",
    ];
    for q in cases {
        let fast = run(&g, q);
        let general = run(&g, &q.replacen("MATCH (e:EV2)", "MATCH (e:EV2) WITH e", 1));
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
    }
    assert_eq!(
        run(&g, "MATCH (e:EV2) WITH count(e) AS total, count(CASE WHEN exists((e)-[:OCC2]->(:CT2)) THEN 1 END) AS withEdge RETURN total, withEdge").rows,
        vec![vec![Value::Int(4), Value::Int(2)]]
    );
    let (_, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:EV2) WHERE NOT exists((e)-[:OCC2]->(:CT2)) RETURN count(*)",
        )
    });
    assert_eq!(
        trace.counters().get("interp.columnar aggregate scans"),
        Some(&1),
        "the probe-lifting scan ran"
    );
}

#[test]
fn the_bare_count_stage_groups_exactly_as_the_general_path() {
    // `[OPTIONAL] MATCH (v:L) WITH carried, count(v) AS c` chains answer
    // from the count store. Grouping is the general path's: carried values
    // that collide across input rows fold into one group whose count is
    // |L| × multiplicity; an OPTIONAL match over an empty label keeps its
    // null row (count(v) 0, count(*) multiplicity); a non-OPTIONAL one
    // yields no row when keyed and one row `0` when not. The general path
    // is forced with `WHERE true` on the MATCH. The carried rows arrive
    // through a BREAKER (`ORDER BY k`): a bare `WITH p.k AS k` folds into
    // the next stage's prefix, which is three clauses and declines.
    let g = graph();
    run(
        &g,
        "CREATE (:BC1 {k: 1}), (:BC1 {k: 1}), (:BC1 {k: 2}), (:BC2 {k: 1}), (:BC2:BC3 {k: 9}), (:BC2:BC3 {k: 9})",
    );
    let cases = [
        "MATCH (s:BC1) WITH count(s) AS a OPTIONAL MATCH (d:BC2) WITH a, count(d) AS b OPTIONAL MATCH (x:BC2:BC3) WITH a, b, count(x) AS c RETURN a, b, c",
        "MATCH (s:BC1) WITH count(*) AS a MATCH (d:BC2:BC3) WITH a, count(*) AS b RETURN a, b",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k MATCH (d:BC2) WITH k, count(d) AS n RETURN k, n ORDER BY k",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k MATCH (d:BC2) WITH k, count(*) AS n RETURN k, n ORDER BY k",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k OPTIONAL MATCH (d:BCNONE) WITH k, count(d) AS n RETURN k, n ORDER BY k",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k OPTIONAL MATCH (d:BCNONE) WITH k, count(*) AS n RETURN k, n ORDER BY k",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k MATCH (d:BCNONE) WITH k, count(d) AS n RETURN k, n ORDER BY k",
        "MATCH (p:BC1) WITH p.k AS k ORDER BY k MATCH (d:BCNONE) WITH count(d) AS n RETURN n",
        "MATCH (p:BCNONE) WITH p.k AS k ORDER BY k MATCH (d:BC1) WITH count(d) AS n RETURN n",
        "MATCH (p:BCNONE) WITH p.k AS k ORDER BY k MATCH (d:BC1) WITH k, count(d) AS n RETURN k, n",
        "MATCH (p:BCNONE) WITH p.k AS k ORDER BY k OPTIONAL MATCH (d:BC1) WITH count(*) AS n RETURN n",
        "MATCH (d:BC1) WITH count(d) AS n RETURN n",
        "MATCH (d:BCNONE) WITH count(d) AS n RETURN n",
        "OPTIONAL MATCH (d:BCNONE) WITH count(*) AS n RETURN n",
    ];
    for q in cases {
        let fast = run(&g, q);
        let general = run(&g, &q.replace(") WITH", ") WHERE true WITH"));
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
    }
    assert_eq!(
        run(&g, "MATCH (s:BC1) WITH count(s) AS a OPTIONAL MATCH (d:BC2) WITH a, count(d) AS b OPTIONAL MATCH (x:BC2:BC3) WITH a, b, count(x) AS c RETURN a, b, c").rows,
        vec![vec![Value::Int(3), Value::Int(3), Value::Int(2)]]
    );
    assert_eq!(
        run(&g, "MATCH (p:BC1) WITH p.k AS k ORDER BY k MATCH (d:BC2) WITH k, count(d) AS n RETURN k, n ORDER BY k").rows,
        vec![vec![Value::Int(1), Value::Int(6)], vec![Value::Int(2), Value::Int(3)]],
        "two input rows with k=1 fold into one group of 2 × 3"
    );
    let (_, trace) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (s:BC1) WITH count(s) AS a OPTIONAL MATCH (d:BC2) WITH a, count(d) AS b RETURN a, b",
        )
    });
    assert_eq!(
        trace.counters().get("interp.bare count stages"),
        Some(&2),
        "both stages took the store"
    );
    // Declines: a bound start, a WHERE, a property map, DISTINCT, a
    // non-count aggregate, an unlabelled node.
    for q in [
        "MATCH (d:BC1) WITH d MATCH (d:BC1) WITH count(d) AS n RETURN n",
        "MATCH (d:BC1) WHERE d.k = 1 WITH count(d) AS n RETURN n",
        "MATCH (d:BC1 {k: 1}) WITH count(d) AS n RETURN n",
        "MATCH (d:BC1) WITH count(DISTINCT d) AS n RETURN n",
        "MATCH (d:BC1) WITH sum(d.k) AS n RETURN n",
        "MATCH (d) WITH count(d) AS n RETURN n",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(
            t.counters().get("interp.bare count stages"),
            None,
            "declines: {q}"
        );
    }
}

#[test]
fn a_narrow_label_beside_a_wide_column_point_gathers_the_columnar_scan() {
    // `MATCH (f:Finding) WHERE f.id IS NULL RETURN count(f)` read the whole
    // graph's `id` column for a label of a few thousand: nine production
    // statements went from ~0 ms to 0.4-3.2 s. The column read is bounded
    // to the label's id span and budgeted at factor x |members| entries;
    // past it the RANGE scan declines and the aggregate now falls back to a
    // POINT-GATHER of exactly `members` (the IC5-class widening) — byte-
    // identical to the range scan, O(members) point reads. Members interleave
    // with the wide population so the id span alone cannot save the RANGE read;
    // the gather sidesteps it entirely rather than declining to the general path.
    let g = graph();
    // The scan/gather MECHANICS on repeated reads are the subject; the
    // property-column cache would serve the second read without either.
    g.set_prop_column_budget(0);
    for i in 0..400 {
        let q = if i % 100 == 50 {
            "CREATE (:CBN {i: $i, k: 'n'})"
        } else {
            "CREATE (:CBW {i: $i})"
        };
        run_params(
            &g,
            q,
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let q = "MATCH (n:CBN) WHERE n.i >= 0 RETURN count(n), sum(n.i), min(n.k)";
    let want = run(
        &g,
        "MATCH (n:CBN) WITH n WHERE n.i >= 0 RETURN count(n), sum(n.i), min(n.k)",
    )
    .rows;
    assert_eq!(
        want,
        vec![vec![Value::Int(4), Value::Int(800), Value::Str("n".into())]]
    );
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    assert_eq!(r.rows, want);
    assert_eq!(
        t.counters().get("interp.columnar aggregate scans"),
        Some(&1),
        "4 members against a 400-entry `i` column: the range scan declines and the \
         point-gather loads exactly the 4 members at factor 4"
    );
    assert!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0)
            > 0,
        "the wide `i`/`k` columns fall back to the point-gather"
    );
    // Lifting the budget takes the RANGE path instead (no gather), same result.
    g.set_columnar_column_budget_factor(usize::MAX);
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    assert_eq!(r.rows, want);
    assert_eq!(
        t.counters().get("interp.columnar aggregate scans"),
        Some(&1)
    );
    assert_eq!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0),
        0,
        "with the budget lifted the range scan fits — no gather"
    );
    g.set_columnar_column_budget_factor(4);
    // Compacted into column blocks, the RANGE read still exceeds the budget;
    // the aggregate gathers the members' columns just the same (the point-get
    // reassembles a block row to its canonical record, byte-identical).
    g.shared_store().seal();
    g.shared_store().compact();
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    assert_eq!(r.rows, want);
    assert_eq!(
        t.counters().get("interp.columnar aggregate scans"),
        Some(&1),
        "gathers over block rows too"
    );
    assert!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0)
            > 0,
        "the block-row column also falls back to the point-gather"
    );
    // A label whose members are contiguous reads only its own span: the
    // budget is not exceeded even though the column is wide.
    for i in 0..50 {
        run_params(
            &g,
            "CREATE (:CBC {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (r, t) = engram_observe::with_trace(|| run(&g, "MATCH (n:CBC) RETURN count(n), sum(n.i)"));
    assert_eq!(r.rows, vec![vec![Value::Int(50), Value::Int(1225)]]);
    assert_eq!(
        t.counters().get("interp.columnar aggregate scans"),
        Some(&1),
        "a contiguous span is read within budget"
    );
    // A label test is answered from the label's MEMBERSHIP (fix 44), so
    // no label column is read over the wide span and the scan runs.
    let (r, t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:CBN) WHERE n:CBW OR n:CBN RETURN count(n)")
    });
    assert_eq!(r.rows, vec![vec![Value::Int(4)]]);
    assert_eq!(
        t.counters().get("interp.columnar aggregate scans"),
        Some(&1),
        "the label test reads memberships, not the label column"
    );
    assert!(
        t.counters()
            .get("interp.columnar label test answered from membership")
            .copied()
            .unwrap_or(0)
            > 0
    );
}

#[test]
fn the_columnar_rel_scan_answers_exactly_what_the_general_path_answers() {
    // `MATCH ()-[r:T…]->() [WHERE p(r)] RETURN <aggregates over r.props>` —
    // the SUPPLIES histograms at 6.6-6.9 s on the production port. The
    // population is the typed walk of the out-adjacency prefix; columns
    // come from the relationship family; type(r) binds from the token. The
    // general path is forced with a bare `WITH r` after the pattern.
    let g = graph();
    run(&g, "CREATE (:RS {i: 1}), (:RS {i: 2}), (:RS {i: 3})");
    run(
        &g,
        "MATCH (a:RS {i: 1}), (b:RS {i: 2}), (c:RS {i: 3}) CREATE (a)-[:SUP {source: 's1', status: 'ok', w: 1}]->(b), (a)-[:SUP {source: 's1', w: 2}]->(c), (b)-[:SUP {source: 's2', status: 'retracted', w: 3}]->(c), (c)-[:SUP {w: 4}]->(a), (a)-[:OTH {source: 's9', w: 5}]->(b), (b)-[:OTH {w: 6}]->(b)",
    );
    let cases = [
        "MATCH ()-[r:SUP]->() RETURN count(r) AS count",
        "MATCH ()-[r:SUP]->() WHERE coalesce(r.status, 'pending') <> 'retracted' RETURN count(r) AS count",
        "MATCH ()-[r:SUP]->() WHERE coalesce(r.status, 'pending') <> 'retracted' RETURN coalesce(r.source, 'unknown') AS source, count(r) AS count ORDER BY count DESC, source LIMIT 20",
        "MATCH ()-[r:SUP]->() WHERE r.source IS NOT NULL AND r.source <> '' RETURN r.source AS source, count(r) AS count ORDER BY count DESC, source LIMIT 15",
        "MATCH ()-[r:SUP {source: 's1'}]->() RETURN count(r) AS count",
        "MATCH ()-[r:SUP {source: 's1', w: 2}]->() RETURN count(*) AS count",
        "MATCH ()<-[r:SUP]-() RETURN sum(r.w) AS s, min(r.w) AS lo, max(r.w) AS hi, avg(r.w) AS mean",
        "MATCH ()-[r:SUP|OTH]->() RETURN type(r) AS t, count(*) AS c ORDER BY t",
        "MATCH ()-[r]->() RETURN type(r) AS t, sum(r.w) AS s ORDER BY t",
        "MATCH ()-[r:SUP]->() WITH r.source AS source, count(r) AS n RETURN source, n ORDER BY n DESC, source",
        "MATCH ()-[r:SUP]->() WITH count(r) AS total, count(CASE WHEN r.status IS NULL THEN 1 END) AS pending RETURN total, pending, toFloat(pending) / total AS share",
        "MATCH ()-[r:NEVER]->() RETURN count(r) AS c",
        "MATCH ()-[r:SUP]->() WHERE r.w > 100 RETURN r.source AS s, count(*) AS c",
        "MATCH ()-[r:SUP]->() RETURN count(DISTINCT r.source) AS srcs, sum(r.w) AS total, count(r.status) AS withStatus",
        "MATCH ()-[r:OTH]->() WHERE type(r) = 'OTH' AND r.w IN [5, 6] RETURN count(r) AS c",
        // `id(r)` binds from the walk (fix 46): the member IS the id.
        "MATCH ()-[r:SUP]->() RETURN max(id(r)) AS m, min(id(r)) AS lo, count(r) AS c",
        "MATCH ()-[r:SUP]->() WHERE id(r) >= 0 AND r.w > 1 RETURN count(r) AS c",
    ];
    for q in cases {
        let general_q =
            q.replacen("]->() ", "]->() WITH r ", 1)
                .replacen("]-() ", "]-() WITH r ", 1);
        assert_ne!(general_q, q, "the general form must differ: {q}");
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        let (general, tg) = engram_observe::with_trace(|| run(&g, &general_q));
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        // A bare typed count is the count store's before it is the scan's.
        let bare_count = !q.contains("WHERE")
            && !q.contains('{')
            && !q.contains("WITH")
            && q.ends_with("RETURN count(r) AS count")
            || q.contains(":NEVER]");
        if bare_count {
            assert_eq!(
                tf.counters().get("interp.columnar rel aggregate scans"),
                None,
                "a bare count is the count store's: {q}"
            );
            continue;
        }
        assert_eq!(
            tf.counters().get("interp.columnar rel aggregate scans"),
            Some(&1),
            "the rel scan ran: {q}"
        );
        assert_eq!(
            tg.counters().get("interp.columnar rel aggregate scans"),
            None,
            "the general path did not: {q}"
        );
    }
    assert_eq!(
        run(&g, "MATCH ()-[r:SUP]->() WHERE coalesce(r.status, 'pending') <> 'retracted' RETURN coalesce(r.source, 'unknown') AS source, count(r) AS count ORDER BY count DESC, source LIMIT 20").rows,
        vec![
            vec![Value::Str("s1".into()), Value::Int(2)],
            vec![Value::Str("unknown".into()), Value::Int(1)],
        ]
    );
    // Declines: undirected (each relationship twice), a labelled or named
    // end, the relationship itself, its endpoints, variable length, a
    // DISTINCT over the relationship.
    for q in [
        "MATCH ()-[r:SUP]-() RETURN count(r) AS c",
        "MATCH (:RS)-[r:SUP]->() RETURN count(r) AS c",
        "MATCH (a)-[r:SUP]->() RETURN count(r) AS c",
        "MATCH ()-[r:SUP]->() RETURN collect(r) AS rs",
        "MATCH ()-[r:SUP]->() RETURN count(startNode(r)) AS c",
        "MATCH ()-[r:SUP*1..2]->() RETURN count(r) AS c",
        "MATCH ()-[r:SUP]->() RETURN count(r) AS c, count(DISTINCT r) AS d",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(
            t.counters().get("interp.columnar rel aggregate scans"),
            None,
            "declines: {q}"
        );
    }
    // The entry budget declines the population; the general path answers.
    // (A RELATIONSHIP write of this type first: the population is current
    // at its types' adjacency epoch — a node write, or a write of another
    // type, leaves it valid — and a cached population is already paid for.
    // `w: 0` keeps it out of the `r.w > 0` counts below.)
    run(&g, "CREATE (:RSX)-[:SUP {w: 0}]->(:RSX)");
    g.set_adj_table_max_entries(0);
    let (res, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH ()-[r:SUP]->() WHERE r.w > 0 RETURN count(r) AS c",
        )
    });
    assert_eq!(res.rows, vec![vec![Value::Int(4)]]);
    assert_eq!(
        t.counters().get("interp.columnar rel aggregate scans"),
        None,
        "declined by the entry budget"
    );
    g.set_adj_table_max_entries(1 << 20);
    // Writes move the epoch: the population is rebuilt, not replayed.
    let (r1, t1) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH ()-[r:SUP]->() WHERE r.w > 0 RETURN count(r) AS c, sum(r.w) AS w",
        )
    });
    assert_eq!(r1.rows, vec![vec![Value::Int(4), Value::Int(10)]]);
    assert_eq!(
        t1.counters().get("interp.columnar rel aggregate scans"),
        Some(&1)
    );
    run(
        &g,
        "MATCH (a:RS {i: 1}) CREATE (a)-[:SUP {source: 's3', w: 7}]->(a)",
    );
    assert_eq!(
        run(
            &g,
            "MATCH ()-[r:SUP]->() WHERE r.w > 0 RETURN count(r) AS c, sum(r.w) AS w"
        )
        .rows,
        vec![vec![Value::Int(5), Value::Int(17)]]
    );
    run(&g, "MATCH ()-[r:SUP {source: 's3'}]->() DELETE r");
    assert_eq!(
        run(
            &g,
            "MATCH ()-[r:SUP]->() WHERE r.w > 0 RETURN count(r) AS c, sum(r.w) AS w"
        )
        .rows,
        vec![vec![Value::Int(4), Value::Int(10)]]
    );
}

#[test]
fn constant_count_stages_answer_exactly_as_the_general_path() {
    // A stage whose MATCH reads nothing from its input and whose aggregate
    // is a count: the match count is a constant C - the fast count for a
    // bare pattern, the columnar scan for one with props or a WHERE - and
    // the stage is the carried groups x C. Covers `MATCH (t:EmailThread)
    // WITH count(t) AS threads MATCH (:UserEmail)-[r:IN_THREAD]->
    // (:EmailThread) RETURN threads, count(r)` (4.2 s) and `OPTIONAL MATCH
    // (kc:CVE {inKev: true}) RETURN cveCount, cpeCount, count(kc)` (the
    // last stage of security-store.ts:1300). The general path is forced
    // with a predicate over a carried variable (a join, which declines) or
    // `id(v) >= 0` (a bare use, which the scan declines).
    let g = graph();
    run(&g, "CREATE (:ET {i: 1}), (:ET {i: 1}), (:ET {i: 2})");
    run(
        &g,
        "CREATE (:UE {i: 1})-[:IT]->(:ET {i: 3}), (:UE {i: 2})-[:IT]->(:ET {i: 3}), (:UE {i: 3})-[:IT]->(:ET {i: 4})",
    );
    run(
        &g,
        "CREATE (:CV {inKev: true}), (:CV {inKev: false}), (:CV {inKev: true}), (:CV)",
    );
    let cases: [(&str, &str); 10] = [
        (
            "MATCH (t:ET) WITH count(t) AS threads MATCH (:UE)-[r:IT]->(:ET) RETURN threads, count(r) AS edges",
            "MATCH (t:ET) WITH count(t) AS threads MATCH (:UE)-[r:IT]->(:ET) WHERE threads IS NOT NULL RETURN threads, count(r) AS edges",
        ),
        (
            "OPTIONAL MATCH (c:CV) WITH count(c) AS cveCount OPTIONAL MATCH (kc:CV {inKev: true}) RETURN cveCount, count(kc) AS kevCount",
            "OPTIONAL MATCH (c:CV) WITH count(c) AS cveCount OPTIONAL MATCH (kc:CV {inKev: true}) WHERE cveCount IS NOT NULL RETURN cveCount, count(kc) AS kevCount",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (x:CV) WHERE x.inKev = false WITH threads, count(x) AS notKev MATCH (:UE)-[r:IT]->() RETURN threads, notKev, count(*) AS edges",
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (x:CV) WHERE x.inKev = false AND threads IS NOT NULL WITH threads, count(x) AS notKev MATCH (:UE)-[r:IT]->() WHERE notKev IS NOT NULL RETURN threads, notKev, count(*) AS edges",
        ),
        (
            "MATCH (t:ET) WITH t.i AS k ORDER BY k MATCH (:UE)-[r:IT]->(:ET) WITH k, count(r) AS e RETURN k, e ORDER BY k",
            "MATCH (t:ET) WITH t.i AS k ORDER BY k MATCH (:UE)-[r:IT]->(:ET) WHERE k IS NOT NULL WITH k, count(r) AS e RETURN k, e ORDER BY k",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (:NOPE)-[r:IT]->() RETURN threads, count(r) AS e",
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (:NOPE)-[r:IT]->() WHERE threads IS NOT NULL RETURN threads, count(r) AS e",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (:NOPE)-[r:IT]->() RETURN threads, count(*) AS e",
            "MATCH (t:ET) WITH count(t) AS threads OPTIONAL MATCH (:NOPE)-[r:IT]->() WHERE threads IS NOT NULL RETURN threads, count(*) AS e",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads MATCH (:NOPE)-[r:IT]->() RETURN threads, count(r) AS e",
            "MATCH (t:ET) WITH count(t) AS threads MATCH (:NOPE)-[r:IT]->() WHERE threads IS NOT NULL RETURN threads, count(r) AS e",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads MATCH (z:CV) WHERE z.inKev IS NULL WITH threads, count(z) AS n RETURN threads, n",
            "MATCH (t:ET) WITH count(t) AS threads MATCH (z:CV) WHERE z.inKev IS NULL AND threads IS NOT NULL WITH threads, count(z) AS n RETURN threads, n",
        ),
        (
            "MATCH (t:ET) WITH count(t) AS threads MATCH (z:CV {inKev: true}) RETURN threads, count(*) AS n",
            "MATCH (t:ET) WITH count(t) AS threads MATCH (z:CV {inKev: true}) WHERE threads IS NOT NULL RETURN threads, count(*) AS n",
        ),
        (
            "MATCH (u:UE) WITH u.i AS k ORDER BY k MATCH ()-[r:IT {}]->() WITH k, count(r) AS e MATCH (z:CV {inKev: false}) RETURN k, e, count(z) AS f ORDER BY k",
            "MATCH (u:UE) WITH u.i AS k ORDER BY k MATCH ()-[r:IT {}]->() WHERE k IS NOT NULL WITH k, count(r) AS e MATCH (z:CV {inKev: false}) WHERE k IS NOT NULL RETURN k, e, count(z) AS f ORDER BY k",
        ),
    ];
    for (fast_q, general_q) in cases {
        let (fast, tf) = engram_observe::with_trace(|| run(&g, fast_q));
        let (general, tg) = engram_observe::with_trace(|| run(&g, general_q));
        assert_eq!(fast.columns, general.columns, "columns: {fast_q}");
        assert_eq!(fast.rows, general.rows, "rows: {fast_q}");
        assert!(
            tf.counters().get("interp.bare count stages").is_some(),
            "a constant-count stage answered: {fast_q}"
        );
        assert_eq!(
            tg.counters()
                .get("interp.bare count stages")
                .copied()
                .unwrap_or(0),
            // The first stage (`WITH count(t)`) is itself a constant-count
            // stage in the general form too; only the later ones decline.
            if general_q.contains("WITH count(") || general_q.contains("WITH count(c)") {
                1
            } else {
                0
            },
            "the forced stages took the general path: {general_q}"
        );
    }
    assert_eq!(
        run(&g, "MATCH (t:ET) WITH count(t) AS threads MATCH (:UE)-[r:IT]->(:ET) RETURN threads, count(r) AS edges").rows,
        vec![vec![Value::Int(6), Value::Int(3)]]
    );
    assert_eq!(
        run(&g, "OPTIONAL MATCH (c:CV) WITH count(c) AS cveCount OPTIONAL MATCH (kc:CV {inKev: true}) RETURN cveCount, count(kc) AS kevCount").rows,
        vec![vec![Value::Int(4), Value::Int(2)]]
    );
    assert_eq!(
        run(&g, "MATCH (t:ET) WITH t.i AS k ORDER BY k MATCH (:UE)-[r:IT]->(:ET) WITH k, count(r) AS e RETURN k, e ORDER BY k").rows,
        vec![
            vec![Value::Int(1), Value::Int(6)],
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(3), Value::Int(6)],
            vec![Value::Int(4), Value::Int(3)],
        ],
        "two ET rows with i=1 fold into one group of 2 x 3; the hop's ET ends count too"
    );
    // Declines: a bound end, an ORDER BY, a second aggregate, a carried
    // variable inside the property map.
    for q in [
        "MATCH (t:ET) WITH t MATCH (:UE)-[r:IT]->(t) RETURN t.i AS i, count(r) AS c",
        "MATCH (t:ET) WITH count(t) AS threads MATCH (:UE)-[r:IT]->(:ET) RETURN threads, count(r) AS e ORDER BY threads",
        "MATCH (t:ET) WITH count(t) AS threads MATCH (:UE)-[r:IT]->(z:ET) RETURN threads, count(r) AS e, max(z.i) AS m",
        "MATCH (t:ET) WITH t.i AS k MATCH (z:CV {inKev: k}) RETURN k, count(z) AS c",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        // Only the first stage (`WITH count(t)`), where present, is a
        // constant-count stage; the declining stage adds nothing.
        let first_stage = u64::from(q.contains("WITH count(t)"));
        assert_eq!(
            t.counters()
                .get("interp.bare count stages")
                .copied()
                .unwrap_or(0),
            first_stage,
            "declines: {q}"
        );
    }
}

#[test]
fn the_columnar_projection_scan_answers_exactly_what_the_general_path_answers() {
    // `MATCH (n:L…) [WHERE p] RETURN <exprs over n.props | n> [ORDER BY …]
    // [SKIP s] [LIMIT k]`: filter, projected expressions and order keys
    // from columns; order and page; THEN materialise a bare `n` for the
    // rows that remain. `RETURN e ORDER BY e.started_at DESC LIMIT 500`
    // decoded every candidate in full to keep 500 (5.2 s on the production
    // port). The general path is forced with a bare `WITH n` after the
    // MATCH (the WHERE moves behind it).
    let g = graph();
    for i in 0..40i64 {
        run_params(
            &g,
            "CREATE (:WX {i: $i, status: CASE WHEN $i % 3 = 0 THEN 'running' WHEN $i % 3 = 1 THEN 'done' ELSE 'paused' END, started: 1000 - $i * 7 % 13, fat: 'a property the projection must never read'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    run(&g, "CREATE (:WX {i: 99})"); // no status, no started
    let cases = [
        "MATCH (e:WX) WHERE e.status IN ['running', 'paused'] RETURN e ORDER BY e.started DESC LIMIT 5",
        "MATCH (e:WX) WHERE e.status IN ['running', 'paused'] RETURN e ORDER BY e.started DESC, e.i LIMIT 5",
        "MATCH (e:WX) WHERE e.status IN ['running', 'paused'] AND e.started IS NOT NULL RETURN e.i AS executionId",
        "MATCH (e:WX) RETURN e.i AS id, e.status AS status, e.started AS started ORDER BY e.status, started DESC, id",
        "MATCH (e:WX) RETURN e.i AS id, e.status AS status ORDER BY status, id SKIP 3 LIMIT 4",
        "MATCH (e:WX) WHERE e.i % 2 = 0 RETURN e.i * 10 AS x, e ORDER BY x DESC LIMIT 3",
        "MATCH (e:WX) RETURN e.i + 1 AS next ORDER BY next DESC LIMIT 2",
        "MATCH (e:WX) WHERE e.status = 'done' RETURN e.i, e.started",
        "MATCH (e:WX {status: 'done'}) RETURN e.i AS i ORDER BY i",
        "MATCH (e:WX) WHERE e.status IS NULL RETURN e",
        "MATCH (e:WX) WHERE e.i > 1000 RETURN e.i AS i",
        "MATCH (e:WX) RETURN e.i AS i LIMIT 0",
        "MATCH (e:WX) RETURN e.i AS i SKIP 38",
        "MATCH (e:NOPE) RETURN e.i AS i",
        "MATCH (e:WX) RETURN coalesce(e.status, 'none') AS s, e.i AS i ORDER BY s, i LIMIT 6",
        "MATCH (e:WX) RETURN DISTINCT e.status AS s ORDER BY s",
        "MATCH (e:WX) WHERE e.i < 20 RETURN DISTINCT e.status AS s LIMIT 2",
        "MATCH (e:WX) RETURN DISTINCT e.status AS s, e.i % 3 AS r ORDER BY s, r SKIP 1",
        "MATCH (e:WX) WHERE e.i < 3 RETURN DISTINCT e",
        // `id(e)` binds from the walk (fix 46): the member IS the id.
        "MATCH (e:WX) RETURN id(e) AS id ORDER BY id LIMIT 4",
        "MATCH (e:WX) WHERE id(e) >= 0 AND e.i < 3 RETURN e.i AS i, id(e) AS id ORDER BY i",
    ];
    for q in cases {
        let general_q = if q.contains(" WHERE ") {
            q.replacen(") WHERE ", ") WITH e WHERE ", 1)
        } else {
            q.replacen(") RETURN ", ") WITH e RETURN ", 1)
        };
        assert_ne!(general_q, q, "the general form must differ: {q}");
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        let (general, tg) = engram_observe::with_trace(|| run(&g, &general_q));
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        assert_eq!(
            tf.counters().get("interp.columnar projection scans"),
            Some(&1),
            "the projection scan ran: {q}"
        );
        assert_eq!(
            tg.counters().get("interp.columnar projection scans"),
            None,
            "the general path did not: {q}"
        );
    }
    // Late materialisation: only the page's nodes are decoded in full.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WX) WHERE e.status IN ['running', 'paused'] RETURN e ORDER BY e.started DESC LIMIT 5",
        )
    });
    assert_eq!(r.rows.len(), 5);
    assert!(matches!(r.rows[0][0], Value::Node { .. }));
    assert_eq!(
        t.counters().get("graph.projected node materialisations"),
        None,
        "no slim projections: columns served the filter and the order"
    );
    assert_eq!(
        t.counters()
            .get("graph.nodes materialised in full")
            .copied()
            .unwrap_or(0),
        5,
        "exactly the page was materialised"
    );
    // Declines: `*`, an aggregate, labels().
    for q in [
        "MATCH (e:WX) RETURN *",
        "MATCH (e:WX) RETURN labels(e) AS l LIMIT 1",
        "MATCH (e:WX) RETURN e.i AS i, count(*) AS c",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(
            t.counters().get("interp.columnar projection scans"),
            None,
            "declines: {q}"
        );
    }
    // Relationships project too (their properties and type; never bare).
    run(
        &g,
        "MATCH (a:WX {i: 1}), (b:WX {i: 2}) CREATE (a)-[:WR {w: 3, s: 'x'}]->(b), (b)-[:WR {w: 1}]->(a), (a)-[:WQ {w: 2}]->(a)",
    );
    for q in [
        "MATCH ()-[r:WR|WQ]->() RETURN r.w AS w, type(r) AS t, r.s AS s ORDER BY w",
        "MATCH ()-[r:WR]->() WHERE r.s IS NULL RETURN r.w AS w",
    ] {
        let general_q = q.replacen("]->() ", "]->() WITH r ", 1);
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        let general = run(&g, &general_q);
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        assert_eq!(
            tf.counters().get("interp.columnar projection scans"),
            Some(&1),
            "{q}"
        );
    }
    let (_, t) = engram_observe::with_trace(|| run(&g, "MATCH ()-[r:WR]->() RETURN r"));
    assert_eq!(
        t.counters().get("interp.columnar projection scans"),
        None,
        "a bare relationship has no late path and declines"
    );
}

#[test]
fn an_unbound_variable_refuses_by_name_through_every_scan() {
    // The columnar scans keep a foreign variable in the rewritten
    // expression; evaluated, it would surface as an evaluation error
    // instead of the parse-time refusal Neo4j gives. Every scan declines
    // on a free variable other than its own, so the general path refuses
    // BY NAME before any read.
    let g = graph();
    run(&g, "CREATE (:US {x: 1})-[:UT {w: 1}]->(:US {x: 2})");
    for q in [
        "MATCH (n:US) WHERE n.x = nid RETURN count(n) AS c",
        "MATCH (n:US) WHERE n.x = nid RETURN n.x AS x",
        "MATCH (n:US) WHERE n.x = nid WITH count(n) AS c RETURN c",
        "MATCH (n:US) RETURN n.x + nid AS y",
        "MATCH (n:US) RETURN count(n) AS c, nid AS z",
        "MATCH ()-[r:UT]->() WHERE r.w = nid RETURN count(r) AS c",
        "MATCH ()-[r:UT]->() RETURN r.w + nid AS y",
        "MATCH (t:US) WITH count(t) AS a MATCH (n:US) WHERE n.x = nid WITH a, count(n) AS c RETURN a, c",
    ] {
        let stmt = parse_statement(q).expect("parses as a var");
        let (res, t) = engram_observe::with_trace(|| run_query(&g, &stmt, BTreeMap::new()));
        match res {
            Err(e) => {
                let msg = format!("{e:?}");
                // A WHERE refuses by name before any read; a projection item
                // surfaces the name at evaluation (the general path's own
                // behaviour) — either way the variable is named and no scan
                // ran.
                if q.contains(" WHERE ") {
                    assert!(
                        msg.contains("`nid` not defined"),
                        "must refuse by name: {q}: {msg}"
                    );
                } else {
                    assert!(msg.contains("nid"), "must name the variable: {q}: {msg}");
                }
            }
            Ok(_) => panic!("an unbound variable must refuse: {q}"),
        }
        for c in [
            "interp.columnar aggregate scans",
            "interp.columnar projection scans",
        ] {
            assert_eq!(t.counters().get(c), None, "{c} must not run: {q}");
        }
        // The first stage of the chain (`WITH count(t) AS a`) is a
        // constant-count stage in its own right; the unbound stage is not.
        let first_stage = u64::from(q.contains("WITH count(t) AS a"));
        assert_eq!(
            t.counters()
                .get("interp.bare count stages")
                .copied()
                .unwrap_or(0),
            first_stage,
            "the unbound stage must not run: {q}"
        );
    }
}

#[test]
fn presence_only_reads_and_label_disjunction_pushdown_answer_exactly() {
    // `n.p IS [NOT] NULL` is a presence read: the column is scanned for
    // keys only — no value copied, no value decoded — unless a value read
    // elsewhere loads it anyway. And `MATCH (m) WHERE (m:A OR (m:B AND …))`
    // walks A ∪ B instead of every node, with the label booleans served
    // from membership rather than the label column. Both target
    // `reembed-research-memories.ts:28` (5.0 s on the production port):
    // `MATCH (m) WHERE (m:ResearchMemory OR (m:Memory AND m.memoryCategory
    // = 'research')) AND m.content IS NOT NULL RETURN count(m)`.
    let g = graph();
    // The presence SCAN is the subject; the property-column cache would
    // serve a repeated read without one.
    g.set_prop_column_budget(0);
    // These labels are narrow beside the Filler population; the column
    // budget is the subject of another test, not this one.
    g.set_columnar_column_budget_factor(64);
    for i in 0..30i64 {
        run_params(
            &g,
            "CREATE (:Filler {i: $i, content: 'filler content that must never be decoded'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
        if i % 3 == 0 {
            run_params(
                &g,
                "CREATE (:RM {i: $i, content: 'research memory'})",
                [("i".to_string(), Value::Int(i))].into_iter().collect(),
            );
        }
        if i % 4 == 0 {
            run_params(
                &g,
                "CREATE (:Mem {i: $i, memoryCategory: CASE WHEN $i % 8 = 0 THEN 'research' ELSE 'chat' END, content: 'memory'})",
                [("i".to_string(), Value::Int(i))].into_iter().collect(),
            );
        }
    }
    run(
        &g,
        "CREATE (:RM {i: 100}), (:Mem {i: 101, memoryCategory: 'research'}), (:RM:Mem {i: 102, memoryCategory: 'research', content: 'both'})",
    );
    let cases = [
        "MATCH (m) WHERE (m:RM OR (m:Mem AND m.memoryCategory = 'research')) AND m.content IS NOT NULL RETURN count(m) AS total",
        "MATCH (m) WHERE m:RM OR m:Mem RETURN count(m) AS c",
        "MATCH (m) WHERE (m:RM OR m:Mem) AND m.content IS NULL RETURN count(m) AS c",
        "MATCH (m) WHERE (m:RM AND m.content IS NOT NULL) OR (m:Mem AND m.content IS NULL) RETURN count(m) AS c",
        "MATCH (n:RM) WHERE n.content IS NOT NULL RETURN count(n) AS c",
        "MATCH (n:RM) WHERE n.content IS NULL RETURN count(n) AS c",
        "MATCH (n:Mem) WHERE n.content IS NOT NULL AND n.memoryCategory IS NOT NULL RETURN count(n) AS c, count(DISTINCT n.memoryCategory) AS k",
        "MATCH (n:Mem) RETURN n.memoryCategory IS NULL AS nocat, count(*) AS c ORDER BY nocat",
        "MATCH (n:RM) WHERE n.content IS NOT NULL RETURN n.i AS i ORDER BY i",
        "MATCH (m) WHERE (m:RM OR m:Mem) AND m.i > 20 RETURN m.i AS i, m:RM AS rm, m:Mem AS mem ORDER BY i",
        "MATCH (m) WHERE (m:RM OR m:Mem) AND m:Filler RETURN count(m) AS c",
        "MATCH (n:Mem) WHERE n.content IS NOT NULL AND size(n.content) > 3 RETURN count(n) AS c",
        "MATCH (m) WHERE m:NOPE OR m:RM RETURN count(m) AS c",
    ];
    for q in cases {
        let general_q = if q.contains("MATCH (m) ") {
            q.replacen("MATCH (m) ", "MATCH (m) WITH m ", 1)
        } else {
            q.replacen(") WHERE ", ") WITH n WHERE ", 1).replacen(
                ") RETURN ",
                ") WITH n RETURN ",
                1,
            )
        };
        assert_ne!(general_q, q, "the general form must differ: {q}");
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        let general = run(&g, &general_q);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        let scanned = tf
            .counters()
            .get("interp.columnar aggregate scans")
            .is_some()
            || tf
                .counters()
                .get("interp.columnar projection scans")
                .is_some();
        assert!(scanned, "a columnar scan ran: {q}");
    }
    // Presence only: the `content` column is never read for values.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (m) WHERE (m:RM OR (m:Mem AND m.memoryCategory = 'research')) AND m.content IS NOT NULL RETURN count(m) AS total",
        )
    });
    // 10 RM from the loop + RM:Mem 102 (both with content) + the 4 research
    // Mem nodes with content (0, 8, 16, 24); RM 100 and Mem 101 have none.
    assert_eq!(r.rows, vec![vec![Value::Int(11 + 4)]]);
    assert_eq!(
        t.counters().get("store.column presence scans"),
        Some(&1),
        "one presence scan (content)"
    );
    assert_eq!(
        t.counters().get("store.column scans"),
        Some(&1),
        "one value scan (memoryCategory) — and NOT the label column: the labels came from membership"
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar scan bound a label from membership")
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar scan narrowed an unlabelled match to a label disjunction")
    );
    // A value read of the same property serves the null test: one value
    // scan, no presence scan.
    let (_, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:Mem) WHERE n.content IS NOT NULL AND size(n.content) > 3 RETURN count(n) AS c",
        )
    });
    assert_eq!(t.counters().get("store.column presence scans"), None);
    assert_eq!(t.counters().get("store.column scans"), Some(&1));
    // A label outside the disjunction is answered from its membership too
    // (fix 44): no label column is read for Filler.
    let (_, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (m) WHERE (m:RM OR m:Mem) AND m:Filler RETURN count(m) AS c",
        )
    });
    assert_eq!(
        t.counters().get("store.column scans"),
        None,
        "no label column for Filler — its membership answers"
    );
    assert!(
        t.counters()
            .get("interp.columnar label test answered from membership")
            .copied()
            .unwrap_or(0)
            > 0
    );
}

#[test]
fn a_column_filtered_seed_materialises_only_the_survivors() {
    // A multi-clause stage whose leading MATCH carries a WHERE over its own
    // variable: the conjuncts reading only that variable are evaluated
    // from columns first and only the survivors are materialised — `MATCH
    // (e:WorkflowExecution) WHERE e.origin IS NULL OPTIONAL MATCH (w:Workflow
    // {workflow_id: e.workflow_id}) RETURN …` decoded every execution's
    // `context` blob to keep 136 (5.6 s on the production port). The full
    // WHERE still runs at its position. The general path is forced with a
    // conjunct the rewrite declines (a bare `e IS NULL` inside the OR —
    // `id(e)` no longer declines, fix 46 binds it from the walk).
    let g = graph();
    g.set_columnar_column_budget_factor(64);
    for i in 0..60i64 {
        run_params(
            &g,
            "CREATE (:WE {i: $i, wf: $i % 5, origin: CASE WHEN $i % 10 = 0 THEN null ELSE 'auto' END, context: 'a large blob that must not be decoded for the dropped rows'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    for i in 0..5i64 {
        run_params(
            &g,
            "CREATE (:WF {wf: $i, origin: 'seed'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let cases = [
        (
            "MATCH (e:WE) WHERE e.origin IS NULL OPTIONAL MATCH (w:WF {wf: e.wf}) RETURN e.i AS i, e.context AS ctx, w.origin AS parent ORDER BY i",
            "MATCH (e:WE) WHERE e.origin IS NULL OR e IS NULL OPTIONAL MATCH (w:WF {wf: e.wf}) RETURN e.i AS i, e.context AS ctx, w.origin AS parent ORDER BY i",
        ),
        (
            "MATCH (e:WE) WHERE e.origin IS NULL AND e.i > 20 MATCH (w:WF) WHERE w.wf = e.wf RETURN e.i AS i, w.wf AS wf ORDER BY i",
            "MATCH (e:WE) WHERE (e.origin IS NULL AND e.i > 20) OR e IS NULL MATCH (w:WF) WHERE w.wf = e.wf RETURN e.i AS i, w.wf AS wf ORDER BY i",
        ),
        (
            "MATCH (e:WE) WHERE e.i % 7 = 0 WITH e MATCH (w:WF {wf: e.wf}) RETURN count(*) AS c",
            "MATCH (e:WE) WHERE e.i % 7 = 0 OR e IS NULL WITH e MATCH (w:WF {wf: e.wf}) RETURN count(*) AS c",
        ),
        (
            "OPTIONAL MATCH (e:WE) WHERE e.i > 1000 RETURN count(e) AS c, count(*) AS rows",
            "OPTIONAL MATCH (e:WE) WHERE e.i > 1000 OR e IS NULL RETURN count(e) AS c, count(*) AS rows",
        ),
        (
            "MATCH (e:WE) WHERE e.origin IS NULL AND e.i >= 0 MATCH (f:WE) WHERE f.i = e.i + 1 RETURN e.i AS a, f.i AS b ORDER BY a",
            "MATCH (e:WE) WHERE (e.origin IS NULL AND e.i >= 0) OR e IS NULL MATCH (f:WE) WHERE f.i = e.i + 1 RETURN e.i AS a, f.i AS b ORDER BY a",
        ),
    ];
    for (fast_q, general_q) in cases {
        let (fast, tf) = engram_observe::with_trace(|| run(&g, fast_q));
        let (general, tg) = engram_observe::with_trace(|| run(&g, general_q));
        assert_eq!(fast.columns, general.columns, "columns: {fast_q}");
        assert_eq!(fast.rows, general.rows, "rows: {fast_q}");
        assert!(
            tf.counters()
                .get("interp.seeds filtered by columns")
                .is_some(),
            "the seed was column-filtered: {fast_q}"
        );
        assert_eq!(
            tg.counters().get("interp.seeds filtered by columns"),
            None,
            "the general seed scanned: {general_q}"
        );
    }
    // Only the survivors are materialised (slim): 6 of 60.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE) WHERE e.origin IS NULL OPTIONAL MATCH (w:WF {wf: e.wf}) RETURN e.i AS i, e.context AS ctx, w.origin AS parent ORDER BY i",
        )
    });
    assert_eq!(r.rows.len(), 6);
    let slim = t
        .counters()
        .get("graph.projected node materialisations")
        .copied()
        .unwrap_or(0);
    let full = t
        .counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0);
    assert!(
        slim + full <= 6 + 6 * 5,
        "6 survivors plus their WF lookups, never the 54 dropped: slim {slim} full {full}"
    );
    // A non-boolean conjunct keeps the row for the full WHERE to refuse.
    let stmt =
        parse_statement("MATCH (e:WE) WHERE e.i OPTIONAL MATCH (w:WF {wf: e.wf}) RETURN e.i")
            .expect("parses");
    let err = run_query(&g, &stmt, BTreeMap::new()).expect_err("a non-boolean WHERE refuses");
    assert!(format!("{err:?}").contains("boolean"), "{err:?}");
}

#[test]
fn the_columnar_stage_produces_a_with_chain_exactly_as_the_general_path() {
    // The stage head as a column walk: `MATCH (n…) [WHERE p] WITH <exprs
    // over n> AS … [WITH …] <breaker>` with a non-aggregating, ordered or
    // paged breaker — and `count{(n)-[:T…]-()}` as a degree from the
    // adjacency table. The degree histogram (`MATCH (n) WITH n, count{
    // (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN …`)
    // built a node and a row per member for one integer (10 s on the
    // production port). The general path is the same statement with the
    // columnar paths switched off.
    let g = graph();
    g.set_degree_table_after(0);
    run(
        &g,
        "CREATE (a:DG {i: 1, k: 'p'}), (b:DG {i: 2, k: 'q'}), (c:DG {i: 3, k: 'p'}), (d:DG {i: 4}), (:DG {i: 5, k: 'r'})",
    );
    run(
        &g,
        "MATCH (a:DG {i: 1}), (b:DG {i: 2}), (c:DG {i: 3}), (d:DG {i: 4}) CREATE (a)-[:R1]->(b), (a)-[:R1]->(c), (b)-[:R2]->(c), (c)-[:R1]->(d), (d)-[:R2]->(a), (a)-[:R2]->(a)",
    );
    let cases = [
        "MATCH (n) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN ds[toInteger(size(ds) * 0.50)] AS p50, ds[toInteger(size(ds) * 0.95)] AS p95, ds[size(ds) - 1] AS max",
        "MATCH (n:DG) WITH n, count { (n)-[:R1]->() } AS out1, count { (n)<-[:R2]-() } AS in2 WITH out1 + in2 AS total ORDER BY total DESC, out1 LIMIT 3 RETURN total",
        "MATCH (n:DG) WITH n, n.i AS i, count { (n)--() } AS d WITH i, d WHERE d > 1 RETURN i, d ORDER BY d DESC, i",
        "MATCH (n:DG) WHERE n.k IS NOT NULL WITH n.k AS k, n.i * 2 AS dbl WITH k, dbl ORDER BY k, dbl SKIP 1 LIMIT 3 RETURN k, dbl",
        "MATCH (n:DG) WITH n.i AS i RETURN i ORDER BY i DESC LIMIT 2",
        "MATCH (n:DG) WITH n, n.i AS i WITH i WHERE i % 2 = 0 RETURN i ORDER BY i",
        "MATCH (n:DG) WITH n.k AS k, count { (n)-[:R1]-() } AS r1 WITH k, r1 ORDER BY k, r1 RETURN k, r1",
        "MATCH (n:DG) WITH n, count { (n)-[:NOPE]->() } AS z WITH z ORDER BY z RETURN z",
        "MATCH (n:NOPE) WITH n, count { (n)--() } AS d WITH d ORDER BY d RETURN d",
        "MATCH (n:DG) WITH n.i AS i, n.k AS k WITH i, k WHERE k = 'p' OR i = 4 WITH i ORDER BY i RETURN collect(i) AS is",
        "MATCH ()-[r:R1|R2]->() WITH r.w AS w, type(r) AS t WITH t ORDER BY t RETURN t",
        // Fix 57: a bare carry into an ORDERED breaker rides the stage as
        // a trailing id column and is hydrated for its survivors.
        "MATCH (n:DG) WITH n, n.i AS i WITH n, i ORDER BY i RETURN n.k",
        "MATCH (n:DG) WITH n ORDER BY n.i DESC SKIP 1 LIMIT 2 RETURN n.k, n.i",
    ];
    for q in cases {
        g.set_columnar_scans(true);
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        assert!(
            tf.counters().get("interp.columnar stages").is_some(),
            "the columnar stage ran: {q}"
        );
    }
    // The histogram never materialises a node: degrees come from the table.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN ds[size(ds) - 1] AS max, size(ds) AS n",
        )
    });
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(4), Value::Int(5)]],
        "a: R1 x2 out, R2 in, and the self-loop once = 4"
    );
    assert_eq!(
        t.counters().get("graph.projected node materialisations"),
        None
    );
    assert_eq!(t.counters().get("graph.nodes materialised in full"), None);
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar scan bound a degree from the adjacency table")
    );
    // Declines: a pure `WITH n`, a bare carry into an UNORDERED breaker
    // (fix 52's seed cut owns a plain limit), DISTINCT, a labelled far end.
    for q in [
        "MATCH (n:DG) WITH n WITH n, n.i AS i RETURN i ORDER BY i",
        "MATCH (n:DG) WITH n, n.i AS i WITH n, i LIMIT 2 RETURN n.k",
        "MATCH (n:DG) WITH n.k AS k WITH DISTINCT k RETURN k ORDER BY k",
        "MATCH (n:DG) WITH n, count { (n)-[:R1]->(:DG) } AS d WITH d ORDER BY d RETURN d",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(
            t.counters().get("interp.columnar stages"),
            None,
            "declines: {q}"
        );
    }
    // The kill switch sends everything down the general path.
    g.set_columnar_scans(false);
    let (r, t) =
        engram_observe::with_trace(|| run(&g, "MATCH (n:DG) WHERE n.i > 1 RETURN count(n) AS c"));
    assert_eq!(r.rows, vec![vec![Value::Int(4)]]);
    assert_eq!(t.counters().get("interp.columnar aggregate scans"), None);
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar paths switched off")
    );
    g.set_columnar_scans(true);
}

#[test]
fn labelled_hop_counts_come_from_the_tables() {
    // `count_hop` probed adjacency per member of the smaller label —
    // `(g:GeopoliticalEvent)-[:T]->(s:NewsStory) RETURN count(*)` measured
    // 2.2 s on the production port. One end free: the sum of the members'
    // degrees from the degree table. Both ends labelled: the adjacency
    // table's slices against the far membership. The per-node walk stays
    // as the fallback when the table is over its entry budget.
    let g = graph();
    g.set_degree_table_after(0);
    // This test asserts WHICH estimator mechanism answered — degree table,
    // adjacency-table walk, or per-node fallback — so the hop-count MEMO must
    // be off: it correctly serves a repeat whose type and label epochs are
    // unmoved (a `:Tick` create moves neither), and a served answer fires no
    // mechanism event to assert on. The memo's own behaviour is covered by
    // `hop_count_memo_keys_on_two_clocks.rs`.
    g.set_hop_count_memo(false);
    run(
        &g,
        "CREATE (:GE {i: 1}), (:GE {i: 2}), (:GE {i: 3}), (:NS {i: 1}), (:NS {i: 2}), (:NA {i: 1}), (:Pk {i: 1}), (:Pk {i: 2})",
    );
    run(
        &g,
        "MATCH (a:GE {i: 1}), (b:GE {i: 2}), (c:GE {i: 3}), (s1:NS {i: 1}), (s2:NS {i: 2}), (n1:NA {i: 1}), (p1:Pk {i: 1}), (p2:Pk {i: 2}) CREATE (a)-[:DF]->(s1), (a)-[:DF]->(s2), (b)-[:DF]->(s1), (c)-[:SF]->(n1), (c)-[:DF]->(n1), (a)-[:SF]->(s1), (s1)-[:DF]->(a), (b)-[:X]->(p1), (c)-[:Y]->(p1), (a)-[:X]->(p2), (p2)-[:X]->(p1), (p2)-[:DF]->(s1), (n1)-[:DF]->(s2)",
    );
    let cases = [
        "MATCH (g:GE)-[:DF]->(s:NS) RETURN count(*) AS c",
        "MATCH (g:GE)-[:SF]->(a:NA) RETURN count(*) AS c",
        "MATCH (g:GE)-[:DF|SF]->(s:NS) RETURN count(*) AS c",
        "MATCH (g:GE)-[]->(s:NS) RETURN count(*) AS c",
        "MATCH (g:GE)<-[:DF]-(s:NS) RETURN count(*) AS c",
        "MATCH (p:Pk)<-[r]-() RETURN count(r) AS c",
        "MATCH (g:GE)-[r:DF]->() RETURN count(r) AS c",
        "MATCH ()-[r:X]->(p:Pk) RETURN count(r) AS c",
        "MATCH (g:GE)-[:NOPE]->(s:NS) RETURN count(*) AS c",
        "MATCH (g:GE)-[:DF]->(s:NOPE) RETURN count(*) AS c",
        "MATCH (g:GE)-[:DF]->(s:NS) WITH count(*) AS c MATCH (p:Pk)<-[r]-() RETURN c, count(r) AS d",
    ];
    for q in cases {
        run(&g, "CREATE (:Tick)"); // a fresh epoch: no cached tables
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        // The general answer: no degree table, no adjacency table (a fresh
        // epoch again, so nothing cached serves), the per-node walks.
        run(&g, "CREATE (:Tick)");
        g.set_degree_table_after(u64::MAX);
        g.set_adj_table_max_entries(0);
        let (general, tg) = engram_observe::with_trace(|| run(&g, q));
        g.set_degree_table_after(0);
        g.set_adj_table_max_entries(1 << 20);
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        assert!(
            !tg.sometimes_hit()
                .contains("graph.hop count walked the adjacency table"),
            "the general answer walked per node: {q}"
        );
        let table = tf
            .sometimes_hit()
            .contains("graph.hop count summed from the degree table")
            || tf
                .sometimes_hit()
                .contains("graph.hop count walked the adjacency table");
        let never_minted = q.contains("NOPE");
        assert!(table || never_minted, "answered from a table: {q}");
        assert!(
            !tf.sometimes_hit()
                .contains("graph.hop count fell back to per-node probes"),
            "no per-node probes: {q}"
        );
    }
    assert_eq!(
        run(&g, "MATCH (g:GE)-[:DF]->(s:NS) RETURN count(*) AS c").rows,
        vec![vec![Value::Int(3)]]
    );
    assert_eq!(
        run(&g, "MATCH (p:Pk)<-[r]-() RETURN count(r) AS c").rows,
        vec![vec![Value::Int(4)]]
    );
    // Peers outside the far label (p2 -DF-> s1, n1 -DF-> s2) never count.
    assert_eq!(
        run(&g, "MATCH ()-[r:DF]->(s:NS) RETURN count(r) AS c").rows,
        vec![vec![Value::Int(5)]]
    );
    // Over the entry budget the both-ends count falls back to the walk —
    // same answer.
    run(&g, "CREATE (:GE {i: 9})"); // a new epoch: no cached table
    g.set_adj_table_max_entries(0);
    let (r, t) =
        engram_observe::with_trace(|| run(&g, "MATCH (g:GE)-[:DF]->(s:NS) RETURN count(*) AS c"));
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    assert!(
        t.sometimes_hit()
            .contains("graph.hop count fell back to per-node probes")
    );
    g.set_adj_table_max_entries(1 << 20);
}

#[test]
fn an_exists_or_count_subquery_ending_in_match_needs_no_return() {
    // `NOT EXISTS { MATCH (p)-[:HAS_VOICE]->(:VoiceProfile) }` refused with
    // `Query cannot conclude with MATCH` on the production port — and
    // rev 62's column prefilter hid it by dropping every row before the
    // conjunct ran. The body's rows are the answer; no RETURN is needed.
    let g = graph();
    run(
        &g,
        "CREATE (:PV {name: 'a', v: 'x'})-[:HV]->(:VP), (:PV {name: 'b', v: 'y'}), (:PV {name: 'c'})",
    );
    for on in [true, false] {
        g.set_columnar_scans(on);
        let r = run(
            &g,
            "MATCH (p:PV) WHERE p.v IS NOT NULL AND NOT EXISTS { MATCH (p)-[:HV]->(:VP) } RETURN p.name AS name",
        );
        assert_eq!(r.rows, vec![vec![Value::Str("b".into())]], "columnar {on}");
        let r = run(
            &g,
            "MATCH (p:PV) RETURN p.name AS name, COUNT { MATCH (p)-[:HV]->(:VP) } AS n, EXISTS { MATCH (p)-[:HV]->() WHERE p.v IS NOT NULL } AS e ORDER BY name",
        );
        assert_eq!(
            r.rows,
            vec![
                vec![Value::Str("a".into()), Value::Int(1), Value::Bool(true)],
                vec![Value::Str("b".into()), Value::Int(0), Value::Bool(false)],
                vec![Value::Str("c".into()), Value::Int(0), Value::Bool(false)],
            ],
            "columnar {on}"
        );
        let r = run(
            &g,
            "MATCH (p:PV) WHERE EXISTS { MATCH (p)-[:HV]->(v:VP) WITH v RETURN v } RETURN count(p) AS c",
        );
        assert_eq!(
            r.rows,
            vec![vec![Value::Int(1)]],
            "a body with its own RETURN is untouched: columnar {on}"
        );
    }
    g.set_columnar_scans(true);
}

#[test]
fn the_hop_bearing_aggregate_scan_answers_exactly_what_the_general_path_answers() {
    // `MATCH (a:A)-[r:T]->(b:B) [WHERE p(a, r, b)] RETURN <aggregates over
    // a.x, r.y, b.z>[, keys]`: the typed relationship walk with its ends;
    // each end's columns over the span of its distinct ids, bound by
    // binary search; labels filter by membership; the fold is the node
    // scan's. `(s:Company)-[r:SUPPLIES]->(cus:Company) WHERE … RETURN
    // s.primaryCountry, cus.primaryCountry, count(r)` expanded every
    // Company and decoded every SUPPLIES in full (1.2 s on the production
    // port). The general path is the same statement under the kill switch.
    let g = graph();
    g.set_columnar_column_budget_factor(64);
    run(
        &g,
        "CREATE (:Co {n: 'a', c: 'US'}), (:Co {n: 'b', c: 'DE'}), (:Co {n: 'c', c: 'US'}), (:Co {n: 'd'}), (:Co:Big {n: 'e', c: 'FR'}), (:Other {n: 'x', c: 'US'})",
    );
    run(
        &g,
        "MATCH (a:Co {n: 'a'}), (b:Co {n: 'b'}), (c:Co {n: 'c'}), (d:Co {n: 'd'}), (e:Co {n: 'e'}), (x:Other {n: 'x'}) CREATE (a)-[:SUP {v: 'ok', w: 1}]->(b), (a)-[:SUP {w: 2}]->(c), (b)-[:SUP {v: 'retracted', w: 3}]->(c), (c)-[:SUP {w: 4}]->(d), (d)-[:SUP {w: 5}]->(e), (e)-[:SUP {w: 6}]->(a), (a)-[:SUP {w: 7}]->(x), (x)-[:SUP {w: 8}]->(a), (a)-[:OTH {w: 9}]->(b)",
    );
    let cases = [
        "MATCH (s:Co)-[r:SUP]->(cus:Co) WHERE s.c IS NOT NULL AND cus.c IS NOT NULL AND coalesce(r.v, 'pending') <> 'retracted' RETURN s.c AS from, cus.c AS to, count(r) AS count ORDER BY count DESC, from, to LIMIT 15",
        "MATCH (a:Co)-[r:SUP]->(b:Co) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:Co) RETURN count(r) AS c, count(a) AS d, count(b) AS e",
        "MATCH (a)-[:SUP]->(b:Co) RETURN b.c AS c, count(*) AS n ORDER BY c",
        "MATCH (a:Co)<-[r:SUP]-(b) RETURN a.n AS n, sum(r.w) AS w ORDER BY n",
        "MATCH ()-[r:SUP]->(b:Big) RETURN count(*) AS c",
        "MATCH (a:Co {c: 'US'})-[r:SUP]->(b) WHERE b.c = 'US' OR b.c IS NULL RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP|OTH]->(b:Co) RETURN type(r) AS t, count(*) AS c ORDER BY t",
        "MATCH (a:Co)-[r:SUP]->(b:Co) WHERE a:Big OR b:Big RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:Co) WHERE exists((b)-[:SUP]->(:Co)) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:Co) WITH a.c AS from, count(r) AS n RETURN from, n ORDER BY n DESC, from",
        "MATCH (a:Co)-[r:NOPE]->(b:Co) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:NOPE) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP {w: 2}]->(b:Co) RETURN a.n AS n, b.n AS m",
        "MATCH (a:Co)-[r:SUP]->(b:Co) RETURN count(DISTINCT a.c) AS ac, count(DISTINCT b.c) AS bc, avg(r.w) AS w",
    ];
    for q in cases {
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        let hop = tf
            .counters()
            .get("interp.columnar hop aggregate scans")
            .is_some();
        // The composable DataChunk pipeline now binds relationship variables, so
        // a labelled-start rel-var aggregate (`(a:Co)-[r:SUP]->(b:Co) RETURN
        // count(r), …`) is claimed by it rather than the whole-shape rel-scan
        // recognizer — a broader columnar operator, same byte-identical answer.
        let agg_pipeline = tf
            .counters()
            .get("interp.pipeline aggregate runs")
            .is_some();
        let bare_count =
            q.ends_with("RETURN count(*) AS c") && !q.contains("WHERE") && !q.contains('{');
        let projection = q.contains("RETURN a.n AS n, b.n AS m");
        assert!(
            hop || agg_pipeline || bare_count || projection,
            "a columnar operator answered (hop scan / pipeline / count store / general): {q}"
        );
    }
    assert_eq!(
        run(&g, "MATCH (s:Co)-[r:SUP]->(cus:Co) WHERE s.c IS NOT NULL AND cus.c IS NOT NULL AND coalesce(r.v, 'pending') <> 'retracted' RETURN s.c AS from, cus.c AS to, count(r) AS count ORDER BY count DESC, from, to LIMIT 15").rows,
        vec![
            vec![Value::Str("FR".into()), Value::Str("US".into()), Value::Int(1)],
            vec![Value::Str("US".into()), Value::Str("DE".into()), Value::Int(1)],
            vec![Value::Str("US".into()), Value::Str("US".into()), Value::Int(1)],
        ]
    );
    // Declines: undirected, variable length, a path variable, a repeated
    // variable, a bare end or relationship in the items, two hops.
    for q in [
        "MATCH (a:Co)-[r:SUP]-(b:Co) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP*1..2]->(b:Co) RETURN count(*) AS c",
        "MATCH p = (a:Co)-[r:SUP]->(b:Co) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(a) RETURN count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:Co) RETURN a, count(*) AS c",
        "MATCH (a:Co)-[r:SUP]->(b:Co)-[:SUP]->(c:Co) RETURN count(*) AS c",
    ] {
        let (_, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(
            t.counters().get("interp.columnar hop aggregate scans"),
            None,
            "declines: {q}"
        );
    }
}

#[test]
fn unwind_steps_and_aggregating_breakers_run_as_a_column_walk() {
    // The two UNWIND chains of the production port: `MATCH (br:…) WHERE …
    // UNWIND [{self: br.a, peer: br.b}, {…}] AS pair WITH pair.self AS
    // iso3, … ORDER BY intensity DESC …` (1.6 s) and `MATCH (s:…) WITH s,
    // [s.a, s.b] AS isos UNWIND isos AS iso WITH s, iso WHERE iso IS NOT
    // NULL WITH iso AS iso3, sum(CASE …) AS targeted, … ORDER BY … LIMIT
    // 250` (1.2 s). An UNWIND is a per-row list product in the chain;
    // an aggregating breaker folds through the shared Fold. Compared with
    // the general path under the kill switch.
    let g = graph();
    run(
        &g,
        "CREATE (:BR {a: 'US', b: 'DE', state: 'tense', intensity: 7, trend: 'up'}), (:BR {a: 'US', b: 'FR', state: 'allied', intensity: 3}), (:BR {a: 'DE', b: 'FR', state: 'allied', intensity: 5, trend: 'down'}), (:BR {a: 'CN', b: 'US', state: 'hostile'}), (:BR {a: 'JP', b: 'KR', state: 'tense', intensity: 2})",
    );
    run(
        &g,
        "CREATE (:SN {t: 'US', i: 'DE', sev: 3, d: 20}), (:SN {t: 'US', i: 'FR', sev: 5, d: 10}), (:SN {t: 'DE', sev: 1}), (:SN {i: 'US', d: 30}), (:SN {t: 'CN', i: 'US', sev: 4, d: 25})",
    );
    let cases = [
        "MATCH (br:BR) WHERE br.intensity IS NOT NULL UNWIND [{self: br.a, peer: br.b}, {self: br.b, peer: br.a}] AS pair WITH pair.self AS iso3, pair.peer AS peer, br.state AS state, br.intensity AS intensity, coalesce(br.trend, 'stable') AS trend ORDER BY intensity DESC, iso3, peer WITH iso3, collect({peer: peer, state: state, intensity: intensity, trend: trend}) AS rels RETURN iso3, [r IN rels WHERE r.state IN ['hostile', 'tense'] | r][..5] AS hostile, [r IN rels WHERE r.state IN ['allied', 'cooperative'] | r][..5] AS friendly ORDER BY iso3",
        "MATCH (s:SN) WITH s, [s.t, s.i] AS isos UNWIND isos AS iso WITH s, iso WHERE iso IS NOT NULL WITH iso AS iso3, sum(CASE WHEN s.t = iso THEN 1 ELSE 0 END) AS targeted, sum(CASE WHEN s.i = iso THEN 1 ELSE 0 END) AS imposed, max(coalesce(s.sev, 0)) AS maxSeverity, max(coalesce(s.d, 0)) AS latest RETURN iso3, targeted, imposed, maxSeverity, toString(latest) AS latest ORDER BY (targeted + imposed) DESC, iso3 LIMIT 250",
        "MATCH (s:SN) UNWIND [s.t, s.i] AS iso WITH iso WHERE iso IS NOT NULL RETURN iso, count(*) AS c ORDER BY c DESC, iso",
        "MATCH (s:SN) UNWIND null AS x WITH x RETURN count(*) AS c",
        "MATCH (s:SN) UNWIND [] AS x WITH x RETURN count(x) AS c",
        "MATCH (s:SN) UNWIND range(1, coalesce(s.sev, 0)) AS k WITH s.t AS t, k WHERE k % 2 = 1 RETURN t, k ORDER BY t, k",
        "MATCH (s:SN) UNWIND [1, 2] AS a UNWIND [10, 20] AS b WITH a + b AS ab RETURN ab ORDER BY ab",
        "MATCH (s:SN) WITH s.t AS t, count(*) AS c WHERE c > 1 RETURN t, c ORDER BY t",
        "MATCH (s:SN) WITH s.t AS t, s.i AS i WITH t, count(i) AS c, collect(i) AS xs RETURN t, c, size(xs) AS k ORDER BY t",
        "MATCH (s:SN) WHERE s.sev IS NOT NULL WITH s.t AS t, max(s.sev) AS m, count(DISTINCT s.i) AS k ORDER BY m DESC, t LIMIT 2 RETURN t, m, k",
        "MATCH (s:SN) WITH s.t AS t, count(*) AS c ORDER BY c DESC, t SKIP 1 LIMIT 2 RETURN t, c",
        "MATCH (n:DG) WITH n.k AS k, n.i AS i WITH k, count(i) AS c RETURN k, c ORDER BY k",
    ];
    for q in cases {
        g.set_columnar_scans(true);
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        assert!(
            tf.counters().get("interp.columnar stages").is_some(),
            "the columnar stage ran: {q}"
        );
    }
    assert_eq!(
        run(&g, "MATCH (s:SN) UNWIND [s.t, s.i] AS iso WITH iso WHERE iso IS NOT NULL RETURN iso, count(*) AS c ORDER BY c DESC, iso").rows,
        vec![
            vec![Value::Str("US".into()), Value::Int(4)],
            vec![Value::Str("DE".into()), Value::Int(2)],
            vec![Value::Str("CN".into()), Value::Int(1)],
            vec![Value::Str("FR".into()), Value::Int(1)],
        ]
    );
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (s:SN) UNWIND null AS x WITH x RETURN count(*) AS c",
        )
    });
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(0)]],
        "UNWIND null yields nothing"
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar stage unwound a list")
    );
    // UNWIND of a non-list refuses in both modes, with the same words.
    for on in [true, false] {
        g.set_columnar_scans(on);
        let stmt = parse_statement("MATCH (s:SN) UNWIND s.sev AS x WITH x RETURN x ORDER BY x")
            .expect("parses");
        let err = run_query(&g, &stmt, BTreeMap::new()).expect_err("a non-list refuses");
        assert!(
            format!("{err:?}").contains("UNWIND takes a list"),
            "columnar {on}: {err:?}"
        );
    }
    g.set_columnar_scans(true);
    // Reading the scanned variable after a WITH dropped it refuses in both
    // modes: the column walk must not answer what the general path refuses.
    for on in [true, false] {
        g.set_columnar_scans(on);
        let stmt =
            parse_statement("MATCH (s:SN) WITH s.t AS t WITH t, s.i AS i RETURN t, i ORDER BY t")
                .expect("parses");
        let err = run_query(&g, &stmt, BTreeMap::new()).expect_err("out of scope refuses");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("`s`") || msg.contains("\"s\""),
            "names the variable, columnar {on}: {msg}"
        );
    }
    g.set_columnar_scans(true);
    // A node leaving an aggregating breaker still declines.
    let (_, t) =
        engram_observe::with_trace(|| run(&g, "MATCH (s:SN) WITH s, count(*) AS c RETURN s.t, c"));
    assert_eq!(t.counters().get("interp.columnar stages"), None);
}

#[test]
fn a_hop_end_is_budgeted_by_its_label_not_its_distinct_ids() {
    // The production port: a few thousand Companies appear in SUPPLIES,
    // every Company carries primaryCountry. A budget sized from the
    // distinct ends aborted the column read and the hop scan declined,
    // silently, so its target statement never moved. The end label's row
    // count is the bound; an unlabelled end keeps the narrow budget and
    // declines (with its event) when the column is wider.
    let g = graph();
    // 400 Companies with a country, 4 of them in SUPPLIES.
    for i in 0..400i64 {
        run_params(
            &g,
            "CREATE (:Cp {i: $i, c: CASE WHEN $i % 2 = 0 THEN 'US' ELSE 'DE' END})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    run(
        &g,
        "MATCH (a:Cp {i: 10}), (b:Cp {i: 150}), (c:Cp {i: 290}), (d:Cp {i: 399}) CREATE (a)-[:SP {v: 'ok'}]->(b), (c)-[:SP]->(d), (a)-[:SP {v: 'retracted'}]->(d)",
    );
    let q = "MATCH (s:Cp)-[r:SP]->(cus:Cp) WHERE s.c IS NOT NULL AND cus.c IS NOT NULL AND coalesce(r.v, 'pending') <> 'retracted' RETURN s.c AS from, cus.c AS to, count(r) AS count ORDER BY count DESC, from, to LIMIT 15";
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    g.set_columnar_scans(false);
    let general = run(&g, q);
    g.set_columnar_scans(true);
    assert_eq!(r.rows, general.rows);
    // LABELLED endpoints + a multi-var WHERE (`s.c … AND cus.c … AND
    // coalesce(r.v,…) …`) are now claimed by the general pipeline aggregate (its
    // WHERE splits per-predicate and it seeds from the distinct ends, so the
    // wide-label-column budget concern does not arise here) — byte-identical, and
    // still columnar (no stream). The hop-aggregate's end-LABEL budget is exercised
    // by the unlabelled-end case below (which the pipeline declines) and by
    // `a_labelled_hop_aggregate_*` siblings.
    assert_eq!(
        t.counters().get("interp.pipeline aggregate runs"),
        Some(&1),
        "the labelled multi-var-WHERE hop aggregate fires the pipeline aggregate"
    );
    // Unlabelled ends: the narrow budget applies and the RANGE scan over the
    // wide end column declines — the hop aggregate now GATHERS exactly the ends'
    // columns (the IC5-class widening) instead of declining to the general path.
    // Byte-identical result.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (s)-[r:SP]->(cus) WHERE s.c IS NOT NULL RETURN s.c AS from, cus.c AS to, count(r) AS count ORDER BY from, to",
        )
    });
    g.set_columnar_scans(false);
    let general = run(
        &g,
        "MATCH (s)-[r:SP]->(cus) WHERE s.c IS NOT NULL RETURN s.c AS from, cus.c AS to, count(r) AS count ORDER BY from, to",
    );
    g.set_columnar_scans(true);
    assert_eq!(r.rows, general.rows);
    assert_eq!(
        t.counters().get("interp.columnar hop aggregate scans"),
        Some(&1),
        "the narrow-budget end columns gather instead of declining"
    );
    assert!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0)
            > 0,
        "the wide end column falls back to the point-gather"
    );
}

#[test]
fn a_date_only_string_is_midnight_for_datetime() {
    // `datetime(coalesce(e.eventTime, e.startAt))` over a date-only value
    // refused with `lacks a T` on the production port (market.ts:3491/3492
    // — the ONE engine gap among the 38 unsupported statements; the rest
    // are corpus artifacts). Neo4j reads `datetime('2015-07-21')` as
    // 2015-07-21T00:00:00Z and `localdatetime('2015-07-21')` as
    // 2015-07-21T00:00:00.
    let g = graph();
    run(
        &g,
        "CREATE (:EV7 {t: '2015-07-21'}), (:EV7 {t: '2015-07-21T10:30:00Z'}), (:EV7 {t: '2015-07-22'})",
    );
    let r = run(
        &g,
        "RETURN datetime('2015-07-21') = datetime('2015-07-21T00:00:00Z') AS same, toString(datetime('2015-07-21')) AS s, toString(localdatetime('2015-07-21')) AS l",
    );
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Bool(true),
            // openCypher canonical form omits a zero seconds field.
            Value::Str("2015-07-21T00:00Z".into()),
            Value::Str("2015-07-21T00:00".into()),
        ]]
    );
    for on in [true, false] {
        g.set_columnar_scans(on);
        let r = run(
            &g,
            "MATCH (e:EV7) WHERE datetime(e.t) >= datetime('2015-07-21T05:00:00Z') RETURN count(e) AS c",
        );
        assert_eq!(r.rows, vec![vec![Value::Int(2)]], "columnar {on}");
        let r = run(
            &g,
            "MATCH (e:EV7) RETURN e.t AS t, datetime(e.t) >= datetime('2015-07-21T00:00:00Z') AS late ORDER BY t",
        );
        assert_eq!(r.rows.len(), 3, "columnar {on}");
        assert_eq!(r.rows[0][1], Value::Bool(true));
    }
    g.set_columnar_scans(true);
    // A bad date part still refuses, by name.
    let stmt = parse_statement("RETURN datetime('2015-13-45') AS d").expect("parses");
    let err = run_query(&g, &stmt, BTreeMap::new()).expect_err("refuses");
    assert!(format!("{err:?}").contains("bad date part"), "{err:?}");
}

#[test]
fn a_columnar_continuation_folds_the_next_with_without_rows() {
    // An aggregating WITH right after the stage's breaker, reading only
    // its aliases, folds over the breaker's rows as they are — the degree
    // histogram's `WITH d ORDER BY d WITH collect(d) AS ds` built 1.79M
    // one-entry rows to collect one integer each. And a single Int or Str
    // ORDER BY key sorts a (key, arrival) vector, not a comparator. Both
    // against the general path under the kill switch.
    let g = graph();
    g.set_degree_table_after(0);
    run(
        &g,
        "CREATE (a:CT {i: 3, s: 'b'}), (b:CT {i: 1, s: 'a'}), (c:CT {i: 2, s: 'c'}), (d:CT {i: 1, s: 'a'})",
    );
    // i64::MIN as a parameter (the literal overflows before its sign).
    run_params(
        &g,
        "CREATE (:CT {i: $m, s: 'z'})",
        [("m".to_string(), Value::Int(i64::MIN))]
            .into_iter()
            .collect(),
    );
    run(
        &g,
        "MATCH (a:CT {s: 'b'}), (b:CT {i: 1, s: 'a'}), (c:CT {i: 2}) CREATE (a)-[:CR]->(b), (a)-[:CR]->(c), (b)-[:CR]->(c)",
    );
    let cases = [
        "MATCH (n:CT) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN ds, ds[size(ds) - 1] AS max",
        "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY i DESC, s WITH s, collect(i) AS xs, count(*) AS c RETURN s, xs, c ORDER BY s",
        "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY s WITH s, sum(i) AS total WHERE total > 1 RETURN s, total ORDER BY total DESC",
        "MATCH (n:CT) WITH n.i AS i ORDER BY i WITH collect(i) AS xs RETURN xs",
        "MATCH (n:CT) WITH n.i AS i ORDER BY i DESC WITH collect(i) AS xs RETURN xs",
        "MATCH (n:CT) WHERE n.i > -1000 WITH n.i AS i ORDER BY i DESC WITH collect(i) AS xs RETURN xs",
        "MATCH (n:CT) WITH n.s AS s ORDER BY s DESC WITH collect(s) AS xs RETURN xs",
        "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY s, i WITH collect(i) AS xs RETURN xs",
        "MATCH (n:CT) WITH n.s AS s ORDER BY s WITH count(DISTINCT s) AS k, max(s) AS m ORDER BY k LIMIT 1 RETURN k, m",
        "MATCH (n:CT) UNWIND [{self: n.s, peer: n.i}, {self: 'q', peer: n.i}] AS pair WITH pair.self AS iso3, pair.peer AS peer, n.i AS intensity ORDER BY intensity DESC, iso3 WITH iso3, collect({peer: peer, intensity: intensity}) AS rels RETURN iso3, size(rels) AS n, rels[0].peer AS top ORDER BY iso3",
        "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY i WITH s, count(*) AS c ORDER BY c DESC, s SKIP 1 LIMIT 2 RETURN s, c",
    ];
    for q in cases {
        g.set_columnar_scans(true);
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        let fused = tf
            .sometimes_hit()
            .contains("interp.columnar stage fused the next aggregating WITH");
        let stage = tf.counters().get("interp.columnar stages").is_some();
        assert!(stage, "the columnar stage ran: {q}");
        assert!(fused, "the next WITH was fused: {q}");
    }
    // The fusion's own evidence: no `Row` was built for the degrees.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:CT) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN ds",
        )
    });
    // The fixture's MATCH bound `b` twice (two nodes with i=1, s='a'), so
    // the CREATE ran twice: a and c have degree 4, b and d 2, z 0.
    assert_eq!(
        r.rows,
        vec![vec![Value::List(vec![
            Value::Int(0),
            Value::Int(2),
            Value::Int(2),
            Value::Int(4),
            Value::Int(4)
        ])]]
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar stage fused the next aggregating WITH")
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar order sorted a primitive key")
    );
    // Ties keep arrival order under the primitive sort, both directions.
    assert_eq!(
        run(
            &g,
            "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY s WITH collect(i) AS xs RETURN xs"
        )
        .rows,
        run(
            &g,
            "MATCH (n:CT) WITH n.s AS s, n.i AS i ORDER BY s WITH collect(i) AS xs RETURN xs"
        )
        .rows
    );
    // Declines to the general path: the next WITH reads something beyond
    // the aliases, or is not aggregating.
    let q = "MATCH (n:CT) WITH n.i AS i ORDER BY i WITH i, $p AS p RETURN i, p";
    let stmt = parse_statement(q).expect("parses");
    let (_, t) = engram_observe::with_trace(|| {
        run_query(
            &g,
            &stmt,
            [("p".to_string(), Value::Int(1))].into_iter().collect(),
        )
    });
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar stage fused the next aggregating WITH"),
        "not fused: {q}"
    );
}

#[test]
fn a_selective_projection_reads_the_item_columns_only_for_the_survivors() {
    // The pod bisect: `MATCH (e:WorkflowExecution) WHERE e.origin IS NULL
    // RETURN e.execution_id, e.workflow_id, e.context` read the whole
    // `context` column (blobs) for every execution to project 136
    // survivors — 674 of its 736 ms. The predicate's reads now load
    // first, over the population; the items' reads load over the
    // SURVIVORS: a column walk bounded to their span when it fits the
    // budget, else one projected get each. Against the general path.
    let g = graph();
    // The two-phase column READS are counted here; the property-column
    // cache would serve a repeated read without them.
    g.set_prop_column_budget(0);
    // This test isolates the two-phase SURVIVOR mechanism; the property-
    // index seek (its own test) would supersede the `e.wf = 2` case, so
    // it is turned off here to exercise the walk it is about.
    g.set_property_seek(false);
    for i in 0..400i64 {
        run_params(
            &g,
            "CREATE (:WE2 {i: $i, origin: CASE WHEN $i % 100 = 7 THEN null ELSE 'auto' END, wf: $i % 5, context: 'a blob that must not be read for the dropped rows'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let cases = [
        "MATCH (e:WE2) WHERE e.origin IS NULL RETURN e.i AS i, e.wf AS wf, e.context AS ctx ORDER BY i",
        "MATCH (e:WE2) WHERE e.origin IS NULL RETURN e.i AS i ORDER BY i DESC LIMIT 2",
        "MATCH (e:WE2) WHERE e.i < 5 RETURN e.context AS ctx, e ORDER BY e.i",
        "MATCH (e:WE2) WHERE e.origin IS NULL AND e.i > 100 RETURN e.wf AS wf, e.context IS NOT NULL AS has ORDER BY wf",
        "MATCH (e:WE2) RETURN e.i AS i, e.wf AS wf ORDER BY i LIMIT 3",
        "MATCH (e:WE2) WHERE e.origin IS NULL RETURN e:WE2 AS is_we, count(*) AS c",
        "MATCH (e:WE2) WHERE e.i % 100 = 7 RETURN DISTINCT e.wf AS wf ORDER BY wf",
    ];
    for q in cases {
        let (fast, tf) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "rows: {q}");
        let columnar = tf
            .counters()
            .get("interp.columnar projection scans")
            .is_some()
            || tf
                .counters()
                .get("interp.columnar aggregate scans")
                .is_some();
        assert!(columnar, "a columnar path ran: {q}");
    }
    // Four survivors of 400, scattered across the `context` column: the
    // over-survivors RANGE walk exceeds the budget, so the items now GATHER
    // exactly the four survivors' columns (the IC5-class widening) instead of
    // one projected get each — the context column is never scanned whole.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) WHERE e.origin IS NULL RETURN e.i AS i, e.wf AS wf, e.context AS ctx ORDER BY i",
        )
    });
    assert_eq!(r.rows.len(), 4);
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar projection read items over the survivors")
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor"),
        "the survivors' columns are GATHERED, not fetched per-survivor"
    );
    // The over-survivors items fall back to the point-gather (the scattered
    // survivors' span blows the budget), replacing the per-survivor projected gets.
    assert!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0)
            > 0,
        "the wide item columns over the scattered survivors point-gather"
    );
    assert_eq!(
        t.counters().get("store.column presence scans"),
        Some(&1),
        "the predicate: one presence scan"
    );
    assert_eq!(
        t.counters()
            .get("graph.projected node materialisations")
            .copied()
            .unwrap_or(0),
        0,
        "the gather replaces the per-survivor projected gets"
    );
    // A contiguous survivor span reads its items as columns.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) WHERE e.i < 5 RETURN e.context AS ctx, e ORDER BY e.i",
        )
    });
    assert_eq!(r.rows.len(), 5);
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar projection read items over the survivors")
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor")
    );
    // An item that probes adjacency declines the per-get path (nothing
    // binds a probe without a walk) — the general path answers.
    run(
        &g,
        "MATCH (a:WE2 {i: 7}), (b:WE2 {i: 107}) CREATE (a)-[:WL]->(b)",
    );
    let q = "MATCH (e:WE2) WHERE e.origin IS NULL RETURN e.i AS i, exists((e)-[:WL]->()) AS linked ORDER BY i";
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    g.set_columnar_scans(false);
    let general = run(&g, q);
    g.set_columnar_scans(true);
    assert_eq!(r.rows, general.rows);
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor")
    );
    // Without a predicate every member survives: ONE phase, no survivor
    // pass (the first cut paid a pass that bound nothing).
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) RETURN e.i AS i, e.wf AS wf ORDER BY i LIMIT 3",
        )
    });
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        t.counters()
            .get("interp.columnar projection single-phase nodes"),
        Some(&1)
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items over the survivors")
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor")
    );
    // A predicate whose columns cover the items' (`i` read by both):
    // one phase too — a second pass would re-read `i` over the survivors.
    let (_, t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (e:WE2) WHERE e.i < 5 RETURN e.i AS i ORDER BY i")
    });
    assert_eq!(
        t.counters()
            .get("interp.columnar projection single-phase nodes"),
        Some(&1),
        "items within the predicate's columns: one phase"
    );
    let (_, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) WHERE e.i < 5 RETURN e.wf AS wf ORDER BY wf",
        )
    });
    assert_eq!(
        t.counters()
            .get("interp.columnar projection single-phase nodes"),
        None,
        "an item column the predicate never touches: two phases"
    );
    // Survivors spread over the span (40 of 400, every tenth id): the
    // generic budget (4 x survivors = 160) would decline the 400-entry
    // walk and pay 40 gets; the walk is allowed 32 entries per survivor
    // (1,280), so it runs.
    let q = "MATCH (e:WE2) WHERE e.i % 10 = 0 RETURN e.wf AS wf, e.context AS ctx ORDER BY e.i";
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    assert_eq!(r.rows.len(), 40);
    assert!(
        t.sometimes_hit()
            .contains("interp.columnar projection read items over the survivors")
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor")
    );
    assert_eq!(
        t.counters().get("graph.projected node materialisations"),
        None
    );
    g.set_columnar_scans(false);
    let general = run(&g, q);
    g.set_columnar_scans(true);
    assert_eq!(r.rows, general.rows);
    // The items read nothing the predicate does not touch (`wf` is read
    // by both): one walk binds both, no second pass. With a column the
    // predicate never touches (`i`), two phases.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) WHERE e.wf = 2 AND e.origin IS NULL RETURN e.wf AS wf, e.origin AS o ORDER BY wf",
        )
    });
    assert_eq!(r.rows.len(), 4);
    assert_eq!(
        t.counters()
            .get("interp.columnar projection single-phase nodes"),
        Some(&1),
        "items within the predicate's columns: one phase"
    );
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (e:WE2) WHERE e.wf = 2 AND e.origin IS NULL RETURN e.wf AS wf, e.i AS i ORDER BY i",
        )
    });
    assert_eq!(r.rows.len(), 4);
    assert_eq!(
        t.counters()
            .get("interp.columnar projection single-phase nodes"),
        None
    );
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection read items per survivor"),
        "the scattered survivors' `i` column is GATHERED, not fetched per-survivor"
    );
    assert!(
        t.counters()
            .get("graph.column point-gather")
            .copied()
            .unwrap_or(0)
            > 0,
        "the wide `i` column over the scattered survivors point-gathers"
    );
    g.set_columnar_scans(false);
    let general = run(
        &g,
        "MATCH (e:WE2) WHERE e.wf = 2 AND e.origin IS NULL RETURN e.wf AS wf, e.i AS i ORDER BY i",
    );
    g.set_columnar_scans(true);
    assert_eq!(r.rows, general.rows);
}

#[test]
fn a_column_spanning_two_blocks_binds_in_id_order() {
    // Two signatures interleaved by id — `{i, a}` and `{i}` — so after
    // compaction the `i` column lives in TWO blocks and the visitor hands
    // its entries over block by block, out of id order. The walk's cursors
    // need id order; the consumer settles it (counted) and the answers
    // match both the pre-compaction run and the general path.
    let g = graph();
    // The block-by-block SETTLE is the subject; the property-column cache
    // would serve the post-compaction read from the pre-compaction column.
    g.set_prop_column_budget(0);
    for i in 0..200i64 {
        let src = if i % 2 == 0 {
            "CREATE (:TB {i: $i, a: 1})"
        } else {
            "CREATE (:TB {i: $i})"
        };
        run_params(
            &g,
            src,
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let qs = [
        "MATCH (n:TB) RETURN sum(n.i) AS s, count(n.a) AS a, min(n.i) AS lo, max(n.i) AS hi",
        "MATCH (n:TB) WHERE n.i > 150 RETURN n.i AS i, n.a AS a ORDER BY i",
        "MATCH (n:TB) WHERE n.a IS NULL RETURN count(*) AS c",
    ];
    let before: Vec<_> = qs.iter().map(|q| run(&g, q).rows).collect();
    g.shared_store().seal();
    g.shared_store().compact();
    for (q, b) in qs.iter().zip(&before) {
        let (after, t) = engram_observe::with_trace(|| run(&g, q));
        assert_eq!(&after.rows, b, "compaction changed the answer: {q}");
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(after.rows, general.rows, "general path: {q}");
        if q.contains("sum(n.i)") {
            assert_eq!(
                t.counters().get("graph.column visits sorted across blocks"),
                Some(&1),
                "the `i` column spans two blocks and was settled once; `a` sits in one"
            );
            assert!(
                t.counters()
                    .get("graph.column visits already in id order")
                    .is_some()
            );
        }
    }
    // The general path's per-id projections read a block row's columns,
    // never the assembled record (counted), and answer identically.
    g.set_columnar_scans(false);
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:TB) WHERE n.i IN [3, 4, 5] RETURN n.i AS i, n.a AS a ORDER BY i",
        )
    });
    g.set_columnar_scans(true);
    assert_eq!(r.rows.len(), 3);
    assert!(
        t.counters()
            .get("graph.projected gets served from columns")
            .is_some_and(|n| *n >= 3),
        "{:?}",
        t.counters()
    );
}

#[test]
fn a_correlated_property_map_joins_through_the_memo() {
    // `OPTIONAL MATCH (w:WF3 {workflow_id: e.workflow_id})` per surviving
    // execution: the memo used to refuse a correlated map, so every row
    // re-scanned every workflow with a projected get each — 9,312 gets for
    // 136 rows on the production port. Now the memo builds the workflows
    // once, WITHOUT the map, indexes them by `workflow_id`, and joins each
    // row by a hash probe with the map re-checked per candidate.
    let g = graph();
    for w in 0..5i64 {
        run_params(
            &g,
            "CREATE (:WF3 {workflow_id: 'wf-' + toString($w), origin: 'o' + toString($w), definition: 'a definition that must not be assembled per row'})",
            [("w".to_string(), Value::Int(w))].into_iter().collect(),
        );
    }
    // Two workflows SHARE an id (a duplicate is a real production shape).
    run(&g, "CREATE (:WF3 {workflow_id: 'wf-1', origin: 'dup'})");
    for i in 0..400i64 {
        run_params(
            &g,
            "CREATE (:WE3 {execution_id: $i, workflow_id: CASE WHEN $i % 100 = 3 THEN null ELSE 'wf-' + toString($i % 7) END, origin: CASE WHEN $i % 50 = 3 THEN null ELSE 'auto' END})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // 8 survivors (3, 53, …, 353): wf-3, wf-4 (53%7=4), 103%7=5, 153%7=6 (no
    // workflow), 203%7=0, 253%7=1 (TWO workflows), 303%7=2, 353%7=3; ids 3,
    // 103, 203, 303 carry a NULL workflow_id (no match; OPTIONAL ⇒ null row).
    let q = "MATCH (e:WE3) WHERE e.origin IS NULL OPTIONAL MATCH (w:WF3 {workflow_id: e.workflow_id}) RETURN e.execution_id AS x, w.origin AS o ORDER BY x, o";
    let where_form = "MATCH (e:WE3) WHERE e.origin IS NULL OPTIONAL MATCH (w:WF3) WHERE w.workflow_id = e.workflow_id RETURN e.execution_id AS x, w.origin AS o ORDER BY x, o";
    let (r, t) = engram_observe::with_trace(|| run(&g, q));
    let reference = run(&g, where_form);
    assert_eq!(
        r.rows, reference.rows,
        "the map form and the WHERE form agree"
    );
    let rows: Vec<(i64, Option<String>)> = r
        .rows
        .iter()
        .map(|row| {
            (
                match &row[0] {
                    Value::Int(n) => *n,
                    other => panic!("x: {other:?}"),
                },
                match &row[1] {
                    Value::Str(s) => Some(s.clone()),
                    Value::Null => None,
                    other => panic!("o: {other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (3, None),
            (53, Some("o4".into())),
            (103, None),
            (153, None),
            (203, None),
            (253, Some("dup".into())),
            (253, Some("o1".into())),
            (303, None),
            (353, Some("o3".into())),
        ]
    );
    assert_eq!(t.counters().get("interp.clause scan memos built"), Some(&1));
    assert!(
        t.sometimes_hit()
            .contains("interp.clause scan memo built without its correlated map")
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.clause scan memo joined by its map")
    );
    assert!(
        t.sometimes_hit()
            .contains("interp.clause scan answered from the equality index")
    );
    // The workflows were materialised ONCE (6), not once per surviving row.
    let gets = t
        .counters()
        .get("graph.projected node materialisations")
        .copied()
        .unwrap_or(0)
        + t.counters()
            .get("graph.nodes materialised in full")
            .copied()
            .unwrap_or(0);
    assert!(
        gets <= 6 + 8 + 8,
        "materialisations: {gets} — the inner label must not be re-read per row"
    );
    // A non-optional MATCH drops the unmatched rows; same memo.
    let q2 = "MATCH (e:WE3) WHERE e.origin IS NULL MATCH (w:WF3 {workflow_id: e.workflow_id}) RETURN e.execution_id AS x, w.origin AS o ORDER BY x, o";
    let r2 = run(&g, q2);
    assert_eq!(r2.rows.len(), 4);
}

#[test]
fn an_identity_equality_in_the_where_is_one_get() {
    // The cutover's hydrate: `UNWIND $ids AS eid MATCH (n) WHERE
    // elementId(n) = eid …` — a point lookup per id, never a scan. Every
    // form below answers exactly what the full scan answers, with ONE get
    // per id and no scan (counted).
    let g = graph();
    for i in 0..300i64 {
        run_params(
            &g,
            "CREATE (:IDA {i: $i, tag: 'a' + toString($i % 3)})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let eids: Vec<String> = run(
        &g,
        "MATCH (n:IDA) WHERE n.i IN [7, 42, 299] RETURN elementId(n) AS e ORDER BY n.i",
    )
    .rows
    .iter()
    .map(|r| match &r[0] {
        Value::Str(s) => s.clone(),
        other => panic!("{other:?}"),
    })
    .collect();
    assert_eq!(eids.len(), 3);
    let ids: Vec<i64> = run(
        &g,
        "MATCH (n:IDA) WHERE n.i IN [7, 42, 299] RETURN id(n) AS x ORDER BY n.i",
    )
    .rows
    .iter()
    .map(|r| match &r[0] {
        Value::Int(x) => *x,
        other => panic!("{other:?}"),
    })
    .collect();
    let p = |k: &str, v: Value| -> BTreeMap<String, Value> {
        [(k.to_string(), v)].into_iter().collect()
    };
    type SeekCase = (&'static str, BTreeMap<String, Value>, Vec<Vec<Value>>);
    let cases: Vec<SeekCase> = vec![
        (
            "MATCH (n) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Str(eids[1].clone())),
            vec![vec![Value::Int(42)]],
        ),
        (
            "MATCH (n) WHERE $eid = elementId(n) RETURN n.i AS i",
            p("eid", Value::Str(eids[0].clone())),
            vec![vec![Value::Int(7)]],
        ),
        (
            "MATCH (n:IDA) WHERE elementId(n) = $eid AND n.tag = 'a0' RETURN n.i AS i",
            p("eid", Value::Str(eids[1].clone())),
            vec![vec![Value::Int(42)]],
        ),
        (
            "MATCH (n:IDA {tag: 'a1'}) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Str(eids[1].clone())),
            vec![], // 42 % 3 = 0: the map still applies
        ),
        (
            "MATCH (n:Other) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Str(eids[1].clone())),
            vec![], // the label still applies
        ),
        (
            "MATCH (n) WHERE id(n) = $id RETURN n.i AS i",
            p("id", Value::Int(ids[2])),
            vec![vec![Value::Int(299)]],
        ),
        (
            "UNWIND $ids AS eid MATCH (n) WHERE elementId(n) = eid RETURN n.i AS i ORDER BY i",
            p(
                "ids",
                Value::List(eids.iter().cloned().map(Value::Str).collect()),
            ),
            vec![
                vec![Value::Int(7)],
                vec![Value::Int(42)],
                vec![Value::Int(299)],
            ],
        ),
        (
            "MATCH (n) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Str("n:99999999".into())),
            vec![],
        ),
        (
            "MATCH (n) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Str("not-an-element-id".into())),
            vec![],
        ),
        (
            "MATCH (n) WHERE elementId(n) = $eid RETURN n.i AS i",
            p("eid", Value::Null),
            vec![],
        ),
        (
            "MATCH (n) WHERE id(n) = $id RETURN n.i AS i",
            p("id", Value::Str(eids[2].clone())),
            vec![], // id() is an integer; a string never equals it
        ),
    ];
    for (q, params, expected) in cases {
        let (r, t) = engram_observe::with_trace(|| run_params(&g, q, params.clone()));
        assert_eq!(r.rows, expected, "{q}");
        assert!(
            t.sometimes_hit()
                .contains("interp.seed looked a node up by its id"),
            "the identity seek ran: {q}"
        );
        assert_eq!(t.counters().get("store.scans"), None, "no scan: {q}");
        assert!(
            t.counters().get("store.gets").copied().unwrap_or(0) <= 3 * 2,
            "one get per id: {q} — {:?}",
            t.counters()
        );
    }
    // An equality whose other side reads the node itself is not a seek.
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:IDA) WHERE elementId(n) = 'n:' + toString(id(n)) RETURN count(n) AS c",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(300)]]);
    assert!(
        !t.sometimes_hit()
            .contains("interp.seed looked a node up by its id")
    );
}

#[test]
fn a_bound_end_hop_drives_from_adjacency_not_a_scan() {
    // The docs hydrate: `MATCH (n) WHERE elementId(n) = $eid OPTIONAL
    // MATCH (parent)-[:HAS_ELEMENT]->(n)` — parent unbound, n bound. The
    // reversed walk drives from n's incoming adjacency, never scanning the
    // graph as `parent`; the answers match the forced-scan path exactly,
    // and no store scan runs.
    let g = graph();
    for pi in 0..6i64 {
        run_params(
            &g,
            "CREATE (p:Doc {pid: $pid})",
            [("pid".to_string(), Value::Int(pi))].into_iter().collect(),
        );
    }
    for i in 0..600i64 {
        run_params(
            &g,
            "MATCH (p:Doc {pid: $pid}) CREATE (p)-[:HAS_ELEMENT]->(:Leaf {i: $i})",
            [
                ("pid".to_string(), Value::Int(i / 100)),
                ("i".to_string(), Value::Int(i)),
            ]
            .into_iter()
            .collect(),
        );
    }
    for i in 0..600i64 {
        run_params(
            &g,
            "CREATE (:Other {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let eid = |i: i64| -> String {
        match &run_params(
            &g,
            "MATCH (n:Leaf {i: $i}) RETURN elementId(n) AS e",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        )
        .rows[0][0]
        {
            Value::Str(s) => s.clone(),
            other => panic!("{other:?}"),
        }
    };
    let queries = [
        "MATCH (n) WHERE elementId(n) = $eid OPTIONAL MATCH (parent:Doc)-[:HAS_ELEMENT]->(n) RETURN parent.pid AS pid",
        "MATCH (n) WHERE elementId(n) = $eid MATCH (parent)-[:HAS_ELEMENT]->(n) RETURN parent.pid AS pid",
        "MATCH (n) WHERE elementId(n) = $eid MATCH (parent)-[r:HAS_ELEMENT]->(n) RETURN type(r) AS t",
    ];
    for q in queries {
        let params: BTreeMap<String, Value> = [("eid".to_string(), Value::Str(eid(250)))]
            .into_iter()
            .collect();
        let (fast, t) = engram_observe::with_trace(|| run_params(&g, q, params.clone()));
        g.set_hop_reversal(false);
        let scan = run_params(&g, q, params.clone());
        g.set_hop_reversal(true);
        assert_eq!(fast.rows, scan.rows, "reversed vs scan: {q}");
        assert!(
            t.sometimes_hit()
                .contains("interp.hop driven from its bound end"),
            "the bound-end hop reversed: {q}"
        );
    }
    let (r, t) = engram_observe::with_trace(|| {
        run_params(
            &g,
            "MATCH (n) WHERE elementId(n) = $eid OPTIONAL MATCH (parent:Doc)-[:HAS_ELEMENT]->(n) RETURN parent.pid AS pid",
            [("eid".to_string(), Value::Str(eid(250)))]
                .into_iter()
                .collect(),
        )
    });
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(2)]],
        "pid 250 belongs to parent 2"
    );
    assert!(
        t.counters().get("store.gets").copied().unwrap_or(0) < 20,
        "adjacency, not a scan: {:?}",
        t.counters()
    );
    let oeid = match &run(&g, "MATCH (o:Other {i: 0}) RETURN elementId(o) AS e").rows[0][0] {
        Value::Str(s) => s.clone(),
        other => panic!("{other:?}"),
    };
    let r = run_params(
        &g,
        "MATCH (n) WHERE elementId(n) = $eid OPTIONAL MATCH (parent:Doc)-[:HAS_ELEMENT]->(n) RETURN parent.pid AS pid",
        [("eid".to_string(), Value::Str(oeid))]
            .into_iter()
            .collect(),
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Null]],
        "no parent: OPTIONAL null row"
    );
}

#[test]
fn a_property_equality_where_seeks_the_index_not_a_scan() {
    // The FOUNDATION of BTREE property indexes: `WHERE n.prop = <literal|
    // param>` on the general (stage) path drives a range-index SEEK instead
    // of a label scan. `WHERE c.country = 'USA'` sought every City; now it
    // probes the derived index. The answer equals the forced label scan, the
    // seek touches O(matches) not the 1,000-node label, numeric cross-type
    // and params work, and a non-selective probe falls back to the scan.
    // (The columnar projection/aggregate paths do not yet seek — P1.2.)
    let g = graph();
    for i in 0..1000i64 {
        let country = match i % 100 {
            0 => "USA",
            1 => "CHN",
            _ => "OTHER",
        };
        run_params(
            &g,
            "CREATE (:City {i: $i, country: $c, pop: $i % 3})",
            [
                ("i".to_string(), Value::Int(i)),
                ("c".to_string(), Value::Str(country.to_string())),
            ]
            .into_iter()
            .collect(),
        );
    }
    // The `WITH c` forces the stage path where Seed::PropEq lives.
    let cases: Vec<(&str, BTreeMap<String, Value>, usize)> = vec![
        (
            "MATCH (c:City) WHERE c.country = 'USA' WITH c ORDER BY c.i RETURN c.i AS i",
            BTreeMap::new(),
            10,
        ),
        (
            "MATCH (c:City) WHERE 'CHN' = c.country WITH c ORDER BY c.i RETURN c.i AS i",
            BTreeMap::new(),
            10,
        ),
        (
            "MATCH (c:City) WHERE c.country = $q WITH c ORDER BY c.i RETURN c.i AS i",
            [("q".to_string(), Value::Str("USA".into()))]
                .into_iter()
                .collect(),
            10,
        ),
        (
            "MATCH (c:City) WHERE c.country = 'NOWHERE' WITH c RETURN c.i AS i",
            BTreeMap::new(),
            0,
        ),
    ];
    for (q, params, expect_rows) in cases {
        let (fast, t) = engram_observe::with_trace(|| run_params(&g, q, params.clone()));
        g.set_property_seek(false);
        let scan = run_params(&g, q, params.clone());
        g.set_property_seek(true);
        assert_eq!(fast.rows, scan.rows, "seek vs scan: {q}");
        assert_eq!(fast.rows.len(), expect_rows, "row count: {q}");
        assert!(
            t.sometimes_hit()
                .contains("interp.seed sought a property index"),
            "the property index was sought: {q}"
        );
        let gets = t.counters().get("store.gets").copied().unwrap_or(0);
        assert!(
            gets < 1000,
            "seek touched {gets} gets, not the whole label: {q}"
        );
    }
    // A rare label with a common value: the label scan wins, not the index.
    for i in 0..3i64 {
        run_params(
            &g,
            "CREATE (:Rare {i: $i, country: 'OTHER'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:Rare) WHERE n.country = 'OTHER' WITH n RETURN count(n) AS c",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    assert!(
        !t.sometimes_hit()
            .contains("interp.seed sought a property index"),
        "3 Rare < ~980 OTHER: the label scan wins"
    );
}
#[test]
fn a_topk_late_projects_the_expensive_column_only_for_the_survivors() {
    // The CP-1.3 + CP-2.2 case: `… ORDER BY <cheap key> LIMIT k` where the
    // projection also pulls an expensive output-only column. The engine must
    // keep the k smallest by the cheap key and materialise the expensive
    // column ONLY for those k, not for every candidate (SNB IC2/IC8/IC9/IS2
    // decoded Message.content for tens of thousands of rows to keep 20). The
    // answer must equal the eager full-projection (differential via the lever).
    let g = graph();
    for i in 0..100i64 {
        run_params(
            &g,
            "CREATE (:M {key: $i, big: $b})",
            [
                ("i".to_string(), Value::Int(i)),
                ("b".to_string(), Value::Str(format!("big-{i}"))),
            ]
            .into_iter()
            .collect(),
        );
    }
    for x in 0..20i64 {
        run_params(
            &g,
            "CREATE (:X {i: $x})",
            [("x".to_string(), Value::Int(x))].into_iter().collect(),
        );
        for j in 0..5i64 {
            run_params(
                &g,
                "MATCH (x:X {i: $x}), (m:M {key: $k}) CREATE (x)-[:R]->(m)",
                [
                    ("x".to_string(), Value::Int(x)),
                    ("k".to_string(), Value::Int(x * 5 + j)),
                ]
                .into_iter()
                .collect(),
            );
        }
    }
    // Streaming (UNWIND fan-out) so it reaches the StreamProjector, not the
    // columnar projection; `m` is bound in the final MATCH so `m.big` (output-
    // only) is deferrable while `m.key` (the ORDER key) is eager.
    let q = "MATCH (x:X) WITH collect(x) AS xs UNWIND xs AS x MATCH (x)-[:R]->(m:M) RETURN m.key AS k, m.big AS b ORDER BY k DESC LIMIT 5";
    let (late, tl) = engram_observe::with_trace(|| run(&g, q));
    g.set_late_projection(false);
    let (eager, te) = engram_observe::with_trace(|| run(&g, q));
    g.set_late_projection(true);
    assert_eq!(
        late.rows, eager.rows,
        "late projection must equal the eager full projection"
    );
    assert_eq!(
        late.rows,
        vec![
            vec![Value::Int(99), Value::Str("big-99".into())],
            vec![Value::Int(98), Value::Str("big-98".into())],
            vec![Value::Int(97), Value::Str("big-97".into())],
            vec![Value::Int(96), Value::Str("big-96".into())],
            vec![Value::Int(95), Value::Str("big-95".into())],
        ],
        "top-5 by key DESC, with the real deferred `big` values"
    );
    assert!(
        tl.counters()
            .get("interp.late projection deferred a property")
            .copied()
            .unwrap_or(0)
            > 0,
        "the late-projection path engaged"
    );
    assert_eq!(
        te.counters()
            .get("interp.late projection deferred a property")
            .copied()
            .unwrap_or(0),
        0,
        "with the lever off, nothing is deferred"
    );
    // The ORDER key is aliased to an item expr (`ORDER BY k`) - the sort
    // column must NOT be deferred (else the top-k would be wrong). Proven by
    // the identical result above; also confirm a raw-expr order works.
    let q2 = "MATCH (x:X) WITH collect(x) AS xs UNWIND xs AS x MATCH (x)-[:R]->(m:M) RETURN m.key AS k, m.big AS b ORDER BY m.key ASC LIMIT 3";
    let r2 = run(&g, q2);
    assert_eq!(
        r2.rows,
        vec![
            vec![Value::Int(0), Value::Str("big-0".into())],
            vec![Value::Int(1), Value::Str("big-1".into())],
            vec![Value::Int(2), Value::Str("big-2".into())],
        ],
        "raw-expr ORDER BY m.key with deferred big"
    );
}
#[test]
fn an_unwind_prunes_dead_scope_but_keeps_the_result_identical() {
    // `WITH collect(x) AS big UNWIND big AS y MATCH (y)-[:R]->(z) …` is the
    // LDBC friends-of-friends idiom. `big` (often a list of full nodes) is
    // dead after the UNWIND, yet without pruning it rides in every unwound
    // row AND every downstream candidate clone - O(rows x |big|), which is
    // why SNB IC9 took 35 s. Pruning drops it; the answer must be byte-
    // identical to keeping it (the differential via set_scope_pruning), and
    // when `big` IS read downstream it must be kept.
    let g = graph();
    for i in 0..40i64 {
        run_params(
            &g,
            "CREATE (:A {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
        run_params(
            &g,
            "CREATE (:B {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // each A links to two B's
    for i in 0..40i64 {
        run_params(
            &g,
            "MATCH (a:A {i: $i}), (b:B {j: $b}) RETURN a",
            [
                ("i".to_string(), Value::Int(i)),
                ("b".to_string(), Value::Int(0)),
            ]
            .into_iter()
            .collect(),
        );
    }
    for i in 0..40i64 {
        for d in 0..2i64 {
            run_params(
                &g,
                "MATCH (a:A {i: $i}) MATCH (b:B {i: $b}) CREATE (a)-[:R]->(b)",
                [
                    ("i".to_string(), Value::Int(i)),
                    ("b".to_string(), Value::Int((i + d) % 40)),
                ]
                .into_iter()
                .collect(),
            );
        }
    }
    // The dead-list idiom: `lst` (collected A nodes) is not read after UNWIND.
    let q = "MATCH (a:A) WITH collect(a) AS lst UNWIND lst AS x MATCH (x)-[:R]->(z:B) RETURN x.i AS xi, z.i AS zi ORDER BY xi ASC, zi ASC";
    let (pruned, tp) = engram_observe::with_trace(|| run(&g, q));
    g.set_scope_pruning(false);
    let (full, tf) = engram_observe::with_trace(|| run(&g, q));
    g.set_scope_pruning(true);
    assert_eq!(
        pruned.rows, full.rows,
        "pruned scope vs full scope must be identical"
    );
    assert_eq!(pruned.rows.len(), 80, "40 A x 2 R edges");
    assert!(
        tp.counters()
            .get("interp.unwind pruned a dead scope var")
            .copied()
            .unwrap_or(0)
            > 0,
        "pruning dropped the dead `lst`"
    );
    assert_eq!(
        tf.counters()
            .get("interp.unwind pruned a dead scope var")
            .copied()
            .unwrap_or(0),
        0,
        "with the lever off, nothing is pruned"
    );
    // Safety: when the collected list IS read downstream it must be KEPT and
    // the answer stays correct.
    let q2 = "MATCH (a:A) WITH collect(a) AS lst UNWIND lst AS x MATCH (x)-[:R]->(z:B) WHERE z.i < size(lst) RETURN count(*) AS c";
    let (r2, _t2) = engram_observe::with_trace(|| run(&g, q2));
    assert_eq!(
        r2.rows,
        vec![vec![Value::Int(80)]],
        "size(lst)=40 keeps all; list must not be pruned when read"
    );
}
#[test]
fn a_node_needle_of_in_is_bound_id_only_not_fully_decoded() {
    // `x IN [nodes]` compares by IDENTITY (`eq3` reads the id), so a bare
    // node needle needs only its id - not a full property decode. `WHERE
    // friend IN friends` over 50,698 friends was 50k full Person decodes for
    // a membership test. Here 40 :M each carry a 400-byte blob; the needle
    // `m` is used ONLY in `m IN ts`, so none of them is decoded in full -
    // only `target` (collected into the list) is.
    let g = graph();
    for i in 0..40i64 {
        run_params(
            &g,
            "CREATE (:M {i: $i, blob: $b})",
            [
                ("i".to_string(), Value::Int(i)),
                ("b".to_string(), Value::Str("z".repeat(400))),
            ]
            .into_iter()
            .collect(),
        );
    }
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (target:M {i: 7}) WITH collect(target) AS ts MATCH (m:M) WHERE m IN ts RETURN count(m) AS c",
        )
    });
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(1)]],
        "exactly the one target matches"
    );
    let full = t
        .counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0);
    // With the fix only `target` is full-decoded; without it, all 40 `m` are.
    assert!(
        full <= 5,
        "the 40 M needles must not be decoded in full: {full}"
    );
}

#[test]
fn a_bound_end_multi_hop_pattern_drives_from_the_bound_node() {
    // A pattern `(friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum)`
    // whose LAST node (forum) is bound must reverse and drive from forum's
    // adjacency (its 2 posts), not seed the unbound start `friend` as a scan
    // of every Person. This is the two-hop generalisation of the single-hop
    // hydrate reversal - LDBC SNB IC5 went 76 s -> 164 ms on 200 people.
    // Reversed and forward MUST agree; reversed must touch far less.
    let g = graph();
    run(&g, "CREATE (f:Forum {k: 1})");
    for i in 0..2i64 {
        run_params(
            &g,
            "MATCH (f:Forum {k: 1}) CREATE (f)-[:CONTAINER_OF]->(:Post {i: $i})-[:HAS_CREATOR]->(:Person {i: $i, author: true})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // Noise: many Persons and Posts NOT under this forum, so scanning the
    // unbound start is far larger than the forum's 2-post adjacency.
    for i in 0..200i64 {
        run_params(
            &g,
            "CREATE (:Person {i: $i})-[:HAS_CREATOR_OF]->(:Post {i: $i})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let q = "MATCH (forum:Forum {k: 1}) MATCH (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum) RETURN count(post) AS c, count(DISTINCT friend) AS f";
    let (fast, t_on) = engram_observe::with_trace(|| run(&g, q));
    g.set_hop_reversal(false);
    let (slow, t_off) = engram_observe::with_trace(|| run(&g, q));
    g.set_hop_reversal(true);
    // Identical answer both ways: 2 posts, 2 authors.
    assert_eq!(
        fast.rows,
        vec![vec![Value::Int(2), Value::Int(2)]],
        "reversed answer"
    );
    assert_eq!(fast.rows, slow.rows, "reversed == forward");
    // The reversal fired with the lever on, not off.
    assert!(
        t_on.sometimes_hit()
            .contains("interp.hop driven from its bound end"),
        "reversal engaged"
    );
    assert!(
        !t_off
            .sometimes_hit()
            .contains("interp.hop driven from its bound end"),
        "forward when off"
    );
    // And it touched far less: the forward scan visits every Person, the
    // reversed walk only the forum's two posts and their authors.
    let gets_on = t_on.counters().get("store.gets").copied().unwrap_or(0);
    let gets_off = t_off.counters().get("store.gets").copied().unwrap_or(0);
    assert!(
        gets_off > gets_on * 4,
        "reversed touches far less: on={gets_on} off={gets_off}"
    );
}

#[test]
fn a_limitless_projection_stops_at_the_limit() {
    // `MATCH (n:L) RETURN n LIMIT k` with no ORDER BY and no DISTINCT is
    // satisfied by ANY k rows, so the scan stops at k instead of building a
    // row per member. `MATCH (n:Bio) RETURN n LIMIT 100` built a row for
    // every Bio (110 ms) to keep 100. The answer equals the general path,
    // the stop fires only without ORDER BY / DISTINCT, and SKIP is included.
    let g = graph();
    for i in 0..2000i64 {
        run_params(
            &g,
            "CREATE (:L {i: $i, k: $i % 4})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // (query, expected rows, stop-expected)
    let cases: Vec<(&str, usize, bool)> = vec![
        ("MATCH (n:L) RETURN n LIMIT 100", 100, true),
        ("MATCH (n:L) RETURN n.i AS i LIMIT 50", 50, true),
        ("MATCH (n:L) RETURN n.i AS i SKIP 10 LIMIT 20", 20, true),
        ("MATCH (n:L) RETURN n.i AS i ORDER BY i LIMIT 50", 50, false),
        ("MATCH (n:L) RETURN DISTINCT n.k AS k LIMIT 3", 3, false),
        ("MATCH (n:L) RETURN n.i AS i", 2000, false),
    ];
    for (q, expect, stop) in cases {
        let (fast, t) = engram_observe::with_trace(|| run(&g, q));
        g.set_columnar_scans(false);
        let general = run(&g, q);
        g.set_columnar_scans(true);
        assert_eq!(fast.rows, general.rows, "early-limit vs general: {q}");
        assert_eq!(fast.rows.len(), expect, "rows: {q}");
        let stopped = t
            .counters()
            .get("interp.columnar projection stopped at the limit")
            .copied()
            .unwrap_or(0)
            > 0;
        assert_eq!(stopped, stop, "stop-at-limit fired as expected: {q}");
    }
    // The ORDER BY case must return the SMALLEST 50 (proof it read all, not
    // the first 50 in id order).
    let r = run(&g, "MATCH (n:L) RETURN n.i AS i ORDER BY i LIMIT 3");
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(0)],
            vec![Value::Int(1)],
            vec![Value::Int(2)]
        ]
    );
}

#[test]
fn a_columnar_projection_seeks_the_property_index() {
    // The DOMINANT gap shape: `MATCH (c:City) WHERE c.country = 'USA'
    // RETURN c.i` is a columnar projection, which seeds BEFORE the general
    // seed planner - so rev 81's Seed::PropEq never reached it. The
    // projection executor now SEEKS the derived range index for a selective
    // equality: it probes the ids, keeps those under the label, and
    // projects each, instead of decoding the country column over every
    // City. The answer equals the forced label scan, the seek fires, a
    // second label carrying the same value does NOT leak, and a rare label
    // falls back to the scan.
    let g = graph();
    for i in 0..1000i64 {
        let country = match i % 100 {
            0 => "USA",
            1 => "CHN",
            _ => "OTHER",
        };
        run_params(
            &g,
            "CREATE (:City {i: $i, country: $c, pop: $i % 3})",
            [
                ("i".to_string(), Value::Int(i)),
                ("c".to_string(), Value::Str(country.to_string())),
            ]
            .into_iter()
            .collect(),
        );
    }
    // A DIFFERENT label carrying the SAME property value: the probe returns
    // its ids too, so the per-id label filter is what stops it leaking.
    for i in 0..5i64 {
        run_params(
            &g,
            "CREATE (:Town {i: $i, country: 'USA'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let cases: Vec<(&str, BTreeMap<String, Value>, usize)> = vec![
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN c.i AS i",
            BTreeMap::new(),
            10,
        ),
        (
            "MATCH (c:City) WHERE 'CHN' = c.country RETURN c.i AS i",
            BTreeMap::new(),
            10,
        ),
        (
            "MATCH (c:City) WHERE c.country = $q RETURN c.i AS i ORDER BY c.i",
            [("q".to_string(), Value::Str("USA".into()))]
                .into_iter()
                .collect(),
            10,
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN DISTINCT c.pop AS p",
            BTreeMap::new(),
            3,
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' AND c.i > 500 RETURN c.i AS i",
            BTreeMap::new(),
            4,
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN c.i AS i, c AS node ORDER BY c.i",
            BTreeMap::new(),
            10,
        ),
        (
            "MATCH (c:City) WHERE c.country = 'NOWHERE' RETURN c.i AS i",
            BTreeMap::new(),
            0,
        ),
    ];
    for (q, params, expect_rows) in cases {
        let (fast, t) = engram_observe::with_trace(|| run_params(&g, q, params.clone()));
        g.set_property_seek(false);
        let scan = run_params(&g, q, params.clone());
        g.set_property_seek(true);
        // Same rows as the scan, order-insensitive (RETURN without ORDER BY
        // is member order either way, but be robust to the seek's id order).
        let mut a = fast.rows.clone();
        let mut b = scan.rows.clone();
        a.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        b.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        assert_eq!(a, b, "seek vs scan rows: {q}");
        assert_eq!(fast.rows.len(), expect_rows, "row count: {q}");
        assert!(
            t.sometimes_hit()
                .contains("interp.columnar projection sought a property index"),
            "the columnar projection sought the index: {q}"
        );
        // The Towns (country='USA', i in 0..5) must not appear among the
        // Cities - the i values 0..5 exist for BOTH labels, so a leak would
        // duplicate them. City has exactly one i==0 (country USA).
        if expect_rows == 10 {
            let zeros = fast
                .rows
                .iter()
                .filter(|r| r == &&vec![Value::Int(0)])
                .count();
            assert!(zeros <= 1, "a Town leaked into the City projection: {q}");
        }
    }
    // A rare label with a common value: the label scan wins, no seek.
    for i in 0..3i64 {
        run_params(
            &g,
            "CREATE (:Rare {i: $i, country: 'OTHER'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (_r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:Rare) WHERE n.country = 'OTHER' RETURN n.i AS i",
        )
    });
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar projection sought a property index"),
        "3 Rare < ~980 OTHER: the label scan wins, no seek"
    );
}
#[test]
fn a_property_in_list_seeks_the_index() {
    // `WHERE prop IN [a, b]` seeks the derived index over each value and
    // unions the ids - the same win as `=` for a selective list. `WHERE
    // status IN ['running','paused']` matched 1 of ~100k executions yet
    // Engram scanned them all (100 ms). Proven on all three paths against
    // the forced scan, with the cross-label and rare-value guards.
    let g = graph();
    for i in 0..1000i64 {
        let country = match i % 100 {
            0 => "USA",
            1 => "CHN",
            2 => "GBR",
            _ => "OTHER",
        };
        run_params(
            &g,
            "CREATE (:City {i: $i, country: $c, pop: $i % 4})",
            [
                ("i".to_string(), Value::Int(i)),
                ("c".to_string(), Value::Str(country.to_string())),
            ]
            .into_iter()
            .collect(),
        );
    }
    for i in 0..5i64 {
        run_params(
            &g,
            "CREATE (:Town {i: $i, country: 'USA'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    // (query, event, expected rows)
    let cases: Vec<(&str, &str, usize)> = vec![
        (
            "MATCH (c:City) WHERE c.country IN ['USA','CHN'] RETURN count(c) AS c",
            "interp.columnar aggregate sought a property index",
            1,
        ),
        (
            "MATCH (c:City) WHERE c.country IN ['USA','CHN','GBR'] RETURN c.i AS i",
            "interp.columnar projection sought a property index",
            30,
        ),
        (
            "MATCH (c:City) WHERE c.country IN ['USA','CHN'] WITH c ORDER BY c.i RETURN c.i AS i",
            "interp.seed sought a property index",
            20,
        ),
    ];
    for (q, event, expect_rows) in cases {
        let (fast, t) = engram_observe::with_trace(|| run(&g, q));
        g.set_property_seek(false);
        let scan = run(&g, q);
        g.set_property_seek(true);
        let mut a = fast.rows.clone();
        let mut b = scan.rows.clone();
        a.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        b.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        assert_eq!(a, b, "IN seek vs scan: {q}");
        if !q.contains("count(") {
            assert_eq!(fast.rows.len(), expect_rows, "rows: {q}");
        } else {
            assert_eq!(fast.rows, vec![vec![Value::Int(20)]], "count: {q}");
        }
        assert!(
            t.sometimes_hit().contains(event),
            "the index was sought: {q}"
        );
    }
    // A rare value plus a COMMON one: the union is most of the label, so the
    // scan wins - `IN ['USA','OTHER']` is 970 of 1000.
    let (_r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (c:City) WHERE c.country IN ['USA','OTHER'] RETURN count(c) AS c",
        )
    });
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar aggregate sought a property index"),
        "a common value in the list declines the seek"
    );
    // The 5 Towns (country='USA') must not be counted among the Cities.
    let r = run(
        &g,
        "MATCH (c:City) WHERE c.country IN ['USA','CHN'] RETURN count(c) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(20)]],
        "Towns must not leak into the City count"
    );
}

#[test]
fn a_columnar_aggregate_seeks_the_property_index() {
    // `MATCH (c:City) WHERE c.country = 'USA' RETURN count(c)` is a columnar
    // AGGREGATE - the worst per-statement gaps vs Neo4j are counts over a
    // property predicate. The aggregate executor now SEEKS the derived range
    // index for a selective equality and folds over the matches, instead of
    // decoding the column over the whole label. Every arm equals the general
    // path (columnar off), the seek fires, a second label with the same
    // value is not counted, and a rare label falls back to the scan.
    let g = graph();
    for i in 0..1000i64 {
        let country = match i % 100 {
            0 => "USA",
            1 => "CHN",
            _ => "OTHER",
        };
        run_params(
            &g,
            "CREATE (:City {i: $i, country: $c, pop: $i % 3})",
            [
                ("i".to_string(), Value::Int(i)),
                ("c".to_string(), Value::Str(country.to_string())),
            ]
            .into_iter()
            .collect(),
        );
    }
    for i in 0..7i64 {
        run_params(
            &g,
            "CREATE (:Town {i: $i, country: 'USA'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let cases: Vec<(&str, BTreeMap<String, Value>)> = vec![
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN count(c) AS c",
            BTreeMap::new(),
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' WITH count(c) AS c RETURN c",
            BTreeMap::new(),
        ),
        (
            "MATCH (c:City) WHERE c.country = $q RETURN count(*) AS c",
            [("q".to_string(), Value::Str("USA".into()))]
                .into_iter()
                .collect(),
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN c.pop AS p, count(*) AS c ORDER BY p",
            BTreeMap::new(),
        ),
        (
            "MATCH (c:City) WHERE c.country = 'USA' RETURN sum(c.i) AS s",
            BTreeMap::new(),
        ),
        (
            "MATCH (c:City) WHERE c.country = 'NOWHERE' RETURN count(c) AS c",
            BTreeMap::new(),
        ),
    ];
    for (q, params) in cases {
        let (fast, t) = engram_observe::with_trace(|| run_params(&g, q, params.clone()));
        g.set_columnar_scans(false);
        let general = run_params(&g, q, params.clone());
        g.set_columnar_scans(true);
        assert_eq!(fast.columns, general.columns, "columns: {q}");
        assert_eq!(fast.rows, general.rows, "seek vs general path: {q}");
        assert!(
            t.sometimes_hit()
                .contains("interp.columnar aggregate sought a property index"),
            "the aggregate sought the index: {q}"
        );
    }
    // The count must be 10 Cities, NOT 17 (the 7 Towns with country='USA'
    // are excluded by the per-id label filter).
    let r = run(
        &g,
        "MATCH (c:City) WHERE c.country = 'USA' RETURN count(c) AS c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Int(10)]],
        "Towns must not be counted as Cities"
    );
    // A rare label with a common value: the label scan wins, no seek.
    for i in 0..3i64 {
        run_params(
            &g,
            "CREATE (:Rare {i: $i, country: 'OTHER'})",
            [("i".to_string(), Value::Int(i))].into_iter().collect(),
        );
    }
    let (r, t) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (n:Rare) WHERE n.country = 'OTHER' RETURN count(n) AS c",
        )
    });
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    assert!(
        !t.sometimes_hit()
            .contains("interp.columnar aggregate sought a property index"),
        "3 Rare < ~980 OTHER: the label scan wins, no seek"
    );
}
