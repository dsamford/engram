#![allow(non_snake_case)]
//! Boot warm-up must build what an operator declared.
//!
//! `Graph::warm` covered memberships and adjacency and stopped there, so the
//! FIRST seek against a declared index built it on the querying client's
//! thread — 249.7 s for `Person.id` and 70.6 s for `Message.id` at SF10, paid
//! again on every restart. Declaring an index is a statement of intent to use
//! it; building it at boot is what that means.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, counters, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;
use std::sync::atomic::Ordering::Relaxed;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}
fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}
fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

/// EVERY TEST HERE TAKES THIS LOCK.
///
/// `WARM_INDEXES_BUILT` is a PROCESS-GLOBAL counter, and these tests measure a
/// before/after delta across their own `warm()`. Cargo runs the file's tests
/// in parallel threads, so a sibling warming at the same moment lands inside
/// another's delta: `warming_with_no_declared_indexes_is_harmless` declared
/// nothing, warmed, and read 2 builds that belonged to other tests. It passed
/// serially and alone, which is what a race looks like from the outside.
///
/// The delta is not enough on its own — it needs the window to be quiet — so
/// the lock is what makes the measurement mean anything. Poisoning is ignored:
/// a panicking sibling has already failed its own test, and blocking the rest
/// of the file behind it would turn one failure into eight.
static WARM: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn warm_guard() -> std::sync::MutexGuard<'static, ()> {
    WARM.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn warming_builds_every_declared_index() {
    let _warm = warm_guard();
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    ddl(&g, "CREATE INDEX m_id FOR (n:Message) ON (n.id)");
    run(&g, "CREATE (:Person {id: 1})");
    run(&g, "CREATE (:Message {id: 2})");

    let before = counters::WARM_INDEXES_BUILT.load(Relaxed);
    let _ = g.warm();
    let built = counters::WARM_INDEXES_BUILT.load(Relaxed) - before;

    assert!(
        built >= 2,
        "warming built {built} declared index(es); two were declared. A 0 here \
         means boot warming still leaves the first seek to build them on a \
         client's thread."
    );
}

#[test]
fn a_seek_after_warming_does_not_build() {
    let _warm = warm_guard();
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    for i in 0..64 {
        run(&g, &format!("CREATE (:Person {{id: {i}}})"));
    }
    let _ = g.warm();

    // The point of warming: the first CLIENT seek finds the index already there.
    let (_, t) = engram_observe::with_trace(|| {
        let r = run(&g, "MATCH (p:Person {id: 7}) RETURN p.id");
        assert_eq!(r.rows.len(), 1, "the seek must still answer");
    });
    let builds = t
        .counters()
        .get("graph.range index builds")
        .copied()
        .unwrap_or(0);
    assert_eq!(
        builds,
        0,
        "the first seek after warming built the index on the query thread: {:?}",
        t.counters()
    );
}

#[test]
fn warming_builds_a_view_for_every_label() {
    let _warm = warm_guard();
    // The same shape as the adjacency defect, in the same function: warming
    // built the UNTYPED `members(None)` aggregate and left the per-LABEL views
    // — the ones `MATCH (p:Person ...)` anchors on — to be built by whichever
    // client asked first.
    let g = graph();
    run(&g, "CREATE (:Person {id: 1})");
    run(&g, "CREATE (:Message {id: 2})");
    run(&g, "CREATE (:Comment {id: 3})");

    let before = counters::WARM_LABEL_MEMBERSHIPS.load(Relaxed);
    let _ = g.warm();
    let built = counters::WARM_LABEL_MEMBERSHIPS.load(Relaxed) - before;

    assert!(
        built >= 3,
        "warming built {built} per-label membership view(s); three labels exist. \
         A 0 here means only the untyped aggregate is warmed."
    );
}

