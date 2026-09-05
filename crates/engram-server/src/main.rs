//! `engram-server` — a Bolt listener over one engine shard.
//!
//! # Durability
//!
//! With `--data-dir DIR` the store is WAL-backed: every acknowledged write is
//! `fsync`'d before the acknowledgement, and a restart replays the log. Without
//! it the store is in-memory and **a restart loses everything** — which is a
//! legitimate mode for tests and for shadow-read comparison runs, but is a
//! footgun as a silent default, so it is announced loudly at startup.
//!
//! The engine has had a durable WAL and eight recovery tests for a long time;
//! what did not exist was any way for an operator to reach it. `run_server`
//! already took a `make_store` closure, so this is the closure finally being
//! given something other than `Store::new()`.

use std::net::TcpListener;
use std::path::PathBuf;

// Thread-caching allocator on musl: the multi-worker server would otherwise
// serialise concurrent allocation-heavy queries on the single-arena system
// allocator's lock (see this crate's Cargo.toml for the measurements). musl-only;
// native builds keep the system allocator the determinism baselines run on.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use engram_key::{Namespace, Realm};
use engram_server::ServerConfig;
use engram_store::Store;

const USAGE: &str = "\
engram-server — a Bolt-protocol graph database server

USAGE:
    engram-server [ADDR] [OPTIONS]

ARGS:
    ADDR                    Listen address           [default: 127.0.0.1:7687]

