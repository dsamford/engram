//! The TCP adapter — risk C12's boundary, drawn as a crate.
//!
//! Everything inside the engine is `Runtime`-generic, single-threaded and
//! sans-io; THIS crate is the one place OS threads, blocking sockets and the
//! wall clock legitimately live. The lints that deny them workspace-wide are
//! allowed here, at the boundary they exist to protect, and nowhere else.
//!
//! # The shape: one engine thread IS the shard
//!
//! The graph is single-threaded by construction (D2), so the server does not
//! share it across OS threads — connection threads do IO ONLY, and every
//! byte funnels through one engine thread that owns the store and every
//! connection's [`BoltServer`] state machine. That is not a workaround; it
//! is the engine's concurrency model surfaced honestly at the adapter:
//! readers feed a channel, the shard applies in arrival order, writers drain
//! per-connection reply channels. Each session holds its own [`Graph`]
//! handle over the ONE shared [`Store`], and the ANN staleness signal is the
//! store's own commit clock, so a write through any session invalidates
//! every session's cache.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engram_bolt::BoltServer;
use engram_graph::{Graph, RefreshReport};
use engram_key::{Namespace, Realm};
use engram_observe::counted;
use engram_store::Store;

/// Process-wide counters for what the maintenance thread does — the
/// `engram_bolt::counters` pattern: the engine's `counted!` traces are
/// thread-local test instruments and the maintenance thread installs none.
pub mod counters {
    use std::sync::atomic::AtomicU64;

    /// Completed derived-structure refresh passes (one per ask or tick that
    /// ran [`engram_graph::Graph::refresh_stale_derived`] over every graph,
    /// whether or not anything was stale). A test waits on this to know the
    /// pass that followed its writes has FINISHED, not merely started.
    pub static MAINTENANCE_REFRESH_RUNS: AtomicU64 = AtomicU64::new(0);

    /// Compaction asks made because the TOMBSTONE ratio crossed its threshold,
    /// rather than because the segment count did. Counted separately so the
    /// delete-aware trigger can be shown to fire at all — a threshold nothing
    /// ever crosses is indistinguishable from one that is not wired up.
    pub static COMPACTIONS_ASKED_FOR_TOMBSTONES: AtomicU64 = AtomicU64::new(0);

    /// Paged compactions that RAN. §5.2 emits the derived bases from a merge,
    /// so this is also the rate at which those bases are refreshed and
    /// persisted — the numerator of §5.5's decision rule.
    ///
    /// Every paged compaction is FULL: `compact_paged_observed` merges the
    /// whole sealed set and has no partial mode, so there is no full:partial
    /// ratio to track. That is a property of the compactor rather than of a
    /// policy, which is why the rule reduces to a rate.
    pub static PAGED_COMPACTIONS: AtomicU64 = AtomicU64::new(0);

    /// Of those, the ones the CADENCE forced — neither the segment count nor
    /// the tombstone ratio had asked. Counted separately because the cadence is
    /// the item under evaluation: a floor that never fires is indistinguishable
    /// from one that is not wired up, and a floor that fires for EVERY
    /// compaction means the other two triggers are doing nothing.
    pub static PAGED_COMPACTIONS_BY_CADENCE: AtomicU64 = AtomicU64::new(0);
}

/// One live connection's engine-side state: its protocol machine, its reply
/// channel, and the backpressure credit it shares with its reader thread.
type Session = (BoltServer, Sender<Vec<u8>>, Arc<AtomicUsize>);

enum ToEngine {
    Open {
        id: u64,
        reply: Sender<Vec<u8>>,
        /// Bytes this connection has queued to the engine and the engine has
        /// not yet consumed. SHARED with the reader thread: the reader adds
        /// before sending, the engine subtracts after consuming, so it is a
        /// real credit loop. A thread-local on the reader could only ever
        /// increase — the consumer is a different thread — and the reader
        /// would park for ever the first time it filled.
        inflight: Arc<AtomicUsize>,
    },
    Bytes {
        id: u64,
        data: Vec<u8>,
    },
    Closed {
        id: u64,
    },
}

