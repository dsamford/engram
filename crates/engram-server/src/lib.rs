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

/// Apply the five `ENGRAM_ALGO_*` ceilings from `get`.
///
/// # Why this is a function taking a lookup rather than five lines inline
///
/// Every algorithm refusal names one of these — "raise
/// `ENGRAM_ALGO_NODE_CEILING`" — and for a release **nothing read them**. An
/// operator following the message exactly would set the variable, see no
/// change, and have no way to tell the advice was fiction.
///
/// Inline, the repair would be untestable: reading the process environment in
/// a test mutates global state every other test in the binary shares, so the
/// only way to catch a future deletion of the block would be to notice it.
/// Taking the lookup as an argument makes "the variable reaches the ceiling" a
/// property a test can assert against a fake environment — the difference
/// between a lever that is reachable and one that is merely written down.
///
/// A value that does not parse is IGNORED rather than fatal: these raise a
/// safety ceiling, so a typo that leaves the default in place is the
/// conservative failure, and a server refusing to start over a malformed
/// tuning variable is worse than one running at its defaults.
pub fn apply_algo_ceilings(g: &engram_graph::Graph, get: impl Fn(&str) -> Option<String>) {
    let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(v) = num("ENGRAM_ALGO_NODE_CEILING") {
        g.set_algo_node_ceiling(v);
    }
    if let Some(v) = num("ENGRAM_ALGO_EDGE_CEILING") {
        g.set_algo_edge_ceiling(v);
    }
    if let Some(v) = num("ENGRAM_ALGO_BYTE_CEILING") {
        g.set_algo_byte_ceiling(v);
    }
    if let Some(v) = num("ENGRAM_ALGO_WORK_CEILING") {
        g.set_algo_work_ceiling(v);
    }
    if let Some(v) = num("ENGRAM_ALGO_CACHE_BYTES") {
        g.set_algo_cache_bytes(usize::try_from(v).unwrap_or(usize::MAX));
    }
}

/// Process-wide counters for what the maintenance thread does — the
/// `engram_bolt::counters` pattern: the engine's `counted!` traces are
/// thread-local test instruments and the maintenance thread installs none.
pub mod counters {
    use std::sync::atomic::AtomicU64;

    /// Connections dropped because the wire refused the client's bytes.
    ///
    /// The error was discarded as `_` and the session removed, so a client
    /// counting 38,327 dropped connections in one 30 s SF10 `algo-churn` level
    /// met a server log with no error lines at all. The cause could only be
    /// guessed at, and was.
    pub static CONNECTION_WIRE_REFUSALS: AtomicU64 = AtomicU64::new(0);

    /// Connections dropped because the session PANICKED.
    ///
    /// Separate from the wire refusals above because the two are identical from
    /// the client — a dropped connection — and telling them apart is the first
    /// question asked of any drop.
    pub static CONNECTION_PANICS: AtomicU64 = AtomicU64::new(0);

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
    /// What this server ANSWERS when a client asks what it is serving under —
    /// its block-cache budget and the intra-query width it installed.
    /// Default `None`, which sends no HELLO key at all.
    ///
    /// It exists because a benchmark's fairness stamp is typed on the CLIENT's
    /// command line while the server is started somewhere else, so the stamp
    /// can describe a server that is not running and nothing can tell. It has
    /// happened twice here: a Neo4j window stamped an 8 GiB cache against a
    /// pod configured with 10 GiB, and an engram dry run stamped a thread cap
    /// of 6 against a server with no morsel executor installed at all. See
    /// [`engram_bolt::serving`].
    pub serving_hint: Option<engram_bolt::ServingHint>,
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
    /// Whether the maintenance refresh SHARES its row budget across every
    /// stale adjacency table and bounds each repair to its slice. Default
    /// **true**.
    ///
    /// Off is the arm that produced the stress sweep's stall: the budget is
    /// raced for first-come-first-served over a stable iteration order, so the
    /// same table is taken and the same tables deferred every pass, and the one
    /// item the pass cannot defer is taken whole however long it is. Measured
    /// on a 400 s sweep: 109 refreshes totalling 82,748 ms, 55 over 500 ms, the
    /// longest 9,935 ms, with `write-only @ 1` at exactly 0 ops/s for the
    /// second the pass ran in. The flag exists so both arms can be run in ONE
    /// window on ONE binary — see `Graph::set_bounded_derived_repair`.
    pub bounded_derived_repair: bool,

    /// Let a single-node reader repair up to the change log's capacity rather
    /// than declining at 8,192 rows and walking its own span — see
    /// `Graph::set_amortised_reader_repair`. Default false (the shipped
    /// behaviour); the A/B arm for the SF10 write-path collapse, where one query
    /// pays ~1,000 store-wide prefix scans because the decline is cached per
    /// snapshot and every reader behind it walks too.
    pub amortised_reader_repair: bool,
    /// Whether the derived-structure refresh runs on its OWN thread instead of
    /// at the tail of the storage thread's loop. Default **true**.
    ///
    /// Off, the two share one thread and the refresh cannot start until the
    /// storage pass returns — which for a paged store is not a rare event. A
    /// worker asks for storage after EVERY batch (`if paged || …`), and a
    /// store past `compact_after` segments then runs `compact_paged_emitting`,
    /// whose own comment says "the merge runs for minutes". Measured on the
    /// pod: with 99 segments on disk, `refresh_runs` froze at 20 for a whole
    /// 70 s window while `adj_repaired` climbed 58 → 2,282 — the pass did not
    /// come back, and a compaction that retires nothing logs nothing, so
    /// nothing said so. Two five-arm budget sweeps measured a thread that was
    /// not running.
    ///
    /// Splitting them is safe by the contract already in the code rather than
    /// by assertion: `adopt_merged_derived` publishes through
    /// `Slot::publish_snapshot`, which CASes and LOSES to a newer snapshot, and
    /// readers already repair-and-publish concurrently with a running
    /// compaction. The pass does exactly what a reader does.
    pub split_maintenance: bool,
    /// Whether the maintenance pass PRICES a repair from the change logs'
    /// lengths (fix 79) instead of walking every entry of the whole,
    /// untruncated delta and building a `BTreeSet` of every changed node —
    /// once per stale table, under the lock writers record into. Default
    /// **true**. The arm is `--no-cheap-repair-pricing`; see
    /// `Graph::adj_repair_cost_rows` for why the two prices lead to the same
    /// decision wherever they could differ.
    pub cheap_repair_pricing: bool,
    /// Whether a membership catch-up the label's log covers runs regardless
    /// of the refresh pass's row budget (fix 82). Default **true**; the arm is
    /// `--no-unmetered-members-catch-up`. See
    /// `Graph::set_members_unmetered_catch_up` for why metering a catch-up
    /// at one row per entry turned a deferral into a whole-label rebuild.
    pub members_unmetered_catch_up: bool,
    /// Whether a READER's adjacency repair leaves the overlay fold to the
    /// maintenance pass (fix 83). Default **true**; the arm is
    /// `--no-deferred-reader-fold`. See `Graph::set_deferred_reader_fold`
    /// for the 130–240 MB per read statement it removes from the query
    /// threads.
    pub deferred_reader_fold: bool,

    /// `Graph::set_range_fold_at`. 0 keeps the built-in 4,096.
    pub range_fold_at: usize,
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
    /// Whether graph algorithms may split their fixpoint across the worker
    /// pool. Default **true**, and effective only when
    /// `ENGRAM_QUERY_PARALLELISM` has installed one.
    ///
    /// Off, every algorithm runs on the calling thread. The answers are
    /// bit-identical either way — morsels partition the output vertex range
    /// and merge in morsel order — so this measures the split's COST, and is
    /// the arm that makes "parallel equals serial" a claim about the running
    /// binary rather than about a unit test.
    pub algo_parallel: bool,
    /// Whether a declared TRIGRAM index may serve `=~`, `CONTAINS`,
    /// `STARTS WITH` and `ENDS WITH`. Default **true**.
    ///
    /// Off, each of those falls back to the label scan that answered it before
    /// the index existed — which is the A/B arm, and what makes "the index
    /// helps" a measurement rather than an assertion.
    pub trigram_indexes: bool,
    /// Whether a BM25-scored fulltext index may serve a query. Default
    /// **true**.
    ///
    /// Off, the query still gets BM25, computed the slow way by a scan.
    /// Deliberately NOT the same switch as [`ServerConfig::bm25_by_default`]:
    /// "the index is faster" and "BM25 ranks better than term frequency" are
    /// two claims, and one lever could not say which had been measured.
    pub bm25_scoring: bool,
    /// Whether a NEWLY created fulltext index is stamped BM25. Default
    /// **true**.
    ///
    /// Existing indexes are unaffected however this is set: an index's scoring
    /// lives in its catalogue row, stamped when it was created, so a rolling
    /// change of this flag can never re-rank one that already exists.
    pub bm25_by_default: bool,
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
    /// THE FOUR FOLD LEVERS THAT WERE NEVER REACHABLE FROM A SERVER. Each was
    /// a `pipeline.rs` thread-local with a setter used only by unit tests, so
    /// the count fold — the mechanism every LSQB number since v64 runs on —
    /// has never been A/B'd on the pod: its effect was only ever attributed
    /// by comparing two BINARIES. Found by the WCOJ attachment-point review;
    /// the same defect fix 77 closed for `subquery_end_gather`. All four are
    /// thread-locals and are set on every worker, as `order_peak_search` is.
    ///
    /// Whether a `count(*)` over a chain FOLDS its unmaterialised suffix into
    /// a weight instead of expanding every hop. Default **true**;
    /// `--no-count-fold` is the arm.
    pub count_fold: bool,
    /// Whether a fold MEMOISES a var's level when it is a pure function of the
    /// node id. Default **true**; `--no-count-fold-memo` is the arm.
    pub count_fold_memo: bool,
    /// Whether fix 120 re-ranks a var's folded children so the semijoin-shaped
    /// child runs first. Default **true**; `--no-fold-child-order` is the arm.
    pub fold_child_order: bool,
    /// Whether the count-only planner may REORDER a pattern's hops at all.
    /// Default **true**; `--no-count-only-reorder` is the arm — and the arm
    /// `--no-order-peak-search` refines, since peak search chooses among
    /// reorderings this one permits.
    pub count_only_reorder: bool,
    /// Fix 84: whether a fold CLOSE probes a HOISTED copy of the bound node's
    /// row (read once per binding, sorted by peer) instead of the adjacency
    /// table per probe. Default **true**; `--no-fold-hoisted-close` is the
    /// arm. LSQB q3's triangle close is 80.6% of its fold walks, every one a
    /// probe of the same ~36-entry row.
    pub fold_hoisted_close: bool,
    /// Fix 84's hoist threshold: the probes a binding of a close's bound node
    /// answers through the adjacency table before its row is hoisted.
    /// Default `FOLD_HOIST_AFTER_DEFAULT` (8); `--fold-hoist-after N`. `0`
    /// hoists on the first probe.
    pub fold_hoist_after: usize,
    /// Fix 90: whether the count fold breaks a proven pattern SYMMETRY —
    /// enumerates one id order of an interchangeable var set and multiplies
    /// by its size factorial (LSQB q3's triangle: the close hop's walks cut
    /// to a quarter). Default **true**; `--no-fold-symmetry-breaking` is the
    /// arm, whose counts must equal the default's byte for byte.
    pub fold_symmetry_breaking: bool,
    /// Fix 124's arm, wired by fix 91: whether a cached property column may
    /// survive a commit that touched NEITHER its label's epoch nor its
    /// property's, instead of the older rule where any write anywhere retired
    /// every column. Default **true**; `--no-prop-column-epoch-currency` is
    /// the control. It decides whether the columnar fast paths survive a write
    /// stream at all, and until fix 91 it was reachable only from one
    /// integration test — so no pod run had ever put it on the other arm.
    pub prop_column_epoch_currency: bool,
    /// Whether a read-only statement the streaming pipeline refuses as a whole
    /// — a procedure `CALL` in the middle — streams its prefix up to the last
    /// `WITH` before that clause, instead of running every clause on the
    /// materialising loop (SNB BI bi15 at SF10: load 1.0 and 80 GB in a
    /// minute without it). Default **true**; `--no-prefix-streaming` is the
    /// control, whose answers must be the default's.
    pub prefix_streaming: bool,

