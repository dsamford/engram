//! The kill-9 durability lane.
//!
//! The question a database has to answer before anything else: **if the process
//! dies without warning, is acknowledged data still there?**
//!
//! This is deliberately a `SIGKILL` test, not a clean-shutdown test. A clean
//! shutdown flushes buffers and proves almost nothing — every implementation
//! passes it, including one that only ever wrote to memory. The interesting
//! question is what survives when nothing gets to run on the way out.
//!
//! Until now it could not even be asked: `Store::open_wal` had eight recovery
//! tests and ZERO non-test callers, and the shipped server called
//! `Store::new()`. The engine was durable; the product was not.
//!
//! # What "acknowledged" means here
//!
//! The client's `run()` returned `Ok`. That is the promise. A write still in
//! flight when the kill lands may or may not survive — either is correct — but
//! one the server said `Ok` to must be present after recovery, and a
//! transaction must be all-or-nothing.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use engram_bolt::client::Client;

/// A scratch directory unique to this process and test.
///
/// Not a fixed name under `temp_dir()`: the workspace already has tests that
/// collide that way when two runs overlap, and a durability test that reads
/// another run's WAL would be worse than no test.
fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-durability-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn server_bin() -> PathBuf {
    // The test binary lives in target/<profile>/deps; the server is two up.
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

/// Start a durable server on `port` over `dir`, waiting until it accepts.
fn start(dir: &Path, port: u16) -> Option<Child> {
    let bin = server_bin();
    if !bin.exists() {
        // The server binary is built by `cargo test` only if it is a dependency
        // of the test target, which it is not. Skipping is the honest outcome —
        // but it must SAY so, because a silently-skipped durability test is
        // exactly the "absent signal read as good" failure this repo is
        // organised against.
        eprintln!(
            "SKIP: {} not built. Run `cargo build -p engram-server` first for this lane to mean \
             anything.",
            bin.display()
        );
        return None;
    }
    // THE OPERATOR STEP after a kill -9.
    //
    // Every test here restarts a server it just killed without unwinding, so
    // the data directory still holds the dead server's `LOCK`. On Linux the
    // server proves the recorded pid is gone (`/proc/<pid>` is absent) and
    // takes the lock over itself; on other platforms there is no such proof
    // in std, so it refuses and tells the operator which file to remove. This
    // line is that operator following the instruction — it is what makes the
    // restart legitimate, not a way around the lock.
    let _ = std::fs::remove_file(dir.join(engram_store::dirlock::LOCK_FILE));
    let mut child = Command::new(&bin)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--data-dir")
        .arg(dir)
        .spawn()
        .expect("spawn server");
    // Wait for the listener rather than sleeping a guessed amount.
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(50));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Some(child);
        }
    }
    // Never leave the child running: a server that failed to come up would
    // otherwise hold its port and its data directory for the rest of the run,
    // and the NEXT test's failure would be blamed on the wrong thing.
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not accept connections on {port}");
}

/// SIGKILL equivalent: no unwinding, no destructors, no flush.
fn hard_kill(mut c: Child) {
    let _ = c.kill();
    let _ = c.wait();
    std::thread::sleep(Duration::from_millis(200));
}

/// Acknowledged autocommit writes survive an abrupt kill.
#[test]
fn acknowledged_writes_survive_kill_9() {
    let dir = scratch("ack");
    let port = 7751;
    let Some(child) = start(&dir, port) else {
        return;
    };

    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    for i in 0..50 {
        c.run(&format!("CREATE (:Durable {{k: {i}}})"))
            .unwrap_or_else(|e| panic!("write {i} was not acknowledged: {e}"));
    }
    // Every one of those returned Ok, so every one of them is a promise.
    drop(c);
    hard_kill(child);

    let Some(child2) = start(&dir, port) else {
        return;
    };
    let mut c2 = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect");
    let rows = c2
        .query("MATCH (n:Durable) RETURN count(n) AS c")
        .expect("count");
    let got = format!("{rows:?}");
    drop(c2);
    hard_kill(child2);

    assert!(
        got.contains("50"),
        "50 acknowledged writes must survive a kill -9; recovered: {got}"
    );
}

