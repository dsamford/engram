//! The maintenance refresh RACING readers and writers on one shared graph:
//! a maintenance thread loops `refresh_stale_derived`, writers create
//! messages and their `HAS_CREATOR` (direct and transaction-batched), and
//! readers read in-rows and memberships continuously. Every read must be
//! LINEARIZABLE against the writers' bookkeeping — a row observed at any
//! instant holds at least every relationship acknowledged before the read
//! began and at most every one attempted before it ended. A half-published
//! table, a regressed slot, or a repair that missed a row all violate that.
//!
//! Two arms: with the maintenance thread (the change under review) and
//! without it (readers alone repair — the pre-existing path), so a failure
//! is ATTRIBUTED. Each arm reports transient violations (a read outside its
//! bounds) and DURABLE loss (the settled table, after every thread stopped,
//! still short of the acknowledged count), and ends with the STORE-LEVEL
//! differential: the settled membership snapshots (`members`) must equal
//! the store's own membership walk (`nodes_by_label`) id for id, and the
//! settled adjacency tables the direct walk with tables declined.
//!
//! This is the file that found the LOST-WRITE race the write fence in
//! `derived.rs` closes: 2 writers + maintenance, 4,847 adjacency and 431,164
//! membership violations and a settled `:Message` membership of 15,976
//! against 16,000 acknowledged, while the store walk had every row. The
//! arms are permanent; their assertions are the gate.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use engram_cypher::Value;
use engram_graph::{Dir, Graph, GraphError};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PERSONS: usize = 40;
const SEED_MESSAGES: u64 = 4_000;
const WRITES_PER_WRITER: u64 = 6_000;
const WRITERS: usize = 2;
const READERS: usize = 3;

struct Counters {
    intent: Vec<AtomicU64>,
    done: Vec<AtomicU64>,
    nodes_intent: AtomicU64,
    nodes_done: AtomicU64,
}

fn setup() -> (Arc<Graph>, Vec<u64>, u32) {
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
    for i in 0..SEED_MESSAGES {
        let m = g.create_node(&message, &none).expect("message");
        g.create_rel(m, "HAS_CREATOR", persons[(i as usize) % PERSONS], &none)
            .expect("has_creator");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["HAS_CREATOR".to_string()]).expect("minted")[0];
    // Build what the race is over.
    for &p in &persons {
        let _ = g.adjacent_slim(p, Dir::In, &Some(vec![tok]));
    }
    let _ = g.members(Some("Message")).expect("members");
    let _ = g.members(None).expect("all");
    (Arc::new(g), persons, tok)
}

struct Outcome {
    reads: u64,
    table_path: u64,
    direct: u64,
    adj_violations: u64,
    members_violations: u64,
    /// `(person, settled in-row, acknowledged)` for every person whose
    /// settled table is short — DURABLE loss.
    durable_adj_loss: Vec<(usize, u64, u64)>,
    durable_members_loss: Option<(u64, u64)>,
    refresh_runs: u64,
    refresh_work: u64,
    txn_batches: bool,
}

