//! The derived-structure protocol (`derived.rs`), proven where the old caches
//! failed: a write to one source leaves every other source's structures
//! current; a structure catches up in O(delta) instead of rebuilding; a
//! transaction's changes reach the structures at COMMIT, not before; and N
//! readers racing a writer end up agreeing with a scan.
//!
//! Each test names the counters it expects NOT to fire. "Built" firing after
//! a warm-up is the defect this module exists to end.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

/// The ids a query returns in its first column, sorted — the scan an
/// snapshot must agree with.
fn ids(g: &Graph, src: &str) -> Vec<u64> {
    let mut v: Vec<u64> = run(g, src)
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i as u64,
            other => panic!("not an id: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

/// Run `src` the way the Bolt server runs every write statement: inside an
/// owned transaction, committed afterwards.
fn in_txn(g: &Graph, src: &str) {
    let q = engram_cypher::parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || engram_graph::run_query(g, &q, BTreeMap::new()));
    r.unwrap_or_else(|e| panic!("txn `{src}`: {e}"));
    g.commit_owned(txn).expect("commit");
}

/// A COMMITTED transaction costs its readers what a direct write costs them —
/// an O(delta) catch-up of each derived structure it touched — and never a
/// rebuild. The first cut remembered only which SOURCES a transaction
/// touched and `touch`ed their logs at commit, which a reader can only
/// answer with a rebuild; with every Bolt statement a transaction, every
/// insert made the next statement rebuild two memberships, a range index
/// and two adjacency tables: 130 ms per insert on the pod, 8 ops/s where
/// the direct path did 4,200. This pins the entries being replayed instead.
#[test]
fn a_committed_transaction_is_caught_up_with_not_rebuilt_from() {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "UNWIND range(0, 999) AS i CREATE (:Person {id: i})");
    run(
        &g,
        "UNWIND range(0, 499) AS i MATCH (a:Person {id: i}), (b:Person {id: i + 1}) \
         CREATE (a)-[:KNOWS]->(b)",
    );
    // Warm every structure the inserts below will touch.
    let _ = g.members(None).expect("all nodes");
    let _ = g.members(Some("Person")).expect("Person");
    assert_eq!(ids(&g, "MATCH (p:Person {id: 5}) RETURN id(p)").len(), 1);
    assert_eq!(ids(&g, "MATCH (p:Person)-[:KNOWS]->(q) RETURN id(q)").len(), 500);
    // The count-over-type shape is what builds a per-type adjacency table.
    assert_eq!(ids(&g, "MATCH (:Person)-[:KNOWS]->(q:Person) RETURN count(*)"), vec![500]);
    assert_eq!(g.count_label_nodes("Person"), 1000);

    let ((), t) = engram_observe::with_trace(|| {
        // The SNB insert shape: two labels, an indexed property, a new edge type.
        in_txn(
            &g,
            "MATCH (p:Person {id: 7}) CREATE (m:Message:Comment {id: 1000001, \
             creationDate: 1, content: 'x', length: 1})-[:HAS_CREATOR]->(p)",
        );
        // An edge of an EXISTING type between existing nodes: its table repairs.
        in_txn(&g, "MATCH (a:Person {id: 7}), (b:Person {id: 900}) CREATE (a)-[:KNOWS]->(b)");
        assert_eq!(g.members(None).expect("all nodes").len(), 1001);
        assert_eq!(g.members(Some("Person")).expect("Person").len(), 1000);
        assert_eq!(ids(&g, "MATCH (p:Person {id: 7}) RETURN id(p)").len(), 1);
        assert_eq!(ids(&g, "MATCH (:Person)-[:KNOWS]->(q:Person) RETURN count(*)"), vec![501]);
        assert_eq!(ids(&g, "MATCH (p:Person)-[:KNOWS]->(q) RETURN id(q)").len(), 501);
        assert_eq!(g.count_label_nodes("Person"), 1000);
        assert_eq!(g.count_all_nodes(), 1001);
    });
    let c = t.counters();
    assert_eq!(counter(&t, "derived.change log touched"), 0, "nothing may be touched: {c:?}");
    assert_eq!(counter(&t, "graph.membership snapshots built"), 0, "no membership rebuild: {c:?}");
    assert_eq!(counter(&t, "graph.range index builds"), 0, "no index rebuild: {c:?}");
    assert_eq!(counter(&t, "graph.adjacency tables built"), 0, "no adjacency rebuild: {c:?}");
    assert_eq!(counter(&t, "graph.stats rebuilt"), 0, "no stats rebuild: {c:?}");
    assert!(
        counter(&t, "graph.membership snapshots caught up") >= 1,
        "the memberships caught up from their logs: {c:?}"
    );
    assert!(
        counter(&t, "graph.adjacency tables repaired") >= 1,
        "the KNOWS table repaired from its log: {c:?}"
    );
}

