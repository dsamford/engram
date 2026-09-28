//! A property's change log has more than one consumer, and its pruner must
//! see all of them.
//!
//! `prop_log` is keyed by PROPERTY token and shared by every structure derived
//! from that property. Pruning below a consumer's snapshot strands it: its next
//! probe finds `covers` false and rebuilds from every record of its label —
//! O(label) store gets, on a READER's thread, because a catch-up runs wherever
//! the reader that needed it happened to be.
//!
//! `prune_prop_log` guarded against that for range indexes from the beginning.
//! The trigram cache was added as a SECOND consumer of the same log and was
//! invisible to the guard, so a range index over the same property could prune
//! the trigram index's window away simply by being probed more often — and
//! nothing failed, because a stranded index rebuilds and returns the right
//! answer. Slowly, and only under a write stream, which is where nobody looks.
//!
//! This is `derived.rs`'s fourth defect from the other side. There a
//! structure's currency test failed to read all of its SOURCES; here a
//! source's pruner failed to see all of its CONSUMERS. Both are a relationship
//! recorded in one direction only, and adding a consumer is precisely when
//! nobody thinks to look for the other end.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("ddl parses"), BTreeMap::new()).expect("ddl");
}

fn q(g: &Graph, src: &str) {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new()).expect("query");
}

/// A corpus large enough that the planner will actually seek rather than
/// decline, since a declined probe never loads the index and the test would
/// pass without exercising anything.
fn corpus(g: &Graph, rows: usize) {
    for i in 0..rows {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i as i64));
        m.insert(
            "path".to_string(),
            Value::Str(if i % 50 == 0 {
                format!("src/zqx{i}.rs")
            } else {
                format!("src/ordinary{i}.rs")
            }),
        );
        g.create_node(&["File".into()], &m).expect("node");
    }
}

#[test]
fn a_range_probe_does_not_strand_the_trigram_index_on_the_same_property() {
    let g = g();
    corpus(&g, 600);
    // BOTH indexes over the SAME property, which is the whole point: they
    // share one log, and only then can one prune the other's window.
    ddl(
        &g,
        "CREATE TRIGRAM INDEX file_path_t FOR (f:File) ON (f.path)",
    );
    ddl(&g, "CREATE INDEX file_path_r FOR (f:File) ON (f.path)");

    // Load both, so each holds a snapshot the log must be kept for.
    q(
        &g,
        "MATCH (f:File) WHERE f.path CONTAINS 'zqx' RETURN count(f)",
    );
    q(
        &g,
        "MATCH (f:File) WHERE f.path STARTS WITH 'src/o' RETURN count(f)",
    );

    // Now write, and probe ONLY the range index — so it publishes at a newer
    // stamp and prunes, while the trigram index's snapshot stays behind.
    for i in 0..40 {
        q(
            &g,
            &format!("MATCH (f:File {{id: {i}}}) SET f.path = 'src/rewritten{i}.rs'"),
        );
        q(
            &g,
            "MATCH (f:File) WHERE f.path STARTS WITH 'src/r' RETURN count(f)",
        );
    }

    // The trigram index must now CATCH UP, not rebuild. A rebuild is the
    // symptom of having been stranded, and it is what this asserts against.
    let (_, trace) = engram_observe::with_trace(|| {
        q(
            &g,
            "MATCH (f:File) WHERE f.path CONTAINS 'zqx' RETURN count(f)",
        );
    });
    let rebuilt = trace
        .counters()
        .get("graph.trigram index built")
        .copied()
        .unwrap_or(0);
    assert_eq!(
        rebuilt, 0,
        "the trigram index rebuilt after a range index over the SAME property was probed and \
         pruned the shared log — a rebuild is O(label) store gets on a reader's thread, and it \
         happens every time the other consumer runs ahead",
    );
}

