#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! `balanced` interference attribution — which mechanism owns the -37.5%?
//!
//! # What this is for
//!
//! Nine of ten SF1 profiles now exceed Neo4j. `balanced` (50% reads / 50%
//! writes) does not, and it is the MOST interference-bound profile left: its
//! throughput sits ~37.5% below the closed-loop interference-free prediction
//!
//! ```text
//! predicted = 1 / ((1 - w) / read_rate + w / write_rate)
//! ```
//!
//! which is the ONLY correct test for a mixed profile. (Comparing a mix's
//! total ops to a pure profile's total is the mistake that made me call
//! `balanced` "read-bound" once already: at 50/50 the mix serves half as many
//! reads as the read-only profile does, so equal totals mean the reads got
//! twice as slow, not that reads are the ceiling.)
//!
//! # Why it lives here rather than in a test
//!
//! It measures wall time, so it needs release codegen. And it is an
//! ATTRIBUTION instrument: perf is quotable only from the bench pod, but
//! *which mechanism owns a ratio* is a question a local machine answers
//! honestly, because the answer is a difference between two arms measured back
//! to back on one host.
//!
//! # How it answers
//!
//! Pure arms at full and half width, then a mix with DEDICATED reader and
//! writer threads. Each class is compared against its own pure rate at its own
//! thread count (`read_kept_pct` / `write_kept_pct`), which is the only form of
//! the comparison that can name a side. A lever spec flips engine levers before
//! the run, so sweeping it says which mechanism the interference lives in.
//!
//! **`--disjoint` is the control and should be read first.** The writers write
//! a relationship type the readers never query, so the readers' table is never
//! invalidated and nothing else about the run changes — same store, same commit
//! path, same allocator, same client count. It is the arm that says where the
//! interference is NOT, and it measured 94% of solo read throughput retained
//! against 0% for the same-type run.
//!
//! `--refresh-after` mirrors `ServerConfig::refresh_after_writes`; the harness
//! runs a maintenance pass on it exactly as the server does, because a bare
//! `Graph` has none and without one this measures a configuration that never
//! ships.
//!
//! ```text
//! balattr [--clients N] [--secs S] [--nodes N] [--disjoint]
//!         [--refresh-after STAMPS] [--lever name=on|off ...]
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use engram_graph::counters::{
    ADJ_OVERLAY_ROWS_CLONED, ADJ_REPAIR_PUBLISH_LOST, ADJ_STALE_DECLINED_TO_WALK,
    ADJ_STALE_SERVED_UNMOVED, ADJ_TABLES_BUILT, ADJ_TABLES_REPAIRED,
};
use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The engine counters this harness attributes with, sampled around a phase.
///
/// Atomics rather than a `Trace`: a trace is thread-local and its recorder
/// writes on every event, so installing one in the worker loop would change
/// the very timings the phase exists to measure.
#[derive(Clone, Copy, Default)]
struct Counts {
    repaired: u64,
    built: u64,
    lost: u64,
    rows: u64,
    served: u64,
    declined: u64,
}