// ── Transactions ────────────────────────────────────────────────────────────

#[test]
fn a_transactions_membership_change_reaches_the_snapshot_at_commit_not_before() {
    let g = graph();
    run(&g, "UNWIND range(0, 99) AS i CREATE (:L {id: i})");
    let before = g.members(Some("L")).expect("members");
    assert_eq!(before.len(), 100);

    g.begin_txn().expect("begin");
    let props: BTreeMap<String, Value> = [("id".to_string(), Value::Int(100))].into();
    g.create_node(&["L".to_string()], &props).expect("create");
    // Inside the transaction THIS thread sees its own buffered write laid
    // over the committed snapshot (read-your-writes) …
    assert_eq!(
        g.members(Some("L")).expect("members").len(),
        101,
        "the transaction must see its own write through the membership overlay"
    );
    // … while the SHARED snapshot — what any other session reads — is still
    // committed state: a buffered write must not reach it before it commits.
    std::thread::scope(|s| {
        let g = &g;
        s.spawn(move || {
            assert_eq!(
                g.members(Some("L")).expect("members").len(),
                100,
                "a buffered write must not reach the shared snapshot before it commits"
            );
        });
    });
    g.commit_txn().expect("commit");

    let after = g.members(Some("L")).expect("members");
    assert_eq!(after.len(), 101, "the committed node joins the label");
    assert_eq!(
        after.iter().collect::<Vec<_>>(),
        ids(&g, "MATCH (n:L) RETURN id(n)"),
        "the snapshot must agree with a scan after the commit"
    );
}

#[test]
fn a_rolled_back_transaction_leaves_the_snapshot_exactly_as_it_was() {
    let g = graph();
    run(&g, "UNWIND range(0, 99) AS i CREATE (:L {id: i})");
    assert_eq!(g.members(Some("L")).expect("members").len(), 100);

    g.begin_txn().expect("begin");
    let props: BTreeMap<String, Value> = [("id".to_string(), Value::Int(100))].into();
    g.create_node(&["L".to_string()], &props).expect("create");
    g.rollback_txn();

    let ((), t) = engram_observe::with_trace(|| {
        let m = g.members(Some("L")).expect("members");
        assert_eq!(m.len(), 100);
        assert_eq!(m.iter().collect::<Vec<_>>(), ids(&g, "MATCH (n:L) RETURN id(n)"));
    });
    assert_eq!(
        counter(&t, "graph.membership snapshots built"),
        0,
        "a rollback changed nothing, so nothing should rebuild"
    );
}

// ── Memberships ─────────────────────────────────────────────────────────────

#[test]
fn a_write_to_one_label_costs_the_others_nothing_and_its_own_an_o_delta_catch_up() {
    let g = graph();
    run(&g, "UNWIND range(0, 999) AS i CREATE (:L {id: i})");
    run(&g, "UNWIND range(0, 999) AS i CREATE (:M {id: i})");
    let _ = g.members(Some("L")).expect("warm L");
    let _ = g.members(Some("M")).expect("warm M");

    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "CREATE (:L {id: 5000})");
        let l = g.members(Some("L")).expect("L");
        let m = g.members(Some("M")).expect("M");
        assert_eq!(l.len(), 1001);
        assert!(l.contains(l.iter().last().expect("one")));
        assert_eq!(m.len(), 1000);
    });
    assert_eq!(
        counter(&t, "graph.membership snapshots built"),
        0,
        "a single insert must not rebuild any label: {:?}",
        t.counters()
    );
    assert!(
        counter(&t, "graph.membership snapshots caught up") >= 1,
        "L caught up from its change log: {:?}",
        t.counters()
    );
    assert!(
        counter(&t, "graph.membership snapshots still current") >= 1,
        "M, untouched, was served as current: {:?}",
        t.counters()
    );
}