#[test]
fn the_prune_floor_is_held_back_for_a_lagging_trigram_index() {
    // The positive form of the same property, on the pruner's own counter:
    // the floor must actually be held back, rather than the test above
    // passing because nothing pruned at all.
    let g = g();
    corpus(&g, 600);
    ddl(
        &g,
        "CREATE TRIGRAM INDEX file_path_t2 FOR (f:File) ON (f.path)",
    );
    ddl(&g, "CREATE INDEX file_path_r2 FOR (f:File) ON (f.path)");
    q(
        &g,
        "MATCH (f:File) WHERE f.path CONTAINS 'zqx' RETURN count(f)",
    );
    q(
        &g,
        "MATCH (f:File) WHERE f.path STARTS WITH 'src/o' RETURN count(f)",
    );

    let (_, trace) = engram_observe::with_trace(|| {
        for i in 0..20 {
            q(
                &g,
                &format!("MATCH (f:File {{id: {i}}}) SET f.path = 'src/again{i}.rs'"),
            );
            q(
                &g,
                "MATCH (f:File) WHERE f.path STARTS WITH 'src/a' RETURN count(f)",
            );
        }
    });
    assert!(
        trace
            .counters()
            .get("graph.property log kept for an older sibling index")
            .copied()
            .unwrap_or(0)
            > 0,
        "the prune floor was never held back, so the test above proves nothing: either no \
         prune ran, or the trigram index's snapshot was not considered when the floor was \
         computed — which is the defect",
    );
}

#[test]
fn a_trigram_index_earns_a_log_wide_enough_to_catch_up_after_an_idle_stretch() {
    // The other half of sharing a log, and the half a trigram index needs
    // most: the DEFAULT cap is a constant, so an index over a large label
    // drops below the floor after an idle stretch of writes and its next
    // probe rebuilds — O(label) store gets on a reader's thread, behind its
    // own build guard, so every concurrent reader of that index waits too.
    //
    // A range index earns its width at build time. A trigram index over a
    // property with NO range index would sit at the default for ever, which
    // is exactly the shape this index exists for: a text column nothing else
    // indexes. So it must earn its own.
    // BIG ENOUGH THAT THE WIDENING IS DISTINGUISHABLE, which is why this
    // corpus is not the 600 rows the tests above use. The earned cap is
    // `(rows / 8).clamp(16_384, 524_288)`, so anything under 131,072 rows
    // earns exactly the default and the assertion below would pass whether
    // the build widened or not — the vacuous differential from one file over,
    // arriving as a constant that swallows the signal.
    let g = g();
    corpus(&g, 140_000);
    ddl(
        &g,
        "CREATE TRIGRAM INDEX file_path_wide FOR (f:File) ON (f.path)",
    );
    // Build it, which is where the size is known for free.
    q(
        &g,
        "MATCH (f:File) WHERE f.path CONTAINS 'zqx' RETURN count(f)",
    );

    let cap = g
        .prop_log_cap_for_test("path")
        .expect("the trigram build must have created the property's log");
    assert!(
        cap > 16_384,
        "the log is still at the default cap ({cap}) after a 140,000-row trigram index was \
         built over it — the index earned no margin, so an idle stretch of writes strands \
         it and its next probe pays a full O(label) rebuild on a reader's thread",
    );

    // An idle stretch: writes to the indexed property with NOBODY probing the
    // trigram index, then one probe. It must catch up, not rebuild.
    for i in 0..500 {
        q(
            &g,
            &format!("MATCH (f:File {{id: {i}}}) SET f.path = 'src/idle{i}.rs'"),
        );
    }
    let (_, trace) = engram_observe::with_trace(|| {
        q(
            &g,
            "MATCH (f:File) WHERE f.path CONTAINS 'zqx' RETURN count(f)",
        );
    });
    assert_eq!(
        trace
            .counters()
            .get("graph.trigram index built")
            .copied()
            .unwrap_or(0),
        0,
        "the trigram index rebuilt after an idle stretch its log should have covered",
    );
}