fn race(maintenance: bool, txn_batches: bool, writers_n: usize) -> Outcome {
    let (g, persons, tok) = setup();
    let seed_per_person = (SEED_MESSAGES as usize / PERSONS) as u64;
    let c = Arc::new(Counters {
        intent: (0..PERSONS).map(|_| AtomicU64::new(seed_per_person)).collect(),
        done: (0..PERSONS).map(|_| AtomicU64::new(seed_per_person)).collect(),
        nodes_intent: AtomicU64::new(SEED_MESSAGES),
        nodes_done: AtomicU64::new(SEED_MESSAGES),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let refresh_runs = Arc::new(AtomicU64::new(0));
    let refresh_work = Arc::new(AtomicU64::new(0));

    // The maintenance thread: the server's loop, at maximum cadence.
    let maint = maintenance.then(|| {
        let (g, stop, runs, work) = (
            Arc::clone(&g),
            Arc::clone(&stop),
            Arc::clone(&refresh_runs),
            Arc::clone(&refresh_work),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let r = g.refresh_stale_derived();
                runs.fetch_add(1, Ordering::Relaxed);
                work.fetch_add(
                    (r.adjacency_repaired + r.adjacency_rebuilt + r.members_caught_up + r.members_rebuilt)
                        as u64,
                    Ordering::Relaxed,
                );
                std::thread::yield_now();
            }
        })
    });

    // Writers: intent BEFORE the write, done AFTER it returns. A transaction
    // that loses OCC validation is retried (its intent stands, so the bounds
    // stay valid throughout).
    let writers: Vec<_> = (0..writers_n)
        .map(|w| {
            let (g, c, persons) = (Arc::clone(&g), Arc::clone(&c), persons.clone());
            std::thread::spawn(move || {
                let message = vec!["Message".to_string()];
                let none = BTreeMap::new();
                let mut i = 0u64;
                let quota = WRITES_PER_WRITER * WRITERS as u64 / writers_n as u64;
                while i < quota {
                    let batch = if txn_batches && i % 16 == 0 { 8 } else { 1 };
                    let targets: Vec<usize> = (0..batch)
                        .map(|k| ((i + k) as usize * 7 + w * 13) % PERSONS)
                        .collect();
                    for &p in &targets {
                        c.intent[p].fetch_add(1, Ordering::SeqCst);
                    }
                    c.nodes_intent.fetch_add(batch, Ordering::SeqCst);
                    loop {
                        if batch > 1 {
                            g.begin_txn().expect("begin");
                        }
                        for &p in &targets {
                            let m = g.create_node(&message, &none).expect("message");
                            g.create_rel(m, "HAS_CREATOR", persons[p], &none).expect("rel");
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
                        c.done[p].fetch_add(1, Ordering::SeqCst);
                    }
                    c.nodes_done.fetch_add(batch, Ordering::SeqCst);
                    i += batch;
                }
            })
        })
        .collect();

    // Readers: bounds checked on every read (violations counted, not
    // panicked, so the settled state below is still observed); their own
    // traces prove they took the table path, not a direct walk.
    let readers: Vec<_> = (0..READERS)
        .map(|r| {
            let (g, c, persons, stop) = (Arc::clone(&g), Arc::clone(&c), persons.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut reads = 0u64;
                let mut adj_v = 0u64;
                let mut mem_v = 0u64;
                let mut first: Option<String> = None;
                let mut x = 0x9E37_79B9u64.wrapping_add(r as u64);
                let (_, trace) = engram_observe::with_trace(|| {
                    while !stop.load(Ordering::Relaxed) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let p = (x % PERSONS as u64) as usize;
                        let lo = c.done[p].load(Ordering::SeqCst);
                        let got = g.adjacent_slim(persons[p], Dir::In, &Some(vec![tok])).len() as u64;
                        let hi = c.intent[p].load(Ordering::SeqCst);
                        if !(lo <= got && got <= hi) {
                            adj_v += 1;
                            first.get_or_insert_with(|| format!("person {p} in-row {got} outside [{lo}, {hi}]"));
                        }
                        let lo = c.nodes_done.load(Ordering::SeqCst);
                        let got = g.members(Some("Message")).expect("members").len() as u64;
                        let hi = c.nodes_intent.load(Ordering::SeqCst);
                        if !(lo <= got && got <= hi) {
                            mem_v += 1;
                            first.get_or_insert_with(|| format!(":Message membership {got} outside [{lo}, {hi}]"));
                        }
                        reads += 1;
                    }
                });
                (reads, adj_v, mem_v, first, trace)
            })
        })
        .collect();

    for w in writers {
        w.join().expect("writer");
    }
    // Let the readers see the settled state a while, then stop everything.
    std::thread::sleep(std::time::Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    if let Some(m) = maint {
        m.join().expect("maintenance");
    }
    let mut out = Outcome {
        reads: 0,
        table_path: 0,
        direct: 0,
        adj_violations: 0,
        members_violations: 0,
        durable_adj_loss: Vec::new(),
        durable_members_loss: None,
        refresh_runs: refresh_runs.load(Ordering::Relaxed),
        refresh_work: refresh_work.load(Ordering::Relaxed),
        txn_batches,
    };
    for r in readers {
        let (reads, adj_v, mem_v, first, trace) = r.join().expect("reader");
        if let Some(f) = first {
            eprintln!("[concurrent maint={maintenance} txn={txn_batches} writers={writers_n}] first violation: {f}");
        }
        out.reads += reads;
        out.adj_violations += adj_v;
        out.members_violations += mem_v;
        let cnt = trace.counters();
        out.table_path += cnt.get("graph.adjacency tables reused").copied().unwrap_or(0)
            + cnt.get("graph.adjacency tables repaired").copied().unwrap_or(0)
            + cnt.get("graph.adjacency tables built").copied().unwrap_or(0)
            + cnt.get("graph.adjacency tables built by another worker").copied().unwrap_or(0);
        out.direct += cnt.get("graph.adjacency table declined by the entry budget").copied().unwrap_or(0);
    }
    // The SETTLED state: every writer returned, nothing else is running. A
    // table still short here is a row the repair lost for good.
    for (p, &id) in persons.iter().enumerate() {
        let got = g.adjacent_slim(id, Dir::In, &Some(vec![tok])).len() as u64;
        let want = c.done[p].load(Ordering::SeqCst);
        if got != want {
            out.durable_adj_loss.push((p, got, want));
        }
    }
    let got = g.members(Some("Message")).expect("members").len() as u64;
    let want = c.nodes_done.load(Ordering::SeqCst);
    if got != want {
        out.durable_members_loss = Some((got, want));
    }
    // THE STORE-LEVEL DIFFERENTIAL. The settled snapshots against the
    // store's own walk, id for id — not counts, which two compensating
    // faults could reconcile. A snapshot short of the walk is a row the
    // catch-up lost; a snapshot past it is a row that never committed.
    for label in [Some("Message"), None] {
        let snapshot: Vec<u64> = g.members(label).expect("members").iter().collect();
        let mut walked = g.nodes_by_label(label).expect("walk");
        walked.sort_unstable();
        assert_eq!(
            snapshot.len(),
            walked.len(),
            "[maint={maintenance} txn={txn_batches} writers={writers_n}] settled {label:?} snapshot has {} ids, \
             the store walk {} — the snapshot {} the store",
            snapshot.len(),
            walked.len(),
            if snapshot.len() < walked.len() { "LOST rows the store has" } else { "holds rows the store does not" }
        );
        assert_eq!(snapshot, walked, "settled {label:?} snapshot and store walk differ id for id");
    }
    assert_eq!(
        g.nodes_by_label(Some("Message")).expect("walk").len() as u64,
        want,
        "the STORE is short of acknowledged nodes — the writer, not the snapshot"
    );
    // And the direct walk agrees with the bookkeeping — the store has the rows.
    g.set_adj_table_max_entries(0);
    for (p, &id) in persons.iter().enumerate() {
        assert_eq!(
            g.adjacent_slim(id, Dir::In, &Some(vec![tok])).len() as u64,
            c.done[p].load(Ordering::SeqCst),
            "the STORE is short for person {p} — the writer, not the table"
        );
    }
    eprintln!(
        "[concurrent maint={maintenance} txn={txn_batches} writers={writers_n}] reads={} table_path={} direct={} adj_violations={} \
         members_violations={} durable_adj_loss={:?} durable_members_loss={:?} refresh_runs={} refresh_work={}",
        out.reads,
        out.table_path,
        out.direct,
        out.adj_violations,
        out.members_violations,
        out.durable_adj_loss,
        out.durable_members_loss,
        out.refresh_runs,
        out.refresh_work
    );
    out
}

