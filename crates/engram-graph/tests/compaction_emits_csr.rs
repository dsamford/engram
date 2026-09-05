#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! §5.2 — a full paged compaction EMITS the derived bases it walked past.
//!
//! `build_adj_table` rescans the whole adjacency span (17.26M rows at official
//! LDBC SF1, across ~32 tables) and `nodes_by_label_committed` rescans the
//! membership span. That rebuild is the derived-refresh tax, and it is why
//! every headline write number in this programme so far was measured with the
//! refresh pass turned OFF.
//!
//! Compaction already walks every one of those rows, in key order, because it
//! has to write them out sorted — and that key order IS a CSR. So the base can
//! be produced by work that must happen anyway.
//!
//! # The bar
//!
//! **Byte-identical `offsets` and `entries`**, not merely equal traversal
//! answers. Two tables can answer every query alike and still differ in layout,
//! and the layout is what the NEXT repair builds on: a differential that
//! compares only answers passes on a table whose successor is wrong.
//!
//! # What each test here is actually guarding
//!
//! The failure mode this whole path has to avoid is not a crash or a stale
//! read. It is a CSR published at a stamp it does not cover — which answers a
//! traversal with a subset, silently, with every checksum intact. So the
//! stamp's provenance gets its own test, and so does the case where a row
//! lands in the tail after the merge saw the segments.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-csr-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        TmpDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one statement in its OWN transaction.
///
/// Not `run_query` on its own: a bare `run_query` autocommits every store write
/// separately, so `CREATE (a)-[:R]->(b)` becomes nine independent commits at
/// nine timestamps. That has produced two false findings in this programme
/// already — a phantom pair of dangling edges, and eight nodes where one was
/// asked for — so every write here goes through the transaction the Bolt loop
/// would have opened.
fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g.commit_owned(txn).unwrap_or_else(|e| panic!("commit {src}: {e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            panic!("run {src}: {e:?}");
        }
    }
}

/// A corpus with enough shape to make the layout mean something: several
/// relationship types, a node with a high degree, a node with none, deletes so
/// the merge has tombstones to drop, and ids that are not dense.
///
/// SEALS ARE INTERLEAVED DELIBERATELY. A store with one segment is already
/// compacted and `compact_paged_observed` says so by doing nothing — so a
/// fixture that seals only once measures a no-op and every assertion below it
/// passes vacuously. That is exactly how this file failed on its first run.
fn build(g: &Graph, s: &Store) {
    for i in 0..24i64 {
        stmt(g, &format!("CREATE (:P {{id: {i}}})"));
        if i % 8 == 7 {
            s.seal();
        }
    }
    for i in 0..8i64 {
        stmt(g, &format!("CREATE (:Q {{id: {i}}})"));
    }
    s.seal();
    // A hub: P0 points at every other P.
    for i in 1..24i64 {
        stmt(
            g,
            &format!("MATCH (a:P {{id: 0}}), (b:P {{id: {i}}}) CREATE (a)-[:R]->(b)"),
        );
        if i % 8 == 0 {
            s.seal();
        }
    }
    // A second type, and a second direction into the hub.
    for i in 0..8i64 {
        stmt(
            g,
            &format!("MATCH (a:Q {{id: {i}}}), (b:P {{id: 0}}) CREATE (a)-[:S]->(b)"),
        );
    }
    s.seal();
    // Cross edges of both types, so the untyped table interleaves them.
    for i in 1..12i64 {
        stmt(
            g,
            &format!(
                "MATCH (a:P {{id: {i}}}), (b:P {{id: {}}}) CREATE (a)-[:S]->(b)",
                (i + 5) % 24
            ),
        );
    }
    s.seal();
    // AN UNSORTED ROW, deliberately. Within one node the key order is
    // `type | peer | rel`, so a single-type row always ascends by peer and
    // `sorted_by_peer` is trivially true — a fixture of only those cannot tell
    // an established flag from an assumed one. The hub already has :R edges to
    // high peers; a :S edge back to a LOW peer sorts after them by type and
    // before them by peer, so the untyped O table's hub row is non-monotone.
    stmt(
        g,
        "MATCH (a:P {id: 0}), (b:P {id: 1}) CREATE (a)-[:S]->(b)",
    );
    s.seal();
    // Deletes: tombstones for the merge to drop, and holes in the membership.
    for i in [3i64, 7, 11] {
        stmt(g, &format!("MATCH (n:Q {{id: {i}}}) DETACH DELETE n"));
    }
    s.seal();
}

