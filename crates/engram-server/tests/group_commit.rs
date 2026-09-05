//! Group commit — the server half, under kill -9.
//!
//! The mechanism defers each write's fsync and pays one per batch of requests.
//! The property that makes that a durability feature rather than a durability
//! hole is that **no reply leaves before the batch's fsync** — not the write's
//! acknowledgement, and not a read that ran after the write in the same batch.
//! A crash test is the only honest instrument for that: with the server killed
//! mid-flight and restarted, everything any client was TOLD must be on disk.
//!
//! Two arms, same test: group commit on (the default) and off
//! (`--no-group-commit`). The off arm is the control — the contract every
//! earlier durability test was written against, still holding.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use engram_bolt::client::Client;

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-group-commit-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn server_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("current exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.push(if cfg!(windows) {
        "engram-server.exe"
    } else {
        "engram-server"
    });
    p
}

/// Start a durable server, waiting until it accepts. `None` = binary not built.
fn start(dir: &Path, port: u16, extra: &[&str]) -> Option<Child> {
    let bin = server_bin();
    if !bin.exists() {
        eprintln!(
            "SKIP: {} not built. Run `cargo build -p engram-server` first for this lane to mean \
             anything.",
            bin.display()
        );
        return None;
    }
    // The operator step after a kill -9 — see `durability.rs` for why this is
    // legitimate rather than a way around the lock.
    let _ = std::fs::remove_file(dir.join(engram_store::dirlock::LOCK_FILE));
    let mut child = Command::new(&bin)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--data-dir")
        .arg(dir)
        .args(extra)
        .spawn()
        .expect("spawn server");
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Some(child);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not accept connections on {port}");
}

fn hard_kill(mut c: Child) {
    let _ = c.kill();
    let _ = c.wait();
    std::thread::sleep(Duration::from_millis(200));
}

