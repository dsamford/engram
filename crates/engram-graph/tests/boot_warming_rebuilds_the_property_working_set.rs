//! Boot warming rebuilds the PROPERTY columns the workload proved it needs.
//!
//! `Graph::warm` builds derived topology — memberships, adjacency, declared
//! indexes — and no property data at all. Queries filter on properties, so the
//! first one to touch a `(label, prop)` pair built its column on the querying
//! client's thread: the same latency cliff warming exists to remove, moved
//! from topology to predicates. Measured on SNB BI at SF3, a first query took
//! 108 s where the next one — doing strictly MORE work — took 40 s.
//!
//! This is the THIRD instance of one mistake, and `warm`'s own comments record
//! the other two: untyped adjacency warmed while the typed tables every
//! traversal uses were not, and the partition-wide membership warmed while the
//! per-label views were not.
//!
//! The working set is not guessed. It is read back out of the property-column
//! cache, whose entries are the survivors of a budgeted LRU against everything
//! the workload has read — so the cache IS the workload's statement about
//! itself.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, counters, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn seed(g: &Graph, n: usize) {
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("p{i}")));
        m.insert("age".to_string(), Value::Int(i as i64));
        g.create_node(&["Person".into()], &m).expect("node");
    }
}

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

#[test]
fn the_cache_reports_its_working_set_by_name() {
    let g = g();
    seed(&g, 64);
    // a query that reads a property over a whole label is what fills the
    // column cache; nothing is warmed speculatively
    let _ = run(&g, "MATCH (p:Person) WHERE p.age >= 0 RETURN count(p) AS n");

    let set = g.cached_prop_columns();
    assert!(
        set.iter().any(|(l, p, _)| l == "Person" && p == "age"),
        "the column the query read should be in the working set: {set:?}"
    );
    // NAMES, not tokens: a token only means something against the dictionary
    // that minted it, so a persisted set keyed on tokens could warm the wrong
    // column after a rebuild.
    for (label, prop, _) in &set {
        assert!(!label.is_empty() && !prop.is_empty(), "{set:?}");
    }
}

#[test]
fn warming_rebuilds_a_named_column_and_counts_it() {
    let g = g();
    seed(&g, 64);
    let before = counters::WARM_PROP_COLUMNS.load(Relaxed);
    let kept = g.warm_prop_columns(&[("Person".to_string(), "age".to_string(), false)]);
    assert_eq!(kept, 1, "the column was rebuilt");
    assert!(
        counters::WARM_PROP_COLUMNS.load(Relaxed) > before,
        "and counted"
    );
    assert!(
        g.cached_prop_columns()
            .iter()
            .any(|(l, p, _)| l == "Person" && p == "age"),
        "and is resident afterwards"
    );
}

#[test]
fn a_warmed_column_answers_the_same_as_a_lazily_built_one() {
    // THE POINT OF WARMING THROUGH THE GATHER PATH: a warmed column must be
    // the object a lazily built one would be. If warming had its own builder,
    // this is where the two would diverge — and a wrong answer served fast is
    // worse than a right one served slow.
    let cold = g();
    seed(&cold, 64);
    let want = run(
        &cold,
        "MATCH (p:Person) WHERE p.age >= 32 RETURN count(p) AS n",
    );

    let warm = g();
    seed(&warm, 64);
    warm.warm_prop_columns(&[("Person".to_string(), "age".to_string(), false)]);
    let got = run(
        &warm,
        "MATCH (p:Person) WHERE p.age >= 32 RETURN count(p) AS n",
    );
    assert_eq!(got, want, "warmed and unwarmed must agree");
    assert_eq!(got[0][0], Value::Int(32), "and be right: {got:?}");
}

#[test]
fn a_column_that_no_longer_exists_is_skipped_not_fatal() {
    // A persisted set outlives the data it describes: labels get dropped,
    // properties stop being written. A boot that cannot warm must still serve.
    let g = g();
    seed(&g, 8);
    let kept = g.warm_prop_columns(&[
        ("Ghost".to_string(), "age".to_string(), false),
        ("Person".to_string(), "nosuchprop".to_string(), false),
        ("Person".to_string(), "age".to_string(), false),
    ]);
    assert_eq!(kept, 1, "only the real one is kept, and nothing panicked");
}

#[test]
fn an_empty_label_warms_nothing_rather_than_caching_emptiness() {
    let g = g();
    seed(&g, 4);
    let _ = run(&g, "MATCH (p:Person) RETURN count(p) AS n");
    let kept = g.warm_prop_columns(&[("Person".to_string(), "age".to_string(), false)]);
    assert_eq!(kept, 1);
    // a label with no members has no column worth holding
    let empty = Graph::new(Store::new(), Realm(1), Namespace(1));
    assert_eq!(
        empty.warm_prop_columns(&[("Person".to_string(), "age".to_string(), false)]),
        0
    );
}

#[test]
fn a_write_after_warming_retires_the_warmed_column() {
    // Warming must not make a column look current through a commit. The stamp
    // is read BEFORE the gather for exactly this reason.
    let g = g();
    seed(&g, 16);
    g.warm_prop_columns(&[("Person".to_string(), "age".to_string(), false)]);

    let mut m = BTreeMap::new();
    m.insert("name".to_string(), Value::Str("late".into()));
    m.insert("age".to_string(), Value::Int(999));
    g.create_node(&["Person".into()], &m).expect("node");

    let r = run(
        &g,
        "MATCH (p:Person) WHERE p.age > 900 RETURN count(p) AS n",
    );
    assert_eq!(
        r[0][0],
        Value::Int(1),
        "the node written after warming must still be found: {r:?}"
    );
}

#[test]
fn a_presence_column_warms_as_presence() {
    let g = g();
    seed(&g, 8);
    let kept = g.warm_prop_columns(&[("Person".to_string(), "age".to_string(), true)]);
    assert_eq!(kept, 1);
    assert!(
        g.cached_prop_columns()
            .iter()
            .any(|(l, p, pres)| l == "Person" && p == "age" && *pres),
        "presence is its own cache entry, not a values column"
    );
}