/// The operational limits an exposed server needs.
///
/// Every field here is a bound that did not exist, and whose absence was
/// reachable by an unauthenticated client. They are grouped into one struct
/// rather than added as four parameters because the CLI/config surface will
/// populate exactly this — a flag is a compatibility promise, so the shape is
/// settled once, here, rather than being invented four times.
///
/// The defaults are chosen to be SAFE, not maximal: a database that refuses an
/// absurd query is recoverable, and one that is OOM-killed is not.
#[derive(Clone)]
pub struct ServerConfig {
    /// Engine worker threads. Connections pin to one by `id % workers`.
    pub workers: usize,
    /// Rows a single query may materialise before it is refused.
    ///
    /// The engine defaults this to `None` (unbounded) and the SERVER never set
    /// it, so `budget_check`'s call sites were inert in the only binary facing
    /// a network: `MATCH (a)-[*]->(b)` enumerated every simple path until the
    /// OOM killer arrived. The engine's own doc says the full-corpus benchmark
    /// died exactly that way.
    pub row_budget: Option<usize>,
    /// Concurrent connections accepted. Each costs two OS threads, so an
    /// unbounded accept loop is an unbounded thread count.
    pub max_connections: usize,
    /// Idle read timeout. Without one a connection that opens and says nothing
    /// holds its thread pair forever — the slowloris shape.
    pub read_timeout: Option<Duration>,
    /// Write timeout, so a peer that stops reading cannot pin a writer thread.
    pub write_timeout: Option<Duration>,
    /// Unacknowledged bytes a single connection may have queued to the engine.
    ///
    /// The reader used to `send` into an unbounded channel as fast as it could
    /// read, so a fast client against a slow engine grew the queue without
    /// limit. This is the backpressure that turns that into a stalled reader.
    pub max_inflight_bytes: usize,
    /// Largest single Bolt message a session will assemble. See
    /// `engram_bolt::MAX_MESSAGE_BYTES` for why this is policy, not protocol.
    pub max_message_bytes: usize,
    /// Applied to EVERY graph the resolver constructs, at the one place a graph
    /// is built for a network session.
    ///
    /// Without this a caller can only configure a graph it builds ITSELF — and
    /// the graph a caller builds in `make_store` is not the graph that serves
    /// queries, because `make_store` returns a `Store`. `portserve` set its
    /// benchmark A/B toggles on that temporary loading graph and they were
    /// silently discarded: both arms of a before/after ran the same engine and
    /// produced numbers within 2% of each other, which reads exactly like "the
    /// fix does nothing".
    ///
    /// The same trap the `row_budget` note below describes, arriving from the
    /// other direction. Anything that must hold for every session belongs here.
    pub configure_graph: Option<ConfigureGraph>,
    /// Build the derived structures a first query would otherwise build inline,
    /// before the listener starts accepting. Default **on**.
    ///
    /// Off, the first query after a restart pays for the whole corpus:
    /// measured at **5.85 s against a 1.48M-node graph**, with the first ten
    /// seconds of a benchmark run producing almost nothing. A server that
    /// starts fast and then stalls its first user is worse than one that takes
    /// a few more seconds to say it is ready, so this defaults on and the
    /// caller opts out.
    pub warm_caches: bool,
    /// GROUP COMMIT: fsync once per batch of requests instead of once per
    /// write. Default **on**.
    ///
    /// The engine thread drains its inbox as a batch, appends every write,
    /// holds every reply the batch produced, pays ONE fsync, and only then
    /// releases the replies. With one client nothing queues during the fsync,
    /// so it degrades to exactly one fsync per write — no regression. With
    /// eight, the fsync's ~2.6 ms is long enough for all eight to send their
    /// next request, so the next batch shares one fsync eight ways.
    ///
    /// Measured before this existed: write throughput flat at 375 → 380 ops/s
    /// from 1 to 8 clients, against an incumbent at 517 → 2,671 on identical
    /// hardware. Off is kept for the A/B and for nothing else.
    pub group_commit: bool,
    /// Seal the store's tail into an immutable segment once it holds this many
    /// versions. Default **65,536**.
    ///
    /// Every read of a store with a non-empty tail takes the hot latch the
    /// writers hold, so an unsealed corpus is served from behind the write
    /// lock — a recovered server did exactly that with its whole history,
    /// ~1,000 latch acquisitions per statement under a balanced load. The
    /// tail is sealed once at startup and then on this threshold by whichever
    /// worker's batch crosses it; a sealed segment is read lock-free.
    pub seal_after_versions: usize,
    /// Compact the sealed segments into one once there are this many. Default
    /// **8**. Compaction runs on a maintenance thread and holds the hot lock
    /// only to swap the result in ([`Store::compact`] is online); a read walks
    /// every segment newest-first, so the count is bounded to keep a point
    /// read a handful of lookups.
    pub compact_after_segments: usize,
    /// PAGED SERVING: spill sealed segments to `seg-<seq>.seg` files in this
    /// directory instead of compacting, so steady-state memory is bounded by
    /// the block cache and the store can be bigger than RAM. Default `None`.
    ///
    /// **There is NO WAL in this mode.** Durability is at SEAL boundaries
    /// only: a version reaches disk when its segment is spilled, and the
    /// unsealed tail is VOLATILE — a crash loses it. This is the
    /// benchmark/bulk-serving mode, not the durable mode.
    pub paged_dir: Option<std::path::PathBuf>,
    /// The live block cache the paged store already reads through — the SAME
    /// handle [`Store::open_paged_dir`] returned, never a fresh one, so every
    /// spill shares one budget (a cache per spill would grow the memory bound
    /// with uptime). Required together with `paged_dir`. A non-data field on
    /// the config, on the `configure_graph` precedent.
    pub paged_spill_cache: Option<Arc<engram_store::paged::BlockCache>>,
    /// READER-INDEPENDENT PUBLISH of derived structures. Default **on**.
    ///
    /// Every adjacency table and membership snapshot catches up on the first
    /// read that needs it, and its change log is pruned only behind that
    /// publish — so a write burst with no reader between hands its WHOLE
    /// changed set to one unlucky reader. SF1's `contention` level stalled
    /// 25 s on its 12th read after two write-only levels for exactly that
    /// reason. On, the maintenance thread runs
    /// [`Graph::refresh_stale_derived`] after `refresh_after_writes` commits
    /// and on every `maintenance_tick`, so readers find current structures.
    /// Off is the A/B arm: the reader pays.
    pub derived_refresh: bool,
    /// COMMIT-CLOCK STAMPS between maintenance refreshes — store versions,
    /// NOT statements. A Bolt write statement costs about three stamps (two
    /// id-counter puts and the one commit of its transaction's write-set —
    /// `group_commit_fsyncs.rs` measures exactly 3.0 fsyncs a statement
    /// with group commit off, one per stamp); a direct `Graph` write costs
    /// one per row. Default **8,192** — roughly 2,700 Bolt statements, at
    /// ~5k statements/s a refresh every ~0.5 s and each repairing at most
    /// that many changed rows, with the tick as the bound under a lighter
    /// load. The first cut of this doc said "commits" and "~1.6 s"; the
    /// clock it reads has never counted commits. `0` refreshes on the tick
    /// only.
    pub refresh_after_writes: u64,
    /// The maintenance thread's tick. Default **5 s**. Paged mode seals a
    /// quiescent tail on it; `derived_refresh` refreshes on it, so a burst
    /// that ended short of `refresh_after_writes` is still caught up within
    /// one tick. Tests shorten it.
    pub maintenance_tick: Duration,
    /// ROWS one maintenance refresh pass may re-read before deferring the
    /// rest to the next pass. Default **250,000**; `0` is unbounded (the
    /// pre-budget behaviour, kept as the A/B arm).
    ///
    /// The rebuild budget was one per pass from the start, but REPAIRS were
    /// unbounded and a large store carries many adjacency tables — official
    /// SF1 carries ~32 — so a pass could repair all of them back to back.
    /// Measured on the pod that cost 2-3x of write throughput; lengthening
    /// the tick did not help, because the cost is the PASS, not its rate.
    pub refresh_pass_rows: usize,
    /// Whether two relationship writes touching one node may commit without
    /// aborting each other (RC1). Default on.
    ///
    /// It has a flag because it is worth 3.7x on the `rel-hub` shape and had
    /// no way to reach it from an operator's hands — the same gap that let a
    /// 2-3x write regression reach a measurement unnoticed in the refresh
    /// pass. A lever nobody can reach is a lever nobody can A/B.
    pub guard_put_put_exempt: bool,
    /// Whether a constraint-list cache hit skips the schema-epoch store probe
    /// (an always-absent KV read that descends every sealed segment). Default
    /// on; off restores the probe and is the differential arm.
    pub constraint_epoch_cache: bool,
    /// Whether the maintenance thread releases the in-memory commit log once
    /// its history is durable elsewhere. Default on.
    ///
    /// Off keeps the full history in memory — the pre-existing behaviour, and
    /// what a pull-style `Store::log_tail` consumer (CDC, replication) needs,
    /// since the server cannot know such a consumer's position.
    pub truncate_log_at_seal: bool,
    /// Ids a serving session reserves per counter write. `0` or `1` restores
    /// one LOGGED counter write per entity.
    ///
    /// The counter row holds the reserved END, so a restart abandons the
    /// unused tail as gaps and an id is never reused. Ids stay dense within a
    /// run; only a restart shows a gap.
    pub id_reservation: usize,
    /// Whether the maintenance thread writes DECLARED range indexes to sidecars
    /// beside the paged segments on a quiescent tick, so a restart loads them
    /// instead of rebuilding. Paged mode only. Default on.
    pub persist_indexes_at_seal: bool,
    /// Tombstone fraction across resident sealed segments past which a seal
    /// also asks for compaction, independently of the segment count.
    ///
    /// 0.2 is Cassandra's `tombstone_threshold` default and the same shape as
    /// RocksDB's `CompactOnDeletionCollector`. `1.0` disables the trigger and
    /// restores count-only scheduling — the differential arm.
    pub tombstone_ratio: f64,
    /// Versions the resident sealed set must hold before the ratio above is
    /// consulted. Without a floor, a store holding four rows — three of them
    /// tombstones — would ask for compaction on every seal.
    pub tombstone_min_versions: u64,
    /// The longest a PAGED store may go between full compactions while it has
    /// more than one segment. `None` (the default) schedules purely on the two
    /// signals above — segment count and tombstone density.
    ///
    /// This is §5.5's cheap alternative, and the plan says to measure it before
    /// spending six days on a multi-level CSR. §5.2 emits the derived bases
    /// from a compaction, so the emit rate is the compaction rate: a store
    /// whose write volume never trips the count trigger also never refreshes
    /// its CSR from a merge, and §5.3 has stopped the maintenance pass
    /// rebuilding it — so the reader pays, and the tail latency this whole
    /// phase exists to remove comes back.
    ///
    /// Setting an interval gives the emit a FLOOR RATE that does not depend on
    /// write volume, at the price of compacting a store that did not otherwise
    /// need it. That price is real and is why this is opt-in rather than a
    /// default: the right value is a measurement on the corpus, not a constant.
    pub compact_max_interval: Option<Duration>,
    /// §7 — PRECISION LOCKING. Validate each transaction's node-pattern
    /// predicates against the rows committed since its snapshot, closing
    /// phantoms. Default **false**.
    ///
    /// It is an isolation UPGRADE and still a behaviour change: it aborts
    /// statements that currently commit, and every abort is a Bolt-level
    /// retry. The plan's gate for flipping it is a full TCK pass and a soak on
    /// both arms.
    ///
    /// A flag on the day the lever lands, per the rule
    /// `docs/derived-refresh-write-tax.md` earned: `set_guard_put_put_exempt`
    /// was worth 3.7x on rel-hub and shipped unreachable from an operator's
    /// hands, which is how a mechanism that can cost 3x of write throughput
    /// went through a whole measurement campaign untested.
    pub precision_locking: bool,
    /// Whether a single-node reader whose adjacency table is STALE asks
    /// whether ITS node moved, instead of repairing the whole change set on
    /// its query thread. Default **true**.
    ///
    /// §8. A write makes a type's table stale for every reader of that type,
    /// and a reader then re-read a row for every node any writer had touched
    /// since the base — proportional to the write stream, paid per read. The
    /// disjoint-type control in `balattr` (writers write a type the readers
    /// never query, nothing else changed) retained 94% of solo read throughput
    /// against 0% for the same-type run, which is what says the interference
    /// is here and not in the store or the commit path.
    pub lazy_stale_serve: bool,
    /// Whether that per-node question is answered by a lock-free stamp filter
    /// rather than under the change log's lock. Default **true**.
    pub adj_change_filter: bool,
    /// Whether a single-node reader whose node DID move declines the table and
    /// walks its own span rather than repairing. Default **true**.
    ///
    /// This is the step that carries the win, and the one with an open
    /// question the pod answers: declining costs O(degree) instead of
    /// O(change set), so an SF1 hub could be the wrong trade. Hence the flag.
    pub single_node_stale_walk: bool,
    /// Whether a reader's repair runs behind the per-table build guard.
    /// Default **false**, and the default is a measurement: on, it cut the
    /// discarded repair work from 54.6% to 3.2% and made the mix 40% SLOWER,
    /// because readers that duplicated work in parallel now queue on a mutex.
    /// Kept as the control that says the redundancy was never the cost.
    pub single_flight_repair: bool,
    /// Whether an anchored MATCH may SEEK a property range index instead of
    /// scanning the label. Default **true**.
    ///
    /// The A/B arm for §9: on SF1 `balanced`, `is7-replies` — the one read
    /// anchored on a property index the writes EXTEND (`Message.id`, one new
    /// id per write) — goes from a 0.16 ms p50 solo to 18.23 ms mixed, 114x,
    /// while reads anchored on a property nothing inserts move 1.2-1.5x. Off,
    /// that read cannot use the index at all, which is what says whether the
    /// index path owns the degradation.
    pub property_seek: bool,
    /// Whether a property index is scoped to the anchor's LABEL rather than
    /// built over the whole partition. Default **true**.
    pub label_scoped_indexes: bool,
    /// Direct adjacency probes tolerated in one epoch before a table may be
    /// BUILT. Default **1024** (`DEGREE_TABLE_AFTER`).
    ///
    /// The gate exists so a one-off query does not pay for a table it will use
    /// once. Its counter is reset whenever the epoch it is ticked with changes,
    /// and that epoch is the GLOBAL adjacency epoch — bumped by every
    /// relationship write of any type. So under a write stream the counter is
    /// reset far more often than it can reach 1,024, and a type whose table
    /// does not yet exist can never accumulate the evidence to build one, even
    /// when no write ever touches that type.
    ///
    /// `derived.rs`'s module doc lists exactly this as defect #1 ("validity
    /// keyed on the wrong clock") and records moving it off the commit clock.
    /// It is still global ACROSS TYPES, which is the half that remains. `0`
    /// admits every table immediately and is the A/B arm that says whether the
    /// gate owns the mixed-profile read collapse.
    pub degree_table_after: u64,
    /// Overlay rows a repaired adjacency table may carry before folding.
    /// Default **4096**. `0` folds every repair — the A/B arm that isolates the
    /// per-hop overlay descent from the per-read staleness check.
    pub adj_overlay_fold: usize,
    /// Whether a hop's label filter is answered by `MembersView::contains`
    /// rather than by materialising the label and binary-searching it.
    /// Default **true**; `--no-hop-membership-contains` is the A/B arm.
    pub hop_membership_contains: bool,
    /// Whether a thread re-serves the adjacency snapshot it just resolved when
    /// the next probe asks for the same table at the same freshness, instead of
    /// rebuilding the map key and walking the table map once per row.
    /// Default **true**; `--no-adj-snap-memo` is the A/B arm.
    pub adj_snap_memo: bool,
    /// Whether a DIRECTED fold close probes from the bound endpoint's row with
    /// the direction flipped (the hot-row locality undirected closes have).
    /// Default **true**; `--no-directed-bound-probe` is the arm.
    pub directed_bound_probe: bool,
    /// Whether an aggregating ORDER BY + LIMIT projection selects its survivor
    /// groups from the finished aggregates BEFORE projecting (ic6: 9,599
    /// groups projected to keep ten). Default **true**; `--no-agg-topk` is the arm.
    pub agg_topk_before_project: bool,
    /// Whether `MATCH … RETURN <literals/params> [SKIP] [LIMIT]` is answered
    /// from the count fold (the match count fixes how many copies of the one
    /// constant row come back) instead of enumerating the pattern as written
    /// (LSQB q3's existence probe: 180 s at SF1 against a 4.5 s count).
    /// Default **true**; `--no-const-projection-fold` is the arm.
    pub const_projection_fold: bool,
    /// Whether the planner's labelled hop counts are memoised on the graph,
    /// keyed on the types' adjacency epoch and the labels' membership epochs.
    /// Default **true**; `--no-hop-count-memo` is the arm.
    pub hop_count_memo: bool,
    /// Base probes after which a membership base is answered from a presence
    /// bitmap. Default **4,096** — measured; `--members-bitmap-after 0` is the
    /// arm that turns it off.
    pub members_bitmap_after: usize,
    /// Whether the count-only reorder picks its path ordering by PEAK
    /// intermediate (searched) rather than by the greedy's next step. Default
    /// **true**; `--no-order-peak-search` is the arm.
    pub order_peak_search: bool,
    /// Whether a span read COPIES the tail's rows out one shard at a time
    /// rather than holding every shard's read latch for the whole merge.
    /// Default **true**.
    ///
    /// The old path excluded every writer for a read's whole duration — and
    /// only when the tail was non-empty, i.e. only in a mixed workload. On the
    /// bench pod at SF1 that showed as 0 span reads excluding writers on both
    /// PURE profiles against 11,681 and 35,723 on the two MIXED ones, which are
    /// exactly the profiles where engram trailed Neo4j.
    ///
    /// `false` is the A/B arm and keeps the old behaviour exactly.
    pub tail_span_copyout: bool,
}

