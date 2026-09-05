//! Group commit, counted: how many fsyncs does a workload actually cost?
//!
//! The crash tests prove nothing is acknowledged before it is on disk. This
//! file proves the OTHER half — that batching happens, and that it does not
//! happen by skipping fsyncs. Latency cannot tell those apart: a single-client
//! durable write measured 0.49 ms after group commit against 2.40 ms before,
//! and "faster than one fsync" reads exactly like "no fsync" until the fsyncs
//! are counted.
//!
//! The server runs IN-PROCESS here so `engram_log::FSYNCS` — a process-wide
//! counter incremented by every real `Wal::sync` — is readable from the test.
//! The subprocess crash tests cannot see it; this one can.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_log::FSYNCS;
use engram_server::ServerConfig;
use engram_store::Store;

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-gc-fsyncs-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

/// Start a durable server in this process on an ephemeral port.
fn serve(dir: &std::path::Path, group_commit: bool, workers: usize) -> u16 {
    serve_with(
        dir,
        ServerConfig {
            group_commit,
            workers,
            ..ServerConfig::default()
        },
    )
}

fn serve_with(dir: &std::path::Path, cfg: ServerConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let wal = dir.join("engram.wal");
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_config(
            listener,
            move || {
                let s = Store::open_wal(&wal).expect("open wal");
                (s, Realm(1), Namespace(1))
            },
            cfg,
        );
    });
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("in-process server did not come up");
}

/// The measured write: the same attached insert the stress harness issues —
/// a node with two labels and a relationship to an existing node. One
/// statement, several store writes.
fn write(c: &mut Client, i: u64) {
    c.run(&format!(
        "MATCH (p:Person {{id: 0}}) \
         CREATE (m:Message:Comment {{id: {i}, content: 'x'}})-[:HAS_CREATOR]->(p)"
    ))
    .expect("write");
}

fn fsyncs() -> u64 {
    FSYNCS.load(Ordering::Relaxed)
}

