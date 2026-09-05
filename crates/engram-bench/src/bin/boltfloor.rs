#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! What is the Bolt path's CEILING, and how much of a real statement is it?
//!
//! # The question
//!
//! On SF1 `balanced` every contention counter this programme has attacked now
//! reads zero — `txn_conflicts=0`, `max_attempts=1`, `fsyncs=0`, `span_excl=0`,
//! `adj_built=0`, and after §8 `adj_repaired=34` — and engram is still at 0.76x
//! of Neo4j. So what is left is CPU per operation, not waiting, and the first
//! thing to establish about per-operation cost is how much of it the engine
//! never sees.
//!
//! CIDR 2025 (*OLTP Through the Looking Glass 16 Years Later*) is the reason
//! this is the first measurement and not the fifth: once engine contention is
//! removed, communication is typically the next dominant term, and a programme
//! that keeps optimising inside the commit path after that point is optimising
//! a term that no longer leads.
//!
//! # How it answers, in one comparison
//!
//! Run statements of increasing engine cost over the SAME Bolt path, same
//! client count, same connections:
//!
//! | statement | what it measures |
//! |---|---|
//! | `RETURN 1` | the PROTOCOL FLOOR: parse, plan, encode, decode, syscalls — no store touch |
//! | a point read | the floor plus one index probe and one record decode |
//! | the profile's write | the floor plus the whole write path |
//!
//! A write that runs near the floor is protocol-bound and no amount of work
//! inside the commit path will move it. A write far below the floor is
//! engine-bound and the floor says how much headroom a protocol fix could ever
//! return. Either way the answer is a number rather than an argument, and it is
//! the same number Neo4j's own client would be paying.
//!
//! ```text
//! boltfloor <addr> <clients> <secs> [--dataset snb]
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use engram_bolt::client::Client;

/// One statement shape to time, and what its number means.
struct Shape {
    name: &'static str,
    /// `{}` is replaced by a per-op sequence number, so no two writes collide
    /// and no read is answered from a single hot row.
    template: &'static str,
    /// Whether it mutates. Reported so a reader cannot mistake the write's
    /// number for a read's.
    writes: bool,
}

const SNB: &[Shape] = &[
    Shape {
        name: "return-1",
        template: "RETURN 1",
        writes: false,
    },
    Shape {
        name: "return-param",
        // Still no store touch, but a row with real content to encode: the
        // difference from `return-1` is the encoder alone.
        template: "RETURN {} AS a, 'stress' AS b, 1.5 AS c",
        writes: false,
    },
    Shape {
        name: "point-read",
        template: "MATCH (p:Person {id: {}}) RETURN p.firstName, p.lastName",
        writes: false,
    },
    Shape {
        name: "one-hop",
        template: "MATCH (p:Person {id: {}})-[:KNOWS]-(f:Person) RETURN f.id LIMIT 25",
        writes: false,
    },
    Shape {
        name: "node-create",
        // The write half of `balanced` / `write-only`, exactly as the stress
        // harness renders it for SNB.
        template: "MATCH (p:Person {id: {}}) \
                   CREATE (m:Message:Comment {id: {}, creationDate: 1400000000000, \
                   content: 'stress', length: 6})-[:HAS_CREATOR]->(p)",
        writes: true,
    },
];

/// Plain textual substitution, NOT `format!` — the templates are Cypher, and a
/// Cypher map is written with the same braces a format string would eat. The
/// first cut escaped them as `{{`, which `str::replace` leaves in place: every
/// shape containing a map failed to parse and reported 8 errors and 0 ops,
/// which reads as the server refusing rather than as the harness sending
/// nonsense. `return-1` and `return-param` have no braces, which is exactly why
/// they were the two that "worked".
fn render(t: &str, seq: u64, space: u64, cid: usize) -> String {
    let anchor = seq % space.max(1);
    // Disjoint per client so two writers never mint the same id.
    let fresh = ((cid as u64) << 40) | seq;
    if t.matches("{}").count() >= 2 {
        t.replacen("{}", &anchor.to_string(), 1)
            .replacen("{}", &fresh.to_string(), 1)
    } else {
        t.replace("{}", &anchor.to_string())
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let addr = argv
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:7707".to_string());
    let clients: usize = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    let secs: u64 = argv.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    // The SNB person id space the stress harness anchors on.
    let space: u64 = 9_892;

    println!("# boltfloor addr={addr} clients={clients} secs={secs}");
    for shape in SNB {
        let stop = Arc::new(AtomicBool::new(false));
        let ops = Arc::new(AtomicU64::new(0));
        let errs = Arc::new(AtomicU64::new(0));
        let mut hs = Vec::new();
        for cid in 0..clients {
            let (stop, ops, errs) = (Arc::clone(&stop), Arc::clone(&ops), Arc::clone(&errs));
            let addr = addr.clone();
            hs.push(std::thread::spawn(move || {
                let mut c = match Client::connect(&addr) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[boltfloor] connect {addr}: {e}");
                        errs.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };
                // One connection per client, established BEFORE the clock
                // starts, because a per-op connect would make every shape a
                // measurement of the handshake.
                let mut n = 0u64;
                let mut seq = (cid as u64) << 24;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..8 {
                        seq += 1;
                        let q = render(shape.template, seq, space, cid);
                        match c.run(&q) {
                            Ok(_) => n += 1,
                            Err(_) => {
                                // Record what this client DID complete before
                                // giving up. Returning here without it made a
                                // partial failure indistinguishable from a
                                // total one — every erroring shape reported
                                // exactly 0 ops, which reads as "the server
                                // answered nothing" rather than "the harness
                                // stopped counting".
                                errs.fetch_add(1, Ordering::Relaxed);
                                ops.fetch_add(n, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                }
                ops.fetch_add(n, Ordering::Relaxed);
            }));
        }
        let t0 = Instant::now();
        std::thread::sleep(std::time::Duration::from_secs(secs));
        stop.store(true, Ordering::Relaxed);
        for h in hs {
            let _ = h.join();
        }
        let rate = ops.load(Ordering::Relaxed) as f64 / t0.elapsed().as_secs_f64();
        println!(
            "{{\"shape\":\"{}\",\"writes\":{},\"clients\":{clients},\"ops_per_s\":{rate:.0},\"errors\":{}}}",
            shape.name,
            shape.writes,
            errs.load(Ordering::Relaxed)
        );
    }
}
