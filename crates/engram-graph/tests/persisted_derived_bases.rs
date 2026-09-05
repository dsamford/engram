#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! §5.4 — the derived bases survive a restart.
//!
//! §5.2 makes the CSR a by-product of compaction and §5.3 stops the maintenance
//! pass rebuilding it, so a healthy server stops rebuilding *during* a
//! process's life. Neither does anything for the START of one: a cold SF1 paged
//! server walks 17.26M adjacency rows across ~32 buckets plus the membership
//! bases before it answers, ~43.2 s. Without this item Phase 5 is a
//! process-lifetime cache rather than a property of the store.
//!
//! # Why this file is mostly refusals
//!
//! A stale persisted CSR is the single most dangerous thing in this programme:
//! it answers a traversal with a SUBSET, silently, with every checksum intact.
//! Nothing goes red. So the tests that matter here are not the happy path — it
//! is one assertion — but the four ways a sidecar can be wrong while looking
//! right, each of which must REFUSE and fall back to a rebuild:
//!
//! 1. the body does not hash to its header (corruption),
//! 2. the sealed set moved underneath it (a segment added, removed, re-merged),
//! 3. the store's clock is above its stamp (rows it does not cover, with no
//!    change log at open to carry the difference),
//! 4. it belongs to another graph.
//!
//! Refusing costs a rebuild. Accepting wrongly costs a wrong answer. Every one
//! of these is asserted to refuse AND asserted to still answer correctly after
//! refusing, because a refusal that leaves the server broken is not a
//! safe failure.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Dir, Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-dsc-{}-{}-{}",
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

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g
            .commit_owned(txn)
            .unwrap_or_else(|e| panic!("commit {src}: {e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            panic!("run {src}: {e:?}");
        }
    }
}

fn graph_on(store: &Store) -> std::sync::Arc<Graph> {
    std::sync::Arc::new(Graph::new(store.clone(), Realm(1), Namespace(1)))
}

fn build(g: &Graph, s: &Store) {
    g.set_degree_table_after(0);
    for i in 0..24i64 {
        stmt(g, &format!("CREATE (:P {{id: {i}}})"));
        if i % 8 == 7 {
            s.seal();
        }
    }
    for i in 1..24i64 {
        stmt(
            g,
            &format!("MATCH (a:P {{id: 0}}), (b:P {{id: {i}}}) CREATE (a)-[:R]->(b)"),
        );
        if i % 8 == 0 {
            s.seal();
        }
    }
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
}

fn warm(g: &Graph) {
    g.set_degree_table_after(0);
    let _ = g.members(Some("P")).expect("members P");
    for i in 0..32u64 {
        let _ = g.adjacent_slim(i, Dir::Out, &None);
        let _ = g.adjacent_slim(i, Dir::In, &None);
    }
    let r = g.type_tokens_peek(&["R".to_string()]);
    for i in 0..32u64 {
        let _ = g.adjacent_slim(i, Dir::Out, &r);
    }
}

/// Every out-neighbourhood, as the answer a restart must still give.
fn answers(g: &Graph) -> Vec<(u64, Vec<u64>)> {
    let mut out = Vec::new();
    for i in 0..40u64 {
        let mut peers: Vec<u64> = g
            .adjacent_slim(i, Dir::Out, &None)
            .iter()
            .map(|e| e.peer)
            .collect();
        peers.sort_unstable();
        if !peers.is_empty() {
            out.push((i, peers));
        }
    }
    out
}

/// A store with a compacted paged set and a sidecar beside it — the state a
/// server is in when it shuts down cleanly.
///
/// Returns the store handle, so the "restart" below is a FRESH `Graph` over the
/// same segments: that is exactly what a restart is from the derived
/// structures' point of view, since every one of them is in-memory.
fn compacted_with_sidecar(dir: &std::path::Path) -> (Store, Vec<(u64, Vec<u64>)>) {
    let store = Store::new();
    let g = graph_on(&store);
    build(&g, &store);
    warm(&g);
    // Writes above the warm's stamp, so the compaction's publish wins and its
    // sidecar records something the process actually served.
    for i in 24..32i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})"));
        stmt(
            &g,
            &format!("MATCH (a:P {{id: 0}}), (b:P {{id: {i}}}) CREATE (a)-[:R]->(b)"),
        );
    }
    store.seal();
    let cache = store.into_paged(dir, 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, dir, &cache).expect("compaction");
    let truth = answers(&g);
    (store, truth)
}

