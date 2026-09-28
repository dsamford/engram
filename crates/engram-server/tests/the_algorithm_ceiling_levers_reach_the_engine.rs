//! The five `ENGRAM_ALGO_*` ceilings must reach the graph that reads them.
//!
//! Every algorithm refusal names one of these — "raise
//! `ENGRAM_ALGO_NODE_CEILING`" — and for a release **nothing read them**. An
//! operator following the message exactly would set the variable, see no
//! change, and have no way to tell the advice was fiction.
//!
//! The wiring is a function taking a LOOKUP rather than reading the process
//! environment directly, precisely so this file can exist: reading the real
//! environment in a test mutates global state every other test in the binary
//! shares, so the only way to catch a future deletion of the block would be to
//! notice it. Against a fake environment, "the variable reaches the ceiling"
//! is a property rather than a hope.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_server::apply_algo_ceilings;
use engram_store::Store;

fn graph_with_a_triangle() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n: Vec<u64> = (0..3)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("name".to_string(), Value::Str(format!("n{i}")));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for i in 0..3 {
        g.create_rel(n[i], "R", n[(i + 1) % 3], &BTreeMap::new())
            .expect("rel");
    }
    g
}

fn run(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e:?}"))
}

const PAGERANK: &str = "CALL engram.algo.pagerank.stream({nodeLabels:['N'], \
                        relationshipTypes:['R']}) YIELD score RETURN count(score)";

#[test]
fn each_ceiling_variable_reaches_the_refusal_that_names_it() {
    for var in [
        "ENGRAM_ALGO_NODE_CEILING",
        "ENGRAM_ALGO_EDGE_CEILING",
        "ENGRAM_ALGO_BYTE_CEILING",
    ] {
        let g = graph_with_a_triangle();
        // A fake environment holding exactly one variable, set to a value the
        // fixture cannot satisfy.
        apply_algo_ceilings(&g, |k| (k == var).then(|| "0".to_string()));
        let e = run(&g, PAGERANK).expect_err("the ceiling must refuse");
        assert!(
            e.contains(var),
            "setting {var} must produce a refusal naming it, got {e}",
        );
    }
}

#[test]
fn the_all_pairs_ceiling_variable_reaches_betweenness() {
    let g = graph_with_a_triangle();
    apply_algo_ceilings(&g, |k| {
        (k == "ENGRAM_ALGO_WORK_CEILING").then(|| "1".to_string())
    });
    let e = run(
        &g,
        "CALL engram.algo.betweenness.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
         YIELD score RETURN count(score)",
    )
    .expect_err("the all-pairs ceiling must refuse");
    assert!(e.contains("ENGRAM_ALGO_WORK_CEILING"), "got {e}");
}

#[test]
fn an_empty_environment_leaves_every_default_in_place() {
    // The negative: the wiring must not itself change anything when nothing is
    // set. A version that applied a parsed-as-zero default would refuse every
    // query on a server with no configuration at all.
    let g = graph_with_a_triangle();
    apply_algo_ceilings(&g, |_| None);
    assert_eq!(
        run(&g, PAGERANK).expect("must still answer")[0][0],
        Value::Int(3),
        "an unset environment must leave the defaults alone",
    );
}

#[test]
fn a_value_that_does_not_parse_is_ignored_rather_than_fatal() {
    // These RAISE a safety ceiling, so a typo that leaves the default in place
    // is the conservative failure. A server refusing to start over a malformed
    // tuning variable is worse than one running at its defaults.
    let g = graph_with_a_triangle();
    apply_algo_ceilings(&g, |k| {
        (k == "ENGRAM_ALGO_NODE_CEILING").then(|| "not-a-number".to_string())
    });
    assert_eq!(
        run(&g, PAGERANK).expect("must still answer")[0][0],
        Value::Int(3),
        "an unparseable value must be ignored, not treated as zero",
    );
}

#[test]
fn a_raised_ceiling_admits_what_the_default_would_have_refused() {
    // The other direction, and the one that proves these RAISE rather than
    // merely lower: set the ceiling below the fixture, confirm the refusal,
    // then set it above and confirm the answer comes back.
    let g = graph_with_a_triangle();
    apply_algo_ceilings(&g, |k| {
        (k == "ENGRAM_ALGO_NODE_CEILING").then(|| "1".to_string())
    });
    assert!(run(&g, PAGERANK).is_err(), "a ceiling of 1 must refuse 3 nodes");
    apply_algo_ceilings(&g, |k| {
        (k == "ENGRAM_ALGO_NODE_CEILING").then(|| "1000".to_string())
    });
    assert_eq!(
        run(&g, PAGERANK).expect("a raised ceiling must admit it")[0][0],
        Value::Int(3),
    );
}