OPTIONS:
    -d, --data-dir DIR      Store data durably in DIR. Without this the server
                            is IN-MEMORY and a restart loses everything.
        --paged-dir DIR     Serve PAGED from seg files in DIR (bigger-than-RAM):
                            sealed segments spill to disk; durability is at seal
                            boundaries ONLY — a crash LOSES the unsealed tail.
                            Mutually exclusive with --data-dir.
        --paged-cache-mb N  Block-cache budget for --paged-dir, MiB [default: 4096]
        --workers N         Engine worker threads    [default: 1]
        --max-connections N Concurrent connections   [default: 512]
        --row-budget N      Max rows one query may materialise, 0 = unlimited
                            [default: 20000000]
        --read-timeout-secs N  Reap a connection quiet for N seconds, 0 = never.
                            A client waiting on a long analytic query is quiet —
                            raise or disable this to serve queries past 5 min
                            [default: 300]
        --adj-overlay-fold N  Overlay rows a repaired adjacency table may
                            carry before folding [default: 4096]. `slice` is
                            the hottest read in the engine and pays a BTreeMap
                            descent per hop while an overlay is present; 0
                            folds every repair.
        --degree-table-after N  Direct adjacency probes tolerated in one
                            epoch before a table may be BUILT [default: 1024].
                            The counter resets on the GLOBAL adjacency epoch, so
                            under a write stream it may never reach N and a
                            table for an untouched type is never built. 0 admits
                            immediately — the A/B arm.
        --no-property-seek  An anchored MATCH scans its label instead of
                            seeking a property range index. A/B arm for the
                            index-churn interference on mixed profiles.
        --no-label-scoped-indexes  A property index covers the whole partition.
        --no-lazy-stale-serve  A single-node reader REPAIRS the whole change
                            set instead of asking whether its own node moved.
                            A/B arm for the mixed-profile interference (§8).
        --no-adj-change-filter  Answer that per-node question under the change
                            log's lock instead of with one atomic load.
        --no-single-node-stale-walk  A reader whose node moved repairs the
                            table rather than walking its own span. A/B arm;
                            the ON default trades O(change set) for O(degree),
                            so this is the arm for a high-degree corpus.
        --single-flight-repair  Readers queue on the build guard so a stale
                            table is repaired once between them. Measured 40%
                            SLOWER; present as the control, not a setting.
        --members-bitmap-after N  Base probes before a membership base is
                            answered from a presence bitmap [default: 4096].
                            0 never builds one.
        --no-hop-membership-contains  A hop's label filter materialises the
                            whole label per published snapshot and binary-
                            searches it, instead of asking the membership view.
                            A/B arm.
        --no-hop-count-memo  Every labelled cardinality estimate walks the
                            smaller label again — 2M nodes for (:Comment) at
                            SF1, ~16 walks per LSQB q2 statement. A/B arm.
        --no-agg-topk       An ORDER BY + LIMIT over groups projects EVERY
                            group and then truncates. A/B arm.
        --no-const-projection-fold  `MATCH … RETURN <constants> [LIMIT]`
                            enumerates the pattern as written. A/B arm.
        --no-directed-bound-probe  A directed fold close reads the level var
                            row — a different CSR line every call. A/B arm.
        --no-adj-snap-memo  Every adjacency probe rebuilds its (tag, types)
                            map key (a heap allocation for a typed hop) and
                            walks the table map, once per row. A/B arm.
        --no-order-peak-search  The count-only reorder keeps its greedy, which
                            scores only the immediate step. A/B arm.
        --no-derived-refresh  Do NOT refresh derived structures from the
                            maintenance thread; the next reader rebuilds instead.
                            A/B arm for the write-stall this refresh can cause.
        --refresh-after-writes N  Commit-clock STAMPS between refreshes (a Bolt
                            write statement is ~3), 0 = tick only [default: 8192]
        --maintenance-tick-secs N  Maintenance thread tick [default: 5]
        --refresh-pass-rows N  Rows ONE refresh pass may re-read before
                            deferring the rest, 0 = unbounded [default: 250000]
        --no-group-commit   fsync once per WRITE instead of once per batch of
                            requests. Slower under concurrent writers; exists
                            for A/B measurement, not for production.
        --id-reservation N  Ids a session reserves per counter write, 0/1 = one
                            durable counter write per entity. The allocator
                            holds a global mutex across that write, so a
                            reservation removes it from N-1 of every N
                            allocations. Ids stay dense within a run; a restart
                            abandons the unused tail as a gap [default: 256]
        --keep-full-log     Retain the whole in-memory commit log instead of
                            releasing it at a seal. ~150 B per version and
                            grows with the corpus (the term that put a paged
                            SF1 load at ~17 GB). Needed only by a log_tail
                            (CDC/replication) consumer.
        --no-guard-exemption  Make two relationship writes touching ONE node
                            abort each other again (they PUT the same guard
                            row). A/B arm for RC1, which is worth 3.7x on the
                            shared-endpoint shape. Not a production setting.
        --no-constraint-epoch-cache  Re-probe the schema-epoch key on every
                            constrained write instead of registering it from a
                            cache hit. The key is ABSENT until the first
                            constraint DDL and the sparse index cannot reject
                            it, so the probe descends every sealed segment.
                            A/B arm; not a production setting.
        --seal-after N      Seal the write tail into an immutable, lock-free
                            segment once it holds N versions [default: 65536]
        --compact-after N   Compact the sealed segments into one once there
                            are N of them (on a maintenance thread) [default: 8]
        --no-tail-copyout   Restore the old span-read path, which holds every
                            tail shard latch for the whole merge and so
                            EXCLUDES every writer for its duration. The A/B arm
                            for the copy-out; on by default.
        --precision-locking Validate each transaction's node-pattern PREDICATES
                            against the rows committed since its snapshot,
                            closing phantoms (S7). An isolation UPGRADE and a
                            behaviour change: it aborts statements that
                            currently commit. Off by default.
        --compact-every S   PAGED only: never go longer than S seconds between
                            full compactions while more than one segment
                            exists. Off by default. A paged compaction EMITS
                            the adjacency CSRs and membership bases (S5.2), so
                            this puts a floor under how often those refresh
                            that does not depend on write volume.
        --bulk-ingest       Serve in BULK-INGEST mode for a corpus load: writes
                            skip the commit log (durability by re-ingest, not by
                            replay) and ids reserve in ranges. Not with
                            --data-dir. Restart without it to serve normally.
    -h, --help              Print this help
    -V, --version           Print version

ENVIRONMENT:
    ENGRAM_SERVER_WORKERS   Default for --workers

SECURITY:
    This server has NO AUTHENTICATION and NO TLS. Do not bind it to a public
    interface. See SECURITY.md.