impl Counts {
    fn now() -> Counts {
        Counts {
            repaired: ADJ_TABLES_REPAIRED.load(Ordering::Relaxed),
            built: ADJ_TABLES_BUILT.load(Ordering::Relaxed),
            lost: ADJ_REPAIR_PUBLISH_LOST.load(Ordering::Relaxed),
            rows: ADJ_OVERLAY_ROWS_CLONED.load(Ordering::Relaxed),
            served: ADJ_STALE_SERVED_UNMOVED.load(Ordering::Relaxed),
            declined: ADJ_STALE_DECLINED_TO_WALK.load(Ordering::Relaxed),
        }
    }
    fn since(self, b: Counts) -> Counts {
        Counts {
            repaired: self.repaired - b.repaired,
            built: self.built - b.built,
            lost: self.lost - b.lost,
            rows: self.rows - b.rows,
            served: self.served - b.served,
            declined: self.declined - b.declined,
        }
    }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

struct Args {
    clients: usize,
    secs: u64,
    nodes: u64,
    /// Write edges of a type the readers do NOT query. Cuts between "reads
    /// interfere through the derived-table machinery" (a write of a type the
    /// reader's table covers makes it stale) and "reads interfere through the
    /// store and the commit path" (which a disjoint type cannot avoid).
    disjoint: bool,
    /// Commit-clock stamps between maintenance refreshes, mirroring
    /// `ServerConfig::refresh_after_writes`. `0` runs no pass at all.
    ///
    /// A bare `Graph` has no maintenance thread, so without this the harness
    /// measures a configuration that never ships: every catch-up falls on some
    /// unlucky reader, the delta grows without bound, and the reads collapse
    /// (measured: 27 reads/s against 14M/s in the disjoint control). That is a
    /// real property of the engine with the pass OFF, and it is not the
    /// property `balanced` on the pod has.
    refresh_after: u64,
    levers: Vec<(String, bool)>,
}

fn parse() -> Args {
    let mut a = Args {
        clients: 8,
        secs: 4,
        nodes: 200_000,
        disjoint: false,
        refresh_after: 8_192,
        levers: Vec::new(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--clients" => {
                a.clients = argv[i + 1].parse().expect("clients");
                i += 2
            }
            "--secs" => {
                a.secs = argv[i + 1].parse().expect("secs");
                i += 2
            }
            "--nodes" => {
                a.nodes = argv[i + 1].parse().expect("nodes");
                i += 2
            }
            "--lever" => {
                let spec = &argv[i + 1];
                let cut = spec.find('=').expect("lever spec is name=on|off");
                let (n, v) = spec.split_at(cut);
                let v = &v[1..];
                a.levers
                    .push((n.to_string(), v == "on" || v == "true" || v == "1"));
                i += 2
            }
            "--disjoint" => {
                a.disjoint = true;
                i += 1
            }
            "--refresh-after" => {
                a.refresh_after = argv[i + 1].parse().expect("refresh-after");
                i += 2
            }
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn apply(g: &Graph, levers: &[(String, bool)]) {
    for (name, on) in levers {
        match name.as_str() {
            "entity_latching" => g.set_entity_latching(*on),
            "reader_rebuild_admission" => g.set_reader_rebuild_admission(*on),
            "single_flight_repair" => g.set_single_flight_repair(*on),
            "lazy_stale_serve" => g.set_lazy_stale_serve(*on),
            "adj_change_filter" => g.set_adj_change_filter(*on),
            "single_node_stale_walk" => g.set_single_node_stale_walk(*on),
            "demote_adjacency_rebuild" => g.set_demote_adjacency_rebuild(*on),
            "adj_cost_repair" => g.set_adj_cost_repair(*on),
            "incremental_caches" => g.set_incremental_caches(*on),
            "label_epoch_atomics" => g.set_label_epoch_atomics(*on),
            "guard_put_put_exempt" => g.set_guard_put_put_exempt(*on),
            "serialisable_autocommit" => g.set_serialisable_autocommit(*on),
            "tail_span_copyout" => g.shared_store().set_tail_span_copyout(*on),
            "window_suffix_scan" => g.shared_store().set_window_suffix_scan(*on),
            other => panic!("unknown lever {other}"),
        }
    }
}

/// The fixture every phase runs against, bundled so a phase's signature is
/// about the RUN (how many readers, how many writers, for how long) rather
/// than about the corpus.
struct Bed {
    g: Arc<Graph>,
    ids: Arc<Vec<u64>>,
    tok: u32,
}

/// A corpus with real adjacency tables: one edge type, a degree of ~4, and
/// enough nodes that a full rebuild is never the cheap answer.
fn seed(nodes: u64) -> Bed {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..nodes)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x5EED_1234_ABCD_0001);
    for &src in &ids {
        for _ in 0..4 {
            let dst = ids[(rng.next() % nodes) as usize];
            g.create_rel(src, "T", dst, &none).expect("rel");
        }
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];
    // Warm the table so the run measures steady state, not a cold build.
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    Bed {
        g: Arc::new(g),
        ids: Arc::new(ids),
        tok,
    }
}

/// One phase: `clients` threads for `secs` seconds, each op a read with
/// probability `1 - w` and a write with probability `w`. Returns ops/s.
fn phase(
    bed: &Bed,
    readers: usize,
    writers: usize,
    secs: u64,
    wtype: &'static str,
    refresh_after: u64,
) -> (f64, f64, Counts) {
    let (g, ids, tok) = (&bed.g, &bed.ids, bed.tok);
    let before = Counts::now();
    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    // The maintenance thread, as `ServerConfig::derived_refresh` runs it: a
    // pass every `refresh_after` commit-clock stamps. It occupies a core, and
    // so does the server's, so the client threads are not given one back.
    let maint = if refresh_after > 0 {
        let g = Arc::clone(g);
        let stop = Arc::clone(&stop);
        Some(std::thread::spawn(move || {
            let mut last = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let now = g.shared_store().now_ts();
                if now.saturating_sub(last) >= refresh_after {
                    last = now;
                    let _ = g.refresh_stale_derived();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }))
    } else {
        None
    };
    for c in 0..(readers + writers) {
        // DEDICATED threads, not an alternating mix. A thread that does a read
        // then a write has one rate for both, so the slower class sets the
        // faster one's number and neither can be attributed. Separate threads
        // let each class be compared against its OWN pure rate at its OWN
        // thread count, which is the only form of the comparison that can name
        // a side.
        let writer = c >= readers;
        let g = Arc::clone(g);
        let ids = Arc::clone(ids);
        let stop = Arc::clone(&stop);
        let ops = Arc::clone(&ops);
        let writes = Arc::clone(&writes);
        hs.push(std::thread::spawn(move || {
            let wtype = wtype;
            let none = BTreeMap::new();
            let mut rng = Lcg(0xC0FF_EE00_0000_0001 ^ ((c as u64 + 1) << 32));
            let mut n = 0u64;
            let mut w = 0u64;
            let len = ids.len() as u64;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..16 {
                    let r = rng.next();
                    let src = ids[(r % len) as usize];
                    if writer {
                        let dst = ids[((r >> 16) % len) as usize];
                        let _ = g.create_rel(src, wtype, dst, &none);
                        w += 1;
                    } else {
                        let _ = g.adjacent_slim(src, Dir::Out, &Some(vec![tok]));
                    }
                    n += 1;
                }
            }
            ops.fetch_add(n, Ordering::Relaxed);
            writes.fetch_add(w, Ordering::Relaxed);
        }));
    }
    let t0 = Instant::now();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().expect("join");
    }
    if let Some(m) = maint {
        m.join().expect("maintenance join");
    }
    let secs = t0.elapsed().as_secs_f64();
    let total = ops.load(Ordering::Relaxed) as f64;
    let w = writes.load(Ordering::Relaxed) as f64;
    ((total - w) / secs, w / secs, Counts::now().since(before))
}

