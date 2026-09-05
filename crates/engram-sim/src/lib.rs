//! The DST harness — R14's home-grown pieces, over the real store.
//!
//! One seed = one run: a seeded, swarm-configured workload on the simulated
//! shard, ending in the invariant checks every run must pass. The sweep runs
//! many seeds and enforces the COVERAGE FLOOR across them: every `sometimes!`
//! event the swept subsystems declare must fire at least once per sweep, or
//! the sweep FAILS — a simulation that never injects a fault passes
//! everything, and this is the mechanism that makes that impossible.
//!
//! The floor has already paid for itself once, against this very file: the
//! first sweep failed with seven never-fired events, and every one was a real
//! harness gap — tamper checks running outside the trace, a workload that
//! never constructed a contended CAS, a codec function nothing exercised, and
//! two executor states no scenario reached. The fixes are the named blocks in
//! [`run_seed`].
//!
//! # Swarm configuration
//!
//! The per-seed [`Config`] is DERIVED FROM THE SEED — op mix, key spread,
//! seal/compact cadence, crash arming, tamper checks. A fixed config explores
//! one narrow slice forever; R14 asks for the config itself to vary per seed
//! so the sweep walks the configuration space as well as the schedule space.
//!
//! # What a failing seed gives you
//!
//! The seed. Every run is deterministic (the harness inherits the runtime's
//! guarantee), so `SEED=n` reproduces the failure exactly — a failure here is
//! a repro, never a flake.

#![forbid(unsafe_code)]

use std::time::Duration;

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::{Subsystem, Trace, with_crash_at, with_trace};
use engram_runtime::{Rng, Runtime, Shard, SimRuntime};
use engram_store::{Store, StoredValue};

/// Per-seed run shape, derived from the seed.
#[derive(Debug, Clone)]
pub struct Config {
    /// Cooperative writer tasks.
    pub writers: u32,
    /// Ops each writer attempts.
    pub ops_per_writer: u32,
    /// Distinct logical keys the workload touches.
    pub key_space: u32,
    /// Seal roughly every N ops (0 = never).
    pub seal_every: u32,
    /// Compact after sealing, roughly every N seals (0 = never).
    pub compact_every: u32,
    /// Whether this run injects a crash at a WAL boundary and recovers.
    pub inject_crash: bool,
    /// Whether this run tampers with a COPY of the log and requires detection.
    pub tamper_check: bool,
    /// Whether a snapshot reader pins the watermark for the whole run.
    pub pinned_reader: bool,
}

impl Config {
    /// Derive a configuration from the seed — the swarm in swarm testing.
    pub fn derive(seed: u64) -> Config {
        let mut rng = Rng::new(seed ^ 0xC0FF_EE00_0000_0000);
        Config {
            writers: 2 + (rng.below(7) as u32),
            ops_per_writer: 4 + (rng.below(12) as u32),
            key_space: 1 + (rng.below(6) as u32),
            seal_every: rng.below(8) as u32,
            compact_every: rng.below(3) as u32,
            inject_crash: rng.below(3) == 0,
            tamper_check: rng.below(3) == 0,
            pinned_reader: rng.below(2) == 0,
        }
    }
}

/// One run's verdict, with everything a failure needs to be acted on.
#[derive(Debug)]
pub struct RunReport {
    /// The seed. THE repro handle.
    pub seed: u64,
    /// The derived config, printed with failures so a repro does not have to
    /// re-derive it to be understood.
    pub config: Config,
    /// The observability trace, for the sweep's coverage floor.
    pub trace: Trace,
    /// Invariant violations. Empty = the run passed.
    pub violations: Vec<String>,
}

fn prefix(realm: u32, kind: Kind) -> KeyPrefix {
    KeyPrefix {
        realm: Realm(realm),
        namespace: Namespace(1),
        kind,
        partition: Partition(1),
    }
}

