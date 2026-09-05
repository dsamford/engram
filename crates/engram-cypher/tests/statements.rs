#![allow(non_snake_case)]
//! The clause grammar — corpus-shaped statements parsed to inspectable ASTs.

use engram_cypher::{
    Clause, Expr, ParseError, Query, RelDir, SetItem, SubqueryBody, VarLength, parse_statement,
};

fn single(src: &str) -> Vec<Clause> {
    match parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")) {
        Query::Single(q) => q.clauses,
        other => panic!("expected a single query, got {other:?}"),
    }
}

#[test]
fn a_full_read_query_parses_with_every_piece_in_place() {
    let clauses = single(
        "MATCH (n:User {id: $id})-[r:OWNS]->(m:Doc) WHERE m.title CONTAINS 'x' \
         RETURN n.name AS name, m ORDER BY m.title DESC SKIP 1 LIMIT 10",
    );
    assert_eq!(clauses.len(), 2);
    let Clause::Match {
        optional,
        pattern,
        where_,
    } = &clauses[0]
    else {
        panic!("expected MATCH, got {:?}", clauses[0]);
    };
    assert!(!optional);
    assert!(where_.is_some());
    let path = &pattern.paths[0];
    assert_eq!(path.start.var.as_deref(), Some("n"));
    assert_eq!(path.start.labels, vec!["User"]);
    assert!(path.start.props.is_some());
    let (rel, end) = &path.hops[0];
    assert_eq!(rel.var.as_deref(), Some("r"));
    assert_eq!(rel.types, vec!["OWNS"]);
    assert_eq!(rel.dir, RelDir::Out);
    assert_eq!(end.labels, vec!["Doc"]);
    let Clause::Return { proj } = &clauses[1] else {
        panic!("expected RETURN");
    };
    assert_eq!(proj.items.len(), 2);
    assert_eq!(proj.items[0].alias.as_deref(), Some("name"));
    assert_eq!(proj.order.len(), 1);
    assert!(proj.order[0].desc);
    assert!(proj.skip.is_some() && proj.limit.is_some());
}

#[test]
fn multi_label_multi_type_and_undirected() {
    let clauses = single("MATCH (m:Bio:Protein)-[:BINDS|INHIBITS|ACTIVATES]-(x) RETURN x");
    let Clause::Match { pattern, .. } = &clauses[0] else {
        panic!()
    };
    let path = &pattern.paths[0];
    assert_eq!(
        path.start.labels,
        vec!["Bio", "Protein"],
        "multi-label is an AND"
    );
    let (rel, _) = &path.hops[0];
    assert_eq!(
        rel.types,
        vec!["BINDS", "INHIBITS", "ACTIVATES"],
        "multi-type is an OR"
    );
    assert_eq!(rel.dir, RelDir::Undirected);
    assert!(rel.var.is_none());
}

#[test]
fn variable_length_forms() {
    let forms = [
        (
            "*",
            VarLength {
                min: None,
                max: None,
            },
        ),
        (
            "*2",
            VarLength {
                min: Some(2),
                max: Some(2),
            },
        ),
        (
            "*1..3",
            VarLength {
                min: Some(1),
                max: Some(3),
            },
        ),
        (
            "*..5",
            VarLength {
                min: None,
                max: Some(5),
            },
        ),
        (
            "*2..",
            VarLength {
                min: Some(2),
                max: None,
            },
        ),
    ];
    for (src, expect) in forms {
        let clauses = single(&format!("MATCH (a)-[{src}]->(b) RETURN b"));
        let Clause::Match { pattern, .. } = &clauses[0] else {
            panic!()
        };
        assert_eq!(
            pattern.paths[0].hops[0].0.length,
            Some(expect),
            "form `{src}`"
        );
    }
    // `*2` meaning EXACTLY two is the trap: it is not `*2..`.
    assert_ne!(
        VarLength {
            min: Some(2),
            max: Some(2)
        },
        VarLength {
            min: Some(2),
            max: None
        }
    );
}

#[test]
fn the_one_shortest_path_shape_in_the_corpus() {
    let clauses = single("MATCH p = shortestPath((a:Place)-[*..20]-(b:Place)) RETURN p");
    let Clause::Match { pattern, .. } = &clauses[0] else {
        panic!()
    };
    let path = &pattern.paths[0];
    assert_eq!(path.var.as_deref(), Some("p"));
    assert!(path.shortest);
    assert_eq!(
        path.hops[0].0.length,
        Some(VarLength {
            min: None,
            max: Some(20)
        })
    );
}