fn main() {
    let a = parse();
    let bed = seed(a.nodes);
    apply(&bed.g, &a.levers);

    // Pure arms first, mix last: the mix is the measurement and it should run
    // against the warmest state, so a difference cannot be a warming artifact.
    let wtype = if a.disjoint { "W" } else { "T" };
    let half = a.clients / 2;
    // The pure arms at FULL width give the closed-loop prediction; the pure
    // arms at HALF width are what the mix's own halves must be compared
    // against, since the mix runs each class on half the threads.
    let (read_rate, _, _) = phase(&bed, a.clients, 0, a.secs, wtype, a.refresh_after);
    let (_, write_rate, _) = phase(&bed, 0, a.clients, a.secs, wtype, a.refresh_after);
    let (half_read, _, _) = phase(&bed, half, 0, a.secs, wtype, a.refresh_after);
    let (_, half_write, _) = phase(&bed, 0, half, a.secs, wtype, a.refresh_after);
    let (mix_r, mix_w, c) = phase(&bed, half, a.clients - half, a.secs, wtype, a.refresh_after);
    let mixed = mix_r + mix_w;
    // PER-CLASS, because a mix's TOTAL cannot say which side slowed down —
    // the exact mistake that once made this profile look read-bound. Each side
    // is compared against its OWN pure rate at its OWN thread count, so a
    // figure below 100% names the side that lost throughput, and by how much.
    let read_kept = mix_r / half_read * 100.0;
    let write_kept = mix_w / half_write * 100.0;

    let w = 0.5;
    let predicted = 1.0 / ((1.0 - w) / read_rate + w / write_rate);
    let interference = (mixed - predicted) / predicted * 100.0;

    let spec = if a.levers.is_empty() {
        if a.disjoint { "defaults,disjoint".to_string() } else { "defaults".to_string() }
    } else {
        a.levers
            .iter()
            .map(|(n, v)| format!("{n}={}", if *v { "on" } else { "off" }))
            .collect::<Vec<_>>()
            .join(",")
    };
    // The counters are the MIXED phase's only — the phase the interference is
    // about. `lost` against `repaired` is the redundancy rate: how much of the
    // repair work the mix pays for is thrown away.
    let redundancy = if c.repaired > 0 {
        c.lost as f64 / c.repaired as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "{{\"levers\":\"{spec}\",\"clients\":{},\"read_rate\":{read_rate:.0},\
         \"write_rate\":{write_rate:.0},\"mixed\":{mixed:.0},\
         \"predicted\":{predicted:.0},\"interference_pct\":{interference:.1},\
         \"mix_read_rate\":{mix_r:.0},\"mix_write_rate\":{mix_w:.0},\
         \"read_kept_pct\":{read_kept:.1},\"write_kept_pct\":{write_kept:.1},\
         \"mix_repaired\":{},\"mix_built\":{},\"mix_publish_lost\":{},\
         \"mix_redundancy_pct\":{redundancy:.1},\"mix_overlay_rows\":{},         \"mix_stale_served\":{},\"mix_stale_declined\":{}}}",
        a.clients, c.repaired, c.built, c.lost, c.rows, c.served, c.declined
    );
}
