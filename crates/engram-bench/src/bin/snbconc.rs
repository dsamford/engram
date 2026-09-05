//! Concurrency harness for the LDBC SNB proving ground. The port benchmark is
//! single-client-sequential, which structurally hides the single-threaded-per-
//! shard ceiling: every query serialises on the one engine thread behind the
//! Bolt server. This drives K CONCURRENT Bolt clients at `portserve` and
//! reports throughput and tail latency as K grows — the measurement that
//! justifies (or refutes) morsel-driven parallelism (redesign Phase 5).
//!
//! `snbconc <addr> <query-file> <clients-csv> <seconds> [write-pct]` e.g.
//! `snbconc 127.0.0.1:7687 queries.txt 1,2,4,8,16,32 5 20`. The query file is a
//! `;;`-separated statement list (the same SNB set the port harness runs). The
//! client is `engram_bolt::client::Client` — no external driver, so the
//! pure-Rust c-deps gate holds. This bin needs real threads (K clients) and a
//! real wall clock (throughput/tail-latency), which the simulation layer's
//! `Runtime` deliberately does not provide, hence the disallowed allow below.
//!
//! # Read+write mix (M0 concurrent-write baseline)
//!
//! `write-pct` (0..=100, default 0 = the original read-only behaviour) makes
//! each client interleave WRITES at that fraction. A write op is one
//! self-contained SNB-shaped insert —
//! `CREATE (:BenchW {c,s})-[:BENCH_KNOWS {t}]->(:BenchW {c,s})` — two node
//! writes plus one relationship (a half-edge pair) published atomically. It is
//! deliberately partitioned per client (`c` = client index, `s` = a per-client
//! counter) and lives under its own label, so concurrent writers never collide
//! with each other or with the read corpus: this measures raw insert
//! throughput and the single write chokepoint, NOT write-write conflicts (that
//! contention workload lands with the MVCC-OCC milestone that needs it). Read
//! and write latency/throughput are reported separately, because the whole
//! point is that today every write serialises on the one engine thread.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use engram_bolt::client::Client;

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: snbconc <addr> <query-file> <clients-csv> <seconds> [write-pct]");
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let queries: Arc<Vec<String>> = Arc::new(
        std::fs::read_to_string(&args[2])
            .expect("query file")
            .split(";;")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    );
    let clients: Vec<usize> = args[3]
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let seconds: u64 = args[4].parse().expect("seconds");
    // Optional write fraction (0..=100). 0 keeps the original read-only run and
    // its exact output format, so committed read baselines still parse.
    let write_pct: u64 = args
        .get(5)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .min(100);
    assert!(!queries.is_empty(), "no statements in {}", args[2]);

    // Warm the server (one query, so the corpus/compaction is resident) and
    // prove the wire works before spinning up the fleet.
    match Client::connect(&addr).and_then(|mut c| c.run(&queries[0])) {
        Ok(n) => eprintln!(
            "[snbconc] connected; warm query returned {n} rows; {} statements, {seconds}s per level",
            queries.len()
        ),
        Err(e) => {
            eprintln!("[snbconc] cannot reach {addr}: {e}");
            std::process::exit(1);
        }
    }

    if write_pct == 0 {
        println!("clients   throughput(q/s)   p50(ms)   p99(ms)   max(ms)   errors");
    } else {
        eprintln!(
            "[snbconc] write mix: {write_pct}% inserts, {}% reads (writes serialise on the one engine thread today)",
            100 - write_pct
        );
        println!(
            "clients      r_q/s      w_q/s   r_p50(ms)   r_p99(ms)   w_p50(ms)   w_p99(ms)   errors"
        );
    }
    for &k in &clients {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(k);
        for cid in 0..k {
            let addr = addr.clone();
            let queries = Arc::clone(&queries);
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut rlat: Vec<u64> = Vec::new();
                let mut wlat: Vec<u64> = Vec::new();
                let mut errors = 0u64;
                let mut conn = Client::connect(&addr).ok();
                let mut ridx = 0usize;
                // A disjoint per-client id space so `s` values never overlap
                // across writers; the label + `c` keep them off the read corpus.
                let mut seq: u64 = (cid as u64) << 40;
                // Exact-fraction interleave: no RNG, so the mix is reproducible.
                let mut wacc: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    wacc += write_pct;
                    let do_write = wacc >= 100;
                    if do_write {
                        wacc -= 100;
                    }
                    let c = match conn.as_mut() {
                        Some(c) => c,
                        None => {
                            conn = Client::connect(&addr).ok();
                            errors += 1;
                            continue;
                        }
                    };
                    if do_write {
                        let a = seq;
                        let b = seq + 1;
                        seq += 2;
                        let stmt = format!(
                            "CREATE (:BenchW {{c: {cid}, s: {a}}})-[:BENCH_KNOWS {{t: {a}}}]->(:BenchW {{c: {cid}, s: {b}}})"
                        );
                        let t = Instant::now();
                        match c.run(&stmt) {
                            Ok(_) => wlat.push(t.elapsed().as_micros() as u64),
                            Err(_) => {
                                errors += 1;
                                conn = None; // drop it; reconnect next iteration
                            }
                        }
                    } else {
                        let q = &queries[ridx % queries.len()];
                        ridx += 1;
                        let t = Instant::now();
                        match c.run(q) {
                            Ok(_) => rlat.push(t.elapsed().as_micros() as u64),
                            Err(_) => {
                                errors += 1;
                                conn = None; // drop it; reconnect next iteration
                            }
                        }
                    }
                }
                (rlat, wlat, errors)
            }));
        }
        // Run the level for `seconds`, then signal stop and join.
        let level_start = Instant::now();
        std::thread::sleep(Duration::from_secs(seconds));
        stop.store(true, Ordering::Relaxed);
        let elapsed = level_start.elapsed().as_secs_f64();
        let mut rall: Vec<u64> = Vec::new();
        let mut wall: Vec<u64> = Vec::new();
        let mut errors = 0u64;
        for h in handles {
            let (rl, wl, e) = h.join().expect("client thread");
            rall.extend(rl);
            wall.extend(wl);
            errors += e;
        }
        rall.sort_unstable();
        wall.sort_unstable();
        if write_pct == 0 {
            // The original read-only format, preserved byte-for-byte.
            println!(
                "{k:>7}   {:>15.0}   {:>7.3}   {:>7.3}   {:>7.3}   {errors:>6}",
                rall.len() as f64 / elapsed,
                pct(&rall, 0.50) as f64 / 1000.0,
                pct(&rall, 0.99) as f64 / 1000.0,
                rall.last().copied().unwrap_or(0) as f64 / 1000.0,
            );
        } else {
            println!(
                "{k:>7}   {:>10.0}   {:>10.0}   {:>9.3}   {:>9.3}   {:>9.3}   {:>9.3}   {errors:>6}",
                rall.len() as f64 / elapsed,
                wall.len() as f64 / elapsed,
                pct(&rall, 0.50) as f64 / 1000.0,
                pct(&rall, 0.99) as f64 / 1000.0,
                pct(&wall, 0.50) as f64 / 1000.0,
                pct(&wall, 0.99) as f64 / 1000.0,
            );
        }
    }
}