fn sidecar_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "dsc"))
        .collect();
    v.sort();
    v
}

/// THE CLAIM: a restart adopts the bases instead of walking the span, and the
/// adopted bases are byte-identical to built ones.
#[test]
fn a_restart_adopts_the_bases_instead_of_rebuilding_them() {
    let dir = TmpDir::new("restart");
    let (store, truth) = compacted_with_sidecar(dir.path());
    assert_eq!(
        sidecar_files(dir.path()).len(),
        1,
        "the compaction must have written exactly one sidecar"
    );

    // ── The restart. A fresh graph over the same segments: every derived
    //    structure is in-memory, so this IS a cold start for them.
    let (adopted, built_on_adopt, answers_on) = {
        let g = graph_on(&store);
        let adopted = g.adopt_derived_sidecar(dir.path());
        let (_, trace) = engram_observe::with_trace(|| warm(&g));
        let built = trace
            .counters()
            .get("graph.adjacency tables built")
            .copied()
            .unwrap_or(0);
        (adopted, built, answers(&g))
    };
    // ── The same restart with the item off: the reference.
    let (built_without, answers_off, parts_off) = {
        let g = graph_on(&store);
        g.set_persist_derived(false);
        assert_eq!(
            g.adopt_derived_sidecar(dir.path()),
            0,
            "off must adopt nothing"
        );
        let (_, trace) = engram_observe::with_trace(|| warm(&g));
        let built = trace
            .counters()
            .get("graph.adjacency tables built")
            .copied()
            .unwrap_or(0);
        (built, answers(&g), g.adj_table_parts_for_test(b'O', &None))
    };

    eprintln!(
        "[persist derived] restart adopted {adopted} structure(s); adjacency \
         tables built during the warm: {built_on_adopt} with the sidecar, \
         {built_without} without it"
    );
    assert!(adopted > 0, "the sidecar must actually be adopted");
    assert!(
        built_without > 0,
        "the OFF arm must actually rebuild, or the ON arm's saving is measured \
         against nothing: {built_without}"
    );
    assert!(
        built_on_adopt < built_without,
        "adopting must REMOVE rebuilds: {built_on_adopt} vs {built_without}"
    );
    assert_eq!(
        answers_on, answers_off,
        "and an adopted base must answer exactly what a built one answers"
    );
    assert!(
        !truth.is_empty() && answers_on == truth,
        "including exactly what the process that wrote it answered"
    );

    // Byte-identity of the adopted base against the built one — the same bar
    // §5.2 holds the emitted base to, now across a process boundary.
    let g = graph_on(&store);
    g.adopt_derived_sidecar(dir.path());
    let parts_on = g
        .adj_table_parts_for_test(b'O', &None)
        .expect("adopted O/any");
    let parts_off = parts_off.expect("built O/any");
    assert_eq!(parts_on.offsets, parts_off.offsets, "offsets");
    assert_eq!(parts_on.entries, parts_off.entries, "entries");
    assert_eq!(parts_on.sorted_by_peer, parts_off.sorted_by_peer, "sorted");
}