";

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("engram-server {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // A hand-rolled parser, deliberately: this crate has no dependencies today
    // and the flag set is small. When the full configuration surface lands
    // (config file, precedence, completions) it brings an argument parser with
    // it — adding one for five flags would be the wrong trade now, and adding
    // it later is not a breaking change.
    let value_of = |names: [&str; 2]| -> Option<String> {
        args.iter()
            .position(|a| a == names[0] || a == names[1])
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let num_of =
        |names: [&str; 2]| -> Option<usize> { value_of(names).and_then(|v| v.parse().ok()) };

    let addr = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:7687".to_string());
    let data_dir: Option<PathBuf> = value_of(["-d", "--data-dir"]).map(PathBuf::from);
    let paged_dir: Option<PathBuf> = value_of(["--paged-dir", "--paged-dir"]).map(PathBuf::from);
    let paged_cache_mb = num_of(["--paged-cache-mb", "--paged-cache-mb"]).unwrap_or(4096);
    if data_dir.is_some() && paged_dir.is_some() {
        // Two durability contracts cannot hold over one store: the WAL dir
        // fsyncs every acknowledged write; the paged dir persists only at seal
        // boundaries. A server claiming both would honour neither.
        eprintln!("[engram-server] --data-dir and --paged-dir are mutually exclusive: pick one.");
        std::process::exit(1);
    }

    let mut cfg = ServerConfig::from_env();
    if let Some(w) = num_of(["--workers", "--workers"]) {
        cfg.workers = w.max(1);
    }
    if let Some(m) = num_of(["--max-connections", "--max-connections"]) {
        cfg.max_connections = m.max(1);
    }
    if let Some(b) = num_of(["--row-budget", "--row-budget"]) {
        // 0 means unlimited, which is what the engine's `None` means. Spelling
        // it as a number keeps the flag one type.
        cfg.row_budget = if b == 0 { None } else { Some(b) };
    }
    if let Some(secs) = num_of(["--read-timeout-secs", "--read-timeout-secs"]) {
        // The read timeout reaps a socket that has SENT nothing — which is
        // exactly what a client waiting on a long analytic query looks like.
        // The default (300 s) silently killed every LSQB query past five
        // minutes: the client saw EOF mid-response ("failed to fill whole
        // buffer") and nothing was logged server-side. 0 disables.
        cfg.read_timeout = if secs == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(secs as u64))
        };
    }
    if let Some(n) = num_of(["--seal-after", "--seal-after"]) {
        cfg.seal_after_versions = n.max(1);
    }
    if let Some(n) = num_of(["--compact-after", "--compact-after"]) {
        cfg.compact_after_segments = n.max(2);
    }
    if args.iter().any(|a| a == "--no-tail-copyout") {
        cfg.tail_span_copyout = false;
        eprintln!(
            "[engram-server] tail span copy-out OFF: span reads hold every tail              shard latch for the whole merge, excluding writers (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-lazy-stale-serve") {
        cfg.lazy_stale_serve = false;
        eprintln!(
            "[engram-server] lazy stale serve OFF: a single-node reader repairs the whole change set rather than asking whether its own node moved (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-adj-change-filter") {
        cfg.adj_change_filter = false;
        eprintln!(
            "[engram-server] adjacency change filter OFF: the per-node staleness question goes under the change log's lock (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-single-node-stale-walk") {
        cfg.single_node_stale_walk = false;
        eprintln!(
            "[engram-server] single-node stale walk OFF: a reader whose node moved repairs the table instead of walking its own span (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--single-flight-repair") {
        cfg.single_flight_repair = true;
        eprintln!(
            "[engram-server] single-flight repair ON: readers queue on the build guard to repair once between them — MEASURED 40% SLOWER, kept as a control"
        );
    }
    if args.iter().any(|a| a == "--no-hop-membership-contains") {
        cfg.hop_membership_contains = false;
        eprintln!(
            "[engram-server] hop membership contains OFF: a hop's label filter materialises the whole label per published snapshot, then binary-searches it (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-hop-count-memo") {
        cfg.hop_count_memo = false;
        eprintln!(
            "[engram-server] hop-count memo OFF: every labelled cardinality estimate walks the smaller label again — 2M nodes for (:Comment) at SF1, ~16 walks per LSQB q2 statement (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-agg-topk") {
        cfg.agg_topk_before_project = false;
        eprintln!(
            "[engram-server] aggregate top-k-before-projection OFF: an ORDER BY + LIMIT over groups projects EVERY group, then truncates (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-const-projection-fold") {
        cfg.const_projection_fold = false;
        eprintln!(
            "[engram-server] constant-projection-over-count OFF: `MATCH … RETURN <constants> [LIMIT]` enumerates the pattern as written (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-directed-bound-probe") {
        cfg.directed_bound_probe = false;
        eprintln!(
            "[engram-server] directed bound-side probe OFF: a directed fold close reads the level var row, a different CSR line every call (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-adj-snap-memo") {
        cfg.adj_snap_memo = false;
        eprintln!(
            "[engram-server] adjacency snapshot memo OFF: every probe rebuilds its (tag, types) map key — a heap allocation for a typed hop — and walks the table map, once per row (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-order-peak-search") {
        cfg.order_peak_search = false;
        eprintln!(
            "[engram-server] ordering peak search OFF: the count-only reorder keeps its greedy, which scores the immediate step (the A/B arm)"
        );
    }
    if let Some(n) = num_of(["--members-bitmap-after", "--members-bitmap-after"]) {
        cfg.members_bitmap_after = n;
        eprintln!(
            "[engram-server] membership base answered from a presence bitmap after {n} probes (0 = never)"
        );
    }
    if let Some(n) = num_of(["--adj-overlay-fold", "--adj-overlay-fold"]) {
        cfg.adj_overlay_fold = n;
        eprintln!("[engram-server] adjacency overlay folds past {n} rows (0 = every repair)");
    }
    if let Some(n) = num_of(["--degree-table-after", "--degree-table-after"]) {
        cfg.degree_table_after = n as u64;
        eprintln!(
            "[engram-server] degree/adjacency table admission after {n} probes per epoch (0 = admit immediately)"
        );
    }
    if args.iter().any(|a| a == "--no-property-seek") {
        cfg.property_seek = false;
        eprintln!(
            "[engram-server] property seek OFF: an anchored MATCH scans its label instead of seeking a range index (the A/B arm for index-churn interference)"
        );
    }
    if args.iter().any(|a| a == "--no-label-scoped-indexes") {
        cfg.label_scoped_indexes = false;
        eprintln!(
            "[engram-server] label-scoped indexes OFF: a property index covers the whole partition (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--precision-locking") {
        cfg.precision_locking = true;
        eprintln!(
            "[engram-server] precision locking ON: phantoms are closed, and              statements that would previously have committed over one now abort              and retry"
        );
    }
    if let Some(n) = num_of(["--compact-every", "--compact-every"]) {
        // Every lever gets a flag the day it lands. `set_guard_put_put_exempt`
        // was worth 3.7x on rel-hub and shipped with none, which is how a
        // mechanism that can cost 3x of write throughput stayed out of an
        // operator's reach through a whole measurement campaign.
        cfg.compact_max_interval = Some(std::time::Duration::from_secs(n.max(1) as u64));
        eprintln!("[engram-server] paged compaction cadence floor: {n}s");
    }
    if args.iter().any(|a| a == "--no-derived-refresh") {
        // The A/B arm for the maintenance refresh. It shipped with no way to
        // reach it from an operator's hands, which is how a 2-3x write
        // regression reached a measurement unnoticed: the refresh runs every
        // `refresh_after_writes` STAMPS (~2,700 Bolt statements, ~0.5 s under
        // load) and each pass repairs derived structures over the whole
        // corpus, so on a large store it stalls the writers it is meant to
        // spare. Off, the next reader pays the rebuild instead.
        cfg.derived_refresh = false;
        eprintln!(
            "[engram-server] derived refresh OFF: readers pay their own rebuild —              this is the A/B baseline, not a production setting"
        );
    }
    if let Some(n) = num_of(["--refresh-after-writes", "--refresh-after-writes"]) {
        // Commit-clock STAMPS, not statements (a Bolt write statement is ~3).
        // 0 means refresh on the tick only.
        cfg.refresh_after_writes = n as u64;
    }
    if let Some(n) = num_of(["--refresh-pass-rows", "--refresh-pass-rows"]) {
        // 0 = unbounded, the pre-budget behaviour.
        cfg.refresh_pass_rows = n;
    }
    if let Some(n) = num_of(["--maintenance-tick-secs", "--maintenance-tick-secs"]) {
        cfg.maintenance_tick = std::time::Duration::from_secs((n as u64).max(1));
    }
    if let Some(n) = num_of(["--id-reservation", "--id-reservation"]) {
        // 0 or 1 = one LOGGED counter write per entity (the pre-reservation
        // behaviour and the A/B arm). Larger reserves a range, so `alloc` is
        // held across a durable put once per N ids instead of once per id.
        cfg.id_reservation = n;
    }
    if args.iter().any(|a| a == "--keep-full-log") {
        // The in-memory commit log is retained for the process lifetime unless
        // the maintenance thread releases it at a seal. Keep it whole when a
        // pull-style `Store::log_tail` consumer (CDC, replication) needs the
        // history, since the server cannot know such a consumer's position.
        cfg.truncate_log_at_seal = false;
        eprintln!(
            "[engram-server] in-memory commit log RETAINED in full: ~150 B per \
             version, growing with the corpus — required only for a log_tail \
             consumer"
        );
    }
    if args.iter().any(|a| a == "--no-guard-exemption") {
        // The A/B arm for RC1. Worth 3.7x on the `rel-hub` shape (1,425 ->
        // 16,539 across 1->8 clients, where it had been 0.76x — going
        // BACKWARDS), and until now it was reachable only from a test. A
        // mechanism that large with no operator-facing switch is how the
        // derived-refresh regression stayed invisible: nobody could turn it
        // off to see what it cost.
        cfg.guard_put_put_exempt = false;
        eprintln!(
            "[engram-server] guard put-vs-put exemption OFF: two relationship \
             writes touching one node abort each other — this is the A/B \
             baseline, not a production setting"
        );
    }
    if args.iter().any(|a| a == "--no-constraint-epoch-cache") {
        cfg.constraint_epoch_cache = false;
        eprintln!(
            "[engram-server] constraint epoch cache OFF: every constrained \
             write re-probes an always-absent KV key across every sealed \
             segment — this is the A/B baseline, not a production setting"
        );
    }
    if args.iter().any(|a| a == "--no-group-commit") {
        cfg.group_commit = false;
        eprintln!(
            "[engram-server] group commit OFF: one fsync per write — this is the \
             A/B baseline, not a production setting"
        );
    }
    if args.iter().any(|a| a == "--bulk-ingest") {
        if data_dir.is_some() {
            // The WAL's contract is that replay restores every acknowledged
            // write; bulk writes never reach the log, so a WAL directory
            // served in bulk mode would replay to a partial database.
            eprintln!("[engram-server] --bulk-ingest cannot be combined with --data-dir.");
            std::process::exit(1);
        }
        // The commit log is retained in memory for the process lifetime and
        // never truncated by the server; under a load it is ~150 B per
        // version and grows with the corpus — the term that put a paged SF1
        // load at ~17 GB. Bulk mode writes through `put_unlogged`, so the
        // log never holds the corpus. It must go through the resolver's
        // hook: the serving graph is built there, not in `make_store`.
        //
        // Serialisable autocommit is switched off with it: that path runs
        // every write statement inside a store transaction whose commit
        // appends to the log regardless of the graph's bulk flag, so with
        // it on the flag would change nothing over Bolt — a loader is one
        // client, and the OCC re-run exists for concurrent hot-key writers.
        cfg.configure_graph = Some(std::sync::Arc::new(|g: &engram_graph::Graph| {
            g.set_bulk_ingest(true)
                .expect("entering bulk-ingest mode has no failure path");
            g.set_serialisable_autocommit(false);
        }));
        eprintln!(
            "[engram-server] BULK INGEST ON: writes skip the commit log (durability by \
             re-ingest, NOT by replay), ids reserve in ranges of 4096, autocommit is not \
             serialisable. Restart without --bulk-ingest to serve normally."
        );
    }

    // LOCK THE DATA DIRECTORY FIRST — before the port is bound.
    //
    // Two servers on one data directory both append to the same WAL and
    // interleave their records, leaving a hash chain no recovery can verify,
    // after both have already acknowledged writes.
    //
    // Before the BIND, not merely before serving: a second server that binds
    // and then refuses has, for that moment, taken the port from the one that
    // legitimately holds the data — which during a restart race is exactly when
    // it happens, and turns a clean refusal into an outage.
    //
    // Held for the process lifetime. Bound to `_lock`, never `_`: `_` drops it
    // immediately and locks nothing.
    //
    // The paged dir is locked for the same reasons: two spillers write the
    // same `seg-<seq>.seg` names over each other's files.
    let _lock = match data_dir.as_ref().or(paged_dir.as_ref()) {
        Some(dir) => match engram_store::dirlock::DirLock::acquire(
            dir,
            &format!("pid {}", std::process::id()),
        ) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("[engram-server] {e}");
                std::process::exit(1);
            }
        },
        // No data directory means no durable state to protect.
        None => None,
    };

    let listener = TcpListener::bind(&addr)?;
    eprintln!(
        "[engram-server] listening on bolt://{}",
        listener.local_addr()?
    );

    if let Some(dir) = paged_dir {
        // `open_paged_dir` read_dirs the directory, so it must exist. The lock
        // above already created it, but the open must not depend on lock order.
        std::fs::create_dir_all(&dir)?;
        // Opened HERE, not in `make_store`: the server needs the SAME cache
        // handle the store reads through, so every later spill shares the one
        // budget rather than minting its own.
        let (store, cache) = match Store::open_paged_dir(&dir, paged_cache_mb << 20) {
            Ok(v) => v,
            Err(e) => panic!(
                "cannot open the paged directory: {e}\n\
                 Refusing to start empty over a paged directory that was requested — \
                 starting empty here would look like an empty database rather than a \
                 failed open."
            ),
        };
        eprintln!(
            "[engram-server] paged: {} — cache {paged_cache_mb} MiB, {} segment(s) on disk. \
             Durability at seal boundaries — the unsealed tail is LOST on crash; not the \
             durable mode.",
            dir.display(),
            store.segment_count()
        );
        cfg.paged_dir = Some(dir);
        cfg.paged_spill_cache = Some(cache);
        return engram_server::run_server_with_config(
            listener,
            move || (store, Realm(1), Namespace(1)),
            cfg,
        );
    }

    match data_dir {
        Some(dir) => {
            // The directory is already locked, above, before the bind.
            let wal = dir.join("engram.wal");
            eprintln!("[engram-server] durable: {}", wal.display());
            // The store is built ON the engine thread (it is deliberately not
            // `Send`), so the open — and any refusal — happens there. A refusal
            // must take the process down rather than silently fall back to
            // memory: "your data directory was unreadable so we started
            // empty" is how a restore gets overwritten.
            engram_server::run_server_with_config(
                listener,
                move || match Store::open_wal(&wal) {
                    Ok(s) => (s, Realm(1), Namespace(1)),
                    Err(e) => panic!(
                        "cannot open the data directory: {e}\n\
                         Refusing to start in-memory over a data directory that was requested — \
                         starting empty here would look like an empty database rather than a \
                         failed open."
                    ),
                },
                cfg,
            )
        }
        None => {
            eprintln!(
                "[engram-server] WARNING: in-memory only — a restart LOSES ALL DATA. \
                 Pass --data-dir DIR for durability."
            );
            engram_server::run_server_with_config(
                listener,
                || (Store::new(), Realm(1), Namespace(1)),
                cfg,
            )
        }
    }
}