    /// Push an `all(x IN <var-length rels> WHERE p)` predicate into the
    /// expansion instead of filtering the finished paths. Default **true**;
    /// `--no-rel-predicate-pushdown` restores enumerate-then-filter, which is
    /// the arm to compare against — both must return the same rows.
    pub rel_predicate_pushdown: bool,

    /// Price a both-ends-bound multi-hop path from a measured first hop and a
    /// cached shape tail, instead of the written order. Default **false**: it
    /// wins on BI16 and loses more on BI8, and the per-partial estimate is the
    /// reason (see `Graph::path_estimate`). `--path-estimate` turns it on, so
    /// the trade can be re-measured rather than argued about.
    pub path_estimate: bool,

    /// Honour LDBC FinBench's `truncationLimit` on variable-length hops:
    /// follow only that many edges out of each node, ranked by a relationship
    /// property. Default **false**, and deliberately so — this is the one
    /// lever that CHANGES ANSWERS, and nine of the twelve FinBench complex
    /// reads are defined with it. `--expand-truncation` turns it on; the
    /// query's own `truncationLimit` / `truncationOrder` parameters then
    /// drive it, so a FinBench query needs no rewriting.
    pub expand_truncation: bool,

    /// Fix 93 (strategy O4): the commit-time RE-STAMP for property columns
    /// whose property has no change log. Default FALSE — it is the one
    /// currency lever that can revive a stale column if a write path is
    /// unaccounted, so it is opt-in until it has a differential of its own.
    pub prop_column_restamp: bool,
    /// The property-column cache's byte budget, in MiB. `None` keeps the
    /// built-in [`engram_graph::PROP_COLUMN_BUDGET_BYTES`] (512 MiB);
    /// `Some(0)` turns the cache OFF, which is the arm that prices the whole
    /// columnar family (fixes 18/19/28/33/35/41) against no cache at all —
    /// and a small budget is how the eviction path gets exercised on a real
    /// corpus instead of only in a unit test. Wired by fix 91: the ratchet
    /// found it declared, documented, and unreachable from the server.
    pub prop_column_budget_mb: Option<usize>,
    /// Whether fix 121's bounded end gather is on. Default **true**.
    ///
    /// Off, a label past the whole-label read ceiling declines the vectorised
    /// subquery hop and falls to one projected record read per end, exactly as
    /// before the fix. A thread-local in the pipeline, so it is set on the
    /// worker rather than on the boot thread.
    ///
    /// It exists here because it did not: the lever was declared in
    /// `pipeline.rs` and reachable only from unit tests, so fix 121 could not
    /// be A/B'd on the pod AT ALL. Its halving of plat-optional-count was
    /// therefore attributed by comparing two BINARIES, which credits the whole
    /// delta between them to one fix.
    pub subquery_end_gather: bool,
    /// The label size past which a whole-label column read is declined
    /// (fix 118). Default **262,144** — `WHOLE_LABEL_READ_MAX`.
    ///
    /// The same story as `subquery_end_gather`: declared on `Graph`, wired
    /// nowhere, so the ceiling that fully accounts for plat-optional-count's
    /// remaining 2.2x behind Neo4j could not be moved on the pod to prove it.
    /// `0` leaves the built-in default in place.
    pub whole_label_read_max: u64,
    /// How many start candidates a writing statement's matcher carries
    /// through a path's hops at once (`Graph::set_match_start_chunk`).
    /// `None` keeps the built-in (4,096); `Some(0)` is the A/B arm — every
    /// candidate at once and the WHERE after collection, the shape that took
    /// an SF10 reset (`MATCH (m:Message) WHERE m.id >= $base DETACH DELETE m`,
    /// a delete of nothing) to the OOM killer on 2026-09-27.
    pub match_start_chunk: Option<usize>,
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

/// The stack every thread that parses or evaluates a statement runs on — the
/// engine workers and the morsel workers.
///
/// The parser bounds expression nesting, and the bound is a guarantee only on
/// a stack large enough to REACH it: `engram_cypher::MIN_PARSER_STACK_BYTES`
/// declares that minimum. These threads were spawned at the platform default
/// instead, so the deepest statement the parser accepts overflowed an engine
/// worker — and a stack overflow is not a panic the session boundary catches;
/// it aborts the process, every session with it, on input that needs no
/// credential. The pin is
/// `tests/the_deepest_statement_the_parser_accepts_is_answered_not_a_crash.rs`.
const ENGINE_THREAD_STACK_BYTES: usize = engram_cypher::MIN_PARSER_STACK_BYTES;

/// Spawn an engine worker on [`ENGINE_THREAD_STACK_BYTES`], named, so a panic
/// or an overflow report says which worker rather than `<unknown>`.
fn spawn_engine_thread<F: FnOnce() + Send + 'static>(name: String, f: F) {
    std::thread::Builder::new()
        .name(name)
        .stack_size(ENGINE_THREAD_STACK_BYTES)
        .spawn(f)
        .expect("spawn an engine worker");
}

/// Spawn a morsel worker inside a statement's scope, on
/// [`ENGINE_THREAD_STACK_BYTES`]: morsels evaluate the statement's expressions,
/// to the same nesting the engine worker does.
fn spawn_morsel_thread<'scope, 'env, T, F>(s: &'scope std::thread::Scope<'scope, 'env>, f: F)
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    std::thread::Builder::new()
        .stack_size(ENGINE_THREAD_STACK_BYTES)
        .spawn_scoped(s, f)
        .expect("spawn a morsel worker");
}

/// The production morsel executor (W3 of the scale-and-integrity plan):
/// real OS threads inside a scope, an atomic cursor doling morsels out so a
/// fast worker takes more of them. The ENGINE never spawns — this lives in
/// the server, the designated OS-thread boundary, and is installed through
/// `Graph::set_exec` behind `ENGRAM_QUERY_PARALLELISM`.
/// Morsel workers currently AVAILABLE across the whole process.
///
/// # Why a global budget and not a per-query width
///
/// `ThreadScopeExec` used to spawn `width` threads per statement with nothing
/// coordinating between statements, so C concurrent clients produced up to
/// `C x width` morsel workers against a fixed CPU quota. Measured on the bench
/// pod (6 CPUs, width 6), that is not merely wasteful — it is NEGATIVE past a
/// point:
///
/// | profile | peak | at | past the peak |
/// |---|---|---|---|
/// | `balanced` | 1,760 ops/s | 8 clients | 1,735 @16, 1,675 @32; p99 42 -> 164 ms |
/// | `read-only` | 2,271 | 16 | 2,254 @32; p99 26 -> 87 ms |
///
/// while the WRITE profiles, which do not parallelise the same way and so never
/// multiply, climbed unabated to 32 clients. The same sweep recorded 567
/// throttle events and 20.0 s of CPU throttle, against 12 events / 0.011 s for
/// a single-client battery at the same width — 1,820x more. Reads saturate and
/// then DECLINE; writes do not. One mechanism explains both, and it is
/// oversubscription.
///
/// # Degrade, never block
///
/// A statement takes what is free and runs on that; it does NOT wait for a
/// slot. Blocking would convert oversubscription into queueing delay and move
/// the same cost into p99 — which the table above shows is already the thing
/// that suffers. Taking zero simply means running serially, which is exactly
/// what the engine did before parallelism existed and is always correct: the
/// `ScopedExec` contract is that `for_each` invokes `f` for every index, not
/// that it uses any particular number of threads.
///
/// # The default preserves the analytical win
///
/// The budget defaults to the configured width, so a lone analytical statement
/// still gets the full parallel fold — the 1.3x-5.8x that parallelism is worth
/// on eight of nine LSQB queries — while the second concurrent statement finds
/// the pool empty and runs serially instead of contending for the same cores.
static PARALLEL_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// Set the process-wide morsel budget. Called once, beside the executor.
fn set_parallel_slots(n: usize) {
    PARALLEL_SLOTS.store(n, Ordering::Release);
}

/// Take up to `want` slots, returning how many were actually granted (possibly
/// zero). A CAS loop rather than a semaphore because the contended case must
/// not park: the caller degrades instead of waiting.
fn take_parallel_slots(want: usize) -> usize {
    if want == 0 {
        return 0;
    }
    let mut cur = PARALLEL_SLOTS.load(Ordering::Acquire);
    loop {
        let take = want.min(cur);
        if take == 0 {
            return 0;
        }
        match PARALLEL_SLOTS.compare_exchange_weak(
            cur,
            cur - take,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return take,
            Err(c) => cur = c,
        }
    }
}

/// Hand `n` slots back. Held in a guard so an unwinding `f` cannot leak them —
/// a leaked slot is permanent, and the pool would bleed down to serial for the
/// life of the process.
struct SlotGuard(usize);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            PARALLEL_SLOTS.fetch_add(self.0, Ordering::Release);
        }
    }
}

struct ThreadScopeExec {
    width: usize,
}

impl engram_graph::ScopedExec for ThreadScopeExec {
    fn width(&self) -> usize {
        self.width
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let want = self.width.min(n);
        if want <= 1 {
            for i in 0..n {
                f(i);
            }
            return;
        }
        // Take what the process can spare. Zero is a normal answer under load
        // and means "run this one serially", not "fail".
        let granted = take_parallel_slots(want);
        let _slots = SlotGuard(granted);
        if granted <= 1 {
            for i in 0..n {
                f(i);
            }
            return;
        }
        // THE CALLING THREAD WORKS, AND HELPERS START ONLY ONCE THE RUN HAS
        // OUTLASTED `RAMP_AFTER`. Every granted thread used to be spawned up
        // front, the caller idle, for every call — and a statement makes many
        // calls. On the bench pod that toll was larger than the work of a
        // small parallel step: at width 40 SNB Interactive IC8 took 14 ms and
        // IS7 2 ms, at width 1 9-10 ms and 1 ms; BI bi5 76-84 ms against
        // 54-66 (rev51, probe102). One helper starts at once and waits out the
        // ramp (`ramp`): a run that ends first sends it home unused; a longer
        // one — however long its first morsel is — has it join the work at
        // the ramp and start the rest, two at a time while the unclaimed
        // morsels outnumber the threads on them, so a large run reaches the
        // full grant within a few spawn latencies of the ramp. The grant is
        // held whole until the run ends.
        let run = Run {
            n,
            f,
            cursor: AtomicUsize::new(0),
            live: AtomicUsize::new(1),
            pending: AtomicUsize::new(0),
            spare: AtomicUsize::new(granted - 1),
            done: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        };
        std::thread::scope(|s| {
            let shared = &run;
            if shared.n >= 2 && take_spare(&shared.spare) {
                shared.live.fetch_add(1, Ordering::Relaxed);
                shared.pending.fetch_add(1, Ordering::Relaxed);
                spawn_morsel_thread(s, move || ramp(s, shared));
            }
            work(s, &run, false);
            // no morsel is left to claim: the ramp helper, if it is still
            // waiting, has nothing to join
            *run.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
            run.wake.notify_all();
        });
    }
}

/// One `ThreadScopeExec::for_each` call's shared state.
struct Run<'f> {
    n: usize,
    f: &'f (dyn Fn(usize) + Sync),
    /// The next unclaimed morsel.
    cursor: AtomicUsize,
    /// Threads working the run, the caller included (only ever raised: a
    /// thread that runs out of morsels has nothing left to spawn for).
    live: AtomicUsize,
    /// Helpers started that have not yet made their first claim.
    pending: AtomicUsize,
    /// Helpers the grant still allows.
    spare: AtomicUsize,
    /// Set once the calling thread finds no morsel left to claim.
    done: std::sync::Mutex<bool>,
    /// Wakes the ramp helper when `done` is set.
    wake: std::sync::Condvar,
}

/// How long a run goes on the calling thread alone before helpers join it. A
/// spawn and its join cost tens of microseconds, and a run shorter than a few
/// of them is finished before a helper would have taken a morsel: one of
/// bi5's steps, a few cheap rows a morsel, made every helper pure cost — at
/// width 40 all of its work still ran on the calling thread (probe105).
const RAMP_AFTER: std::time::Duration = std::time::Duration::from_micros(250);