/// Caller configuration applied to every graph the resolver builds.
pub type ConfigureGraph = Arc<dyn Fn(&Graph) + Send + Sync>;

/// The production morsel executor (W3 of the scale-and-integrity plan):
/// real OS threads inside a scope, an atomic cursor doling morsels out so a
/// fast worker takes more of them. The ENGINE never spawns — this lives in
/// the server, the designated OS-thread boundary, and is installed through
/// `Graph::set_exec` behind `ENGRAM_QUERY_PARALLELISM`.
struct ThreadScopeExec {
    width: usize,
}

impl engram_graph::ScopedExec for ThreadScopeExec {
    fn width(&self) -> usize {
        self.width
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let threads = self.width.min(n);
        if threads <= 1 {
            for i in 0..n {
                f(i);
            }
            return;
        }
        let cursor = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(i);
                    }
                });
            }
        });
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("workers", &self.workers)
            .field("row_budget", &self.row_budget)
            .field("max_connections", &self.max_connections)
            .field("read_timeout", &self.read_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("max_inflight_bytes", &self.max_inflight_bytes)
            .field("max_message_bytes", &self.max_message_bytes)
            .field("configure_graph", &self.configure_graph.is_some())
            .field("warm_caches", &self.warm_caches)
            .field("group_commit", &self.group_commit)
            .field("seal_after_versions", &self.seal_after_versions)
            .field("compact_after_segments", &self.compact_after_segments)
            .field("paged_dir", &self.paged_dir)
            .field("paged_spill_cache", &self.paged_spill_cache.is_some())
            .field("derived_refresh", &self.derived_refresh)
            .field("refresh_after_writes", &self.refresh_after_writes)
            .field("maintenance_tick", &self.maintenance_tick)
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: 1,
            // 20M rows is the value the benchmark harness uses for a
            // 2.7M-row corpus: high enough that no legitimate query on a
            // realistic graph reaches it, low enough to refuse a runaway
            // product long before the allocator gives up.
            row_budget: Some(20_000_000),
            max_connections: 512,
            read_timeout: Some(Duration::from_secs(300)),
            write_timeout: Some(Duration::from_secs(60)),
            max_inflight_bytes: 8 * 1024 * 1024,
            max_message_bytes: engram_bolt::MAX_MESSAGE_BYTES,
            configure_graph: None,
            warm_caches: true,
            group_commit: true,
            seal_after_versions: 65_536,
            compact_after_segments: 8,
            paged_dir: None,
            paged_spill_cache: None,
            derived_refresh: true,
            refresh_after_writes: 8_192,
            maintenance_tick: Duration::from_secs(5),
            refresh_pass_rows: 250_000,
            guard_put_put_exempt: true,
            constraint_epoch_cache: true,
            truncate_log_at_seal: true,
            id_reservation: 256,
            persist_indexes_at_seal: true,
            tombstone_ratio: 0.2,
            tombstone_min_versions: 4_096,
            compact_max_interval: None,
            precision_locking: false,
            lazy_stale_serve: true,
            adj_change_filter: true,
            single_node_stale_walk: true,
            single_flight_repair: false,
            property_seek: true,
            label_scoped_indexes: true,
            degree_table_after: 1024,
            adj_overlay_fold: 4096,
            hop_membership_contains: true,
            adj_snap_memo: true,
            directed_bound_probe: true,
            agg_topk_before_project: true,
            const_projection_fold: true,
            hop_count_memo: true,
            members_bitmap_after: 4_096,
            order_peak_search: true,
            tail_span_copyout: true,
        }
    }
}