/// A transaction is all-or-nothing across a kill.
///
/// This is the assertion the WAL fsync gap made impossible to satisfy: before
/// it, `Transaction::commit` appended its records, published them to the
/// version map and returned `Ok` WITHOUT syncing — so a committed, acknowledged
/// multi-write transaction could vanish, and a partially-written one could
/// survive. 0 or N, never a number in between.
#[test]
fn a_committed_transaction_is_all_or_nothing_across_a_kill() {
    let dir = scratch("txn");
    let port = 7752;
    let Some(child) = start(&dir, port) else {
        return;
    };

    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    // Five writes in ONE statement: one commit, one write-set, one fsync.
    c.run("CREATE (:Txn {k: 0}), (:Txn {k: 1}), (:Txn {k: 2}), (:Txn {k: 3}), (:Txn {k: 4})")
        .expect("the transaction must be acknowledged");
    drop(c);
    hard_kill(child);

    let Some(child2) = start(&dir, port) else {
        return;
    };
    let mut c2 = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect");
    let rows = c2
        .query("MATCH (n:Txn) RETURN count(n) AS c")
        .expect("count");
    let got = format!("{rows:?}");
    drop(c2);
    hard_kill(child2);

    assert!(
        got.contains('5'),
        "an acknowledged 5-write transaction must recover as 5, never partially: {got}"
    );
}

/// Recovery is repeatable: two restarts do not duplicate or drop history.
///
/// A replay that re-appends what it read would double the data on the second
/// restart, and a replay that mis-seeks would drop it. Neither shows up in a
/// single-restart test.
#[test]
fn recovery_is_idempotent_across_repeated_restarts() {
    let dir = scratch("idem");
    let port = 7753;
    let Some(child) = start(&dir, port) else {
        return;
    };
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    for i in 0..20 {
        c.run(&format!("CREATE (:Rep {{k: {i}}})")).expect("write");
    }
    drop(c);
    hard_kill(child);

    let mut counts = Vec::new();
    for _ in 0..3 {
        let Some(ch) = start(&dir, port) else { return };
        let mut cc = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect");
        let rows = cc
            .query("MATCH (n:Rep) RETURN count(n) AS c")
            .expect("count");
        counts.push(format!("{rows:?}"));
        drop(cc);
        hard_kill(ch);
    }
    assert!(
        counts.iter().all(|c| c.contains("20")),
        "three restarts must each recover exactly 20 nodes, got {counts:?}"
    );
}

/// A data directory holding a foreign file is REFUSED, not destroyed.
///
/// The old `Wal::open` parsed from byte 0 and truncated whatever it could not
/// parse — so pointing the server at the wrong path silently emptied the file.
/// The header exists to make "not a WAL" and "a damaged WAL" different answers.
#[test]
fn a_foreign_file_in_the_data_dir_is_refused_not_truncated() {
    let dir = scratch("foreign");
    let wal = dir.join("engram.wal");
    // LONGER than WAL_HEADER_LEN on purpose. A short decoy is caught by the
    // header-length check and never reaches the magic comparison, so it would
    // prove only that a truncated file is refused — the interesting case is a
    // full-sized file that simply is not ours, which is what an operator
    // actually points at by mistake.
    let contents: Vec<u8> = b"somebody's important file, definitely not a WAL, and long enough                               to clear the header length check so the MAGIC is what refuses it."
        .to_vec();
    assert!(
        contents.len() > 64,
        "the decoy must clear the header length"
    );
    std::fs::write(&wal, &contents).expect("write decoy");

    let bin = server_bin();
    if !bin.exists() {
        eprintln!("SKIP: server binary not built");
        return;
    }
    let out = Command::new(&bin)
        .arg("127.0.0.1:7754")
        .arg("--data-dir")
        .arg(&dir)
        .output()
        .expect("run server");

    // The file must be byte-identical. This is the whole point: a refusal that
    // still destroyed the file would be no better than the truncation.
    let after = std::fs::read(&wal).expect("read back");
    assert_eq!(
        after, contents,
        "the server must not modify a file it refused to open"
    );
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(
        msg.contains("not an engram WAL") || msg.contains("cannot open the data directory"),
        "the refusal must NAME the reason; stderr was: {msg}"
    );
}