/// Run one seed end to end and check every invariant.
///
/// EVERYTHING happens inside one trace — workload and invariant checks alike.
/// The first version traced only the workload; the tamper checks ran outside
/// it, their `sometimes!` events reached no trace, and the coverage floor
/// correctly reported them never-fired. The floor found the harness bug.
pub fn run_seed(seed: u64) -> RunReport {
    let outer_config = Config::derive(seed);

    let (violations, trace) = with_trace(|| {
        let cfg = Config::derive(seed);
        let mut violations: Vec<String> = Vec::new();

        // ── The workload ────────────────────────────────────────────────
        let rt = SimRuntime::new(seed);
        let store = Store::new();
        let pin = if cfg.pinned_reader {
            Some(store.pin_snapshot())
        } else {
            None
        };

        // The contended-CAS phase: every writer races the SAME create — the
        // only workload shape that reliably produces lock contention and CAS
        // losses. Scattered per-key writes almost never collide; the first
        // sweep measured exactly that, with both events never-fired.
        for w in 0..cfg.writers {
            let rt2 = rt.clone();
            let store = store.clone();
            rt.spawn(async move {
                rt2.sleep(Duration::from_millis(u64::from(w % 2))).await;
                let _ = store
                    .cas(
                        &prefix(3, Kind::KV),
                        b"lease",
                        None,
                        StoredValue::Plain(vec![w as u8]),
                    )
                    .await;
            });
        }

        // The stale-timer scenario: a task completes while its long timer is
        // still in the heap (its race resolved on the short arm). The timer
        // later fires against the dead id, and `woke a completed task` is how
        // the sweep observes the executor shrugging correctly.
        {
            let rt2 = rt.clone();
            rt.spawn(async move {
                let mut long = Box::pin(rt2.sleep(Duration::from_millis(500)));
                let mut short = Box::pin(rt2.sleep(Duration::from_millis(1)));
                std::future::poll_fn(move |cx| {
                    use std::future::Future as _;
                    if short.as_mut().poll(cx).is_ready() {
                        return std::task::Poll::Ready(());
                    }
                    long.as_mut().poll(cx).map(|_| ())
                })
                .await;
            });
        }

        // The general writers.
        for w in 0..cfg.writers {
            let rt2 = rt.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            rt.spawn(async move {
                let mut rng = Rng::new(seed.wrapping_mul(0x9E37).wrapping_add(u64::from(w)));
                for op in 0..cfg.ops_per_writer {
                    rt2.sleep(Duration::from_millis(rng.below(5))).await;
                    let key = [rng.below(u64::from(cfg.key_space)) as u8];
                    let p = prefix(1, Kind::NODE);
                    match rng.below(10) {
                        0..=5 => {
                            let _ =
                                store.put(&p, &key, StoredValue::Plain(vec![w as u8, op as u8]));
                        }
                        6 => {
                            let _ = store.delete(&p, &key);
                        }
                        7..=8 => {
                            let cur = store.get(&p, &key);
                            let _ = store
                                .cas(
                                    &p,
                                    &key,
                                    cur.as_deref(),
                                    StoredValue::Plain(vec![0xCA, w as u8]),
                                )
                                .await;
                        }
                        _ => {
                            // A protected put — Plain must refuse, Sealed must land.
                            let pp = prefix(1, Kind::PROTECTED_PROPERTY);
                            if store.put(&pp, &key, StoredValue::Plain(vec![1])).is_ok() {
                                engram_observe::unreachable_hit!("sim.protected plaintext landed");
                            }
                            let _ = store.put(&pp, &key, StoredValue::Sealed(vec![2]));
                        }
                    }
                    if cfg.seal_every > 0 && rng.below(u64::from(cfg.seal_every) + 1) == 0 {
                        let _ = store.seal();
                        if cfg.compact_every > 0 && rng.below(u64::from(cfg.compact_every) + 1) == 0
                        {
                            store.compact();
                        }
                    }
                }
            });
        }

        match rt.run(1_000_000) {
            Ok(_) => {}
            Err(e) => engram_observe::unreachable_hit!(match e {
                engram_runtime::RunError::Stalled { .. } => "sim.run stalled",
                engram_runtime::RunError::BudgetExhausted { .. } => "sim.run exhausted its budget",
            }),
        }
        drop(pin);

        // ── The deliberate-stall probe ──────────────────────────────────
        // `executor.stalled` fires only when a run wedges, which is a failure
        // everywhere else. A SEPARATE runtime hosts one never-woken task and
        // the harness requires the executor to SAY SO — a runtime returning
        // Ok here is reporting a deadlock as a clean run.
        {
            let stall_rt = SimRuntime::new(seed ^ 0x57A1_57A1);
            stall_rt.spawn(async { std::future::pending::<()>().await });
            if !matches!(
                stall_rt.run(1_000),
                Err(engram_runtime::RunError::Stalled { .. })
            ) {
                violations.push("the deliberate stall was not reported as a stall".into());
            }
        }

        // ── The var-bytes round trip ────────────────────────────────────
        // No v1 key component is variable-length, so no store path can fire
        // the escape event — but the codec ships, and shipping unexercised is
        // how escaping rots. Seeded payloads with embedded zeros round-trip
        // here, inside the trace.
        {
            let mut rng = Rng::new(seed ^ 0x0B0B);
            for _ in 0..4 {
                let len = 1 + rng.below(5) as usize;
                let payload: Vec<u8> = (0..len)
                    .map(|_| if rng.below(2) == 0 { 0x00 } else { 0x41 })
                    .collect();
                let mut enc = Vec::new();
                engram_key::encode_var_bytes(&payload, &mut enc);
                match engram_key::decode_var_bytes(&enc) {
                    Some((back, n)) if back == payload && n == enc.len() => {}
                    _ => violations.push(format!("var-bytes round trip failed for {payload:?}")),
                }
            }
        }

        // ── The adjacency scenario ──────────────────────────────────────
        // Contended edge-adds on one node (fires the CAS retry through real
        // interleaving), a small-capacity-equivalent rollover via enough
        // edges... rollover needs CHUNK_CAPACITY+ edges, which at 1024 is too
        // hot for every seed — so ONE deterministic fill runs here, sized to
        // just cross the boundary, and the half-edge window is opened by an
        // armed crash and then FOUND, every seed.
        {
            use engram_store::{EdgeDir, EdgeType, NodeAt, add_edge, chunk_stats, find_half_edges};
            let astore = Store::new();
            let art = SimRuntime::new(seed ^ 0xAD7A);
            let at = |id: u64| NodeAt {
                realm: Realm(1),
                ns: Namespace(1),
                partition: Partition(1),
                node: id,
            };
            let et = EdgeType(1);

            // Contended adds: every writer targets node 1.
            for w in 0..4u64 {
                let store = astore.clone();
                let rt2 = art.clone();
                art.spawn(async move {
                    rt2.sleep(Duration::from_millis(w % 2)).await;
                    let _ = add_edge(&store, at(1), EdgeType(1), at(100 + w)).await;
                });
            }
            if art.run(1_000_000).is_err() {
                violations.push("adjacency scenario did not complete".into());
            }
            match engram_store::degree(&astore, at(1), EdgeDir::Out, et) {
                Ok(4) => {}
                other => violations.push(format!("contended adds lost an edge: {other:?}")),
            }

            // Rollover: fill past one chunk on a second runtime, once.
            let rt2 = SimRuntime::new(seed ^ 0x0117);
            {
                let store = astore.clone();
                rt2.spawn(async move {
                    for id in 0..(engram_store::adjacency::CHUNK_CAPACITY as u64 + 8) {
                        let _ = add_edge(&store, at(2), EdgeType(1), at(10_000 + id)).await;
                    }
                });
            }
            if rt2.run(20_000_000).is_err() {
                violations.push("rollover fill did not complete".into());
            }
            match chunk_stats(&astore, at(2), EdgeDir::Out, et) {
                Ok((chunks, raw)) => {
                    if chunks < 2 {
                        violations.push("the posting list never rolled over".into());
                    }
                    if raw != engram_store::adjacency::CHUNK_CAPACITY + 8 {
                        violations.push(format!("rollover lost or doubled entries: {raw}"));
                    }
                }
                Err(e) => violations.push(format!("chunk stats failed: {e}")),
            }

            // The half-edge window: crash between the two direction writes,
            // then REQUIRE the checker to find the damage.
            let crash_store = Store::new();
            let cs = crash_store.clone();
            let crashed = with_crash_at("adjacency.between_out_and_in", || {
                let rt3 = SimRuntime::new(seed ^ 0xC8A5);
                let inner = cs.clone();
                rt3.spawn(async move {
                    let _ = add_edge(&inner, at(5), EdgeType(1), at(6)).await;
                });
                let _ = rt3.run(1_000_000);
            });
            if crashed.is_ok() {
                violations.push("the adjacency crash point never fired".into());
            }
            match find_half_edges(&crash_store, at(5), et, at(6)) {
                Ok(f) if f.len() == 1 => {}
                other => violations.push(format!("the half edge was not found: {other:?}")),
            }
        }

        // ── The semi-mask scenario ──────────────────────────────────────
        // Seeded candidate sets through the operator, with the empty
        // intersection reached deliberately (its event is on the floor) and
        // the count identity asserted on every application.
        {
            use engram_exec::{OffsetList, RowIdSet, semi_mask};
            let mut rng = Rng::new(seed ^ 0x5E31);
            let cap = 64 + rng.below(128) as usize;
            let input_offs: Vec<usize> = (0..rng.below(20))
                .map(|_| rng.below(cap as u64) as usize)
                .collect();
            let mask_offs: Vec<usize> = (0..rng.below(20))
                .map(|_| rng.below(cap as u64) as usize)
                .collect();
            let input = RowIdSet::from_offsets(cap, &input_offs).expect("input in range");
            match semi_mask(&input, &OffsetList(&mask_offs)) {
                Ok((out, r)) => {
                    if r.input != r.output + r.masked_out || out.count() != r.output {
                        violations.push("semi-mask measurement identity broke".into());
                    }
                }
                Err(e) => violations.push(format!("semi-mask refused valid input: {e}")),
            }
            // The empty intersection, deterministically.
            let a = RowIdSet::from_offsets(64, &[1, 2, 3]).expect("a");
            let (out, _) = semi_mask(&a, &OffsetList(&[40, 41])).expect("mask");
            if out.count() != 0 {
                violations.push("a disjoint mask left candidates".into());
            }
        }

        // ── The Expand scenario ─────────────────────────────────────────
        // A tiny seeded graph expanded through directories, with one peer
        // deliberately OUTSIDE the destination group so the declared event
        // fires and the carried-not-dropped contract is asserted per seed.
        {
            use engram_exec::{GroupAt, RowDirectory, expand};
            use engram_store::{EdgeDir, EdgeType, NodeAt, add_edge};
            let gstore = Store::new();
            let grt = SimRuntime::new(seed ^ 0xE4B);
            let gs = gstore.clone();
            grt.spawn(async move {
                let at = |id: u64| NodeAt {
                    realm: Realm(1),
                    ns: Namespace(1),
                    partition: Partition(1),
                    node: id,
                };
                let _ = add_edge(&gs, at(1), EdgeType(1), at(10)).await;
                let _ = add_edge(&gs, at(1), EdgeType(1), at(20)).await;
            });
            if grt.run(1_000_000).is_err() {
                violations.push("expand scenario graph build failed".into());
            }
            let src = RowDirectory::from_ids([1]);
            let dst = RowDirectory::from_ids([10]); // 20 is outside, on purpose
            let (input, _) = src.to_set(&[1]).expect("input maps");
            match expand(
                &gstore,
                GroupAt {
                    realm: Realm(1),
                    ns: Namespace(1),
                    partition: Partition(1),
                },
                &src,
                &input,
                EdgeDir::Out,
                EdgeType(1),
                &dst,
                Namespace(1),
            ) {
                Ok((out, r)) => {
                    if out.count() != 1 || r.outside_group != vec![(1, 20)] {
                        violations.push(format!("expand mis-carried the frontier: {r:?}"));
                    }
                }
                Err(e) => violations.push(format!("expand refused valid input: {e}")),
            }

            // The pipeline shell over the same graph, with the set emptied
            // mid-run so the skip event (on the floor) fires, and the report
            // asserted to name every planned stage — skipped or not.
            {
                use engram_exec::{Pipeline, RowIdSet, StageDetail};
                let (seed_set, _) = src.to_set(&[1]).expect("seed maps");
                match Pipeline::seed(&gstore, &src, seed_set, "seed")
                    .and_then(|p| p.mask(&RowIdSet::empty(1), "kill"))
                    .and_then(|p| {
                        p.expand_hop(
                            GroupAt {
                                realm: Realm(1),
                                ns: Namespace(1),
                                partition: Partition(1),
                            },
                            EdgeDir::Out,
                            EdgeType(1),
                            &dst,
                            Namespace(1),
                            "hop",
                        )
                    })
                    .and_then(|p| p.finish())
                {
                    Ok((ids, report)) => {
                        if !ids.is_empty() || report.stages.len() != 3 {
                            violations.push(format!("pipeline mis-reported: {report:?}"));
                        }
                        if report.stages[2].detail != StageDetail::Skipped {
                            violations
                                .push("the skipped expand was not reported as skipped".into());
                        }
                    }
                    Err(e) => violations.push(format!("pipeline refused valid input: {e}")),
                }
            }
        }

        // ── The index scenario ──────────────────────────────────────────
        // A range index built over seeded typed rows plus one deliberately
        // unorderable row, so the declared event fires and the answer's floor
        // contract is asserted per seed.
        {
            use engram_key::value::Tag;
            use engram_store::{IndexDef, IndexKey, PropertyId, RangeIndex, Record};
            let istore = Store::new();
            let ig = KeyPrefix {
                realm: Realm(1),
                namespace: Namespace(1),
                kind: Kind::NODE,
                partition: Partition(7),
            };
            let prop = PropertyId(3);
            let mut rng = Rng::new(seed ^ 0x1DE);
            let n = 3 + rng.below(6);
            for i in 0..n {
                let mut r = Record::new();
                let v = (rng.below(100) as i64) - 50;
                let mut tagged = vec![Tag::INT64.byte()];
                tagged.extend_from_slice(&v.to_le_bytes());
                r.set(prop, tagged);
                let _ = istore.put(&ig, &[i as u8], StoredValue::Plain(r.encode()));
            }
            let mut r = Record::new();
            r.set(prop, vec![Tag::BOOL.byte(), 1]);
            let _ = istore.put(&ig, b"boolrow", StoredValue::Plain(r.encode()));

            let idx = RangeIndex::build(&istore, &ig, IndexDef::new(1, prop), istore.now_ts());
            let ans = idx.range(&IndexKey::Int(i64::MIN), &IndexKey::Int(i64::MAX));
            if ans.unindexable != 1 {
                violations.push(format!(
                    "the unorderable row was not counted: {}",
                    ans.unindexable
                ));
            }
            if ans.bodies.len() != n as usize {
                violations.push(format!(
                    "index lost typed rows: {} of {n}",
                    ans.bodies.len()
                ));
            }
        }

        // ── The columnar scenario ───────────────────────────────────────
        // The head/tail layout under the sweep: a blockable population plus
        // one non-canonical row, compacted, then read through every path.
        // The per-seed invariant is the module's one property — the layout
        // changes WHERE bytes live, never WHAT a reader sees — and the six
        // declared columnar events fire here.
        {
            use engram_key::value::Tag;
            use engram_store::COLUMNAR_MIN_ROWS;
            use engram_store::{PropertyId, Record};
            let cstore = Store::new();
            let cg = KeyPrefix {
                realm: Realm(1),
                namespace: Namespace(1),
                kind: Kind::NODE,
                partition: Partition(9),
            };
            let mut rng = Rng::new(seed ^ 0xC01);
            let n = COLUMNAR_MIN_ROWS as u64 + rng.below(16);
            let make = |i: u64, salt: u64| -> Vec<u8> {
                let mut r = Record::new();
                let mut tagged = vec![Tag::INT64.byte()];
                tagged.extend_from_slice(&((i ^ salt) as i64).to_le_bytes());
                r.set(PropertyId(1), tagged);
                r.encode()
            };
            for i in 0..n {
                let _ = cstore.put(&cg, &i.to_be_bytes(), StoredValue::Plain(make(i, 0)));
            }
            // A record with ids OUT of order: decodes, re-encodes sorted —
            // blocking it would change its bytes, so it must stay a row.
            let mut nc = Vec::new();
            nc.extend_from_slice(&2u32.to_le_bytes());
            nc.extend_from_slice(&9u32.to_le_bytes());
            nc.extend_from_slice(&[Tag::BOOL.byte(), 1]);
            nc.extend_from_slice(&3u32.to_le_bytes());
            nc.extend_from_slice(&[Tag::BOOL.byte(), 0]);
            let _ = cstore.put(&cg, b"noncanon", StoredValue::Plain(nc.clone()));
            let _ = cstore.seal();
            cstore.compact();
            let (blocks, rows) = cstore.columnar_stats();
            if blocks < 1 || rows != n as usize {
                violations.push(format!(
                    "columnar head did not block: {blocks} block(s), {rows} of {n} rows"
                ));
            }
            let probe = rng.below(n);
            if cstore.get(&cg, &probe.to_be_bytes()) != Some(make(probe, 0)) {
                violations.push("a blocked row's bytes changed".into());
            }
            if cstore.get(&cg, b"noncanon") != Some(nc) {
                violations.push("the non-canonical row's bytes changed".into());
            }
            // A later write must shadow its block row on every read path.
            let over = rng.below(n);
            let newer = make(over, 0xDEAD);
            let _ = cstore.put(&cg, &over.to_be_bytes(), StoredValue::Plain(newer.clone()));
            let scan = cstore.scan_at(&cg, u64::MAX);
            if scan.len() != n as usize + 1 {
                violations.push(format!("columnar scan row count changed: {}", scan.len()));
            }
            let col = cstore.scan_column_at(&cg, &[], 1, u64::MAX);
            if col.len() != n as usize {
                violations.push(format!("column scan lost rows: {} of {n}", col.len()));
            }
            let over_body = over.to_be_bytes().to_vec();
            let want = Record::decode(&newer)
                .ok()
                .and_then(|r| r.get(PropertyId(1)).map(<[u8]>::to_vec));
            let got = col
                .iter()
                .find(|(b, _)| *b == over_body)
                .map(|(_, v)| v.clone());
            if got != want {
                violations.push("the overwrite did not win the column scan".into());
            }
        }

        // ── The log-truncation scenario ─────────────────────────────────
        // The shipped boundary: reads survive, seq allocation is unchanged,
        // and a from-genesis recovery of the retained suffix REFUSES —
        // fail closed, asserted per seed.
        {
            let tstore = Store::new();
            let tg = KeyPrefix {
                realm: Realm(1),
                namespace: Namespace(1),
                kind: Kind::NODE,
                partition: Partition(11),
            };
            let mut rng = Rng::new(seed ^ 0x7A0);
            let n = 4 + rng.below(12);
            for i in 0..n {
                let _ = tstore.put(&tg, &[i as u8], StoredValue::Plain(vec![i as u8]));
            }
            let cut = 1 + rng.below(n - 1);
            tstore.truncate_log_below(cut);
            for i in 0..n {
                if tstore.get(&tg, &[i as u8]) != Some(vec![i as u8]) {
                    violations.push(format!("truncation changed a read at {i}"));
                }
            }
            if tstore.log_len() != n {
                violations.push("truncation changed seq allocation".into());
            }
            if Store::recover(&tstore.log_tail(0)).is_ok() {
                violations.push("a truncated suffix recovered from genesis — must refuse".into());
            }

            // Bulk contract: an unlogged put reads back like any other write,
            // and a log replay NEVER sees it — durability is by re-ingest.
            let bstore = Store::new();
            let _ = bstore.put(&tg, b"logged", StoredValue::Plain(vec![1]));
            let bn = 1 + rng.below(4);
            for i in 0..bn {
                let _ =
                    bstore.put_unlogged(&tg, &[0xB0, i as u8], StoredValue::Plain(vec![i as u8]));
            }
            for i in 0..bn {
                if bstore.get(&tg, &[0xB0, i as u8]) != Some(vec![i as u8]) {
                    violations.push("an unlogged put did not read back".into());
                }
            }
            if bstore.unlogged_count() != bn {
                violations.push("unlogged_count disagrees with the puts made".into());
            }
            match Store::recover(&bstore.log_tail(0)) {
                Ok(replayed) => {
                    if replayed.get(&tg, b"logged") != Some(vec![1]) {
                        violations.push("replay lost a LOGGED row".into());
                    }
                    for i in 0..bn {
                        if replayed.get(&tg, &[0xB0, i as u8]).is_some() {
                            violations.push("replay saw an UNLOGGED row — the log heard it".into());
                        }
                    }
                }
                Err(_) => violations.push("an untruncated log refused to recover".into()),
            }
        }

        // ── The vector scenario ─────────────────────────────────────────
        // Seeded vectors + one unindexable row per seed; the search must rank
        // the exact-match vector first, and the answer must carry the count.
        {
            use engram_store::{PropertyId, Record, VectorIndex, encode_f32_vector};
            let vstore = Store::new();
            let vg = KeyPrefix {
                realm: Realm(1),
                namespace: Namespace(1),
                kind: Kind::NODE,
                partition: Partition(9),
            };
            let vprop = PropertyId(4);
            let mut rng = Rng::new(seed ^ 0x7EC);
            let n = 4 + rng.below(6);
            let target = rng.below(n);
            for i in 0..n {
                let mut r = Record::new();
                // Distinct, well-separated directions: basis-vector-ish.
                let mut v = vec![0.05f32; 8];
                v[(i % 8) as usize] = 1.0;
                if i >= 8 {
                    v[((i + 3) % 8) as usize] = 0.9;
                }
                r.set(vprop, encode_f32_vector(&v));
                let _ = vstore.put(&vg, &[i as u8], StoredValue::Plain(r.encode()));
            }
            let mut r = Record::new();
            r.set(vprop, encode_f32_vector(&[0.0; 8])); // zero norm: unindexable
            let _ = vstore.put(&vg, b"zerovec", StoredValue::Plain(r.encode()));

            let idx = VectorIndex::build(&vstore, &vg, vprop, vstore.now_ts());
            let mut q = vec![0.05f32; 8];
            q[(target % 8) as usize] = 1.0;
            if target >= 8 {
                q[((target + 3) % 8) as usize] = 0.9;
            }
            match idx.search(&q, 1) {
                Ok(ans) => {
                    if ans.unindexable != 1 {
                        violations.push(format!(
                            "the zero vector was not counted: {}",
                            ans.unindexable
                        ));
                    }
                    if ans.hits.first().map(|(b, _)| b.as_slice()) != Some(&[target as u8][..]) {
                        violations.push(format!("vector search missed its own target {target}"));
                    }
                }
                Err(e) => violations.push(format!("vector search refused valid input: {e}")),
            }
        }

        // ── The crypto scenario ─────────────────────────────────────────
        // Seal a seeded secret, round-trip it, refuse a tamper — the refusal
        // event is on the floor and the refusal itself is asserted per seed.
        {
            use engram_crypto::{Sealer, Secret, derive_dek, open};
            let master = {
                let mut m = [0u8; 32];
                m[..8].copy_from_slice(&seed.to_be_bytes());
                m
            };
            let mut sealer = Sealer::new(derive_dek(&master, Realm(1)), 0);
            let mut rng = Rng::new(seed ^ 0xC0DE);
            let plain: Vec<u8> = (0..8 + rng.below(24))
                .map(|_| rng.below(256) as u8)
                .collect();
            let env = sealer.seal(&Secret::new(plain.clone()));
            match open(&derive_dek(&master, Realm(1)), &env) {
                Ok(back) if back.expose() == plain.as_slice() => {}
                _ => violations.push("crypto round trip failed".into()),
            }
            let mut tampered = env.clone();
            let idx = (rng.below(tampered.len() as u64)) as usize;
            tampered[idx] ^= 0x01;
            if open(&derive_dek(&master, Realm(1)), &tampered).is_ok() {
                violations.push(format!("a tampered envelope authenticated (byte {idx})"));
            }
            if open(&derive_dek(&master, Realm(2)), &env).is_ok() {
                violations.push("a foreign realm opened the envelope".into());
            }
        }

        // ── The object-storage scenario ─────────────────────────────────
        // Sealed round trip, the mis-tier refusal, a corrupt fetch, the
        // stale-discovery refusal, an under-answered presence query — every
        // declared objstore event, per seed — then the crash window.
        {
            use engram_crypto::{Sealer, Secret, derive_dek};
            use engram_objstore::{
                DiscoverError, Discovered, FaultPlan, FaultTier, MemoryTier, Placement, RootBeacon,
                SegmentKind, StoreRefused, TierError, TierPolicy, advance_root, discover_root,
                resumable_put,
                seal::{fetch_sealed, store_sealed},
            };
            let master = {
                let mut m = [0u8; 32];
                m[..8].copy_from_slice(&seed.to_be_bytes());
                m
            };
            let policy =
                TierPolicy::new(Placement::Tier1, Placement::Tier0Pinned).expect("valid policy");
            let mut rng = Rng::new(seed ^ 0x0B57);
            let payload: Vec<u8> = (0..8 + rng.below(24))
                .map(|_| rng.below(256) as u8)
                .collect();
            let roots = 1 + rng.below(5);
            let tier = MemoryTier::new();
            let faults = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));

            let (t, f, p) = (tier.clone(), faults.clone(), payload.clone());
            let ort = SimRuntime::new(seed ^ 0x0B58);
            ort.spawn(async move {
                let push = |m: String| f.borrow_mut().push(m);
                let mut sealer = Sealer::new(derive_dek(&master, Realm(1)), 0);

                // Round trip through the tier.
                let name = match store_sealed(
                    &t,
                    &policy,
                    SegmentKind::Plain,
                    &mut sealer,
                    &Secret::new(p.clone()),
                )
                .await
                {
                    Ok(n) => n,
                    Err(e) => {
                        push(format!("store_sealed refused a valid segment: {e}"));
                        return;
                    }
                };
                match fetch_sealed(&t, &derive_dek(&master, Realm(1)), &name).await {
                    Ok(back) if back.expose() == p.as_slice() => {}
                    other => push(format!("sealed round trip failed: {other:?}")),
                }

                // The mis-tier refusal (vector segments never leave tier 0).
                match store_sealed(
                    &t,
                    &policy,
                    SegmentKind::Vector,
                    &mut sealer,
                    &Secret::new(p.clone()),
                )
                .await
                {
                    Err(StoreRefused::PinnedToTier0) => {}
                    other => push(format!("a vector segment was not pinned: {other:?}")),
                }

                // A flipped byte must refuse as corrupt, bytes withheld.
                let flipping = FaultTier::new(
                    t.clone(),
                    FaultPlan {
                        flip_byte: 255,
                        ..FaultPlan::none()
                    },
                    seed,
                );
                match fetch_sealed(&flipping, &derive_dek(&master, Realm(1)), &name).await {
                    Err(engram_objstore::FetchError::Tier(TierError::Corrupt { .. })) => {}
                    other => push(format!("a flipped byte was not refused: {other:?}")),
                }

                // Beacons: advance, then a fresh site discovers the newest.
                for s in 1..=roots {
                    let mut manifest = [0u8; 32];
                    manifest[..8].copy_from_slice(&s.to_be_bytes());
                    if advance_root(&t, RootBeacon { seq: s, manifest })
                        .await
                        .is_err()
                    {
                        push(format!("advance_root {s} refused"));
                    }
                }
                match discover_root(&t.fresh_site(), Some(roots)).await {
                    Ok(Discovered::Root(b)) if b.seq == roots => {}
                    other => push(format!(
                        "the restore drill missed the newest root: {other:?}"
                    )),
                }
                // A truncated listing below the floor must refuse, not rewind.
                let truncating = FaultTier::new(
                    t.clone(),
                    FaultPlan {
                        truncate_listing: 255,
                        ..FaultPlan::none()
                    },
                    seed,
                );
                match discover_root(&truncating, Some(roots)).await {
                    Err(DiscoverError::Stale { .. }) => {}
                    other => push(format!("a truncated listing was believed: {other:?}")),
                }

                // An under-answered presence query refuses as indeterminate.
                let under = FaultTier::new(
                    t.clone(),
                    FaultPlan {
                        under_answer: 255,
                        ..FaultPlan::none()
                    },
                    seed,
                );
                let chunks = vec![(name, Vec::new())];
                match resumable_put(&under, &chunks).await {
                    Err(TierError::Indeterminate { .. }) => {}
                    other => push(format!("an under-answer was read as an answer: {other:?}")),
                }
            });
            if ort.run(10_000_000).is_err() {
                violations.push("objstore scenario stalled".into());
            }
            violations.extend(faults.borrow().iter().cloned());

            // The crash window: seal → CRASH → put loses the upload, never
            // publishes a reference. Armed OUTSIDE the runtime above so the
            // one-shot disarm cannot race another op.
            let crash_tier = MemoryTier::new();
            let ct = crash_tier.clone();
            let crashed =
                engram_observe::with_crash_at("objstore.between_seal_and_put", move || {
                    let rt = SimRuntime::new(seed ^ 0x0B59);
                    rt.spawn(async move {
                        let mut sealer = Sealer::new(derive_dek(&master, Realm(1)), 0);
                        let _ = store_sealed(
                            &ct,
                            &policy,
                            SegmentKind::Plain,
                            &mut sealer,
                            &Secret::new(b"doomed".to_vec()),
                        )
                        .await;
                    });
                    let _ = rt.run(10_000_000);
                });
            if crashed.is_ok() {
                violations.push("the objstore crash point never fired".into());
            }
            if crash_tier.object_count() != 0 {
                violations.push("a crashed upload left bytes in the bucket".into());
            }
        }

        // ── The blob scenario ───────────────────────────────────────────
        // Chunked round trip + a seeded chunk tamper, dedup, the lease
        // suppression, the resurrection spare, and the unknown-aborts sweep —
        // every declared blob event, per seed.
        {
            use engram_blob::{
                Liveness, SweepPlan, Tier, acquire_lease, add_ref, content_key, manifest_prefix,
                open_range, plan_sweep, process_tombstones, remove_ref, seal_chunked,
                tombstone_prefix,
            };
            use engram_crypto::{Sealer, Secret, derive_dek};
            let master = {
                let mut m = [0u8; 32];
                m[..8].copy_from_slice(&seed.to_be_bytes());
                m
            };
            let mut rng = Rng::new(seed ^ 0xB10B);

            // Chunks: round trip a seeded payload, then tamper one chunk.
            let payload: Vec<u8> = (0..64 + rng.below(192))
                .map(|_| rng.below(256) as u8)
                .collect();
            let mut sealer = Sealer::new(derive_dek(&master, Realm(1)), 0);
            let blob =
                seal_chunked(&mut sealer, &Secret::new(payload.clone()), 32).expect("seeded seal");
            let dek = derive_dek(&master, Realm(1));
            match open_range(&dek, &blob, 0, payload.len() as u64) {
                Ok((out, _)) if out == payload => {}
                other => violations.push(format!("chunked round trip failed: {other:?}")),
            }
            let victim = (rng.below(blob.chunk_count() as u64)) as u32;
            let mut tampered = blob.clone();
            let mut env = tampered.swap_chunk(victim, Vec::new());
            let flip = (rng.below(env.len() as u64)) as usize;
            env[flip] ^= 0x01;
            let _ = tampered.swap_chunk(victim, env);
            if open_range(&dek, &tampered, 0, payload.len() as u64).is_ok() {
                violations.push(format!("a tampered chunk {victim} authenticated"));
            }

            // Manifest lifecycle on the seeded store runtime.
            let bstore = Store::new();
            let mp = manifest_prefix(Realm(1), Namespace(1), Partition(2));
            let tp = tombstone_prefix(Realm(1), Namespace(1), Partition(2));
            let bfaults = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
            let (bs, bf) = (bstore.clone(), bfaults.clone());
            let brt = SimRuntime::new(seed ^ 0xB10C);
            brt.spawn(async move {
                let push = |m: String| bf.borrow_mut().push(m);
                let dedup_key = content_key(b"dedup me");
                for expect in [1u32, 2] {
                    match add_ref(&bs, &mp, &dedup_key, 8, Tier::T1Engine, None, vec![1]).await {
                        Ok(rc) if rc == expect => {}
                        other => push(format!("add_ref expected {expect}: {other:?}")),
                    }
                }
                // The lease suppression: zero refs, live lease, no tombstone.
                let leased_key = content_key(b"leased");
                let _ = add_ref(&bs, &mp, &leased_key, 8, Tier::T1Engine, None, vec![]).await;
                let _ = acquire_lease(&bs, &mp, &leased_key, u64::MAX).await;
                match remove_ref(&bs, &mp, &tp, &leased_key, 0).await {
                    Ok(o) if !o.tombstoned => {}
                    other => push(format!("a leased blob was tombstoned: {other:?}")),
                }
                // The resurrection spare: tombstone, re-add, worker spares.
                let lazarus = content_key(b"lazarus");
                let _ = add_ref(&bs, &mp, &lazarus, 8, Tier::T1Engine, None, vec![]).await;
                let _ = remove_ref(&bs, &mp, &tp, &lazarus, 0).await;
                let _ = add_ref(&bs, &mp, &lazarus, 8, Tier::T1Engine, None, vec![]).await;
                let report = process_tombstones(&bs, &mp, &tp, 0, |_| {
                    Err("unlink must not run for a live entry".into())
                });
                if report.spared != 1 || report.unlinked != 0 {
                    push(format!("the resurrected blob was not spared: {report:?}"));
                }
            });
            if brt.run(20_000_000).is_err() {
                violations.push("blob scenario stalled".into());
            }
            violations.extend(bfaults.borrow().iter().cloned());

            // The unknown-aborts sweep, with a seeded mix.
            let answers: Vec<([u8; 32], Liveness)> = (0..3 + rng.below(5))
                .map(|i| {
                    let l = match rng.below(3) {
                        0 => Liveness::Live,
                        1 => Liveness::Dead,
                        _ => Liveness::Unknown,
                    };
                    (content_key(&[i as u8]), l)
                })
                .chain([(content_key(b"forced unknown"), Liveness::Unknown)])
                .collect();
            match plan_sweep(&answers) {
                SweepPlan::Aborted { unknowns } if unknowns >= 1 => {}
                other => violations.push(format!("an unknown did not abort: {other:?}")),
            }
        }

        // ── The Cypher scenario ─────────────────────────────────────────
        // A seeded differential check (parse+eval vs direct arithmetic), the
        // load-bearing null case, and every declared refusal — per seed.
        {
            use engram_cypher::{Scope, Value, eval, parse_expression, tokenize};
            let mut rng = Rng::new(seed ^ 0xC1FE);
            let (a, b, c) = (
                (rng.below(2000) as i64) - 1000,
                (rng.below(2000) as i64) - 1000,
                (rng.below(2000) as i64) - 1000,
            );
            let src = format!("{a} + {b} * {c} - ({a} % 7)");
            let expect = a + b * c - (a % 7);
            match parse_expression(&src).map(|e| eval(&e, &Scope::default())) {
                Ok(Ok(Value::Int(got))) if got == expect => {}
                other => violations.push(format!("`{src}` expected {expect}: {other:?}")),
            }
            // The load-bearing case, every seed: null = 'x' is NULL.
            match parse_expression("null = 'x'").map(|e| eval(&e, &Scope::default())) {
                Ok(Ok(Value::Null)) => {}
                other => violations.push(format!("null = 'x' must be null: {other:?}")),
            }
            // A miss against a null element propagates (the declared event).
            match parse_expression(&format!("{} IN [null]", rng.below(100)))
                .map(|e| eval(&e, &Scope::default()))
            {
                Ok(Ok(Value::Null)) => {}
                other => violations.push(format!("IN [null] must be null: {other:?}")),
            }
            // Every refusal event, deliberately.
            if tokenize("SELECT ~ FROM").is_ok() {
                violations.push("the lexer accepted `~`".into());
            }
            if parse_expression("1 +").is_ok() {
                violations.push("the parser accepted a truncated expression".into());
            }
            match parse_expression("no_such_fn_ever(1)").map(|e| eval(&e, &Scope::default())) {
                Ok(Err(engram_cypher::EvalError::UnknownFunction(_))) => {}
                other => {
                    violations.push(format!("unknown function must refuse by name: {other:?}"))
                }
            }
            // A seeded statement, round-tripped through the clause parser.
            let lim = 1 + rng.below(50);
            let stmt = format!(
                "MATCH (n:Label{} {{k: {}}})-[:R*1..{}]->(m) WHERE m.x > {} \
                 RETURN m.x AS x ORDER BY x DESC LIMIT {lim}",
                rng.below(10),
                rng.below(100),
                1 + rng.below(3),
                rng.below(100),
            );
            match engram_cypher::parse_statement(&stmt) {
                Ok(engram_cypher::Query::Single(q)) if q.clauses.len() == 2 => {}
                other => violations.push(format!("seeded statement mis-parsed: {other:?}")),
            }
            // `<-[:R]->` — arrowheads on BOTH ends — is openCypher's redundant
            // spelling of an UNDIRECTED relationship, not a syntax error. This
            // invariant previously asserted the refusal; the TCK's
            // `Two bound nodes pointing to the same node` and
            // `Handling mixed relationship patterns and directions 2` both
            // require it to parse, so the invariant was wrong, not the parser.
            // It is kept, inverted, because the SHAPE still needs pinning: a
            // regression that made it directed would be silent otherwise.
            match engram_cypher::parse_statement("MATCH (a)<-[:R]->(b) RETURN b") {
                Ok(engram_cypher::Query::Single(q)) => {
                    let undirected = matches!(
                        q.clauses.first(),
                        Some(engram_cypher::stmt::Clause::Match { pattern, .. })
                            if pattern.paths.first().is_some_and(|p| p
                                .hops
                                .first()
                                .is_some_and(|(rel, _)| rel.dir
                                    == engram_cypher::stmt::RelDir::Undirected))
                    );
                    if !undirected {
                        violations.push("a double-headed arrow is not undirected".into());
                    }
                }
                other => {
                    violations.push(format!("a double-headed arrow must parse: {other:?}"));
                }
            }
        }

        // ── The interpreter scenario ────────────────────────────────────
        // Seeded statements end to end against a real store, firing every
        // declared graph/interp event, with decoded values asserted.
        {
            use engram_cypher::{Value, parse_any, parse_statement};
            use engram_graph::{Graph, GraphError, RunError, run_query, run_stmt};
            let g = Graph::new(Store::new(), Realm(1), Namespace(1));
            g.set_degree_table_after(0);
            g.set_wall_ms(1_600_000_000_000 + (seed as i64) * 86_400_000);
            let run = |src: &str| -> Result<engram_graph::QueryResult, RunError> {
                run_stmt(
                    &g,
                    &parse_any(src).expect("scenario statements parse"),
                    Default::default(),
                )
            };
            // The row budget: a seeded cartesian must REFUSE under a tight
            // budget (the OOM killer refuses nothing) and answer exactly
            // once the budget lifts.
            {
                let mut brng = Rng::new(seed ^ 0xB4D6);
                let n = 6 + brng.below(6);
                for i in 0..n {
                    let _ = run(&format!("CREATE (:BX {{i: {i}}}), (:BY {{i: {i}}})"));
                }
                // Streaming makes the folded cartesian FREE of the budget…
                g.set_row_budget(Some((n * n / 2) as usize));
                match run("MATCH (a:BX), (b:BY) RETURN count(*) AS c") {
                    Ok(r) => {
                        if r.rows != vec![vec![Value::Int((n * n) as i64)]] {
                            violations
                                .push(format!("streamed cartesian count wrong: {:?}", r.rows));
                        }
                    }
                    Err(e) => violations.push(format!("streamed cartesian refused: {e:?}")),
                }
                // …while the PROJECTED product still refuses: buffered
                // output rows are what the budget guards.
                match run("MATCH (a:BX), (b:BY) RETURN a.i, b.i") {
                    Err(e) => {
                        if !format!("{e:?}").contains("row budget") {
                            violations.push(format!("budget refusal named wrong: {e:?}"));
                        }
                    }
                    Ok(_) => {
                        violations.push("the row budget did not refuse buffered output".into())
                    }
                }
                g.set_row_budget(None);
            }
            // An unbound WHERE variable refuses BEFORE any scan, naming
            // the variable — the alternative was measured: full-database
            // materialisation and the OOM killer.
            match run("MATCH (q:BX) WHERE q.i = unbound_name RETURN q") {
                Err(e) => {
                    if !format!("{e:?}").contains("unbound_name") {
                        violations.push(format!("scope refusal named wrong: {e:?}"));
                    }
                }
                Ok(_) => violations.push("an unbound WHERE variable did not refuse".into()),
            }
            // A query concluding with a reading clause refuses by name.
            match run("MATCH (q:BX)") {
                Err(e) => {
                    if !format!("{e:?}").contains("conclude with MATCH") {
                        violations.push(format!("conclude refusal named wrong: {e:?}"));
                    }
                }
                Ok(_) => violations.push("a concluding MATCH did not refuse".into()),
            }
            // The count fast path answers the bare shapes and must AGREE
            // with a shape the general path serves.
            match (
                run("MATCH (q:BX) RETURN count(q) AS c"),
                run("MATCH (q:BX) WHERE true RETURN count(q) AS c"),
            ) {
                (Ok(fast), Ok(general)) => {
                    if fast.rows != general.rows {
                        violations.push(format!(
                            "count fast path disagreed: {:?} vs {:?}",
                            fast.rows, general.rows
                        ));
                    }
                }
                other => violations.push(format!("count paths errored: {other:?}")),
            }
            // The planner's seeds hold their answers: a two-label pattern
            // (smallest-label pick) and a rel-driven typed count.
            let _ = run("CREATE (:BX:BXtra {i: 900}), (:BX:BXtra {i: 901})");
            // A projection, not an aggregate: the columnar scan would take a
            // count and the planner's smallest-label seed would never run. The
            // bare `WITH t` keeps the projection scan off it too (rev 60).
            match run("MATCH (t:BX:BXtra) RETURN count(t) AS c") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(2)]] {
                        violations.push(format!("two-label seed count wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("two-label seed failed: {e:?}")),
            }
            match run("MATCH (t:BX:BXtra) WITH t RETURN t.i ORDER BY t.i") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(900)], vec![Value::Int(901)]] {
                        violations.push(format!("two-label seed projection wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("two-label seed projection failed: {e:?}")),
            }
            let _ = run("MATCH (a:BX {i: 900}), (b:BX {i: 901}) CREATE (a)-[:BR]->(b)");
            // The WHERE keeps this off the hop-count fast path (which the
            // coverage floor proved was swallowing the sweep's only
            // Seed::Rels statement) — the rel-driven scan itself must run.
            match run("MATCH ()-[r:BR]->() WHERE true RETURN count(r) AS c") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(1)]] {
                        violations.push(format!("rel-driven count wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("rel-driven seed failed: {e:?}")),
            }
            // A bounded `*1..2` whose end node the breaker consumes DISTINCT-only
            // runs as a frontier BFS; over the single BR edge it reaches 901 once.
            match run("MATCH (a:BX {i: 900})-[:BR*1..2]->(b:BX) WITH DISTINCT b RETURN b.i AS i") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(901)]] {
                        violations.push(format!("frontier BFS wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("frontier BFS failed: {e:?}")),
            }
            // The index seed answers a point lookup identically.
            match run("MATCH (q:BX {i: 900}) RETURN count(q) AS c") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(1)]] {
                        violations.push(format!("index seed count wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("index seed failed: {e:?}")),
            }
            // Pushed conjuncts prune the cartesian without changing it.
            match run("MATCH (a:BX), (b:BX) WHERE a.i = 900 AND b.i = 901 RETURN count(*) AS c") {
                Ok(r) => {
                    if r.rows != vec![vec![Value::Int(1)]] {
                        violations.push(format!("pushed conjunct count wrong: {:?}", r.rows));
                    }
                }
                Err(e) => violations.push(format!("pushed conjunct failed: {e:?}")),
            }
            // Top-k pushdown pages identically to the full sort.
            match run("MATCH (t:BX) WITH t RETURN t.i AS i ORDER BY i LIMIT 1") {
                Ok(r) => {
                    if r.rows.len() != 1 {
                        violations.push(format!("top-k row count wrong: {:?}", r.rows.len()));
                    }
                }
                Err(e) => violations.push(format!("top-k failed: {e:?}")),
            }
            // The clause-scan memo, its equality index, and the bound-bound
            // exists probe: a cartesian OPTIONAL MATCH with a correlated string
            // equality over several outer rows fires all three, and the answer
            // is small enough to assert exactly.
            {
                for (i, k) in [("x", "K1"), ("y", "K2"), ("z", "K1")] {
                    let _ = run(&format!("CREATE (:BMC {{i: '{i}', k: '{k}'}})"));
                }
                for k in ["K1", "K1", "K3"] {
                    let _ = run(&format!("CREATE (:BMS {{k: '{k}'}})"));
                }
                match run(
                    "MATCH (c:BMC) OPTIONAL MATCH (s:BMS) WHERE s.k = c.k                  RETURN c.i, count(s) ORDER BY c.i",
                ) {
                    Ok(res) => {
                        let want = vec![
                            vec![Value::Str("x".into()), Value::Int(2)],
                            vec![Value::Str("y".into()), Value::Int(0)],
                            vec![Value::Str("z".into()), Value::Int(2)],
                        ];
                        if res.rows != want {
                            violations.push(format!(
                                "memoised cartesian answered {:?}, wanted {:?}",
                                res.rows, want
                            ));
                        }
                    }
                    Err(e) => violations.push(format!("memoised cartesian refused: {e:?}")),
                }
                let _ = run("MATCH (a:BMC {i: 'x'}), (b:BMS {k: 'K3'}) CREATE (a)-[:BML]->(b)");
                // Anonymous-hop expansion reads key bytes only; first with the
                // table budget at zero (the decline path), then with a table.
                g.set_adj_table_max_entries(0);
                for pass in 0..2 {
                    match run("MATCH (a:BMC {i: 'x'})-[:BML]->(b) RETURN b.k") {
                        Ok(res) => {
                            if res.rows != vec![vec![Value::Str("K3".into())]] {
                                violations.push(format!(
                                    "slim expansion (pass {pass}) answered {:?}",
                                    res.rows
                                ));
                            }
                        }
                        Err(e) => violations.push(format!("slim expansion refused: {e:?}")),
                    }
                    g.set_adj_table_max_entries(1 << 20);
                }
                // The relationship columnar scan: declined by the entry budget
                // first, then run — each vs the general path, with a type(r)
                // key so the token binding is exercised per seed.
                // On a fresh node: the degree line below counts BMC 'x'.
                let _ = run(
                    "CREATE (z:BRZ)-[:BRS {w: 1, s: 'p'}]->(z), (z)-[:BRS {w: 2}]->(z), (z)-[:BRT {w: 3, s: 'q'}]->(z)",
                );
                g.set_adj_table_max_entries(0);
                for pass in 0..2 {
                    let fast = run(
                        "MATCH ()-[r:BRS|BRT]->() WHERE coalesce(r.s, 'n') <> 'q' RETURN type(r) AS t, count(r) AS c, sum(r.w) AS w ORDER BY t",
                    );
                    let slow = run(
                        "MATCH ()-[r:BRS|BRT]->() WITH r WHERE coalesce(r.s, 'n') <> 'q' RETURN type(r) AS t, count(r) AS c, sum(r.w) AS w ORDER BY t",
                    );
                    match (fast, slow) {
                        (Ok(a), Ok(b)) => {
                            if a.rows != b.rows {
                                violations.push(format!(
                                    "rel scan (pass {pass}) {:?} vs general {:?}",
                                    a.rows, b.rows
                                ));
                            }
                        }
                        (a, b) => violations
                            .push(format!("rel scan refused (pass {pass}): {a:?} / {b:?}")),
                    }
                    g.set_adj_table_max_entries(1 << 20);
                }
                // Constant-count stages: a hop count and a predicated label
                // count with carried variables, at a WITH and at the RETURN,
                // each vs the general path (a carried-variable predicate).
                for (fast_q, slow_q) in [
                    (
                        "MATCH (t:BMC) WITH count(t) AS a MATCH (:BMC)-[r:BML]->(:BMS) RETURN a, count(r) AS e",
                        "MATCH (t:BMC) WITH count(t) AS a MATCH (:BMC)-[r:BML]->(:BMS) WHERE a IS NOT NULL RETURN a, count(r) AS e",
                    ),
                    (
                        "MATCH (t:BMC) WITH count(t) AS a OPTIONAL MATCH (z:BMS {k: 'K3'}) WITH a, count(z) AS n MATCH (y:BMS) WHERE y.k <> 'K3' RETURN a, n, count(y) AS m",
                        "MATCH (t:BMC) WITH count(t) AS a OPTIONAL MATCH (z:BMS {k: 'K3'}) WHERE a IS NOT NULL WITH a, count(z) AS n MATCH (y:BMS) WHERE y.k <> 'K3' AND n IS NOT NULL RETURN a, n, count(y) AS m",
                    ),
                ] {
                    match (run(fast_q), run(slow_q)) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "constant-count stage {:?} vs general {:?}: {fast_q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => violations.push(format!(
                            "constant-count stage refused: {x:?} / {y:?}: {fast_q}"
                        )),
                    }
                }
                // The projection scan with late materialisation, over nodes
                // (a bare `n` on the page) and over relationships, each vs
                // the general path, per seed.
                for (fast_q, slow_q) in [
                    (
                        "MATCH (n:BX) WHERE n.i >= 2 RETURN n.i AS i, n ORDER BY i DESC LIMIT 2",
                        "MATCH (n:BX) WITH n WHERE n.i >= 2 RETURN n.i AS i, n ORDER BY i DESC LIMIT 2",
                    ),
                    (
                        "MATCH ()-[r:BRS|BRT]->() RETURN r.w AS w, type(r) AS t ORDER BY w",
                        "MATCH ()-[r:BRS|BRT]->() WITH r RETURN r.w AS w, type(r) AS t ORDER BY w",
                    ),
                ] {
                    match (run(fast_q), run(slow_q)) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "projection scan {:?} vs general {:?}: {fast_q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => violations
                            .push(format!("projection scan refused: {x:?} / {y:?}: {fast_q}")),
                    }
                }
                // A column-filtered seed for a two-clause stage, vs the general
                // seed (a conjunct the rewrite declines), per seed.
                match (
                    run(
                        "MATCH (n:BX) WHERE n.i >= 2 OPTIONAL MATCH (m:BX {i: n.i}) RETURN n.i AS a, m.i AS b ORDER BY a",
                    ),
                    run(
                        "MATCH (n:BX) WHERE n.i >= 2 OR id(n) < 0 OPTIONAL MATCH (m:BX {i: n.i}) RETURN n.i AS a, m.i AS b ORDER BY a",
                    ),
                ) {
                    (Ok(x), Ok(y)) => {
                        if x.rows != y.rows {
                            violations.push(format!(
                                "column-filtered seed {:?} vs general {:?}",
                                x.rows, y.rows
                            ));
                        }
                    }
                    (x, y) => {
                        violations.push(format!("column-filtered seed refused: {x:?} / {y:?}"))
                    }
                }
                // The columnar stage producer: a degree chain at a WITH and at a
                // RETURN, vs the general path under the kill switch, per seed.
                for q in [
                    "MATCH (n:BX) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds RETURN size(ds) AS n, ds[size(ds) - 1] AS max",
                    "MATCH (n:BX) WITH n.i AS i, count { (n)-[:BXR]->() } AS d RETURN i, d ORDER BY d DESC, i LIMIT 3",
                ] {
                    let fast = run(q);
                    g.set_columnar_scans(false);
                    let slow = run(q);
                    g.set_columnar_scans(true);
                    match (fast, slow) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "columnar stage {:?} vs general {:?}: {q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => {
                            violations.push(format!("columnar stage refused: {x:?} / {y:?}: {q}"))
                        }
                    }
                }
                // Hop counts from the tables (one end free: degree sums; both
                // ends: the adjacency walk; over budget: the per-node walk),
                // and a DISTINCT projection — each vs the general path under
                // the switch, per seed.
                for (q, budget) in [
                    (
                        "MATCH (a:BMC)-[r:BML]->() RETURN count(r) AS c",
                        1usize << 20,
                    ),
                    ("MATCH (:BMC)-[r:BML]->(:BMS) RETURN count(r) AS c", 1 << 20),
                    ("MATCH (:BMC)-[r:BML]->(:BMS) RETURN count(r) AS c", 0),
                    (
                        "MATCH (n:BX) RETURN DISTINCT n.i % 2 AS p ORDER BY p",
                        1 << 20,
                    ),
                    (
                        "MATCH (n:BX) WHERE n.i >= 2 RETURN DISTINCT n LIMIT 2",
                        1 << 20,
                    ),
                ] {
                    let _ = run("CREATE (:BHC)"); // a fresh epoch: no cached tables
                    g.set_adj_table_max_entries(budget);
                    let fast = run(q);
                    g.set_columnar_scans(false);
                    let slow = run(q);
                    g.set_columnar_scans(true);
                    g.set_adj_table_max_entries(1 << 20);
                    match (fast, slow) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "table hop count / distinct {:?} vs general {:?}: {q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => violations.push(format!(
                            "table hop count / distinct refused: {x:?} / {y:?}: {q}"
                        )),
                    }
                }
                // EXISTS { MATCH … } / COUNT { MATCH … } need no RETURN, and an
                // outer property read inside the body sees the full node.
                match run(
                    "MATCH (a:BMC {i: 'x'}) RETURN EXISTS { MATCH (a)-[:BML]->() WHERE a.i IS NOT NULL } AS e, COUNT { MATCH (a)-[:BML]->(:BMS) } AS c",
                ) {
                    Ok(r) => {
                        if r.rows != vec![vec![Value::Bool(true), Value::Int(1)]] {
                            violations.push(format!(
                                "subquery body without RETURN answered {:?}",
                                r.rows
                            ));
                        }
                    }
                    Err(e) => {
                        violations.push(format!("subquery body without RETURN refused: {e:?}"))
                    }
                }
                // A body that is MORE than one plain MATCH (a trailing WITH) is
                // not pattern-shaped: it still concludes with a synthesised
                // RETURN through the interpreter — the single-MATCH bodies
                // above lift to the pattern probe since v92 and no longer
                // reach that path, so this keeps the state on the floor.
                match run(
                    "MATCH (a:BMC {i: 'x'}) RETURN EXISTS { MATCH (a)-[:BML]->(m) WITH m WHERE m IS NOT NULL } AS e, COUNT { MATCH (a)-[:BML]->(m:BMS) WITH m } AS c",
                ) {
                    Ok(r) => {
                        if r.rows != vec![vec![Value::Bool(true), Value::Int(1)]] {
                            violations.push(format!(
                                "multi-clause subquery body without RETURN answered {:?}",
                                r.rows
                            ));
                        }
                    }
                    Err(e) => violations.push(format!(
                        "multi-clause subquery body without RETURN refused: {e:?}"
                    )),
                }
                // The hop-bearing aggregate scan vs the general path under the
                // switch, per seed: end labels filter by membership, end and
                // relationship properties from columns.
                // Two ends 70 ids apart with the wide `i` column between them:
                // an unlabelled end's budget is its distinct ids, so the hop
                // scan declines its end column here.
                let wide_hop = (2..=70)
                    .map(|i| format!("(:BHW {{i: {i}}})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = run(&format!(
                    "CREATE (x:BHX {{i: 1, k: 1}}), {wide_hop}, (y:BHX {{i: 71, k: 2}}) WITH x, y CREATE (x)-[:BNX]->(y), (y)-[:BNX]->(x)"
                ));
                for q in [
                    "MATCH (a:BMC)-[r:BML]->(b:BMS) WHERE a.i IS NOT NULL RETURN a.i AS i, b.k AS k, count(r) AS c ORDER BY i, k",
                    "MATCH (a)-[r:BRS|BRT]->(b:BRZ) RETURN type(r) AS t, sum(r.w) AS w ORDER BY t",
                    // Unlabelled ends 70 ids apart across the wide `i` column:
                    // the hop scan declines its end column and the general
                    // path answers.
                    "MATCH (a)-[r:BNX]->(b) WHERE a.i IS NOT NULL RETURN b.i AS k, count(r) AS c ORDER BY k",
                ] {
                    let fast = run(q);
                    g.set_columnar_scans(false);
                    let slow = run(q);
                    g.set_columnar_scans(true);
                    match (fast, slow) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "hop scan {:?} vs general {:?}: {q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => violations.push(format!("hop scan refused: {x:?} / {y:?}: {q}")),
                    }
                }
                // UNWIND steps and an aggregating breaker in the columnar stage,
                // vs the general path under the switch, per seed.
                for q in [
                    "MATCH (n:BX) UNWIND [n.i, n.i * 10, null] AS v WITH v WHERE v IS NOT NULL RETURN v ORDER BY v",
                    "MATCH (n:BX) UNWIND [1, 2] AS k WITH n.i % 2 AS p, k WITH p, sum(k) AS s, count(*) AS c ORDER BY p RETURN p, s, c",
                ] {
                    let fast = run(q);
                    g.set_columnar_scans(false);
                    let slow = run(q);
                    g.set_columnar_scans(true);
                    match (fast, slow) {
                        (Ok(x), Ok(y)) => {
                            if x.rows != y.rows {
                                violations.push(format!(
                                    "unwind/fold stage {:?} vs general {:?}: {q}",
                                    x.rows, y.rows
                                ));
                            }
                        }
                        (x, y) => violations
                            .push(format!("unwind/fold stage refused: {x:?} / {y:?}: {q}")),
                    }
                }
                // A date-only string is midnight for datetime(), per seed.
                match run(
                    "RETURN datetime('2015-07-21') = datetime('2015-07-21T00:00:00Z') AS same",
                ) {
                    Ok(r) => {
                        if r.rows != vec![vec![Value::Bool(true)]] {
                            violations.push(format!("date-only datetime answered {:?}", r.rows));
                        }
                    }
                    Err(e) => violations.push(format!("date-only datetime refused: {e:?}")),
                }
                // Selective projection: the predicate reads the narrow `k`
                // column; the two BHX survivors sit 70 ids apart across the
                // wide `i` column, so the items come from projected gets; vs
                // the general path, per seed.
                match (
                    run("MATCH (n:BHX) WHERE n.k > 0 RETURN n.i AS i ORDER BY i"),
                    {
                        g.set_columnar_scans(false);
                        let r = run("MATCH (n:BHX) WHERE n.k > 0 RETURN n.i AS i ORDER BY i");
                        g.set_columnar_scans(true);
                        r
                    },
                ) {
                    (Ok(x), Ok(y)) => {
                        if x.rows != y.rows {
                            violations.push(format!(
                                "selective projection {:?} vs general {:?}",
                                x.rows, y.rows
                            ));
                        }
                    }
                    (x, y) => {
                        violations.push(format!("selective projection refused: {x:?} / {y:?}"))
                    }
                }
                // Selective projection over a CONTIGUOUS survivor span: the
                // predicate reads `p`, the items read `q` (a column the
                // predicate never touches), and the two survivors of three sit
                // side by side, so the items' walk runs over the survivors.
                let _ =
                    run("CREATE (:BSV {p: 1, q: 10}), (:BSV {p: 2, q: 20}), (:BSV {p: 3, q: 30})");
                match (
                    run("MATCH (n:BSV) WHERE n.p > 1 RETURN n.q AS q ORDER BY q"),
                    {
                        g.set_columnar_scans(false);
                        let r = run("MATCH (n:BSV) WHERE n.p > 1 RETURN n.q AS q ORDER BY q");
                        g.set_columnar_scans(true);
                        r
                    },
                ) {
                    (Ok(x), Ok(y)) => {
                        if x.rows != y.rows {
                            violations.push(format!(
                                "selective projection (span) {:?} vs general {:?}",
                                x.rows, y.rows
                            ));
                        }
                    }
                    (x, y) => violations.push(format!(
                        "selective projection (span) refused: {x:?} / {y:?}"
                    )),
                }
                // Property-index SEEK coverage: a label past the seek's
                // min-size floor with a selective value, so the seek fires on
                // all three paths (stage, columnar projection, columnar
                // aggregate) and is checked against the forced scan. Gated to
                // one seed - 520 creates are not worth repeating, and the
                // floor only needs each event once.
                if seed == 1 {
                    for i in 0..520u32 {
                        let kind = if i % 52 == 0 { "rare" } else { "common" };
                        let _ = run(&format!(
                            "CREATE (:SEEK {{i: {i}, kind: '{kind}', x: {}}})",
                            i % 7
                        ));
                    }
                    // 10 rare of 520: 10*16 < 520, so the seek is taken.
                    for q in [
                        "MATCH (n:SEEK) WHERE n.kind = 'rare' WITH n ORDER BY n.i RETURN n.i AS i",
                        "MATCH (n:SEEK) WHERE n.kind = 'rare' RETURN n.x AS x ORDER BY x",
                        "MATCH (n:SEEK) WHERE n.kind = 'rare' RETURN count(n) AS c",
                    ] {
                        let fast = run(q);
                        g.set_property_seek(false);
                        let slow = run(q);
                        g.set_property_seek(true);
                        match (fast, slow) {
                            (Ok(x), Ok(y)) => {
                                if x.rows != y.rows {
                                    violations.push(format!(
                                        "property seek {:?} vs scan {:?}: {q}",
                                        x.rows, y.rows
                                    ));
                                }
                            }
                            (x, y) => violations
                                .push(format!("property seek refused: {x:?} / {y:?}: {q}")),
                        }
                    }
                }
                // The general path's per-id seed over a minority label answers
                // identically to the columnar scan (the columnar paths are
                // switched off for it).
                g.set_columnar_scans(false);
                match run("MATCH (c:BMC) RETURN c.i ORDER BY c.i") {
                    Ok(res) => {
                        let want = vec![
                            vec![Value::Str("x".into())],
                            vec![Value::Str("y".into())],
                            vec![Value::Str("z".into())],
                        ];
                        if res.rows != want {
                            violations.push(format!("share-gated scan answered {:?}", res.rows));
                        }
                    }
                    Err(e) => violations.push(format!("share-gated scan refused: {e:?}")),
                }
                g.set_columnar_scans(true);
                // A dead projection: c is carried by the WITH and never read.
                // A multi-label count through the intersection vs the general path.
                let _ = run("CREATE (:BML1:BML2 {i: 'w'}), (:BML1 {i: 'v'}), (:BML2 {i: 'u'})");
                let fast = run("MATCH (n:BML1:BML2) RETURN count(n)");
                let slow = run("MATCH (n:BML1:BML2) WHERE true RETURN count(n)");
                match (fast, slow) {
                    (Ok(a), Ok(b)) => {
                        if a.rows != b.rows {
                            violations.push(format!(
                                "multi-label count {:?} vs walk {:?}",
                                a.rows, b.rows
                            ));
                        }
                    }
                    (a, b) => violations.push(format!("multi-label count refused: {a:?} / {b:?}")),
                }
                // The columnar aggregate scan vs the general path, per seed.
                let fast = run("MATCH (n:BX) WHERE n.i >= 2 RETURN count(*), sum(n.i), min(n.i)");
                let slow =
                    run("MATCH (n:BX) WITH n WHERE n.i >= 2 RETURN count(*), sum(n.i), min(n.i)");
                match (fast, slow) {
                    (Ok(a), Ok(b)) => {
                        if a.rows != b.rows {
                            violations.push(format!(
                                "columnar scan {:?} vs general {:?}",
                                a.rows, b.rows
                            ));
                        }
                    }
                    (a, b) => violations.push(format!("columnar scan refused: {a:?} / {b:?}")),
                }
                // The columnar scan with a lifted exists probe, and a bare-count
                // stage chain, each vs the general path, per seed.
                let _ = run(
                    "CREATE (:BEX {i: 1})-[:BER]->(:BEY {i: 1}), (:BEX {i: 2})-[:BER]->(:BEZ {i: 2}), (:BEX {i: 3})",
                );
                let fast = run(
                    "MATCH (n:BEX) WITH count(n) AS t, count(CASE WHEN exists((n)-[:BER]->(:BEY)) THEN 1 END) AS w RETURN t, w",
                );
                let slow = run(
                    "MATCH (n:BEX) WITH n WITH count(n) AS t, count(CASE WHEN exists((n)-[:BER]->(:BEY)) THEN 1 END) AS w RETURN t, w",
                );
                match (fast, slow) {
                    (Ok(a), Ok(b)) => {
                        if a.rows != b.rows {
                            violations.push(format!(
                                "lifted-probe scan {:?} vs general {:?}",
                                a.rows, b.rows
                            ));
                        }
                    }
                    (a, b) => violations.push(format!("lifted-probe scan refused: {a:?} / {b:?}")),
                }
                let fast = run(
                    "MATCH (s:BEX) WITH count(s) AS a OPTIONAL MATCH (d:BEY) WITH a, count(d) AS b OPTIONAL MATCH (z:BEZ) WITH a, b, count(*) AS c RETURN a, b, c",
                );
                let slow = run(
                    "MATCH (s:BEX) WHERE true WITH count(s) AS a OPTIONAL MATCH (d:BEY) WHERE true WITH a, count(d) AS b OPTIONAL MATCH (z:BEZ) WHERE true WITH a, b, count(*) AS c RETURN a, b, c",
                );
                match (fast, slow) {
                    (Ok(a), Ok(b)) => {
                        if a.rows != b.rows {
                            violations.push(format!(
                                "bare-count chain {:?} vs general {:?}",
                                a.rows, b.rows
                            ));
                        }
                    }
                    (a, b) => violations.push(format!("bare-count chain refused: {a:?} / {b:?}")),
                }
                // A narrow label beside a wide column: the columnar scan must
                // decline on its budget and the general path must answer the
                // same thing, per seed.
                // Two members 70 ids apart: a 71-entry span against a budget
                // of 4 x 2, so the column read aborts and the scan declines.
                // 69 wide rows clear COLUMNAR_MIN_ROWS, so the compacted
                // phase serves them from a block and aborts THERE.
                let wide = (2..=70)
                    .map(|i| format!("(:BWD {{i: {i}}})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = run(&format!("CREATE (:BNR {{i: 1}}), {wide}, (:BNR {{i: 71}})"));
                // Once in row form, once over compacted column blocks: the
                // budget aborts in both walks.
                for phase in 0..2 {
                    if phase == 1 {
                        g.shared_store().seal();
                        g.shared_store().compact();
                    }
                    for (fast_q, slow_q) in [
                        (
                            "MATCH (n:BNR) WHERE n.i > 0 RETURN count(n), sum(n.i)",
                            "MATCH (n:BNR) WITH n WHERE n.i > 0 RETURN count(n), sum(n.i)",
                        ),
                        // Presence reads: within budget over the wide label
                        // (served from rows, then from blocks), declined on
                        // the budget over the narrow one.
                        (
                            "MATCH (n:BWD) WHERE n.i IS NOT NULL RETURN count(n)",
                            "MATCH (n:BWD) WITH n WHERE n.i IS NOT NULL RETURN count(n)",
                        ),
                        (
                            "MATCH (n:BNR) WHERE n.i IS NULL RETURN count(n)",
                            "MATCH (n:BNR) WITH n WHERE n.i IS NULL RETURN count(n)",
                        ),
                        // The label disjunction: BNR ∪ BWD, labels from
                        // membership.
                        (
                            "MATCH (m) WHERE (m:BNR OR (m:BWD AND m.i > 60)) AND m.i IS NOT NULL RETURN count(m)",
                            "MATCH (m) WITH m WHERE (m:BNR OR (m:BWD AND m.i > 60)) AND m.i IS NOT NULL RETURN count(m)",
                        ),
                    ] {
                        match (run(fast_q), run(slow_q)) {
                            (Ok(a), Ok(b)) => {
                                if a.rows != b.rows {
                                    violations.push(format!(
                                        "budget/presence scan {:?} vs general {:?} (phase {phase}): {fast_q}",
                                        a.rows, b.rows
                                    ));
                                }
                            }
                            (a, b) => violations.push(format!(
                                "budget/presence scan refused: {a:?} / {b:?} (phase {phase}): {fast_q}"
                            )),
                        }
                    }
                    // A bound-end hop: `(p)-[:OWNS]->(c)` with c bound and p
                    // unbound drives from c's incoming adjacency, not a scan of
                    // p; forced-scan and reversed agree, per seed.
                    let _ = run(
                        "CREATE (a:OWN {i: 1}), (b:OWN {i: 2}) WITH a, b CREATE (a)-[:OWNS]->(b)",
                    );
                    match (
                        run("MATCH (c:OWN {i: 2}) MATCH (p)-[:OWNS]->(c) RETURN p.i AS pi"),
                        {
                            g.set_hop_reversal(false);
                            let r =
                                run("MATCH (c:OWN {i: 2}) MATCH (p)-[:OWNS]->(c) RETURN p.i AS pi");
                            g.set_hop_reversal(true);
                            r
                        },
                    ) {
                        (Ok(a), Ok(b)) => {
                            if a.rows != b.rows {
                                violations.push(format!(
                                    "bound-end hop {:?} vs scan {:?} (phase {phase})",
                                    a.rows, b.rows
                                ));
                            }
                        }
                        (a, b) => violations.push(format!(
                            "bound-end hop refused: {a:?} / {b:?} (phase {phase})"
                        )),
                    }
                    // The identity seek: `elementId(m) = e` with `e` carried in
                    // the row is ONE get, never a scan — vs the row's own value.
                    match (
                        run(
                            "MATCH (n:BWD) WHERE n.i = 30 WITH elementId(n) AS e MATCH (m) WHERE elementId(m) = e RETURN m.i AS i",
                        ),
                        run("MATCH (n:BWD) WHERE n.i = 30 RETURN n.i AS i"),
                    ) {
                        (Ok(a), Ok(b)) => {
                            if a.rows != b.rows {
                                violations.push(format!(
                                    "identity seek {:?} vs direct {:?} (phase {phase})",
                                    a.rows, b.rows
                                ));
                            }
                        }
                        (a, b) => violations.push(format!(
                            "identity seek refused: {a:?} / {b:?} (phase {phase})"
                        )),
                    }
                    // A per-id projection of one BWD row — in the compacted
                    // phase a block row, read as its columns, never as the
                    // assembled record — against the columnar scan.
                    g.set_columnar_scans(false);
                    let per_id = run("MATCH (n:BWD) WHERE n.i = 30 RETURN n.i AS i, n.i + 1 AS j");
                    g.set_columnar_scans(true);
                    match (
                        run("MATCH (n:BWD) WHERE n.i = 30 RETURN n.i AS i, n.i + 1 AS j"),
                        per_id,
                    ) {
                        (Ok(a), Ok(b)) => {
                            if a.rows != b.rows || a.rows.len() != 1 {
                                violations.push(format!(
                                    "projected get {:?} vs scan {:?} (phase {phase})",
                                    b.rows, a.rows
                                ));
                            }
                        }
                        (a, b) => violations.push(format!(
                            "projected get refused: {a:?} / {b:?} (phase {phase})"
                        )),
                    }
                }
                // The count store vs the membership walk, per seed, after every
                // write this scenario made.
                for l in ["BX", "BY", "BMC", "BMS"] {
                    let walked = g.members(Some(l)).map(|m| m.len() as u64).unwrap_or(0);
                    if g.count_label_nodes(l) != walked {
                        violations.push(format!("count store disagrees with the walk on {l}"));
                    }
                }
                match run("MATCH (c:BMC) WITH c, c.i AS i WHERE id(c) >= 0 RETURN i ORDER BY i") {
                    Ok(res) => {
                        if res.rows.len() != 3 {
                            violations
                                .push(format!("dead-projection scan answered {:?}", res.rows));
                        }
                    }
                    Err(e) => violations.push(format!("dead-projection scan refused: {e:?}")),
                }
                match run("MATCH (c:BMC) WITH c RETURN c.i LIMIT 1") {
                    Ok(res) => {
                        if res.rows.len() != 1 {
                            violations
                                .push(format!("plain LIMIT answered {} rows", res.rows.len()));
                        }
                    }
                    Err(e) => violations.push(format!("plain LIMIT refused: {e:?}")),
                }
                match run("MATCH (a:BMC) RETURN a.i, exists((a)-[:BML]->(:BMS)) ORDER BY a.i") {
                    Ok(res) => {
                        let want = vec![
                            vec![Value::Str("x".into()), Value::Bool(true)],
                            vec![Value::Str("y".into()), Value::Bool(false)],
                            vec![Value::Str("z".into()), Value::Bool(false)],
                        ];
                        if res.rows != want {
                            violations.push(format!("labelled exists answered {:?}", res.rows));
                        }
                    }
                    Err(e) => violations.push(format!("labelled exists refused: {e:?}")),
                }
                match run("MATCH (a:BMC {i: 'x'}) WITH a RETURN count { (a)--() }") {
                    Ok(res) => {
                        if res.rows != vec![vec![Value::Int(1)]] {
                            violations.push(format!("degree fast path answered {:?}", res.rows));
                        }
                    }
                    Err(e) => violations.push(format!("degree count refused: {e:?}")),
                }
                match run(
                    "MATCH (a:BMC {i: 'x'}), (b:BMS {k: 'K3'})                  RETURN exists((a)-[:BML]->(b)), exists((b)-[:BML]->(a))",
                ) {
                    Ok(res) => {
                        if res.rows != vec![vec![Value::Bool(true), Value::Bool(false)]] {
                            violations
                                .push(format!("bound-bound exists probe answered {:?}", res.rows));
                        }
                    }
                    Err(e) => violations.push(format!("exists probe refused: {e:?}")),
                }
            }

            let mut rng = Rng::new(seed ^ 0x6EA9);
            let v = (rng.below(1000) as i64) - 500;

            // Round trip a seeded value; MERGE fires its created event.
            if run(&format!("MERGE (n:S {{k: 1}}) ON CREATE SET n.v = {v}")).is_err() {
                violations.push("merge refused".into());
            }
            match run("MATCH (n:S {k: 1}) RETURN n.v") {
                Ok(r) if r.rows == vec![vec![Value::Int(v)]] => {}
                other => violations.push(format!("seeded value did not round trip: {other:?}")),
            }
            // A second MERGE must not duplicate.
            let _ = run("MERGE (n:S {k: 1})");
            match run("MATCH (n:S) RETURN count(*)") {
                Ok(r) if r.rows == vec![vec![Value::Int(1)]] => {}
                other => violations.push(format!("MERGE duplicated: {other:?}")),
            }
            // null SET removes (event), and IS NULL agrees.
            let _ = run("MATCH (n:S) SET n.v = null");
            match run("MATCH (n:S) RETURN n.v IS NULL") {
                Ok(r) if r.rows == vec![vec![Value::Bool(true)]] => {}
                other => violations.push(format!("null set did not remove: {other:?}")),
            }
            // A connected delete refuses (event).
            let _ = run("MATCH (n:S) CREATE (n)-[:R]->(:S2)");
            match run("MATCH (n:S) DELETE n") {
                Err(RunError::Graph(GraphError::StillConnected(_))) => {}
                other => violations.push(format!("connected delete did not refuse: {other:?}")),
            }
            // OPTIONAL MATCH produces the null row (event).
            match run("MATCH (n:S) OPTIONAL MATCH (n)-[:NOPE]->(m) RETURN m") {
                Ok(r) if r.rows == vec![vec![Value::Null]] => {}
                other => violations.push(format!("optional match broke: {other:?}")),
            }
            // A procedure refuses BY NAME (event). `db.labels` was the
            // example until R-5 taught the engine the connect-time
            // introspection procedures (`db.labels`, `db.relationshipTypes`,
            // `db.propertyKeys`, `dbms.components`) — R-5's gate ran the graph
            // suites and not this sweep, so the invariant went red on main
            // unnoticed. The known procedure is now asserted to ANSWER (its
            // own labels among the rows) and an unknown one to refuse.
            match run("CALL db.labels() YIELD label RETURN label") {
                Ok(r) if r.rows.iter().any(|row| row == &vec![Value::Str("S".into())]) => {}
                other => violations.push(format!("db.labels() did not list the seeded label: {other:?}")),
            }
            match run("CALL db.noSuchProcedure() YIELD x RETURN x") {
                Err(RunError::Unsupported(_)) => {}
                other => violations.push(format!("procedure did not refuse: {other:?}")),
            }
            // A unique constraint refuses a duplicate (event), and the
            // vector procedure skips a wrong-dimension row (event) while
            // ranking a seeded query correctly.
            let _ = run("CREATE CONSTRAINT FOR (u:Uq) REQUIRE u.k IS UNIQUE");
            let _ = run("CREATE (:Uq {k: 1})");
            match run("CREATE (:Uq {k: 1})") {
                Err(RunError::Graph(GraphError::ConstraintViolation(_))) => {}
                other => violations.push(format!("a duplicate was not refused: {other:?}")),
            }
            let _ = run("CREATE VECTOR INDEX sv FOR (d:Vec) ON (d.v)");
            let (a, b) = (
                1.0 + (rng.below(50) as f64) / 100.0,
                (rng.below(100) as f64) / 100.0,
            );
            let _ = run(&format!("CREATE (:Vec {{n: 'hit', v: [{a}, {b}]}})"));
            let _ = run("CREATE (:Vec {n: 'threedee', v: [1.0, 0.0, 0.0]})");
            match run(&format!(
                "CALL db.index.vector.queryNodes('sv', 5, [{a}, {b}]) YIELD node, score RETURN node.n, score"
            )) {
                Ok(r)
                    if r.rows.len() == 1
                        && r.rows[0][0] == engram_cypher::Value::Str("hit".into()) =>
                {
                    if let engram_cypher::Value::Float(s) = r.rows[0][1] {
                        if (s - 1.0).abs() > 1e-9 {
                            violations.push(format!("self-cosine was {s}, not 1.0"));
                        }
                    }
                }
                other => violations.push(format!("vector query broke: {other:?}")),
            }
            // The ANN arm, crossed cheaply: three more eligible vectors and a
            // lowered crossover; both declared events fire, and the top hit
            // must STILL be the exact-match vector (ANN is not allowed to
            // change the answer here).
            for i in 0..3u64 {
                let x = 0.1 + (rng.below(100) as f64) / 200.0 + i as f64;
                let _ = run(&format!(
                    "CREATE (:Vec {{n: 'filler{i}', v: [{x:.3}, {:.3}]}})",
                    1.0 - x / 10.0
                ));
            }
            g.set_vector_exact_max(2);
            match run(&format!(
                "CALL db.index.vector.queryNodes('sv', 1, [{a}, {b}]) YIELD node RETURN node.n"
            )) {
                Ok(r) if r.rows == vec![vec![engram_cypher::Value::Str("hit".into())]] => {}
                other => violations.push(format!("the ANN arm changed the answer: {other:?}")),
            }
            g.set_vector_exact_max(2048);
            // Temporal: the 30-day-window idiom against the SEEDED clock, and
            // a calendar differential (add months, verify via components).
            let m_off = 1 + rng.below(11) as i64;
            match run(&format!(
                "RETURN (date('2020-01-15') + duration('P{m_off}M')).month"
            )) {
                Ok(r) if r.rows == vec![vec![Value::Int(1 + m_off)]] => {}
                other => violations.push(format!("month arithmetic broke: {other:?}")),
            }
            let _ = run("CREATE (:Tw {at: datetime('2000-01-01T00:00:00Z')})");
            match run("MATCH (t:Tw) WHERE t.at > datetime() - duration('P30D') RETURN count(*)") {
                Ok(r) if r.rows == vec![vec![Value::Int(0)]] => {}
                other => violations.push(format!("the 30-day window broke: {other:?}")),
            }

            // The crash window: node record written, membership not — the
            // torn state must be OBSERVABLE as record-without-membership.
            let cg = Graph::new(Store::new(), Realm(1), Namespace(1));
            let crashed =
                engram_observe::with_crash_at("graph.between_node_and_membership", || {
                    let _ = run_query(
                        &cg,
                        &parse_statement("CREATE (:Torn {x: 1})").expect("parses"),
                        Default::default(),
                    );
                });
            if crashed.is_ok() {
                violations.push("the graph crash point never fired".into());
            }
            match run_query(
                &cg,
                &parse_statement("MATCH (n:Torn) RETURN n").expect("parses"),
                Default::default(),
            ) {
                Ok(r) if r.rows.is_empty() => {}
                other => violations.push(format!("the torn node was label-visible: {other:?}")),
            }
        }

        // ── The declared-state scenario ─────────────────────────────────
        // Eight states the Graph declares (`sometimes!`) that the sections
        // above never reached: the first `cargo test --workspace` gate of the
        // engine programme (2026-09-02) found the coverage floor red on all
        // eight, unnoticed because earlier gates ran a crate subset. Each is
        // reached the way its emitter documents, with the answer asserted as
        // well as the event — a state reached with a wrong answer is worse
        // than one never reached.
        {
            use engram_cypher::{Value, parse_any};
            use engram_graph::{Graph, RunError, run_stmt};
            let mk = || {
                let g = Graph::new(Store::new(), Realm(1), Namespace(1));
                g.set_wall_ms(1_600_000_000_000 + (seed as i64) * 86_400_000);
                g
            };
            let run_on = |g: &Graph, src: &str| -> Result<engram_graph::QueryResult, RunError> {
                run_stmt(g, &parse_any(src).expect("declared-state statements parse"), Default::default())
            };
            let count_of = |g: &Graph, src: &str| -> Option<i64> {
                match run_on(g, src) {
                    Ok(r) => match r.rows.first().and_then(|row| row.first()) {
                        Some(Value::Int(n)) => Some(*n),
                        _ => None,
                    },
                    Err(_) => None,
                }
            };

            // (1) + (2) A multi-key pattern map SEEKS the declared index on
            // its first property, and DECLINES to the label scan when no
            // declared index covers one. Both answer the same rows.
            {
                let g = mk();
                let _ = run_on(&g, "CREATE INDEX mk_a FOR (n:MK) ON (n.a)");
                let mut rng = Rng::new(seed ^ 0x5EEC);
                let n = 4 + rng.below(6) as i64;
                for i in 0..n {
                    let _ = run_on(&g, &format!("CREATE (:MK {{a: {}, b: {i}}}), (:MQ {{x: {}, y: {i}}})", i % 3, i % 2));
                }
                // MERGE is the statement that matches through `match_path`
                // (a count is claimed by the pipeline first): a MERGE of a row
                // that exists must match it — and not create a second one.
                for (stmt, label, want) in [
                    ("MERGE (n:MK {a: 1, b: 1}) RETURN n.b", "MK", n),
                    ("MERGE (n:MQ {x: 0, y: 2}) RETURN n.y", "MQ", n),
                ] {
                    match run_on(&g, stmt) {
                        Ok(r) if r.rows.len() == 1 => {}
                        other => violations.push(format!("multi-key merge `{stmt}` answered {other:?}")),
                    }
                    match count_of(&g, &format!("MATCH (n:{label}) RETURN count(n)")) {
                        Some(c) if c == want => {}
                        other => violations.push(format!("multi-key merge changed :{label} to {other:?}, want {want}")),
                    }
                }
            }

            // (3) A labelled hop count with NO table of either kind falls back
            // to per-node probes, and answers what the tables answer.
            {
                let g = mk();
                g.set_degree_table_after(0);
                let mut rng = Rng::new(seed ^ 0x0F0B);
                let m = 3 + rng.below(5) as i64;
                for i in 0..m {
                    let _ = run_on(&g, &format!("CREATE (a:HA {{i: {i}}})-[:HR]->(:HB {{i: {i}}})"));
                }
                let _ = run_on(&g, "CREATE (:Tick)");
                g.set_degree_table_after(u64::MAX);
                g.set_adj_table_max_entries(0);
                let probed = count_of(&g, "MATCH (a:HA)-[:HR]->(b:HB) RETURN count(*) AS c");
                g.set_degree_table_after(0);
                g.set_adj_table_max_entries(1 << 20);
                let _ = run_on(&g, "CREATE (:Tick)");
                let tabled = count_of(&g, "MATCH (a:HA)-[:HR]->(b:HB) RETURN count(*) AS c");
                if probed != Some(m) || tabled != Some(m) {
                    violations.push(format!("hop count per-node probes {probed:?} vs tables {tabled:?}, want {m}"));
                }
            }

            // (4) A reader holding a STALE table it cannot repair, below the
            // probe admission, is kept off the full-span rebuild and served
            // from the span walk — the same truth.
            {
                let g = mk();
                g.set_degree_table_after(0);
                let mut rng = Rng::new(seed ^ 0xA11D);
                let m = 3 + rng.below(5) as i64;
                for i in 0..m {
                    let _ = run_on(&g, &format!("CREATE (a:KA {{i: {i}}})-[:KR]->(:KB {{i: {i}}})"));
                }
                // Build the table on a first read.
                let first = count_of(&g, "MATCH (a:KA)-[:KR]->(b:KB) RETURN count(*) AS c");
                // Then: no admission, no repair, no priced single-node decline.
                g.set_degree_table_after(u64::MAX);
                g.set_incremental_caches(false);
                g.set_single_node_stale_walk(false);
                let _ = run_on(&g, "MATCH (a:KA {i: 0}) CREATE (a)-[:KR]->(:KB {i: 99})");
                // A row-returning anchored read walks per node on the general
                // path — the reader that asks the stale table with admission
                // off; a count would be folded by the pipeline instead.
                let stale = match run_on(&g, "MATCH (a:KA {i: 0})-[:KR]->(b:KB) RETURN b.i ORDER BY b.i") {
                    Ok(r) => Some(r.rows.len() as i64),
                    Err(_) => None,
                };
                g.set_degree_table_after(0);
                g.set_incremental_caches(true);
                g.set_single_node_stale_walk(true);
                if first != Some(m) || stale != Some(2) {
                    violations.push(format!("kept-off reader answered {stale:?} rows after {first:?}, want 2"));
                }
            }

            // (5) An adjacency row whose relationship RECORD is gone — removed
            // through the raw store, as a torn write would leave it — is
            // skipped by DETACH DELETE rather than failing it.
            {
                let store = Store::new();
                let g = Graph::new(store.clone(), Realm(1), Namespace(1));
                let _ = run_on(&g, "CREATE (a:OA {i: 1})-[:OR]->(b:OB {i: 1}), (a)-[:OR]->(:OB {i: 2})");
                let rid = match run_on(&g, "MATCH (:OA)-[r:OR]->(:OB {i: 2}) RETURN id(r)") {
                    Ok(r) => match r.rows.first().and_then(|row| row.first()) {
                        Some(Value::Int(id)) => Some(*id as u64),
                        _ => None,
                    },
                    Err(_) => None,
                };
                match rid {
                    Some(id) => {
                        let _ = store.delete(&g.rel_prefix_for_test(), &id.to_be_bytes());
                        match run_on(&g, "MATCH (a:OA) DETACH DELETE a") {
                            Ok(_) => {}
                            Err(e) => violations.push(format!("detach over an orphan row refused: {e:?}")),
                        }
                        match count_of(&g, "MATCH (a:OA) RETURN count(a)") {
                            Some(0) => {}
                            other => violations.push(format!("detached node still present: {other:?}")),
                        }
                    }
                    None => violations.push("orphan scenario could not read the relationship id".into()),
                }
            }

            // (6) + (7) A derived sidecar written by a compaction is REFUSED
            // when the sealed set moved under it, and when the store holds
            // rows above its stamp — and the graph still answers.
            {
                use engram_graph::compact_paged_emitting;
                let dir = std::env::temp_dir().join(format!(
                    "engram-sim-sidecar-{}-{}",
                    std::process::id(),
                    seed
                ));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).expect("scratch dir");
                let build = |store: &Store| -> std::sync::Arc<Graph> {
                    let g = std::sync::Arc::new(Graph::new(store.clone(), Realm(1), Namespace(1)));
                    g.set_degree_table_after(0);
                    for i in 0..12i64 {
                        let _ = run_on(&g, &format!("CREATE (:SP {{id: {i}}})"));
                        if i % 4 == 3 {
                            store.seal();
                        }
                    }
                    for i in 1..12i64 {
                        let _ = run_on(&g, &format!("MATCH (a:SP {{id: 0}}), (b:SP {{id: {i}}}) CREATE (a)-[:SR]->(b)"));
                    }
                    store.seal();
                    // Warm: the tables the sidecar will record.
                    let _ = count_of(&g, "MATCH (a:SP {id: 0})-[:SR]->(b) RETURN count(b)");
                    for i in 12..16i64 {
                        let _ = run_on(&g, &format!("CREATE (:SP {{id: {i}}})"));
                        let _ = run_on(&g, &format!("MATCH (a:SP {{id: 0}}), (b:SP {{id: {i}}}) CREATE (a)-[:SR]->(b)"));
                    }
                    store.seal();
                    g
                };
                let truth = |g: &Graph| count_of(g, "MATCH (a:SP {id: 0})-[:SR]->(b) RETURN count(b)");
                for arm in ["moved", "newer"] {
                    let sub = dir.join(arm);
                    std::fs::create_dir_all(&sub).expect("scratch arm dir");
                    let store = Store::new();
                    let g = build(&store);
                    let want = truth(&g);
                    let paged = match store.into_paged(&sub, 8 << 20) {
                        Ok(c) => c,
                        Err(e) => {
                            violations.push(format!("sidecar {arm}: into_paged failed: {e:?}"));
                            continue;
                        }
                    };
                    let list = [std::sync::Arc::clone(&g)];
                    if let Err(e) = compact_paged_emitting(&list, &store, &sub, &paged) {
                        violations.push(format!("sidecar {arm}: compaction failed: {e:?}"));
                        continue;
                    }
                    let g0 = Graph::new(store.clone(), Realm(1), Namespace(1));
                    let _ = run_on(&g0, "CREATE (:SP {id: 900})");
                    if arm == "moved" {
                        store.seal(); // the sealed set's identity changes
                    } // "newer": the write stays in the tail, above the stamp
                    let g1 = Graph::new(store.clone(), Realm(1), Namespace(1));
                    let adopted = g1.adopt_derived_sidecar(&sub);
                    if adopted != 0 {
                        violations.push(format!("sidecar {arm}: adopted {adopted} structure(s), must refuse"));
                    }
                    let now = truth(&g1);
                    if now != want || want.is_none() {
                        violations.push(format!("sidecar {arm}: answered {now:?} after refusing, want {want:?}"));
                    }
                }
                let _ = std::fs::remove_dir_all(&dir);
            }

            // (8) MERGE loses a create race — another writer's commit lands in
            // the window between its empty match and its create — and
            // CONVERGES on that writer's node instead of surfacing the
            // uniqueness violation.
            {
                let g = std::sync::Arc::new(mk());
                let _ = run_on(&g, "CREATE CONSTRAINT rm_u FOR (n:RM) REQUIRE n.u IS UNIQUE");
                let mut rng = Rng::new(seed ^ 0x9ACE);
                let u = rng.below(1000) as i64;
                let hook: engram_graph::MergeRaceHook = std::sync::Arc::new(move |g: &Graph| {
                    let mut p = std::collections::BTreeMap::new();
                    p.insert("u".to_string(), Value::Int(u));
                    let _ = g.create_node(&["RM".to_string()], &p);
                });
                g.set_merge_race_hook_for_test(Some(hook));
                match run_on(&g, &format!("MERGE (n:RM {{u: {u}}}) RETURN n.u")) {
                    Ok(r) if r.rows == vec![vec![Value::Int(u)]] => {}
                    other => violations.push(format!("merge did not converge on the racer's node: {other:?}")),
                }
                match count_of(&g, "MATCH (n:RM) RETURN count(n)") {
                    Some(1) => {}
                    other => violations.push(format!("merge race left {other:?} nodes, want 1")),
                }
                g.set_merge_race_hook_for_test(None);
            }
        }

        // ── The replication scenario ────────────────────────────────────
        // A seeded primary shipped to a replica in seeded chunks with an
        // overlap, reads compared key by key; a tamper, a gap, and restore
        // verification — every declared replica event, per seed — plus a
        // PITR differential against get_at.
        {
            use engram_store::{
                Replica, RestoreVerdict, recover_to, replica::ApplyError, verify_restore,
            };
            let mut rng = Rng::new(seed ^ 0x8E9);
            let n = 5 + rng.below(8) as u8;
            let rp = prefix(1, Kind::NODE);
            let rprimary = Store::new();
            for i in 0..n {
                let _ = rprimary.put(&rp, &[i], StoredValue::Plain(vec![i, seed as u8]));
            }
            let entries = rprimary.log_tail(0);

            let mut replica = Replica::new();
            let cut = 2 + (rng.below(u64::from(n) - 2)) as usize;
            let overlap = 1 + (rng.below(cut as u64 - 1)) as usize;
            if replica.apply(&entries[0..cut]).is_err() {
                violations.push("replica refused the first chunk".into());
            }
            match replica.apply(&entries[cut - overlap..]) {
                Ok(r) if r.skipped == overlap => {}
                other => violations.push(format!("the overlap did not skip cleanly: {other:?}")),
            }
            for i in 0..n {
                if replica.store().get(&rp, &[i]) != rprimary.get(&rp, &[i]) {
                    violations.push(format!("replica diverged on key {i}"));
                }
            }
            // A gap refuses.
            let mut gapped = Replica::new();
            let _ = gapped.apply(&entries[0..1]);
            if !matches!(gapped.apply(&entries[2..]), Err(ApplyError::Gap { .. })) {
                violations.push("a gap did not refuse".into());
            }
            // A tamper refuses at its seq.
            let mut tampered = entries.clone();
            let victim = (rng.below(u64::from(n))) as usize;
            tampered[victim].payload[0] ^= 1;
            let mut tr = Replica::new();
            match tr.apply(&tampered) {
                Err(ApplyError::ChainMismatch { seq }) if seq == victim as u64 => {}
                other => violations.push(format!("tamper not refused at {victim}: {other:?}")),
            }
            // Restore: faithful verifies; a tampered source refuses.
            match Store::recover(&entries) {
                Ok(restored) => {
                    if !matches!(
                        verify_restore(&entries, &restored),
                        RestoreVerdict::Faithful { .. }
                    ) {
                        violations.push("a faithful restore did not verify".into());
                    }
                }
                Err(e) => violations.push(format!("restore failed: {e:?}")),
            }
            if !matches!(
                verify_restore(&tampered, replica.store()),
                RestoreVerdict::SourceBroken { .. }
            ) {
                violations.push("a tampered source verified".into());
            }
            // PITR differential: the recovered state at ts equals get_at.
            let mid_ts = 1 + rng.below(u64::from(n));
            match recover_to(&entries, mid_ts) {
                Ok(pitr) => {
                    for i in 0..n {
                        if pitr.get(&rp, &[i]) != rprimary.get_at(&rp, &[i], mid_ts) {
                            violations.push(format!(
                                "PITR diverged from get_at on key {i} at ts {mid_ts}"
                            ));
                        }
                    }
                }
                Err(e) => violations.push(format!("PITR failed: {e:?}")),
            }
        }

        // ── The wire scenario ───────────────────────────────────────────
        // A whole Bolt session per seed — the real driver's handshake bytes,
        // a seeded round trip, and every declared wire event.
        {
            use engram_bolt::{BoltServer, Decoder, Pack, WireError};
            use engram_cypher::Value;
            use engram_graph::Graph;
            let mut rng = Rng::new(seed ^ 0xB017);

            // The non-Bolt preamble refusal.
            let mut bad = BoltServer::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
            if !matches!(
                bad.feed(b"GET / HTTP/1.1\r\n\r\n"),
                Err(WireError::NotBolt { .. })
            ) {
                violations.push("an HTTP preamble was not refused".into());
            }

            // The three handshake shapes a server meets, so every declared
            // negotiation event fires each seed: a manifest VERSION we do not
            // speak (v2) passed over in favour of the legacy range behind it;
            // a manifest exchange whose client picks a version that was never
            // offered (refused); and the real session's exchange below, which
            // picks a seeded member of the offer.
            {
                let mut v2 = BoltServer::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
                let unknown_manifest: [u8; 20] = [
                    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x02, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00,
                    0x02, 0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
                ];
                match v2.feed(&unknown_manifest) {
                    Ok(r) if r == vec![0, 0, 8, 5] => {}
                    other => violations.push(format!("unknown manifest not passed over: {other:?}")),
                }
                let mut bad_pick = BoltServer::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
                let manifest: [u8; 20] = [
                    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00,
                    0x02, 0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
                ];
                let _ = bad_pick.feed(&manifest);
                if !matches!(
                    bad_pick.feed(&[0x00, 0x00, 0x04, 0x04, 0x00]),
                    Err(WireError::NoCommonVersion)
                ) {
                    violations.push("a pick outside the offer was not refused".into());
                }
            }

            // A real session through the manifest: the offer is answered,
            // then a seeded pick from inside it — 6.0 or one of 5.0..5.8.
            let mut srv = BoltServer::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
            let handshake: [u8; 20] = [
                0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02,
                0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
            ];
            let pick: (u8, u8) = if rng.below(2) == 0 { (6, 0) } else { (5, rng.below(9) as u8) };
            let msg = |tag: u8, fields: Vec<Pack>| -> Vec<u8> {
                let mut payload = Vec::new();
                engram_bolt::packstream::encode_struct(tag, &fields, &mut payload)
                    .expect("encodes");
                let mut out = (payload.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&payload);
                out.extend_from_slice(&[0, 0]);
                out
            };
            let empty_map = || Pack::Value(Value::Map(Default::default()));
            let sval = |s: String| Pack::Value(Value::Str(s));
            let reply_tags = |bytes: &[u8]| -> Vec<u8> {
                let mut tags = Vec::new();
                let mut at = 0usize;
                let mut payload: Vec<u8> = Vec::new();
                while at + 2 <= bytes.len() {
                    let size =
                        u16::from_be_bytes(bytes[at..at + 2].try_into().expect("2")) as usize;
                    at += 2;
                    if size == 0 {
                        if !payload.is_empty() {
                            if let Ok(Pack::Struct { tag, .. }) = Decoder::new(&payload).decode() {
                                tags.push(tag);
                            }
                            payload.clear();
                        }
                        continue;
                    }
                    payload.extend_from_slice(&bytes[at..at + size]);
                    at += size;
                }
                tags
            };

            match srv.feed(&handshake) {
                Ok(r) if r.starts_with(&[0, 0, 1, 0xFF]) => {}
                other => violations.push(format!("manifest not offered: {other:?}")),
            }
            match srv.feed(&[0x00, 0x00, pick.1, pick.0, 0x00]) {
                Ok(r) if r.is_empty() && srv.version() == pick => {}
                other => violations.push(format!("pick {pick:?} not negotiated: {other:?}")),
            }
            let _ = srv.feed(&msg(0x01, vec![empty_map()]));
            if srv.version() >= (5, 1) {
                let _ = srv.feed(&msg(0x6A, vec![empty_map()]));
            }
            // A seeded value through RUN + PULL, decoded identical.
            let v = (rng.below(100_000) as i64) - 50_000;
            let mut bytes = msg(
                0x10,
                vec![sval(format!("RETURN {v} AS x")), empty_map(), empty_map()],
            );
            let mut pull = std::collections::BTreeMap::new();
            pull.insert("n".to_string(), Value::Int(-1));
            bytes.extend(msg(0x3F, vec![Pack::Value(Value::Map(pull))]));
            match srv.feed(&bytes) {
                Ok(r) => {
                    let tags = reply_tags(&r);
                    if tags != vec![0x70, 0x71, 0x70] {
                        violations.push(format!("RUN+PULL replied {tags:?}"));
                    }
                }
                Err(e) => violations.push(format!("RUN+PULL refused: {e}")),
            }
            // A failure, an IGNORED message, then RESET; a BEGIN + ROLLBACK now
            // SUCCEEDS (session-owned transactions) rather than being refused.
            let _ = srv.feed(&msg(
                0x10,
                vec![sval("MATCH (".to_string()), empty_map(), empty_map()],
            ));
            match srv.feed(&msg(
                0x10,
                vec![sval("RETURN 1".to_string()), empty_map(), empty_map()],
            )) {
                Ok(r) if reply_tags(&r) == vec![0x7E] => {}
                other => violations.push(format!("failed state did not IGNORE: {other:?}")),
            }
            let _ = srv.feed(&msg(0x0F, vec![]));
            let _ = srv.feed(&msg(0x11, vec![empty_map()]));
            match srv.feed(&msg(0x13, vec![])) {
                Ok(r) if reply_tags(&r) == vec![0x70] => {}
                other => violations.push(format!("rollback did not succeed: {other:?}")),
            }
        }

        // ── The overlay refusals ────────────────────────────────────────
        // Every declared overlay event, exercised each seed: the floor caught
        // these as never-fired the moment they were declared, because the
        // workload never touched the overlay layer. Each refusal is also an
        // ASSERTION — a refusal that stops refusing is a violation here, not
        // just a coverage gap.
        {
            use engram_store::{
                NamespaceRegistry, NamespaceRole, OverlayError, TenantSession, system_put,
            };
            let ostore = Store::new();
            let mut reg = NamespaceRegistry::new();
            let sys = Namespace(1);
            let ns_a = Namespace(100);
            let ns_b = Namespace(200);
            let _ = reg.declare(sys, NamespaceRole::System);
            let _ = reg.declare(ns_a, NamespaceRole::TenantOverlay(Realm(10)));
            let _ = reg.declare(ns_b, NamespaceRole::TenantOverlay(Realm(20)));
            let cap = reg.mint_system_write();
            if cap.is_none() {
                violations.push("first system-cap mint failed".into());
            }
            if reg.mint_system_write().is_some() {
                violations.push("second system-cap mint succeeded".into());
            }
            if let Some(cap) = &cap {
                let _ = system_put(
                    &ostore,
                    &reg,
                    cap,
                    engram_store::overlay::SystemTarget {
                        ns: sys,
                        kind: Kind::NODE,
                        partition: Partition(1),
                    },
                    b"shared",
                    StoredValue::Plain(vec![1]),
                );
            }
            let a = TenantSession::new(&ostore, &reg, Realm(10));
            if !matches!(
                a.get(ns_b, Kind::NODE, Partition(1), b"x"),
                Err(OverlayError::ForeignOverlay { .. })
            ) {
                violations.push("a foreign overlay read was not refused".into());
            }
            if !matches!(
                a.put(
                    sys,
                    Kind::NODE,
                    Partition(1),
                    b"shared",
                    StoredValue::Plain(vec![0])
                ),
                Err(OverlayError::SystemWriteRequiresCap(_))
            ) {
                violations.push("a tenant write to the system corpus was not refused".into());
            }
            if !matches!(
                a.get(Namespace(9999), Kind::NODE, Partition(1), b"x"),
                Err(OverlayError::UndeclaredNamespace(_))
            ) {
                violations.push("an undeclared namespace was not refused".into());
            }
            if a.get(sys, Kind::NODE, Partition(1), b"shared") != Ok(Some(vec![1])) {
                violations.push("the shared system read did not resolve".into());
            }
        }

        // ── Invariants every seed must satisfy ──────────────────────────
        let entries = store.log_tail(0);

        // 1. The chain verifies.
        if !matches!(
            engram_log::CommitLog::verify_entries(&entries),
            engram_log::ChainVerify::Intact { .. }
        ) {
            violations.push("commit log chain did not verify".into());
        }

        // 2. Recovery reproduces the store: same head, same current reads.
        match Store::recover(&entries) {
            Err(e) => violations.push(format!("recovery refused its own log: {e}")),
            Ok(recovered) => {
                if recovered.log_head() != store.log_head() {
                    violations.push("recovered log head diverged".into());
                }
                for key in 0..cfg.key_space {
                    let p = prefix(1, Kind::NODE);
                    let k = [key as u8];
                    if recovered.get(&p, &k) != store.get(&p, &k) {
                        violations.push(format!("recovery answered differently for key {key}"));
                    }
                }
            }
        }

        // 3. Compaction after the fact changes no current read.
        {
            let before: Vec<_> = (0..cfg.key_space)
                .map(|k| store.get(&prefix(1, Kind::NODE), &[k as u8]))
                .collect();
            store.seal();
            store.compact();
            let after: Vec<_> = (0..cfg.key_space)
                .map(|k| store.get(&prefix(1, Kind::NODE), &[k as u8]))
                .collect();
            if before != after {
                violations.push("compaction changed a current read".into());
            }
        }

        // 4. Optional crash scenario: kill at the WAL boundary, recover, compare.
        if cfg.inject_crash {
            let fresh = Store::new();
            fresh
                .put(&prefix(2, Kind::NODE), b"pre", StoredValue::Plain(vec![1]))
                .expect("pre");
            let crashed = with_crash_at("store.between_log_and_publish", || {
                let _ = fresh.put(&prefix(2, Kind::NODE), b"mid", StoredValue::Plain(vec![2]));
            });
            if crashed.is_ok() {
                violations.push("armed crash point never fired".into());
            }
            match Store::recover(&fresh.log_tail(0)) {
                Err(e) => violations.push(format!("post-crash recovery failed: {e}")),
                Ok(r) => {
                    if r.get(&prefix(2, Kind::NODE), b"mid") != Some(vec![2]) {
                        violations.push("the logged write was not redone after the crash".into());
                    }
                }
            }
        }

        // 5. Optional tamper scenarios: a flipped byte and a dropped entry in
        //    COPIES must both be detected — the two ChainVerify failure arms.
        if cfg.tamper_check && entries.len() >= 2 {
            let mut flipped = entries.clone();
            let idx = (seed as usize) % flipped.len();
            if let Some(b) = flipped[idx].payload.first_mut() {
                *b ^= 0x40;
            }
            if matches!(
                engram_log::CommitLog::verify_entries(&flipped),
                engram_log::ChainVerify::Intact { .. }
            ) {
                violations.push("a tampered log verified as intact".into());
            }

            let mut gapped = entries.clone();
            gapped.remove((seed as usize) % gapped.len());
            if matches!(
                engram_log::CommitLog::verify_entries(&gapped),
                engram_log::ChainVerify::Intact { .. }
            ) {
                violations.push("a gapped log verified as intact".into());
            }
        }

        violations
    });

    RunReport {
        seed,
        config: outer_config,
        trace,
        violations,
    }
}

