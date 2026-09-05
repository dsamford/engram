#![allow(non_snake_case)]
//! Schema — constraints enforced at the write, and the two live procedures.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, GraphError, QueryResult, RunError, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) -> QueryResult {
    try_run(g, src).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn try_run(g: &Graph, src: &str) -> Result<QueryResult, RunError> {
    let stmt = parse_any(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_stmt(g, &stmt, BTreeMap::new())
}

// ─── Vector search ──────────────────────────────────────────────────────────

#[test]
fn vector_query_ranks_by_cosine_and_caps_at_k() {
    let g = graph();
    run(
        &g,
        "CREATE VECTOR INDEX embeddings FOR (d:Doc) ON (d.embedding)",
    );
    run(&g, "CREATE (:Doc {name: 'x', embedding: [1.0, 0.0]})");
    run(&g, "CREATE (:Doc {name: 'diag', embedding: [1.0, 1.0]})");
    run(&g, "CREATE (:Doc {name: 'y', embedding: [0.0, 1.0]})");

    let r = run(
        &g,
        "CALL db.index.vector.queryNodes('embeddings', 2, [1.0, 0.1]) YIELD node, score \
         RETURN node.name, score",
    );
    assert_eq!(r.rows.len(), 2, "k caps the answer");
    assert_eq!(
        r.rows[0][0],
        Value::Str("x".into()),
        "the aligned vector wins"
    );
    assert_eq!(r.rows[1][0], Value::Str("diag".into()));
    let (Value::Float(s0), Value::Float(s1)) = (&r.rows[0][1], &r.rows[1][1]) else {
        panic!()
    };
    assert!(s0 > s1, "scores descend");
    assert!(
        (*s0 - 0.995_037).abs() < 1e-5,
        "the score IS the cosine: {s0}"
    );
}

#[test]
fn vector_query_skips_wrong_dimension_rows_instead_of_scoring_them() {
    // A different dimension is a DIFFERENT EMBEDDING SPACE — scoring it
    // would rank incomparable numbers (the X2 lesson at the procedure).
    let g = graph();
    run(&g, "CREATE VECTOR INDEX emb FOR (d:D2) ON (d.v)");
    run(&g, "CREATE (:D2 {name: 'ok', v: [1.0, 0.0]})");
    run(&g, "CREATE (:D2 {name: 'threedee', v: [1.0, 0.0, 0.0]})");
    run(&g, "CREATE (:D2 {name: 'no-vector'})");
    let r = run(
        &g,
        "CALL db.index.vector.queryNodes('emb', 10, [1.0, 0.0]) YIELD node RETURN node.name",
    );
    assert_eq!(r.rows, vec![vec![Value::Str("ok".into())]]);
}

#[test]
fn yields_alias_and_where_filter() {
    let g = graph();
    run(&g, "CREATE VECTOR INDEX vi FOR (d:D3) ON (d.v)");
    run(
        &g,
        "CREATE (:D3 {name: 'a', v: [1.0, 0.0]}), (:D3 {name: 'b', v: [-1.0, 0.0]})",
    );
    let r = run(
        &g,
        "CALL db.index.vector.queryNodes('vi', 10, [1.0, 0.0]) YIELD node AS n, score \
         WHERE score > 0.5 RETURN n.name",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Str("a".into())]],
        "the anti-aligned vector filtered"
    );
}

#[test]
fn a_missing_index_refuses_BY_NAME() {
    let g = graph();
    match try_run(
        &g,
        "CALL db.index.vector.queryNodes('nope', 1, [1.0]) YIELD node RETURN node",
    ) {
        Err(RunError::Graph(GraphError::SchemaConflict(d))) => {
            assert!(d.contains("nope"), "{d}")
        }
        other => panic!("expected the named refusal, got {other:?}"),
    }
}

// ─── Fulltext ───────────────────────────────────────────────────────────────

#[test]
fn fulltext_matches_terms_across_labels_and_orders_by_score() {
    let g = graph();
    run(
        &g,
        "CREATE FULLTEXT INDEX ft FOR (n:Article|Note) ON EACH [n.title, n.body]",
    );
    run(
        &g,
        "CREATE (:Article {title: 'rust rust rust', body: 'engines'})",
    );
    run(&g, "CREATE (:Note {title: 'a rust note', body: 'nothing'})");
    run(
        &g,
        "CREATE (:Article {title: 'unrelated', body: 'cooking'})",
    );
    let r = run(
        &g,
        "CALL db.index.fulltext.queryNodes('ft', 'rust') YIELD node, score RETURN node.title, score",
    );
    assert_eq!(
        r.rows.len(),
        2,
        "the unrelated row is ABSENT, not zero-scored"
    );
    assert_eq!(
        r.rows[0][0],
        Value::Str("rust rust rust".into()),
        "term frequency ranks"
    );
    let r = run(
        &g,
        "CALL db.index.fulltext.queryNodes('ft', 'zzz') YIELD node RETURN node",
    );
    assert!(r.rows.is_empty());
}

