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
                            sealed segments spill to disk, and DIR/engram.wal
                            fronts the unsealed tail — every acknowledged write
                            is fsync'd before it is acknowledged and replayed on
                            open, exactly as under --data-dir; a spill checkpoints
                            the WAL behind the segments it wrote. Mutually
                            exclusive with --data-dir (two layouts, one durability).
        --paged-cache-mb N  Block-cache budget for --paged-dir, MiB [default: 4096]
        --workers N         Engine worker threads    [default: 1]
        --max-connections N Concurrent connections   [default: 512]
        --row-budget N      Max rows one query may materialise, 0 = unlimited
                            [default: derived from this process's memory limit
                            (cgroup, else MemTotal): ceiling / 4 / 96 B per row,
                            clamped to 1M..4G. Printed at startup. An explicit
                            value wins and is what a reproducible run should pin]
        --read-timeout-secs N  Reap a connection quiet for N seconds, 0 = never.
                            A client waiting on a long analytic query is quiet —
                            raise or disable this to serve queries past 5 min
                            [default: 300]
        --adj-overlay-fold N  Overlay rows a repaired adjacency table may
                            carry before folding [default: 4096]. `slice` is
                            the hottest read in the engine and pays a BTreeMap
                            descent per hop while an overlay is present; 0
                            folds every repair.
        --memory-max-mb N   Resident memory ceiling. Above 90% of it new
                            statements QUEUE (and are refused only after
                            waiting 30 s); below 80% they are admitted again.
                            0 disables the ceiling entirely, leaving the OOM
                            killer as the only limit. Default: the container's
                            own memory limit, so the process uses the machine
                            it was given.
        --degree-table-after N  Direct adjacency probes tolerated in one
                            epoch before a table may be BUILT [default: 1024].
                            The counter resets on the GLOBAL adjacency epoch, so
                            under a write stream it may never reach N and a
                            table for an untouched type is never built. 0 admits
                            immediately — the A/B arm.
        --no-algo-parallel  A graph algorithm runs its fixpoint on one thread
                            even when ENGRAM_QUERY_PARALLELISM installed a
                            pool. The scores are bit-identical either way; this
                            measures the split's cost.
        --no-trigram-indexes
                            `=~`, CONTAINS, STARTS WITH and ENDS WITH scan the
                            label instead of seeking a declared trigram index.
        --no-bm25           A BM25 fulltext index is answered by a scan rather
                            than by its term index. The SCORES are unchanged;
                            only the path is.
        --no-bm25-by-default
                            A newly created fulltext index is stamped for
                            term-frequency scoring rather than BM25. Existing
                            indexes are unaffected either way.
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
        --no-subquery-end-gather  A label past the whole-label read ceiling
                            falls to one projected record read per subquery
                            hop end, as before fix 121. A/B arm.
        --whole-label-read-max N  The label size past which a whole-label
                            column read is declined (fix 118's ceiling)
                            [default: 262144]. 0 keeps the built-in.
        --match-start-chunk N  How many start candidates a writing
                            statement's matcher carries through a path's
                            hops at once, testing its WHERE as rows finish
                            [default: 4096]. 0 is the A/B arm: every
                            candidate at once, the WHERE after collection.
        --no-split-maintenance  The derived refresh runs at the tail of the
                            storage thread's loop instead of on its own
                            thread, so it cannot start until a spill or a
                            compaction returns. A/B arm: on the pod, with 99
                            segments on disk, that froze refresh_runs at 20
                            for a whole 70 s window.
        --range-fold-at N   Overlay size past which a range-index catch-up
                            folds. A READER is the only thing that folds a
                            range index, and `folded()` is O(base) whatever it
                            collapses — so a larger N gives proportionally
                            FEWER folds at the same cost each, paid back on
                            ordinary reads that merge a bigger overlay. The
                            SNB balanced SF10 stall is one such fold, 3.7 s
                            inside a p95 of 1.25 ms [default: 4096]. 0 keeps
                            the built-in.
        --no-deferred-reader-fold
                            A READER's adjacency repair folds its overlay into
                            a fresh base on the query thread — one pass over
                            every row of the table, 50-100 MB on SF1, once per
                            stale table per multi-node statement under a write
                            stream. Fix 83's A/B arm.
        --no-unmetered-members-catch-up
                            A membership catch-up the label's log covers is
                            metered against the pass's row budget at one row
                            per entry and deferred when it does not fit —
                            until the log overflows and the pass REBUILDS the
                            whole label. Fix 82's A/B arm.
        --no-cheap-repair-pricing  The maintenance refresh prices each stale
                            table's repair by WALKING its whole change set
                            and building a set of every changed node, once
                            per table, instead of reading the logs' lengths.
                            A/B arm: the walk is what fix 76 multiplied by the
                            stale-table count, and the 320 -> 920 ms median.
        --amortised-reader-repair  A single-node reader repairs up to the
                            change log's capacity instead of declining at 8,192
                            rows and walking its own span. A/B arm: the decline
                            is cached per snapshot, so every reader behind it
                            walks too — at SF10 one query pays ~1,000 store-wide
                            prefix scans and takes 194-316 s.
        --no-bounded-derived-repair  The maintenance refresh races for its row
                            budget first-come-first-served and takes ONE
                            unbounded repair, instead of sharing the budget
                            max-min across every stale table and bounding each
                            repair to its slice. A/B arm: same window, same
                            binary, the OFF arm's worst refresh was 12,344 ms
                            against the ON arm's 1,772.
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
        --no-count-fold     A count(*) over a chain expands every hop instead
                            of folding its unmaterialised suffix into a
                            weight. Server-unreachable until v187. A/B arm.
        --no-count-fold-memo  A var's level is recomputed per visit even when
                            it is a pure function of the node id. A/B arm.
        --no-fold-child-order  A var's folded children run in pattern order
                            rather than semijoin-first. A/B arm.
        --no-count-only-reorder  A count(*) pattern's hops run in the order
                            written. A/B arm.
        --no-fold-hoisted-close  A fold CLOSE probes the adjacency table per
                            probe instead of a hoisted, peer-sorted copy of
                            the bound node's row (fix 84). A/B arm.
        --fold-hoist-after N  Probes a binding of a close's bound node answers
                            through the table before its row is hoisted
                            (default 8; 0 hoists on the first probe). A hoist
                            costs the row; a probe costs a lookup.
        --no-fold-symmetry-breaking  A count fold enumerates every order of
                            an interchangeable var set instead of one order
                            times its size factorial (fix 90). A/B arm.
        --expand-truncation          Follow only `truncationLimit` edges out of
                                     each node on a variable-length hop (LDBC
                                     FinBench). CHANGES ANSWERS by design.
        --path-estimate              Price a both-ends-bound multi-hop path
                                     from a measured first hop (off: use the
                                     written order).
        --no-rel-predicate-pushdown  Filter variable-length paths after
                                     enumerating them, instead of refusing an
                                     edge the path predicate already rejects.
        --no-prefix-streaming        Run every clause of a statement the
                                     streaming pipeline refuses as a whole (a
                                     procedure CALL mid-statement) on the
                                     materialising loop, instead of streaming
                                     its prefix. A/B arm.
        --no-prop-column-epoch-currency  Any commit anywhere retires every
                            cached property column, instead of only a commit
                            that moved the column's own label or property
                            epoch (fix 124). A/B arm — the one that says what
                            the column cache is worth under a write stream.
        --prop-column-restamp  A commit that touched neither a cached
                            property column's label nor its property advances
                            that column's stamp instead of retiring it, giving
                            currency to properties with NO change log (fix 93,
                            strategy O4). OFF by default: it is the one
                            currency lever that can revive a stale column if a
                            write path is unaccounted for.
        --prop-column-budget-mb N  The property-column cache's byte budget in
                            MiB [default: 512]. 0 turns the cache OFF — the
                            arm that prices the whole columnar family against
                            no cache at all; a small value exercises eviction.
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

    // Fix 86: the growth report's wall clock, so its lines carry the same
    // `t=<unix ms>` stamp as the maintenance lines and the stress harness's
    // level-start / slow-statement log. Every mode, not only paged: the report
    // is bolt-side and fires whatever the store shape. The bolt crate cannot
    // read a clock itself (the determinism gate), so it is handed the
    // library's (the one crate the gate lets read one).
    engram_bolt::set_wall_clock_probe(Box::new(engram_server::unix_ms));

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

    // ── --data-dir over a PAGED directory is the one wrong-answer case ──────
    //
    // `Store::open_wal` opens one FILE. It never enumerates the directory, so
    // it cannot see `seg-<seq>.seg` and does not know it is standing on a paged
    // store. Point `--data-dir` at a paged directory whose WAL is still
    // genesis-anchored and the open SUCCEEDS: the whole log replays, every
    // segment on disk is ignored, and the server serves an empty — or worse, a
    // partial — database while reporting nothing wrong at all.
    //
    // That shape is reachable rather than theoretical. `--bulk-ingest` writes
    // through `put_unlogged`, landing rows in segments while leaving the WAL at
    // genesis, and the project's own fixture builds exactly this state
    // (`engram-store/tests/paged_wal.rs`).
    //
    // The other refusals on this path exist because "your data directory was
    // unreadable so we started empty" is how a restore gets overwritten. This
    // is the same argument for the case where nothing is unreadable and the
    // answer is still wrong.
    //
    // IT LIVES HERE, with the other argument checks, and NOT at the open site,
    // for a reason worth stating: `std::process::exit` does not run
    // destructors, so a refusal raised after `DirLock::acquire` would leave a
    // stale LOCK behind and the next start would report a lock held by a pid
    // that never ran. Argument validation belongs before anything is acquired —
    // before the lock, and before the listener binds.
    //
    // An unreadable directory is deliberately NOT this check's business; the
    // open reports that properly with the OS error attached, so a failed
    // `read_dir` falls through rather than inventing a second diagnostic for
    // one fault.
    // One `if let` over a pair rather than a let chain: the MSRV (1.85)
    // predates let chains.
    if let Some((dir, Ok(rd))) = data_dir.as_ref().map(|d| (d, std::fs::read_dir(d))) {
        let mut segs: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("seg-") && n.ends_with(".seg"))
            .collect();
        if !segs.is_empty() {
            segs.sort();
            let shown = segs.len().min(4);
            eprintln!(
                "[engram-server] --data-dir was given a PAGED directory: {} contains {} segment \
                 file(s) ({}{}).\n\
                 Refusing to start. Resident mode reads only `engram.wal` and would ignore every \
                 one of them — serving an empty or partial database without reporting an error, \
                 which is worse than failing to start.\n\
                 Use --paged-dir for this directory.",
                dir.display(),
                segs.len(),
                segs[..shown].join(", "),
                if segs.len() > shown { ", …" } else { "" }
            );
            std::process::exit(1);
        }
    }

    let mut cfg = ServerConfig::from_env();
    if let Some(w) = num_of(["--workers", "--workers"]) {
        cfg.workers = w.max(1);
    }
    // ── What this server ANSWERS about the knobs a benchmark compares it on ─
    //
    // A benchmark's fairness stamp is typed on the CLIENT's command line while
    // the server is started here, by someone else, possibly on another day. So
    // a stamp can describe a server that is not running and nothing downstream
    // can tell — the harness's reporter compares two stamps for equality and
    // both can be equally wrong. Answering in HELLO closes that: a client that
    // asks this server cannot be told about a different one.
    //
    // Both numbers are the ones that are actually in circuit, not the flags
    // that were typed:
    //
    //   * a block cache exists only under `--paged-dir`. `--data-dir` and the
    //     in-memory mode are fully resident, so `--paged-cache-mb` is not in
    //     circuit and this reports `None` rather than the flag's value — a
    //     `--cache-mb 8192` stamped against a `--data-dir` run is describing a
    //     knob that is not there, which is worth SAYING rather than agreeing
    //     with.
    //   * the intra-query width is `ENGRAM_QUERY_PARALLELISM`, read through
    //     the one function that also INSTALLS it. `--workers` is the
    //     connection worker count and is reported separately, because the
    //     2026-09-08 dry run stamped `thread_cap: 6` off `--workers 6` against
    //     a server whose real width was 1.
    cfg.serving_hint = Some(engram_bolt::ServingHint {
        cache_budget_mb: paged_dir
            .as_ref()
            .and_then(|_| u32::try_from(paged_cache_mb).ok()),
        thread_cap: u32::try_from(engram_server::installed_query_parallelism()).ok(),
        workers: u32::try_from(cfg.workers).ok(),
    });
    if let Some(m) = num_of(["--max-connections", "--max-connections"]) {
        cfg.max_connections = m.max(1);
    }
    // ONE call, both arms, in library code a test can reach. This used to be
    // an if/else here in `main`, where nothing in the suite could see it: the
    // budget arithmetic had four tests and the decision that consumes it had
    // none, so a server could announce one budget and enforce another — which
    // it did, at SF10, for q7.
    let (resolved, why) = engram_server::resolve_row_budget(num_of(["--row-budget", "--row-budget"]));
    cfg.row_budget = resolved;
    eprintln!("[engram-server] row budget: {why}");

    // THE MEMORY CEILING, and it is a different kind of guard from the budget
    // above. The row budget bounds one statement's materialisation between
    // samples; this bounds the PROCESS over time, by measuring the resident
    // set rather than multiplying a row count by an assumed 96 bytes. Over the
    // ceiling, new statements QUEUE — they are refused only after waiting the
    // whole deadline, because a peak that drains should cost latency and not
    // an error.
    let (mem_ceiling, mem_why) =
        engram_server::resolve_memory_max(num_of(["--memory-max-mb", "--memory-max-mb"]));
    eprintln!("[engram-server] memory ceiling: {mem_why}");
    engram_server::spawn_memory_governor(mem_ceiling, engram_server::memory_queue_wait_ms());
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
    // A weakened default is said out loud at boot (security plan §1.1). The
    // permission itself is read where the sessions are built.
    if std::env::var_os("ENGRAM_TRACE_MARKER").is_some() {
        eprintln!(
            "[engram-server] per-statement trace marker HONOURED (ENGRAM_TRACE_MARKER is set): any client may trace its own statements, at a traced statement's cost, into this log"
        );
    }
    if args.iter().any(|a| a == "--no-tail-copyout") {
        cfg.tail_span_copyout = false;
        eprintln!(
            "[engram-server] tail span copy-out OFF: span reads hold every tail \
             shard latch for the whole merge, excluding writers (the A/B arm)"
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
    if args.iter().any(|a| a == "--no-subquery-end-gather") {
        cfg.subquery_end_gather = false;
        eprintln!(
            "[engram-server] subquery end gather OFF: a label past the whole-label read ceiling falls to one projected record read per end, as before fix 121 (the A/B arm)"
        );
    }
    if let Some(n) = num_of(["--whole-label-read-max", "--whole-label-read-max"]) {
        // 0 keeps the built-in WHOLE_LABEL_READ_MAX (262,144).
        cfg.whole_label_read_max = n as u64;
    }
    if let Some(n) = num_of(["--match-start-chunk", "--match-start-chunk"]) {
        cfg.match_start_chunk = Some(n);
        if n == 0 {
            eprintln!(
                "[engram-server] match start chunking OFF: a writing statement binds every start candidate at once and filters its WHERE after collecting every row (the A/B arm)"
            );
        }
    }
    if args.iter().any(|a| a == "--no-split-maintenance") {
        cfg.split_maintenance = false;
        eprintln!(
            "[engram-server] split maintenance OFF: the derived refresh runs at the tail of the storage thread's loop and cannot start until a spill or compaction returns — the arm in which refresh_runs froze at 20 through a 70 s window on a 99-segment paged store (the A/B arm)"
        );
    }
    if let Some(n) = args
        .iter()
        .position(|a| a == "--range-fold-at")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
    {
        cfg.range_fold_at = n;
        eprintln!("[engram-server] range-index fold threshold: {n} (0 = built-in 4096)");
    }
    if args.iter().any(|a| a == "--no-deferred-reader-fold") {
        cfg.deferred_reader_fold = false;
        eprintln!(
            "[engram-server] reader folds ON: a reader's adjacency repair folds its overlay into a fresh base on the query thread, once per stale table per multi-node statement (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-unmetered-members-catch-up") {
        cfg.members_unmetered_catch_up = false;
        eprintln!(
            "[engram-server] membership catch-ups METERED: a covered catch-up over the membership half of the row budget is deferred, and a deferred label rebuilds once its log overflows (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-cheap-repair-pricing") {
        cfg.cheap_repair_pricing = false;
        eprintln!(
            "[engram-server] cheap repair pricing OFF: every stale table's repair is priced by walking its whole change set under the writers' lock, once per table — the arm behind fix 76's 320 -> 920 ms median (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--amortised-reader-repair") {
        cfg.amortised_reader_repair = true;
        eprintln!(
            "[engram-server] amortised reader repair ON: a single-node reader repairs up to the change log's capacity instead of declining at 8,192 rows and walking its own span (the A/B arm for the SF10 write-path collapse)"
        );
    }
    if args.iter().any(|a| a == "--no-bounded-derived-repair") {
        cfg.bounded_derived_repair = false;
        eprintln!(
            "[engram-server] bounded derived repair OFF: the maintenance pass races for its row budget first-come-first-served and takes ONE UNBOUNDED repair — the arm that produced 109 refreshes totalling 82.7 s, the longest 9.9 s, on a 400 s sweep (the A/B arm)"
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
    if args.iter().any(|a| a == "--no-count-fold") {
        cfg.count_fold = false;
        eprintln!(
            "[engram-server] count fold OFF: a count(*) over a chain expands every hop instead of folding its unmaterialised suffix into a weight (the A/B arm — the first time this mechanism can be run both ways on one binary)"
        );
    }
    if args.iter().any(|a| a == "--no-count-fold-memo") {
        cfg.count_fold_memo = false;
        eprintln!(
            "[engram-server] count fold memo OFF: a var's level is recomputed per visit even when it is a pure function of the node id (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-fold-child-order") {
        cfg.fold_child_order = false;
        eprintln!(
            "[engram-server] fold child order OFF: a var's folded children run in pattern order rather than semijoin-first, as before fix 120 (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-count-only-reorder") {
        cfg.count_only_reorder = false;
        eprintln!(
            "[engram-server] count-only reorder OFF: a count(*) pattern's hops run in the order written, as before v55 (the A/B arm)"
        );
    }
    if let Some(n) = num_of(["--fold-hoist-after", "--fold-hoist-after"]) {
        cfg.fold_hoist_after = n;
        eprintln!(
            "[engram-server] fold hoist after {n} probe(s): a close's bound row is read only once a binding has been probed that many times through the table (fix 84)"
        );
    }
    if args.iter().any(|a| a == "--no-fold-hoisted-close") {
        cfg.fold_hoisted_close = false;
        eprintln!(
            "[engram-server] hoisted close OFF: every fold close probes the adjacency table through edges_to_peer_slim, as before fix 84 (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-fold-symmetry-breaking") {
        cfg.fold_symmetry_breaking = false;
        eprintln!(
            "[engram-server] fold symmetry breaking OFF: a count fold enumerates every order of a symmetric var set and multiplies nothing, as before fix 90 (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-rel-predicate-pushdown") {
        cfg.rel_predicate_pushdown = false;
    }
    if args.iter().any(|a| a == "--path-estimate") {
        cfg.path_estimate = true;
    }
    if args.iter().any(|a| a == "--expand-truncation") {
        cfg.expand_truncation = true;
    }
    if args.iter().any(|a| a == "--no-prefix-streaming") {
        cfg.prefix_streaming = false;
        eprintln!(
            "[engram-server] prefix streaming OFF: a statement the pipeline refuses as a whole runs every clause on the materialising loop (the A/B arm)"
        );
    }
    if args.iter().any(|a| a == "--no-prop-column-epoch-currency") {
        cfg.prop_column_epoch_currency = false;
        eprintln!(
            "[engram-server] property column epoch currency OFF: any commit retires every cached column, as before fix 124 (the A/B arm — first reachable from the server in fix 91)"
        );
    }
    if args.iter().any(|a| a == "--prop-column-restamp") {
        cfg.prop_column_restamp = true;
        eprintln!(
            "[engram-server] property column re-stamp ON (fix 93 / O4): a commit that touched neither a column's label nor its property advances its stamp instead of retiring it"
        );
    }
    if let Some(mb) = num_of(["--prop-column-budget-mb", "--prop-column-budget-mb"]) {
        // 0 is a setting, not an absence: it turns the cache off.
        cfg.prop_column_budget_mb = Some(mb);
        eprintln!("[engram-server] property column cache budget: {mb} MiB (0 = off)");
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
    if args.iter().any(|a| a == "--no-algo-parallel") {
        cfg.algo_parallel = false;
        eprintln!(
            "[engram-server] algorithm parallelism OFF: every fixpoint runs on the calling thread (the A/B arm; the answers are bit-identical either way)"
        );
    }
    if args.iter().any(|a| a == "--no-trigram-indexes") {
        cfg.trigram_indexes = false;
        eprintln!(
            "[engram-server] trigram indexes OFF: `=~`, CONTAINS, STARTS WITH and ENDS WITH scan their label (the A/B arm for the text seek)"
        );
    }
    if args.iter().any(|a| a == "--no-bm25") {
        cfg.bm25_scoring = false;
        eprintln!(
            "[engram-server] BM25 term index OFF: a fulltext query is scored the same way by a scan (the A/B arm for the index, not for the scoring)"
        );
    }
    if args.iter().any(|a| a == "--no-bm25-by-default") {
        cfg.bm25_by_default = false;
        eprintln!(
            "[engram-server] new fulltext indexes will be stamped `tf`: existing indexes keep whatever their catalogue row says"
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
            "[engram-server] precision locking ON: phantoms are closed, and \
             statements that would previously have committed over one now abort \
             and retry"
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
            "[engram-server] derived refresh OFF: readers pay their own rebuild — \
             this is the A/B baseline, not a production setting"
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
        //
        // WITH ITS WAL. A paged store used to be durable only at seal
        // boundaries — the unsealed tail died with the process, and the
        // pod's preStop checkpoint was the only thing standing between an
        // acknowledged write and its loss — while the WAL lived in the
        // resident `--data-dir` mode alone: capacity and durability were
        // two modes. `DIR/engram.wal` now fronts the tail: replayed on open,
        // appended and fsync'd before every acknowledgement, checkpointed
        // behind every spill.
        let wal = dir.join("engram.wal");
        let (store, cache) = match Store::open_paged_dir_with_wal(&dir, paged_cache_mb << 20, &wal)
        {
            Ok(v) => v,
            Err(e) => panic!(
                "cannot open the paged directory: {e}\n\
                     Refusing to start empty over a paged directory that was requested — \
                     starting empty here would look like an empty database rather than a \
                     failed open."
            ),
        };
        eprintln!(
            "[engram-server] paged: {} — cache {paged_cache_mb} MiB, {} segment(s) on disk; \
             durable: {} fronts the tail ({} version(s) replayed).",
            dir.display(),
            store.segment_count(),
            wal.display(),
            store.tail_versions()
        );
        cfg.paged_dir = Some(dir);
        // Fix 85 (instrument): a statement's resident-set growth report names
        // the block cache's share of it, so a cold fill is not read as a leak.
        let probe_cache = std::sync::Arc::clone(&cache);
        engram_bolt::set_resident_cache_probe(Box::new(move || probe_cache.resident_bytes()));
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
            //
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