impl ServerConfig {
    /// Defaults, with `workers` taken from `ENGRAM_SERVER_WORKERS` when set.
    ///
    /// The env var predates this struct and is kept as the DEFAULT SOURCE only,
    /// so an explicit config always wins over ambient process state.
    pub fn from_env() -> ServerConfig {
        let workers = std::env::var("ENGRAM_SERVER_WORKERS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1);
        ServerConfig {
            workers,
            ..ServerConfig::default()
        }
    }
}

/// Serve connections from `listener`. The store is built ON the engine
/// thread by `make_store` (the engine's state is deliberately not `Send`).
/// Blocks forever; background use spawns a thread and keeps the address it
/// read off the listener beforehand.
pub fn run_server(
    listener: TcpListener,
    make_store: impl FnOnce() -> (Store, Realm, Namespace) + Send + 'static,
) -> std::io::Result<()> {
    // Worker count: 1 = byte-for-byte the old single-shard behaviour (what every
    // test and the determinism model expect); ENGRAM_SERVER_WORKERS=N fans
    // connections across N worker threads sharing one graph — the D2-revision
    // concurrent path. Opt-in so nothing changes until a deployment asks.
    run_server_with_config(listener, make_store, ServerConfig::from_env())
}

/// [`run_server`] with an explicit worker count (the env var is only the default
/// source). Tests use this to force the concurrent path without a process-global
/// env that would race other tests in the same binary.
pub fn run_server_with_workers(
    listener: TcpListener,
    make_store: impl FnOnce() -> (Store, Realm, Namespace) + Send + 'static,
    workers: usize,
) -> std::io::Result<()> {
    run_server_with_config(
        listener,
        make_store,
        ServerConfig {
            workers,
            ..ServerConfig::default()
        },
    )
}

/// [`run_server`] with the full operational configuration.
pub fn run_server_with_config(
    listener: TcpListener,
    make_store: impl FnOnce() -> (Store, Realm, Namespace) + Send + 'static,
    cfg: ServerConfig,
) -> std::io::Result<()> {
    let workers = cfg.workers.max(1);

    // The store + ONE Graph per (realm, namespace) over it, built on THIS thread
    // (the store is Send + Sync now) and SHARED across every worker. The graph's
    // caches are internally latched, so N sessions on N threads share one graph;
    // a graph PER worker would each rebuild memberships/indexes and race the
    // id/token counters. Federation is a routing choice, not a graph per session.
    let (store, realm, ns) = make_store();
    // Group commit is a property of the STORE (its log defers fsyncs) that the
    // worker loops below pay off per batch. Set once, here, before any worker
    // can append — a worker that appended under per-write fsync and then
    // switched would be fine, but a worker that appended under deferral before
    // anyone had agreed to pay would have made a write nobody syncs.
    if cfg.group_commit {
        store.set_group_commit(true);
    }
    // The span-read path, applied to the STORE (not per graph): the tail is one
    // structure shared by every coordinate, and a per-session setting would let
    // one session's reads exclude another session's writers.
    store.set_tail_span_copyout(cfg.tail_span_copyout);
    type GraphCache = Arc<Mutex<HashMap<(Realm, Namespace), Arc<Graph>>>>;
    let cache: GraphCache = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<engram_bolt::GraphResolver> = {
        let cache = Arc::clone(&cache);
        let store = store.clone();
        let row_budget = cfg.row_budget;
        let configure = cfg.configure_graph.clone();
        let refresh_pass_rows = cfg.refresh_pass_rows;
        let guard_put_put_exempt = cfg.guard_put_put_exempt;
        let constraint_epoch_cache = cfg.constraint_epoch_cache;
        let id_reservation = cfg.id_reservation;
        let precision_locking = cfg.precision_locking;
        let lazy_stale_serve = cfg.lazy_stale_serve;
        let adj_change_filter = cfg.adj_change_filter;
        let single_node_stale_walk = cfg.single_node_stale_walk;
        let single_flight_repair = cfg.single_flight_repair;
        let property_seek = cfg.property_seek;
        let label_scoped_indexes = cfg.label_scoped_indexes;
        let degree_table_after = cfg.degree_table_after;
        let adj_overlay_fold = cfg.adj_overlay_fold;
        let hop_membership_contains = cfg.hop_membership_contains;
        let adj_snap_memo = cfg.adj_snap_memo;
        let directed_bound_probe = cfg.directed_bound_probe;
        let agg_topk_before_project = cfg.agg_topk_before_project;
        let const_projection_fold = cfg.const_projection_fold;
        let hop_count_memo = cfg.hop_count_memo;
        let members_bitmap_after = cfg.members_bitmap_after;
        // The checkpoint hook's captures: the paged directory and the spill
        // cache, both `None` on a resident server — which then installs no
        // hook and refuses `CALL engram.checkpoint()` rather than answering
        // "durable" about a store whose durability is its WAL.
        let checkpoint_target = cfg
            .paged_dir
            .clone()
            .zip(cfg.paged_spill_cache.clone());
        Arc::new(move |r: Realm, n: Namespace| {
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(c.entry((r, n)).or_insert_with(|| {
                let g = Graph::new(store.clone(), r, n);
                if let Some((dir, spill_cache)) = checkpoint_target.clone() {
                    let store = store.clone();
                    g.set_checkpoint_hook(Some(Arc::new(move || {
                        let t = std::time::Instant::now();
                        // Seal whatever the tail holds (a no-op on an empty
                        // tail), then spill EVERY resident sealed segment —
                        // `spill_sealed_into` is idempotent and takes the
                        // swap latch, so a maintenance-thread spill or a
                        // compaction in flight is neither raced nor waited
                        // for. What is reported is read AFTER the spill, so
                        // a writer landing meanwhile shows in `tail` instead
                        // of hiding behind this call's own seal.
                        let sealed = store.seal().is_some();
                        let spilled = store
                            .spill_sealed_into(&dir, &spill_cache)
                            .map_err(|e| format!("spill failed: {e}"))?;
                        let report = engram_graph::CheckpointReport {
                            spilled,
                            segments: store.segment_count(),
                            resident: store.resident_segment_count(),
                            tail: store.tail_versions(),
                        };
                        eprintln!(
                            "[engram-server] checkpoint in {} ms: sealed the tail={sealed}, spilled {} \
                             segment(s); {} sealed, {} still resident, {} version(s) in the tail",
                            t.elapsed().as_millis(),
                            report.spilled,
                            report.segments,
                            report.resident,
                            report.tail
                        );
                        Ok(report)
                    })));
                }
                // The budget must be applied HERE, at the one place a graph is
                // constructed for a network session. Setting it in `main` would
                // miss every graph a HELLO-routed coordinate creates later, and
                // that silent gap is exactly the shape of the original defect:
                // the mechanism existed, the server never turned it on, and the
                // 30 `budget_check` call sites were inert in the only binary
                // that faces a network.
                if let Some(b) = row_budget {
                    g.set_row_budget(Some(b));
                }
                // Same argument as the row budget above: applied at the one
                // place a session's graph is constructed, so a coordinate
                // created later by a HELLO cannot quietly run unbudgeted.
                g.set_refresh_pass_rows(refresh_pass_rows);
                // Same argument again: applied at the one place a session's
                // graph is constructed, so a HELLO-routed coordinate cannot
                // silently run a different configuration from the one the
                // operator asked for.
                g.set_guard_put_put_exempt(guard_put_put_exempt);
                g.set_constraint_epoch_cache(constraint_epoch_cache);
                g.set_id_reservation(id_reservation);
                // §7, applied at the same one place and for the same reason: a
                // coordinate a HELLO creates later must not silently run a
                // different ISOLATION LEVEL from the one the operator asked
                // for. That is a worse version of the row-budget gap — a
                // per-session guarantee difference nothing reports.
                g.set_precision_locking(precision_locking);
                g.set_lazy_stale_serve(lazy_stale_serve);
                g.set_adj_change_filter(adj_change_filter);
                g.set_single_node_stale_walk(single_node_stale_walk);
                g.set_single_flight_repair(single_flight_repair);
                g.set_property_seek(property_seek);
                g.set_label_scoped_indexes(label_scoped_indexes);
                g.set_degree_table_after(degree_table_after);
                g.set_adj_overlay_fold(adj_overlay_fold);
                g.set_hop_membership_contains(hop_membership_contains);
                g.set_adj_snap_memo(adj_snap_memo);
                g.set_directed_bound_probe(directed_bound_probe);
                g.set_agg_topk_before_project(agg_topk_before_project);
                g.set_const_projection_fold(const_projection_fold);
                g.set_hop_count_memo(hop_count_memo);
                g.set_members_bitmap_after(members_bitmap_after);
                // Morsel parallelism (W3): the server is the ONLY production
                // implementor of the engine's ScopedExec seam — the engine
                // itself never spawns. Off unless the operator sets a width.
                if let Some(width) = std::env::var("ENGRAM_QUERY_PARALLELISM")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|w| *w > 1)
                {
                    eprintln!("[engram-server] query parallelism ON: width {width}");
                    g.set_exec(Some(Arc::new(ThreadScopeExec { width })));
                    g.set_parallel_expand(true);
                    // The COUNT FOLD parallelises under the same seam (P-2);
                    // `--no-parallel-fold` is its A/B arm within a parallel run.
                    if std::env::var("ENGRAM_NO_PARALLEL_FOLD").is_err() {
                        g.set_parallel_fold(true);
                    }
                }
                // The A/B arms' env toggles, applied at the same one place
                // for the same reason. `=0`/`=false` turns an arm OFF; the
                // defaults are the shipped configuration.
                if matches!(
                    std::env::var("ENGRAM_CONFLICT_ESCALATION").as_deref(),
                    Ok("0") | Ok("false")
                ) {
                    eprintln!("[engram-server] conflict escalation OFF (A/B arm)");
                    g.set_conflict_escalation(false);
                }
                // Caller configuration, applied at the same one place and for
                // the same reason.
                if let Some(f) = configure.as_ref() {
                    f(&g);
                }
                Arc::new(g)
            }))
        })
    };
    // Warm the default coordinate so its caches build once, not per worker.
    //
    // This USED to be `let _ = resolver(realm, ns);` and warmed nothing: the
    // resolver constructs a `Graph`, and every derived structure — the
    // label-membership snapshots, the adjacency CSR — is built lazily on first
    // use. So the comment described an intent the line did not carry out, and
    // the first query after start paid for the whole corpus: 5.85 s against a
    // 1.48M-node / 6.66M-relationship graph, with the benchmark's first ten
    // seconds producing almost nothing.
    //
    // Building here, before the listener accepts, moves that cost to where an
    // operator expects it — startup — and makes "ready" mean ready.
    // Seal whatever the store holds in its tail BEFORE warming or accepting:
    // a store bulk-loaded by `make_store` (or replayed by `open_wal`, which
    // seals itself) keeps its whole corpus in the tail, and every read of a
    // non-empty tail takes the hot latch the writers hold. Sealed, the corpus
    // is read lock-free, warming included.
    if let Some(seq) = store.seal() {
        eprintln!(
            "[engram-server] sealed the loaded tail into segment {seq} ({} segment(s))",
            store.segment_count()
        );
    }
    // Paged mode: spill what the boot seal (or `make_store` itself) left
    // resident BEFORE serving — a bulk-loaded corpus would otherwise stay
    // resident for the process lifetime, the exact set bigger-than-RAM
    // serving cannot hold.
    if let (Some(dir), Some(cache)) = (cfg.paged_dir.as_ref(), cfg.paged_spill_cache.as_ref()) {
        spill_and_report(&store, dir, cache);
    }
    // The maintenance thread. Non-paged: compacts the sealed set when a worker
    // asks. Paged: NEVER compacts — [`Store::compact`] materialises a RESIDENT
    // merged segment, the exact allocation a bigger-than-RAM store cannot
    // make — it SPILLS instead, and ticks so a quiescent tail still reaches
    // disk. Off the engine threads because both are O(corpus); each holds the
    // hot lock only to swap its result in, so the workers keep serving
    // meanwhile. Requests are coalesced — a burst of seals asks once.
    //
    // It ALSO owns the derived-structure refresh (`derived_refresh`): on a
    // worker's ask (every `refresh_after_writes` commits) and on every tick,
    // it brings every cached adjacency table and membership snapshot that is
    // behind its source current — the work the next reader would otherwise
    // do inline, and for a write-only burst the work of the WHOLE burst.
    let (maint_tx, maint_rx): (Sender<Maint>, Receiver<Maint>) = channel();
    {
        let store = store.clone();
        let paged = cfg.paged_dir.clone().zip(cfg.paged_spill_cache.clone());
        let graphs = Arc::clone(&cache);
        let tick = cfg.maintenance_tick.max(Duration::from_millis(1));
        let derived_refresh = cfg.derived_refresh;
        let truncate_log = cfg.truncate_log_at_seal;
        let persist_indexes = cfg.persist_indexes_at_seal;
        // The paged compactor runs on THIS thread, so it needs the same two
        // signals the worker's ask uses — a paged store must not be compacted
        // more eagerly than a resident one would be.
        let compact_after = cfg.compact_after_segments.max(2);
        let tombstone_ratio = cfg.tombstone_ratio;
        let tombstone_min_versions = cfg.tombstone_min_versions;
        let compact_max_interval = cfg.compact_max_interval;
        std::thread::spawn(move || {
            // When the last paged compaction ran, for the cadence floor below.
            // `None` means "never in this process", which counts as overdue —
            // a server that starts with many segments and a light write load
            // should reach a compacted state, not wait indefinitely for a
            // trigger its traffic will never pull.
            let mut last_compaction: Option<std::time::Instant> = None;
            // The sealed-set id at the previous tick. Equal across two ticks
            // means the store has SETTLED, which is the condition §5.4's
            // sidecar needs — see its use below.
            let mut last_sealed_id: Option<u64> = None;
            // The tail count at the previous tick. Equal and non-zero
            // across two ticks means QUIESCENT — a finished load's final
            // partial tail — and only then is it sealed, so it spills
            // within ~2 ticks without minting tiny segments mid-load.
            let mut last_tail = 0usize;
            loop {
                // One wake per ask or tick; every ask already queued is
                // folded into the same pass (a burst of seals asks once).
                let (mut storage, mut refresh, mut ticked) = (false, false, false);
                let mut note = |m: Maint| match m {
                    Maint::Storage => storage = true,
                    Maint::Refresh => refresh = true,
                };
                match maint_rx.recv_timeout(tick) {
                    Ok(m) => {
                        note(m);
                        while let Ok(m) = maint_rx.try_recv() {
                            note(m);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ticked = true,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                match paged.as_ref() {
                    Some((dir, cache)) => {
                        let mut quiescent = false;
                        if ticked {
                            let tail = store.tail_versions();
                            quiescent = tail > 0 && tail == last_tail;
                            last_tail = tail;
                            if quiescent {
                                store.seal();
                                storage = true;
                            }
                            // §5.5's cadence floor has to reach a store that writes
                            // NOTHING, not only one that writes too little. With an
                            // empty tail `quiescent` is false, `storage` stays
                            // false, and the block below — the only place `overdue`
                            // is evaluated — never runs: the platform's read-only
                            // paged mirror sat at 13 sealed segments for the life
                            // of every process with `--compact-every` set, and every
                            // prefix walk took the k-way owned-range path instead of
                            // the single-segment stream (a 15-node label count cost
                            // 86 s and ~8 GiB; 2026-09-04). An overdue floor on a
                            // multi-segment store is a storage pass in its own right.
                            if !storage
                                && store.segment_count() > 1
                                && compact_max_interval.is_some_and(|iv| {
                                    last_compaction
                                        .is_none_or(|t: std::time::Instant| t.elapsed() >= iv)
                                })
                            {
                                storage = true;
                            }
                        }
                        if storage {
                            spill_and_report(&store, dir, cache);
                            // COMPACT the paged set, which nothing did before.
                            // The paged arm only ever spilled, so segments and
                            // their tombstones accumulated for the life of the
                            // process and every O(segments) path — merge_span,
                            // every prefix walk, every adjacency scan — got
                            // monotonically slower with uptime.
                            //
                            // Gated on the same two signals the resident path
                            // uses, so a paged store is not compacted more
                            // eagerly than a resident one would be: too many
                            // segments, or too many tombstones among them.
                            let (ratio, versions) = store.tombstone_ratio();
                            let dense = versions >= tombstone_min_versions && ratio > tombstone_ratio;
                            // §5.5's CADENCE FLOOR. The two triggers above are
                            // both proportional to write volume, and §5.2 made
                            // the compaction rate the rate at which the derived
                            // bases refresh — so a store that writes too little
                            // to trip either one also stops refreshing its CSR,
                            // while §5.3 has stopped the maintenance pass
                            // rebuilding it. The reader then pays, which is the
                            // latency this phase exists to remove.
                            //
                            // Opt-in: compacting a store that did not need it
                            // is a real cost, and the right interval is a
                            // measurement on the corpus, not a constant.
                            let overdue = compact_max_interval.is_some_and(|iv| {
                                last_compaction.is_none_or(|t: std::time::Instant| t.elapsed() >= iv)
                            }) && store.segment_count() > 1;
                            let asked = store.segment_count() >= compact_after || dense;
                            if asked || overdue {
                                if overdue && !asked {
                                    counters::PAGED_COMPACTIONS_BY_CADENCE
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                counters::PAGED_COMPACTIONS.fetch_add(1, Ordering::Relaxed);
                                last_compaction = Some(std::time::Instant::now());
                                let t = std::time::Instant::now();
                                // §5.2: the merge walks every adjacency and
                                // membership row in key order anyway, and that
                                // order IS the CSR — so the compaction emits
                                // the derived bases instead of leaving them to
                                // a separate O(corpus) rescan. With
                                // `set_compaction_csr(false)` on every graph,
                                // or nothing published to emit for, this is
                                // exactly `store.compact_paged_to_dir`.
                                let list: Vec<Arc<Graph>> = graphs
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .values()
                                    .cloned()
                                    .collect();
                                match engram_graph::compact_paged_emitting(
                                    &list, &store, dir, cache,
                                ) {
                                    Ok((retired, dropped)) if retired > 0 || dropped > 0 => {
                                        eprintln!(
                                            "[engram-server] paged-compacted in {} ms: {retired} \
                                             version(s) retired, {dropped} key(s) dropped, {} \
                                             segment(s) remain",
                                            t.elapsed().as_millis(),
                                            store.segment_count()
                                        );
                                    }
                                    Ok(_) => {}
                                    // A failed compaction is a lost optimisation,
                                    // not lost data: the old segments are still
                                    // published and still correct. Say so once
                                    // rather than taking the server down.
                                    Err(e) => {
                                        eprintln!("[engram-server] paged compaction failed: {e}");
                                    }
                                }
                            }
                        }
                        // Quiescent only: the serialize is O(index), so doing
                        // it under load would trade a restart cost for a
                        // steady-state one.
                        if quiescent && persist_indexes {
                            persist_declared_indexes(&graphs, dir);
                        }
                        // §5.4 kept CURRENT — a SEPARATE gate from the index
                        // persist above, deliberately.
                        //
                        // A sidecar's vintage is the sealed set it came from,
                        // so the next seal invalidates the one a compaction
                        // wrote. Measured on the pod: a compaction wrote a
                        // 1.41 GB sidecar against a 1-segment sealed set, the
                        // load sealed more, and the next boot adopted NOTHING.
                        // Re-stamping on a quiescent tick is what makes the
                        // cold-start saving survive a server that keeps working.
                        //
                        // The gate is `quiescent` alone. Sharing
                        // `persist_indexes` would mean an operator turning off
                        // INDEX persistence silently turned off the CSR
                        // persistence beside it — one flag disabling the
                        // mechanism next to the one it names. `set_persist_derived`
                        // is this item's own lever and is checked inside.
                        //
                        // Same reason for the quiescent gate as the index
                        // persist: O(corpus) to serialise, so never under load.
                        // It declines silently whenever a base is stale, and
                        // skips entirely when the sealed set has not moved.
                        // SETTLED, not "quiescent". Measured on the pod: with
                        // the tail-based gate the sidecar was written and then
                        // REFUSED at the next boot —
                        //   "it describes sealed set 0x0ad5…, the store has
                        //    0x6945…"
                        // — because `quiescent` means "a NON-EMPTY tail did not
                        // change", which has two consequences that both defeat
                        // this item:
                        //
                        //   1. an IDLE server (tail 0) is never quiescent, so a
                        //      store nobody is writing never re-stamps at all;
                        //   2. the persist runs, and a LATER tick compacts (the
                        //      cadence floor) and moves the sealed set again —
                        //      with the tail now empty, nothing re-stamps after
                        //      it.
                        //
                        // The condition this item actually needs is that the
                        // SEALED SET has not moved for a whole tick, because
                        // the sealed set is what the file's vintage names.
                        // Satisfiable by an idle server, and false exactly when
                        // a rewrite would be wasted.
                        let sealed_now = store.sealed_set_id();
                        let settled = last_sealed_id == Some(sealed_now);
                        last_sealed_id = Some(sealed_now);
                        if settled {
                            let list: Vec<Arc<Graph>> = graphs
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .values()
                                .cloned()
                                .collect();
                            let mut wrote = 0usize;
                            for g in &list {
                                if g.persist_derived_now(dir, mono_secs()) {
                                    wrote += 1;
                                }
                            }
                            if wrote > 0 {
                                eprintln!(
                                    "[engram-server] persisted the derived bases for                                      {wrote} graph(s)"
                                );
                            }
                        }
                    }
                    None => {
                        if storage {
                            let t = std::time::Instant::now();
                            let (retired, dropped) = store.compact();
                            eprintln!(
                                "[engram-server] compacted in {} ms: {} version(s) retired, {} \
                                 key(s) dropped, {} segment(s) remain",
                                t.elapsed().as_millis(),
                                retired,
                                dropped,
                                store.segment_count()
                            );
                        }
                    }
                }
                // Release the in-memory commit log once its history is durable
                // somewhere else. `CommitLog::entries` was retained for the
                // process lifetime and never truncated by the server — at
                // ~150 B per version that is the term which put a paged SF1
                // load at ~17 GB of the pod's 40.
                //
                // WHY THIS CANNOT LOSE A WRITE, argued rather than assumed:
                //   - `append_prehashed` writes the record to the durable sink
                //     BEFORE pushing it to `entries` (engram-log), so the file
                //     already holds everything the vector does.
                //   - `--data-dir` recovery is `Store::open_wal` -> `Wal::open`,
                //     which reads ENTRIES FROM THE FILE and replays them into a
                //     fresh sink-less log. It never reads this vector.
                //   - `--paged-dir` has no sink and no replay contract at all;
                //     durability is at seal boundaries, and the seal happened
                //     above.
                //   - `truncate_below` keeps `len()` and carries the dropped
                //     prefix's hash into `truncated_head`, so the chain, the
                //     sequence allocator and `log_head()` are unaffected.
                //
                // The one live consumer of the retained vector is
                // `Store::log_tail`, the pull-style CDC/replication read. The
                // server has no such consumer today; `--keep-full-log` exists
                // so that adding one does not require a code change to keep
                // its history.
                if storage && truncate_log {
                    let upto = store.log_len();
                    let dropped = store.truncate_log_below(upto);
                    if dropped > 0 {
                        eprintln!(
                            "[engram-server] released {dropped} in-memory log entry(ies) \
                             below seq {upto} (durable via {})",
                            if paged.is_some() { "sealed segments" } else { "the WAL" }
                        );
                    }
                }
                if derived_refresh && (refresh || ticked) {
                    refresh_derived(&graphs);
                }
            }
        });
    }
    // The production counter surface (P0 of docs/scale-and-integrity-plan.md):
    // the thread-local `counted!` traces are test instruments — nothing
    // installs one here — so the events that matter operationally are global
    // atomics (the FSYNCS pattern), printed when they move. Every 30 s, one
    // line, only on change, so a quiet server logs nothing.
    //
    // Beside it, a MEMORY line: what the block cache holds against its
    // budget and what every graph's derived structures hold (adjacency
    // directories + rows, membership snapshots, range indexes). Printed when
    // any term moves by more than 64 MB. `kubectl top` said 25 GiB under
    // shadow reads and nothing said which structure; this says.
    let graphs_for_memory = Arc::clone(&cache);
    let spill_cache_for_memory = cfg.paged_spill_cache.clone();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering::Relaxed;
        let mut last = String::new();
        let mut last_memory: [usize; 6] = [usize::MAX; 6];
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            {
                let mut r = engram_graph::MemoryReport::default();
                let graphs: Vec<Arc<Graph>> = graphs_for_memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .cloned()
                    .collect();
                for g in &graphs {
                    let m = g.memory_report();
                    r.adjacency_tables += m.adjacency_tables;
                    r.adjacency_bytes += m.adjacency_bytes;
                    r.memberships += m.memberships;
                    r.membership_bytes += m.membership_bytes;
                    r.range_indexes += m.range_indexes;
                    r.range_index_bytes += m.range_index_bytes;
                    r.prop_columns += m.prop_columns;
                    r.prop_column_bytes += m.prop_column_bytes;
                }
                let (cache_resident, cache_budget) = match &spill_cache_for_memory {
                    Some(c) => (c.resident_bytes(), c.budget_bytes()),
                    None => (0, 0),
                };
                // The process's resident set beside what the engine can name:
                // the difference is the number an operator needs (allocator
                // retention, decode transients, an under-charged cache). v84
                // printed 5.4 GB of parts against 11.85 GB of anonymous RSS
                // and the gap had to be read out of /proc by hand.
                let rss = process_rss_bytes().unwrap_or(0);
                let now = [
                    cache_resident,
                    r.adjacency_bytes,
                    r.membership_bytes,
                    r.range_index_bytes,
                    r.prop_column_bytes,
                    rss,
                ];
                let moved = now
                    .iter()
                    .zip(last_memory.iter())
                    .any(|(a, b)| a.abs_diff(*b) > 64 << 20);
                if moved {
                    let mb = |b: usize| b / (1024 * 1024);
                    let attributed = cache_resident
                        + r.adjacency_bytes
                        + r.membership_bytes
                        + r.range_index_bytes
                        + r.prop_column_bytes;
                    eprintln!(
                        "[engram-server] memory: cache {}/{} MB, adjacency {} MB in {} table(s), memberships {} MB in {} label(s), range indexes {} MB in {} index(es), property columns {} MB in {} column(s); rss {} MB, unattributed {} MB",
                        mb(cache_resident),
                        mb(cache_budget),
                        mb(r.adjacency_bytes),
                        r.adjacency_tables,
                        mb(r.membership_bytes),
                        r.memberships,
                        mb(r.range_index_bytes),
                        r.range_indexes,
                        mb(r.prop_column_bytes),
                        r.prop_columns,
                        mb(rss),
                        mb(rss.saturating_sub(attributed))
                    );
                    last_memory = now;
                }
            }
            let w: Vec<u64> = engram_bolt::counters::WON_AT
                .iter()
                .map(|c| c.load(Relaxed))
                .collect();
            let line = format!(
                "txn_conflicts={} autocommit_reruns={} won@1={} won@2={} won@3-4={} \
                 won@5-8={} won@9+={} max_attempts={} escalations={} escalated_losses={} \
                 fsyncs={} adj_built={} adj_repaired={} derived_refreshed={} refresh_runs={}                  span_excl={} span_free={} span_rows_excl={}                  stale_served={} stale_declined={}                  idx_builds={} idx_catchups={} idx_folds={}                  mem_caught={} mem_built={} mem_folds={} mem_flat={} mem_flat_rows={} mem_probes={} mem_bitmaps={} seed_scan_rows={}",
                engram_store::TXN_CONFLICTS.load(Relaxed),
                engram_bolt::counters::AUTOCOMMIT_RERUNS.load(Relaxed),
                w[0],
                w[1],
                w[2],
                w[3],
                w[4],
                engram_bolt::counters::MAX_ATTEMPTS.load(Relaxed),
                engram_bolt::counters::ESCALATIONS.load(Relaxed),
                engram_bolt::counters::ESCALATED_LOSSES.load(Relaxed),
                engram_store::FSYNCS.load(Relaxed),
                engram_graph::counters::ADJ_TABLES_BUILT.load(Relaxed),
                engram_graph::counters::ADJ_TABLES_REPAIRED.load(Relaxed),
                engram_graph::counters::DERIVED_REFRESHED_BY_MAINTENANCE.load(Relaxed),
                counters::MAINTENANCE_REFRESH_RUNS.load(Relaxed),
                // The writer-exclusion probe. `span_excl` counts span reads
                // that held ALL 64 tail shard latches — mutually excluding
                // every writer — and `span_free` those that skipped them
                // because the tail was empty. The ratio is the diagnosis: it
                // should be ~0 on read-only (tail drains at the seal) and ~0 on
                // write-only (no span reads), and dominate in a MIX, which is
                // exactly where engram loses to Neo4j.
                engram_store::SPAN_READS_EXCLUDING_WRITERS.load(Relaxed),
                engram_store::SPAN_READS_LATCH_FREE.load(Relaxed),
                engram_store::SPAN_ROWS_UNDER_LATCHES.load(Relaxed),
                // §8. `stale_served` counts single-node reads answered from a
                // table that is stale as a WHOLE but current for the node they
                // asked about; `stale_declined` those that fell back to the
                // direct span walk because a repair would have cost more than
                // a reader should pay. Together they are the attribution the
                // throughput number cannot give: the same ops/s can mean the
                // tables are serving or that every read is walking, and only
                // this pair says which.
                engram_graph::counters::ADJ_STALE_SERVED_UNMOVED.load(Relaxed),
                engram_graph::counters::ADJ_STALE_DECLINED_TO_WALK.load(Relaxed),
                // §9. Against the profile's read count these give the per-read
                // frequency of each range-index path: a FULL build is O(group)
                // (~3M rows for `Message` at SF1), a fold is O(base), and a
                // catch-up clones and re-sorts `added`. Frequency times known
                // cost is what turns the per-shape correlation into a mechanism.
                engram_store::INDEX_BUILDS.load(Relaxed),
                engram_store::INDEX_CATCHUPS.load(Relaxed),
                engram_store::INDEX_FOLDS.load(Relaxed),
                engram_graph::counters::MEMBERS_CAUGHT_UP.load(Relaxed),
                engram_graph::counters::MEMBERS_BUILT.load(Relaxed),
                engram_graph::counters::MEMBERS_FOLDS.load(Relaxed),
                engram_graph::counters::MEMBERS_MATERIALISED.load(Relaxed),
                engram_graph::counters::MEMBERS_FLAT_ROWS.load(Relaxed),
                // How many candidate peers a hop's label filter tested. Against
                // the profile's read count this is probes per read, and against
                // the shape table it says whether the per-peer test is worth a
                // denser representation than a binary search over the label.
                engram_graph::counters::MEMBERS_PROBES.load(Relaxed),
                engram_graph::counters::MEMBERS_BITMAPS.load(Relaxed),
                engram_graph::counters::SEED_SCAN_ROWS.load(Relaxed),
            );
            if line != last {
                eprintln!("[engram-server] counters: {line}");
                last = line;
            }
        }
    });

    let warm_graph = resolver(realm, ns);
    // Upgrade v1 constraints to marker families (W1.2 of the scale-and-
    // integrity plan): idempotent, one population walk per v1 constraint.
    // A constraint that cannot be upgraded (un-encodable tuples, or
    // pre-existing duplicates that drifted in through the phantom this
    // closes) stays on walk enforcement and is REPORTED, never silently
    // certified.
    match warm_graph.upgrade_constraint_markers() {
        Ok((0, skipped)) if skipped.is_empty() => {}
        Ok((n, skipped)) => {
            eprintln!("[engram-server] constraint markers: {n} constraint(s) upgraded");
            for s in skipped {
                eprintln!("[engram-server] constraint markers: {s}");
            }
        }
        Err(e) => eprintln!("[engram-server] constraint marker upgrade FAILED: {e:?}"),
    }
    // §5.4 — adopt the persisted derived bases BEFORE warming.
    //
    // The order is the whole point. `warm()` builds every structure a sidecar
    // would have supplied, so adopting after it would leave the file correct,
    // the counters honest, and the 43.2 s walk still paid — a change that
    // measures as a no-op and looks like one that did not work, rather than one
    // in the wrong place.
    //
    // A refused sidecar adopts nothing and the warm below does what it always
    // did, so this line can only remove work.
    if let Some(dir) = cfg.paged_dir.as_ref() {
        let t = std::time::Instant::now();
        let adopted = warm_graph.adopt_derived_sidecar(dir);
        if adopted > 0 {
            eprintln!(
                "[engram-server] adopted {adopted} derived structure(s) from disk in {} ms",
                t.elapsed().as_millis()
            );
        }
    }
    if cfg.warm_caches {
        let t = std::time::Instant::now();
        let w = warm_graph.warm();
        eprintln!(
            "[engram-server] warmed in {} ms: {} nodes, {} out-edges, {} in-edges, \
             {} adjacency table(s) holding {} MB in {} MB allocated",
            t.elapsed().as_millis(),
            w.nodes,
            w.out_edges,
            w.in_edges,
            w.tables,
            w.table_bytes >> 20,
            w.table_capacity_bytes >> 20
        );
    }

    // One worker thread per shard: each owns its own sessions map and drains its
    // own channel, all sharing the resolver/graph. A connection is PINNED to a
    // worker (id % workers), so its Bolt state machine stays single-threaded (the
    // protocol is ordered per connection) while DIFFERENT connections run in
    // parallel over the one shared graph.
    // The commit stamp at which the derived structures were last asked to
    // refresh — SHARED by the workers, so N workers ask once per window, not
    // N times. Seeded at the current clock: a loaded corpus is current after
    // the warm above and owes no refresh.
    let refresh_mark = Arc::new(std::sync::atomic::AtomicU64::new(store.now_ts()));
    let mut worker_txs: Vec<Sender<ToEngine>> = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (wtx, wrx): (Sender<ToEngine>, Receiver<ToEngine>) = channel();
        worker_txs.push(wtx);
        let resolver = Arc::clone(&resolver);
        let max_message_bytes = cfg.max_message_bytes;
        let store = store.clone();
        let group_commit = cfg.group_commit;
        let seal_after = cfg.seal_after_versions.max(1);
        let compact_after = cfg.compact_after_segments.max(2);
        let tombstone_ratio = cfg.tombstone_ratio;
        let tombstone_min_versions = cfg.tombstone_min_versions;
        let paged = cfg.paged_dir.is_some();
        let maint_tx = maint_tx.clone();
        let refresh_after = if cfg.derived_refresh {
            cfg.refresh_after_writes
        } else {
            0
        };
        let refresh_mark = Arc::clone(&refresh_mark);
        // Diagnostic: `ENGRAM_TRACE_STATEMENTS=1` prints every statement as it
        // is received, so the last line before a stall names the statement.
        let trace_statements = std::env::var_os("ENGRAM_TRACE_STATEMENTS").is_some();
        // Diagnostic: `ENGRAM_TRACE_COUNTERS=1` dumps every counter each
        // statement records, biggest first — the per-statement attribution the
        // periodic counters line cannot give, because that line is a fixed
        // selection and accumulates across every session.
        let trace_counters = std::env::var_os("ENGRAM_TRACE_COUNTERS").is_some();
        let order_peak_search = cfg.order_peak_search;
        std::thread::spawn(move || {
            // The ordering search's lever is a THREAD-LOCAL (the pipeline's
            // levers all are), so it must be set on every worker that plans a
            // statement — setting it once on the boot thread would leave every
            // query running the default.
            engram_graph::pipeline::set_order_peak_search(order_peak_search);
            // The inflight counter rides alongside each session so the engine
            // can release the reader's credit as it consumes.
            let mut sessions: HashMap<u64, Session> = HashMap::new();
            // GROUP COMMIT.
            //
            // The inbox is drained as a BATCH: block for the first message,
            // then take everything already queued without blocking. Every
            // reply the batch produces is HELD, the batch's writes are made
            // durable with one fsync, and only then are the replies released.
            //
            // Why hold EVERY reply and not only the write acknowledgements: a
            // read later in the same batch can observe a write earlier in it
            // (the write is published to the memtable on append, before the
            // fsync). Releasing that read's reply first would tell a client
            // about data a crash could still lose. Holding the whole batch
            // keeps "you were told it, so it is durable" true for every
            // message, not just the ones that wrote.
            //
            // No timer, deliberately. With one client nothing queues while the
            // fsync runs, so each batch is one write and one fsync — exactly
            // the previous behaviour. With many, the fsync's own latency is the
            // window in which the next batch accumulates, so batches size
            // themselves to the load.
            loop {
                let first = match wrx.recv() {
                    Ok(m) => m,
                    // Every sender dropped: the listener is gone.
                    Err(_) => break,
                };
                let mut batch = vec![first];
                while let Ok(m) = wrx.try_recv() {
                    batch.push(m);
                }
                let mut held: Vec<(u64, Vec<u8>)> = Vec::new();
                let mut closing: Vec<u64> = Vec::new();
                for msg in batch {
                match msg {
                    ToEngine::Open {
                        id,
                        reply,
                        inflight,
                    } => {
                        let mut server = BoltServer::routed(Arc::clone(&resolver), realm, ns);
                        server.set_max_message_bytes(max_message_bytes);
                        // The adapter is the layer that knows what a connection
                        // is, so it supplies the identity the driver sees.
                        server.set_connection_id(id);
                        server.set_trace_statements(trace_statements);
                        server.set_trace_counters(trace_counters);
                        // The trace header's wall figure reads a LIVE clock,
                        // not the per-batch stamp below: inside one statement
                        // that stamp never moves, so every traced statement
                        // reported 0 ms.
                        server.set_trace_clock(std::sync::Arc::new(|| {
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_micros() as i64)
                                .unwrap_or(-1)
                        }));
                        sessions.insert(id, (server, reply, inflight));
                    }
                    ToEngine::Bytes { id, data } => {
                        // `reply` is not used here: output is held and sent
                        // after the batch's fsync, below.
                        let Some((server, _reply, inflight)) = sessions.get_mut(&id) else {
                            // No session: the credit still has to be released or
                            // the reader parks for ever against a counter nobody
                            // will ever decrement.
                            continue;
                        };
                        // Release the reader's credit for these bytes. Done
                        // BEFORE the (possibly slow, possibly panicking) feed, so
                        // a panic cannot strand the credit — the session is torn
                        // down on that path anyway, but a leaked credit on a
                        // shared counter would be a slow poison rather than a
                        // clean failure.
                        inflight.fetch_sub(
                            data.len().min(inflight.load(Ordering::Acquire)),
                            Ordering::AcqRel,
                        );
                        // The adapter injects the wall clock at the last honest
                        // moment: right before the bytes that may read it.
                        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
                            server.graph().set_wall_ms(now.as_millis() as i64);
                        }
                        // PANIC ISOLATION.
                        //
                        // Without this a panic anywhere in the engine unwound
                        // out of the `for msg in wrx` loop and killed the whole
                        // worker THREAD, taking its entire session map with it.
                        // The receiver then dropped, so every later send to that
                        // worker failed — and the accept loop treats a failed
                        // send as `continue`, so 1/N of all future connections
                        // were SILENTLY refused. With the default `workers = 1`
                        // that is the entire server, permanently, while the
                        // process stays alive and the listener keeps accepting.
                        // A liveness probe sees a healthy server.
                        //
                        // The TCK measures that ordinary openCypher does panic
                        // this engine, so this is not a hypothetical path.
                        //
                        // A panic leaves the session's state machine of unknown
                        // validity, so the SESSION is dropped — but the worker,
                        // and every other connection on it, survive.
                        let fed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            server.feed(&data)
                        }));
                        match fed {
                            Ok(Ok(out)) => {
                                // HELD, not sent: released after the batch's
                                // fsync below. A session that said GOODBYE is
                                // removed only after its final bytes go out.
                                if !out.is_empty() {
                                    held.push((id, out));
                                }
                                if server.closed() {
                                    closing.push(id);
                                }
                            }
                            Ok(Err(_)) => {
                                // A wire refusal is terminal for the CONNECTION and
                                // invisible to every other one: drop the session;
                                // the writer's channel closes; the socket closes.
                                sessions.remove(&id);
                            }
                            Err(_) => {
                                // The panic payload has already been reported by
                                // the default hook (stderr), which is the only
                                // diagnostic this server has until structured
                                // logging lands. Drop the session, keep serving.
                                sessions.remove(&id);
                            }
                        }
                    }
                    ToEngine::Closed { id } => {
                        sessions.remove(&id);
                    }
                }
                }
                // ONE fsync for every write in the batch, BEFORE any reply
                // leaves. `Ok(false)` means the batch wrote nothing and cost
                // nothing here.
                //
                // A failed fsync ABORTS THE PROCESS. The batch's writes are
                // already published to the memtable, so readers on every
                // worker can see data that is not on disk and cannot be made
                // so; unwinding N sessions' writes across workers is not
                // possible, and a panic here would only kill THIS worker while
                // the others kept acknowledging against a disk that has
                // stopped. Restarting replays exactly the fsynced prefix, which
                // is the only consistent state left.
                if group_commit {
                    if let Err(e) = store.sync_pending() {
                        eprintln!(
                            "[engram-server] FATAL: WAL fsync failed (durability): {e} — \
                             acknowledged data may not be on disk; aborting so a restart \
                             recovers the durable prefix"
                        );
                        std::process::abort();
                    }
                }
                // Seal on the threshold — AFTER the fsync, so the segment holds
                // only durable versions, and after the batch, so the latch it
                // briefly takes is not in any statement's path. Whichever
                // worker's batch crosses the threshold seals; the others find
                // the tail already empty. Past the segment budget, ask the
                // maintenance thread to compact (it coalesces asks). Paged
                // mode asks on EVERY seal: a spill is cheap and is what keeps
                // RSS bounded, and `compact_after` is a compaction concern.
                // The compaction ask is delete-AWARE, not merely count-based.
                // A segment count cannot tell a store of live rows from one
                // that is mostly deletions waiting to be reclaimed: under a
                // create/delete churn the tombstones accumulate and every
                // scan, prefix walk and `merge_span` keeps paying for them
                // until the count threshold happens to fire. This is the
                // shape RocksDB's `CompactOnDeletionCollector` and Cassandra's
                // `tombstone_threshold` exist for.
                //
                // The ratio counts RESIDENT segments only, so it is a FLOOR
                // over what the store holds — the trigger fires late rather
                // than spuriously. `tombstone_min_versions` keeps a store that
                // holds four rows, three of them tombstones, from compacting
                // on every seal.
                if store.tail_versions() >= seal_after && store.seal().is_some() {
                    let (ratio, versions) = store.tombstone_ratio();
                    let dead_enough =
                        versions >= tombstone_min_versions && ratio > tombstone_ratio;
                    if dead_enough {
                        counters::COMPACTIONS_ASKED_FOR_TOMBSTONES
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if paged || store.segment_count() >= compact_after || dead_enough {
                        let _ = maint_tx.send(Maint::Storage);
                    }
                }
                // Ask for a derived refresh once the commit clock has moved
                // `refresh_after` STAMPS past the last ask (see the field's
                // doc: stamps, not statements). The clock is read AFTER the
                // batch, so the ask covers every write the batch made; the
                // compare-exchange makes one worker the asker for this
                // window. Checked on read-only batches too — it is one atomic
                // load, and a burst's last batch is as likely to be a read.
                // The ask is a channel send AFTER the batch's fsync and
                // before its replies: it neither splits a batch nor syncs.
                if refresh_after > 0 {
                    let ts = store.now_ts();
                    let mark = refresh_mark.load(Ordering::Relaxed);
                    if ts.saturating_sub(mark) >= refresh_after
                        && refresh_mark
                            .compare_exchange(mark, ts, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    {
                        let _ = maint_tx.send(Maint::Refresh);
                    }
                }
                for (id, out) in held {
                    if let Some((_, reply, _)) = sessions.get(&id) {
                        if reply.send(out).is_err() {
                            sessions.remove(&id);
                        }
                    }
                }
                for id in closing {
                    sessions.remove(&id);
                }
            }
        });
    }

    // The accept loop runs on THIS thread and blocks forever, routing every new
    // connection (and the reader/writer it spawns) to its pinned worker.
    //
    // `live` counts connections that hold thread pairs. Each connection costs
    // TWO OS threads, so an unbounded accept loop is an unbounded thread count:
    // ~5k connections is ~10k threads, each with a default stack. The cap turns
    // that from an outage into a refusal.
    let live = Arc::new(AtomicUsize::new(0));
    let mut next_id = 0u64;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };

        if live.load(Ordering::Relaxed) >= cfg.max_connections {
            // Refuse by closing immediately. Accepting and then hanging would
            // look like a slow server rather than a full one, and the client
            // could not tell the difference.
            counted!("server.connection refused at the cap");
            drop(stream);
            continue;
        }

        let id = next_id;
        next_id += 1;
        let w = (id as usize) % workers;
        let _ = stream.set_nodelay(true);
        // Timeouts, so an idle or stalled peer cannot pin its thread pair for
        // ever. `None` disables, which is what the in-process tests want.
        let _ = stream.set_read_timeout(cfg.read_timeout);
        let _ = stream.set_write_timeout(cfg.write_timeout);

        let (reply_tx, reply_rx) = channel::<Vec<u8>>();
        let inflight = Arc::new(AtomicUsize::new(0));
        if worker_txs[w]
            .send(ToEngine::Open {
                id,
                reply: reply_tx,
                inflight: Arc::clone(&inflight),
            })
            .is_err()
        {
            continue;
        }
        match stream.try_clone() {
            Ok(read_half) => {
                live.fetch_add(1, Ordering::Relaxed);
                spawn_reader(
                    id,
                    read_half,
                    worker_txs[w].clone(),
                    cfg.max_inflight_bytes,
                    Arc::clone(&live),
                    inflight,
                );
                spawn_writer(stream, reply_rx);
            }
            Err(_) => {
                let _ = worker_txs[w].send(ToEngine::Closed { id });
            }
        }
    }
    Ok(())
}