/// Warm the tables and membership a reader would use, so there is a published
/// slot for compaction to emit into. Emitting only into published slots is what
/// bounds the merge's peak memory.
fn warm(g: &Graph) {
    // A probe only ADMITS a table build after `degree_table_after` probes on
    // the same adjacency epoch, and every relationship write resets that count
    // — so on a write-heavy fixture the gate is closed and the walk answers
    // instead. Opening it is what makes an unwarmed slot into a BUILT table,
    // which is the arm this file compares the emitted one against.
    g.set_degree_table_after(0);
    let _ = g.members(Some("P")).expect("members P");
    let _ = g.members(Some("Q")).expect("members Q");
    for i in 0..24u64 {
        let _ = g.adjacent_slim(i, engram_graph::Dir::Out, &None);
        let _ = g.adjacent_slim(i, engram_graph::Dir::In, &None);
    }
    let r = g.type_tokens_peek(&["R".to_string()]);
    for i in 0..24u64 {
        let _ = g.adjacent_slim(i, engram_graph::Dir::Out, &r);
    }
}

/// The lowest node id carrying `label`. Node ids are MINTED, not the `id`
/// property the fixture writes, and they do not start at zero — asking for
/// adjacency at a literal 0 reads an absent node and answers nothing, which is
/// indistinguishable from a table that lost every row.
fn first_member(g: &Graph, label: &str) -> u64 {
    *g.members(Some(label))
        .expect("members")
        .to_arc_vec()
        .first()
        .expect("at least one member")
}

/// A graph and the store handle behind it — the store is a private field on
/// `Graph`, and compaction is a store operation.
fn graph_on(store: &Store) -> std::sync::Arc<Graph> {
    std::sync::Arc::new(Graph::new(store.clone(), Realm(1), Namespace(1)))
}

/// The derived structures this file compares: named adjacency tables, and
/// named membership id-sets.
type Derived = (
    Vec<(String, engram_graph::AdjTableParts)>,
    Vec<(String, Vec<u64>)>,
);

/// Read back every derived structure this file compares.
fn parts(g: &Graph) -> Derived {
    let mut tables = Vec::new();
    for (name, tag_byte, types) in [
        ("O/any", b'O', None),
        ("I/any", b'I', None),
        ("O/R", b'O', g.type_tokens_peek(&["R".to_string()])),
    ] {
        if let Some(p) = g.adj_table_parts_for_test(tag_byte, &types) {
            tables.push((name.to_string(), p));
        }
    }
    let mut members = Vec::new();
    for l in ["P", "Q"] {
        if let Some((_, ids)) = g.members_snapshot_for_test(l) {
            members.push((l.to_string(), ids));
        }
    }
    (tables, members)
}

/// One arm of the differential, at the SAME store state, reached two ways.
///
/// # Why the sequence is what it is
///
/// `Slot::publish_snapshot` is a CAS that only wins when the stamp ADVANCES.
/// So a compaction whose stamp is below a table the reader has already
/// published loses silently, and the slot still holds the reader's table. A
/// first draft of this file warmed the tables and then compacted, compared the
/// two arms, and passed — while comparing a reader-built table against the same
/// reader-built table on both arms. Deleting the observer's type filter did not
/// move it, which is how that was caught.
///
/// The sequence below is the one a live server actually runs, and it is also
/// the one that makes the comparison mean something: warm, then WRITE MORE and
/// seal, so the merged segments carry rows above the reader's stamp and the
/// compaction's publish wins on its merits.
///
/// The OFF arm then reads the same store through a FRESH graph, whose empty
/// caches force a from-scratch `build_adj_table` — a pure base with an empty
/// overlay, which is exactly the reference the emitted base has to match.
fn arm(tag: &str, csr: bool) -> Derived {
    let dir = TmpDir::new(tag);
    let store = Store::new();
    let g = graph_on(&store);
    g.set_compaction_csr(csr);
    build(&g, &store);
    warm(&g);
    let warm_stamp = g
        .adj_table_parts_for_test(b'O', &None)
        .expect("the warm must publish an O/any table")
        .at;

    // Writes ABOVE the warm's stamp, sealed so the merge sees them.
    for i in 24..32i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})"));
    }
    for i in 24..32i64 {
        stmt(
            &g,
            &format!("MATCH (a:P {{id: 0}}), (b:P {{id: {i}}}) CREATE (a)-[:R]->(b)"),
        );
    }
    store.seal();

    let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    let (retired, dropped) =
        engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache)
            .expect("compaction");
    assert!(
        retired > 0 || dropped > 0,
        "[{tag}] the fixture must give compaction something to reclaim, or \
         this differential compares two no-ops"
    );

    if csr {
        let after = g
            .adj_table_parts_for_test(b'O', &None)
            .expect("O/any after the merge");
        assert!(
            after.at > warm_stamp,
            "[{tag}] the emit's publish must have WON — a publish that loses \
             the CAS leaves the reader's own table in the slot, and comparing \
             that against a built table compares a build to a build \
             ({} vs {warm_stamp})",
            after.at
        );
        return parts(&g);
    }
    // A fresh graph over the same compacted store: empty caches, so every
    // structure below is built from scratch by the ordinary span walk.
    let g2 = graph_on(&store);
    warm(&g2);
    parts(&g2)
}

