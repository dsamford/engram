//! The `engram.algo.*` procedure surface, through Cypher.
//!
//! Four modes over one computation: `stream`, `stats`, `mutate`, `write`. The
//! rule they exist to make safe is **the procedure never writes; the statement
//! does** — compute happens against a read snapshot, and `write` is an
//! ordinary bulk property set in a separate short transaction afterwards.
//!
//! The tests that matter most here are the refusals: an algorithm has no
//! slower path to fall back to, so a projection it cannot afford must produce
//! a typed error naming the numbers rather than an out-of-memory.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn node(g: &Graph, name: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str(name.into()));
    g.create_node(&["N".into()], &m).expect("node")
}

fn edge(g: &Graph, a: u64, b: u64) {
    g.create_rel(a, "R", b, &BTreeMap::new()).expect("rel");
}

fn run(g: &Graph, src: &str) -> engram_graph::interp::QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn err(g: &Graph, src: &str) -> String {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    match run_query(g, &q, BTreeMap::new()) {
        Ok(r) => panic!(
            "`{src}` was expected to be refused, answered {:?}",
            r.columns
        ),
        Err(e) => e.to_string(),
    }
}

/// Two triangles joined by one edge.
fn corpus(g: &Graph) -> Vec<u64> {
    let n: Vec<u64> = (0..6).map(|i| node(g, &format!("n{i}"))).collect();
    for (a, b) in [(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3), (2, 3)] {
        edge(g, n[a], n[b]);
    }
    n
}

// ─── The four modes ────────────────────────────────────────────────────────

#[test]
fn an_algorithm_procedure_reports_the_snapshot_it_computed_at() {
    let g = g();
    corpus(&g);
    let r = run(
        &g,
        "CALL engram.algo.pageRank.stream({nodeLabels: ['N']}) \
         YIELD nodeId, score, asOf RETURN nodeId, score, asOf",
    );
    assert_eq!(r.rows.len(), 6);
    for row in &r.rows {
        assert!(
            matches!(row[2], Value::Int(t) if t > 0),
            "every row carries the snapshot it describes: {row:?}",
        );
        assert!(matches!(row[1], Value::Float(_)));
    }
}

#[test]
fn a_stream_is_ordered_by_node_id_rather_than_by_score() {
    // Sorting by score would need a tie-break the algorithm cannot supply, and
    // `ORDER BY score DESC` composes on top for free.
    let g = g();
    let n = corpus(&g);
    let r = run(
        &g,
        "CALL engram.algo.pageRank.stream({nodeLabels: ['N']}) \
         YIELD nodeId RETURN nodeId",
    );
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|x| match x[0] {
            Value::Int(i) => i,
            _ => panic!("expected an id"),
        })
        .collect();
    let mut want: Vec<i64> = n.iter().map(|x| *x as i64).collect();
    want.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn stats_summarises_without_a_row_per_node() {
    let g = g();
    corpus(&g);
    let r = run(
        &g,
        "CALL engram.algo.wcc.stats({nodeLabels: ['N']}) \
         YIELD nodeCount, converged, asOf, distribution \
         RETURN nodeCount, converged, asOf, distribution",
    );
    assert_eq!(r.rows.len(), 1, "stats is one row");
    assert_eq!(r.rows[0][0], Value::Int(6));
    assert!(matches!(r.rows[0][3], Value::Map(_)));
}

#[test]
fn mutate_publishes_a_result_that_result_stream_can_read_back() {
    // `mutate` is the composition mechanism — without a way to read it back it
    // would be a no-op with a receipt.
    let g = g();
    corpus(&g);
    let r = run(
        &g,
        "CALL engram.algo.pageRank.mutate({nodeLabels: ['N'], mutateKey: 'pr1'}) \
         YIELD mutateKey, nodeCount RETURN mutateKey, nodeCount",
    );
    assert_eq!(r.rows[0][0], Value::Str("pr1".into()));
    assert_eq!(r.rows[0][1], Value::Int(6));

    let s = run(
        &g,
        "CALL engram.algo.result.stream({mutateKey: 'pr1'}) \
         YIELD nodeId, value RETURN nodeId, value",
    );
    assert_eq!(s.rows.len(), 6);

    let l = run(
        &g,
        "CALL engram.algo.result.list() YIELD mutateKey, nodeCount, stale \
         RETURN mutateKey, nodeCount, stale",
    );
    assert_eq!(l.rows[0][0], Value::Str("pr1".into()));

    let d = run(
        &g,
        "CALL engram.algo.result.drop({mutateKey: 'pr1'}) YIELD dropped RETURN dropped",
    );
    assert_eq!(d.rows[0][0], Value::Bool(true));
}