/// The consumers that need a contiguous slice (the columnar walks) pay the
/// O(label) materialisation ONCE per snapshot, not once per read: after a
/// write the snapshot carries an overlay for up to 4,096 changes, and under
/// a read-only load that overlay lives for ever. Measured on the pod before
/// this was pinned: the 90k-id `:Message` label copied on every pipeline
/// statement.
#[test]
fn a_slice_consumer_materialises_a_snapshot_once_not_per_read() {
    let g = graph();
    run(&g, "UNWIND range(0, 4999) AS i CREATE (:L {id: i, v: i % 7})");
    run(&g, "MATCH (n:L) RETURN count(n.v)"); // warm: snapshot built, no overlay
    run(&g, "CREATE (:L {id: 5000, v: 1})"); // one change: the overlay is live
    let ((), t) = engram_observe::with_trace(|| {
        for _ in 0..20 {
            assert_eq!(
                run(&g, "MATCH (n:L) WHERE n.v = 1 RETURN count(n) AS c").rows,
                vec![vec![Value::Int(716)]] // 715 of the 5,000, plus the one just written
            );
        }
    });
    assert_eq!(counter(&t, "graph.membership snapshots built"), 0, "{:?}", t.counters());
    assert!(
        counter(&t, "derived.members view materialised") <= 1,
        "twenty reads over one snapshot must materialise it at most once: {:?}",
        t.counters()
    );
}

#[test]
fn label_add_and_remove_flow_through_the_snapshot_without_a_rebuild() {
    let g = graph();
    run(&g, "UNWIND range(0, 199) AS i CREATE (:L {id: i})");
    let _ = g.members(Some("L")).expect("warm");
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:L) WHERE n.id < 50 REMOVE n:L");
        run(&g, "MATCH (n) WHERE n.id >= 190 SET n:X");
        run(&g, "MATCH (n:X) SET n:L"); // re-add: a no-op for membership
        let m = g.members(Some("L")).expect("L");
        assert_eq!(m.len(), 150);
        assert_eq!(m.iter().collect::<Vec<_>>(), ids(&g, "MATCH (n:L) RETURN id(n)"));
        assert!(!m.contains(0));
        assert!(m.contains(199));
    });
    assert_eq!(counter(&t, "graph.membership snapshots built"), 0, "{:?}", t.counters());
}

// ── Range indexes ───────────────────────────────────────────────────────────