/// CORRUPTION: a flipped byte refuses the RECORD it lands in and everything
/// after it — never the record as a good one — and the server must still be
/// right afterwards.
///
/// v2 of the sidecar is read one hashed record at a time (the dense, read-whole
/// v1 OOM-killed a 12Gi pod at adoption), so a flip in the middle of the file
/// costs the tables from that record on, not the ones verified before it.
/// The contract that matters is unchanged: no corrupt bytes are ever adopted,
/// and what is not adopted is built.
#[test]
fn a_corrupt_sidecar_is_refused_and_the_graph_is_still_correct() {
    let dir = TmpDir::new("corrupt");
    let (store, truth) = compacted_with_sidecar(dir.path());
    let file = sidecar_files(dir.path()).pop().expect("a sidecar");
    // How much a CLEAN sidecar adopts — the bar the corrupt one must fall short of.
    let clean = graph_on(&store).adopt_derived_sidecar(dir.path());
    assert!(clean > 1, "fixture: the clean sidecar must adopt several structures, got {clean}");

    let mut bytes = std::fs::read(&file).expect("read");
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(&file, &bytes).expect("write");

    let g = graph_on(&store);
    let adopted = g.adopt_derived_sidecar(dir.path());
    assert!(
        adopted < clean,
        "a corrupt record and everything after it must NOT be adopted (adopted {adopted} of {clean})"
    );
    let (_, trace) = engram_observe::with_trace(|| warm(&g));
    assert!(
        trace
            .counters()
            .get("graph.adjacency tables built")
            .copied()
            .unwrap_or(0)
            > 0,
        "and the graph must fall back to building — a refusal that leaves \
         nothing built is a refusal that broke the server"
    );
    assert_eq!(answers(&g), truth, "answers must be unaffected");

    // A flip in the FOOTER (the last bytes) or the table of contents refuses
    // the whole file: nothing is adopted, the graph is still right.
    let good = std::fs::read(&file).expect("read");
    let mut tail_bad = good.clone();
    let last = tail_bad.len() - 1;
    tail_bad[last] ^= 0x01;
    std::fs::write(&file, &tail_bad).expect("write");
    let g = graph_on(&store);
    assert_eq!(
        g.adopt_derived_sidecar(dir.path()),
        0,
        "a corrupt footer must adopt NOTHING"
    );
    warm(&g);
    assert_eq!(answers(&g), truth, "answers must be unaffected");
}

/// A MOVED SEALED SET: every byte intact, every checksum good, and the content
/// wrong — the failure a checksum cannot see, and the reason the vintage is the
/// sealed set rather than a timestamp.
#[test]
fn a_sealed_set_that_moved_refuses_an_intact_sidecar() {
    let dir = TmpDir::new("moved");
    let (store, truth) = compacted_with_sidecar(dir.path());

    // Move the sealed set the honest way: write and seal, which adds a segment
    // and changes the set's identity without touching the sidecar at all.
    let g0 = graph_on(&store);
    stmt(&g0, "CREATE (:P {id: 900})");
    store.seal();

    let g = graph_on(&store);
    assert_eq!(
        g.adopt_derived_sidecar(dir.path()),
        0,
        "a sidecar produced from a different sealed set must be refused, \
         however intact its bytes are"
    );
    warm(&g);
    let now = answers(&g);
    assert_eq!(
        now.len(),
        truth.len(),
        "the new node has no edges, so the neighbourhoods are unchanged"
    );
    assert_eq!(now, truth, "and the graph still answers correctly");
}

/// A STORE ABOVE THE STAMP: the sealed set can be identical and the store still
/// hold rows the sidecar does not cover — a WAL replay reconstructs exactly
/// that shape. At open there is no change log to carry the difference, so a
/// base below the clock would be treated as current while being short.
#[test]
fn a_store_newer_than_the_sidecar_is_refused() {
    let dir = TmpDir::new("newer");
    let (store, _truth) = compacted_with_sidecar(dir.path());
    let id_before = store.sealed_set_id();

    // A write that stays in the TAIL: the sealed set is untouched, so the
    // vintage still matches and only the clock check can catch this.
    let g0 = graph_on(&store);
    stmt(&g0, "CREATE (:P {id: 901})");
    assert_eq!(
        store.sealed_set_id(),
        id_before,
        "the sealed set must be UNCHANGED, or this test is exercising the \
         vintage check instead of the clock check"
    );

    let g = graph_on(&store);
    assert_eq!(
        g.adopt_derived_sidecar(dir.path()),
        0,
        "a store holding rows above the sidecar's stamp must refuse it: at \
         open there is no change log to carry the difference"
    );
}