#[test]
fn a_cached_result_survives_a_write_and_says_it_is_stale() {
    // **NEVER AUTO-INVALIDATED.** A cached result is a measurement of a past
    // graph; deleting it because the graph moved would destroy evidence rather
    // than maintain a cache — and the only available "refresh" would be a
    // silent multi-minute recompute inside what the user wrote as a read.
    let g = g();
    corpus(&g);
    run(
        &g,
        "CALL engram.algo.wcc.mutate({nodeLabels: ['N'], mutateKey: 'c'}) \
         YIELD mutateKey RETURN mutateKey",
    );
    node(&g, "newcomer");

    let s = run(
        &g,
        "CALL engram.algo.result.stream({mutateKey: 'c'}) YIELD nodeId RETURN nodeId",
    );
    assert_eq!(
        s.rows.len(),
        6,
        "the cached result still describes the graph it was computed on",
    );
    let l = run(
        &g,
        "CALL engram.algo.result.list() YIELD stale RETURN stale",
    );
    assert_eq!(
        l.rows[0][0],
        Value::Bool(true),
        "and it says so rather than pretending to be current",
    );
}

#[test]
fn a_missing_cached_result_is_an_error_not_an_empty_answer() {
    // "The algorithm found nothing" and "that result is gone" are different
    // facts, and only one of them is about the graph.
    let g = g();
    corpus(&g);
    let e = err(
        &g,
        "CALL engram.algo.result.stream({mutateKey: 'never'}) YIELD nodeId RETURN nodeId",
    );
    assert!(e.contains("no cached result"), "{e}");
    assert!(
        e.contains("ENGRAM_ALGO_CACHE_BYTES"),
        "must name the lever: {e}"
    );
}

#[test]
fn write_persists_the_scores_and_reports_both_stamps() {
    let g = g();
    let n = corpus(&g);
    let r = run(
        &g,
        "CALL engram.algo.degree.write({nodeLabels: ['N'], writeProperty: 'deg'}) \
         YIELD nodesWritten, asOf, committedAt RETURN nodesWritten, asOf, committedAt",
    );
    assert_eq!(r.rows[0][0], Value::Int(6));
    let (Value::Int(as_of), Value::Int(committed)) = (&r.rows[0][1], &r.rows[0][2]) else {
        panic!("both stamps must be integers: {:?}", r.rows[0]);
    };
    assert!(
        committed >= as_of,
        "the values describe snapshot {as_of} and landed at {committed}",
    );

    // And the property is an ordinary property afterwards.
    let read = run(&g, "MATCH (x:N) WHERE x.deg > 0 RETURN count(x)");
    assert_eq!(read.rows[0][0], Value::Int(6));
    let _ = n;
}

// ─── The refusals ──────────────────────────────────────────────────────────

#[test]
fn an_unknown_config_key_is_refused_with_a_suggestion() {
    // A misspelled `tolerance` that is silently ignored is a wrong answer that
    // looks right: the algorithm converges somewhere else and says nothing.
    let g = g();
    corpus(&g);
    let e = err(
        &g,
        "CALL engram.algo.pageRank.stream({tolerence: 0.1}) YIELD nodeId RETURN nodeId",
    );
    assert!(e.contains("unknown config key"), "{e}");
    assert!(e.contains("tolerance"), "must suggest the real key: {e}");
}

#[test]
fn a_config_value_of_the_wrong_type_is_refused_by_name() {
    let g = g();
    corpus(&g);
    for (cfg, needle) in [
        ("{maxIterations: 'many'}", "maxIterations"),
        ("{nodeLabels: 3}", "nodeLabels"),
        ("{orientation: 'SIDEWAYS'}", "orientation"),
        ("{dampingFactor: 2.0}", "dampingFactor"),
    ] {
        let e = err(
            &g,
            &format!("CALL engram.algo.pageRank.stream({cfg}) YIELD nodeId RETURN nodeId"),
        );
        assert!(e.contains(needle), "`{cfg}` must name the key: {e}");
    }
}

#[test]
fn mutate_without_a_key_and_write_without_a_property_are_refused() {
    let g = g();
    corpus(&g);
    let e = err(
        &g,
        "CALL engram.algo.wcc.mutate({nodeLabels: ['N']}) YIELD mutateKey RETURN mutateKey",
    );
    assert!(e.contains("mutateKey"), "{e}");
    let e = err(
        &g,
        "CALL engram.algo.wcc.write({nodeLabels: ['N']}) YIELD writeProperty RETURN writeProperty",
    );
    assert!(e.contains("writeProperty"), "{e}");
}

#[test]
fn a_traversal_without_a_source_node_is_refused() {
    let g = g();
    corpus(&g);
    let e = err(
        &g,
        "CALL engram.algo.bfs.stream({nodeLabels: ['N']}) YIELD nodeId RETURN nodeId",
    );
    assert!(e.contains("sourceNode"), "{e}");
}

#[test]
fn a_source_outside_the_projection_is_refused_rather_than_answering_nothing() {
    let g = g();
    corpus(&g);
    let e = err(
        &g,
        "CALL engram.algo.bfs.stream({nodeLabels: ['N'], sourceNode: 99999}) \
         YIELD nodeId RETURN nodeId",
    );
    assert!(e.contains("not in this projection"), "{e}");
}