// ─── Constraints ────────────────────────────────────────────────────────────

#[test]
fn uniqueness_is_enforced_at_create_set_and_via_merge() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS UNIQUE",
    );
    run(&g, "CREATE (:User {email: 'a@x'})");
    // A duplicate CREATE refuses.
    match try_run(&g, "CREATE (:User {email: 'a@x'})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    // A SET that would duplicate refuses (the POST-image is what matters).
    run(&g, "CREATE (:User {email: 'b@x'})");
    match try_run(&g, "MATCH (u:User {email: 'b@x'}) SET u.email = 'a@x'") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    // Setting a node's value to ITSELF is not a self-collision.
    run(&g, "MATCH (u:User {email: 'b@x'}) SET u.email = 'b@x'");
    // MERGE converges on the existing node instead of violating.
    run(
        &g,
        "MERGE (u:User {email: 'a@x'}) ON MATCH SET u.matched = true",
    );
    let r = run(
        &g,
        "MATCH (u:User {email: 'a@x'}) RETURN count(*), max(u.matched)",
    );
    assert_eq!(r.rows[0], vec![Value::Int(1), Value::Bool(true)]);
}

#[test]
fn constraint_creation_validates_the_EXISTING_population() {
    // A constraint enforced only forward would certify a uniqueness that
    // does not hold — creation over violating data must refuse.
    let g = graph();
    run(&g, "CREATE (:Dup {k: 1}), (:Dup {k: 1})");
    match try_run(&g, "CREATE CONSTRAINT FOR (d:Dup) REQUIRE d.k IS UNIQUE") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
}

#[test]
fn not_null_is_enforced_on_create_set_and_label_addition() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.name IS NOT NULL",
    );
    match try_run(&g, "CREATE (:Person {age: 3})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    run(&g, "CREATE (:Person {name: 'ok'})");
    match try_run(&g, "MATCH (p:Person) SET p.name = null") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    // Adding the label to a node LACKING the property refuses.
    run(&g, "CREATE (:Bare {x: 1})");
    match try_run(&g, "MATCH (b:Bare) SET b:Person") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
}

#[test]
fn if_not_exists_and_drop() {
    let g = graph();
    run(&g, "CREATE CONSTRAINT c1 FOR (a:A) REQUIRE a.k IS UNIQUE");
    run(
        &g,
        "CREATE CONSTRAINT c1 IF NOT EXISTS FOR (a:A) REQUIRE a.k IS UNIQUE",
    );
    match try_run(&g, "CREATE CONSTRAINT c1 FOR (a:A) REQUIRE a.k IS UNIQUE") {
        Err(RunError::Graph(GraphError::SchemaConflict(_))) => {}
        other => panic!("expected the conflict, got {other:?}"),
    }
    run(&g, "DROP CONSTRAINT c1");
    run(&g, "DROP CONSTRAINT c1 IF EXISTS");
    match try_run(&g, "DROP CONSTRAINT c1") {
        Err(RunError::Graph(GraphError::SchemaConflict(_))) => {}
        other => panic!("expected the conflict, got {other:?}"),
    }
    // The dropped constraint stops constraining.
    run(&g, "CREATE (:A {k: 1}), (:A {k: 1})");
}

#[test]
fn the_two_label_FOR_defect_refuses_at_parse() {
    // A real deployment's vector indexes were declared FOR (m:Bio:Protein) —
    // a Cypher syntax error swallowed by a debug-logged catch, so ~44
    // "created" indexes never existed. The refusal must NAME the shape.
    let e = parse_any("CREATE VECTOR INDEX v FOR (m:Bio:Protein) ON (m.embedding)").unwrap_err();
    assert!(format!("{e}").contains("two-label"), "{e}");
}