/// A SIDECAR NEWER THAN THE STORE — the asymmetry the clock check cannot see,
/// and therefore the case that makes the sealed-set vintage load-bearing rather
/// than belt-and-braces.
///
/// `now_ts() > stamp` catches a store that has moved ON from the sidecar. It
/// says nothing about a store that has moved BACK: a restore from an older or
/// partial segment set, or a data directory pointed at the wrong store. There
/// the clock is at or below the stamp and every byte of the sidecar is perfect,
/// while it describes rows this store does not hold — a CSR naming edges that
/// are not there, which is the silent wrong answer in its purest form.
///
/// Written after finding that the two checks overlapped on every other fixture
/// here: deleting the vintage check changed no result, because the clock check
/// happened to fire first in all of them.
///
/// MEASURED, not argued: with the vintage check removed, this store adopts 4
/// structures from the other store's sidecar and then reports out-neighbours
/// for nodes that have no edges at all. The guard prevents a wrong answer, not
/// merely a wasted cache — which is why it refuses rather than warns.
#[test]
fn a_sidecar_newer_than_the_store_is_refused() {
    let dir = TmpDir::new("older-store");
    let (rich, _truth) = compacted_with_sidecar(dir.path());
    let rich_stamp = rich.now_ts();

    // A DIFFERENT, smaller store in the same directory — the shape a restore
    // from an older backup leaves behind. Its clock is below the sidecar's
    // stamp, so only the vintage can tell them apart.
    let poor = Store::new();
    let g = graph_on(&poor);
    stmt(&g, "CREATE (:P {id: 0})");
    poor.seal();
    assert!(
        poor.now_ts() <= rich_stamp,
        "the fixture needs the smaller store's clock at or below the \
         sidecar's stamp, or the CLOCK check would catch this and the vintage \
         check would go untested: {} vs {rich_stamp}",
        poor.now_ts()
    );
    assert_ne!(
        poor.sealed_set_id(),
        rich.sealed_set_id(),
        "and the two sealed sets must differ, or there is nothing to refuse"
    );

    assert_eq!(
        g.adopt_derived_sidecar(dir.path()),
        0,
        "a sidecar describing a sealed set this store does not have must be \
         refused — its rows name edges that are not here"
    );
    warm(&g);
    assert!(
        g.adjacent_slim(0, Dir::Out, &None).is_empty()
            && g.adjacent_slim(1, Dir::Out, &None).is_empty(),
        "and the store must answer from its OWN rows: the small store has no \
         edges at all, so any neighbour here came from the refused file"
    );
}

/// RE-STAMPING keeps the sidecar useful, and folding the overlay in is what
/// makes it honest.
///
/// A sidecar's vintage is the sealed set it came from, so the next seal
/// invalidates the one a compaction wrote. Measured on the pod: a 1.69 GB
/// sidecar went stale seconds after the compaction that produced it, because
/// the sweep's own writes sealed another segment. §5.4 as specified is then
/// correct and useless — it delivers its saving only for a server that compacts
/// and is never written to again.
///
/// The trap in fixing it is the OVERLAY. A table repaired since its base was
/// built holds the corrections there, and persisting the base alone would write
/// rows that have since moved — a wrong answer with a valid checksum. So this
/// fixture deliberately REPAIRS a table before re-stamping, then asserts the
/// adopted base answers what the live table answers.
#[test]
fn a_restamped_sidecar_folds_in_the_overlay() {
    let dir = TmpDir::new("restamp");
    let (store, _truth) = compacted_with_sidecar(dir.path());

    // Writes AFTER the compaction: they seal, which invalidates the
    // compaction's sidecar, and they leave the warmed tables stale so the next
    // read REPAIRS them — putting rows in the overlay, the case under test.
    let g = graph_on(&store);
    warm(&g);
    for i in 100..110i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})"));
        stmt(
            &g,
            &format!("MATCH (a:P {{id: 0}}), (b:P {{id: {i}}}) CREATE (a)-[:R]->(b)"),
        );
    }
    store.seal();
    warm(&g); // repairs the stale tables from the change log
    // AND the maintenance pass, because a READ no longer guarantees a
    // republish. Since §8 a single-node reader whose node did not move is
    // served from the stale table and repairs nothing — that is the point of
    // the change — so `warm` alone can leave a published base short of its
    // epoch, and `persist_derived_now` (correctly) refuses to write a stale
    // one. The server has never relied on a read for this either: it persists
    // from the maintenance thread, which is also what runs the refresh, and on
    // a settled store there are no reads to rely on. So this is the fixture
    // catching up with the caller it was always standing in for.
    let _ = g.refresh_stale_derived();
    let live = answers(&g);
    let parts_before = g
        .adj_table_parts_for_test(b'O', &None)
        .expect("O/any is published");
    assert!(
        !parts_before.overlay.is_empty(),
        "the fixture must leave rows in the OVERLAY, or the fold this test          exists to check is never exercised"
    );

    assert!(
        g.persist_derived_now(dir.path(), 0),
        "a quiescent graph whose bases are current must re-stamp"
    );

    let g2 = graph_on(&store);
    let adopted = g2.adopt_derived_sidecar(dir.path());
    assert!(adopted > 0, "the re-stamped sidecar must be adopted");
    let parts_after = g2
        .adj_table_parts_for_test(b'O', &None)
        .expect("O/any adopted");
    assert!(
        parts_after.overlay.is_empty(),
        "an adopted base is a PURE base — the overlay was folded into it"
    );
    assert_eq!(
        answers(&g2),
        live,
        "and it must answer exactly what the repaired table answered: a fold          that dropped the overlay would answer with the rows as they were          BEFORE the repair, silently"
    );
}