/// Take one helper from the grant, if it allows one.
fn take_spare(spare: &AtomicUsize) -> bool {
    spare
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |k| k.checked_sub(1))
        .is_ok()
}

/// The first helper: wait out the ramp, or until the calling thread has run
/// out of morsels, and then — if any is left — work and start the others.
/// A timed wait on a condition, not a sleep: an early end wakes it at once,
/// so a short run pays one spawn and one join, and nothing else.
fn ramp<'scope, 'env, 'f: 'env>(s: &'scope std::thread::Scope<'scope, 'env>, run: &'env Run<'f>) {
    let done = run.done.lock().unwrap_or_else(|e| e.into_inner());
    let (done, _) = run
        .wake
        .wait_timeout_while(done, RAMP_AFTER, |d| !*d)
        .unwrap_or_else(|e| e.into_inner());
    if *done {
        run.pending.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    drop(done);
    work(s, run, true);
}

/// Claim and run morsels until none is left. A helper (`spawns`) first starts
/// up to two more while the unclaimed morsels outnumber the helpers already
/// started but not yet claiming; the calling thread only works.
///
/// # Why pending helpers, and not every thread on the run
///
/// rev52's rule compared the unclaimed morsels with EVERY live thread. Each
/// new thread claims as soon as it starts, so the claimed count kept pace with
/// the live count and growth stopped at about half the morsels: a run of as
/// many heavy morsels as threads -- a seed split's shares, one per worker --
/// ran on half of them, each taking two. LSQB q9 at SF3 went 1.20 -> 1.46 s
/// (rev49 -> rev52, probe119/probe120), and the same binary with every thread
/// spawned up front read 1.19-1.22 again (probe122). A helper still starts
/// only for a morsel no started helper is about to take, so a short run does
/// not fill the grant; the ramp keeps tiny runs from spawning at all.
fn work<'scope, 'env, 'f: 'env>(
    s: &'scope std::thread::Scope<'scope, 'env>,
    run: &'env Run<'f>,
    spawns: bool,
) {
    // a helper is pending until its first claim
    let mut first = spawns;
    loop {
        if spawns {
            for _ in 0..2 {
                let unclaimed = run.n.saturating_sub(run.cursor.load(Ordering::Relaxed));
                if unclaimed <= run.pending.load(Ordering::Relaxed) || !take_spare(&run.spare) {
                    break;
                }
                run.live.fetch_add(1, Ordering::Relaxed);
                run.pending.fetch_add(1, Ordering::Relaxed);
                spawn_morsel_thread(s, move || work(s, run, true));
            }
        }
        let i = run.cursor.fetch_add(1, Ordering::Relaxed);
        if first {
            run.pending.fetch_sub(1, Ordering::Relaxed);
            first = false;
        }
        if i >= run.n {
            break;
        }
        (run.f)(i);
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

/// The memory this process is actually allowed to use, and where that came
/// from — cgroup v2, cgroup v1, then the machine.
///
/// Container-aware on purpose: a 160 GiB pod on a 192 GiB host and a 4 GiB pod
/// on the same host must not derive the same budget, and `MemTotal` cannot tell
/// them apart. The order mirrors `engram-bench`'s fairness stamp
/// (`report.rs:1844`), which has read these same three sources since 2026-09-09.
fn memory_ceiling_bytes() -> (u64, String) {
    let read_u64 = |p: &str| -> Option<u64> {
        std::fs::read_to_string(p).ok()?.trim().parse::<u64>().ok()
    };
    if let Some(v) = read_u64("/sys/fs/cgroup/memory.max") {
        return (v, "cgroup-v2:memory.max".to_string());
    }
    if let Some(v) = read_u64("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        // v1 writes a sentinel near u64::MAX for "unlimited"; treat it as absent
        // rather than deriving a budget from a number that means "no limit".
        if v < (1u64 << 62) {
            return (v, "cgroup-v1:memory.limit_in_bytes".to_string());
        }
    }
    if let Ok(mi) = std::fs::read_to_string("/proc/meminfo") {
        for line in mi.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|k| k.parse::<u64>().ok())
                {
                    return (kb * 1024, "/proc/meminfo:MemTotal".to_string());
                }
            }
        }
    }
    // Nothing readable (Windows, a sandbox, a stripped image). 8 GiB is the
    // assumption, and it is the CONSERVATIVE direction: it derives a smaller
    // budget than any real bench machine would, so the guard stays tighter
    // rather than looser when it cannot see.
    (8 * 1024 * 1024 * 1024, "assumed (no cgroup or meminfo readable)".to_string())
}

/// One statement's share of that ceiling. The rest belongs to the paged cache,
/// the derived structures (8.5 GB adopted at SF10), and every other session.
const BUDGET_SHARE_DIVISOR: u64 = 4;

/// Assumed bytes per materialised intermediate row. MEASURED, not guessed.
///
/// LSQB q7 at SF10 — the widest real statement this project has, two
/// `OPTIONAL MATCH` clauses over a 30 M-node corpus — was run on 2026-09-11
/// with the budget lifted, while a sampler recorded the server's RSS every
/// ten seconds:
///
/// ```text
/// baseline after warm   29.2 GB
/// peak during q7        53.5 GB
/// attributable to q7    24.3 GB for 331,627,527 rows  =  73 B per row
/// ```
///
/// 96 is that measurement with a ~1.3x margin, and the margin is the whole
/// point: assuming a row costs MORE than it does derives a SMALLER budget,
/// which is the safe direction for a guard whose job is to refuse before the
/// allocator does.
///
/// The first version of this constant was 128, picked by reasoning about
/// `Vec<u64>` widths rather than by measuring. It was safe but too coarse: at
/// 160 GiB it derived 335.5M rows against q7's 331.6M — a 1% margin, which
/// would have passed this query and refused the next slightly larger one. The
/// measurement is what turned a plausible number into a defensible one.
///
/// Sanity check on the other side: at 96 B the derived budget for a 160 GiB
/// pod is 447M rows, and 447M rows at the MEASURED 73 B is 32.6 GB — inside
/// the 43 GB quarter-share the divisor allows. The budget and the memory it
/// stands for agree, which is what makes the proxy honest.
const ASSUMED_BYTES_PER_ROW: u64 = 96;

/// Never refuse a query a laptop could obviously run...
const MIN_AUTO_ROW_BUDGET: u64 = 1_000_000;
/// ...and never derive a number so large it stops being a guard at all.
const MAX_AUTO_ROW_BUDGET: u64 = 4_000_000_000;

/// The row budget this process installs when the operator did not name one.
///
/// # Why this is derived and not a constant
///
/// It WAS a constant — 20,000,000, chosen against "a 2.7M-row corpus". A row
/// count is a proxy for memory, and that proxy was calibrated once, on one
/// machine, and then shipped everywhere. At SF10 it refused a LEGITIMATE query:
/// LSQB q7 (two `OPTIONAL MATCH` clauses, answer 331,627,527) died with
/// "row budget exceeded", on a node with 160 GiB where nothing was close to
/// exhausting anything. A guard that refuses correct work on a machine that
/// could do it is no longer protecting the machine; it is protecting the
/// calibration.
///
/// The alternatives are both worse. Turning it off (`--row-budget 0`) removes
/// the protection entirely and hands the job to the OOM killer, which refuses
/// NOTHING and takes every other session with it. Making the operator tune it
/// moves a number nobody can compute from first principles onto a human who
/// has to re-compute it per scale factor and per pod size.
///
/// So it is derived from the thing it was always a proxy FOR: the memory this
/// process may actually use.
///
/// # The check that says the formula is right
///
/// The derivation must REPRODUCE the old constant on the machine the old
/// constant was chosen for. At the 8–16 GiB the original bench pods carried:
///
/// ```text
/// 8 GiB / 4 / 128 B  = 16.8M rows      (old constant: 20M)
/// 16 GiB / 4 / 128 B = 33.6M rows
/// ```
///
/// The same shape, within a factor of two, on the hardware that produced the
/// 20M — so the constant was not wrong, it was un-generalised. On today's rigs:
///
/// ```text
/// 40 GiB pod  ->  83.9M rows
/// 160 GiB pod -> 335.5M rows
/// ```
///
/// A refusal above those figures is a statement that would genuinely have spent
/// a quarter of the container on one intermediate set, which is what the guard
/// was written to stop.
///
/// The arithmetic is [`derive_row_budget`], separated so it can be tested at
/// ceilings this machine does not have.
#[must_use]
pub fn auto_row_budget() -> (usize, String) {
    let (ceiling, source) = memory_ceiling_bytes();
    let derived = derive_row_budget(ceiling);
    let why = format!(
        "{derived} rows = {} MiB ceiling ({source}) / {BUDGET_SHARE_DIVISOR} / {ASSUMED_BYTES_PER_ROW} B per row",
        ceiling / (1024 * 1024)
    );
    (
        usize::try_from(derived).unwrap_or(usize::MAX),
        why,
    )
}

/// The row budget a process should install, given whatever the operator named.
///
/// # Why this is not left in `main`
///
/// It WAS in `main`, as an if/else around `cfg.row_budget`, and nothing in the
/// test suite reaches `main` — no test spawns the binary. So the arithmetic
/// (`derive_row_budget`) was covered from four angles while the DECISION that
/// consumes it was covered from none, and an SF10 run booted announcing
/// `447392426 rows` and then refused q7 at a budget of `1000000`. The number
/// was computed, printed, and not the one in force.
///
/// Moving it here does not by itself prove the value reaches a session graph —
/// that is what `the_resolved_budget_is_the_one_a_session_graph_gets` asserts —
/// but it puts the branch somewhere a test can see both of its arms.
///
/// `None` in, and the budget is derived from this process's memory ceiling.
/// `Some(0)` in means UNLIMITED, spelled as a number so the flag stays one
/// type; every other `Some` wins as given, including values larger than the
/// derivation would allow, because a reproducible run pins the budget so that
/// two machines refuse at the same row.
#[must_use]
pub fn resolve_row_budget(explicit: Option<usize>) -> (Option<usize>, String) {
    match explicit {
        Some(0) => (None, "unlimited (explicit --row-budget 0)".to_string()),
        Some(b) => (Some(b), format!("{b} (explicit --row-budget)")),
        None => {
            let (budget, why) = auto_row_budget();
            (Some(budget), why)
        }
    }
}

/// Fraction of the ceiling at which the governor starts queueing, and the
/// fraction at which it stops. HYSTERESIS, not one threshold: a single line
/// makes the flag chatter once RSS sits near it, and a statement admitted on
/// one sample and queued on the next gets neither backpressure nor throughput.
const MEMORY_HIGH_WATER_PCT: u64 = 90;
const MEMORY_LOW_WATER_PCT: u64 = 80;

/// How long a statement may wait for memory before it is refused (ms).
///
/// Long enough that an ordinary peak — a few heavy statements finishing — is
/// absorbed as latency; short enough that a corpus which simply does not fit
/// says so rather than hanging.
const MEMORY_QUEUE_WAIT_MS: u64 = 30_000;

/// How often resident memory is sampled.
///
/// A statement can allocate GBs between two samples, which is exactly why the
/// row budget still exists beside this: the governor bounds the PROCESS over
/// time, the row budget bounds ONE statement between samples. Neither
/// subsumes the other.
const MEMORY_SAMPLE_MS: u64 = 100;

/// The memory ceiling this process governs itself against.
///
/// # Why a ceiling rather than a killer
///
/// The bench harness had an external sampler that killed the SERVER at a
/// threshold. That is not a memory policy, it is a crash with better manners:
/// every in-flight statement dies, the store lock is left behind, and the
/// whole remaining run measures a corpse — at SF10 one profile (`contention`)
/// took the process to 122 GiB and voided the eleven profiles queued behind
/// it. A ceiling the process respects itself turns that into latency.
///
/// `None` means UNLIMITED and is a real choice: a dedicated box where the
/// operator would rather have the OOM killer than a refusal. `Some(0)` is not
/// representable for the same reason it is not for the row budget.
///
/// Default: the container's own ceiling, so the server uses the machine it was
/// given rather than a fraction someone guessed.
#[must_use]
pub fn resolve_memory_max(explicit_mb: Option<usize>) -> (Option<u64>, String) {
    match explicit_mb {
        Some(0) => (
            None,
            "unlimited (explicit --memory-max-mb 0) — the OOM killer is the only limit"
                .to_string(),
        ),
        Some(mb) => {
            let b = (mb as u64) * 1024 * 1024;
            (Some(b), format!("{mb} MiB (explicit --memory-max-mb)"))
        }
        None => {
            let (ceiling, source) = memory_ceiling_bytes();
            (
                Some(ceiling),
                format!(
                    "{} MiB ({source}), queueing above {MEMORY_HIGH_WATER_PCT}% and \
                     resuming below {MEMORY_LOW_WATER_PCT}%",
                    ceiling / (1024 * 1024)
                ),
            )
        }
    }
}