/// Read from one socket into the engine channel, with backpressure.
///
/// `inflight` is the bytes this connection has queued to the engine and the
/// engine has not yet consumed. Without it the reader pushed into an unbounded
/// channel as fast as the socket delivered, so one fast client against a busy
/// engine grew the queue until the process died — and because the queue is
/// per-process, that is a denial of service against every OTHER connection too.
///
/// The engine decrements as it consumes, so this is a real credit loop rather
/// than a fixed window: the reader parks only while the engine is genuinely
/// behind, and resumes without a wakeup protocol because the sleep is short and
/// the condition is rechecked.
fn spawn_reader(
    id: u64,
    mut stream: TcpStream,
    tx: Sender<ToEngine>,
    max_inflight: usize,
    live: Arc<AtomicUsize>,
    inflight: Arc<AtomicUsize>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(ToEngine::Closed { id });
                    live.fetch_sub(1, Ordering::Relaxed);
                    return;
                }
                Ok(n) => {
                    while inflight.load(Ordering::Acquire) >= max_inflight {
                        counted!("server.reader parked on backpressure");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    inflight.fetch_add(n, Ordering::AcqRel);
                    if tx
                        .send(ToEngine::Bytes {
                            id,
                            data: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        live.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    });
}

/// What a worker asks the maintenance thread for. Coalesced per wake: a
/// burst of seals is one spill/compaction, a burst of refresh asks one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Maint {
    /// The sealed set crossed its threshold: spill (paged) or compact.
    Storage,
    /// `refresh_after_writes` commits have landed since the last refresh.
    Refresh,
}