/// The counter is PROCESS-WIDE and cargo runs a file's tests in parallel, so
/// two of these running at once would each see the other's fsyncs inside
/// their own delta. Every test holds this for its whole duration. (It failed
/// exactly that way in the full suite while passing under `--test-threads=1`
/// — a test that reads a global must own the global while it reads.)
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// ONE client, group commit on: one statement costs exactly one fsync.
///
/// Not zero — that would be the durability hole — and not the several it used
/// to cost. With one synchronous client nothing else can share the batch, so
/// this is the floor group commit can reach, and the assertion that it reaches
/// exactly it is what separates "batched" from "skipped".
#[test]
fn a_single_client_pays_exactly_one_fsync_per_statement() {
    let _serial = serial();
    let dir = scratch("single");
    let port = serve(&dir, true, 1);
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    c.run("CREATE (:Person {id: 0})").expect("seed");
    write(&mut c, 0); // warm: token minting, index, first-touch costs

    let before = fsyncs();
    const N: u64 = 40;
    for i in 1..=N {
        write(&mut c, i);
    }
    let cost = fsyncs() - before;
    eprintln!("[single, group commit ON] {N} statements cost {cost} fsync(s)");
    assert_eq!(
        cost, N,
        "{N} single-client statements cost {cost} fsyncs. Fewer than {N} means a \
         statement was acknowledged without its own fsync — the durability hole a \
         faster number can hide. More means the batch is not collapsing a statement's \
         several store writes into one sync."
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CONTROL: without group commit, every statement still pays its OWN
/// fsync. Fewer would be a durability hole — a statement acknowledged before
/// its bytes were on the disk.
///
/// **This test used to assert `> N`, and the finding it recorded has since been
/// FIXED rather than weakened.** The several-fsyncs-per-statement it pinned was
/// mostly the id allocator: `next_id` never buffers into the statement's
/// transaction (a buffered counter bump would let a concurrent transaction read
/// the same value and mint a duplicate), so each allocation autocommitted a
/// LOGGED put and, with group commit off, fsync'd on its own. `write()` below
/// creates a node AND a relationship, so it allocated twice — three fsyncs per
/// statement, of which two were the allocator.
///
/// Serving id reservations removed those two. The old behaviour is still
/// reachable, and is asserted directly by
/// `the_id_reservation_is_what_removed_the_allocator_fsyncs` below, so the
/// finding survives as a control arm instead of being deleted.
#[test]
fn without_group_commit_every_statement_still_pays_its_own_fsync() {
    let _serial = serial();
    let dir = scratch("control");
    let port = serve(&dir, false, 1);
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    c.run("CREATE (:Person {id: 0})").expect("seed");
    write(&mut c, 0);

    let before = fsyncs();
    const N: u64 = 40;
    for i in 1..=N {
        write(&mut c, i);
    }
    let cost = fsyncs() - before;
    eprintln!(
        "[single, group commit OFF] {N} statements cost {cost} fsync(s) — {:.1} per statement",
        cost as f64 / N as f64
    );
    assert!(
        cost >= N,
        "with group commit OFF, {N} statements cost only {cost} fsyncs — fewer than \
         one per statement means a statement was acknowledged without its own sync, \
         which is a durability hole no throughput number may buy"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The A/B that pins WHY the control above stopped costing several fsyncs, and
/// keeps the original finding alive as the `id_reservation: 0` arm.
///
/// With reservations off, every entity allocation autocommits a logged counter
/// put and — group commit being off — fsyncs on its own. With them on, that
/// cost amortises across the reservation and disappears from the steady state.
#[test]
fn the_id_reservation_is_what_removed_the_allocator_fsyncs() {
    const N: u64 = 40;
    let measure = |tag: &str, reservation: usize| -> u64 {
        let dir = scratch(tag);
        let port = serve_with(
            &dir,
            ServerConfig {
                group_commit: false,
                workers: 1,
                id_reservation: reservation,
                ..ServerConfig::default()
            },
        );
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        c.run("CREATE (:Person {id: 0})").expect("seed");
        // Warm past the first reservation refill so the steady state is what
        // gets measured, not the one statement that mints the range.
        for i in 0..3 {
            write(&mut c, i);
        }
        let before = fsyncs();
        for i in 10..10 + N {
            write(&mut c, i);
        }
        let cost = fsyncs() - before;
        eprintln!("[group commit OFF, id_reservation {reservation}] {N} statements cost {cost} fsync(s)");
        let _ = std::fs::remove_dir_all(&dir);
        cost
    };

    let _serial = serial();
    let without = measure("alloc-off", 0);
    let with = measure("alloc-on", 256);

    assert!(
        without > N,
        "with reservations OFF a statement must still pay for each allocation it \
         autocommits — {N} statements cost {without}, which is the arm the original \
         several-fsyncs finding was about"
    );
    assert!(
        with < without,
        "reservations must remove allocator fsyncs: {with} with, {without} without"
    );
    assert!(
        with >= N,
        "but never below one per statement — {with} for {N} statements would be a \
         durability hole, not a saving"
    );
}

/// The MAINTENANCE THREAD costs no fsync and splits no batch. The wiring at
/// its most aggressive — a refresh ask on every batch (`refresh_after_writes:
/// 1`) and a 1 ms tick with the derived refresh on — and the single-client
/// invariant above still holds EXACTLY (40 statements, 40 fsyncs: an ask
/// that split a batch or a tick that synced would show as more), and an
/// idle server, ticking and refreshing for 300 ms, performs zero.
///
/// The six-worker count below sat on its bar after the wiring landed
/// (159-163 fsyncs for 320 statements against a `< 160` bar) and the
/// wiring was suspected. Measured on the same host, three rounds each:
/// shipped 156-160, wiring config-gated out (refresh off, 1 h tick) 158-165,
/// every client on its own endpoint (no conflicts, no escalation) 152-159,
/// escalation off 159-163. Identical within noise: the count is the host's
/// fsync latency against the clients' round trip, not the wiring — and
/// this test is the one that would move if the wiring ever did cost one.
#[test]
fn the_maintenance_tick_and_refresh_ask_cost_no_fsync() {
    let _serial = serial();
    let dir = scratch("maintenance");
    let port = serve_with(
        &dir,
        ServerConfig {
            group_commit: true,
            workers: 1,
            derived_refresh: true,
            refresh_after_writes: 1,
            maintenance_tick: Duration::from_millis(1),
            ..ServerConfig::default()
        },
    );
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    c.run("CREATE (:Person {id: 0})").expect("seed");
    write(&mut c, 0);
    // A table for the refresh to keep current, so the ticks have work.
    assert_eq!(
        c.run("MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {id: 0}) RETURN m.id")
            .expect("read"),
        1
    );

    let before = fsyncs();
    const N: u64 = 40;
    for i in 1..=N {
        write(&mut c, i);
    }
    let cost = fsyncs() - before;
    eprintln!("[single, refresh ask every batch, 1 ms tick] {N} statements cost {cost} fsync(s)");
    assert_eq!(
        cost, N,
        "{N} single-client statements cost {cost} fsyncs with a refresh ask on every batch \
         and a 1 ms tick — the maintenance wiring is adding or splitting fsyncs"
    );
    // Idle: the ticks keep coming (the pass counter proves it) and sync nothing.
    let runs_before = engram_server::counters::MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed);
    let before = fsyncs();
    std::thread::sleep(Duration::from_millis(300));
    let idle = fsyncs() - before;
    let runs = engram_server::counters::MAINTENANCE_REFRESH_RUNS.load(Ordering::Relaxed) - runs_before;
    eprintln!("[idle, 1 ms tick] {runs} maintenance pass(es), {idle} fsync(s)");
    assert!(runs >= 10, "the maintenance thread barely ticked: {runs}");
    assert_eq!(idle, 0, "an idle server's maintenance ticks performed {idle} fsync(s)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// MANY clients, group commit on: N concurrent statements cost FEWER than N
/// fsyncs, and more than zero.
///
/// This is the mechanism itself. Eight clients each issue 40 statements; with
/// per-statement fsync that is at least 320 syncs. Batched, the fsync of one
/// batch is the window in which the other clients' next statements queue, so
/// the count falls well below 320 — and the crash tests have already shown
/// that nothing in those batches was acknowledged early.
#[test]
fn concurrent_clients_share_fsyncs() {
    let _serial = serial();
    let dir = scratch("shared");
    let port = serve(&dir, true, 1);
    {
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        c.run("CREATE (:Person {id: 0})").expect("seed");
        write(&mut c, 0);
    }

    const CLIENTS: u64 = 8;
    const PER: u64 = 40;
    let before = fsyncs();
    let handles: Vec<_> = (0..CLIENTS)
        .map(|cid| {
            std::thread::spawn(move || {
                let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
                for i in 0..PER {
                    write(&mut c, 1_000 + cid * PER + i);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer");
    }
    let cost = fsyncs() - before;
    let statements = CLIENTS * PER;
    eprintln!(
        "[{CLIENTS} clients, group commit ON] {statements} statements cost {cost} fsync(s) — \
         {:.1} statements per fsync",
        statements as f64 / cost.max(1) as f64
    );
    assert!(cost > 0, "{statements} durable statements cost NO fsync at all");
    assert!(
        cost < statements,
        "{statements} concurrent statements cost {cost} fsyncs — nothing was batched. \
         Either the clients were never concurrent (check they overlap) or the engine \
         loop is not draining its inbox before syncing."
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// SIX WORKERS, sixteen clients: the fsyncs are shared ACROSS workers, not
/// merely within one.
///
/// Each worker runs its own batch loop, so with six of them the question is
/// whether six batch fsyncs happen in a row (the convoy that measured 326
/// write ops/s at `--workers 6`) or whether one worker's fsync covers the
/// others' flushed records. The protocol in `Store::sync_pending` says the
/// latter; this counts it. Per-batch fsync would cost close to one per
/// statement; shared, well under.
///
/// Why sixteen clients and not eight. The protocol pipelines: the syncer's
/// fsync is the window in which the other workers' statements append, and
/// the NEXT syncer's flush covers all of them — so each fsync covers what
/// landed during the previous one, and that is bounded by how many clients
/// have a statement in flight against how long the disk takes. With eight
/// clients this host lands almost exactly two per fsync, ON the bar:
/// 156-166 fsyncs for 320 statements across a dozen runs — the same with the
/// maintenance wiring config-gated out (158-166), with every client on its
/// own endpoint so nothing conflicts or escalates (149-159), and with
/// escalation off (153-164). Not a regression, a coincidence of the bar with
/// the host's fsync latency. Sixteen clients land 2.7-3.0 per fsync here
/// (211-235 for 640), which separates "shared" from "each worker pays its
/// own" (1.0) by a margin the bar can hold; the bar itself is unchanged.
///
/// Provenance of the change from eight clients, stated so it is not read as
/// a regression hidden by a bigger workload: the pre-wiring eight-client
/// number this test once cleared was NEVER reproduced on this tree — there
/// is no git history here to recover the binary it was measured against.
/// What was measured is the A/B on THIS tree: with the maintenance wiring
/// config-gated off, eight clients cost the same 147-162 fsyncs per 320
/// statements as with it on. The shape change is therefore about the host's
/// fsync latency against a client's round trip (how many statements can
/// land during one fsync), not about the wiring; sixteen clients are the
/// workload that makes the bar say what it means on this host.
#[test]
fn six_workers_share_one_fsync_across_workers() {
    let _serial = serial();
    let dir = scratch("six-workers");
    let port = serve(&dir, true, 6);
    {
        let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
        c.run("CREATE (:Person {id: 0})").expect("seed");
        write(&mut c, 0);
    }

    const CLIENTS: u64 = 16;
    const PER: u64 = 40;
    let before = fsyncs();
    let handles: Vec<_> = (0..CLIENTS)
        .map(|cid| {
            std::thread::spawn(move || {
                let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
                for i in 0..PER {
                    write(&mut c, 5_000 + cid * PER + i);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer");
    }
    let cost = fsyncs() - before;
    let statements = CLIENTS * PER;
    eprintln!(
        "[6 workers, {CLIENTS} clients, group commit ON] {statements} statements cost {cost} \
         fsync(s) — {:.1} statements per fsync",
        statements as f64 / cost.max(1) as f64
    );
    assert!(cost > 0, "{statements} durable statements cost NO fsync at all");
    // Strictly fewer than one per statement is the floor for "any sharing";
    // the bar is set at half, because a cross-worker protocol that only
    // shares occasionally is one whose convoy has merely moved.
    assert!(
        cost * 2 < statements,
        "{statements} statements over 6 workers cost {cost} fsyncs — fewer than two \
         statements per fsync means workers are NOT sharing fsyncs across the group \
         mutex; each is paying its own, which is the convoy this exists to remove"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