fn assert_clean(o: &Outcome, arm: &str) {
    assert!(o.reads >= 500, "{arm}: too few reads to have raced anything: {}", o.reads);
    assert!(o.table_path >= o.reads / 2, "{arm}: readers did not take the table path");
    assert_eq!(o.direct, 0, "{arm}: a reader fell back to the direct walk");
    assert!(
        o.durable_adj_loss.is_empty(),
        "{arm}: the settled adjacency table is SHORT of acknowledged rows (person, table, acked): {:?}",
        o.durable_adj_loss
    );
    assert!(
        o.durable_members_loss.is_none(),
        "{arm}: the settled :Message membership is short: {:?}",
        o.durable_members_loss
    );
    assert_eq!(
        (o.adj_violations, o.members_violations),
        (0, 0),
        "{arm}: reads outside their acknowledged/attempted bounds (adjacency, membership)"
    );
    let _ = o.txn_batches;
}

#[test]
fn with_the_maintenance_thread_readers_never_observe_a_torn_or_stale_table() {
    let o = race(true, true, WRITERS);
    assert!(o.refresh_runs >= 10, "the maintenance thread barely ran: {}", o.refresh_runs);
    assert!(o.refresh_work >= 5, "the maintenance thread brought nothing current while racing");
    assert_clean(&o, "maintenance+txn");
}

#[test]
fn with_the_maintenance_thread_and_direct_writes_only() {
    let o = race(true, false, WRITERS);
    assert!(o.refresh_work >= 5);
    assert_clean(&o, "maintenance, direct writes");
}

/// ONE writer, direct writes, no maintenance: no second writer can fold a
/// stamp, so any loss here is the reader's own catch-up racing the writer.
#[test]
fn single_writer_direct_writes_readers_alone() {
    let o = race(false, false, 1);
    assert_clean(&o, "single writer, no maintenance");
}

/// ONE writer, direct writes, WITH the maintenance thread.
#[test]
fn single_writer_direct_writes_with_maintenance() {
    let o = race(true, false, 1);
    assert_clean(&o, "single writer, maintenance");
}

/// The attribution arm: NO maintenance thread — only readers repair, the
/// path that existed before this change.
#[test]
fn without_the_maintenance_thread_readers_alone() {
    let o = race(false, true, WRITERS);
    assert_clean(&o, "no maintenance");
}