#[test]
fn an_index_catches_up_from_its_log_and_ignores_writes_to_other_properties() {
    let g = graph();
    ddl(&g, "CREATE INDEX l_id FOR (n:L) ON (n.id)");
    run(&g, "UNWIND range(0, 999) AS i CREATE (:L {id: i, name: 'n' + toString(i)})");
    assert_eq!(run(&g, "MATCH (n:L {id: 5}) RETURN n.name").rows.len(), 1); // warm
    // Warm the LABEL-SCOPED index explicitly, because that is the one the
    // anchored seek now uses and the read above does not necessarily build it:
    // that statement can be served by the columnar projection, which never
    // seeks. Before seeks were scoped, both paths shared one partition-wide
    // `id` index and the read alone was enough to warm it.
    //
    // This is the test noticing a real consequence rather than an accident of
    // fixtures: scoping means one index per (label, property), so each pair
    // pays its own first build. At SF1 the `Message.id` index is built over
    // ~3M members — a one-off, and the reason `idx_builds` is watched on the
    // pod run rather than assumed to stay at 1.
    let _ = g
        .ensure_range_index_for_test("id", Some("L"))
        .expect("scoped L.id index");
    // And the PARTITION-WIDE one. Scoping the anchored seek did not remove the
    // unscoped index's other users — the range and ordered paths still ask for
    // it — so a property that is both seeked and ranged now has two indexes,
    // and a test that warms one still builds the other mid-trace.
    //
    // That is a cost this change introduces and it is written down here rather
    // than absorbed: two overlays maintained per property instead of one, in
    // exchange for a seek that a write to another label no longer invalidates.
    let _ = g.ensure_range_index_for_test("id", None);
    // `name` too, both ways: the traced regions below read `n.name`, and the
    // columnar/range paths build an index for it on first use. Warming every
    // index a region will touch is the only way the region's build count is a
    // statement about the CHANGE under test rather than about which index
    // happened to be cold.
    let _ = g.ensure_range_index_for_test("name", Some("L"));
    let _ = g.ensure_range_index_for_test("name", None);

    // A write to ANOTHER property: the index is still current.
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:L {id: 7}) SET n.name = 'renamed'");
        assert_eq!(run(&g, "MATCH (n:L {id: 5}) RETURN n.name").rows.len(), 1);
    });
    assert_eq!(counter(&t, "graph.range index builds"), 0, "{:?}", t.counters());
    assert!(
        counter(&t, "graph.range index still current") + counter(&t, "graph.range index cache hit")
            >= 1,
        "{:?}",
        t.counters()
    );

    // A write to the INDEXED property: the index catches up, and answers it.
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "CREATE (:L {id: 5000, name: 'new'})");
        run(&g, "MATCH (n:L {id: 5}) SET n.id = 6000");
        assert_eq!(
            run(&g, "MATCH (n:L {id: 5000}) RETURN n.name").rows,
            vec![vec![Value::Str("new".into())]]
        );
        assert_eq!(run(&g, "MATCH (n:L {id: 5}) RETURN n.name").rows.len(), 0);
        assert_eq!(run(&g, "MATCH (n:L {id: 6000}) RETURN n.name").rows.len(), 1);
    });
    // Back to an exact 0: `set_scoped_seek` ships OFF, so a property has one
    // index again and the fixture can enumerate what it warms. The bound this
    // replaced was introduced when scoped seeks were briefly on by default and
    // is not needed while they are not.
    //
    // Anchored seeks are now LABEL-SCOPED, so a property that is both seeked
    // and ranged has two indexes — `(L, id)` for the seek and the
    // partition-wide one for the range and ordered paths, which were not
    // changed. The warm above builds every index this test can name, and this
    // region still builds one: the topology now has more indexes than the
    // fixture can enumerate from outside.
    //
    // What the test is FOR is unchanged and still asserted exactly: a write to
    // the indexed property must be answered by a CATCH-UP, not by rescanning
    // the partition. That is the line below, and it is the one that would fail
    // if catch-up regressed. The bound here stops the count growing with the
    // number of statements, which is what "it rebuilt instead of catching up"
    // would look like.
    //
    // The catch-up semantics this weakens are covered directly, and on the
    // index rather than on the answer, by `scoped_index_catch_up.rs`.
    assert_eq!(counter(&t, "graph.range index builds"), 0, "{:?}", t.counters());
    assert!(counter(&t, "graph.range index caught up") >= 1, "{:?}", t.counters());
}

// ── Adjacency ───────────────────────────────────────────────────────────────

fn persons_in_cities(persons: i64) -> Graph {
    let g = graph();
    ddl(&g, "CREATE INDEX p_id FOR (n:Person) ON (n.id)");
    run(&g, "UNWIND range(0, 99) AS i CREATE (:City {id: i, name: 'City' + toString(i)})");
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (c:City {{id: i % 100}}) \
             CREATE (:Person {{id: i}})-[:IS_LOCATED_IN]->(c)",
            persons - 1
        ),
    );
    g
}

const AGG_BY_CITY: &str =
    "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) RETURN c.name, count(p) AS n ORDER BY n DESC, c.name LIMIT 3";