/// Sample resident memory and publish the governor's verdict.
///
/// Returns the new state, so a test can drive the transition without a clock
/// or a thread. The hysteresis is the whole logic and is worth testing on its
/// own: above high -> pressure, below low -> clear, BETWEEN THE TWO -> keep
/// whatever it was, which is the part a single threshold gets wrong.
#[must_use]
pub fn memory_governor_step(rss_bytes: u64, ceiling_bytes: u64, was_under_pressure: bool) -> bool {
    if ceiling_bytes == 0 {
        return false;
    }
    let pct = rss_bytes.saturating_mul(100) / ceiling_bytes;
    if pct >= MEMORY_HIGH_WATER_PCT {
        true
    } else if pct <= MEMORY_LOW_WATER_PCT {
        false
    } else {
        was_under_pressure
    }
}

/// The queue deadline, exposed so `main` names it rather than duplicating it.
#[must_use]
pub fn memory_queue_wait_ms() -> u64 {
    MEMORY_QUEUE_WAIT_MS
}

/// Whether `CALL engram.checkpoint()` may run its DERIVED drain — the refresh
/// to a fixed point, the warm, the persist of the derived sidecar — given the
/// resident set and the ceiling the governor last published (MiB; a ceiling of
/// 0 means none is configured, and the drain always runs).
///
/// # Only below half the ceiling
///
/// The drain is an optimisation for the NEXT start (it adopts instead of
/// rebuilding), and it is expensive exactly when it matters: after a large
/// delete it rebuilds memberships and warms every stale adjacency direction
/// while the old tables are still published. On 2026-09-27 at SF10 the stress
/// protocol's reset deleted ~160k messages (64 -> 94.6 GB), and the drain that
/// followed took the process from 98 GB to the 129 GB high-water mark; the
/// kernel then killed it. The four SF10 drains before it started at 53-64 GB of
/// the 140 GiB ceiling and completed. Above half the ceiling the drain is
/// skipped and says so: the next start rebuilds — slower, not dead.
#[must_use]
pub fn derived_drain_has_headroom(rss_mb: u64, max_mb: u64) -> bool {
    max_mb == 0 || rss_mb.saturating_mul(2) <= max_mb
}