/// One derived-structure refresh pass over every graph the resolver has
/// built, logging a line when anything was brought current. The graph list
/// is copied out from under the resolver's lock before any work runs, so a
/// session resolving a new coordinate never waits on a repair.
/// Write every DECLARED range index to a sidecar beside the paged segments,
/// so a restart loads it instead of rebuilding.
///
/// `Graph::persist_indexes` existed and the server never called it, so every
/// restart rebuilt from a partition scan — measured at 43.2 s to warm SF1. The
/// sidecar is safe by construction: `ensure_range_index` DISCARDS one whose
/// vintage has moved, so a stale file costs a rebuild and never a wrong answer.
///
/// Only DECLARED indexes are persisted. Persisting whatever happened to be
/// cached would let one ad-hoc query's index become a permanent cost at every
/// seal; a declared index is the operator saying they want it.
///
/// Called only on a QUIESCENT tick — the serialize is O(index), so doing it on
/// every seal under load would trade a restart cost for a steady-state one.
fn persist_declared_indexes(
    graphs: &Mutex<HashMap<(Realm, Namespace), Arc<Graph>>>,
    dir: &std::path::Path,
) {
    let list: Vec<Arc<Graph>> = graphs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();
    let mut written = 0usize;
    for g in &list {
        let props = g.declared_index_props();
        if props.is_empty() {
            continue;
        }
        let refs: Vec<&str> = props.iter().map(String::as_str).collect();
        match g.persist_indexes(dir, &refs) {
            Ok(n) => written += n,
            // A sidecar that cannot be written is a lost optimisation, not a
            // lost write — the index rebuilds from the store. Say so once
            // rather than failing the maintenance pass.
            Err(e) => eprintln!("[engram-server] index sidecar write failed: {e}"),
        }
    }
    if written > 0 {
        eprintln!("[engram-server] persisted {written} range-index sidecar(s)");
    }
}

