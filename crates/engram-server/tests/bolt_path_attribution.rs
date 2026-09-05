//! How much of a write's wall time is the BOLT PATH rather than the engine?
//!
//! *OLTP Through the Looking Glass 16 Years Later* (CIDR 2025) reports that
//! once buffer pool, latching and locking were removed, **communication ate
//! ~70% of the CPU cycles** and the bottleneck moved to the networking layer
//! and the kernel. That is the warning this file exists to answer BEFORE more
//! budget goes inside the commit protocol: an engine change that halves a term
//! worth 10% of wall time is worth a tenth of what its microbenchmark says.
//!
//! The method is a floor, not an attribution: a statement that does almost no
//! engine work (`RETURN 1`) costs essentially one round trip — connect-free
//! request, parse, reply, and the syscalls around them. Whatever fraction of a
//! real write that floor represents is protocol overhead the engine cannot
//! remove however fast it gets.
//!
//! This runs IN PROCESS over loopback, so it is a LOWER bound on the protocol
//! share: a real deployment adds a network. Treated as "at least this much",
//! never as the number.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_store::Store;

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("engram-bolt-attrib-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

fn serve(dir: &std::path::Path) -> u16 {
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
            ServerConfig {
                workers: 1,
                ..ServerConfig::default()
            },
        );
    });
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        if Client::connect(format!("127.0.0.1:{port}")).is_ok() {
            return port;
        }
    }
    panic!("server never came up");
}

/// Median of `n` timed runs — a mean over a loopback round trip is dominated by
/// whichever iteration the scheduler interrupted.
fn median_us(mut samples: Vec<u128>) -> f64 {
    samples.sort_unstable();
    samples[samples.len() / 2] as f64
}

fn time_each(c: &mut Client, n: usize, mut f: impl FnMut(&mut Client, usize)) -> f64 {
    // Warm: the first statements pay token minting, index and first-touch costs.
    for i in 0..20 {
        f(c, 10_000 + i);
    }
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        f(c, i);
        samples.push(t.elapsed().as_micros());
    }
    median_us(samples)
}

/// Report the protocol floor as a fraction of a real write.
///
/// Asserted loosely on purpose. The POINT is the reported number, and a tight
/// assertion on a timing ratio is a flaky test pretending to be a measurement.
/// What is asserted is only what must be true for the number to mean anything:
/// that the trivial statement is cheaper than the write, and that both actually
/// ran.
#[test]
fn report_the_bolt_protocol_floor() {
    let dir = scratch("floor");
    let port = serve(&dir);
    let mut c = Client::connect(format!("127.0.0.1:{port}")).expect("connect");
    c.run("CREATE (:Anchor {a: 0})").expect("seed");

    const N: usize = 400;
    // Almost no engine work: one round trip plus a parse.
    let trivial = time_each(&mut c, N, |c, _| {
        c.run("RETURN 1").expect("trivial");
    });
    // A durable write: id allocation, record, membership, index, log, fsync.
    let write = time_each(&mut c, N, |c, i| {
        c.run(&format!("CREATE (:W {{k: {i}}})")).expect("write");
    });
    // A relationship write: the six-rows-per-edge shape.
    let rel = time_each(&mut c, N, |c, i| {
        c.run(&format!(
            "MATCH (a:Anchor {{a: 0}}) CREATE (a)-[:R]->(:W2 {{k: {i}}})"
        ))
        .expect("rel");
    });

    eprintln!(
        "[bolt attribution] median us/statement: RETURN 1 = {trivial:.0}, \
         node create = {write:.0}, rel create = {rel:.0}"
    );
    eprintln!(
        "[bolt attribution] protocol floor is >= {:.0}% of a node create and \
         >= {:.0}% of a rel create (loopback, in-process: a LOWER bound)",
        100.0 * trivial / write.max(1.0),
        100.0 * trivial / rel.max(1.0)
    );

    assert!(
        trivial > 0.0 && write > 0.0 && rel > 0.0,
        "every arm must actually have run: {trivial} {write} {rel}"
    );
    assert!(
        trivial < write,
        "a statement doing almost no engine work must be cheaper than a durable \
         write — if it is not, the write path is not what this is measuring"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