#[test]
fn merge_with_both_on_arms() {
    let clauses = single(
        "MERGE (n:User {id: $id}) ON CREATE SET n.created = 1, n:Fresh ON MATCH SET n.seen = 2",
    );
    let Clause::Merge {
        path,
        on_create,
        on_match,
    } = &clauses[0]
    else {
        panic!()
    };
    assert_eq!(path.start.labels, vec!["User"]);
    assert_eq!(on_create.len(), 2);
    assert!(matches!(&on_create[0], SetItem::Prop { key, .. } if key == "created"));
    assert!(
        matches!(&on_create[1], SetItem::Labels { labels, .. } if labels == &vec!["Fresh".to_string()])
    );
    assert_eq!(on_match.len(), 1);
}

#[test]
fn all_four_set_forms() {
    let clauses = single("MATCH (n) SET n.a = 1, n = $props, n += {x: 1}, n:L1:L2");
    let Clause::Set { items } = &clauses[1] else {
        panic!()
    };
    assert!(matches!(&items[0], SetItem::Prop { key, .. } if key == "a"));
    assert!(matches!(&items[1], SetItem::Replace { var, .. } if var == "n"));
    assert!(matches!(&items[2], SetItem::Merge { var, .. } if var == "n"));
    assert!(matches!(&items[3], SetItem::Labels { labels, .. } if labels.len() == 2));
}

#[test]
fn remove_delete_and_detach() {
    let clauses = single("MATCH (n)-[r]->(m) REMOVE n.flag, n:Old DETACH DELETE n, m DELETE r");
    assert!(matches!(&clauses[1], Clause::Remove { items } if items.len() == 2));
    assert!(matches!(&clauses[2], Clause::Delete { detach: true, exprs } if exprs.len() == 2));
    assert!(matches!(&clauses[3], Clause::Delete { detach: false, .. }));
}

#[test]
fn with_distinct_where_and_aggregates_in_projection() {
    let clauses =
        single("MATCH (n) WITH DISTINCT n.kind AS kind, count(*) AS c WHERE c > 1 RETURN kind");
    let Clause::With { proj, where_ } = &clauses[1] else {
        panic!()
    };
    assert!(proj.distinct);
    assert!(matches!(&proj.items[1].expr, Expr::Call { star: true, .. }));
    assert!(where_.is_some());
}

#[test]
fn unwind_and_return_star() {
    let clauses = single("UNWIND $items AS item RETURN *, item");
    assert!(matches!(&clauses[0], Clause::Unwind { alias, .. } if alias == "item"));
    let Clause::Return { proj } = &clauses[1] else {
        panic!()
    };
    assert!(proj.star);
    assert_eq!(proj.items.len(), 1, "RETURN *, item carries both");
}

#[test]
fn union_and_union_all_and_the_mixing_refusal() {
    let q =
        parse_statement("RETURN 1 AS x UNION RETURN 2 AS x UNION RETURN 3 AS x").expect("parses");
    assert!(matches!(q, Query::Union { all: false, ref arms } if arms.len() == 3));
    let q = parse_statement("RETURN 1 AS x UNION ALL RETURN 1 AS x").expect("parses");
    assert!(matches!(q, Query::Union { all: true, .. }));
    let e = parse_statement("RETURN 1 UNION RETURN 2 UNION ALL RETURN 3").unwrap_err();
    assert!(matches!(e, ParseError::Unexpected { expected, .. } if expected.contains("mixing")));
}

#[test]
fn THE_conditional_write_idiom() {
    // 212 sites: FOREACH (_ IN CASE WHEN … THEN [1] ELSE [] END | …).
    let clauses =
        single("MATCH (n) FOREACH (_ IN CASE WHEN n.due THEN [1] ELSE [] END | SET n.flag = true)");
    let Clause::Foreach {
        var,
        source,
        updates,
    } = &clauses[1]
    else {
        panic!()
    };
    assert_eq!(var, "_");
    assert!(matches!(source, Expr::Case { .. }));
    assert!(matches!(&updates[0], Clause::Set { .. }));
}

#[test]
fn call_subquery_and_in_transactions() {
    let clauses = single("CALL { MATCH (n) RETURN n } IN TRANSACTIONS OF 100 ROWS");
    let Clause::CallSubquery {
        query,
        in_transactions,
        ..
    } = &clauses[0]
    else {
        panic!()
    };
    assert!(in_transactions);
    assert!(matches!(**query, Query::Single(_)));
    let clauses = single("CALL { CREATE (:Log) }");
    assert!(matches!(
        &clauses[0],
        Clause::CallSubquery {
            in_transactions: false,
            ..
        }
    ));
}