/// Every `sometimes!` event the swept subsystems declare.
pub fn declared_events() -> Vec<engram_observe::SometimesEvent> {
    let mut out = Vec::new();
    out.extend(Store::register().sometimes_events().to_vec());
    out.extend(
        engram_log::CommitLog::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(Shard::register().sometimes_events().to_vec());
    out.extend(engram_key::KeyCodec::register().sometimes_events().to_vec());
    out.extend(
        engram_exec::ExecOperators::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_crypto::CryptoSeam::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_objstore::ObjectStorage::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_blob::BlobLayer::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_cypher::CypherFrontend::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_graph::GraphLayer::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_bolt::BoltWire::register()
            .sometimes_events()
            .to_vec(),
    );
    out.extend(
        engram_store::replica::Replication::register()
            .sometimes_events()
            .to_vec(),
    );
    out
}

/// The sweep: run `seeds` consecutive seeds from `base`, fail on any per-seed
/// violation, and enforce the coverage floor across the whole sweep.
pub fn sweep(base: u64, seeds: u64) -> Result<SweepReport, SweepFailure> {
    sweep_runs(base, seeds, run_seed, &declared_events())
}

/// The sweep's decision logic, parametrized over the runner.
///
/// Public so its FAILURE branches are testable with fabricated reports. On a
/// healthy corpus none of them execute, so canaries that disabled them came
/// back NOT DETECTED — enforcement whose only exercise is a healthy input is
/// enforcement nobody has watched fire, and this seam is what lets the tests
/// hand it unhealthy input on purpose.
pub fn sweep_runs(
    base: u64,
    seeds: u64,
    mut run: impl FnMut(u64) -> RunReport,
    declared: &[engram_observe::SometimesEvent],
) -> Result<SweepReport, SweepFailure> {
    let mut traces = Vec::new();
    let mut failures = Vec::new();
    for seed in base..base + seeds {
        let report = run(seed);
        // A trace violation (an `always!` broken, an `unreachable!` reached)
        // is a seed failure exactly like an invariant string — merged HERE so
        // the merge itself sits in testable code rather than in the runner.
        let mut violations = report.violations.clone();
        for v in report.trace.violations() {
            violations.push(format!("trace violation: {}", v.name));
        }
        if !violations.is_empty() {
            failures.push((seed, report.config.clone(), violations));
        }
        traces.push(report.trace);
    }
    if !failures.is_empty() {
        return Err(SweepFailure::SeedViolations(failures));
    }

    let coverage = engram_observe::coverage(declared.iter(), traces.iter());
    if !coverage.is_covered() {
        // The floor. Never-fired is a FAILURE, not a note: a simulation that
        // never reaches a state proves nothing about behaviour in it, and the
        // report would otherwise read as a clean bill for territory never
        // visited.
        return Err(SweepFailure::CoverageFloor {
            never_fired: coverage.never_fired,
        });
    }
    Ok(SweepReport {
        seeds,
        hit: coverage.hit,
        undeclared: coverage.undeclared,
    })
}

/// A passing sweep's summary.
#[derive(Debug)]
pub struct SweepReport {
    /// Seeds run.
    pub seeds: u64,
    /// Declared events observed.
    pub hit: Vec<String>,
    /// Observed events nobody declared — outside the floor, reported so the
    /// declared set cannot quietly fall behind the code.
    pub undeclared: Vec<String>,
}

/// Why a sweep failed.
#[derive(Debug)]
pub enum SweepFailure {
    /// Seeds with invariant violations: (seed, config, violations).
    SeedViolations(Vec<(u64, Config, Vec<String>)>),
    /// Declared events that never fired across the whole sweep.
    CoverageFloor {
        /// The gap.
        never_fired: Vec<String>,
    },
}