/// AN UNCHANGED SEALED SET does not get rewritten.
///
/// The sidecar is 1.69 GB at SF1 and a quiescent tick recurs, so without this
/// an idle server writes the whole CSR to disk over and over to produce a file
/// identical in the only way that matters. Asserted as a PAIR — the first write
/// must happen, or "the second one is skipped" is true of a mechanism that
/// never wrote anything.
#[test]
fn an_unchanged_sealed_set_is_not_rewritten() {
    let dir = TmpDir::new("norewrite");
    let (store, _truth) = compacted_with_sidecar(dir.path());
    // A fresh graph has written nothing, so its first pass does.
    let g = graph_on(&store);
    warm(&g);
    assert!(
        g.persist_derived_now(dir.path(), 0),
        "the first re-stamp must write, or the skip below proves nothing"
    );
    assert!(
        !g.persist_derived_now(dir.path(), 0),
        "a second pass over an unchanged sealed set must skip the write"
    );
    // A SEAL moves the sealed set, so the next pass must write again — the
    // skip is keyed on the vintage, not a one-shot latch.
    stmt(&g, "CREATE (:P {id: 700})");
    store.seal();
    warm(&g);
    assert!(
        g.persist_derived_now(dir.path(), 0),
        "once the sealed set moves, the sidecar it described is stale and must          be rewritten — a skip that never re-arms would silently stop          persisting for the life of the process"
    );
}

/// BASES PUBLISHED AFTER THE FILE WAS WRITTEN are persisted by the next tick,
/// on an UNCHANGED sealed set.
///
/// The production mirror's first quiescent tick fired while the 74 s warm was
/// still walking the span: it wrote a sidecar holding one membership, recorded
/// the sealed set as persisted, and — a read-only mirror never sealing again —
/// skipped every tick for the life of the process. Every start rebuilt 318
/// tables, and the file on disk existed only to say so. The skip is keyed on
/// the vintage of WHAT THE FILE HOLDS, not the sealed set alone.
#[test]
fn bases_published_after_the_write_are_persisted_by_the_next_tick() {
    let dir = TmpDir::new("latepublish");
    let (store, truth) = compacted_with_sidecar(dir.path());
    // ── With the DEFAULT growth interval, growth on an unchanged sealed set
    //    is deferred, not written on the very next tick: v84 rewrote the 1.3 GB
    //    production file nine times in four minutes, once per membership the
    //    shadow traffic built.
    {
        let g = graph_on(&store);
        let _ = g.members(Some("P")).expect("members P");
        assert!(g.persist_derived_now(dir.path(), 0), "first write");
        warm(&g);
        assert!(
            !g.persist_derived_now(dir.path(), 0),
            "growth inside the default rewrite interval must be DEFERRED — a tick per          new base is a full rewrite per new base"
        );
        assert!(
            !g.persist_derived_now(dir.path(), 599),
            "still deferred one second inside the interval"
        );
        assert!(
            g.persist_derived_now(dir.path(), 600),
            "and written once the interval has elapsed — deferred is not never"
        );
    }
    let g = graph_on(&store);
    // Every tick may write for the rest of this test: the claim under test is
    // WHAT is persisted, not how often.
    g.set_persist_growth_interval(0);
    // The early tick: only a membership is published when it fires.
    let _ = g.members(Some("P")).expect("members P");
    assert!(
        g.persist_derived_now(dir.path(), 0),
        "the early tick must write, or the rewrite below proves nothing"
    );
    assert!(
        !g.persist_derived_now(dir.path(), 0),
        "and a tick with nothing new published must skip"
    );
    // The warm finishes: adjacency tables are published on the same sealed set.
    warm(&g);
    assert!(
        g.persist_derived_now(dir.path(), 0),
        "a tick after MORE bases were published must write the fuller file —          keyed on the sealed set alone it never did, and every start rebuilt          what the process had already built"
    );
    assert!(
        !g.persist_derived_now(dir.path(), 0),
        "and then skip again until something else is published"
    );
    // The restart adopts the warmed tables, not just the membership.
    let g2 = graph_on(&store);
    let adopted = g2.adopt_derived_sidecar(dir.path());
    let (_, trace) = engram_observe::with_trace(|| warm(&g2));
    let built = trace
        .counters()
        .get("graph.adjacency tables built")
        .copied()
        .unwrap_or(0);
    eprintln!("[persist derived] late publish: restart adopted {adopted}, built {built} during the warm");
    assert!(
        adopted > 1,
        "the restart must adopt the adjacency tables the second write persisted, not the          membership alone: adopted {adopted}"
    );
    assert_eq!(built, 0, "every table the warm wants was adopted");
    assert_eq!(answers(&g2), truth, "and answers exactly what the writer answered");
}

