//! A PROBE, not a benchmark: where does the write path stop scaling with
//! threads, and which part of the serialised section is responsible?
//!
//! Runs N threads of `put` on DISJOINT keys against one in-memory store, with
//! the log's fsync deferred (so no disk is in the picture), and reports
//! throughput per thread count for three variants:
//!
//!   logged     — `put`: hot latch + chain hash + log buffer + memtable
//!   unlogged   — `put_unlogged`: hot latch + memtable only (no chain hash)
//!   txn        — `Transaction` of 16 puts, committed (OCC validate + publish)
//!
//! The RATIOS between variants and across thread counts are the finding; the
//! absolute numbers are this machine's and are not to be reported.
//! `cargo test -p engram-store --test write_scaling_probe -- --ignored --nocapture`

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn run(threads: usize, per_thread: usize, variant: &str) -> f64 {
    let s = Store::new();
    s.set_group_commit(true);
    let value = vec![7u8; 120];
    let t0 = std::time::Instant::now();
    std::thread::scope(|sc| {
        for t in 0..threads {
            let s = s.clone();
            let value = value.clone();
            sc.spawn(move || {
                let base = (t as u64) << 40;
                match variant {
                    "logged" => {
                        for i in 0..per_thread as u64 {
                            s.put(&prefix(), &(base + i).to_be_bytes(), StoredValue::Plain(value.clone()))
                                .expect("put");
                        }
                    }
                    "unlogged" => {
                        for i in 0..per_thread as u64 {
                            s.put_unlogged(&prefix(), &(base + i).to_be_bytes(), StoredValue::Plain(value.clone()))
                                .expect("put");
                        }
                    }
                    "txn16" => {
                        let mut i = 0u64;
                        while (i as usize) < per_thread {
                            let mut txn = s.begin();
                            for _ in 0..16 {
                                txn.put(&prefix(), &(base + i).to_be_bytes(), StoredValue::Plain(value.clone()))
                                    .expect("put");
                                i += 1;
                            }
                            txn.commit().expect("commit");
                        }
                    }
                    other => panic!("variant {other}"),
                }
            });
        }
    });
    let secs = t0.elapsed().as_secs_f64();
    (threads * per_thread) as f64 / secs
}

#[test]
#[ignore = "a probe with timings; run by hand with --ignored --nocapture"]
fn where_the_write_path_stops_scaling() {
    let per_thread = 40_000;
    eprintln!("\n{:<10} {:>8} {:>10} {:>10} {:>10}", "variant", "threads", "ops/s", "x1", "share");
    for variant in ["logged", "unlogged", "txn16"] {
        let base = run(1, per_thread, variant);
        for &threads in &[1usize, 2, 4, 6, 8] {
            let ops = run(threads, per_thread, variant);
            eprintln!(
                "{:<10} {:>8} {:>10.0} {:>9.2}x {:>9.0}%",
                variant,
                threads,
                ops,
                ops / base,
                100.0 * ops / (base * threads as f64)
            );
        }
    }
}