#[test]
fn range_index_ddl_is_accepted_and_stored() {
    let g = graph();
    run(&g, "CREATE INDEX idx_a FOR (a:A) ON (a.x, a.y)");
    run(
        &g,
        "CREATE INDEX idx_a IF NOT EXISTS FOR (a:A) ON (a.x, a.y)",
    );
    run(&g, "DROP INDEX idx_a");
}

#[test]
fn unknown_procedures_still_refuse_by_name() {
    let g = graph();
    // `db.labels` was the specimen until R-5 implemented it (see
    // tests/introspection_procedures.rs); the property — unsupported
    // procedures refuse and the refusal NAMES the procedure — is unchanged.
    match try_run(&g, "CALL db.schema.visualization() YIELD nodes RETURN nodes") {
        Err(RunError::Unsupported(w)) => assert!(w.contains("db.schema.visualization")),
        other => panic!("expected the named refusal, got {other:?}"),
    }
}

#[test]
fn composite_uniqueness_is_a_TUPLE_and_nulls_exempt() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR (p:Pol) REQUIRE (p.orgId, p.policyId) IS UNIQUE",
    );
    run(&g, "CREATE (:Pol {orgId: 'a', policyId: 1})");
    // Same org, different policy: fine — the TUPLE is the identity.
    run(&g, "CREATE (:Pol {orgId: 'a', policyId: 2})");
    // The same tuple refuses.
    match try_run(&g, "CREATE (:Pol {orgId: 'a', policyId: 1})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    // A null component EXEMPTS the row (Neo4j's rule).
    run(&g, "CREATE (:Pol {orgId: 'a'})");
    run(&g, "CREATE (:Pol {orgId: 'a'})");
}

#[test]
fn node_key_requires_every_component_AND_the_tuple() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR (k:Keyed) REQUIRE (k.a, k.b) IS NODE KEY",
    );
    run(&g, "CREATE (:Keyed {a: 1, b: 1})");
    match try_run(&g, "CREATE (:Keyed {a: 1})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("NODE KEY requires every component, got {other:?}"),
    }
    match try_run(&g, "CREATE (:Keyed {a: 1, b: 1})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("NODE KEY requires tuple uniqueness, got {other:?}"),
    }
}

// ─── Relationship constraints ───────────────────────────────────────────────
//
// Before this, `CREATE CONSTRAINT FOR ()-[r:T]-() REQUIRE …` was refused at
// parse — relationship property integrity could not be declared at all. The
// hazard the scope tests below pin: a rel constraint stored against the node
// population (or the reverse) enforces over the WRONG set and silently never
// fires, certifying an integrity rule that does not hold.

#[test]
fn relationship_constraint_forms_parse() {
    parse_any("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.p IS NOT NULL").expect("existence");
    parse_any("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.p IS UNIQUE").expect("uniqueness");
    parse_any("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE (r.a, r.b) IS RELATIONSHIP KEY")
        .expect("relationship key");
    // The node form is unchanged.
    parse_any("CREATE CONSTRAINT FOR (n:N) REQUIRE n.p IS UNIQUE").expect("node form still parses");
}

#[test]
fn relationship_existence_is_enforced_at_create_and_set() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR ()-[r:RATED]-() REQUIRE r.score IS NOT NULL",
    );
    // An edge of the type without the property refuses.
    match try_run(&g, "CREATE (:U)-[:RATED]->(:M)") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation on CREATE, got {other:?}"),
    }
    // With it, fine.
    run(&g, "CREATE (:U)-[:RATED {score: 5}]->(:M)");
    // Setting it to null refuses — the POST-image is what must satisfy it.
    match try_run(&g, "MATCH ()-[r:RATED]->() SET r.score = null") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation on SET null, got {other:?}"),
    }
}

#[test]
fn relationship_uniqueness_is_enforced_nulls_exempt_and_self_is_not_a_collision() {
    let g = graph();
    run(
        &g,
        "CREATE CONSTRAINT FOR ()-[r:PAIR]-() REQUIRE r.code IS UNIQUE",
    );
    run(&g, "CREATE (:N)-[:PAIR {code: 'x'}]->(:N)");
    // A second edge with the same code refuses.
    match try_run(&g, "CREATE (:N)-[:PAIR {code: 'x'}]->(:N)") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation, got {other:?}"),
    }
    // A different code is fine.
    run(&g, "CREATE (:N)-[:PAIR {code: 'y'}]->(:N)");
    // Setting an edge's value to ITSELF is not a self-collision.
    run(
        &g,
        "MATCH ()-[r:PAIR]->() WHERE r.code = 'y' SET r.code = 'y'",
    );
    // A null component EXEMPTS the row (Neo4j's rule) — two are allowed.
    run(&g, "CREATE (:N)-[:PAIR]->(:N)");
    run(&g, "CREATE (:N)-[:PAIR]->(:N)");
}