/// Start the sampler that drives the governor. Does nothing when unlimited.
pub fn spawn_memory_governor(ceiling: Option<u64>, queue_wait_ms: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(ceiling) = ceiling else {
        engram_graph::interp::MEMORY_QUEUE_MAX_WAIT_MS.store(0, Relaxed);
        return;
    };
    // THE CEILING IS ONLY REAL IF THE RESIDENT SET CAN BE READ.
    //
    // `process_rss_bytes` reads /proc/self/statm. On a platform without it the
    // sampler reads None on every tick, sets nothing, and the governor is a
    // silent no-op — while the boot line above has already announced a ceiling.
    // An operator would configure a limit, see it echoed, and be unprotected.
    // Caught on Windows with `--memory-max-mb 1`, where not one statement was
    // ever queued.
    //
    // So probe ONCE, here, and refuse to be quiet about it.
    if process_rss_bytes().is_none() {
        eprintln!(
            "[engram-server] WARNING: a memory ceiling was configured but this \
             process cannot read its own resident set (no /proc/self/statm). The \
             ceiling CANNOT be enforced and no statement will ever be queued. \
             Run with --memory-max-mb 0 to say so deliberately."
        );
        return;
    }
    engram_graph::interp::MEMORY_MAX_MB.store(ceiling / (1024 * 1024), Relaxed);
    engram_graph::interp::MEMORY_QUEUE_MAX_WAIT_MS.store(queue_wait_ms, Relaxed);
    std::thread::Builder::new()
        .name("memory-governor".to_string())
        .spawn(move || {
            let mut under = false;
            loop {
                if let Some(rss) = process_rss_bytes() {
                    let rss = rss as u64;
                    engram_graph::interp::MEMORY_RSS_MB.store(rss / (1024 * 1024), Relaxed);
                    let now = memory_governor_step(rss, ceiling, under);
                    if now != under {
                        if now {
                            engram_graph::interp::MEMORY_PRESSURE_ENTRIES.fetch_add(1, Relaxed);
                            eprintln!(
                                "[engram-server] memory ceiling reached at {} MiB of {} MiB \
                                 — queueing new statements",
                                rss / (1024 * 1024),
                                ceiling / (1024 * 1024)
                            );
                        } else {
                            eprintln!(
                                "[engram-server] memory back to {} MiB of {} MiB — \
                                 admitting again",
                                rss / (1024 * 1024),
                                ceiling / (1024 * 1024)
                            );
                        }
                        engram_graph::interp::MEMORY_CEILING_REACHED.store(now, Relaxed);
                        under = now;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(MEMORY_SAMPLE_MS));
            }
        })
        .expect("spawn memory governor");
}

/// Bytes one adjacency entry occupies — `SlimAdj` is `rel` + `type_token` +
/// `peer`, padded. Measured against the warm report's own byte accounting.
const ADJ_ENTRY_BYTES: u64 = 24;

/// The share of the container an adjacency table set may hold.
///
/// Sixteenth, not quarter: adjacency is one structure among several (the paged
/// cache, memberships, property columns, per-statement intermediates) and it is
/// the one that persists for the life of the process.
const ADJ_SHARE_DIVISOR: u64 = 16;

/// The adjacency entry budget this container can afford.
///
/// THE CONSTANT IT REPLACES WAS MACHINE-INDEPENDENT. `ADJ_TABLE_MAX_ENTRIES` is
/// `64 << 20` = 67,108,864 entries — about **1.6 GB** — on an 8 GB laptop and on
/// a 160 GiB server alike. SF10 carries 176,623,448 edges, so the UNTYPED
/// adjacency bucket (which holds every type at once) is 2.63x over it, while
/// every one of the fifteen TYPED tables fits comfortably (the largest,
/// HAS_TAG, is 38.4M — 57% of the old budget).
///
/// Before this and the bucket fix beside it, that single overflow declined the
/// whole warm pass: the server printed "warmed in ..." having built NOTHING,
/// and every query then paid its own lazy first build (q4 269.8 s cold against
/// 7.8 s warm; a first index seek 249.7 s). Users pay that on every restart.
///
/// At 160 GiB this derives ~447M entries, which holds SF10's untyped bucket
/// with room to spare. The old constant is the FLOOR, so no machine gets a
/// smaller budget than it had.
#[must_use]
pub fn derive_adj_table_max_entries(ceiling_bytes: u64) -> u64 {
    (ceiling_bytes / ADJ_SHARE_DIVISOR / ADJ_ENTRY_BYTES).max(64 << 20)
}

/// The budget arithmetic, pure so it is testable at any ceiling.
///
/// Clamped at both ends: a container too small to derive a workable budget
/// still gets one a trivial query will not trip, and a host too large does not
/// derive a number so big the guard stops guarding.
#[must_use]
pub fn derive_row_budget(ceiling_bytes: u64) -> u64 {
    (ceiling_bytes / BUDGET_SHARE_DIVISOR / ASSUMED_BYTES_PER_ROW)
        .clamp(MIN_AUTO_ROW_BUDGET, MAX_AUTO_ROW_BUDGET)
}

/// The INTRA-QUERY morsel width this process installs, read in ONE place.
///
/// `ENGRAM_QUERY_PARALLELISM` below 2 installs no executor, and the width a
/// statement then gets is **1** — which is a value, not an absence, and is
/// what a benchmark's fairness stamp has to be checked against.
///
/// One reader on purpose. The server both INSTALLS this width and REPORTS it
/// to a client in HELLO ([`engram_bolt::serving`]), and a second reader is how
/// a server ends up honestly reporting a width it did not install. The
/// engram dry run of 2026-09-08 stamped `thread_cap: 6` from `--workers 6`
/// while this env var was unset, so the real width was 1: `--workers` is the
/// CONNECTION worker count and has nothing to do with what one statement may
/// reach.
#[must_use]
pub fn installed_query_parallelism() -> usize {
    std::env::var("ENGRAM_QUERY_PARALLELISM")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|w| *w > 1)
        .unwrap_or(1)
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: 1,
            serving_hint: None,
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
            bounded_derived_repair: true,
            amortised_reader_repair: false,
            split_maintenance: true,
            cheap_repair_pricing: true,
            members_unmetered_catch_up: true,
            deferred_reader_fold: true,
            range_fold_at: 0,
            property_seek: true,
            algo_parallel: true,
            trigram_indexes: true,
            bm25_scoring: true,
            bm25_by_default: true,
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
            count_fold: true,
            count_fold_memo: true,
            fold_child_order: true,
            count_only_reorder: true,
            fold_hoisted_close: true,
            fold_hoist_after: engram_graph::pipeline::FOLD_HOIST_AFTER_DEFAULT,
            fold_symmetry_breaking: true,
            prop_column_epoch_currency: true,
            prefix_streaming: true,
            rel_predicate_pushdown: true,
            path_estimate: false,
            expand_truncation: false,
            // Fix 93 (O4): opt-in, see the field's doc.
            prop_column_restamp: false,
            prop_column_budget_mb: None,
            subquery_end_gather: true,
            whole_label_read_max: 0,
            match_start_chunk: None,
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
        let bounded_derived_repair = cfg.bounded_derived_repair;
        let amortised_reader_repair = cfg.amortised_reader_repair;
        // Sized to THIS container, not to a constant chosen on another machine.
        let adj_entries = usize::try_from(derive_adj_table_max_entries(memory_ceiling_bytes().0))
            .unwrap_or(usize::MAX);
        let cheap_repair_pricing = cfg.cheap_repair_pricing;
        let members_unmetered_catch_up = cfg.members_unmetered_catch_up;
        let deferred_reader_fold = cfg.deferred_reader_fold;
        let prop_column_epoch_currency = cfg.prop_column_epoch_currency;
        let prefix_streaming = cfg.prefix_streaming;
        let rel_predicate_pushdown = cfg.rel_predicate_pushdown;
        let path_estimate = cfg.path_estimate;
        let expand_truncation = cfg.expand_truncation;
        let prop_column_restamp = cfg.prop_column_restamp;
        let prop_column_budget_mb = cfg.prop_column_budget_mb;
        let whole_label_read_max = cfg.whole_label_read_max;
        let match_start_chunk = cfg.match_start_chunk;
        let property_seek = cfg.property_seek;
        let algo_parallel = cfg.algo_parallel;
        let trigram_indexes = cfg.trigram_indexes;
        let bm25_scoring = cfg.bm25_scoring;
        let bm25_by_default = cfg.bm25_by_default;
        let label_scoped_indexes = cfg.label_scoped_indexes;
        let range_fold_at = cfg.range_fold_at;
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
        let checkpoint_target = cfg.paged_dir.clone().zip(cfg.paged_spill_cache.clone());
        Arc::new(move |r: Realm, n: Namespace| {
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(c.entry((r, n)).or_insert_with(|| {
                let g = Graph::new(store.clone(), r, n);
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
                g.set_bounded_derived_repair(bounded_derived_repair);
                g.set_adj_table_max_entries(adj_entries);
                g.set_amortised_reader_repair(amortised_reader_repair);
                g.set_cheap_repair_pricing(cheap_repair_pricing);
                g.set_members_unmetered_catch_up(members_unmetered_catch_up);
                g.set_deferred_reader_fold(deferred_reader_fold);
                g.set_prop_column_epoch_currency(prop_column_epoch_currency);
                g.set_prefix_streaming(prefix_streaming);
                g.set_rel_predicate_pushdown(rel_predicate_pushdown);
                // Only when asked: the env var seeded it at construction, and
                // a flag that always set it would overwrite that.
                if path_estimate {
                    g.set_path_estimate(true);
                }
                g.set_expand_truncation(expand_truncation);
                g.set_prop_column_restamp(prop_column_restamp);
                // `Some(0)` is a REAL setting (the cache off), so the "unset"
                // case has to be its own value — the `> 0 means set` shape
                // used by whole_label_read_max cannot express it.
                if let Some(mb) = prop_column_budget_mb {
                    g.set_prop_column_budget(mb << 20);
                }
                if whole_label_read_max > 0 {
                    g.set_whole_label_read_max(whole_label_read_max);
                }
                // `Some(0)` is a real setting (the A/B arm), so unset is `None`.
                if let Some(n) = match_start_chunk {
                    g.set_match_start_chunk(n);
                }
                g.set_property_seek(property_seek);
                g.set_trigram_indexes(trigram_indexes);
                g.set_bm25_scoring(bm25_scoring);
                g.set_bm25_by_default(bm25_by_default);
                g.set_label_scoped_indexes(label_scoped_indexes);
                g.set_range_fold_at(range_fold_at);
                g.set_degree_table_after(degree_table_after);
                g.set_adj_overlay_fold(adj_overlay_fold);
                g.set_hop_membership_contains(hop_membership_contains);
                g.set_adj_snap_memo(adj_snap_memo);
                g.set_directed_bound_probe(directed_bound_probe);
                g.set_agg_topk_before_project(agg_topk_before_project);
                g.set_const_projection_fold(const_projection_fold);
                g.set_hop_count_memo(hop_count_memo);
                g.set_members_bitmap_after(members_bitmap_after);
                // THE CEILING LEVERS, made real — see `apply_algo_ceilings`.
                // Outside the parallelism block below, because a refusal
                // ceiling has nothing to do with whether a thread pool was
                // installed.
                apply_algo_ceilings(&g, |k| std::env::var(k).ok());
                // Morsel parallelism (W3): the server is the ONLY production
                // implementor of the engine's ScopedExec seam — the engine
                // itself never spawns. Off unless the operator sets a width.
                let width = installed_query_parallelism();
                if width > 1 {
                    // The PROCESS-WIDE morsel budget, not a per-statement one.
                    // Defaults to the width, so one analytical statement still
                    // gets the full fold while concurrent statements degrade to
                    // serial instead of multiplying into `clients x width`
                    // threads against a fixed CPU quota. `ENGRAM_PARALLEL_SLOTS`
                    // is the A/B arm: setting it equal to `width x clients`
                    // restores the old unbounded behaviour.
                    let slots = std::env::var("ENGRAM_PARALLEL_SLOTS")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|s| *s > 0)
                        .unwrap_or(width);
                    set_parallel_slots(slots);
                    eprintln!(
                        "[engram-server] query parallelism ON: width {width}, \
                         global slot budget {slots}"
                    );
                    g.set_exec(Some(Arc::new(ThreadScopeExec { width })));
                    g.set_parallel_expand(true);
                    // Graph algorithms take the same seam, and are armed here
                    // rather than defaulting on for the same reason expand is:
                    // a run with no width installed must be byte-for-byte the
                    // run that shipped. `--no-algo-parallel` is the A/B arm
                    // WITHIN a parallel run — the only way to show the two
                    // lanes agree on one binary.
                    g.set_algo_parallel(algo_parallel);
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
                let g = Arc::new(g);
                if let Some((dir, spill_cache)) = checkpoint_target.clone() {
                    let store = store.clone();
                    // WEAK: the hook lives inside the graph it drains, and a
                    // strong reference would be a cycle that never drops.
                    let graph = Arc::downgrade(&g);
                    g.set_checkpoint_hook(Some(Arc::new(move || {
                        let t = std::time::Instant::now();
                        // THE DRAIN — what a graceful stop (the preStop's
                        // `CALL engram.checkpoint()`) needs for the NEXT start
                        // to adopt the derived bases instead of rebuilding
                        // them. It used to seal and spill only, so the next
                        // boot replayed the WAL's tail, sealed it into a new
                        // segment, and refused the sidecar as describing a
                        // different sealed set: every restart after a write
                        // rebuilt everything (64-127 s at SF3, 128-300 s at
                        // SF10 on 2026-09-27).
                        //
                        // 1. Seal whatever the tail holds (a no-op on an
                        //    empty tail).
                        let sealed = store.seal().is_some();
                        // 2. Spill EVERY resident sealed segment, and
                        //    checkpoint the WAL behind them so the next boot
                        //    replays nothing into a fresh segment. The spill
                        //    is idempotent and takes the swap latch, so a
                        //    maintenance spill or compaction in flight is
                        //    neither raced nor waited for.
                        let (spilled, below) = store
                            .spill_sealed_into_reporting(&dir, &spill_cache)
                            .map_err(|e| format!("spill failed: {e}"))?;
                        let mut wal = String::from("unchanged");
                        if let (Some(below), true) = (below, store.has_wal()) {
                            match store.checkpoint_wal(below) {
                                Ok(dropped) => {
                                    wal = format!(
                                        "checkpointed below seq {below} ({dropped} record(s) dropped)"
                                    );
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
                                Err(e) => {
                                    wal = format!(
                                        "checkpoint FAILED: {e} (durability intact; the WAL keeps \
                                         its prefix)"
                                    );
                                }
                            }
                        }
                        // 3. Bring every derived structure current — the
                        //    sidecar is written only when nothing published
                        //    is stale — and 4. persist it, named for the
                        //    sealed set the next boot will find.
                        //    ONLY WITH HEADROOM: see `derived_drain_has_headroom`.
                        let (rss_mb, max_mb) = (
                            engram_graph::interp::MEMORY_RSS_MB
                                .load(std::sync::atomic::Ordering::Relaxed),
                            engram_graph::interp::MEMORY_MAX_MB
                                .load(std::sync::atomic::Ordering::Relaxed),
                        );
                        let drain = derived_drain_has_headroom(rss_mb, max_mb);
                        if !drain {
                            eprintln!(
                                "[engram-server] checkpoint: derived drain SKIPPED -- resident set \
                                 {rss_mb} MiB is over half the {max_mb} MiB ceiling; the next start \
                                 rebuilds the derived structures instead"
                            );
                        }
                        let (mut passes, mut persisted) = (0usize, false);
                        if let Some(g) = graph.upgrade().filter(|_| drain) {
                            loop {
                                let r = g.refresh_stale_derived();
                                passes += 1;
                                let left =
                                    r.adjacency_deferred + r.adjacency_declined + r.members_deferred;
                                if left == 0 || passes >= 64 {
                                    break;
                                }
                            }
                            // A table the pass cannot repair (it never
                            // REBUILDS an untyped one) would leave a stale
                            // base published, and the persist refuses those.
                            // `warm` rebuilds exactly the directions still
                            // stale and keeps every current one.
                            let _ = g.warm();
                            persisted = g.persist_derived_at_stop(&dir, mono_secs());
                        }
                        // What is reported is read AFTER all of it, so a
                        // writer landing meanwhile shows in `tail` instead of
                        // hiding behind this call's own seal.
                        let report = engram_graph::CheckpointReport {
                            spilled,
                            segments: store.segment_count(),
                            resident: store.resident_segment_count(),
                            tail: store.tail_versions(),
                        };
                        eprintln!(
                            "[engram-server] checkpoint in {} ms: sealed the tail={sealed}, spilled {} \
                             segment(s), WAL {wal}; {passes} refresh pass(es), derived bases \
                             persisted={persisted}; {} sealed, {} still resident, {} version(s) in \
                             the tail",
                            t.elapsed().as_millis(),
                            report.spilled,
                            report.segments,
                            report.resident,
                            report.tail
                        );
                        Ok(report)
                    })));
                }
                g
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
    // It USED TO ALSO OWN the derived-structure refresh, and that is fix 78.
    //
    // The refresh is O(the delta), it fires on a worker's ask (every
    // `refresh_after_writes` commits) and on every tick, and it exists to do
    // inline what the next reader would otherwise do. The storage work beside
    // it is O(CORPUS). Sharing one thread means the refresh cannot start until
    // the storage pass returns, and for a paged store that is not a rare
    // event: a worker asks for storage after EVERY batch (`if paged || …`
    // below the `refresh_after` block), and a store past `compact_after`
    // segments then runs `compact_paged_emitting`, whose own comment reads
    // "the merge runs for minutes".
    //
    // Measured on the bench pod, SF1 paged, 99 segments on disk: through a
    // 70 s window `refresh_runs` was frozen at 20 while `adj_repaired` climbed
    // 58 -> 2,282. `MAINTENANCE_REFRESH_RUNS` is incremented at the END of
    // `refresh_derived`, so a frozen counter is not "the pass ran and found
    // nothing" — a pass that finds nothing still increments it. The pass did
    // not come back. A compaction that retires nothing prints nothing, so no
    // log line said so, and two five-arm budget sweeps measured a thread that
    // was not running.
    //
    // So the refresh gets its OWN thread and its own ask channel. It is safe
    // by the contract already in the code rather than by assertion:
    // `adopt_merged_derived` publishes through `Slot::publish_snapshot`, which
    // CASes and LOSES to a newer snapshot, and readers already
    // repair-and-publish concurrently with a running compaction. The pass does
    // exactly what a reader does. `--no-split-maintenance` puts it back at the
    // tail of the storage loop, which is the arm the measurement compares
    // against.
    let (maint_tx, maint_rx): (Sender<Maint>, Receiver<Maint>) = channel();
    let (refresh_tx, refresh_rx): (Sender<Maint>, Receiver<Maint>) = channel();
    let split_maintenance = cfg.split_maintenance;
    if cfg.derived_refresh && split_maintenance {
        let graphs = Arc::clone(&cache);
        let tick = cfg.maintenance_tick.max(Duration::from_millis(1));
        std::thread::spawn(move || {
            loop {
                // Ask or tick — both mean the same thing here, so neither is
                // distinguished. A burst of asks is still ONE pass: every ask
                // already queued is drained into this wake.
                match refresh_rx.recv_timeout(tick) {
                    Ok(_) => while refresh_rx.try_recv().is_ok() {},
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                refresh_derived(&graphs);
            }
        });
    }
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
                            let dense =
                                versions >= tombstone_min_versions && ratio > tombstone_ratio;
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
                                last_compaction
                                    .is_none_or(|t: std::time::Instant| t.elapsed() >= iv)
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
                                            "[engram-server] {} paged-compacted in {} ms: {retired} \
                                             version(s) retired, {dropped} key(s) dropped, {} \
                                             segment(s) remain",
                                            unix_ms_tag(),
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
                                    "[engram-server] persisted the derived bases for \
                                     {wrote} graph(s)"
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
                            "[engram-server] {} released {dropped} in-memory log entry(ies) \
                             below seq {upto} (durable via {})",
                            unix_ms_tag(),
                            if paged.is_some() && !store.has_wal() {
                                "sealed segments"
                            } else {
                                "the WAL"
                            }
                        );
                    }
                }
                // THE LEVER-OFF ARM. With the split on, the refresh runs on
                // its own thread and `refresh` never arrives here — the ask is
                // routed to the other channel — so this is dead in the default
                // configuration and is the shape the measurement compares
                // against, not a second place the pass can run from.
                if derived_refresh && !split_maintenance && (refresh || ticked) {
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
    // The directory the PROPERTY WORKING SET is persisted into, so the next
    // boot can rebuild the columns this run proved it needs. Beside the store,
    // not in an operator-managed location, so it travels with the data: a pod
    // that moves its volume keeps its warm-up, and a store restored elsewhere
    // does not warm from another deployment's workload.
    let warmset_dir = cfg.paged_dir.clone();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering::Relaxed;
        let mut last = String::new();
        let mut last_memory: [usize; 6] = [usize::MAX; 6];
        // ENGRAM_STATS_SECS overrides the 30 s cadence, floor 1 s.
        //
        // 30 s is right for a long run's log and USELESS for a stall: the SF10
        // write-path collapse is a ~3 s blocking event (p99.9 2.2-2.9 s, max
        // 3.5-5.6 s), so a 30 s sample cannot say which subsystem moved during
        // the second that collapsed. At 1 s the per-second throughput the stress
        // harness reports can be aligned against per-second counter deltas, and
        // whichever counter jumps in a stalled second names the subsystem.
        // Twelve hypotheses have been falsified from outside; this is the
        // cheapest way to stop guessing.
        let stats_secs = std::env::var("ENGRAM_STATS_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30)
            .max(1);
        // The MEMORY REPORT is the expensive half and must NOT ride the fast
        // cadence. At ENGRAM_STATS_SECS=1 on SF10 it walked every graph's
        // structures once a second and drove the server to 124 GiB at only 8
        // clients -- with a 1M row budget that should need ~8 GB, and the same
        // budget had completed a full 32-client ladder. The instrument caused
        // it. Counters are plain atomic loads and are free; the report is not,
        // so it keeps its own 30 s floor however fast the counters are polled.
        let mut since_report = u64::MAX; // force one on the first iteration
        loop {
            std::thread::sleep(std::time::Duration::from_secs(stats_secs));
            since_report = since_report.saturating_add(stats_secs);
            let want_report = since_report >= 30;
            if want_report {
                since_report = 0;
            }
            if want_report {
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
                        "[engram-server] {} memory: cache {}/{} MB, adjacency {} MB in {} table(s), memberships {} MB in {} label(s), range indexes {} MB in {} index(es), property columns {} MB in {} column(s); rss {} MB, unattributed {} MB",
                        unix_ms_tag(),
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
                // PERSIST THE WORKING SET ON THE SAME CYCLE.
                //
                // Written periodically rather than at shutdown because a
                // database is not always stopped politely: the bench harness
                // SIGKILLs the server after every ceiling hit, and a
                // shutdown-only write would have recorded nothing in exactly
                // the runs that most needed warming. A file up to one cycle
                // stale costs a few columns; a file that is never written
                // costs every column.
                if let Some(dir) = &warmset_dir {
                    let mut want: Vec<(String, String, bool)> = Vec::new();
                    for g in &graphs {
                        want.extend(g.cached_prop_columns());
                    }
                    want.sort();
                    want.dedup();
                    if !want.is_empty() {
                        let body: String = want
                            .iter()
                            .map(|(l, p, pres)| {
                                format!("{l}\t{p}\t{}\n", u8::from(*pres))
                            })
                            .collect();
                        // temp + rename, so a boot never reads a half-written
                        // set — the failure that would warm an arbitrary
                        // prefix and call it the working set
                        let tmp = dir.join("warmset.tsv.tmp");
                        let dst = dir.join("warmset.tsv");
                        if std::fs::write(&tmp, body).is_ok() {
                            let _ = std::fs::rename(&tmp, &dst);
                        }
                    }
                }
            }
            let w: Vec<u64> = engram_bolt::counters::WON_AT
                .iter()
                .map(|c| c.load(Relaxed))
                .collect();
            let line = format!(
                "txn_conflicts={} autocommit_reruns={} won@1={} won@2={} won@3-4={} \
                 won@5-8={} won@9+={} max_attempts={} escalations={} escalated_losses={} \
                 fsyncs={} val_window={} val_fellback={} val_fallback_keys={} seed_scan_fallbacks={} adj_tbl={} adj_walk={} walk_overlaid={} walk_nocur={} walk_idcap={} stale_declined={} adj_repair_beat_walk={} span_stopped={} scoped_rejected={} adj_built={} adj_repaired={} derived_refreshed={} refresh_runs={}                  span_excl={} span_free={} span_rows_excl={}                  stale_served={} stale_declined={}                  idx_builds={} idx_catchups={} idx_folds={}                  mem_caught={} mem_built={} mem_folds={} mem_flat={} mem_flat_rows={} mem_probes={} mem_bitmaps={} seed_scan_rows={}                  conn_wire_refused={} conn_panicked={}                  paged_preads={} paged_misses={} paged_evicted={} logs_poisoned={}",
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
                // The SF10 write-gap discriminators. See VALIDATE_FROM_WINDOW.
                engram_store::VALIDATE_FROM_WINDOW.load(Relaxed),
                engram_store::VALIDATE_FELL_BACK.load(Relaxed),
                engram_store::VALIDATE_FALLBACK_KEYS.load(Relaxed),
                engram_graph::pipeline::ANCHORED_SEED_FELL_BACK_TO_SCAN.load(Relaxed),
                // The SF10 write-path discriminators: which reason sends a hop
                // to the prefix walk. See ADJ_SERVED_BY_TABLE in engram-graph.
                engram_graph::ADJ_SERVED_BY_TABLE.load(Relaxed),
                engram_graph::ADJ_FELL_TO_WALK.load(Relaxed),
                engram_graph::ADJ_WALK_OVERLAID.load(Relaxed),
                engram_graph::ADJ_WALK_NO_CURRENT_TABLE.load(Relaxed),
                engram_graph::ADJ_WALK_ID_CEILING.load(Relaxed),
                // Separates "table stale, repair DECLINED" from "no table exists
                // yet" — `walk_nocur` conflates them, which made two A/Bs null.
                engram_graph::counters::ADJ_STALE_DECLINED_TO_WALK.load(Relaxed),
                engram_graph::counters::ADJ_REPAIR_BEAT_THE_WALK.load(Relaxed),
                // The two handoff fixes' engagement counters. A fix that never
                // fires is the failure mode two earlier attempts had.
                engram_store::SPAN_STOPPED_EARLY.load(Relaxed),
                engram_graph::counters::SCOPED_INDEX_FOREIGN_ROW_REJECTED.load(Relaxed),
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
                counters::CONNECTION_WIRE_REFUSALS.load(Relaxed),
                counters::CONNECTION_PANICS.load(Relaxed),
                // The paged read path, previously visible only to a `Trace`.
                // A stall no printed counter explained is why these are here:
                // every counter above was flat or LOWER in a stall second, so
                // the time was in a path this dump could not see, and on a
                // paged store block I/O is the first place to look. See
                // `engram_store::PAGED_PREADS`.
                engram_store::PAGED_PREADS.load(Relaxed),
                engram_store::PAGED_BLOCK_MISSES.load(Relaxed),
                engram_store::PAGED_BLOCK_EVICTIONS.load(Relaxed),
                // Change logs cleared wholesale by a stamp below a published
                // snapshot — the path that forces a from-scratch rebuild and
                // had no global counter. See `engram_store::CHANGE_LOGS_POISONED`.
                engram_store::CHANGE_LOGS_POISONED.load(Relaxed),
            );
            if line != last {
                eprintln!("[engram-server] {} counters: {line}", unix_ms_tag());
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
             {} adjacency table(s) holding {} MB in {} MB allocated;              labels={} range={} composite={} trigram={} fulltext={} counts={}",
            t.elapsed().as_millis(),
            w.nodes,
            w.out_edges,
            w.in_edges,
            w.tables,
            w.table_bytes >> 20,
            w.table_capacity_bytes >> 20,
            // ENGAGEMENT, not intent. Every structure warming claims to build
            // is counted where it is built, so a pass that enumerated an empty
            // catalogue and a pass that built nothing are distinguishable from
            // the boot line alone -- which is the whole reason the adjacency
            // defect above went unnoticed for so long.
            engram_graph::counters::WARM_LABEL_MEMBERSHIPS.load(std::sync::atomic::Ordering::Relaxed),
            engram_graph::counters::WARM_INDEXES_BUILT.load(std::sync::atomic::Ordering::Relaxed),
            engram_graph::counters::WARM_COMPOSITE_INDEXES.load(std::sync::atomic::Ordering::Relaxed),
            engram_graph::counters::WARM_TRIGRAM_INDEXES.load(std::sync::atomic::Ordering::Relaxed),
            engram_graph::counters::WARM_TERM_INDEXES.load(std::sync::atomic::Ordering::Relaxed),
            engram_graph::counters::WARM_STATS.load(std::sync::atomic::Ordering::Relaxed),
        );

        // THE PROPERTY WORKING SET, rebuilt from what the last run actually
        // read.
        //
        // Everything above is derived TOPOLOGY. Queries filter on PROPERTIES,
        // and until this ran the first query to touch a `(label, prop)` pair
        // built its column on the querying client's thread — measured on SNB
        // BI at SF3 as a 108 s first query where the next one, doing strictly
        // more work, took 40 s.
        //
        // The set is not speculative: it is the columns resident in the cache
        // when the last run wrote the file, which are the survivors of a
        // budgeted LRU against everything that run read. Warming the whole
        // `(label x property)` space instead would thrash the same budget and
        // finish holding an arbitrary tail — the mistake `Graph::warm`'s own
        // comments record twice, where warming what the workload does not use
        // looked like it had worked.
        if let Some(dir) = cfg.paged_dir.as_ref() {
            let want: Vec<(String, String, bool)> = std::fs::read_to_string(dir.join("warmset.tsv"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let mut f = l.split('\t');
                    let label = f.next()?.to_string();
                    let prop = f.next()?.to_string();
                    let presence = f.next()? == "1";
                    (!label.is_empty() && !prop.is_empty()).then_some((label, prop, presence))
                })
                .collect();
            if !want.is_empty() {
                let t = std::time::Instant::now();
                let kept = warm_graph.warm_prop_columns(&want);
                eprintln!(
                    "[engram-server] property working set: {kept} of {} column(s) rebuilt in {} ms",
                    want.len(),
                    t.elapsed().as_millis()
                );
            }
        }
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
    // THE FLUSHER (see `spawn_flusher`): under group commit, the one fsync
    // path. Without group commit every write fsyncs inline, and there is
    // nothing to hand off.
    let seal_policy = SealPolicy {
        seal_after: cfg.seal_after_versions.max(1),
        compact_after: cfg.compact_after_segments.max(2),
        tombstone_ratio: cfg.tombstone_ratio,
        tombstone_min_versions: cfg.tombstone_min_versions,
        paged: cfg.paged_dir.is_some(),
    };
    let flush_tx: Option<Sender<FlushJob>> = cfg
        .group_commit
        .then(|| spawn_flusher(store.clone(), seal_policy, maint_tx.clone()));
    let mut worker_txs: Vec<Sender<ToEngine>> = Vec::with_capacity(workers);
    for worker_index in 0..workers {
        let (wtx, wrx): (Sender<ToEngine>, Receiver<ToEngine>) = channel();
        worker_txs.push(wtx);
        let resolver = Arc::clone(&resolver);
        let max_message_bytes = cfg.max_message_bytes;
        let serving_hint = cfg.serving_hint;
        let store = store.clone();
        let flush_tx = flush_tx.clone();
        // This worker's jobs not yet released by the flusher (see `FlushJob`).
        let outstanding = Arc::new(AtomicUsize::new(0));
        let maint_tx = maint_tx.clone();
        let refresh_tx = refresh_tx.clone();
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
        // Permission, not a diagnostic: `ENGRAM_TRACE_MARKER=1` lets a CLIENT
        // trace one statement by prefixing `/* engram:trace */`. Off by
        // default, because the marker is the client's choice and the cost —
        // an order of magnitude on a traced statement, and its text in this
        // log — is the server's (security plan §2.13).
        let trace_marker = std::env::var_os("ENGRAM_TRACE_MARKER").is_some();
        let order_peak_search = cfg.order_peak_search;
        let count_fold = cfg.count_fold;
        let count_fold_memo = cfg.count_fold_memo;
        let fold_child_order = cfg.fold_child_order;
        let count_only_reorder = cfg.count_only_reorder;
        let fold_hoisted_close = cfg.fold_hoisted_close;
        let fold_hoist_after = cfg.fold_hoist_after;
        let fold_symmetry_breaking = cfg.fold_symmetry_breaking;
        let subquery_end_gather = cfg.subquery_end_gather;
        spawn_engine_thread(format!("engine-{worker_index}"), move || {
            // The ordering search's lever is a THREAD-LOCAL (the pipeline's
            // levers all are), so it must be set on every worker that plans a
            // statement — setting it once on the boot thread would leave every
            // query running the default.
            engram_graph::pipeline::set_order_peak_search(order_peak_search);
            engram_graph::pipeline::set_count_fold(count_fold);
            engram_graph::pipeline::set_count_fold_memo(count_fold_memo);
            engram_graph::pipeline::set_fold_child_order(fold_child_order);
            engram_graph::pipeline::set_count_only_reorder(count_only_reorder);
            engram_graph::pipeline::set_fold_hoisted_close(fold_hoisted_close);
            engram_graph::pipeline::set_fold_hoist_after(fold_hoist_after);
            engram_graph::pipeline::set_fold_symmetry_breaking(fold_symmetry_breaking);
            engram_graph::pipeline::set_subquery_end_gather(subquery_end_gather);
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
                            // What this server is serving under, answered to
                            // the client rather than typed on its command
                            // line. Absent unless configured, so a HELLO from
                            // an unconfigured server is unchanged.
                            if let Some(hint) = serving_hint {
                                server.set_serving_hint(hint);
                            }
                            // The adapter is the layer that knows what a connection
                            // is, so it supplies the identity the driver sees.
                            server.set_connection_id(id);
                            server.set_trace_statements(trace_statements);
                            server.set_trace_counters(trace_counters);
                            server.set_trace_marker(trace_marker);
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
                            let fed =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                                Ok(Err(e)) => {
                                    // A wire refusal is terminal for the CONNECTION and
                                    // invisible to every other one: drop the session;
                                    // the writer's channel closes; the socket closes.
                                    //
                                    // IT IS NOT INVISIBLE TO THE OPERATOR. The error
                                    // used to be discarded as `_`, so a client counting
                                    // 38,327 dropped connections across one 30 s SF10
                                    // `algo-churn` level found a server log with ZERO
                                    // error lines in it. A drop nothing records cannot
                                    // be diagnosed, only guessed at — and it was.
                                    //
                                    // Counted ALWAYS, printed SPARSELY: at this rate a
                                    // line per drop is 38k lines that bury the run's
                                    // other output and cost more than the drops do.
                                    let n = counters::CONNECTION_WIRE_REFUSALS
                                        .fetch_add(1, Ordering::Relaxed)
                                        + 1;
                                    if n <= 5 || n % 1000 == 0 {
                                        eprintln!(
                                            "[engram-server] connection {id} refused at the \
                                             wire and dropped (#{n}): {e}"
                                        );
                                    }
                                    sessions.remove(&id);
                                }
                                Err(_) => {
                                    // The panic payload has already been reported by
                                    // the default hook (stderr), which is the only
                                    // diagnostic this server has until structured
                                    // logging lands. Drop the session, keep serving.
                                    //
                                    // Counted anyway: the hook's output and a wire
                                    // refusal look the same from the client (a dropped
                                    // connection), and separating them is the first
                                    // question asked of any drop.
                                    counters::CONNECTION_PANICS.fetch_add(1, Ordering::Relaxed);
                                    sessions.remove(&id);
                                }
                            }
                        }
                        ToEngine::Closed { id } => {
                            sessions.remove(&id);
                        }
                    }
                }
                // Ask for a derived refresh once the commit clock has moved
                // `refresh_after` STAMPS past the last ask (see the field's
                // doc: stamps, not statements). The clock is read AFTER the
                // batch, so the ask covers every write the batch made; the
                // compare-exchange makes one worker the asker for this
                // window. Checked on read-only batches too — it is one atomic
                // load, and a burst's last batch is as likely to be a read.
                // The ask is a channel send: it neither splits a batch nor
                // syncs, and it needs no durability.
                if refresh_after > 0 {
                    let ts = store.now_ts();
                    let mark = refresh_mark.load(Ordering::Relaxed);
                    if ts.saturating_sub(mark) >= refresh_after
                        && refresh_mark
                            .compare_exchange(mark, ts, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    {
                        // FIX 78: to the thread that owns the refresh. With
                        // the split on that is a channel of its own, so the
                        // ask cannot queue behind a compaction the storage
                        // thread is in the middle of.
                        let _ = if split_maintenance {
                            refresh_tx.send(Maint::Refresh)
                        } else {
                            maint_tx.send(Maint::Refresh)
                        };
                    }
                }
                // DURABILITY, THEN THE REPLIES. No reply leaves before every
                // record appended up to the end of this batch is on disk —
                // including other workers' records this batch's READS may
                // have observed (a write is visible on append, before its
                // fsync), which is why `need` is the log's length now and not
                // just this batch's own appends.
                //
                // Under group commit the fsync belongs to the FLUSHER: when
                // `need` is already durable and nothing of this worker's is
                // still queued there, the replies go at once (the old "covered
                // by another worker" exit, minus the wait on the mutex a
                // running fsync holds); otherwise they are handed over and
                // this worker takes its next batch. Without group commit every
                // write fsynced inline, so the replies go now and the seal
                // check runs here.
                let direct = match flush_tx.as_ref() {
                    None => {
                        seal_policy.seal_if_due(&store, &maint_tx);
                        true
                    }
                    Some(_) if held.is_empty() => true,
                    Some(_) => {
                        let need = store.log_len();
                        outstanding.load(Ordering::Acquire) == 0 && store.durable_seq() >= need
                    }
                };
                if direct {
                    for (id, out) in held {
                        if let Some((_, reply, _)) = sessions.get(&id) {
                            if reply.send(out).is_err() {
                                sessions.remove(&id);
                            }
                        }
                    }
                } else if let Some(ftx) = flush_tx.as_ref() {
                    let out: Vec<(Sender<Vec<u8>>, Vec<u8>)> = held
                        .into_iter()
                        .filter_map(|(id, bytes)| {
                            sessions.get(&id).map(|(_, reply, _)| (reply.clone(), bytes))
                        })
                        .collect();
                    outstanding.fetch_add(1, Ordering::AcqRel);
                    if ftx
                        .send(FlushJob {
                            out,
                            outstanding: Arc::clone(&outstanding),
                        })
                        .is_err()
                    {
                        // The flusher only stops with the process; a failed
                        // hand-off leaves these writes unacknowledged, never
                        // falsely acknowledged.
                        outstanding.fetch_sub(1, Ordering::AcqRel);
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

/// One worker's held replies, handed to the FLUSHER: each is released only
/// after an fsync that covers every record appended before the hand-off.
struct FlushJob {
    /// The replies, in the order the worker produced them, each with its
    /// connection's writer channel (a clone: the session may close meanwhile,
    /// and the clone keeps its writer alive until these bytes are sent).
    out: Vec<(Sender<Vec<u8>>, Vec<u8>)>,
    /// The handing worker's count of jobs not yet released. The worker sends
    /// its next batch's replies through the flusher too while this is
    /// non-zero, so no reply ever overtakes an earlier one on its connection.
    outstanding: Arc<AtomicUsize>,
}

/// When to seal the tail and ask for a spill or compaction. Checked AFTER an
/// fsync, so a sealed segment holds only durable versions.
#[derive(Debug, Clone, Copy)]
struct SealPolicy {
    seal_after: usize,
    compact_after: usize,
    tombstone_ratio: f64,
    tombstone_min_versions: u64,
    paged: bool,
}

impl SealPolicy {
    /// Seal on the threshold. Whichever caller crosses it seals; the others
    /// find the tail already empty. Past the segment budget, ask the
    /// maintenance thread to compact (it coalesces asks). Paged mode asks on
    /// EVERY seal: a spill is cheap and is what keeps RSS bounded, and
    /// `compact_after` is a compaction concern.
    ///
    /// The compaction ask is delete-AWARE, not merely count-based. A segment
    /// count cannot tell a store of live rows from one that is mostly
    /// deletions waiting to be reclaimed: under a create/delete churn the
    /// tombstones accumulate and every scan, prefix walk and `merge_span`
    /// keeps paying for them until the count threshold happens to fire. This
    /// is the shape RocksDB's `CompactOnDeletionCollector` and Cassandra's
    /// `tombstone_threshold` exist for. The ratio counts RESIDENT segments
    /// only, so it is a FLOOR over what the store holds — the trigger fires
    /// late rather than spuriously — and `tombstone_min_versions` keeps a
    /// store of four rows, three of them tombstones, from compacting on every
    /// seal.
    fn seal_if_due(&self, store: &Store, maint_tx: &Sender<Maint>) {
        if store.tail_versions() >= self.seal_after && store.seal().is_some() {
            let (ratio, versions) = store.tombstone_ratio();
            let dead_enough =
                versions >= self.tombstone_min_versions && ratio > self.tombstone_ratio;
            if dead_enough {
                counters::COMPACTIONS_ASKED_FOR_TOMBSTONES.fetch_add(1, Ordering::Relaxed);
            }
            if self.paged || store.segment_count() >= self.compact_after || dead_enough {
                let _ = maint_tx.send(Maint::Storage);
            }
        }
    }
}

/// THE FLUSHER: one thread that makes the workers' writes durable, so no
/// worker waits on the disk.
///
/// Before it, each worker ended its batch in `Store::sync_pending`, which
/// holds the group mutex for the whole fsync — so a worker whose records were
/// NOT covered waited for the running fsync and then paid its own, and all
/// that time its other sessions' next requests sat in its inbox. On the bench
/// volume (2026-09-27: ~6.7 ms an fsync) that pinned the server at ~150 fsyncs
/// a second covering 2-6 commits each, ~1,000 writes/s from 8 to 64 clients,
/// while PostgreSQL did 12,000 on the same disk.
///
/// Now a worker hands its held replies here and takes its next batch at once.
/// The flusher drains every queued hand-off, pays ONE `sync_pending` — which
/// flushes and syncs everything appended before it runs, so it covers every
/// job it drained, from every worker — and releases the replies in order.
/// Then it runs the seal check, AFTER the fsync as before, and after the
/// replies, so no reply waits on a seal.
///
/// A failed fsync ABORTS THE PROCESS, as it always has: the writes are
/// already visible to readers on every worker and cannot be made durable.
fn spawn_flusher(store: Store, policy: SealPolicy, maint_tx: Sender<Maint>) -> Sender<FlushJob> {
    let (tx, rx): (Sender<FlushJob>, Receiver<FlushJob>) = channel();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut jobs = vec![first];
            while let Ok(j) = rx.try_recv() {
                jobs.push(j);
            }
            if let Err(e) = store.sync_pending() {
                eprintln!(
                    "[engram-server] FATAL: WAL fsync failed (durability): {e} — \
                     acknowledged data may not be on disk; aborting so a restart \
                     recovers the durable prefix"
                );
                std::process::abort();
            }
            for job in jobs {
                for (reply, bytes) in job.out {
                    // A failed send is a connection that closed meanwhile; its
                    // reader has posted (or will post) `Closed` to the worker.
                    let _ = reply.send(bytes);
                }
                job.outstanding.fetch_sub(1, Ordering::AcqRel);
            }
            policy.seal_if_due(&store, &maint_tx);
        }
    });
    tx
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
        eprintln!(
            "[engram-server] {} persisted {written} range-index sidecar(s)",
            unix_ms_tag()
        );
    }
}

/// Fix 86: unix milliseconds, as a log-line prefix — `t=1757300000123`.
///
/// Every maintenance line the server prints (refresh, spill, checkpoint,
/// compaction, sidecar persist, memory, counters) carries it, and the
/// stress harness stamps its levels and its slow statements with the same
/// clock, so a stalled SECOND of a sweep can be aligned with the server
/// event that ran through it. Before this the server log had no clock at
/// all: the v187 sweep's `write-heavy @ 8` stopped for ~2 s every ~6 s and
/// nothing in the log could be placed against those seconds — four fixes
/// were attributed by ordering and counts alone.
pub fn unix_ms_tag() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("t={}", d.as_millis()),
        Err(_) => "t=?".to_string(),
    }
}

/// Fix 86: the same clock as a number — `0` if the clock is before the
/// epoch. The binary hands this to the Bolt crate's growth report
/// (`engram_bolt::set_wall_clock_probe`), which cannot read a clock itself
/// under the determinism gate; the library is the crate that is allowed to.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Monotonic seconds since the first call — the clock the graph's sidecar
/// growth-rewrite interval is measured on. The server owns the clock; the
/// engine takes it as an argument so the simulation can own it there.
fn mono_secs() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
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
        // Fix 82's per-family clock is the pass's own wall clock, handed in:
        // the graph reads no clock of its own (the determinism gate).
        total.add(&g.refresh_stale_derived_timed(&|| t.elapsed().as_millis() as u64));
    }
    counters::MAINTENANCE_REFRESH_RUNS.fetch_add(1, Ordering::Release);
    // Logged only when something was BROUGHT CURRENT (`any` excludes what
    // was declined or deferred — those left the structure as stale as it
    // was), and the line names only what changed.
    if total.any() {
        eprintln!(
            "[engram-server] {} derived refresh in {} ms: {}",
            unix_ms_tag(),
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
    match store.spill_sealed_into_reporting(dir, cache) {
        Ok((0, _)) => {}
        Ok((n, durable_below)) => {
            eprintln!(
                "[engram-server] {} spilled {n} segment(s) to paged in {} ms ({} sealed total)",
                unix_ms_tag(),
                t.elapsed().as_millis(),
                store.segment_count()
            );
            // The segments just written hold every logged record below the
            // boundary, fsync'd: the WAL may drop that prefix. A boundary
            // below the log's retained range (a newer seal already truncated
            // past it, and its segment is still resident) is left for the
            // spill that writes that segment.
            if let (Some(below), true) = (durable_below, store.has_wal()) {
                match store.checkpoint_wal(below) {
                    Ok(dropped) => eprintln!(
                        "[engram-server] {} WAL checkpointed below seq {below} ({dropped} record(s) \
                         dropped behind the spilled segments)",
                        unix_ms_tag()
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
                    Err(e) => eprintln!(
                        "[engram-server] WAL checkpoint FAILED: {e} — the WAL keeps its prefix; \
                         durability is intact, the file grows until a checkpoint succeeds"
                    ),
                }
            }
        }
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

#[cfg(test)]
mod bounded_parallel_pool {
    //! The morsel budget is process-wide, and these pin the three properties
    //! that make it safe to turn on.
    //!
    //! The measurement that motivates it: at width 6 on a 6-CPU quota, the
    //! `balanced` profile peaked at EIGHT clients (1,760 ops/s) and then
    //! DECLINED — 1,735 at 16, 1,675 at 32, with p99 going 42 -> 164 ms — while
    //! the write profiles, which never multiply, climbed to 32 unabated. The
    //! same sweep logged 20.0 s of CPU throttle against 0.011 s for one client
    //! at the same width. Unbounded per-statement width is not merely
    //! wasteful past that point; it is negative.
    //!
    //! What must hold for the fix to be safe rather than merely faster:
    //!   a. the budget actually CAPS concurrent workers across statements;
    //!   b. a statement granted NOTHING still visits every index — degrading is
    //!      a scheduling decision, never a correctness one;
    //!   c. slots come back, including when `f` unwinds. A leaked slot is
    //!      permanent and would bleed the pool down to serial for the life of
    //!      the process, which would look exactly like "parallelism stopped
    //!      helping" and be almost impossible to attribute.

    use super::{PARALLEL_SLOTS, ThreadScopeExec, set_parallel_slots};
    use engram_graph::ScopedExec;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// These assert on PROCESS-GLOBAL state, and cargo runs a binary's tests
    /// concurrently — so without this they race each other, not the code. The
    /// first run failed exactly that way: `c` observed a budget of 10 against
    /// its own 6 because a sibling had re-set it mid-flight, and `a` saw 8
    /// workers against a budget of 4 for the same reason. The failures were
    /// real and the defect was in the tests.
    static POOL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the lock, tolerating a sibling's panic having poisoned it — the
    /// data is a plain counter each test sets for itself, so poisoning carries
    /// no information here.
    fn serialise() -> std::sync::MutexGuard<'static, ()> {
        POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` while tracking the high-water mark of threads inside it.
    fn peak_concurrency(budget: usize, statements: usize, width: usize, n: usize) -> usize {
        set_parallel_slots(budget);
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..statements {
                s.spawn(|| {
                    let ex = ThreadScopeExec { width };
                    ex.for_each(n, &|_i| {
                        let now = live.fetch_add(1, Ordering::AcqRel) + 1;
                        peak.fetch_max(now, Ordering::AcqRel);
                        // Hold the slot briefly so overlap is real rather than
                        // a scheduling accident.
                        std::thread::sleep(std::time::Duration::from_micros(200));
                        live.fetch_sub(1, Ordering::AcqRel);
                    });
                });
            }
        });
        peak.load(Ordering::Acquire)
    }

    #[test]
    fn a_the_budget_caps_the_amplification_not_the_statement_count() {
        let _serial = serialise();
        // WHAT THIS DOES NOT ASSERT, and why. A statement granted nothing runs
        // SERIALLY ON ITS OWN THREAD, and that thread still executes `f`. So
        // with S concurrent statements the floor is S bodies whatever the
        // budget is: the pool bounds the AMPLIFICATION (S clients must not
        // become S x width workers), not the absolute count. The first version
        // of this test asserted `peak <= budget`, failed at 8 against a budget
        // of 4, and the assertion was what was wrong.
        //
        // So the mechanism is tested as a DIFFERENTIAL: the same workload under
        // a tight budget must show materially less concurrency than under a
        // loose one. That is the property the bench pod needs — 8 clients at
        // width 6 producing up to 48 workers on 6 CPUs is what made `balanced`
        // peak at 8 clients and decline thereafter.
        const STATEMENTS: usize = 8;
        const WIDTH: usize = 6;
        let bounded = peak_concurrency(WIDTH, STATEMENTS, WIDTH, 96);
        let unbounded = peak_concurrency(STATEMENTS * WIDTH, STATEMENTS, WIDTH, 96);
        assert!(
            bounded <= STATEMENTS + WIDTH,
            "bounded run reached {bounded} concurrent bodies, above the \
             statements-plus-budget ceiling of {}",
            STATEMENTS + WIDTH
        );
        assert!(
            bounded < unbounded,
            "the budget changed nothing: bounded {bounded} vs unbounded {unbounded}"
        );
    }

    #[test]
    fn b_a_statement_granted_nothing_still_visits_every_index() {
        let _serial = serialise();
        // Drain the pool, then run. Granted == 0 must mean SERIAL, not skipped:
        // `ScopedExec`'s contract is that every index is visited, and the
        // number of threads is an implementation detail.
        set_parallel_slots(0);
        let seen = AtomicUsize::new(0);
        let ex = ThreadScopeExec { width: 8 };
        ex.for_each(1_000, &|_i| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(
            seen.load(Ordering::Relaxed),
            1_000,
            "a starved statement dropped work — degrading to serial must never \
             change the ANSWER, only the schedule"
        );
    }

    #[test]
    fn c_slots_are_returned_so_the_pool_does_not_bleed_down() {
        let _serial = serialise();
        set_parallel_slots(6);
        for _ in 0..20 {
            let ex = ThreadScopeExec { width: 6 };
            ex.for_each(50, &|_i| {});
        }
        assert_eq!(
            PARALLEL_SLOTS.load(Ordering::Acquire),
            6,
            "the pool did not return to its budget after 20 statements — a \
             leaked slot is permanent and degrades the process to serial for \
             good"
        );
    }

    #[test]
    fn e_a_run_starts_no_more_helpers_than_its_work_needs() {
        let _serial = serialise();
        // The caller works too, and a helper starts only while the unclaimed
        // morsels outnumber the threads on them: two morsels are ONE helper
        // at most, whatever the grant — the executor used to spawn a thread
        // per granted slot for every call, the caller idle.
        set_parallel_slots(8);
        for n in [2usize, 3, 5] {
            let caller = std::thread::current().id();
            let threads = std::sync::Mutex::new(std::collections::BTreeSet::new());
            let seen = AtomicUsize::new(0);
            let ex = ThreadScopeExec { width: 8 };
            ex.for_each(n, &|_i| {
                seen.fetch_add(1, Ordering::Relaxed);
                let me = std::thread::current().id();
                if me != caller {
                    threads.lock().unwrap_or_else(|e| e.into_inner()).insert(format!("{me:?}"));
                }
            });
            assert_eq!(seen.load(Ordering::Relaxed), n, "a morsel was lost or repeated");
            let helpers = threads.lock().unwrap_or_else(|e| e.into_inner()).len();
            assert!(helpers < n, "{n} morsels ran on {helpers} helpers besides the caller");
        }
        assert_eq!(PARALLEL_SLOTS.load(Ordering::Acquire), 8, "the grant was not returned");
    }

    #[test]
    fn g_a_long_first_morsel_does_not_keep_the_helpers_out() {
        let _serial = serialise();
        // The calling thread works and starts no helper itself; one helper
        // waits out the ramp and then joins. So a first morsel that runs for
        // as long as its caller likes must not hold the others back: morsel 0
        // here waits for the LAST morsel to have run — which only a helper
        // can do while the caller is inside morsel 0. A timed wait, so a
        // failure is an assertion and not a hang.
        set_parallel_slots(4);
        let last_ran = std::sync::Mutex::new(false);
        let ran = std::sync::Condvar::new();
        let waited_out = AtomicUsize::new(0);
        let ex = ThreadScopeExec { width: 4 };
        ex.for_each(8, &|i| {
            if i == 0 {
                let guard = last_ran.lock().unwrap_or_else(|e| e.into_inner());
                let (guard, timeout) = ran
                    .wait_timeout_while(guard, std::time::Duration::from_secs(10), |r| !*r)
                    .unwrap_or_else(|e| e.into_inner());
                if timeout.timed_out() || !*guard {
                    waited_out.fetch_add(1, Ordering::Relaxed);
                }
            } else if i == 7 {
                *last_ran.lock().unwrap_or_else(|e| e.into_inner()) = true;
                ran.notify_all();
            }
        });
        assert_eq!(
            waited_out.load(Ordering::Relaxed),
            0,
            "no helper ran the other morsels while the caller was inside morsel 0"
        );
        assert_eq!(PARALLEL_SLOTS.load(Ordering::Acquire), 4, "the grant was not returned");
    }

    #[test]
    fn h_as_many_heavy_morsels_as_threads_all_run_at_once() {
        let _serial = serialise();
        // Every morsel waits until all eight have started: only eight threads
        // at once -- the caller and seven helpers -- get through. rev52's rule
        // stopped starting helpers at about half the morsels (each new thread
        // claims at once, so the claimed count kept pace with the live one),
        // and a run of one heavy share per worker ran on half the threads.
        // A timed wait, so a failure is an assertion and not a hang.
        set_parallel_slots(8);
        let started = std::sync::Mutex::new(0usize);
        let all_in = std::sync::Condvar::new();
        let short = AtomicUsize::new(0);
        let ex = ThreadScopeExec { width: 8 };
        ex.for_each(8, &|_| {
            let mut n = started.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            all_in.notify_all();
            let (n, t) = all_in
                .wait_timeout_while(n, std::time::Duration::from_secs(10), |n| *n < 8)
                .unwrap_or_else(|e| e.into_inner());
            if t.timed_out() || *n < 8 {
                short.fetch_add(1, Ordering::Relaxed);
            }
        });
        assert_eq!(
            short.load(Ordering::Relaxed),
            0,
            "eight heavy morsels did not all run at once on a grant of eight"
        );
        assert_eq!(PARALLEL_SLOTS.load(Ordering::Acquire), 8, "the grant was not returned");
    }

    #[test]
    fn f_every_morsel_runs_once_at_every_size() {
        let _serial = serialise();
        set_parallel_slots(16);
        for n in [0usize, 1, 2, 7, 16, 17, 1_000, 10_007] {
            let hits: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            let ex = ThreadScopeExec { width: 16 };
            ex.for_each(n, &|i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
            });
            assert!(
                hits.iter().all(|h| h.load(Ordering::Relaxed) == 1),
                "a morsel of {n} ran other than once"
            );
        }
    }

    #[test]
    fn d_a_panicking_body_still_returns_its_slots() {
        let _serial = serialise();
        // The guard exists for exactly this. Without it an unwinding statement
        // would keep its slots for ever.
        set_parallel_slots(4);
        let r = std::panic::catch_unwind(|| {
            let ex = ThreadScopeExec { width: 4 };
            ex.for_each(32, &|i| {
                if i == 7 {
                    panic!("deliberate");
                }
            });
        });
        assert!(r.is_err(), "the panic did not propagate; this test proves nothing");
        assert_eq!(
            PARALLEL_SLOTS.load(Ordering::Acquire),
            4,
            "slots leaked through an unwind — the SlotGuard is not doing its job"
        );
    }
}