// ─── Classification ────────────────────────────────────────────────────────

#[test]
fn stream_and_stats_are_read_only_while_mutate_and_write_are_not() {
    // The mode-sensitive classification the procedure catalogue makes possible:
    // no part of the engine parses a procedure NAME to decide what it may do.
    for name in [
        "engram.algo.pagerank.stream",
        "engram.algo.pagerank.stats",
        "engram.algo.result.list",
        "engram.algo.result.stream",
    ] {
        assert!(engram_proc::is_read_only(name), "{name} must be read-only");
    }
    for name in [
        "engram.algo.pagerank.mutate",
        "engram.algo.pagerank.write",
        "engram.algo.result.drop",
    ] {
        assert!(!engram_proc::is_read_only(name), "{name} must not be");
    }
}

#[test]
fn every_algorithm_is_reachable_in_every_mode() {
    // THE NAME WAS A CLAIM THIS TEST DID NOT MAKE.
    //
    // It iterated seven of the eleven algorithms and called `.stream` on each,
    // under a name that promised every algorithm in every mode. Around thirty
    // algorithm/mode pairs looked covered by it and were not — including every
    // `.write`, which is the only mode that touches the keyspace, and every
    // mode of `sssp`, which no test reached at all.
    //
    // It is now driven from the CATALOGUE rather than a hand-written list, so
    // an algorithm added later is covered the day it is added rather than the
    // day someone remembers this file. That is the only version of "every"
    // that stays true.
    let g = g();
    corpus(&g);
    let src_id = first_node_id(&g);

    let mut seen = 0usize;
    for name in engram_proc::names() {
        let Some(rest) = name.strip_prefix("engram.algo.") else {
            continue;
        };
        // The path and cache procedures are not algorithm/mode pairs: they
        // have their own files, and `result.*` has no projection at all.
        if rest.starts_with("result.") || rest.starts_with("kshortestpaths") {
            continue;
        }
        let Some((alg, mode)) = rest.rsplit_once('.') else {
            continue;
        };
        seen += 1;

        // Each mode needs the config that mode requires — a missing
        // `writeProperty` is a refusal, and a test that took the refusal as
        // "reachable" would be the same defect one layer along.
        let extra = match mode {
            "mutate" => format!(", mutateKey: 'k_{alg}'"),
            "write" => format!(", writeProperty: 'p_{alg}'"),
            _ => String::new(),
        };
        // BFS and SSSP are the two that traverse FROM somewhere.
        let source = if alg == "bfs" || alg == "sssp" {
            format!(", sourceNode: {src_id}")
        } else {
            String::new()
        };
        let stmt = format!(
            "CALL engram.algo.{alg}.{mode}({{nodeLabels: ['N'], relationshipTypes: ['R']             {source}{extra}}})"
        );
        let r = engram_graph::run_query(
            &g,
            &engram_cypher::parse_statement(&stmt).expect("parses"),
            BTreeMap::new(),
        )
        .unwrap_or_else(|e| panic!("`{name}` is catalogued but not callable: {e:?}"));
        assert!(
            !r.rows.is_empty(),
            "`{name}` answered no rows — a catalogued procedure that returns nothing is \
             indistinguishable from one with no body",
        );
    }
    // THE VACUITY GUARD. Driving from the catalogue means a filter that
    // matched nothing would pass silently, which is exactly the shape this
    // test was already failing in.
    assert!(
        seen >= 40,
        "only {seen} algorithm/mode pairs were exercised; the catalogue holds eleven \
         algorithms in four modes, so a count this low means the filter above stopped \
         matching",
    );
}

/// The id of some node in the corpus, for the traversal algorithms.
fn first_node_id(g: &engram_graph::Graph) -> i64 {
    let r = engram_graph::run_query(
        g,
        &engram_cypher::parse_statement("MATCH (n:N) RETURN id(n) ORDER BY id(n) LIMIT 1")
            .expect("parses"),
        BTreeMap::new(),
    )
    .expect("runs");
    match r.rows[0][0] {
        Value::Int(i) => i,
        ref other => panic!("expected an id, got {other:?}"),
    }
}

#[test]
fn a_standalone_algorithm_call_returns_its_declared_columns() {
    // The procedure-catalogue work and the algorithm work meeting: a `CALL`
    // that ends a query needs no YIELD.
    let g = g();
    corpus(&g);
    let r = run(&g, "CALL engram.algo.wcc.stats({nodeLabels: ['N']})");
    assert_eq!(
        r.columns,
        vec![
            "nodeCount".to_string(),
            "relationshipCount".into(),
            "iterations".into(),
            "converged".into(),
            "asOf".into(),
            "distribution".into(),
            "outsideProjection".into(),
        ],
    );
}