/// Monotonic seconds since the first call — the clock the graph's sidecar
/// growth-rewrite interval is measured on. The server owns the clock; the
/// engine takes it as an argument so the simulation can own it there.
fn mono_secs() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs()
}

/// The process's resident set in bytes, from `/proc/self/statm` (Linux, which
/// is where the pod runs). `None` elsewhere, or when it cannot be read — the
/// memory line then prints 0 rather than a guess.
fn process_rss_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages * 4096)
}

fn refresh_derived(graphs: &Mutex<HashMap<(Realm, Namespace), Arc<Graph>>>) {
    let list: Vec<Arc<Graph>> = graphs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();
    let t = std::time::Instant::now();
    let mut total = RefreshReport::default();
    for g in &list {
        total.add(&g.refresh_stale_derived());
    }
    counters::MAINTENANCE_REFRESH_RUNS.fetch_add(1, Ordering::Release);
    // Logged only when something was BROUGHT CURRENT (`any` excludes what
    // was declined or deferred — those left the structure as stale as it
    // was), and the line names only what changed.
    if total.any() {
        eprintln!(
            "[engram-server] derived refresh in {} ms: {}",
            t.elapsed().as_millis(),
            total.describe()
        );
    }
}

/// Spill the sealed set to `dir` through the ONE shared `cache`, logging a
/// line when anything converted. A failure is loud but not fatal: the
/// segments stay resident — memory is unbounded until a spill succeeds — and
/// the next ask or tick retries.
fn spill_and_report(
    store: &Store,
    dir: &std::path::Path,
    cache: &Arc<engram_store::paged::BlockCache>,
) {
    let t = std::time::Instant::now();
    match store.spill_sealed_into(dir, cache) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "[engram-server] spilled {n} segment(s) to paged in {} ms ({} sealed total)",
            t.elapsed().as_millis(),
            store.segment_count()
        ),
        Err(e) => eprintln!(
            "[engram-server] spill FAILED: {e} — sealed segments stay RESIDENT and memory \
             is unbounded until a spill succeeds"
        ),
    }
}

fn spawn_writer(mut stream: TcpStream, rx: Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        for bytes in rx {
            if stream.write_all(&bytes).is_err() {
                return;
            }
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
}