/// Crash the server WHILE it is under concurrent write load, then recover.
///
/// The tests above kill an idle server, which exercises replay but not the
/// interesting window: a kill that lands *between* an append and its fsync, or
/// midway through a transaction's write-set, or while several connections are
/// mid-statement. That window is where a torn tail actually comes from, and it
/// is the only way to find out whether "recover the longest valid prefix" holds
/// against a real one rather than a hand-crafted one.
///
/// # What is asserted, and what deliberately is not
///
/// NOT "N writes survive" — under load nobody knows what N is at the instant of
/// the kill, and a test that guessed would be flaky by construction. What is
/// asserted is the pair of properties that must hold whatever N turns out to be:
///
///  1. **The server comes back at all.** A torn tail must be recovered from, not
///     choked on. Before the WAL header this was the case that truncated the
///     file to zero.
///  2. **What comes back is COHERENT.** Every `:Load` node carries a marker
///     property written in the same statement, so a node recovered without its
///     marker would mean a half-applied write — the thing MVCC plus a
///     write-set fsync exists to prevent.
///  3. **It is still writable.** Recovery that leaves a read-only or wedged
///     store is not recovery.
#[test]
fn a_crash_under_write_load_recovers_coherently() {
    let dir = scratch("underload");
    let port = 7755;
    let Some(child) = start(&dir, port) else {
        return;
    };

    // Four writers, hammering until told to stop.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut writers = Vec::new();
    for w in 0..4u64 {
        let stop = std::sync::Arc::clone(&stop);
        let acked = std::sync::Arc::clone(&acked);
        writers.push(std::thread::spawn(move || {
            let Ok(mut c) = Client::connect(format!("127.0.0.1:{port}")) else {
                return;
            };
            let mut i = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let k = (w << 32) | i;
                i += 1;
                // Marker and key written together: if one can be recovered
                // without the other, the write was not atomic.
                let stmt = format!("CREATE (:Load {{k: {k}, mark: {k}}})");
                if c.run(&stmt).is_ok() {
                    acked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    break; // the server is gone — expected, that is the point
                }
            }
        }));
    }

    // Let real load build, then kill mid-flight.
    std::thread::sleep(Duration::from_millis(1200));
    let before = acked.load(std::sync::atomic::Ordering::Relaxed);
    hard_kill(child);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in writers {
        let _ = h.join();
    }
    assert!(
        before > 0,
        "the load generator acknowledged nothing, so this test proved nothing about crashing \
         under load"
    );

    // 1. It comes back.
    let Some(child2) = start(&dir, port) else {
        return;
    };
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("reconnect after crash");

    // 2. What came back is coherent: no node has a key without its marker.
    let incoherent = c
        .query("MATCH (n:Load) WHERE n.mark IS NULL OR n.mark <> n.k RETURN count(n) AS c")
        .expect("coherence query");
    let incoherent = format!("{incoherent:?}");

    // 3. It is still writable.
    c.run("CREATE (:AfterCrash {ok: 1})")
        .expect("the recovered store must accept writes");
    let survived = c
        .query("MATCH (n:Load) RETURN count(n) AS c")
        .expect("count");
    let survived = format!("{survived:?}");
    drop(c);
    hard_kill(child2);

    assert!(
        incoherent.contains("Int(0)"),
        "recovery produced half-applied writes (nodes whose marker does not match their key): \
         {incoherent}"
    );
    eprintln!("crash-under-load: {before} acks before the kill, recovered {survived}");
}