#[test]
fn the_two_live_procedures() {
    let clauses = single(
        "CALL db.index.vector.queryNodes('embeddings', 10, $v) YIELD node, score \
         WHERE score > 0.5 RETURN node",
    );
    let Clause::CallProcedure {
        name,
        args,
        yields,
        where_,
    } = &clauses[0]
    else {
        panic!()
    };
    assert_eq!(name, "db.index.vector.querynodes");
    assert_eq!(args.len(), 3);
    assert_eq!(
        yields,
        &vec![("node".to_string(), None), ("score".to_string(), None)]
    );
    assert!(where_.is_some());
    let clauses = single("CALL db.index.fulltext.queryNodes('idx', $q) YIELD node AS n RETURN n");
    let Clause::CallProcedure { yields, .. } = &clauses[0] else {
        panic!()
    };
    assert_eq!(yields, &vec![("node".to_string(), Some("n".to_string()))]);
}

#[test]
fn exists_and_count_subqueries_in_expressions() {
    let clauses = single("MATCH (n) WHERE EXISTS { (n)-[:OWNS]->(:Doc) } RETURN n");
    let Clause::Match {
        where_: Some(w), ..
    } = &clauses[0]
    else {
        panic!()
    };
    let Expr::ExistsSub(body) = w else {
        panic!("expected EXISTS {{}}, got {w:?}")
    };
    assert!(matches!(**body, SubqueryBody::Pattern { .. }));

    let clauses = single("MATCH (n) WHERE EXISTS { MATCH (n)-->(m) WHERE m.ok } RETURN n");
    let Clause::Match {
        where_: Some(Expr::ExistsSub(body)),
        ..
    } = &clauses[0]
    else {
        panic!()
    };
    assert!(
        matches!(**body, SubqueryBody::Query(_)),
        "a clause keyword makes it a subquery"
    );

    let clauses = single("MATCH (n) RETURN COUNT { (n)--() } AS degree");
    let Clause::Return { proj } = &clauses[1] else {
        panic!()
    };
    assert!(matches!(&proj.items[0].expr, Expr::CountSub(_)));
}

#[test]
fn pattern_comprehensions_and_the_list_disambiguation() {
    let clauses = single("MATCH (n) RETURN [ (n)-[:R]->(m) WHERE m.ok | m.name ] AS names");
    let Clause::Return { proj } = &clauses[1] else {
        panic!()
    };
    let Expr::PatternComp { path, filter, .. } = &proj.items[0].expr else {
        panic!(
            "expected a pattern comprehension, got {:?}",
            proj.items[0].expr
        );
    };
    assert_eq!(path.hops.len(), 1);
    assert!(filter.is_some());

    // A parenthesized expression in a list is STILL a list.
    let clauses = single("RETURN [(1 + 2), 3] AS xs");
    let Clause::Return { proj } = &clauses[0] else {
        panic!()
    };
    assert!(
        matches!(&proj.items[0].expr, Expr::List(items) if items.len() == 2),
        "got {:?}",
        proj.items[0].expr
    );
}

#[test]
fn bare_arrow_relationships_without_brackets() {
    for (src, dir) in [
        ("-->", RelDir::Out),
        ("<--", RelDir::In),
        ("--", RelDir::Undirected),
        // Arrowheads on BOTH ends are the redundant-but-legal UNDIRECTED spelling.
        ("<-->", RelDir::Undirected),
    ] {
        let clauses = single(&format!("MATCH (a){src}(b) RETURN b"));
        let Clause::Match { pattern, .. } = &clauses[0] else {
            panic!()
        };
        assert_eq!(pattern.paths[0].hops[0].0.dir, dir, "form `{src}`");
    }
}

#[test]
fn refusals_are_specific() {
    // `<-[:R]->` is UNDIRECTED (both arrowheads), not a refusal — see
    // `bare_arrow_relationships_without_brackets`.
    assert!(
        matches!(
            &single("MATCH (a)<-[:R]->(b) RETURN b")[0],
            Clause::Match { pattern, .. }
                if pattern.paths[0].hops[0].0.dir == RelDir::Undirected
        ),
        "double-headed arrow is undirected"
    );
    assert!(
        parse_statement("OPTIONAL RETURN 1").is_err(),
        "OPTIONAL without MATCH"
    );
    assert!(
        parse_statement("MERGE (n) ON DELETE SET n.x = 1").is_err(),
        "ON needs CREATE/MATCH"
    );
    assert!(
        parse_statement("MATCH (n) RETURN n extra").is_err(),
        "trailing tokens refuse"
    );
    assert!(parse_statement("").is_err(), "an empty statement refuses");
    assert!(
        parse_statement("FOREACH (x IN [] | )").is_err(),
        "FOREACH needs updates"
    );
    assert!(
        parse_statement("RETURN [ (n) | n.x ] AS xs").is_err(),
        "a relationship-free pattern comprehension is refused, not guessed at"
    );
}