/// Block until at least `min` writes have been acknowledged, or `cap` elapses.
///
/// LOAD-driven, not time-driven. A fixed sleep assumes a write rate, and the
/// per-write-fsync arm on a slow-fsync platform (Windows) produced fewer than
/// the required acknowledgements in 1.5 s — the control arm then failed for
/// "not enough load", which is the test's assumption failing, not durability.
/// Waiting for the count makes the kill land inside a known amount of work on
/// every platform.
fn wait_for_load(acks: &AtomicU64, min: u64, cap: Duration) {
    let t = std::time::Instant::now();
    while acks.load(Ordering::Relaxed) < min && t.elapsed() < cap {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What one writer thread saw: how many of its writes were ACKNOWLEDGED before
/// the server died.
fn writer(port: u16, cid: u64, stop: Arc<AtomicBool>, acks: Arc<AtomicU64>) -> u64 {
    let Ok(mut c) = Client::connect(format!("127.0.0.1:{port}")) else {
        return 0;
    };
    let mut acked = 0u64;
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match c.run(&format!("CREATE (:GC {{c: {cid}, i: {i}}})")) {
            Ok(_) => {
                acked += 1;
                acks.fetch_add(1, Ordering::Relaxed);
            }
            // The server is gone (or going). Anything after this point was
            // never acknowledged, so it makes no promise.
            Err(_) => break,
        }
        i += 1;
    }
    acked
}

const WRITERS: u64 = 8;

/// The core property, in either arm: every write a client was told succeeded
/// is present after a kill -9 landed while writers were running.
///
/// Recovered must lie in `[acked, acked + WRITERS]`: at least everything
/// acknowledged, at most that plus one in-flight write per client (sent,
/// possibly appended and fsynced, reply lost with the process). Anything below
/// `acked` is a lost acknowledged write — the failure group commit could
/// introduce if it released a reply before the batch's fsync. Anything above
/// the ceiling would mean writes nobody sent.
fn acked_writes_survive_a_kill_under_concurrent_load(tag: &str, port: u16, extra: &[&str]) {
    let dir = scratch(tag);
    let Some(child) = start(&dir, port, extra) else {
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let acks = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..WRITERS)
        .map(|cid| {
            let stop = Arc::clone(&stop);
            let acks = Arc::clone(&acks);
            std::thread::spawn(move || writer(port, cid, stop, acks))
        })
        .collect();

    // Let the writers get well into their loops, then pull the plug UNDER
    // them. The kill lands between some append and its fsync somewhere; which
    // write it lands on is the whole point of not choosing.
    wait_for_load(&acks, 200, Duration::from_secs(10));
    hard_kill(child);
    stop.store(true, Ordering::Relaxed);
    let acked: u64 = handles.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    assert!(
        acked >= 200,
        "only {acked} writes were acknowledged in 1.5 s across {WRITERS} writers — the \
         load did not run long enough for the kill to land inside it, so this test \
         measured nothing"
    );

    let child2 = start(&dir, port, extra).expect("restart after kill");
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect");
    let recovered = c.run("MATCH (n:GC) RETURN n.c").expect("count");
    hard_kill(child2);

    assert!(
        recovered >= acked,
        "{acked} writes were ACKNOWLEDGED before the kill but only {recovered} were \
         recovered — a reply left the server before the fsync that covered it. \
         That is the one thing group commit must never do."
    );
    assert!(
        recovered <= acked + WRITERS,
        "{recovered} writes recovered against {acked} acknowledged — more than one \
         in-flight write per client survived, which means the accounting is wrong, \
         not the durability"
    );
    eprintln!(
        "[{tag}] {acked} acknowledged, {recovered} recovered ({} in-flight survived)",
        recovered - acked
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn with_group_commit_every_acknowledged_write_survives_kill_9() {
    acked_writes_survive_a_kill_under_concurrent_load("gc-on", 7971, &[]);
}

/// The CONTROL: the per-write-fsync arm, unchanged.
#[test]
fn without_group_commit_every_acknowledged_write_survives_kill_9() {
    acked_writes_survive_a_kill_under_concurrent_load("gc-off", 7972, &["--no-group-commit"]);
}

/// SIX WORKERS: the cross-worker fsync sharing must not acknowledge a record
/// one worker flushed on the strength of an fsync another worker started
/// BEFORE that flush. `sync_pending` reads its coverage watermark before the
/// fsync, never after, and this is the crash that would catch it reading after.
#[test]
fn with_six_workers_every_acknowledged_write_survives_kill_9() {
    acked_writes_survive_a_kill_under_concurrent_load("gc-w6", 7974, &["--workers", "6"]);
}

/// A READER must never be told about data a crash then loses.
///
/// This is the property that decides whether replies are held per batch or
/// only for the writes themselves. A write is published to the memtable on
/// append, BEFORE the batch's fsync, so a read later in the same batch sees
/// it. If that read's reply were released ahead of the fsync, the client would
/// learn a count that a crash in the next millisecond could make false.
///
/// So: writers hammer, a reader polls the count and remembers the LARGEST it
/// was ever told, the server is killed under them, and after recovery the
/// store must hold at least that many. A recovered count below the reader's
/// maximum is a phantom read.
#[test]
fn a_reader_is_never_told_about_data_a_crash_then_loses() {
    let dir = scratch("phantom");
    let port = 7973;
    let Some(child) = start(&dir, port, &[]) else {
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let acks = Arc::new(AtomicU64::new(0));
    let writers: Vec<_> = (0..WRITERS)
        .map(|cid| {
            let stop = Arc::clone(&stop);
            let acks = Arc::clone(&acks);
            std::thread::spawn(move || writer(port, cid, stop, acks))
        })
        .collect();
    let max_told = Arc::new(AtomicU64::new(0));
    let reader = {
        let stop = Arc::clone(&stop);
        let max_told = Arc::clone(&max_told);
        std::thread::spawn(move || {
            let Ok(mut c) = Client::connect(format!("127.0.0.1:{port}")) else {
                return;
            };
            while !stop.load(Ordering::Relaxed) {
                match c.run("MATCH (n:GC) RETURN n.c") {
                    Ok(n) => {
                        max_told.fetch_max(n, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        })
    };

    wait_for_load(&acks, 200, Duration::from_secs(10));
    hard_kill(child);
    stop.store(true, Ordering::Relaxed);
    let acked: u64 = writers.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    let _ = reader.join();
    let told = max_told.load(Ordering::Relaxed);
    assert!(
        told > 0,
        "the reader was never told any count — it did not observe the load, so the \
         phantom check compared nothing"
    );

    let child2 = start(&dir, port, &[]).expect("restart after kill");
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect");
    let recovered = c.run("MATCH (n:GC) RETURN n.c").expect("count");
    hard_kill(child2);

    assert!(
        recovered >= told,
        "the reader was told the store held {told} rows, but only {recovered} survived \
         the crash — a read reply was released before the fsync covering the writes \
         it observed. Replies must be held for the whole batch, not only for writes."
    );
    eprintln!("[phantom] reader's max {told}, writers acked {acked}, recovered {recovered}");
    let _ = std::fs::remove_dir_all(&dir);
}
