//! REVIEW HAMMER for the write fence (`derived.rs`, "The write fence"):
//! four writers (direct writes, transaction batches, relationship deletes and
//! detach-deletes of messages), three readers with linearisability bounds
//! on every read, and the maintenance refresh at maximum cadence — ~200k row
//! writes — and at the end EVERY node is checked, not a sample:
//!
//! - every person's settled IN row (table path) equals its acknowledged live
//!   count AND the direct walk's row, rel id for rel id;
//! - every message's settled OUT row (table path) equals the direct walk's;
//! - the settled `:Message` and all-nodes memberships equal the store's own
//!   walk id for id;
//! - `DERIVED_LOG_POISONED` stayed at zero (the fail-closed path is meant to
//!   be unreachable under the fence) and the fence counter actually fired
//!   (the mechanism under test was exercised, not bypassed).
//!
//! Two arms: creates only with transaction batches, and creates + deletes.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use engram_cypher::Value;
use engram_graph::{Dir, Graph, GraphError, SlimAdj};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PERSONS: usize = 64;
/// An exact multiple of `PERSONS`, so the per-person seed count is exact.
const SEED_MESSAGES: u64 = 2_048;
const WRITERS: usize = 4;
/// Ops per writer; each op is a node and a relationship (two row writes at
/// least), so four writers × 25k ops is ~200k row writes before deletes.
const OPS_PER_WRITER: u64 = 25_000;
const READERS: usize = 3;

/// Per-person bookkeeping that stays a valid bound under deletes:
/// `done_add - intent_del <= row <= intent_add - done_del`.
struct Book {
    intent_add: Vec<AtomicU64>,
    done_add: Vec<AtomicU64>,
    intent_del: Vec<AtomicU64>,
    done_del: Vec<AtomicU64>,
    nodes_intent_add: AtomicU64,
    nodes_done_add: AtomicU64,
    nodes_intent_del: AtomicU64,
    nodes_done_del: AtomicU64,
}

impl Book {
    fn new(seed_per_person: u64) -> Book {
        Book {
            intent_add: (0..PERSONS).map(|_| AtomicU64::new(seed_per_person)).collect(),
            done_add: (0..PERSONS).map(|_| AtomicU64::new(seed_per_person)).collect(),
            intent_del: (0..PERSONS).map(|_| AtomicU64::new(0)).collect(),
            done_del: (0..PERSONS).map(|_| AtomicU64::new(0)).collect(),
            nodes_intent_add: AtomicU64::new(SEED_MESSAGES),
            nodes_done_add: AtomicU64::new(SEED_MESSAGES),
            nodes_intent_del: AtomicU64::new(0),
            nodes_done_del: AtomicU64::new(0),
        }
    }
    /// The counters a bound needs BEFORE the read: adds done and deletes
    /// done by then are certainly reflected in the row.
    fn person_before(&self, p: usize) -> (u64, u64) {
        (self.done_add[p].load(Ordering::SeqCst), self.done_del[p].load(Ordering::SeqCst))
    }
    /// The bound, closed AFTER the read: at every instant of the read the
    /// row held at least `done_add(before) - intent_del(after)` and at most
    /// `intent_add(after) - done_del(before)`.
    fn person_bounds(&self, p: usize, before: (u64, u64)) -> (u64, u64) {
        let lo = before.0.saturating_sub(self.intent_del[p].load(Ordering::SeqCst));
        let hi = self.intent_add[p].load(Ordering::SeqCst) - before.1;
        (lo, hi)
    }
    fn nodes_before(&self) -> (u64, u64) {
        (self.nodes_done_add.load(Ordering::SeqCst), self.nodes_done_del.load(Ordering::SeqCst))
    }
    fn nodes_bounds(&self, before: (u64, u64)) -> (u64, u64) {
        let lo = before.0.saturating_sub(self.nodes_intent_del.load(Ordering::SeqCst));
        let hi = self.nodes_intent_add.load(Ordering::SeqCst) - before.1;
        (lo, hi)
    }
}