/// THE DIFFERENTIAL. A table emitted by the merge must be byte-identical to the
/// one the ordinary span walk produces.
#[test]
fn an_emitted_csr_is_byte_identical_to_a_built_one() {
    let (on_tables, on_members) = arm("on", true);
    let (off_tables, off_members) = arm("off", false);

    assert!(
        !on_tables.is_empty() && !off_tables.is_empty(),
        "both arms must publish tables, or this compares two absences: \
         on {} off {}",
        on_tables.len(),
        off_tables.len()
    );
    assert_eq!(
        on_tables.len(),
        off_tables.len(),
        "the arms must publish the SAME tables"
    );

    for ((n_on, on), (n_off, off)) in on_tables.iter().zip(off_tables.iter()) {
        assert_eq!(n_on, n_off, "table order");
        assert_eq!(
            on.offsets, off.offsets,
            "[{n_on}] offsets must be byte-identical, not merely equivalent"
        );
        assert_eq!(on.entries, off.entries, "[{n_on}] entries must be byte-identical");
        assert_eq!(
            on.sorted_by_peer, off.sorted_by_peer,
            "[{n_on}] the sorted claim is what a binary search over a row \
             relies on — it must be ESTABLISHED the same way on both arms"
        );
        if n_on == "O/any" {
            assert!(
                !on.sorted_by_peer,
                "the fixture's hub carries two edge types with the second \
                 pointing BACK at a low peer, so the untyped table must be \
                 unsorted — otherwise the flag agrees on both arms for the \
                 trivial reason and this assertion proves nothing"
            );
        }
        assert_eq!(
            on.overlay, off.overlay,
            "[{n_on}] an emitted base must not smuggle rows into the overlay"
        );
    }
    assert_eq!(
        on_members, off_members,
        "membership bases must be byte-identical too — the merge walks \
         'L|label|node' in the exact order MembersView's base wants"
    );
}

/// THE CANARY, staged on the case §5.2 exists for: a table a reader NEVER
/// BUILDS.
///
/// A hop only admits a table build after `degree_table_after` probes on the
/// same adjacency epoch, and every relationship write resets that count. So on
/// a write-heavy corpus — the workload this whole programme is about — the gate
/// stays shut, the slot stays empty, and every hop walks the span. Compaction
/// walks those rows anyway, so it can fill the slot the reader never will.
///
/// Two halves, and both are needed. The publish half alone could be publishing
/// a table nobody can use; the answer half alone could be comparing a walk
/// against a walk — the mistake `index_agrees_with_scan.rs` documents having
/// made, and the reason every lever in this programme ships with a counter
/// half.
#[test]
fn compaction_fills_a_slot_a_write_heavy_reader_never_builds() {
    // Note: NO `set_degree_table_after(0)` here — the default gate, closed by
    // the fixture's own writes, is the point.
    let run = |csr: bool| -> (bool, Vec<u64>) {
        let dir = TmpDir::new(if csr { "gate-on" } else { "gate-off" });
        let store = Store::new();
        let g = graph_on(&store);
        g.set_compaction_csr(csr);
        build(&g, &store);
        // One hop per node, which creates the SLOT (the reader asked) without
        // admitting a build (the gate is shut).
        let hub = first_member(&g, "P");
        let _ = g.adjacent_slim(hub, engram_graph::Dir::Out, &None);
        let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
        let _ = g.adjacent_slim(hub, engram_graph::Dir::Out, &None);

        let list = [std::sync::Arc::clone(&g)];
        engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache)
            .expect("compaction");
        let published = g.adj_table_parts_for_test(b'O', &None).is_some();
        let peers: Vec<u64> = g
            .adjacent_slim(hub, engram_graph::Dir::Out, &None)
            .iter()
            .map(|e| e.peer)
            .collect();
        (published, peers)
    };
    let (on_pub, on_peers) = run(true);
    let (off_pub, off_peers) = run(false);
    eprintln!(
        "[compaction csr] O/any published after a full compaction: {on_pub} with \
         the emit, {off_pub} without it ({} peers either way)",
        on_peers.len()
    );
    assert!(
        on_pub,
        "the emit must fill the slot the closed gate left empty"
    );
    assert!(
        !off_pub,
        "with the lever off nothing may be published, or the ON arm's publish \
         is not what this test is attributing the difference to"
    );
    assert!(
        !on_peers.is_empty(),
        "the fixture's hub must have out-edges, or both arms answer nothing \
         and agreeing proves nothing"
    );
    assert_eq!(
        on_peers, off_peers,
        "and the emitted table must answer EXACTLY what the span walk answers \
         — same peers, same order"
    );
}