#[test]
fn a_trailing_semicolon_is_accepted() {
    let clauses = single("MATCH (n) RETURN n;");
    assert_eq!(clauses.len(), 2);
}

#[test]
fn multiple_paths_and_anonymous_nodes() {
    let clauses = single("CREATE (a:X)-[:R]->(), (b:Y {k: 1})");
    let Clause::Create { pattern } = &clauses[0] else {
        panic!()
    };
    assert_eq!(pattern.paths.len(), 2);
    assert!(
        pattern.paths[0].hops[0].1.var.is_none(),
        "the anonymous node"
    );
    assert_eq!(pattern.paths[1].start.labels, vec!["Y"]);
}

#[test]
fn graph_dependent_expressions_refuse_in_scalar_eval_by_name() {
    use engram_cypher::{EvalError, Scope, eval, parse_expression};
    let e = parse_expression("EXISTS { (a)-->(b) }").expect("parses as an expression");
    assert!(matches!(
        eval(&e, &Scope::default()),
        Err(EvalError::GraphDependent("EXISTS {}"))
    ));
}

// Neo4j (5.26) accepts WITH's ORDER BY / SKIP / LIMIT AFTER its WHERE, and the two
// orders mean different things: the canonical form limits then filters, this form
// filters then orders/limits, and its ORDER BY sees only the projected names. It
// parses as the filtering WITH plus a `WITH *` carrying the tail -- the shape the
// platform's story-tracker query has, the first read the shadow instrument refused.
#[test]
fn with_where_before_order_by_desugars_to_a_star_with_carrying_the_tail() {
    let clauses = single(
        "MATCH (a)-->(e)<--(x)-->(s) WITH s, count(DISTINCT e) AS shared WHERE shared >= 1 \
         ORDER BY shared DESC SKIP 1 LIMIT 5 RETURN s.id AS id",
    );
    assert_eq!(clauses.len(), 4, "MATCH, WITH, WITH *, RETURN");
    let Clause::With { proj, where_ } = &clauses[1] else {
        panic!("{:?}", clauses[1])
    };
    assert_eq!(proj.items.len(), 2);
    assert!(where_.is_some(), "the filter stays on the projecting WITH");
    assert!(proj.order.is_empty() && proj.skip.is_none() && proj.limit.is_none());
    let Clause::With { proj: tail, where_: None } = &clauses[2] else {
        panic!("{:?}", clauses[2])
    };
    assert!(tail.star && tail.items.is_empty() && !tail.distinct);
    assert_eq!(tail.order.len(), 1);
    assert!(tail.order[0].desc);
    assert!(tail.skip.is_some() && tail.limit.is_some());
    assert!(matches!(&clauses[3], Clause::Return { .. }));
}

#[test]
fn with_where_before_order_by_keeps_distinct_and_works_with_limit_alone() {
    let clauses = single("UNWIND [3, 1, 2, 3] AS v WITH DISTINCT v WHERE v > 1 LIMIT 5 RETURN v");
    assert_eq!(clauses.len(), 4);
    let Clause::With { proj, where_: Some(_) } = &clauses[1] else {
        panic!()
    };
    assert!(proj.distinct);
    let Clause::With { proj: tail, .. } = &clauses[2] else {
        panic!()
    };
    assert!(tail.star && tail.order.is_empty() && tail.limit.is_some());
}

#[test]
fn the_canonical_with_order_is_untouched_and_a_second_where_is_refused() {
    // ORDER BY before WHERE: one clause, exactly as before.
    let clauses = single("UNWIND [1, 2, 3] AS v WITH v ORDER BY v DESC LIMIT 2 WHERE v < 4 RETURN v");
    assert_eq!(clauses.len(), 3);
    let Clause::With { proj, where_: Some(_) } = &clauses[1] else {
        panic!()
    };
    assert_eq!(proj.order.len(), 1);
    assert!(proj.limit.is_some());
    // WHERE … ORDER BY … WHERE: Neo4j refuses the second WHERE; so does this parser.
    assert!(parse_statement("UNWIND [1, 2, 3] AS v WITH v WHERE v > 1 ORDER BY v WHERE v < 3 RETURN v").is_err());
}