fn setup() -> (Arc<Graph>, Vec<u64>, u32, u64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let person = vec!["Person".to_string()];
    let message = vec!["Message".to_string()];
    let none = BTreeMap::new();
    let persons: Vec<u64> = (0..PERSONS)
        .map(|i| {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(i as i64));
            g.create_node(&person, &p).expect("person")
        })
        .collect();
    let mut probe_message = 0;
    for i in 0..SEED_MESSAGES {
        let m = g.create_node(&message, &none).expect("message");
        if i == 0 {
            probe_message = m;
        }
        g.create_rel(m, "HAS_CREATOR", persons[(i as usize) % PERSONS], &none)
            .expect("has_creator");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["HAS_CREATOR".to_string()]).expect("minted")[0];
    for &p in &persons {
        let _ = g.adjacent_slim(p, Dir::In, &Some(vec![tok]));
    }
    let _ = g.adjacent_slim(probe_message, Dir::Out, &Some(vec![tok]));
    let _ = g.members(Some("Message")).expect("members");
    let _ = g.members(None).expect("all");
    (Arc::new(g), persons, tok, probe_message)
}

fn key(v: &[SlimAdj]) -> Vec<(u64, u64)> {
    let mut s: Vec<(u64, u64)> = v.iter().map(|e| (e.peer, e.rel)).collect();
    s.sort_unstable();
    s
}