/// THE STAMP. The one genuinely dangerous outcome on this path is a base
/// published at a stamp it does not cover: it answers a traversal with a
/// subset, silently, with every checksum intact.
///
/// So this pins the direction of the inequality — the published stamp must not
/// exceed the store's clock at the moment the merge ran — and pins that a row
/// written AFTER the merge is still found, which is the observable consequence.
#[test]
fn a_row_written_after_the_merge_is_still_found() {
    let dir = TmpDir::new("after");
    let store = Store::new();
    let g = graph_on(&store);
    build(&g, &store);
    warm(&g);
    let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    warm(&g);
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache).expect("compaction");

    let stamp = g
        .adj_table_parts_for_test(b'O', &None)
        .expect("a published O/any table")
        .at;
    assert!(
        stamp <= store.now_ts(),
        "a base may never claim a stamp beyond the clock: {stamp} > {}",
        store.now_ts()
    );

    // A new edge lands in the TAIL, strictly above every merged segment.
    let hub = first_member(&g, "P");
    stmt(
        &g,
        "MATCH (a:P {id: 0}), (b:Q {id: 0}) CREATE (a)-[:R]->(b)",
    );
    let out = g.adjacent_slim(hub, engram_graph::Dir::Out, &None);
    let peers: Vec<u64> = out.iter().map(|e| e.peer).collect();
    let q0 = first_member(&g, "Q");
    assert!(
        peers.contains(&q0),
        "the edge written after the merge must be visible — the emitted base \
         covers the segments, and the change log covers everything above it. \
         peers {peers:?}, wanted {q0}"
    );
}

/// A TOMBSTONE THAT SURVIVES THE MERGE must not enter the derived structures.
///
/// The ordinary fixture never exercises this: a full compaction purges every
/// tombstone at or below the gc watermark, so `visit`'s `live` test is dead
/// code there — deleting it changed no result, which is how this gap was found.
/// A tombstone survives only when a PINNED READER holds the watermark below the
/// delete, and then it still shadows a base row: admitting it would put a
/// deleted node in the membership base and a dead edge in the CSR, silently.
#[test]
fn a_surviving_tombstone_is_not_a_member_and_not_an_edge() {
    let dir = TmpDir::new("pinned");
    let store = Store::new();
    let g = graph_on(&store);
    build(&g, &store);
    warm(&g);
    let before: Vec<u64> = g.members(Some("Q")).expect("Q").to_arc_vec().to_vec();
    assert!(before.len() >= 3, "the fixture needs Q members to delete");

    // A reader pinned HERE holds the gc watermark below everything that
    // follows, so the deletes below cannot be purged by the merge.
    let _pin = store.pin_snapshot();

    for i in [0i64, 2, 4] {
        stmt(&g, &format!("MATCH (n:Q {{id: {i}}}) DETACH DELETE n"));
    }
    store.seal();
    // Writes above the warm's stamp so the merge's publish wins.
    for i in 40..48i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})"));
    }
    store.seal();

    let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache).expect("compaction");

    let (ratio, versions) = store.tombstone_ratio();
    assert!(
        ratio > 0.0 && versions > 0,
        "the pin must actually have kept tombstones alive through the merge, \
         or this test proves nothing about the live check: ratio {ratio}, \
         versions {versions}"
    );

    let (emitted_tables, emitted_members) = parts(&g);
    let g2 = graph_on(&store);
    warm(&g2);
    let (built_tables, built_members) = parts(&g2);
    assert_eq!(
        emitted_members, built_members,
        "a deleted node's membership row is a surviving TOMBSTONE, not a member"
    );
    assert_eq!(
        emitted_tables.len(),
        built_tables.len(),
        "both must publish the same tables"
    );
    for ((n_e, e), (n_b, b)) in emitted_tables.iter().zip(built_tables.iter()) {
        assert_eq!(n_e, n_b, "table order");
        assert_eq!(e.offsets, b.offsets, "[{n_e}] offsets");
        assert_eq!(e.entries, b.entries, "[{n_e}] entries");
    }
}

/// A compaction on a graph with NOTHING published must publish nothing.
///
/// The temptation is to emit every bucket unconditionally, which at SF1 adds
/// ~1.6 GB to the merge's peak for tables no reader asked for. This pins the
/// scoping rule so it cannot be quietly widened.
#[test]
fn a_graph_with_no_published_tables_emits_nothing() {
    let dir = TmpDir::new("cold");
    let store = Store::new();
    let g = graph_on(&store);
    build(&g, &store);
    // Deliberately NOT warmed: no slot exists.
    let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache).expect("compaction");
    assert!(
        g.adj_table_parts_for_test(b'O', &None).is_none(),
        "an unpublished table has no reader waiting on it, and emitting one \
         costs the merge memory nobody asked for"
    );
}