/// THE COMPACTION'S OWN WRITE records its vintage, so the very next quiescent
/// tick does not rewrite the file it just produced.
///
/// `persisted_sealed_id` is per-`Graph`, and the emit and the tick run on the
/// SAME graph in a server — so this has to be asserted on that graph and not on
/// a fresh one, which legitimately knows nothing about a file on disk.
#[test]
fn the_compactions_own_write_records_its_vintage() {
    let dir = TmpDir::new("emitvintage");
    let store = Store::new();
    let g = graph_on(&store);
    build(&g, &store);
    warm(&g);
    for i in 24..32i64 {
        stmt(&g, &format!("CREATE (:P {{id: {i}}})"));
    }
    store.seal();
    let cache = store.into_paged(dir.path(), 8 << 20).expect("into_paged");
    let list = [std::sync::Arc::clone(&g)];
    engram_graph::compact_paged_emitting(&list, &store, dir.path(), &cache).expect("compaction");
    assert_eq!(
        sidecar_files(dir.path()).len(),
        1,
        "the compaction must have written a sidecar"
    );
    warm(&g);
    assert!(
        !g.persist_derived_now(dir.path(), 0),
        "the emit already wrote this vintage — a tick that rewrites it burns          1.69 GB at SF1 to produce a file identical in the only way that matters"
    );
}

/// A base behind the clock is NOT re-stamped.
///
/// Writing one would persist a subset and call it complete — and at boot there
/// is no change log to carry the difference, so nothing could catch it up.
#[test]
fn a_base_behind_the_clock_is_not_restamped() {
    let dir = TmpDir::new("behind");
    let (store, _truth) = compacted_with_sidecar(dir.path());
    let g = graph_on(&store);
    warm(&g);
    // A write with NO subsequent read: the tables are now behind the clock and
    // nothing has repaired them.
    stmt(&g, "CREATE (:P {id: 500})");
    assert!(
        !g.persist_derived_now(dir.path(), 0),
        "a graph whose published bases are behind the store's clock must          DECLINE to persist them"
    );
}

/// ANOTHER GRAPH'S SIDECAR is not this graph's. Two coordinates share a data
/// directory, and a shared file would let one adopt the other's rows and pass
/// every check it has.
#[test]
fn one_graphs_sidecar_is_not_anothers() {
    let dir = TmpDir::new("tenant");
    let (store, _truth) = compacted_with_sidecar(dir.path());
    assert_eq!(sidecar_files(dir.path()).len(), 1);

    // A DIFFERENT coordinate over the same store.
    let other = std::sync::Arc::new(Graph::new(store.clone(), Realm(2), Namespace(7)));
    assert_eq!(
        other.adopt_derived_sidecar(dir.path()),
        0,
        "a graph must never adopt a sidecar written by another coordinate"
    );
}