fn hammer(deletes: bool, txn_batches: bool) {
    let (g, persons, tok, probe_message) = setup();
    let book = Arc::new(Book::new(SEED_MESSAGES / PERSONS as u64));
    let stop = Arc::new(AtomicBool::new(false));
    let poisoned_before = engram_graph::counters::DERIVED_LOG_POISONED.load(Ordering::Relaxed);

    // Maintenance at maximum cadence, traced so the fence counter is visible.
    let maint = {
        let (g, stop) = (Arc::clone(&g), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut runs = 0u64;
            let (_, trace) = engram_observe::with_trace(|| {
                while !stop.load(Ordering::Relaxed) {
                    let _ = g.refresh_stale_derived();
                    runs += 1;
                    std::thread::yield_now();
                }
            });
            (runs, trace)
        })
    };

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (g, book, persons) = (Arc::clone(&g), Arc::clone(&book), persons.clone());
            std::thread::spawn(move || {
                let message = vec!["Message".to_string()];
                let none = BTreeMap::new();
                // This writer's own live `(message, person, rel)` triples.
                let mut mine: Vec<(u64, usize, u64)> = Vec::new();
                // A seed MIXER, so the multiply is meant to wrap: for `w >= 2`
                // the plain `*` overflows u64 and, in a debug build, panics
                // the writer thread before it writes anything.
                let mut x = 0xA5A5_5A5A_1234_5678u64 ^ (w as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let mut i = 0u64;
                let (_, trace) = engram_observe::with_trace(|| {
                    while i < OPS_PER_WRITER {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        // Deletes: every 8th op a relationship, every 16th a
                        // detach-delete of a message (which deletes its rel).
                        if deletes && i % 8 == 4 && !mine.is_empty() {
                            let j = (x % mine.len() as u64) as usize;
                            let (m, p, r) = mine.swap_remove(j);
                            let detach = i % 16 == 12;
                            book.intent_del[p].fetch_add(1, Ordering::SeqCst);
                            if detach {
                                book.nodes_intent_del.fetch_add(1, Ordering::SeqCst);
                                g.delete_node(m, true).expect("detach delete");
                                book.nodes_done_del.fetch_add(1, Ordering::SeqCst);
                            } else {
                                g.delete_rel(r).expect("delete rel");
                            }
                            book.done_del[p].fetch_add(1, Ordering::SeqCst);
                            i += 1;
                            continue;
                        }
                        let batch = if txn_batches && i % 16 == 0 { 8 } else { 1 };
                        let targets: Vec<usize> = (0..batch)
                            .map(|k| ((i + k) as usize * 7 + w * 13 + (x as usize % 3)) % PERSONS)
                            .collect();
                        for &p in &targets {
                            book.intent_add[p].fetch_add(1, Ordering::SeqCst);
                        }
                        book.nodes_intent_add.fetch_add(batch, Ordering::SeqCst);
                        let mut made: Vec<(u64, usize, u64)> = Vec::with_capacity(batch as usize);
                        loop {
                            made.clear();
                            if batch > 1 {
                                g.begin_txn().expect("begin");
                            }
                            for &p in &targets {
                                let m = g.create_node(&message, &none).expect("message");
                                let r = g.create_rel(m, "HAS_CREATOR", persons[p], &none).expect("rel");
                                made.push((m, p, r));
                            }
                            if batch > 1 {
                                match g.commit_txn() {
                                    Ok(()) => break,
                                    Err(GraphError::TxnConflict) => continue,
                                    Err(e) => panic!("commit: {e:?}"),
                                }
                            }
                            break;
                        }
                        for &p in &targets {
                            book.done_add[p].fetch_add(1, Ordering::SeqCst);
                        }
                        book.nodes_done_add.fetch_add(batch, Ordering::SeqCst);
                        mine.append(&mut made);
                        i += batch;
                    }
                });
                (mine, trace)
            })
        })
        .collect();

    let readers: Vec<_> = (0..READERS)
        .map(|r| {
            let (g, book, persons, stop) = (Arc::clone(&g), Arc::clone(&book), persons.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut reads = 0u64;
                let mut violations = 0u64;
                let mut first: Option<String> = None;
                let mut x = 0x9E37_79B9u64.wrapping_add(r as u64);
                let (_, trace) = engram_observe::with_trace(|| {
                    while !stop.load(Ordering::Relaxed) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let p = (x % PERSONS as u64) as usize;
                        let before = book.person_before(p);
                        let got = g.adjacent_slim(persons[p], Dir::In, &Some(vec![tok])).len() as u64;
                        let (lo, hi) = book.person_bounds(p, before);
                        if !(lo <= got && got <= hi) {
                            violations += 1;
                            first.get_or_insert_with(|| format!("person {p} in-row {got} outside [{lo}, {hi}]"));
                        }
                        let before = book.nodes_before();
                        let got = g.members(Some("Message")).expect("members").len() as u64;
                        let (lo, hi) = book.nodes_bounds(before);
                        if !(lo <= got && got <= hi) {
                            violations += 1;
                            first.get_or_insert_with(|| format!(":Message membership {got} outside [{lo}, {hi}]"));
                        }
                        // Keep the OUT table alive and racing too.
                        let _ = g.adjacent_slim(probe_message, Dir::Out, &Some(vec![tok]));
                        reads += 1;
                    }
                });
                (reads, violations, first, trace)
            })
        })
        .collect();

    let mut all_mine: Vec<(u64, usize, u64)> = Vec::new();
    let mut txn_replays = 0u64;
    for w in writers {
        let (mine, trace) = w.join().expect("writer");
        all_mine.extend(mine);
        txn_replays += trace
            .counters()
            .get("graph.transaction replayed its changes at commit")
            .copied()
            .unwrap_or(0);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    let (runs, maint_trace) = maint.join().expect("maintenance");
    let mut reads = 0u64;
    let mut violations = 0u64;
    let mut table_path = 0u64;
    let mut direct = 0u64;
    let mut fenced = maint_trace
        .counters()
        .get("graph.publish stamp fenced below an in-flight writer")
        .copied()
        .unwrap_or(0);
    for r in readers {
        let (n, v, first, trace) = r.join().expect("reader");
        if let Some(f) = first {
            eprintln!("[hammer deletes={deletes} txn={txn_batches}] first violation: {f}");
        }
        reads += n;
        violations += v;
        let c = trace.counters();
        table_path += c.get("graph.adjacency tables reused").copied().unwrap_or(0)
            + c.get("graph.adjacency tables repaired").copied().unwrap_or(0)
            + c.get("graph.adjacency tables built").copied().unwrap_or(0)
            + c.get("graph.adjacency tables built by another worker").copied().unwrap_or(0);
        direct += c.get("graph.adjacency table declined by the entry budget").copied().unwrap_or(0);
        fenced += c.get("graph.publish stamp fenced below an in-flight writer").copied().unwrap_or(0);
    }
    let poisoned = engram_graph::counters::DERIVED_LOG_POISONED.load(Ordering::Relaxed) - poisoned_before;
    eprintln!(
        "[hammer deletes={deletes} txn={txn_batches}] reads={reads} table_path={table_path} direct={direct} \
         violations={violations} refresh_runs={runs} fenced={fenced} txn_replays={txn_replays} poisoned={poisoned} \
         live_rels={}",
        all_mine.len()
    );

    // ── SETTLED, EVERY NODE ─────────────────────────────────────────────
    // Persons: table row == acknowledged live count == direct walk, id for id.
    let mut live_per_person = vec![SEED_MESSAGES / PERSONS as u64; PERSONS];
    for &(_, p, _) in &all_mine {
        live_per_person[p] += 1;
    }
    let mut person_rows: Vec<Vec<SlimAdj>> = Vec::with_capacity(PERSONS);
    for &p in &persons {
        person_rows.push(g.adjacent_slim(p, Dir::In, &Some(vec![tok])));
    }
    let messages: Vec<u64> = g.members(Some("Message")).expect("members").iter().collect();
    let mut message_rows: Vec<Vec<SlimAdj>> = Vec::with_capacity(messages.len());
    for &m in &messages {
        message_rows.push(g.adjacent_slim(m, Dir::Out, &Some(vec![tok])));
    }
    // Memberships against the store walk.
    for label in [Some("Message"), None] {
        let snapshot: Vec<u64> = g.members(label).expect("members").iter().collect();
        let mut walked = g.nodes_by_label(label).expect("walk");
        walked.sort_unstable();
        assert_eq!(
            snapshot.len(),
            walked.len(),
            "[deletes={deletes} txn={txn_batches}] settled {label:?} membership {} vs store walk {}",
            snapshot.len(),
            walked.len()
        );
        assert_eq!(snapshot, walked, "settled {label:?} membership differs from the store walk id for id");
    }
    let (nlo, nhi) = book.nodes_bounds(book.nodes_before());
    assert_eq!(nlo, nhi, "bookkeeping did not settle");
    assert_eq!(messages.len() as u64, nlo, "settled :Message count != acknowledged");
    // Now the direct walk (tables declined) against every table row.
    g.set_adj_table_max_entries(0);
    let mut short_persons = Vec::new();
    for (i, &p) in persons.iter().enumerate() {
        let direct = g.adjacent_slim(p, Dir::In, &Some(vec![tok]));
        let (lo, hi) = book.person_bounds(i, book.person_before(i));
        assert_eq!(lo, hi, "person {i} bookkeeping did not settle");
        assert_eq!(direct.len() as u64, lo, "the STORE is short for person {i} — the writer, not the table");
        assert_eq!(direct.len() as u64, live_per_person[i], "writer-side live count disagrees with the store for person {i}");
        if key(&person_rows[i]) != key(&direct) {
            short_persons.push((i, person_rows[i].len(), direct.len()));
        }
    }
    assert!(
        short_persons.is_empty(),
        "[deletes={deletes} txn={txn_batches}] settled IN tables differ from the store (person, table, store): {short_persons:?}"
    );
    let mut bad_messages = 0usize;
    for (i, &m) in messages.iter().enumerate() {
        let direct = g.adjacent_slim(m, Dir::Out, &Some(vec![tok]));
        if key(&message_rows[i]) != key(&direct) {
            bad_messages += 1;
        }
    }
    assert_eq!(bad_messages, 0, "settled OUT tables differ from the store for {bad_messages} messages");
    // The instrument: enough racing, the table path taken, the fence exercised,
    // no poison, no transient violation.
    assert!(reads >= 500, "too few reads to have raced anything: {reads}");
    assert!(table_path >= reads / 2, "readers did not take the table path");
    assert_eq!(direct, 0, "a reader fell back to the direct walk");
    assert!(fenced > 0, "the fence never clamped a publish — the mechanism was not exercised");
    if txn_batches {
        assert!(txn_replays > 0, "no transaction ever committed");
    }
    assert_eq!(poisoned, 0, "the change log was POISONED {poisoned} time(s) — the fence has a hole");
    assert_eq!(violations, 0, "reads outside their acknowledged/attempted bounds");
}

#[test]
fn four_writers_txn_batches_maintenance_every_node_settles_to_the_store() {
    hammer(false, true);
}

#[test]
fn four_writers_with_deletes_and_detach_deletes_every_node_settles_to_the_store() {
    hammer(true, true);
}