#[test]
fn relationship_constraint_creation_validates_the_existing_population() {
    let g = graph();
    run(&g, "CREATE (:N)-[:DUP {k: 1}]->(:N)");
    run(&g, "CREATE (:N)-[:DUP {k: 1}]->(:N)");
    // A uniqueness constraint over already-duplicate edges must refuse.
    match try_run(
        &g,
        "CREATE CONSTRAINT FOR ()-[r:DUP]-() REQUIRE r.k IS UNIQUE",
    ) {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the violation over existing edges, got {other:?}"),
    }
    // An existence constraint over an edge lacking the property also refuses.
    run(&g, "CREATE (:N)-[:NEED]->(:N)");
    match try_run(
        &g,
        "CREATE CONSTRAINT FOR ()-[r:NEED]-() REQUIRE r.k IS NOT NULL",
    ) {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("expected the existence violation over existing edges, got {other:?}"),
    }
}

#[test]
fn a_node_constraint_does_not_fire_on_a_relationship_of_the_same_name() {
    let g = graph();
    // ONLY a node constraint on `LINK`.
    run(&g, "CREATE CONSTRAINT FOR (n:LINK) REQUIRE n.w IS NOT NULL");
    // A relationship of type LINK with no `w` must be ALLOWED — the node
    // constraint's population is nodes, not edges. (A rel constraint stored
    // against the node label, or a node constraint enforced over edges, is the
    // silent-no-op this scoping exists to prevent.)
    run(&g, "CREATE (:A)-[:LINK]->(:A)");
    // The node constraint still fires on an actual node.
    match try_run(&g, "CREATE (:LINK {x: 1})") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("node constraint must still fire on its node, got {other:?}"),
    }
}

#[test]
fn a_rel_constraint_does_not_fire_on_a_node_of_the_same_name() {
    let g = graph();
    // ONLY a relationship constraint on type `LINK`.
    run(
        &g,
        "CREATE CONSTRAINT FOR ()-[r:LINK]-() REQUIRE r.w IS NOT NULL",
    );
    // A node labelled LINK with no `w` must be ALLOWED — the rel constraint's
    // population is edges.
    run(&g, "CREATE (:LINK {x: 1})");
    // The rel constraint fires on an actual relationship.
    match try_run(&g, "CREATE (:A)-[:LINK]->(:A)") {
        Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
        other => panic!("rel constraint must fire on its edge, got {other:?}"),
    }
}

#[test]
fn pattern_predicates_and_label_predicates_run_against_the_graph() {
    let g = graph();
    run(&g, "CREATE (a:Auth {n: 'linked'})-[:AUTHORED_BY]->(:Doc)");
    run(&g, "CREATE (:Auth {n: 'orphan'})");
    // The corpus shape: WHERE NOT (a)-[:X]->() — prune the unlinked.
    let r = run(
        &g,
        "MATCH (a:Auth) WHERE NOT (a)-[:AUTHORED_BY]->() RETURN a.n",
    );
    assert_eq!(r.rows, vec![vec![Value::Str("orphan".into())]]);
    // And the label predicate: WHERE n:Label OR n:Other.
    run(&g, "CREATE (:Mixed:Position {v: 1}), (:Mixed {v: 2})");
    let r = run(&g, "MATCH (n:Mixed) WHERE n:Position RETURN n.v");
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn show_with_a_tail_refuses_and_rel_index_ddl_is_accepted() {
    let g = graph();
    // Bare SHOW CONSTRAINTS answers now (see show_schema.rs) — but a
    // YIELD/WHERE tail is swallowed unvalidated by the parser, so answering
    // would silently ignore the projection. It must refuse, naming the
    // subject.
    match try_run(&g, "SHOW CONSTRAINTS YIELD name RETURN name") {
        Err(RunError::Graph(GraphError::SchemaConflict(d))) => {
            assert!(d.contains("SHOW CONSTRAINTS"), "{d}")
        }
        other => panic!("expected the named refusal, got {other:?}"),
    }
    run(
        &g,
        "CREATE INDEX rel_idx FOR ()-[r:VIA]-() ON (r.relationId)",
    );
    run(&g, "DROP INDEX rel_idx");
}