#[test]
fn a_node_write_or_another_types_edge_leaves_an_adjacency_table_current() {
    // 2,000 persons: past the probe gate, so the table is BUILT on the first
    // run and every hop of the second reuses it.
    let g = persons_in_cities(2_000);
    run(&g, AGG_BY_CITY);
    run(&g, AGG_BY_CITY);
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, AGG_BY_CITY);
    });
    let reused_alone = counter(&t, "graph.adjacency tables reused");
    assert!(reused_alone >= 2_000, "the warm run must serve every hop from the table: {:?}", t.counters());

    // A NODE write.
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "CREATE (:Message {id: 1})");
        run(&g, AGG_BY_CITY);
    });
    assert_eq!(counter(&t, "graph.adjacency tables built"), 0, "{:?}", t.counters());
    assert_eq!(counter(&t, "graph.adjacency tables repaired"), 0, "{:?}", t.counters());
    assert!(
        counter(&t, "graph.adjacency tables reused") >= reused_alone,
        "after a node write every hop must still come from the table — the probe \
         gate used to reset on the commit clock and send the first 1,024 hops to a \
         scan: {:?}",
        t.counters()
    );

    // An edge of ANOTHER type.
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (p:Person {id: 0}) CREATE (:Message {id: 2})-[:HAS_CREATOR]->(p)");
        run(&g, AGG_BY_CITY);
    });
    assert_eq!(counter(&t, "graph.adjacency tables built"), 0, "{:?}", t.counters());
    assert_eq!(counter(&t, "graph.adjacency tables repaired"), 0, "{:?}", t.counters());
    assert!(counter(&t, "graph.adjacency tables reused") >= reused_alone, "{:?}", t.counters());

    // An edge of THIS type: the table is repaired over the one row that moved,
    // and answers the new count.
    let before = run(&g, "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City {id: 1}) RETURN count(p)").rows;
    let ((), t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (p:Person {id: 0}), (c:City {id: 1}) CREATE (p)-[:IS_LOCATED_IN]->(c)");
        run(&g, AGG_BY_CITY);
    });
    assert_eq!(counter(&t, "graph.adjacency tables built"), 0, "{:?}", t.counters());
    assert!(counter(&t, "graph.adjacency tables repaired") >= 1, "{:?}", t.counters());
    let after = run(&g, "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City {id: 1}) RETURN count(p)").rows;
    let n = |rows: &Vec<Vec<Value>>| match &rows[0][0] {
        Value::Int(i) => *i,
        _ => panic!("count"),
    };
    assert_eq!(n(&after), n(&before) + 1, "the repaired table carries the new edge");
}

// ── Concurrency ─────────────────────────────────────────────────────────────

/// Four readers hammer the membership snapshot and the label intersection
/// while the main thread inserts and relabels. No reader may see a stale or
/// torn set, and at the end the snapshot must equal a scan. (The counters
/// are thread-local and so not asserted here; the O(delta) claim is the
/// single-threaded tests above.)
#[test]
fn concurrent_readers_racing_a_writer_agree_with_a_scan() {
    let g = std::sync::Arc::new(graph());
    run(&g, "UNWIND range(0, 1999) AS i CREATE (:L:M {id: i})");
    let _ = g.members(Some("L")).expect("warm");
    let baseline = 2_000usize;
    std::thread::scope(|s| {
        for _ in 0..4 {
            let g = std::sync::Arc::clone(&g);
            s.spawn(move || {
                for _ in 0..300 {
                    let m = g.members(Some("L")).expect("members");
                    // Monotone in this test: ids are only added to L, so a
                    // snapshot can never be smaller than the warm one.
                    assert!(m.len() >= baseline - 100, "a torn or stale snapshot: {}", m.len());
                    let both = g.members_all(&["L".to_string(), "M".to_string()]).expect("all");
                    assert!(both.len() <= m.len());
                }
            });
        }
        for i in 0..300u64 {
            run(&g, &format!("CREATE (:L {{id: {}}})", 10_000 + i));
            if i % 50 == 0 {
                run(&g, &format!("MATCH (n:M {{id: {i}}}) REMOVE n:M"));
            }
        }
    });
    let m = g.members(Some("L")).expect("members");
    assert_eq!(m.iter().collect::<Vec<_>>(), ids(&g, "MATCH (n:L) RETURN id(n)"));
    let both = g.members_all(&["L".to_string(), "M".to_string()]).expect("all");
    assert_eq!(both.iter().collect::<Vec<_>>(), ids(&g, "MATCH (n:L:M) RETURN id(n)"));
}