#[test]
fn warming_with_no_declared_indexes_is_harmless() {
    let _warm = warm_guard();
    // A graph with no declared index must warm cleanly and answer normally —
    // the loop must not be a new failure mode for the common case.
    let g = graph();
    run(&g, "CREATE (:Person {id: 1})");
    let before = counters::WARM_INDEXES_BUILT.load(Relaxed);
    let _ = g.warm();
    assert_eq!(
        counters::WARM_INDEXES_BUILT.load(Relaxed) - before,
        0,
        "nothing was declared, so nothing should be built"
    );
    let r = run(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    assert_eq!(r.rows.len(), 1, "the graph must still answer after warming");
}

#[test]
fn warming_builds_the_composite_not_just_its_single_keys() {
    let _warm = warm_guard();
    // The declared-index loop walks a def's properties ONE AT A TIME, so a
    // two-key declaration was warmed as two single-key indexes and the
    // COMPOSITE — the structure fix 115 built and the planner seeks — was
    // still built by whichever client asked first.
    let g = graph();
    ddl(
        &g,
        "CREATE INDEX u_ut FOR (n:UserDataNode) ON (n.userId, n.nodeType)",
    );
    for i in 0..32 {
        run(
            &g,
            &format!("CREATE (:UserDataNode {{userId: 'u{i}', nodeType: 'note'}})"),
        );
    }

    let before = counters::WARM_COMPOSITE_INDEXES.load(Relaxed);
    let _ = g.warm();
    let built = counters::WARM_COMPOSITE_INDEXES.load(Relaxed) - before;

    assert!(
        built >= 1,
        "warming built {built} composite index(es); one two-key index was \
         declared. A 0 here means the composite is still built on the first \
         two-key seek's thread."
    );
}

#[test]
fn warming_builds_declared_trigram_indexes() {
    let _warm = warm_guard();
    // Covered by no warm pass at all: the first CONTAINS predicate per
    // (label, property) paid for the build.
    let g = graph();
    ddl(&g, "CREATE TRIGRAM INDEX d_t FOR (d:Doc) ON (d.title)");
    for i in 0..32 {
        run(&g, &format!("CREATE (:Doc {{title: 'chapter {i}'}})"));
    }

    let before = counters::WARM_TRIGRAM_INDEXES.load(Relaxed);
    let _ = g.warm();
    let built = counters::WARM_TRIGRAM_INDEXES.load(Relaxed) - before;

    assert!(
        built >= 1,
        "warming built {built} trigram index(es); one was declared."
    );
}

#[test]
fn warming_builds_declared_fulltext_indexes() {
    let _warm = warm_guard();
    // Same omission as the trigram one: the catalogue was never enumerated at
    // boot, so the first `db.index.fulltext.*` call paid the build.
    let g = graph();
    ddl(
        &g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title]",
    );
    for i in 0..32 {
        run(&g, &format!("CREATE (:Doc {{title: 'chapter {i}'}})"));
    }

    let before = counters::WARM_TERM_INDEXES.load(Relaxed);
    let _ = g.warm();
    let built = counters::WARM_TERM_INDEXES.load(Relaxed) - before;

    assert!(
        built >= 1,
        "warming built {built} fulltext index(es); one was declared."
    );
}

#[test]
fn warming_a_REOPENED_store_covers_every_label_not_just_the_cached_ones() {
    let _warm = warm_guard();
    // THE DEFECT THIS PINS, measured at SF10: a server that warmed a store from
    // cold reported `memberships 487 MB in 3 label(s)`, while adopting the SAME
    // store from its sidecar restored `718 MB in 10 label(s)`. Warming iterated
    // `self.labels`, which `Graph::token` fills ONE NAME AT A TIME on first use
    // and nothing bulk-loads at open — so the loop could only ever warm the
    // labels the boot path had already touched. It ran, its counter moved, and
    // two thirds of the label memberships stayed cold.
    //
    // A second `Graph` over the SAME `Store` is exactly that situation: the
    // store holds every label token, the new graph's cache holds none.
    let store = Store::new();
    {
        let g = Graph::new(store.clone(), Realm(1), Namespace(1));
        for (i, label) in ["Person", "Message", "Comment", "Post", "Forum"]
            .into_iter()
            .enumerate()
        {
            run(&g, &format!("CREATE (:{label} {{id: {i}}})"));
        }
    }

    // Reopened: same store, a graph whose token cache has never been used.
    let reopened = Graph::new(store, Realm(1), Namespace(1));
    assert!(
        reopened.labels_cached_for_test() < 5,
        "the fixture must start with a COLD cache or it proves nothing; it \
         already holds {} label(s)",
        reopened.labels_cached_for_test()
    );

    let before = counters::WARM_LABEL_MEMBERSHIPS.load(Relaxed);
    let _ = reopened.warm();
    let built = counters::WARM_LABEL_MEMBERSHIPS.load(Relaxed) - before;

    assert!(
        built >= 5,
        "warming built {built} per-label view(s); five labels are in the store. \
         Anything less means warming is reading the lazily-filled cache rather \
         than the catalogue, and is warming a fraction of the graph while \
         reporting success."
    );
}
