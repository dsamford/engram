//! M7 — the property-graph facade over the store.
//!
//! # The label decision, made where the plan left it
//!
//! The plan's layout hypothesis: segments homogeneous by property-set
//! signature, **labels as pure bitmap membership orthogonal to the column
//! layout** (457 labels used combinatorially as a namespacing device kill
//! both per-label tables and per-combination tables). This facade encodes
//! that decision in row form: a node's labels are a token set ON the record
//! (for reading a bound node) plus one MEMBERSHIP ROW per label (for
//! scanning by label) — the bitmap, one row per bit, upgradeable to a real
//! bitmap segment without changing a caller.
//!
//! # Names are tokens
//!
//! Labels, relationship types and property KEYS are interned in a catalog
//! (KV rows in the store's own domain), so records carry u32 tokens and the
//! wire carries strings. The catalog is append-only: a token, once minted,
//! never renames — a rename would rewrite history's meaning.

#![forbid(unsafe_code)]

pub(crate) mod batch;
pub mod cardinality;
pub(crate) mod constraint_key;
pub(crate) mod derived;
pub(crate) mod derived_sidecar;
pub mod hnsw;
pub mod interp;
pub(crate) mod merge_derived;
pub(crate) mod precision;
pub mod pipeline;
pub mod schema;
pub mod scoped_exec;
pub mod shadow;
pub mod vectorized;

pub use interp::{QueryResult, RunError, run_query, run_stmt};
pub use merge_derived::compact_paged_emitting;

/// Process-wide operational counters — the `engram_store::FSYNCS` /
/// `engram_bolt::counters` pattern. The `counted!` traces are thread-local test
/// instruments; a server's worker and maintenance threads install none, so
/// what the derived-structure maintenance does in production is observable
/// only here. Monotonic, `Relaxed`, never read by engine logic — a test or an
/// operator reads a delta across a window.
pub mod counters {
    use std::sync::atomic::AtomicU64;

    /// Overlay rows CARRIED by adjacency repairs — see `repaired_adj_table`.
    ///
    /// Each repair inherits the whole overlay of the table it repairs, which
    /// holds one complete row per node repaired since the base was built.
    /// Against `ADJ_TABLES_REPAIRED` this says whether the carrying is a
    /// constant per repair or grows with the number of them. It grows, and
    /// `ADJ_OVERLAY_FOLD` is what bounds it.
    pub static ADJ_OVERLAY_ROWS_CLONED: AtomicU64 = AtomicU64::new(0);

    /// Adjacency repairs whose PUBLISH LOST — the work was done and thrown
    /// away because another worker had already advanced the slot.
    ///
    /// The repair is attempted BEFORE the per-table build guard, so N readers
    /// finding the same stale table all repair it concurrently and N-1 of them
    /// discard the result. Against `ADJ_TABLES_REPAIRED` this is the redundancy
    /// rate, and it is the number that says whether a mixed profile is paying
    /// for one repair per write or for one repair per reader per write.
    pub static ADJ_REPAIR_PUBLISH_LOST: AtomicU64 = AtomicU64::new(0);

    /// Single-node reads served from a STALE adjacency table because the
    /// change set did not touch the node asked for — the repairs that did not
    /// happen on a query thread. See `Graph::set_lazy_stale_serve`.
    pub static ADJ_STALE_SERVED_UNMOVED: AtomicU64 = AtomicU64::new(0);

    /// Label-membership snapshots CAUGHT UP by a reader — O(delta), but the
    /// delta is whatever the write stream has produced since the snapshot.
    ///
    /// The membership counters exist because four hypotheses have now been
    /// refuted by measurement (the Bolt protocol, the refresh cadence, the
    /// range index, and the table-admission gate), and membership is the
    /// remaining derived family a read can be made to pay for. Counted rather
    /// than reasoned about, for the same reason the index ones were: against a
    /// profile's read count these give a per-read frequency, and a frequency of
    /// ~0 exonerates a path in one run.
    pub static MEMBERS_CAUGHT_UP: AtomicU64 = AtomicU64::new(0);
    /// Label-membership snapshots REBUILT by a walk of the label — O(label),
    /// ~2M ids for `Comment` at SF1.
    pub static MEMBERS_BUILT: AtomicU64 = AtomicU64::new(0);
    /// Membership views MATERIALISED into a flat sorted vector
    /// (`MembersView::to_arc_vec`) — O(label) each.
    ///
    /// Free when the view has no overlay (an `Arc` clone of the shared base),
    /// O(n) merge when it has one, cached per SNAPSHOT. That amortisation is
    /// right while snapshots are rare and wrong under a write stream: every
    /// catch-up publishes a new snapshot with a fresh cache, so "once per
    /// snapshot" becomes once per catch-up. The `nolabels` control priced the
    /// named-label channel at -30% with `mem_caught` 0 vs ~1,700, and this is
    /// the counter that says whether the cost is the catch-up itself or the
    /// re-materialisation it invalidates.
    pub static MEMBERS_MATERIALISED: AtomicU64 = AtomicU64::new(0);

    /// Membership overlay FOLDS — O(base), so one is worth many catch-ups.
    pub static MEMBERS_FOLDS: AtomicU64 = AtomicU64::new(0);

    /// Membership PROBES from a hop's label filter — one per candidate peer
    /// examined by `(f)<-[:R]-(m:Label)`.
    ///
    /// Accumulated per hop and added once, not per edge: a relaxed atomic on
    /// every edge would contend across eight query threads and cost more than
    /// the probe it counts.
    ///
    /// This sizes the filter. `MembersView::contains` is three binary
    /// searches, and the base one walks the whole label — ~21 probes over 3.1M
    /// `Message` ids, nearly all cache misses. A large count here says the
    /// per-peer test is worth a denser representation; a count near zero
    /// refutes that before one is built.
    pub static MEMBERS_PROBES: AtomicU64 = AtomicU64::new(0);

    /// Presence BITMAPS built over a membership base — one per base that
    /// crossed the probe threshold and was dense enough to accept one.
    ///
    /// Bases are rebuilt rarely (six times in a 30 s `balanced` run), so this
    /// should stay small however many probes it serves. A count that tracks
    /// `mem_built` says the bitmaps are being rebuilt as fast as they are used
    /// and the threshold is wrong.
    pub static MEMBERS_BITMAPS: AtomicU64 = AtomicU64::new(0);

    /// IDS copied by those materialisations — the SIZE of the work, where
    /// `MEMBERS_MATERIALISED` is only its frequency.
    ///
    /// The two differ by four orders of magnitude across labels: `Person` at
    /// SF1 is ~10k ids and merges in microseconds, `Message` is ~3.1M and
    /// copies 25 MB. A residual count of a few hundred materialisations means
    /// nothing until this says which label they were on.
    pub static MEMBERS_FLAT_ROWS: AtomicU64 = AtomicU64::new(0);

    /// IDS a pipeline seed SCAN produced — the whole-label fallback taken when
    /// no property anchor is seekable.
    ///
    /// Its own counter because it is invisible to `MEMBERS_FLAT_ROWS`: with no
    /// overlay `to_arc_vec` is an `Arc` clone and records nothing, yet the scan
    /// still copies every id. On `:Message` at SF1 that is 3.06M ids — 24.5 MB —
    /// to seed a query whose next hop keeps 300 of them. Against a profile's
    /// read count this is ids scanned per read, and a number in the millions
    /// says a shape is scanning a label to answer a point question.
    pub static SEED_SCAN_ROWS: AtomicU64 = AtomicU64::new(0);

    /// Single-node reads that DECLINED a stale adjacency table and walked
    /// their own span instead of repairing the whole change set on the query
    /// thread. See `Graph::set_single_node_stale_walk`.
    pub static ADJ_STALE_DECLINED_TO_WALK: AtomicU64 = AtomicU64::new(0);

    /// Adjacency tables built from a full span walk (one per
    /// `graph.adjacency tables built` event) — the cost a stale table pays
    /// when it cannot be repaired.
    pub static ADJ_TABLES_BUILT: AtomicU64 = AtomicU64::new(0);
    /// Adjacency tables carried forward by re-reading only their changed rows
    /// (one per `graph.adjacency tables repaired` event).
    pub static ADJ_TABLES_REPAIRED: AtomicU64 = AtomicU64::new(0);
    /// Derived structures (adjacency tables, membership snapshots) brought
    /// current by [`super::Graph::refresh_stale_derived`] — the
    /// reader-independent publish — rather than by the reader that next
    /// needed them.
    pub static DERIVED_REFRESHED_BY_MAINTENANCE: AtomicU64 = AtomicU64::new(0);
    /// Change logs POISONED by an entry stamped at or below a snapshot a
    /// publisher had already pruned behind — the invariant the write fence
    /// (`derived.rs`) guarantees. Non-zero means the fence has a hole and the
    /// engine failed closed (dropped the log, retracted the snapshots, made
    /// the next reader rebuild) rather than serve a repair missing a row.
    pub static DERIVED_LOG_POISONED: AtomicU64 = AtomicU64::new(0);
}

/// What [`Graph::refresh_stale_derived`] did. Reported rather than logged
/// inside the engine, so the caller decides whether and how to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshReport {
    /// Adjacency tables that were behind their types' epoch and were REPAIRED
    /// (changed rows re-read) to current — counted only when the repair's
    /// fenced publish WON, i.e. the slot actually advanced.
    pub adjacency_repaired: usize,
    /// Adjacency tables that were behind and had to be REBUILT (a full span
    /// walk): their logs no longer reached them, or repair cost more.
    pub adjacency_rebuilt: usize,
    /// Adjacency tables that were behind and could be neither — declined by
    /// the entry budget; the reader will fall back to the direct walk.
    pub adjacency_declined: usize,
    /// Adjacency tables that were behind, could not be repaired, and were NOT
    /// rebuilt by this pass: untyped tables (never rebuilt by the refresh —
    /// the whole span, 25 s on SF1 paged, for a table only the warm asked
    /// for) or a typed table past the pass's one-rebuild budget. The next
    /// pass or the next reader rebuilds it. Also a table that WAS repaired
    /// but whose fenced publish lost to the stamp the slot already held — a
    /// writer registered at that stamp is still in flight, so the slot did
    /// not advance and the next pass (after the writer) takes it.
    pub adjacency_deferred: usize,
    /// Membership snapshots caught up from their label's change log — only
    /// when the fenced publish won.
    pub members_caught_up: usize,
    /// Membership snapshots rebuilt from a label walk (log did not cover).
    pub members_rebuilt: usize,
    /// Membership snapshots behind that this pass did not advance: uncovered
    /// and the one-rebuild budget was spent, or caught up but the fenced
    /// publish lost to the slot's current stamp (a writer in flight there).
    pub members_deferred: usize,
}

impl RefreshReport {
    /// Whether anything was BROUGHT CURRENT — repaired, rebuilt or caught up.
    /// A declined or deferred structure is exactly as stale as before and does
    /// not count; it used to, and the server logged "refreshed" for passes
    /// that changed nothing.
    pub fn any(&self) -> bool {
        self.adjacency_repaired + self.adjacency_rebuilt + self.members_caught_up + self.members_rebuilt
            > 0
    }

    /// Fold another pass's report into this one.
    pub fn add(&mut self, r: &RefreshReport) {
        self.adjacency_repaired += r.adjacency_repaired;
        self.adjacency_rebuilt += r.adjacency_rebuilt;
        self.adjacency_declined += r.adjacency_declined;
        self.adjacency_deferred += r.adjacency_deferred;
        self.members_caught_up += r.members_caught_up;
        self.members_rebuilt += r.members_rebuilt;
        self.members_deferred += r.members_deferred;
    }

    /// The non-zero fields, named — `adjacency repaired=3 members caught up=2`
    /// — so a log line says what changed rather than printing seven zeros.
    pub fn describe(&self) -> String {
        let parts: Vec<String> = [
            ("adjacency repaired", self.adjacency_repaired),
            ("adjacency rebuilt", self.adjacency_rebuilt),
            ("adjacency declined", self.adjacency_declined),
            ("adjacency deferred", self.adjacency_deferred),
            ("members caught up", self.members_caught_up),
            ("members rebuilt", self.members_rebuilt),
            ("members deferred", self.members_deferred),
        ]
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{k}={n}"))
        .collect();
        if parts.is_empty() {
            "nothing stale".to_string()
        } else {
            parts.join(" ")
        }
    }
}
pub use schema::{ConstraintDef, IndexDef, VectorArm, VectorPlan};
pub use derived::MembersView;
use derived::{ChangeLog, Slot, Snapshot, slot_in};
pub use scoped_exec::{ScopedExec, SerialExec};
pub use shadow::{ShadowVerdict, shadow_compare};

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_observe::{Canary, Gate, Registration, Subsystem, counted, crash_point, sometimes};
use engram_store::{PropertyId, Record, Store, StoredValue, record::get_property};

/// A cached ANN build: the index AND the gather snapshot it was built from,
/// so a warm query touches no node records at all. The bench harness found
/// the gap: re-gathering every vector per query made the ANN arm cost the
/// same as the exact scan it exists to beat (0.98x at 50k).
pub(crate) struct AnnEntry {
    pub(crate) epoch: u64,
    pub(crate) dim: usize,
    pub(crate) index: std::sync::Arc<crate::hnsw::Hnsw>,
    pub(crate) vectors: std::sync::Arc<BTreeMap<u64, Vec<f64>>>,
    pub(crate) skipped: usize,
}

/// Reserved property ids — allocation of user property tokens starts above.
const P_LABELS: PropertyId = PropertyId(0);
const P_SRC: PropertyId = PropertyId(1);
const P_DST: PropertyId = PropertyId(2);
const P_TYPE: PropertyId = PropertyId(3);
const FIRST_USER_TOKEN: u32 = 8;

/// Why a graph operation refused.
#[derive(Debug)]
pub enum GraphError {
    /// A stored row did not decode.
    Corrupt(String),
    /// A value type properties cannot hold (maps, nodes, nested nulls in
    /// lists…). Neo4j's rule, kept: properties are scalars and homogeneous
    /// scalar lists.
    BadPropertyValue(String),
    /// The store refused.
    Store(engram_store::StoreError),
    /// The entity does not exist.
    Missing(&'static str, u64),
    /// Deleting a node that still has relationships, without DETACH.
    StillConnected(u64),
    /// A schema conflict: a duplicate name, a missing index, a wrong kind.
    SchemaConflict(String),
    /// A constraint refused a write — or refused to be created over a
    /// violating population.
    ConstraintViolation(String),
    /// A transaction was used out of order (a nested `begin`, or a
    /// `commit`/`rollback` with none active).
    Txn(String),
    /// The transaction lost an optimistic race: a key it read or wrote was
    /// committed by another transaction after its snapshot. NOTHING was
    /// published; the statement is safe to retry from a fresh transaction.
    TxnConflict,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::Corrupt(d) => write!(f, "corrupt graph row: {d}"),
            GraphError::BadPropertyValue(d) => write!(f, "not a legal property value: {d}"),
            GraphError::Store(e) => write!(f, "{e}"),
            GraphError::Missing(what, id) => write!(f, "{what} {id} does not exist"),
            GraphError::StillConnected(id) => {
                write!(f, "node {id} still has relationships — use DETACH DELETE")
            }
            GraphError::SchemaConflict(d) => write!(f, "{d}"),
            GraphError::ConstraintViolation(d) => write!(f, "constraint violation: {d}"),
            GraphError::Txn(d) => write!(f, "transaction error: {d}"),
            GraphError::TxnConflict => write!(
                f,
                "transaction conflict: a read/written key changed since the snapshot — retry"
            ),
        }
    }
}

impl std::error::Error for GraphError {}

/// Degree tables: (direction tag, sorted type tokens) → (epoch, counts by
/// node id). Counts are `u32` — a node with 4 billion adjacent rows is a
/// different engine's problem; the table declines above `DEGREE_TABLE_MAX_ID`.
type DegreeTables = BTreeMap<(u8, Vec<u32>), (u64, std::sync::Arc<DegreeTable>)>;

/// One direction's per-node adjacency-row counts. `loops` is the number
/// of self-loop rows seen from the O side, so `Both` can dedup by
/// arithmetic exactly as `count_adjacent` does.
pub struct DegreeTable {
    counts: Vec<u32>,
    loops: Vec<u32>,
}

/// Direct probes tolerated per epoch before a table is built (default).
const DEGREE_TABLE_AFTER: u64 = 1024;
/// Above this node id the table is declined (a Vec<u32> per id).
const DEGREE_TABLE_MAX_ID: u64 = 1 << 28;

/// Which record family a column read walks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnFamily {
    Nodes,
    Rels,
}

/// The out-adjacency tag byte: every relationship appears exactly once
/// under this tag, keyed by its source.
const TAG_OUT: u8 = b'O';

/// A relationship population: id-sorted ids with the type token and the
/// (source, destination) ends aligned.
pub(crate) type RelPopulation = (
    std::sync::Arc<Vec<u64>>,
    Vec<u32>,
    std::sync::Arc<Vec<(u64, u64)>>,
);

/// Relationship populations per type set, keyed by the commit epoch.
type RelMembersCache = BTreeMap<Vec<u32>, (u64, RelPopulation)>;

/// Maintained statistics — the count store. Live node and relationship
/// totals and per-label / per-type counts, kept CURRENT by every write
/// path rather than recomputed by a walk: `MATCH ()-[r:SUPPLIES]->()
/// RETURN count(r)` measured 10.8 s on the production port walking the
/// whole O prefix for a number the loader had in hand the entire time.
///
/// Lifecycle: a graph over an EMPTY store starts with empty stats and
/// maintains them from the first write (the bulk load builds them for
/// free). A graph over a store that already holds data — recovered from a
/// log, or opened — starts with `None` and rebuilds from one walk on the
/// first read, then maintains. Either way the numbers are exact; the
/// equivalence against the membership walk is pinned per seed in the sim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live nodes.
    pub nodes: u64,
    /// Live relationships.
    pub rels: u64,
    /// Live nodes per label token.
    pub by_label: BTreeMap<u32, u64>,
    /// Live relationships per type token.
    pub by_type: BTreeMap<u32, u64>,
}

fn bump(m: &mut BTreeMap<u32, u64>, k: u32, delta: i64) {
    let e = m.entry(k).or_insert(0);
    *e = (*e as i64 + delta).max(0) as u64;
}

/// One adjacency row as the KEY carries it — no record fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlimAdj {
    /// The relationship id.
    pub rel: u64,
    /// Its type token.
    pub type_token: u32,
    /// The node at the other end.
    pub peer: u64,
}

/// The CSR row directory of an [`AdjTable`]: node id → its row's `[start,
/// end)` in `entries`, in O(1) — SPARSE over the id space.
///
/// The dense form — one `u32` per node id up to the highest node the table
/// names — costs O(ids) PER TABLE, and there is a table per (type, direction).
/// On the ported corpus (159 relationship types, ~3.4M ids named) that was
/// 4.36 GB of offsets carrying 540 MB of entries: "318 adjacency table(s)
/// holding 4894 MB in 7640 MB allocated". LDBC, with ~15 types, never showed
/// it.
///
/// Here a node is one BIT; a rank directory (set bits before each 64-bit
/// word) turns a set bit into a position in `starts`, which holds one `u32`
/// per node that HAS a row, plus the terminator. Cost per table: ids/8 bytes
/// of bits, ids/16 of rank, 4 bytes per non-empty node — an order of magnitude
/// less on that graph — and `row()` is two dependent loads and a popcount.
/// Nothing a caller can observe changes: `slice()` hands out the same
/// `&[SlimAdj]`, and `to_dense()` reproduces the dense offsets byte for byte
/// (the sidecar and the layout differential still speak dense).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowIndex {
    /// The id space the directory covers: `[0, nodes)`. A node at or past
    /// this has no row — the dense form's "past the terminator" case.
    nodes: usize,
    /// Bit `n` set ⇔ node `n` has a non-empty row.
    bits: Vec<u64>,
    /// `rank[w]` = set bits in `bits[..w]` — the `starts` position of the
    /// first row in word `w`.
    rank: Vec<u32>,
    /// Row starts of the nodes with a row, in id order, then the terminator
    /// (`entries.len()`), so row `k` is `starts[k]..starts[k + 1]`.
    starts: Vec<u32>,
}

impl RowIndex {
    /// From the dense CSR offsets (`offsets[n]..offsets[n+1]` is node `n`'s
    /// row; the last element is the terminator). An empty slice is the empty
    /// table.
    #[cfg(test)]
    pub(crate) fn from_dense(dense: &[u32]) -> RowIndex {
        let nodes = dense.len().saturating_sub(1);
        let words = nodes.div_ceil(64);
        let mut bits = vec![0u64; words];
        let mut rank = Vec::with_capacity(words);
        let mut starts = Vec::new();
        let mut seen = 0u32;
        for (w, word_out) in bits.iter_mut().enumerate() {
            rank.push(seen);
            let mut word = 0u64;
            let base = w * 64;
            for b in 0..64 {
                let n = base + b;
                if n >= nodes {
                    break;
                }
                if dense[n + 1] > dense[n] {
                    word |= 1u64 << b;
                    starts.push(dense[n]);
                    seen += 1;
                }
            }
            *word_out = word;
        }
        starts.push(dense.last().copied().unwrap_or(0));
        RowIndex {
            nodes,
            bits,
            rank,
            starts,
        }
    }

    /// The dense offsets this directory stands for — byte-identical to what
    /// `from_dense` was given: an empty row sits where the NEXT row starts,
    /// which is exactly what the dense build writes when it pads up to a node.
    pub(crate) fn to_dense(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.nodes + 1);
        let mut k = 0usize;
        for n in 0..self.nodes {
            out.push(self.starts[k]);
            if self.bits[n / 64] & (1u64 << (n % 64)) != 0 {
                k += 1;
            }
        }
        out.push(*self.starts.last().unwrap_or(&0));
        out
    }

    /// Node `n`'s row in `entries`, or `None` when it has none.
    #[inline]
    pub(crate) fn row(&self, n: u64) -> Option<std::ops::Range<usize>> {
        let n = n as usize;
        if n >= self.nodes {
            return None;
        }
        let word = self.bits[n / 64];
        let bit = 1u64 << (n % 64);
        if word & bit == 0 {
            return None;
        }
        let k = self.rank[n / 64] as usize + (word & (bit - 1)).count_ones() as usize;
        Some(self.starts[k] as usize..self.starts[k + 1] as usize)
    }

    /// The id space covered — the dense form's `offsets.len() - 1`.
    pub(crate) fn nodes(&self) -> usize {
        self.nodes
    }

    /// The nodes that HAVE a row, ascending — a walk of the set bits.
    pub(crate) fn nodes_with_rows(&self) -> impl Iterator<Item = u64> + '_ {
        self.bits.iter().enumerate().flat_map(|(w, word)| {
            let mut word = *word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let b = word.trailing_zeros();
                word &= word - 1;
                Some((w as u64) * 64 + u64::from(b))
            })
        })
    }
}

/// Builds a [`RowIndex`] DIRECTLY from rows arriving in ascending node order —
/// no dense `Vec<u32>` in between. The builders used to grow a dense offsets
/// vector to the highest node id and convert it at the end: on the production
/// graph that is 318 tables × ~5M ids × 4 B ≈ 6.5 GB of transient during a
/// cold warm (the pod peaked at 20 GiB with a 5.4 GiB steady state), which is
/// exactly the peak the 12Gi envelope cannot hold. This holds one bit per id
/// and one `u32` per non-empty row while it builds.
pub(crate) struct RowIndexBuilder {
    bits: Vec<u64>,
    starts: Vec<u32>,
    current: Option<u64>,
    last: Option<u64>,
}

impl RowIndexBuilder {
    pub(crate) fn new() -> RowIndexBuilder {
        RowIndexBuilder {
            bits: Vec::new(),
            starts: Vec::new(),
            current: None,
            last: None,
        }
    }

    /// Note that an entry for `node` is about to be pushed at `entries_len`.
    /// Nodes must arrive in non-decreasing order. Returns the row's start —
    /// the position a builder compares the previous entry's peer against.
    pub(crate) fn note(&mut self, node: u64, entries_len: usize) -> usize {
        if self.current == Some(node) {
            return *self.starts.last().expect("a current row has a start") as usize;
        }
        debug_assert!(self.last.is_none_or(|l| node > l), "rows must arrive in id order");
        let w = (node / 64) as usize;
        if self.bits.len() <= w {
            self.bits.resize(w + 1, 0);
        }
        self.bits[w] |= 1u64 << (node % 64);
        self.starts.push(entries_len as u32);
        self.current = Some(node);
        self.last = Some(node);
        entries_len
    }

    /// The directory, `entries_len` being the terminator. Covers ids up to and
    /// including the last node noted — the dense form's `offsets.len() - 1`.
    pub(crate) fn finish(mut self, entries_len: usize) -> RowIndex {
        self.starts.push(entries_len as u32);
        let nodes = self.last.map_or(0, |l| l as usize + 1);
        let words = nodes.div_ceil(64);
        self.bits.truncate(words);
        self.bits.resize(words, 0);
        let mut rank = Vec::with_capacity(words);
        let mut seen = 0u32;
        for w in &self.bits {
            rank.push(seen);
            seen += w.count_ones();
        }
        debug_assert_eq!(self.starts.len(), seen as usize + 1);
        RowIndex {
            nodes,
            bits: self.bits,
            rank,
            starts: self.starts,
        }
    }
}

impl RowIndex {
    /// The directory's persisted parts — `(nodes, bits, starts)`; `rank` is
    /// derived and is not persisted. This is the SPARSE form the derived
    /// sidecar writes (v2): ids/8 bytes of bits and 4 bytes per non-empty
    /// node, against 4 bytes per id for the dense form.
    pub(crate) fn parts(&self) -> (usize, &[u64], &[u32]) {
        (self.nodes, &self.bits, &self.starts)
    }

    /// Rebuild from persisted parts, REFUSING anything inconsistent: the word
    /// count must cover `nodes`, `starts` must hold one start per set bit plus
    /// the terminator and be non-decreasing, and no bit may sit at or past
    /// `nodes`. A sidecar is a cache read from disk; a directory that does not
    /// check out is a rebuild, never a panic and never a wrong slice.
    pub(crate) fn from_parts(nodes: usize, bits: Vec<u64>, starts: Vec<u32>) -> Option<RowIndex> {
        if bits.len() != nodes.div_ceil(64) {
            return None;
        }
        if let Some(last) = bits.last() {
            let valid = nodes - (bits.len() - 1) * 64; // bits meaningful in the last word
            if valid < 64 && (*last >> valid) != 0 {
                return None;
            }
        }
        let mut rank = Vec::with_capacity(bits.len());
        let mut seen = 0u32;
        for w in &bits {
            rank.push(seen);
            seen += w.count_ones();
        }
        if starts.len() != seen as usize + 1 {
            return None;
        }
        if starts.windows(2).any(|w| w[0] > w[1]) {
            return None;
        }
        Some(RowIndex {
            nodes,
            bits,
            rank,
            starts,
        })
    }

    /// Bytes held, for the warm report's attribution.
    pub(crate) fn bytes(&self) -> usize {
        self.bits.len() * 8 + self.rank.len() * 4 + self.starts.len() * 4
    }

    /// Bytes allocated (the `Vec` capacities), for the same report.
    pub(crate) fn capacity_bytes(&self) -> usize {
        self.bits.capacity() * 8 + self.rank.capacity() * 4 + self.starts.capacity() * 4
    }
}

#[cfg(test)]
mod row_index_tests {
    use super::{RowIndex, RowIndexBuilder};

    /// The oracle: the dense slice for node `n`, `None` when empty or past
    /// the terminator — what `AdjTable::slice` computed before the directory.
    fn dense_row(dense: &[u32], n: usize) -> Option<std::ops::Range<usize>> {
        if n + 1 < dense.len() && dense[n + 1] > dense[n] {
            Some(dense[n] as usize..dense[n + 1] as usize)
        } else {
            None
        }
    }

    fn check(dense: &[u32]) {
        let idx = RowIndex::from_dense(dense);
        assert_eq!(idx.nodes(), dense.len().saturating_sub(1));
        for n in 0..dense.len() + 70 {
            assert_eq!(idx.row(n as u64), dense_row(dense, n), "node {n} of {dense:?}");
        }
        let back = idx.to_dense();
        let want: Vec<u32> = if dense.is_empty() { vec![0] } else { dense.to_vec() };
        assert_eq!(back, want, "round trip of {dense:?}");
    }

    #[test]
    fn hand_built_shapes_round_trip_and_answer_the_dense_slice() {
        check(&[]);
        check(&[0]);
        check(&[0, 0]);
        check(&[0, 3]);
        check(&[0, 0, 2, 2, 5]);
        check(&[0, 2, 2, 2, 2]); // rows only at the front
        check(&[0, 0, 0, 0, 4]); // rows only at the back
        // Word boundaries: rows at 0, 63, 64, 65, 127, 128, 129 in a 130-node table.
        let mut dense = vec![0u32; 131];
        let mut cur = 0u32;
        for (n, slot) in dense.iter_mut().enumerate().take(130) {
            *slot = cur;
            if [0, 63, 64, 65, 127, 128, 129].contains(&n) {
                cur += 1 + (n as u32 % 3);
            }
        }
        dense[130] = cur;
        check(&dense);
    }

    #[test]
    fn parts_round_trip_and_inconsistent_parts_are_refused() {
        let dense = [0u32, 0, 2, 2, 5, 5, 5, 9];
        let idx = RowIndex::from_dense(&dense);
        let (nodes, bits, starts) = idx.parts();
        let back = RowIndex::from_parts(nodes, bits.to_vec(), starts.to_vec()).expect("parts");
        assert_eq!(back, idx);
        assert_eq!(back.to_dense(), dense);
        // Refusals: a short word vector, a start count off by one, a decreasing
        // start, a bit past `nodes`.
        assert!(RowIndex::from_parts(nodes, vec![], starts.to_vec()).is_none());
        assert!(RowIndex::from_parts(nodes, bits.to_vec(), starts[..starts.len() - 1].to_vec()).is_none());
        let mut bad = starts.to_vec();
        bad.swap(0, 1);
        assert!(RowIndex::from_parts(nodes, bits.to_vec(), bad).is_none() || starts[0] == starts[1]);
        assert!(RowIndex::from_parts(3, vec![1u64 << 5], vec![0, 1]).is_none());
        // The empty table.
        let e = RowIndex::from_dense(&[0]);
        let (n, b, s) = e.parts();
        assert_eq!(RowIndex::from_parts(n, b.to_vec(), s.to_vec()).expect("empty"), e);
    }

    /// The builder produces exactly what `from_dense` produces for the same
    /// rows — including the empty table, a leading empty run, and gaps.
    #[test]
    fn the_builder_equals_from_dense_for_the_same_rows() {
        for dense in [
            vec![0u32],
            vec![0, 0],
            vec![0, 2, 2, 3],
            vec![0, 0, 0, 4, 4, 4, 4],
            vec![0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 3],
        ] {
            let mut b = RowIndexBuilder::new();
            let mut total = 0usize;
            for n in 0..dense.len().saturating_sub(1) {
                let degree = (dense[n + 1] - dense[n]) as usize;
                for _ in 0..degree {
                    let start = b.note(n as u64, total);
                    assert_eq!(start, dense[n] as usize, "row start of {n} in {dense:?}");
                    total += 1;
                }
            }
            let built = b.finish(total);
            // A builder covers ids up to the LAST NON-EMPTY row (as the dense
            // builders always did: they grew to the highest node seen); the
            // dense form's trailing empty rows carry no information, so the
            // oracle is `from_dense` of the vector trimmed to that row.
            let mut trimmed = dense.clone();
            while trimmed.len() > 1 && trimmed[trimmed.len() - 1] == trimmed[trimmed.len() - 2] {
                trimmed.pop();
            }
            assert_eq!(built, RowIndex::from_dense(&trimmed), "builder vs from_dense on {dense:?}");
            for n in 0..dense.len() {
                assert_eq!(
                    built.row(n as u64).map(|r| (r.start, r.end)),
                    dense_row(&dense, n).map(|r| (r.start, r.end)),
                    "row {n} of {dense:?}"
                );
            }
        }
    }

    #[test]
    fn random_tables_round_trip_and_answer_the_dense_slice() {
        // A small deterministic LCG: the shapes must be reproducible.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for _ in 0..200 {
            let nodes = (next() % 300) as usize;
            let mut dense = Vec::with_capacity(nodes + 1);
            let mut cur = 0u32;
            for _ in 0..nodes {
                dense.push(cur);
                // 60% empty rows, else a degree of 1..=4.
                if next() % 10 >= 6 {
                    cur += 1 + next() % 4;
                }
            }
            dense.push(cur);
            check(&dense);
        }
    }
}

/// CSR-on-demand: one direction's adjacency rows grouped by node, built in
/// a single walk of the prefix and keyed on the commit clock. `index.row(n)`
/// is node n's slice (the dense `offsets[n]..offsets[n+1]`, kept sparse —
/// see `RowIndex`). MEASURED: expansions, exists() probes and degree counts
/// each opened a k-way visitor scan per node (~195 µs on the compacted
/// production index); the table turns them into slice reads.
pub struct AdjTable {
    /// The immutable CSR base. Behind `Arc`s so that repairing a table over a
    /// handful of changed nodes shares them instead of copying — the whole
    /// point of repairing rather than rebuilding.
    index: std::sync::Arc<RowIndex>,
    entries: std::sync::Arc<Vec<SlimAdj>>,
    /// Nodes whose row changed since the base was built, each mapped to its
    /// COMPLETE current row.
    ///
    /// A whole row rather than a set of edits, because that keeps `slice`
    /// returning a contiguous `&[SlimAdj]` — the accessor's shape, and its two
    /// call sites, are unchanged. A row here is produced by `adj_row_for`,
    /// which applies the same filters in the same index order as the base
    /// build, so a repaired table is indistinguishable from a rebuilt one.
    ///
    /// The rows are behind `Arc`s for the same reason `offsets` and `entries`
    /// are: a repair INHERITS this map from the table it repairs, and a table
    /// is immutable once published, so every repair copies every row an
    /// earlier repair left here. `ADJ_OVERLAY_FOLD` bounds how many rows that
    /// can be (4,096) but not what copying one costs — as `Vec<SlimAdj>` each
    /// was a fresh allocation and memcpy, so a repair that re-reads ONE
    /// changed node still performed up to 4,096 of them. Behind an `Arc` the
    /// inherited rows are refcount bumps and only the re-read rows allocate,
    /// which is the asymmetry repairing rather than rebuilding is supposed to
    /// have. `slice` still hands out `&[SlimAdj]`: `Arc<[T]>` derefs to `[T]`,
    /// so no accessor and no caller changes shape.
    overlay: BTreeMap<u64, std::sync::Arc<[SlimAdj]>>,
    /// Whether EVERY node's row — base and overlay alike — is non-decreasing
    /// in `peer`, so `edge_count_slim` may binary-search a row for one peer.
    ///
    /// ESTABLISHED, never inferred. The key layout (`tag|from|type|to|rel`)
    /// and the k-way merge that fills a table in key order make a single-type
    /// row sorted by `(peer, rel)`, and that is the property the probe relies
    /// on — but a property relied on silently is a property a later change to
    /// either can break without any test noticing, because a binary search
    /// over an unsorted row returns SOME count, not a crash. So the build
    /// checks every entry against its predecessor (O(1) each, on a pass that
    /// already decodes it), a repair checks each re-read row, and a fold
    /// carries the flag (it concatenates rows already checked). The probe
    /// consults the flag and walks when it is false. The untyped table is
    /// ordered by `(type, peer, rel)`, so its flag is usually false; the probe
    /// never asks it.
    sorted_by_peer: bool,
}

/// Whether `row` is non-decreasing in `peer` — the invariant behind
/// [`AdjTable::sorted_by_peer`], checked rather than assumed.
fn row_sorted_by_peer(row: &[SlimAdj]) -> bool {
    row.windows(2).all(|w| w[0].peer <= w[1].peer)
}

impl AdjTable {
    /// Live entries: the base plus the overlay's rows, minus what the overlay
    /// replaced. Used to report what warming built.
    fn len(&self) -> usize {
        if self.overlay.is_empty() {
            return self.entries.len();
        }
        // Each overlay row REPLACES that node's base row, so the base's
        // contribution for those nodes has to come out or the count double-
        // counts a repaired node.
        let replaced: usize = self
            .overlay
            .keys()
            .map(|n| self.index.row(*n).map_or(0, |r| r.len()))
            .sum();
        self.entries.len() - replaced + self.overlay.values().map(|r| r.len()).sum::<usize>()
    }

    fn slice(&self, node: u64) -> &[SlimAdj] {
        // The empty check first: this is the hottest read in the engine (one
        // call per expanded node), a freshly built table has no overlay, and
        // `is_empty` is a null-root test where `get` is a descent. Skipping it
        // keeps the no-overlay path exactly the code it was before repair
        // existed.
        if !self.overlay.is_empty() {
            if let Some(row) = self.overlay.get(&node) {
                return row;
            }
        }
        match self.index.row(node) {
            Some(r) => &self.entries[r],
            None => &[],
        }
    }

    /// Fix 48: every node with a NON-EMPTY row, ascending — the sources of
    /// an out-table, the destinations of an in-table. A repaired node's
    /// overlay row replaces its base row: emptied, it leaves; new, it joins.
    pub(crate) fn sources(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .index
            .nodes_with_rows()
            .filter(|n| self.overlay.get(n).is_none_or(|r| !r.is_empty()))
            .collect();
        if !self.overlay.is_empty() {
            for (n, r) in &self.overlay {
                if !r.is_empty() && self.index.row(*n).is_none() {
                    out.push(*n);
                }
            }
            out.sort_unstable();
        }
        out
    }

    /// The same table with its overlay folded into a fresh base — one pass
    /// over every row, so `slice` stops descending a map on each read.
    fn folded(&self) -> AdjTable {
        let base_nodes = self.index.nodes();
        let overlay_nodes = self
            .overlay
            .keys()
            .next_back()
            .map_or(0, |n| *n as usize + 1);
        let nodes = base_nodes.max(overlay_nodes);
        let mut index = RowIndexBuilder::new();
        let mut entries: Vec<SlimAdj> = Vec::with_capacity(self.len());
        for n in 0..nodes {
            let row = self.slice(n as u64);
            if !row.is_empty() {
                index.note(n as u64, entries.len());
                entries.extend_from_slice(row);
            }
        }
        AdjTable {
            index: std::sync::Arc::new(index.finish(entries.len())),
            entries: std::sync::Arc::new(entries),
            overlay: BTreeMap::new(),
            // Each row is copied whole from a row the flag already vouches
            // for, so the fold changes nothing the flag describes.
            sorted_by_peer: self.sorted_by_peer,
        }
    }

    /// The same table with `sorted_by_peer` CLEARED — the canary arm of the
    /// edge-probe differential, which must prove the walk answers exactly as
    /// the binary search does. Shares the base and clones the overlay.
    fn unsorted(&self) -> AdjTable {
        AdjTable {
            index: std::sync::Arc::clone(&self.index),
            entries: std::sync::Arc::clone(&self.entries),
            overlay: self.overlay.clone(),
            sorted_by_peer: false,
        }
    }
}

type AdjTables = BTreeMap<(u8, Vec<u32>), std::sync::Arc<Slot<AdjTable>>>;

/// The MERGE race-window hook — see [`Graph::set_merge_race_hook_for_test`].
pub type MergeRaceHook = std::sync::Arc<dyn Fn(&Graph) + Send + Sync>;

/// Fix 75: how many `yield_now`s `Graph::settle_in_flight_writers` spends
/// behind writers that are between their publish and their log record —
/// a few milliseconds at most, against a window measured in microseconds.
const SETTLE_SPINS: usize = 4_096;

/// What the derived structures hold — see [`Graph::memory_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryReport {
    /// Published adjacency tables.
    pub adjacency_tables: usize,
    /// Their directories' and rows' bytes.
    pub adjacency_bytes: usize,
    /// Published membership snapshots.
    pub memberships: usize,
    /// Their id bytes.
    pub membership_bytes: usize,
    /// Cached range indexes, scoped and partition-wide.
    pub range_indexes: usize,
    /// An estimate of their entry bytes (~64 B per entry).
    pub range_index_bytes: usize,
    /// Cached whole-label property columns (values and presence).
    pub prop_columns: usize,
    /// Their estimated bytes (entries plus string payloads).
    pub prop_column_bytes: usize,
}

/// The default byte budget of the property-column cache — see
/// `Graph::prop_column`. 512 MB holds every column the production corpus
/// reads several times over (the widest, NewsArticle's 150k-member columns,
/// are ~6 MB each); a store that reads more than this cycles the least
/// recently used out.
pub const PROP_COLUMN_BUDGET_BYTES: usize = 512 << 20;

/// One cached column: the sorted `(id, value)` entries of a property over a
/// label's members (`Values`), or the sorted ids carrying it (`Presence`).
#[derive(Clone)]
pub(crate) enum PropColumn {
    Values(std::sync::Arc<Vec<(u64, Value)>>),
    Presence(std::sync::Arc<Vec<u64>>),
}

struct PropColumnEntry {
    /// The commit clock when the column was read: current while nothing
    /// has been committed since (`Store::now_ts` is bumped by every write).
    at: u64,
    col: PropColumn,
    /// The value column ALIGNED to the label's members (position i = member
    /// i's value, Null where absent) — built on the first column-at-a-time
    /// read and kept beside the column, charged to the same budget. See
    /// [`Graph::prop_column_aligned`].
    aligned: Option<std::sync::Arc<Vec<Value>>>,
    bytes: usize,
    /// The cache's tick at the last hit — the LRU order.
    used: u64,
}

/// Whole-label property columns, keyed by `(label token, property token,
/// presence-only)`, under a byte budget with least-recently-used eviction.
struct PropColumnCache {
    entries: BTreeMap<(u32, u32, bool), PropColumnEntry>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl PropColumnCache {
    fn new(budget: usize) -> Self {
        PropColumnCache {
            entries: BTreeMap::new(),
            bytes: 0,
            budget,
            tick: 0,
        }
    }

    fn evict_to(&mut self, room: usize) {
        while self.bytes.saturating_add(room) > self.budget && !self.entries.is_empty() {
            let Some((&key, _)) = self.entries.iter().min_by_key(|(_, e)| e.used) else {
                break;
            };
            if let Some(e) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(e.bytes);
                counted!("graph.property column evicted");
            }
        }
    }
}

/// An estimate of a value's heap footprint beside its 32-byte enum slot —
/// the string bytes that dominate a column of titles or ids.
fn value_heap_bytes(v: &Value) -> usize {
    match v {
        Value::Str(s) => s.len(),
        Value::List(items) => items.iter().map(value_heap_bytes).sum::<usize>() + items.len() * 32,
        Value::Map(m) => m
            .iter()
            .map(|(k, v)| k.len() + 32 + value_heap_bytes(v))
            .sum::<usize>(),
        _ => 0,
    }
}

/// What `CALL engram.checkpoint()` established — see [`Graph::set_checkpoint_hook`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointReport {
    /// Sealed segments written to disk BY THIS CALL.
    pub spilled: usize,
    /// Sealed segments the store holds afterwards (all of them on disk when
    /// `resident` is 0).
    pub segments: usize,
    /// Sealed segments still resident afterwards — non-zero only if the
    /// spill failed part-way; the caller must treat it as "not durable".
    pub resident: usize,
    /// Versions in the unsealed tail afterwards — writes that landed after
    /// the seal this call took; zero on a quiescent server.
    pub tail: usize,
}

/// The checkpoint hook — the server's seal-and-spill, reachable from a
/// statement so a client (a loader, a pod's preStop hook) can make the store
/// durable and KNOW it is, instead of sleeping and hoping. See
/// [`Graph::set_checkpoint_hook`].
pub type CheckpointHook =
    std::sync::Arc<dyn Fn() -> Result<CheckpointReport, String> + Send + Sync>;

/// One published adjacency table, taken apart — see
/// [`Graph::adj_table_parts_for_test`]. Deliberately the RAW parts: a
/// differential over how a CSR was built has to compare the layout, because
/// the layout is what the next repair builds on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjTableParts {
    /// The CSR row starts, one per node id plus a terminator.
    pub offsets: Vec<u32>,
    /// The packed rows.
    pub entries: Vec<SlimAdj>,
    /// Whether every row is non-decreasing in `peer`.
    pub sorted_by_peer: bool,
    /// Rows repaired since the base was built.
    ///
    /// Owned `Vec`s rather than the table's own `Arc<[SlimAdj]>`: this is the
    /// differential's comparison type, and a differential over a layout must
    /// compare the LAYOUT, not how the two arms happen to share it.
    pub overlay: BTreeMap<u64, Vec<SlimAdj>>,
    /// The stamp the table is published at.
    pub at: u64,
}

/// What a stale adjacency table can still do for a SINGLE-NODE reader — see
/// [`Graph::adj_stale_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleProbe {
    /// The change set does not touch this node: serve the row the table holds.
    Unmoved,
    /// It does, but the delta is short enough to repair on this thread.
    MovedSmallDelta,
    /// It moved and the delta is past the reader's budget. Decline and walk:
    /// the table is still REPAIRABLE, so the maintenance pass will bring it
    /// back, and paying for that on a query thread is what §8 removed.
    LongDelta,
    /// The log no longer covers the table's stamp, so no repair can ever fix
    /// it — only a rebuild can. This must NOT decline: with the pass's rebuild
    /// demoted (§5.3) the reader is the only one left who rebuilds, and a
    /// reader that declines here leaves a table that never comes back and a
    /// span that is walked for ever. `demoted_adjacency_rebuild` states that
    /// as an invariant and is what caught the first cut conflating the two.
    Uncovered,
}

/// What resolving an adjacency table at an epoch did — reported by
/// `adj_table_snapshot_reporting` so the maintenance refresh can say whether
/// it repaired, rebuilt, or found the table already current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdjOutcome {
    /// The published snapshot was at or past the epoch (or another worker
    /// published one meanwhile).
    Current,
    /// Carried forward by re-reading the changed rows.
    Repaired,
    /// Rebuilt from a full span walk.
    Rebuilt,
    /// No table: not admitted, or declined by the entry budget.
    Declined,
    /// The slot was NOT advanced by this call: stale, not repairable, and
    /// the caller forbade a rebuild (the maintenance refresh's budget) — left
    /// as it was; or repaired, but the fenced publish lost to the stamp the
    /// slot already held (a writer registered at that stamp still in flight)
    /// or to a newer one. The repaired table is still returned and served.
    Deferred,
}

/// What resolving a membership snapshot did — the membership analogue of
/// [`AdjOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MembersOutcome {
    /// Current, or published by another worker meanwhile.
    Current,
    /// Caught up from the label's change log.
    CaughtUp,
    /// Rebuilt from a walk of the label.
    Rebuilt,
    /// The slot was NOT advanced: stale, uncovered by its log, and the
    /// caller forbade a rebuild; or caught up, but the fenced publish lost
    /// to the stamp the slot already held. The caught-up view is still
    /// returned and served.
    Deferred,
}

/// What [`Graph::warm`] built. Reported rather than logged inside the engine,
/// so the caller decides whether and how to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmReport {
    /// Nodes in the all-nodes membership snapshot.
    pub nodes: usize,
    /// Entries in the untyped outgoing adjacency table.
    pub out_edges: usize,
    /// Entries in the untyped incoming adjacency table.
    pub in_edges: usize,
    /// Adjacency tables cached: the untyped one plus one per relationship type,
    /// for each direction.
    pub tables: usize,
    /// Bytes the tables' entries and offsets HOLD (length × element size),
    /// summed over every table published by this warm.
    pub table_bytes: usize,
    /// Bytes the same vectors have ALLOCATED (capacity × element size). Both
    /// builders grow by `push` from `Vec::new()`, so this sits in
    /// [held, 2·held). The difference is VIRTUAL, not resident: measured on
    /// served SF3 (2026-09-02), 2,767 MB of it against 5,632 MB held, and a
    /// `shrink_to_fit` at publish moved RSS by 0 — the two arms' per-query
    /// RSS traces agreed within 30 MB at every stage, because pages a `Vec`
    /// reserved and never wrote are never faulted in. Reported so a reader
    /// can tell held from allocated, not because it costs RAM.
    pub table_capacity_bytes: usize,
    /// The commit clock when warming began.
    pub at: u64,
}

/// Entries above which a table is declined (memory is a budget, not a wish).
const ADJ_TABLE_MAX_ENTRIES: usize = 64 << 20;

/// Distinct adjacency tables cached before the cache is dropped wholesale.
///
/// This bounds a COMBINATORIAL risk, not a memory one: the key is
/// `(direction, type set)`, so a workload issuing `-[:A|B]->`, `-[:A|C]->`,
/// `-[:B|C]->` … can mint tables faster than any of them is reused. Per-table
/// memory is bounded separately by `adj_table_max_entries`.
///
/// It was **32**, which is exactly what an LDBC SNB schema needs: 15
/// relationship types plus the untyped table, in two directions. Warming
/// therefore evicted its own work on the last insert and left the cache
/// holding one table, so a 1.48M-node graph warmed for six seconds and then
/// still stalled five seconds on its first query — a fix that ran, reported
/// success, and did nothing. A limit met exactly by the thing it is meant to
/// accommodate is not a limit, it is a coincidence waiting to be discovered.
///
/// 512 leaves room for a schema several times larger than SNB's while still
/// refusing an unbounded type-set fan-out.
const ADJ_TABLE_CACHE_MAX: usize = 512;

/// The changed-node count up to which a stale table is ALWAYS repaired —
/// the fixed cap that predates the cost gate, kept as the gate's floor.
///
/// Repair admission is ADDITIVE: a change set the old rule admitted is still
/// admitted (this cap), and past it the cost model below decides. The first
/// cut of the cost gate REPLACED the cap and charged 64 rows per changed node
/// against half the span, which on the small fixtures every existing repair
/// test uses (a few hundred entries) declined every repair — three tests
/// went red and the properties they guard (a repaired table binary-searches,
/// serves emptied rows, survives a transaction's delete) were untested. The
/// `adj_cost_repair` lever's OFF arm is this cap ALONE, so the before/after
/// stays a measurement: on SF1 a 42k-write burst changed 9,892 persons'
/// `HAS_CREATOR` rows — over the cap — so repair declined and the rebuild
/// rescanned the whole 17.26M-row `[I]` span (25 s on paged storage).
const ADJ_REPAIR_MAX: usize = 4_096;


/// The fixed cost of re-reading ONE node's row, in rebuild-row units — what
/// the cost gate charges per changed node on top of the changed rows
/// themselves. A per-node prefix scan sets up a k-way merge (64 tail-shard
/// read latches and ranges, the sealed set, a merge cursor per source);
/// MEASURED at ~45 µs per node on a resident store with an unsealed tail
/// (300 nodes → 13.9 ms, 1,000 → 54 ms), against ~1.45 µs per row of a paged
/// rebuild (25 s / 17.26M rows) — 31 rows' worth — and ~0.24 µs per row of a
/// resident one (190 rows' worth). 32 is the paged figure, the storage where
/// a rebuild is catastrophic; on a resident store it under-charges repair
/// setup ~6×, which errs toward repair — the choice that touches only the
/// changed rows' blocks and neither re-faults nor churns the block cache.
///
/// The gate: `entries + nodes × this < table.len()`, where `entries` is the
/// log's count of changed rows since the build (exact) and the table is the
/// one being repaired. The span a rebuild actually walks is at least the
/// table (every type's rows, filtered), so the table is a LOWER bound on the
/// rebuild and the comparison errs toward rebuilding — the old behaviour —
/// never toward a repair that costs more than the walk. The first cut summed
/// every changed node's existing row into the work and compared against half
/// the untyped span: an all-persons burst then charged the whole typed table
/// as work and never repaired (SF1: built=1 repaired=0 on every arm).
const ADJ_REPAIR_SCAN_ROWS: usize = 32;

/// The absolute ceiling on a repair's changed-node set, whatever the cost
/// model says — a MEMORY bound (the set is collected before the rows are
/// re-read), not a crossover. One log holds at most `ADJ_LOG_CAP` entries,
/// so this is the most one table's logs can name before they overflow and
/// force a rebuild anyway.
const ADJ_REPAIR_MAX_NODES: usize = ADJ_LOG_CAP;

/// Per-node adjacency snapshots — (type token, peer id) pairs per
/// (node, direction tag) — keyed by the commit epoch they were read at.
type AdjCache = BTreeMap<(u64, u8), (u64, std::sync::Arc<Vec<(u32, u64)>>)>;

/// A single-source forward-BFS tree: the distance from the source to each
/// reachable node and the `(predecessor, rel)` that first reached it — enough to
/// answer a BOUNDED `shortestPath`'s length and reconstruct one such path.
#[derive(Default)]
pub(crate) struct BfsTree {
    pub dist: BTreeMap<u64, u64>,
    pub parent: BTreeMap<u64, (u64, u64)>,
}
/// `(source, dir tag, sorted type tokens, max)` → `(epoch, tree)`.
type BfsMemo = BTreeMap<(u64, u8, Vec<u32>, u64), (u64, std::sync::Arc<BfsTree>)>;

/// Fix 73: `(label, key, value, property epoch, sorted ids)` — see
/// `Graph::constant_end_ids`.
type EndSetMemo = Vec<(String, String, Value, u64, std::sync::Arc<Vec<u64>>)>;

/// Fix 73: how many ids a resolved constant end may hold — over this the
/// hop keeps its per-peer record test (a set this wide is no longer the
/// selective side of the hop).
pub(crate) const CONSTANT_END_CAP: usize = 65_536;

/// Fix 73: entries kept in the constant-end memo before it is cleared.
const END_SET_MEMO_MAX: usize = 64;

/// Per-label membership snapshots keyed by the commit epoch they were
/// read at — see `Graph::members`.
type MembersCache = BTreeMap<u32, std::sync::Arc<Slot<MembersView>>>;
/// Range indexes by property token, one monotone slot each.
/// Range indexes, keyed on `(label token, property token)` packed into a u64.
///
/// It used to be keyed on the PROPERTY alone, which made every index
/// partition-wide: `CREATE INDEX ... FOR (n:Churn) ON (n.id)` names a LABEL and
/// Cypher means it, but the built index covered every node in the partition
/// carrying `id`. On official SF1 that is 3.18M entries for an index the
/// operator scoped to a few hundred nodes — paid at every build, every fold and
/// in memory for the life of the process.
type RangeCache = BTreeMap<u64, std::sync::Arc<Slot<engram_store::RangeIndex>>>;

/// The cache key for a label-scoped index, or an unscoped one.
///
/// `u32::MAX` in the high half means "no label" — the partition-wide index,
/// which is still what an undeclared probe gets.
fn range_key(label_token: Option<u32>, prop_token: u32) -> u64 {
    ((label_token.unwrap_or(u32::MAX) as u64) << 32) | prop_token as u64
}
/// A transaction's buffered rows under one key prefix, in key order:
/// `(full body, put?)` — `false` is a buffered delete. Values deliberately
/// absent: every overlay consumer works on keys and presence.
type PendingRows = Vec<(Vec<u8>, bool)>;
/// As [`PendingRows`], WITH the buffered values — the rel-scan's shape.
type PendingRowValues = Vec<(Vec<u8>, Option<Vec<u8>>)>;
/// One buffered index-row change: `(body, key)` — `None` leaves the index.
type PendingIndexEntry = (Vec<u8>, Option<engram_store::IndexKey>);
/// Per-property change logs: each entry is a row body and the index key it
/// now carries (`None` = it left the index). See `Graph::note_prop_change`.
type PropLogs = BTreeMap<u32, ChangeLog<(Vec<u8>, Option<engram_store::IndexKey>)>>;
/// Per-creator messages sorted `(creationDate DESC, id ASC)` — the date-ordered
/// index behind IC2's k-way merge. Each entry is `(creationDate, message.id,
/// message node id)`: the first two are the ORDER BY keys (so the merge never
/// touches the store to rank), the third is the node for late projection.
pub(crate) type CreatorMsgs = BTreeMap<u64, Vec<(i64, i64, u64)>>;

/// The ids a vector index has yet to fold into its cached HNSW/vectors,
/// accumulated by the write path since the index was last built or caught
/// up. IDS ONLY — the vectors are point-got on apply, so a bulk load does
/// not hold a million embeddings in memory; past the cap the delta gives
/// up and the next query rebuilds the index in full (the old behaviour,
/// correct for bulk, wasteful only for it).
#[derive(Default)]
struct VectorDelta {
    upserts: std::collections::BTreeSet<u64>,
    deletes: std::collections::BTreeSet<u64>,
    /// Set once the delta outgrew `VECTOR_DELTA_CAP`: apply gives up and
    /// rebuilds. Deltas past the cap stop tracking ids (bounded memory).
    overflow: bool,
}

impl VectorDelta {
    fn note_upsert(&mut self, id: u64) {
        if self.overflow {
            return;
        }
        self.deletes.remove(&id);
        self.upserts.insert(id);
        self.check_cap();
    }
    fn note_delete(&mut self, id: u64) {
        if self.overflow {
            return;
        }
        self.upserts.remove(&id);
        self.deletes.insert(id);
        self.check_cap();
    }
    fn check_cap(&mut self) {
        if self.upserts.len() + self.deletes.len() > VECTOR_DELTA_CAP {
            self.overflow = true;
            self.upserts.clear();
            self.deletes.clear();
        }
    }
}

/// Past this many pending ids, a vector index rebuilds rather than catching
/// up id by id — a bulk load is a rebuild, a stream of writes is not.
const VECTOR_DELTA_CAP: usize = 4096;

// ── Change-log caps — see `derived::ChangeLog` ──────────────────────────────
//
// Every derived structure (label membership, range index, adjacency table) is
// caught up from an append-only, epoch-stamped change log of its SOURCE, and
// readers never consume the log. These caps bound the logs' MEMORY: past a cap
// the log gives up on catch-up for anything older than the overflow and those
// readers rebuild — the conservative direction. They are not crossover points;
// catch-up is O(delta) regardless of how much is pending.

/// Membership entries are a bare `(id, joined)`.
const LABEL_LOG_CAP: usize = 65_536;
/// Index entries carry a body and a key; smaller.
const PROP_LOG_CAP: usize = 16_384;

/// Ids a BULK load reserves per counter write. Bulk trades durability for
/// ingest rate by contract, so it can afford a large range: a crash abandons
/// the unused tail as gaps and the load is re-run anyway.
const BULK_ID_RANGE: u64 = 4_096;

/// Ids a SERVING session reserves per counter write. Smaller than bulk's
/// because a serving process restarts more often and each restart abandons the
/// unused tail as gaps — and because `AdjTable::offsets` is sized by the
/// MAXIMUM id, so a large reservation with low utilisation inflates it.
const SERVING_ID_RANGE: usize = 256;
/// Adjacency entries are node ids, two per relationship write — 16 bytes an
/// entry with its stamp, 4 MB a log at this cap, one log per (side, type).
///
/// It was 65,536: one SF1 all-persons burst (42-47k relationship writes, so
/// that many entries in EACH of `HAS_CREATOR`'s two logs) was within a
/// third of overflowing it, and an overflow is the 17M-row rebuild the cost
/// gate exists to avoid. The logs are pruned behind every publish and the
/// maintenance refresh publishes every `refresh_after_writes` stamps, so in
/// practice a log holds one refresh window; the cap is the bound for a
/// server running with the refresh off.
const ADJ_LOG_CAP: usize = 262_144;
/// Past this many repaired rows in an adjacency table's overlay, fold them into
/// a fresh base — the same `FOLD_AT` pattern the range index and the members
/// view use. An overlay is a `BTreeMap` consulted on every `slice`, so it is
/// kept small; a fold is O(entries), amortised over `ADJ_OVERLAY_FOLD` repairs.
const ADJ_OVERLAY_FOLD: usize = 4_096;
/// Base probes after which a membership base is answered from a presence
/// BITMAP rather than a binary search.
///
/// A probe count, not a size: a big label a workload barely touches must not
/// pay a build. Equal to `MEMBERS_FOLD_AT` so the two membership thresholds
/// age together, and small next to the per-query probe volume a hop's label
/// filter produces — the build is amortised over the very first query that
/// leans on the label.
///
/// MEASURED on the pod (v40, interleaved arms, one binary, SF1 paged):
/// `read-only` 905/923 -> 984/979 and `read-heavy` 878 -> 922, with
/// `ic6-friend-tags` 88.96 -> 82.28 ms — a shape that is 52% of read time and
/// does ~42,000 label probes per query. `mem_bitmaps` stayed at 3 per run
/// rather than tracking `mem_built`, so the probe threshold is doing its job
/// and not rebuilding. `--members-bitmap-after 0` is the arm that turns it off.
const MEMBERS_BITMAP_AFTER: usize = 4_096;
/// The most repair WORK — in the rows `adj_repair_cost_rows` prices, the same
/// meter the maintenance pass budgets with — a single-node READER will do on
/// its own query thread before declining the table and walking its own span.
///
/// A different question from `ADJ_REPAIR_MAX` and the cost model, which decide
/// repair-versus-REBUILD once a repair has been asked for. This decides whether
/// THIS caller should be the one to pay, and both regimes it separates are
/// real:
///
/// - **Light write pressure** — a couple of hundred changed nodes. Repairing is
///   cheap, it leaves a fresh table for every reader behind, and declining
///   would throw away the cached CSR for no gain. Today's behaviour, kept, and
///   what `adjacency_probe_slim{,_adversarial}` and `adjacency_cost_repair`
///   hold.
/// - **Heavy write pressure** — thousands. The repair re-reads a row per
///   changed node to answer about ONE, per read, growing with the write
///   stream. That is §8's interference; here the reader declines and the
///   maintenance pass republishes.
///
/// 8,192 rows is ~256 changed nodes at `ADJ_REPAIR_SCAN_ROWS` (32) — well above
/// a lightly-written table between passes and well below a mixed profile's.
const ADJ_READER_REPAIR_MAX_ROWS: usize = 8_192;
/// How far the cheap per-node staleness scan walks before giving up and
/// handing the question to the priced path. Bounds what a read pays to learn
/// "my node did not move", which is the answer most reads get.
const ADJ_STALENESS_SCAN_MAX: usize = 4_096;

/// Bounds on the per-key slot maps of the membership and range-index caches
/// (`derived::slot_in`): cleared wholesale past these, the same bound the
/// maps they replaced carried.
const MEMBERS_CACHE_MAX: usize = 1_024;
const RANGE_CACHE_MAX: usize = 256;

/// A property-equality seek fires only when the label has at least this many
/// nodes - below it the column scan is already sub-millisecond and building a
/// range index over the store is not worth amortising.
const PROPERTY_SEEK_MIN_LABEL: u64 = 512;

/// ...and only when the label is at least this many times the probe size: a
/// per-id node materialisation costs roughly this much more than one column
/// entry, so the seek wins only when its matches are that much rarer than the
/// label. Measured against the production port, where `probe < label` alone
/// admitted probes that were most of the label and regressed by 10x+.
const PROPERTY_SEEK_SELECTIVITY: u64 = 16;

/// ...and the match set must be small in ABSOLUTE terms: a full node decode
/// (blobs included) costs ~250x a column entry, so even a small FRACTION of a
/// large label - `WHERE nodeType = 'email'`, 5% of 600k UserDataNode - is tens
/// of thousands of decodes that dwarf the column scan. Above this the scan
/// wins however selective the ratio.
pub(crate) const PROPERTY_SEEK_MAX_PROBE: usize = 2048;

/// Rebuild a vector index once its HNSW holds this fraction more nodes than
/// are live — dead nodes (superseded or deleted ids, filtered at rescore)
/// bloat the graph and cost recall past a point.
pub(crate) const VECTOR_BLOAT_RATIO: f64 = 0.25;

/// One vector index's coordinates, cached so the write path does not scan
/// the schema on every mutation.
#[derive(Clone)]
pub(crate) struct VecIndex {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) prop: String,
}

// Lock-free cells for the config primitives (D2-revision read-scaling fix).
// (The Mutex-backed `SyncCell<T>` stand-in for `Cell<T>` that preceded them
// left with its last field — the candidate-batch share gate — in fix 61.)
// Config FLAGS/BUDGETS are read on EVERY query; a Mutex-backed cell made that 6+
// exclusive Mutex acquisitions per query, which serialised the whole read path
// under many workers. These expose the SAME `get`/`set` API over an atomic, so no
// call site changes — only the field type. Relaxed ordering: a config value is a
// plan hint with no happens-before relationship to the data it plans over.
macro_rules! atomic_cell {
    ($name:ident, $atomic:ty, $prim:ty) => {
        struct $name($atomic);
        impl $name {
            fn new(v: $prim) -> Self {
                Self(<$atomic>::new(v))
            }
            fn get(&self) -> $prim {
                self.0.load(std::sync::atomic::Ordering::Relaxed)
            }
            fn set(&self, v: $prim) {
                self.0.store(v, std::sync::atomic::Ordering::Relaxed)
            }
        }
    };
}
atomic_cell!(BoolCell, std::sync::atomic::AtomicBool, bool);
atomic_cell!(UsizeCell, std::sync::atomic::AtomicUsize, usize);
atomic_cell!(U64Cell, std::sync::atomic::AtomicU64, u64);

/// Lock-free `SyncCell<Option<i64>>` (the injected wall clock, set once per
/// statement). `i64::MIN` is the None sentinel — never a real epoch-ms.
struct OptI64Cell(std::sync::atomic::AtomicI64);
impl OptI64Cell {
    fn new(v: Option<i64>) -> Self {
        Self(std::sync::atomic::AtomicI64::new(v.unwrap_or(i64::MIN)))
    }
    fn get(&self) -> Option<i64> {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            i64::MIN => None,
            v => Some(v),
        }
    }
    fn set(&self, v: Option<i64>) {
        self.0
            .store(v.unwrap_or(i64::MIN), std::sync::atomic::Ordering::Relaxed)
    }
}

/// Lock-free `SyncCell<Option<usize>>` (the row budget, read per query).
/// `usize::MAX` is the None sentinel — never a real budget.
struct OptUsizeCell(std::sync::atomic::AtomicUsize);
impl OptUsizeCell {
    fn new(v: Option<usize>) -> Self {
        Self(std::sync::atomic::AtomicUsize::new(v.unwrap_or(usize::MAX)))
    }
    fn get(&self) -> Option<usize> {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            usize::MAX => None,
            v => Some(v),
        }
    }
    fn set(&self, v: Option<usize>) {
        self.0.store(
            v.unwrap_or(usize::MAX),
            std::sync::atomic::Ordering::Relaxed,
        )
    }
}

/// The epoch-scoped probe counter (was `SyncCell<(u64,u64)>`), lock-free: an
/// adjacency probe happens on EVERY hop, so a Mutex here serialised the whole
/// traversal under many workers. It is a HINT — it decides when a degree table
/// is worth building — so relaxed atomics with the occasional torn count across
/// threads are fine. Single-threaded it is bit-identical to the old get/set
/// pair (no interleaving), so the determinism trace is unchanged.
struct ProbeGate {
    epoch: std::sync::atomic::AtomicU64,
    count: std::sync::atomic::AtomicU64,
}
impl ProbeGate {
    fn new() -> Self {
        Self {
            epoch: std::sync::atomic::AtomicU64::new(0),
            count: std::sync::atomic::AtomicU64::new(0),
        }
    }
    /// The probe count in `epoch` WITHOUT recording one (0 on a fresh epoch).
    fn peek(&self, epoch: u64) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        if self.epoch.load(Relaxed) == epoch {
            self.count.load(Relaxed)
        } else {
            0
        }
    }
    /// Record a probe in `epoch`; return the count BEFORE it (0 on a fresh
    /// epoch, which also re-bases the counter to that epoch).
    fn tick(&self, epoch: u64) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        if self.epoch.load(Relaxed) == epoch {
            self.count.fetch_add(1, Relaxed)
        } else {
            self.epoch.store(epoch, Relaxed);
            self.count.store(1, Relaxed);
            0
        }
    }
}

/// A `Send + Sync` stand-in for `RefCell<T>`, part of the D2 revision. The API
/// is `RefCell`'s `borrow`/`borrow_mut`, so converting a cache field is a type
/// change with no call-site churn; the guards are `RwLock` guards, which `Deref`
/// to `T` exactly as `Ref`/`RefMut` do.
///
/// SAFETY OF THE SWAP (audited): where `RefCell` PANICS — a `borrow_mut` while a
/// borrow is live — a `RwLock` on ONE thread SELF-DEADLOCKS, and where `RefCell`
/// ALLOWS (nested shared `borrow`) a single-threaded `RwLock` read cannot block
/// (no writer can be waiting). So "the battery never panics here" (it exercises
/// every cache-build path) proves "no held guard is upgraded to a write", which
/// is exactly the deadlock precondition — the two are the same scenario. Under
/// real M3 concurrency this coarse latch is replaced by fine-grained ones.
/// Poison-tolerant like `engram_store`'s state latch.
struct SyncRefCell<T>(std::sync::RwLock<T>);

impl<T> SyncRefCell<T> {
    fn new(v: T) -> Self {
        SyncRefCell(std::sync::RwLock::new(v))
    }
    fn borrow(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }
    fn borrow_mut(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl<T: Default> Default for SyncRefCell<T> {
    fn default() -> Self {
        SyncRefCell(std::sync::RwLock::new(T::default()))
    }
}

/// The property-graph facade: one realm+namespace's graph.
pub struct Graph {
    store: Store,
    nodes: KeyPrefix,
    rels: KeyPrefix,
    index: KeyPrefix,
    kv: KeyPrefix,
    /// The injected wall clock (epoch ms) for `datetime()`/`timestamp()`.
    /// D1: time is a dependency — absent means those constructors refuse.
    wall_ms: OptI64Cell,
    /// See [`Graph::set_row_budget`].
    row_budget: OptUsizeCell,
    /// Entries one column read may cost per member before the columnar
    /// aggregate scan declines (× the member count).
    columnar_column_budget_factor: UsizeCell,
    /// The columnar paths (aggregate / projection / stage scans, column
    /// seeds) — `false` sends every statement down the general path.
    columnar_scans: BoolCell,
    /// Batch the columnar aggregate's member scan so the materialised property
    /// column is bounded to one batch, not the whole label (BI3's ~1.5M-row
    /// column, ~369 MB). `false` loads the whole column (the differential arm).
    columnar_agg_batch: BoolCell,
    /// Member-batch size for the columnar aggregate; a test lowers it to force a
    /// multi-batch split. `0` means the default `COLUMNAR_AGG_BATCH`.
    columnar_agg_batch_size: SyncRefCell<usize>,
    /// The single-primitive-key aggregate fast path — `false` forces the
    /// general `agg_key_of` serialization path (the differential-test arm).
    agg_native_key: BoolCell,
    /// The degree short-circuit for `count(src) GROUP BY dst` over one hop —
    /// `false` sends it through the ordinary chunk build + reduce (the
    /// differential-test arm).
    degree_aggregate: BoolCell,
    /// IC2's date-ordered k-way-merge fast path — `false` runs the ordinary
    /// expand + gather + top-k (the differential-test arm).
    ic2_ordered: BoolCell,
    /// IC11's anchored-endpoint semijoin fast path — `false` runs the ordinary
    /// multistage expand + filter (the differential-test arm).
    ic11_semijoin: BoolCell,
    /// BI7's 2-hop count-rollup fast path — `false` runs the ordinary expand +
    /// reduce (the differential-test arm).
    bi7_rollup: BoolCell,
    /// Morsel-parallel `expand` — `false` (the DEFAULT) runs the single-threaded
    /// expansion, so the determinism digest and every benchmark path are
    /// unchanged. `true` splits a large driving row-set across worker threads and
    /// concatenates their outputs IN ORDER — byte-identical to serial, proven by
    /// the A/B differential. Opt-in, so the default convention here is the
    /// reverse of the fast-path levers above.
    parallel_expand: BoolCell,
    /// IC3's date-windowed HAS_CREATOR last stage — seek each friend's in-window
    /// messages from the date-ordered `creator_msgs` index instead of reading
    /// every message's date. `false` runs the ordinary batched expansion.
    ic3_datewindow: BoolCell,
    /// The sorted-CSR edge-membership probe (`edge_count_slim`): a single-type
    /// row is binary-searched for the far end instead of walked. `false` walks
    /// every row (the differential-test arm).
    edge_probe: BoolCell,
    hop_reversal: BoolCell,
    scope_pruning: BoolCell,
    late_projection: BoolCell,
    /// Frontier-BFS variable-length expansion: a bounded `*1..n` hop whose end
    /// node is consumed DISTINCT-only runs as a set-at-a-time BFS over a
    /// visited set (each node reached once) instead of DFS path enumeration.
    /// `false` forces the enumerating path — the A/B lever for the differential.
    frontier_expand: BoolCell,
    property_seek: BoolCell,
    /// Fix 72: whether a `count(<chain var>)` over the chain a MATCH binds
    /// folds into its projection as `sum(COUNT { <chain> })`. Default on;
    /// off keeps the clause, for the differential test.
    chain_count_fold: BoolCell,
    /// Fix 76: a subquery body (a pattern comprehension, an EXISTS / COUNT
    /// pattern body) is seeded with every bound NODE trimmed to what the
    /// body reads of it, instead of a whole copy of the outer row. Default
    /// on; see `set_lean_subquery_seed`.
    lean_subquery_seed: BoolCell,
    /// Whether a MULTI-KEY pattern map may seek a declared index instead of
    /// falling to a label scan. See `set_pattern_map_seek`.
    pattern_map_seek: BoolCell,
    /// Whether `delete_node` enumerates incident relationship IDS instead of
    /// decoding every incident record. See `set_detach_via_rel_ids`.
    detach_via_rel_ids: BoolCell,
    /// Whether `label_epoch` reads an atomic beside the log instead of taking a
    /// read lock on the log itself. See `set_label_epoch_atomics`.
    label_epoch_atomics: BoolCell,
    /// Whether the adjacency GUARD row's puts are written volatile (visible,
    /// not logged). See `set_volatile_guards`.
    volatile_guards: BoolCell,
    /// Whether a DECLARED index is built over its label's members only, rather
    /// than the whole partition. See `set_label_scoped_indexes`.
    label_scoped_indexes: BoolCell,
    /// Whether a candidate enters the OCC read set only once it BECOMES a
    /// binding. Default OFF. See `set_read_set_bindings_only`.
    read_set_bindings_only: BoolCell,
    /// Whether an in-transaction count change is accumulated as a signed DELTA
    /// rather than discovered by cloning the counts twice. See
    /// `set_stats_delta`.
    stats_delta: BoolCell,
    /// Whether a full paged compaction also EMITS the adjacency CSRs and
    /// membership bases it walked past, instead of leaving them to be rebuilt
    /// by a separate O(corpus) scan. Default on; see `set_compaction_csr`.
    compaction_csr: BoolCell,
    /// Whether the MAINTENANCE pass declines to rebuild a stale adjacency
    /// table, leaving it to a repair, the next compaction, or a reader that
    /// actually wants it. Default on; see `set_demote_adjacency_rebuild`.
    demote_adj_rebuild: BoolCell,
    /// Whether the probe-count admission gate applies to a READER that holds a
    /// stale snapshot it cannot repair, keeping it off a full-span rebuild.
    /// Default on; see `set_reader_rebuild_admission`.
    reader_rebuild_admission: BoolCell,
    /// Whether a reader's REPAIR runs behind the per-table build guard, so N
    /// readers finding the same stale table do one repair between them instead
    /// of N. Default OFF — measured a regression; see `set_single_flight_repair`.
    single_flight_repair: BoolCell,
    /// Whether a single-node reader may be served from a STALE table when the
    /// change set does not touch its node. Default on; see
    /// `set_lazy_stale_serve`.
    lazy_stale_serve: BoolCell,
    /// Whether the lock-free filter fronts that check, instead of every such
    /// read taking the change log's lock. Default on; see
    /// `set_adj_change_filter`.
    adj_change_filter_on: BoolCell,
    /// Overlay rows a repaired table may carry before it is folded into a
    /// fresh base. Default `ADJ_OVERLAY_FOLD`; see `set_adj_overlay_fold`.
    adj_overlay_fold: UsizeCell,
    /// Whether a hop's label filter is answered by `MembersView::contains`
    /// (base + overlay, O(log n)) instead of materialising the whole label and
    /// binary-searching that. Default on; see `set_hop_membership_contains`.
    hop_membership_contains: BoolCell,
    /// Base probes after which a membership base is answered from a presence
    /// BITMAP instead of a binary search. `0` never builds one. Default
    /// `MEMBERS_BITMAP_AFTER`; see `set_members_bitmap_after`.
    members_bitmap_after: UsizeCell,
    /// Whether a single-node reader whose node DID move declines the table and
    /// walks its own span, instead of repairing the whole change set on its
    /// query thread. Default on; see `set_single_node_stale_walk`.
    single_node_stale_walk: BoolCell,
    /// Whether the derived bases are written to a SIDECAR after a compaction
    /// and adopted from one at open. Default on; see `set_persist_derived`.
    persist_derived: BoolCell,
    /// The vintage of the last sidecar this process wrote — the sealed-set id
    /// AND how many bases were published when it was written — so a quiescent
    /// tick that recurs does not rewrite an unchanged 1.69 GB file, while a
    /// tick after MORE bases were published (the warm finishing behind an
    /// early tick, a lazily built table) does write them. Keyed on the sealed
    /// set alone, the production mirror wrote a sidecar holding one membership
    /// before its 74 s warm finished and then skipped every tick for the life
    /// of the process: 318 tables rebuilt at every start.
    persisted_vintage: std::sync::atomic::AtomicU64,
    /// The sealed-set id the last sidecar described, kept beside the vintage
    /// so a tick can tell "the sealed set moved" (write now — the file on disk
    /// names rows this store does not have) from "more bases were published"
    /// (write, but not more often than `persist_growth_interval_secs`).
    persisted_sealed_id: std::sync::atomic::AtomicU64,
    /// The caller's monotonic seconds at which the last sidecar was written.
    persisted_at_secs: std::sync::atomic::AtomicU64,
    /// The caller's monotonic seconds at the last `persist_derived_now` tick —
    /// the clock a compaction's own write is stamped with, since the engine
    /// reads no clock of its own.
    last_tick_secs: std::sync::atomic::AtomicU64,
    /// Minimum seconds between two sidecar writes whose only reason is that
    /// MORE bases were published on an unchanged sealed set. v84 in
    /// production rewrote the 1.3 GB file nine times in four minutes — once
    /// per membership the shadow traffic built — because every new base was
    /// a new vintage. Growth is worth persisting; it is not worth persisting
    /// on every tick. A sealed-set move still writes at once. Default 600.
    persist_growth_interval_secs: std::sync::atomic::AtomicU64,
    /// §7 — whether a transaction's node-pattern predicates are validated
    /// against the rows committed since its snapshot. Default OFF; see
    /// `set_precision_locking`.
    precision_locking: BoolCell,
    /// Bounded-memory batching of the two-stage top-k tail (Track B working-set
    /// bound): fold the stage-2 expansion into a bounded accumulator one driving
    /// batch at a time. Default on; `false` forces the whole-chunk expand — the
    /// A/B lever proving the batched path is byte-identical.
    multistage_topk_batch: BoolCell,
    /// Degree tables per (direction, type set), built from ONE walk of the
    /// adjacency prefix and keyed on the commit clock. MEASURED: the
    /// degree-histogram census made 1.79M `count_adjacent` calls at ~195 µs
    /// each — every call opened two k-way visitor scans over the compacted
    /// index partition, a setup cost proportional to its segment count that
    /// no small repro reproduces. Engaged LAZILY after `DEGREE_TABLE_AFTER`
    /// probes in one epoch, so a single-node query never pays a full walk.
    degree_tables: arc_swap::ArcSwap<DegreeTables>,
    /// Probes served directly this epoch, and the epoch they count for.
    degree_probes: ProbeGate,
    /// Direct probes tolerated before a table builds — settable so tests
    /// and the sim reach the table on small graphs.
    degree_table_after: U64Cell,
    /// The count store — `None` until rebuilt for a pre-populated store.
    stats: SyncRefCell<Option<Stats>>,
    /// The constraint list, cached against the schema epoch — see
    /// `Graph::constraints_snapshot`. Per Graph INSTANCE (the server shares
    /// one per coordinate); the epoch read validates it on every use.
    constraint_cache:
        SyncRefCell<Option<(u64, std::sync::Arc<Vec<schema::LoadedConstraint>>)>>,
    /// The DECLARED range indexes (`CREATE INDEX ... FOR (n:L) ON (n.p)`),
    /// cached against the same schema epoch — see
    /// `Graph::declared_range_indexes`.
    ///
    /// These were stored by the DDL and read by nothing: `IndexDef::Range`'s
    /// own doc says "the scan planner consumes it in a later slice". This is
    /// that slice. Until now `CREATE INDEX` wrote a catalogue row and changed
    /// no plan, so a workload could declare exactly the index it needed and be
    /// answered by a label scan anyway.
    range_index_cache: SyncRefCell<Option<(u64, std::sync::Arc<Vec<schema::RangeIndexDef>>)>>,
    /// Whether the Bolt retry loop may ESCALATE a write-write conflict to
    /// the store's FIFO entity lock (W2.2) — the A/B arm's toggle.
    conflict_escalation: BoolCell,
    /// The installed morsel executor (W3) — absent means every operator
    /// takes its serial path. See `scoped_exec`.
    exec: SyncRefCell<Option<std::sync::Arc<dyn ScopedExec>>>,
    /// The fewest driving rows worth splitting: below this, spawn/join
    /// overhead exceeds the win and the serial loop runs. Settable so the
    /// differential tests exercise the parallel machinery on small corpora.
    parallel_min_rows: UsizeCell,
    /// Relationship populations by type set — see `Graph::rel_members`.
    rel_members_cache: arc_swap::ArcSwap<RelMembersCache>,
    /// Adjacency tables per (direction tag, type set), epoch-keyed; built
    /// after the same probe count that admits a degree table.
    adj_tables: arc_swap::ArcSwap<AdjTables>,
    /// Whether an aggregating projection with ORDER BY + LIMIT selects its
    /// `skip+limit` survivor GROUPS from the finished aggregates BEFORE
    /// evaluating the projection items, instead of projecting every group and
    /// truncating after the sort. Default on; see `set_agg_topk_before_project`.
    agg_topk_before_project: BoolCell,
    /// Whether `MATCH … RETURN <literals/params> [SKIP] [LIMIT]` is answered
    /// through the count fold — the match count decides how many copies of
    /// the one constant row come back — instead of enumerating the pattern in
    /// source order. Default on; see `set_const_projection_fold`.
    const_projection_fold: BoolCell,
    /// A one-shot hook fired inside MERGE between its empty match and its
    /// create — the race window — standing in for another writer's commit.
    /// Test forcing only; see `set_merge_race_hook_for_test`.
    merge_race_hook: std::sync::RwLock<Option<MergeRaceHook>>,
    /// The server's seal-and-spill; see `set_checkpoint_hook`.
    checkpoint_hook: std::sync::RwLock<Option<CheckpointHook>>,
    /// `count_hop` answers memoised per (start labels, dir, types, end labels),
    /// valid while BOTH the types' adjacency epoch and every named label's
    /// membership epoch stand still. Default on; see `set_hop_count_memo`.
    ///
    /// The map is small — one entry per hop SHAPE the workload plans — and the
    /// cap exists only so a shape-generating adversary cannot grow it.
    hop_count_memo: SyncRefCell<BTreeMap<HopCountKey, HopCountEntry>>,
    /// The lever for the memo above.
    hop_count_memo_on: BoolCell,
    /// `count_hop_estimate` answers, memoised SEPARATELY from the exact map —
    /// an estimate served where an answer was asked is a correctness bug, and
    /// keeping two maps makes that confusion unrepresentable.
    hop_estimate_memo: SyncRefCell<BTreeMap<HopCountKey, HopCountEntry>>,
    /// The estimator's sample budget: a labelled side larger than this is
    /// STRIDE-SAMPLED down to about this many probes and scaled back up.
    /// Settable so a small-fixture canary can force the path.
    estimate_sample_budget: UsizeCell,
    /// Whether a thread re-serves the adjacency snapshot it just resolved when
    /// the next probe asks the same (table, freshness) question. Default on;
    /// see `set_adj_snap_memo`.
    adj_snap_memo: BoolCell,
    /// Whether a DIRECTED fold close probes from the BOUND endpoint's row
    /// (with the direction flipped) instead of the level var's — the hot-row
    /// locality `Dir::Both` closes already have. Default on; see
    /// `set_directed_bound_probe`.
    directed_bound_probe: BoolCell,
    /// Whether `fold_tail` splits its driving rows into morsels across the
    /// installed [`ScopedExec`] — P-2 of the floor plan. Default off, exactly
    /// as `parallel_expand`: the engine never spawns, and the digest and every
    /// published single-thread number run serial. See `set_parallel_fold`.
    parallel_fold: BoolCell,
    /// This graph's identity, for the thread-local memo above.
    ///
    /// A pointer would be the obvious key and is WRONG: a dropped `Graph` frees
    /// its address for the next one, and the tests build graphs in a loop, so a
    /// second graph at a recycled address would inherit the first's tables.
    /// A counter never repeats.
    graph_id: u64,
    /// The adjacency-table entry budget — settable so tests and the sim can
    /// exercise the decline on small graphs.
    adj_table_max_entries: UsizeCell,
    /// Per-node adjacency snapshots, keyed (node, direction tag) against
    /// the commit clock — the both-endpoints-bound existence probe reads
    /// these instead of re-scanning the adjacency prefix per evaluation.
    /// Bounded: the whole map clears at 65,536 entries.
    adj_cache: SyncRefCell<AdjCache>,
    /// Single-source forward-BFS trees for a BOUNDED `shortestPath`, keyed by the
    /// commit epoch and `(source, dir, types, max)` — IC1's many `firstName='Ana'`
    /// seeds share source p=10, so its bounded neighbourhood is built ONCE and each
    /// seed is an O(path) distance lookup. Bounded: clears at 4,096 entries.
    bfs_memo: SyncRefCell<BfsMemo>,
    /// Fix 73: a hop end with a VAR-FREE one-key map on a declared key
    /// (`(:User {userId: $u})`) resolved ONCE into the sorted ids that carry
    /// the label and the value — `(label, key, value, property epoch, ids)`,
    /// a handful of entries, cleared when it overflows. Every peer of every
    /// row is then a binary search, never a projected record read.
    end_set_memo: SyncRefCell<EndSetMemo>,
    // ── Derived structures — see `derived.rs` for the one rule they share ──
    //
    // Each family below is: a SOURCE change log (append-only, epoch-stamped,
    // never consumed by readers), a map of monotone SLOTS holding the current
    // snapshot per key, and a build-path guard. The source's epoch lives in
    // its log; nothing here is validated against the global commit clock.

    /// Membership snapshots per label token (`u32::MAX` = every node), one
    /// monotone slot each. A snapshot is a [`MembersView`] — a shared base plus
    /// a small overlay — so a catch-up is O(delta), never a copy of the label.
    members_cache: arc_swap::ArcSwap<MembersCache>,
    /// Change log per label token: `(id, joined)` entries stamped with the
    /// commit clock. Written only by `note_membership_of`, from the four sites
    /// that touch a membership row.
    label_log: SyncRefCell<BTreeMap<u32, ChangeLog<(u64, bool)>>>,
    /// Change log per property token: `(entity body, new index key)` entries,
    /// `None` when the row leaves the index. A property's log is created when
    /// an index for it is first built, so every write after that is carried
    /// forward; writes before it do not matter to an index built later.
    prop_log: SyncRefCell<PropLogs>,
    /// The clock at which ANY relationship was created or deleted — the source
    /// clock of the UNTYPED adjacency table, which covers every type.
    adj_epoch: std::sync::atomic::AtomicU64,
    /// Per-type adjacency epochs — the clock of the newest change to each
    /// relationship type — LOCK-FREE, because the probe gate and every table
    /// validity check read them on every hop. The change logs carry the same
    /// stamps for catch-up; these are the fast validity check beside them.
    /// Raised by `fetch_max`, never stored: two writers may observe the clock
    /// in one order and publish in the other.
    /// Per-label change epochs, as ATOMICS beside the log rather than a field
    /// read out of it.
    ///
    /// `label_epoch` used to take a READ lock on `label_log` — the same lock
    /// `note_membership_of` takes in WRITE mode on every node create and every
    /// node delete. So every `members()` read serialised against every
    /// membership write, which is the mechanism behind the derived-refresh tax
    /// showing up at ONE client: the maintenance pass and the single writer
    /// were contending on one lock.
    ///
    /// Exactly the shape `adj_type_epoch` below already had for adjacency; the
    /// membership side simply never got it.
    label_epoch_map:
        arc_swap::ArcSwap<BTreeMap<u32, std::sync::Arc<std::sync::atomic::AtomicU64>>>,
    adj_type_epoch:
        arc_swap::ArcSwap<BTreeMap<u32, std::sync::Arc<std::sync::atomic::AtomicU64>>>,
    /// Change log per `(direction tag, relationship type)`: the nodes whose
    /// row for that tag and type changed. A stale table is repaired over
    /// exactly those rows. The type's epoch lives in its logs.
    adj_log: SyncRefCell<BTreeMap<(u8, u32), ChangeLog<u64>>>,
    /// The lock-free front of `adj_log` for the single-node staleness
    /// question — see `derived::AdjChangeFilter`. Written inside the same
    /// critical section as the log and strictly before it, so it is never the
    /// staler of the two.
    adj_change_filter: derived::AdjChangeFilter,
    /// THE WRITE FENCE — the low-water mark of in-flight writers: the visible
    /// clock each observed before its first row write → how many observed
    /// it. A direct writer registers for the span of its write method, a
    /// transaction for its commit; every publisher of a derived snapshot
    /// clamps its stamp below the lowest entry (`Graph::fenced`). See
    /// `derived.rs`, "The write fence". A `BTreeMap` for its ordered first
    /// key; a `Mutex` because it is touched twice per write and once per
    /// publish, microseconds apart, and never held across anything else.
    inflight: std::sync::Mutex<BTreeMap<u64, u32>>,
    /// Anchor a fresh pattern at its most selective endpoint rather than at
    /// whichever one was written first. Default on; off is the pre-fix
    /// behaviour, kept switchable so the gain is measurable in one run.
    selective_anchor: BoolCell,
    /// Carry the range index, membership snapshots and adjacency tables forward
    /// across writes instead of rebuilding them. Default on; off is the pre-fix
    /// behaviour.
    incremental_caches: BoolCell,
    /// Decide repair-vs-rebuild of a stale adjacency table by COST — the
    /// changed rows' re-read work against half the span a rebuild walks —
    /// instead of the fixed `ADJ_REPAIR_MAX` node cap. Default on; off is the
    /// cap (the arm that declined repair on 9,892 changed persons and rescanned
    /// 17M rows). See `repaired_adj_table`.
    adj_cost_repair: BoolCell,
    /// Whether two relationship writes touching one node may commit without
    /// aborting each other (RC1 / O3). See `set_guard_put_put_exempt`.
    guard_put_put_exempt: BoolCell,
    /// Whether a constraint-list cache HIT may skip the schema-epoch store
    /// probe and register the read set entry directly. See
    /// `set_constraint_epoch_cache`.
    constraint_epoch_cache: BoolCell,
    /// Ids a serving session reserves per counter write. `0` or `1` restores
    /// one logged counter write per entity. See `set_id_reservation`.
    id_reservation: UsizeCell,
    /// ROWS a single `refresh_stale_derived` pass may re-read before it defers
    /// the rest to the next pass.
    ///
    /// The rebuild budget (one per pass) was never the expensive half: REPAIRS
    /// were unbounded, and SF1 carries ~32 adjacency tables, so one pass could
    /// repair all of them back to back. Measured on the pod, that cost 2-3x of
    /// write throughput with the 10th-percentile second at 0.08 of the median
    /// — and lengthening the tick did NOT help, because the cost is the PASS,
    /// not its frequency. A pass is now O(this), whatever the corpus holds;
    /// what it defers, the next pass takes.
    refresh_pass_rows: UsizeCell,
    /// Fold a membership snapshot's pending changes with one sort and one
    /// merge pass (`MembersView::apply_batched`) instead of a sorted insert
    /// per change. Default on; off is the O(k²) serial fold, the differential
    /// arm.
    members_batch_fold: BoolCell,
    /// Walk the adjacency span under the block cache's SCAN policy when a
    /// table is (re)built (`Store::for_each_key_span_scan`): a paged rebuild
    /// then neither promotes what it crosses nor displaces a reader's working
    /// set, and admits into free room only. Default on; off is the plain walk.
    /// Resident stores are byte-identical either way.
    scan_resistant_rebuild: BoolCell,
    /// Bulk-ingest mode: writes skip the commit log (the importer's
    /// re-ingest IS the durability story) and ids reserve in ranges.
    bulk_ingest: BoolCell,
    /// Reserved id ranges per counter: (next, end-exclusive).
    id_reservations: SyncRefCell<BTreeMap<String, (u64, u64)>>,
    /// Range indexes per property token, one monotone slot each — see
    /// [`Graph::index_probe_eq`].
    range_cache: arc_swap::ArcSwap<RangeCache>,
    /// Whole-label PROPERTY COLUMNS the columnar walks read, kept between
    /// statements under a byte budget — see [`Graph::prop_column`].
    prop_columns: std::sync::Mutex<PropColumnCache>,
    /// name -> the cached ANN build.
    ann_cache: SyncRefCell<BTreeMap<String, AnnEntry>>,
    /// The date-ordered per-creator message index (IC2's native lever): each
    /// creator maps to its messages sorted `(creationDate DESC, id ASC)`. Keyed by
    /// the commit epoch; a prototype of ordered adjacency at seal — see
    /// `creator_sorted_messages`.
    creator_msgs: SyncRefCell<Option<(u64, std::sync::Arc<CreatorMsgs>)>>,
    /// Per-vector-index pending writes since its cache was last current.
    vector_deltas: SyncRefCell<BTreeMap<String, VectorDelta>>,
    /// The vector indexes, cached; `None` until first needed, cleared when
    /// an index is created or dropped.
    vector_index_list: SyncRefCell<Option<std::sync::Arc<Vec<VecIndex>>>>,
    /// The injected timezone rules (D1 for tz). None = fixed zones only.
    zone_provider: SyncRefCell<Option<std::sync::Arc<dyn engram_cypher::ZoneProvider>>>,
    /// The exact-vs-ANN crossover: at or below this many eligible vectors the
    /// planner scans exactly (R26: brute force is exact AND faster below the
    /// crossover). Provisional default, measured by the bench harness;
    /// configurable so tests and the sweep can cross the boundary cheaply.
    vector_exact_max: UsizeCell,
    /// name → token caches, loaded lazily, write-through.
    labels: arc_swap::ArcSwap<BTreeMap<String, u32>>,
    types: arc_swap::ArcSwap<BTreeMap<String, u32>>,
    props: arc_swap::ArcSwap<BTreeMap<String, u32>>,
    /// token → name, the reverse cache. Tokens never rename (the catalog is
    /// append-only), so this cache cannot go stale — and without it every
    /// node materialisation paid one store read PER PROPERTY for the name.
    rev_names: arc_swap::ArcSwap<BTreeMap<(&'static str, u32), String>>,
    /// The allocation latch (D2 revision). `next_id` and `token` mint monotonic
    /// ids/tokens by reading a store counter and writing it back — a
    /// read-modify-write that the old "one shard, no await between = atomic"
    /// comment relied on, and that real threads break. This serializes JUST the
    /// mint, so two threads creating nodes never collide on an id or a token
    /// counter. It is uncontended (and event-free) on the single-threaded path,
    /// so it does not perturb the determinism trace. A later step replaces it
    /// with a non-contending block/atomic allocator (the throughput form).
    alloc: std::sync::Mutex<()>,
    /// Per-entity write latches, striped by id. A read-modify-write of one
    /// RECORD — `SET` (read the record, change one property, write it back),
    /// a label added or removed, a delete — holds its stripe from the read to
    /// the put, so two sessions updating different properties of the same
    /// node cannot each write back a record missing the other's property, and
    /// a `SET` cannot resurrect a node a concurrent delete removed.
    ///
    /// What it does NOT cover, stated plainly: a STATEMENT-level
    /// read-modify-write such as `SET n.hits = n.hits + 1`, whose read
    /// happened when `MATCH` materialised the node, before `set_prop` ran.
    /// Measured through the Bolt server: 8 clients × 200 of those on one node
    /// landed 756 of 1,600. That needs the statement to run as a transaction
    /// whose commit validates what it read (the M3 item "wire the transaction
    /// into the write path"), not a latch — see `tests/hot_key_updates.rs`.
    ///
    /// Lock order, never reversed: a node's stripe before a relationship's (a
    /// detach delete holds the node while it deletes its relationships;
    /// nothing holds a relationship and then wants a node). Uncontended and
    /// event-free single-threaded, so the determinism trace is unchanged.
    entity_latches: Vec<std::sync::Mutex<()>>,
    /// Whether the entity latches are taken (default on). Off is the A/B and
    /// the canary's arm: `tests/hot_key_updates.rs` proves the latch does
    /// something by showing the loss without it.
    entity_latching: BoolCell,
    /// Whether the Bolt adapter runs every autocommit statement as a
    /// read-validated transaction (default on) — see
    /// `BoltServer::run`. Off is the A/B arm and the canary's:
    /// `tests/hot_key_updates.rs` shows the lost increments without it.
    serialisable_autocommit: BoolCell,
}

/// Stripes in [`Graph::entity_latches`]: enough that unrelated entities rarely
/// share one under a spread write load, few enough to be a trivial table.
const ENTITY_LATCH_STRIPES: usize = 1024;

// The D2-revision canary for the Graph layer (the store has its own). The Graph
// WAS `!Send`/`!Sync` by way of its `Cell`/`RefCell`/`Rc` members; the concurrent
// write program requires it to cross threads, so `Send + Sync` is now load-bearing
// and this fails the BUILD if a future field reintroduces a non-thread-safe member
// (a bare `Cell`/`RefCell`, or an `Rc` in a cache value).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Graph>();
};

/// An in-flight writer's registration in the write fence — see
/// [`Graph::fence`]. Dropping it unregisters; `None` when nothing was
/// registered (a buffered write inside a transaction).
struct WriteFence<'g> {
    graph: &'g Graph,
    at: Option<u64>,
}

impl Drop for WriteFence<'_> {
    fn drop(&mut self) {
        let Some(at) = self.at else { return };
        let mut g = self
            .graph
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.get_mut(&at) {
            *n -= 1;
            if *n == 0 {
                g.remove(&at);
            }
        }
    }
}

/// An owned write transaction a session carries between statements. Opaque on
/// purpose — a session holds it, installs it around each statement it runs with
/// [`Graph::with_txn`], and concludes it with [`Graph::commit_owned`] or
/// [`Graph::rollback_owned`]. Dropping it without committing discards its
/// buffered writes (nothing was published), so a dropped session rolls back.
pub struct GraphTxn {
    inner: engram_store::Transaction,
    /// The derived sources this transaction's buffered writes will change,
    /// applied to their change logs when it commits
    /// (`Graph::touch_after_commit`), dropped when it rolls back.
    touched: TxnTouched,
}

/// The sources a buffered transaction has changed. A mutation site records
/// the SOURCE here while a transaction is installed instead of writing the
/// change log — the write is not committed, so no reader may catch up with
/// it yet. At commit the entries remembered here are REPLAYED into the
/// change logs, stamped with the commit clock, so readers catch up O(delta)
/// exactly as they do from a direct write.
#[derive(Default)]
struct TxnTouched {
    /// Membership changes per label token — `(id, joined)`, in the order
    /// they happened — replayed into the label logs at commit. The ENTRIES,
    /// not only the tokens: a first cut remembered which sources a
    /// transaction touched and `touch`ed their logs at commit, which a
    /// reader can only answer with a REBUILD. Once every statement was a
    /// transaction, every insert made the next `MATCH` rebuild two
    /// memberships, a range index and two adjacency tables from scratch —
    /// measured on the pod as 130 ms per insert, 8 ops/s where the direct
    /// path did 4,200. Carrying the entries makes a committed transaction
    /// cost its readers exactly what the direct write costs them: O(delta).
    labels: BTreeMap<u32, Vec<(u64, bool)>>,
    /// Index-row changes per property token: `(body, key)`.
    props: BTreeMap<u32, Vec<PendingIndexEntry>>,
    /// Adjacency-row changes per (side, type token): the node whose row moved.
    adj: BTreeMap<(u8, u32), Vec<u64>>,
    /// §7 — the node restrictions this transaction's unbound scans imposed,
    /// deduplicated. Lives here rather than in a thread-local of its own
    /// because this is already the bag of "things accumulated during this
    /// transaction", with exactly the right lifetime: it travels with the
    /// transaction across `with_txn` calls and is dropped on rollback.
    restrictions: Vec<crate::precision::Restriction>,
    /// Vector-index changes per index name: `(id, upsert)` in order —
    /// applied to the shared `vector_deltas` at commit, dropped on rollback.
    /// Buffered writes must not reach the shared deltas before commit: the
    /// next `vector_query` on ANY session consumes a delta into the shared
    /// ANN cache, and it would read either nothing (the row is not committed
    /// — the id silently leaves the index for ever) or, on the writer's own
    /// thread, the buffered value of a write that may yet roll back.
    vectors: BTreeMap<String, Vec<(u64, bool)>>,
    /// The net change the transaction's buffered writes make to the count
    /// store, applied at commit. The count store used to be DROPPED at every
    /// commit and rollback and rebuilt by the next count with two prefix
    /// walks — O(corpus) per write statement once every statement is a
    /// transaction, and a race besides (a reader between the drop and its
    /// own rebuild found nothing).
    stats: StatsDelta,
}

/// A signed change to [`Stats`].
#[derive(Default)]
struct StatsDelta {
    nodes: i64,
    rels: i64,
    by_label: BTreeMap<u32, i64>,
    by_type: BTreeMap<u32, i64>,
}

/// An explicit, signed change to [`Stats`] — what a mutation DID, rather than a
/// closure whose effect has to be discovered by applying it to a copy.
///
/// `stats_apply` took a closure, so the only way to learn its effect inside a
/// transaction was `before = committed.clone(); after = before.clone();
/// f(&mut after); diff`. Two `Stats` clones per call, each deep-copying two
/// `BTreeMap`s, six times for a `CREATE (a)-[:R]->(b)`.
#[derive(Default)]
struct StatsChange {
    nodes: i64,
    rels: i64,
    by_label: Vec<(u32, i64)>,
    by_type: Vec<(u32, i64)>,
}

impl StatsDelta {
    /// Accumulate an explicit change — no clone, no diff.
    ///
    /// The seeding `stats_apply` did (applying the transaction's own delta to
    /// `before` so a saturating decrement could see it) is unnecessary here:
    /// accumulation is signed, so nothing saturates until the delta is applied
    /// to the committed counts at commit.
    fn add_change(&mut self, c: &StatsChange) {
        self.nodes += c.nodes;
        self.rels += c.rels;
        for (t, d) in &c.by_label {
            *self.by_label.entry(*t).or_insert(0) += *d;
        }
        for (t, d) in &c.by_type {
            *self.by_type.entry(*t).or_insert(0) += *d;
        }
    }

    /// `after - before`, accumulated.
    fn add_diff(&mut self, before: &Stats, after: &Stats) {
        self.nodes += after.nodes as i64 - before.nodes as i64;
        self.rels += after.rels as i64 - before.rels as i64;
        for (t, a) in &after.by_label {
            let b = before.by_label.get(t).copied().unwrap_or(0);
            if *a != b {
                *self.by_label.entry(*t).or_insert(0) += *a as i64 - b as i64;
            }
        }
        for (t, a) in &after.by_type {
            let b = before.by_type.get(t).copied().unwrap_or(0);
            if *a != b {
                *self.by_type.entry(*t).or_insert(0) += *a as i64 - b as i64;
            }
        }
    }

    fn apply(&self, st: &mut Stats) {
        st.nodes = st.nodes.saturating_add_signed(self.nodes);
        st.rels = st.rels.saturating_add_signed(self.rels);
        // A NEGATIVE delta on an ABSENT key contributes nothing, rather than
        // creating a zero entry.
        //
        // `add_diff` cannot record such a delta (it compares 0 against 0 and
        // skips), so the diff path never produced a `label -> 0` entry. Signed
        // accumulation can, and this keeps the two byte-identical.
        //
        // **Honest scope:** no current reader can tell the difference —
        // `count_label_nodes` is the only consumer and it does
        // `.get(&t).unwrap_or(0)`, so absent and zero answer the same. A canary
        // that removed this rule failed no test. It is kept because exact
        // equivalence is cheap insurance and because the assumption it rests on
        // is a property of the READERS, not of the counts; the test
        // `an_absent_label_count_reads_the_same_as_a_zero_one` pins that
        // assumption so a future reader that distinguishes them fails loudly
        // rather than silently.
        for (t, d) in &self.by_label {
            match st.by_label.entry(*t) {
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let v = e.get_mut();
                    *v = v.saturating_add_signed(*d);
                }
                std::collections::btree_map::Entry::Vacant(v) => {
                    if *d > 0 {
                        v.insert(*d as u64);
                    }
                }
            }
        }
        for (t, d) in &self.by_type {
            match st.by_type.entry(*t) {
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let v = e.get_mut();
                    *v = v.saturating_add_signed(*d);
                }
                std::collections::btree_map::Entry::Vacant(v) => {
                    if *d > 0 {
                        v.insert(*d as u64);
                    }
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.nodes == 0 && self.rels == 0 && self.by_label.is_empty() && self.by_type.is_empty()
    }
}

impl TxnTouched {
    fn is_empty(&self) -> bool {
        self.labels.is_empty()
            && self.props.is_empty()
            && self.adj.is_empty()
            && self.vectors.is_empty()
            && self.stats.is_empty()
    }
}

thread_local! {
    /// The write transaction active on THIS thread, if a statement (or a
    /// BEGIN..COMMIT block) is running through one. It is thread-local rather
    /// than a `Graph` field ON PURPOSE: one `Arc<Graph>` is SHARED across every
    /// session (`Graph` is `Send + Sync`), so a field would let concurrent
    /// sessions clobber each other's transaction. A single statement executes
    /// synchronously on one thread, so a thread-local scopes the transaction to
    /// exactly that execution. `None` is autocommit — the default, and the only
    /// mode the read path and every existing caller ever see.
    static ACTIVE_TXN: std::cell::RefCell<Option<engram_store::Transaction>> =
        const { std::cell::RefCell::new(None) };

    /// The touched-set of the transaction in `ACTIVE_TXN`, installed and
    /// removed with it. Kept beside the transaction rather than inside it
    /// because the store's transaction knows nothing of graph tokens.
    static TXN_TOUCHED: std::cell::RefCell<Option<TxnTouched>> =
        const { std::cell::RefCell::new(None) };

    /// Fix 75: the entity a uniqueness refusal named — `(is_rel, id)` — set
    /// by the constraint check on THIS thread and taken by the MERGE that
    /// lost its create race, so the loser binds the winner BY ID instead of
    /// re-matching through a derived structure that may not have ingested
    /// the winner's commit yet (see `Graph::settle_in_flight_writers`).
    static LAST_UNIQUE_REFUSAL: std::cell::Cell<Option<(bool, u64)>> =
        const { std::cell::Cell::new(None) };

    /// The adjacency tables this thread has resolved recently — see
    /// [`Graph::adj_snap_memo_get`].
    ///
    /// Thread-local for exactly `ACTIVE_TXN`'s reason: one `Arc<Graph>` is
    /// shared across every session, so a field would let concurrent sessions
    /// serve each other's tables and would need a lock on the hottest read path
    /// in the engine to do it.
    static ADJ_SNAP_MEMO: AdjSnapMemoSet = const { AdjSnapMemoSet::new() };
}

/// One remembered adjacency snapshot: which table it is, and which graph's.
struct AdjSnapMemo {
    graph: u64,
    tag: u8,
    tokens: Vec<u32>,
    snap: std::sync::Arc<Snapshot<AdjTable>>,
}

/// How many distinct tables a thread remembers at once.
///
/// It has to cover every `(direction, type set)` a single statement INTERLEAVES,
/// which is decided by the fold's shape and not by how many hops there are: the
/// DFS descends and backtracks, so consecutive probes come from different hops.
/// LSQB q2 alternates `REPLY_OF(O)`, `HAS_CREATOR(O)`, `KNOWS(O)`, `KNOWS(I)`
/// once per comment — four pairs, three of them competing for the same
/// direction.
///
/// Both smaller cuts of this were measured by the canary and both thrashed: one
/// entry missed on every undirected probe, and one entry PER DIRECTION still
/// missed on every chain probe. Eight covers the interleave of every query in
/// the LSQB and LDBC sets with room over; a query wider than that degrades to
/// the map walk, which is the pre-fix path and still correct.
const ADJ_SNAP_MEMO_WAYS: usize = 8;

/// A thread's remembered tables. Fully associative and scanned linearly — with
/// eight ways of `(u64, u8, short slice)` that is a handful of L1 compares,
/// against a heap allocation and a `BTreeMap` walk for the miss it replaces.
///
/// # One `RefCell` PER WAY, not one around the set
///
/// A hit runs the caller's closure while borrowing its way, and the fold
/// recurses inside that closure — hop 2's probes run under hop 1's serve under
/// hop 0's serve. With one cell around the whole set, a nested MISS could not
/// file (the outer borrow is live), and a table whose first probe only ever
/// happens nested NEVER files and misses forever. That was v62 as first built,
/// and the pod named the victims exactly: hop 2's table and both close tables,
/// `memo declined to file under an outer hit` × 3,029,508 on one q2. Per-way
/// cells let a nested miss file into a FREE way while the outer serves from
/// its own; only evicting the way an outer frame is serving is refused, and
/// with recursion depth bounded by the plan's hops (≤5) against 8 ways there
/// is always a free one.
struct AdjSnapMemoSet {
    ways: [std::cell::RefCell<Option<AdjSnapMemo>>; ADJ_SNAP_MEMO_WAYS],
    /// Round-robin victim. The working set is small and stable, so the policy
    /// costs nothing to get right and is not worth a recency field.
    next: std::cell::Cell<usize>,
}

impl AdjSnapMemoSet {
    const fn new() -> Self {
        AdjSnapMemoSet {
            ways: [const { std::cell::RefCell::new(None) }; ADJ_SNAP_MEMO_WAYS],
            next: std::cell::Cell::new(0),
        }
    }
}

/// Source of [`Graph::graph_id`]. Never reused, unlike an address.
static NEXT_GRAPH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One `count_hop` question: `(:start)-[:types]->(:end)` with `dir` as the
/// probe direction byte. Labels and types in caller order — two spellings of
/// one set cost a duplicate entry, never a wrong answer.
type HopCountKey = (Vec<String>, u8, Vec<String>, Vec<String>);

/// A memoised `count_hop` answer and the two clocks it is valid under.
///
/// TWO clocks, deliberately. The adjacency epoch moves when a relationship of
/// the counted types is written — but `count_hop` also depends on which nodes
/// CARRY the named labels, and a node can gain or lose a label with no
/// relationship write at all. An entry keyed on the adjacency epoch alone
/// serves a stale count after exactly that, which is `derived.rs`'s defect
/// class #1 — validity keyed on the wrong clock — in its subtlest form here:
/// the wrong clock is not a stale one, it is a clock that does not SEE the
/// change.
struct HopCountEntry {
    adj_epoch: u64,
    label_epoch: u64,
    count: u64,
}

/// The cap on the hop-count memo — one entry per hop SHAPE ever planned, so
/// real workloads hold a handful; the cap only stops adversarial growth.
const HOP_COUNT_MEMO_MAX: usize = 512;

/// The estimator's default sample budget. A labelled side above this is
/// stride-sampled to about this many probes; ~4K keeps a first-call estimate
/// over a 2M-member label near a millisecond with sampling error a planner
/// cannot feel — ordering decisions turn on orders of magnitude, not percents.
const ESTIMATE_SAMPLE_BUDGET: usize = 4_096;

impl Graph {
    /// Begin a write transaction on the current thread. Subsequent graph writes
    /// buffer into it — visible to this thread by read-your-writes, to no other
    /// session until [`Graph::commit_txn`]. A nested begin is refused.
    pub fn begin_txn(&self) -> Result<(), GraphError> {
        ACTIVE_TXN.with(|t| {
            let mut slot = t.borrow_mut();
            if slot.is_some() {
                return Err(GraphError::Txn("a transaction is already active".into()));
            }
            *slot = Some(self.store.begin());
            TXN_TOUCHED.with(|t| *t.borrow_mut() = Some(TxnTouched::default()));
            Ok(())
        })
    }

    /// Commit the current thread's write transaction, publishing its whole
    /// write-set atomically at one commit ts. On an OCC conflict NOTHING is
    /// published and [`GraphError::TxnConflict`] comes back for the caller to
    /// retry. Either way the derived stats cache is dropped so it rebuilds from
    /// the committed store (the eager counters may reflect a write-set that, on
    /// conflict, never published). On success the sources the transaction
    /// changed are touched AFTER the publish, stamped with the commit clock.
    pub fn commit_txn(&self) -> Result<(), GraphError> {
        let txn = ACTIVE_TXN.with(|t| t.borrow_mut().take());
        let touched = TXN_TOUCHED.with(|t| t.borrow_mut().take()).unwrap_or_default();
        let Some(txn) = txn else {
            return Err(GraphError::Txn("no active transaction to commit".into()));
        };
        // Fenced from BEFORE the publish until the entries are recorded —
        // the transaction is no longer installed, so this registers.
        let _fence = self.fence();
        let r = txn.commit();
        match r {
            Ok(ts) => {
                self.touch_after_commit(touched, ts);
                Ok(())
            }
            Err(engram_store::StoreError::Conflict) => Err(GraphError::TxnConflict),
            Err(e) => Err(GraphError::Store(e)),
        }
    }

    /// Discard the current thread's write transaction (ROLLBACK). Its writes
    /// were only buffered, never published, so nothing in the store is undone;
    /// the stats cache is dropped to shed any eager counts. A no-op (and not an
    /// error) when none is active, so a failed statement can roll back blindly.
    pub fn rollback_txn(&self) {
        ACTIVE_TXN.with(|t| t.borrow_mut().take());
        // The recorded deltas go with the buffered writes: nothing published,
        // nothing to apply.
        TXN_TOUCHED.with(|t| t.borrow_mut().take());
    }

    /// Whether a write transaction is active on this thread.
    pub fn in_txn(&self) -> bool {
        ACTIVE_TXN.with(|t| t.borrow().is_some())
    }

    // ── Session-owned transactions ──────────────────────────────────────
    //
    // A Bolt worker MULTIPLEXES many sessions on one thread, so a session's
    // transaction cannot simply live in the thread-local between messages — it
    // would leak into the next session the worker services. Instead the SESSION
    // owns a [`GraphTxn`] and installs it into the thread-local for exactly the
    // duration of each statement it runs (via [`Graph::with_txn`]), so the write
    // path sees it while that statement executes and no other session ever does.

    /// Open a detached write transaction the caller OWNS and carries between
    /// statements. It is NOT active until installed with [`Graph::with_txn`];
    /// conclude it with [`Graph::commit_owned`] or [`Graph::rollback_owned`].
    pub fn open_txn(&self) -> GraphTxn {
        let mut inner = self.store.begin();
        if self.guard_put_put_exempt.get() {
            // The store does not know what a key MEANS, so the class is
            // declared here: the guard row family, and nothing else.
            inner.set_exempt_put_put(std::sync::Arc::new(|key: &[u8]| {
                key.get(engram_key::PREFIX_LEN) == Some(&b'G')
            }));
        }
        GraphTxn {
            inner,
            touched: TxnTouched::default(),
        }
    }

    /// Run `f` with `txn` installed as this thread's active transaction, then
    /// take the transaction back out before returning — so it is visible to the
    /// write path only while `f` runs and never leaks to another session sharing
    /// the worker thread. Returns the carried-forward transaction and `f`'s value.
    pub fn with_txn<R>(&self, txn: GraphTxn, f: impl FnOnce() -> R) -> (GraphTxn, R) {
        let GraphTxn { inner, touched } = txn;
        ACTIVE_TXN.with(|t| *t.borrow_mut() = Some(inner));
        TXN_TOUCHED.with(|t| *t.borrow_mut() = Some(touched));
        let r = f();
        let inner = ACTIVE_TXN
            .with(|t| t.borrow_mut().take())
            .expect("the transaction just installed is still present");
        let touched = TXN_TOUCHED
            .with(|t| t.borrow_mut().take())
            .unwrap_or_default();
        (GraphTxn { inner, touched }, r)
    }

    /// Commit an owned transaction, publishing its whole write-set atomically.
    /// [`GraphError::TxnConflict`] on an OCC conflict (nothing published; retry).
    /// On success the sources it changed are touched AFTER the publish.
    pub fn commit_owned(&self, txn: GraphTxn) -> Result<(), GraphError> {
        self.commit_owned_reporting(txn).map_err(|(e, _)| e)
    }

    /// As [`Graph::commit_owned`], but a conflict also reports WHAT
    /// conflicted — the retry loop's escalation decision needs the keys.
    pub fn commit_owned_reporting(
        &self,
        txn: GraphTxn,
    ) -> Result<(), (GraphError, Option<engram_store::ConflictInfo>)> {
        let GraphTxn { mut inner, touched } = txn;
        // §7 — install the predicate validator, if this transaction recorded
        // any restriction it could represent.
        //
        // Built HERE rather than incrementally during the statement: the guard
        // is read under the commit latch and must not change while it is being
        // consulted, and building it once at commit is also the only point at
        // which the full set is known.
        if !touched.restrictions.is_empty() {
            inner.set_change_guard(std::sync::Arc::new(crate::precision::PredicateGuard::new(
                self,
                touched.restrictions.clone(),
            )));
        }
        // Fenced from BEFORE the publish until the entries are recorded.
        let _fence = self.fence();
        match inner.commit_reporting() {
            Ok(ts) => {
                self.touch_after_commit(touched, ts);
                Ok(())
            }
            Err((engram_store::StoreError::Conflict, info)) => {
                Err((GraphError::TxnConflict, info))
            }
            Err((e, _)) => Err((GraphError::Store(e), None)),
        }
    }

    /// Discard an owned transaction (ROLLBACK) — its buffered writes never
    /// published, so nothing in the store is undone; the stats cache is dropped.
    pub fn rollback_owned(&self, txn: GraphTxn) {
        drop(txn); // its buffered writes and recorded deltas go together
    }

    /// A point read that respects an active transaction (read-your-writes),
    /// else reads the committed store. Used by the WRITE path, whose reads
    /// (a relationship's endpoints, a record about to be updated) must see this
    /// transaction's own not-yet-committed writes.
    fn store_get_w(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<Vec<u8>> {
        ACTIVE_TXN.with(|t| match t.borrow_mut().as_mut() {
            Some(txn) => txn.get(prefix, body),
            None => self.store.get(prefix, body),
        })
    }

    /// A read for the GENERAL accessors (`node`, `rel`): it overlays an active
    /// transaction's OWN buffered writes (so a just-created entity materialises
    /// inside its transaction — read-your-writes) but does NOT record a read, so
    /// a MATCHed committed entity stays read-committed and cannot spuriously
    /// abort the transaction. Outside a transaction it is exactly `store.get`.
    fn store_get_peek(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<Vec<u8>> {
        ACTIVE_TXN.with(|t| {
            if let Some(txn) = t.borrow_mut().as_mut() {
                if let Some(buffered) = txn.peek(prefix, body) {
                    return buffered;
                }
                // Served by the committed store — and RECORDED, so that if
                // this read feeds a write (`SET n.x = n.x + 1` reads `n`
                // when MATCH materialises it) and the entity moves before
                // the commit, the commit aborts and the statement re-runs.
                // A transaction that never writes never validates, so a
                // read-only statement pays nothing for the record.
                txn.note_read(prefix, body);
            }
            self.store.get(prefix, body)
        })
    }

    /// [`Graph::store_get_peek`] WITHOUT recording the read.
    ///
    /// For materialising a CANDIDATE that may yet be rejected. See
    /// `set_read_set_bindings_only` for when that is sound and when it is not.
    fn store_get_peek_unrecorded(&self, prefix: &KeyPrefix, body: &[u8]) -> Option<Vec<u8>> {
        ACTIVE_TXN.with(|t| {
            if let Some(txn) = t.borrow_mut().as_mut() {
                if let Some(buffered) = txn.peek(prefix, body) {
                    return buffered;
                }
            }
            self.store.get(prefix, body)
        })
    }

    /// Record that a NODE became a binding — the read-set entry
    /// `store_get_peek` would have made when the node was materialised.
    pub(crate) fn note_node_read(&self, id: u64) {
        self.store_note_read(&self.nodes, &id.to_be_bytes());
    }

    /// Register a key in the active transaction's read set WITHOUT reading it.
    ///
    /// For a key whose value we already resolved elsewhere — a cache hit — but
    /// whose OCC visibility must be identical to having read it. Validation
    /// only ever asks "did this key move since my snapshot", never "what did
    /// it say", so a registered key and a read key produce the same verdict
    /// and the same abort set; the probe was buying nothing but its own cost.
    ///
    /// `ACTIVE_TXN` is private to this module, so callers in sibling modules
    /// (`schema.rs`) reach the read set through here rather than through a
    /// widened thread-local.
    /// The encoded `index` prefix — the literal byte prefix every index row's
    /// LOGICAL key starts with (`encode_key` writes the prefix, then the body,
    /// then the ts, and a logical key is that minus the ts).
    ///
    /// This is how a merge observer, which is handed whole logical keys and
    /// knows nothing of realms or namespaces, tells THIS graph's rows from
    /// another's in the same merged run.
    /// The `nodes` key prefix — §7's guard re-reads changed node records
    /// through it.
    pub(crate) fn nodes_prefix(&self) -> KeyPrefix {
        self.nodes
    }

    pub(crate) fn index_prefix_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(engram_key::PREFIX_LEN);
        self.index.encode_into(&mut out);
        out
    }

    pub(crate) fn store_note_read(&self, prefix: &KeyPrefix, body: &[u8]) {
        ACTIVE_TXN.with(|t| {
            if let Some(txn) = t.borrow_mut().as_mut() {
                txn.note_read(prefix, body);
            }
        });
    }

    /// Whether a transaction with BUFFERED WRITES is active on this thread.
    /// The read paths overlay those writes over the committed store so a
    /// statement sees its own earlier clauses' effects; with nothing
    /// buffered there is nothing to overlay, and every read path takes its
    /// ordinary shape.
    pub(crate) fn in_txn_with_writes(&self) -> bool {
        ACTIVE_TXN.with(|t| t.borrow().as_ref().is_some_and(|x| x.has_writes()))
    }

    /// The active transaction's buffered writes under `prefix` starting with
    /// `body_prefix` — `None` when there is no transaction or it has written
    /// nothing, which is the common case and costs one thread-local read.
    fn txn_pending(&self, prefix: &KeyPrefix, body_prefix: &[u8]) -> Option<PendingRows> {
        ACTIVE_TXN.with(|t| {
            t.borrow()
                .as_ref()
                .filter(|x| x.has_writes())
                .map(|x| x.pending_body_prefix_present(prefix, body_prefix))
        })
    }

    /// As [`Graph::txn_pending`], WITH the buffered values — for the one
    /// consumer that must materialise the buffered rows themselves (the
    /// relationship scan), not merely know which keys moved.
    fn txn_pending_values(
        &self,
        prefix: &KeyPrefix,
        body_prefix: &[u8],
    ) -> Option<PendingRowValues> {
        ACTIVE_TXN.with(|t| {
            t.borrow()
                .as_ref()
                .filter(|x| x.has_writes())
                .map(|x| x.pending_body_prefix(prefix, body_prefix))
        })
    }

    /// The index rows under `body_prefix`, as the ACTIVE TRANSACTION sees
    /// them: the committed rows with this transaction's buffered puts added
    /// and its buffered deletes removed, in key order. The read every
    /// adjacency, membership and count path that answers a query directly
    /// goes through; the paths that BUILD a shared structure keep reading the
    /// committed store, because a private write must never enter a shared
    /// snapshot.
    fn index_bodies(&self, body_prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut out = self.store.scan_bodies_prefix(&self.index, body_prefix);
        if let Some(pending) = self.txn_pending(&self.index, body_prefix) {
            if !pending.is_empty() {
                counted!("graph.scan overlaid a transaction's writes");
            }
            for (body, is_put) in pending {
                match (is_put, out.binary_search(&body)) {
                    (true, Err(at)) => out.insert(at, body),
                    (false, Ok(at)) => {
                        out.remove(at);
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// The net change this transaction's buffered writes make to the number
    /// of live rows under `prefix`/`body_prefix`: a put of a row the store
    /// does not hold is a creation, a delete of one it holds is a removal.
    /// Zero, at no cost, when nothing is buffered.
    fn txn_row_delta(&self, prefix: &KeyPrefix, body_prefix: &[u8]) -> i64 {
        let Some(pending) = self.txn_pending(prefix, body_prefix) else {
            return 0;
        };
        let mut delta = 0i64;
        for (body, is_put) in pending {
            let exists = self.store.get(prefix, &body).is_some();
            match (is_put, exists) {
                (true, false) => delta += 1,
                (false, true) => delta -= 1,
                _ => {}
            }
        }
        delta
    }

    /// A delete that buffers into an active transaction, else autocommits.
    /// Returns the tombstone's commit ts — `0` when buffered (no ts exists
    /// yet; the commit stamps the replayed entries).
    fn store_delete_w(&self, prefix: &KeyPrefix, body: &[u8]) -> u64 {
        ACTIVE_TXN.with(|t| match t.borrow_mut().as_mut() {
            Some(txn) => {
                txn.delete(prefix, body);
                0
            }
            None => self.store.delete(prefix, body),
        })
    }

    // ── The write fence ─────────────────────────────────────────────────
    //
    // See `derived.rs`, "The write fence": a writer stamps its change-log
    // entries with the commit ts of its rows, and a snapshot publisher must
    // never stamp itself at or past an entry a still in-flight writer will
    // record. The registry below is how a publisher knows the lowest stamp
    // any in-flight writer can still produce.

    /// Register this thread as an in-flight writer for the guard's lifetime.
    ///
    /// Taken at the TOP of every direct write method (before its first row
    /// write) and around a transaction's commit; released by drop, after the
    /// method has recorded its entries. Inside an installed transaction a
    /// write buffers and records nothing, so nothing is registered — the
    /// commit registers once for the whole write-set.
    fn fence(&self) -> WriteFence<'_> {
        if self.in_txn() {
            return WriteFence { graph: self, at: None };
        }
        // The VISIBLE clock: every row this writer will commit is allocated
        // after this read, so it is stamped strictly above `at`.
        let at = self.store.now_ts();
        *self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(at)
            .or_insert(0) += 1;
        WriteFence {
            graph: self,
            at: Some(at),
        }
    }

    /// Clamp a publish stamp below every in-flight writer: the lowest
    /// registered clock, if any, else `at` itself. Read in the SAME critical
    /// section as the log entries a catch-up applies (a writer that recorded
    /// after that section is either registered or observed a clock at or
    /// past the epoch), and AFTER the walk a build made (a writer registering
    /// after this read observed a clock at or past the walk's).
    fn fenced(&self, at: u64) -> u64 {
        let g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        match g.keys().next() {
            Some(&low) if low < at => {
                counted!("graph.publish stamp fenced below an in-flight writer");
                low
            }
            _ => at,
        }
    }

    /// The fail-closed response to a POISONED label log (see
    /// `ChangeLog::record`): retract the token's snapshot so the next reader
    /// rebuilds it from the store.
    fn retract_members(&self, token: u32) {
        counters::DERIVED_LOG_POISONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(slot) = self.members_cache.load().get(&token) {
            slot.retract();
        }
    }

    /// As [`Graph::retract_members`], for every cached adjacency table of
    /// `tag` that covers `type_token` — the typed tables naming it and the
    /// untyped one.
    fn retract_adj_tables(&self, tag: u8, type_token: u32) {
        counters::DERIVED_LOG_POISONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for ((t, types), slot) in self.adj_tables.load().iter() {
            if *t == tag && (types.is_empty() || types.binary_search(&type_token).is_ok()) {
                slot.retract();
            }
        }
    }

    /// As [`Graph::retract_members`], for a property's range index.
    ///
    /// EVERY index over the property, scoped and unscoped. The property log is
    /// per-property, so a poisoned log says nothing about which label's index
    /// is affected — retracting only one would leave the others trusting a log
    /// that has already admitted it lost entries.
    fn retract_range_index(&self, token: u32) {
        counters::DERIVED_LOG_POISONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for (key, slot) in self.range_cache.load().iter() {
            if (*key & 0xFFFF_FFFF) as u32 == token {
                slot.retract();
            }
        }
    }

    /// Toggle the per-entity write latches (default on) — the A/B arm and the
    /// canary's. See `entity_latches`.
    pub fn set_entity_latching(&self, on: bool) {
        self.entity_latching.set(on);
    }

    /// Toggle conflict escalation (default on): whether the adapter's retry
    /// loop, after an OCC conflict on a key the statement itself WRITES, may
    /// queue on the store's FIFO entity lock for the re-run instead of
    /// re-running optimistically. Advisory — OCC validation stays the sole
    /// correctness authority; this only orders the losers.
    pub fn set_conflict_escalation(&self, on: bool) {
        self.conflict_escalation.set(on);
    }

    /// See [`Graph::set_conflict_escalation`].
    pub fn conflict_escalation_enabled(&self) -> bool {
        self.conflict_escalation.get()
    }

    /// The escalation lane's lock acquisition — a passthrough to the store's
    /// synchronous FIFO entity lock. Callers acquire in SORTED key order.
    pub fn lock_conflict_key(&self, key: Vec<u8>) -> engram_store::LockGuard {
        self.store.lock_key_sync(key)
    }

    /// Install (or remove) the morsel executor — see `scoped_exec`. The
    /// server installs its thread-scope pool here behind
    /// `ENGRAM_QUERY_PARALLELISM`; tests install `SerialExec` or their own.
    pub fn set_exec(&self, e: Option<std::sync::Arc<dyn ScopedExec>>) {
        *self.exec.borrow_mut() = e;
    }

    /// The installed morsel executor, if any.
    pub(crate) fn exec(&self) -> Option<std::sync::Arc<dyn ScopedExec>> {
        self.exec.borrow().clone()
    }

    /// The fewest driving rows worth splitting across morsels (default 256).
    pub fn set_parallel_min_rows(&self, n: usize) {
        self.parallel_min_rows.set(n.max(2));
    }

    /// See [`Graph::set_parallel_min_rows`].
    pub(crate) fn parallel_min_rows(&self) -> usize {
        self.parallel_min_rows.get()
    }

    /// Toggle serialisable autocommit (default on): whether an adapter
    /// should run each autocommit statement as a read-validated transaction
    /// and re-run it on conflict. The graph only carries the switch; the
    /// Bolt server reads it per statement.
    pub fn set_serialisable_autocommit(&self, on: bool) {
        self.serialisable_autocommit.set(on);
    }

    /// Whether serialisable autocommit is on.
    pub fn serialisable_autocommit_enabled(&self) -> bool {
        self.serialisable_autocommit.get()
    }

    /// The write latch for one entity — see `entity_latches`. Held by the
    /// caller across its read-modify-write of that record and nothing else.
    /// `None` when latching is switched off.
    fn entity_latch(&self, is_node: bool, id: u64) -> Option<std::sync::MutexGuard<'_, ()>> {
        if !self.entity_latching.get() {
            return None;
        }
        // Nodes take the first half of the table, relationships the second:
        // DISJOINT stripes, so a detach delete holding its node's stripe can
        // never map one of the node's relationships onto the same mutex and
        // wait on itself. (Std mutexes are not re-entrant.)
        let half = self.entity_latches.len() / 2;
        let mixed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let stripe = (mixed >> 32) as usize % half + if is_node { 0 } else { half };
        Some(
            self.entity_latches[stripe]
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// A graph over `store`, scoped to one realm and namespace.
    pub fn new(store: Store, realm: Realm, ns: Namespace) -> Graph {
        // The count store starts live over an EMPTY store (the first write
        // maintains it) and deferred over a pre-populated one (one rebuild
        // on first read). Decided before `store` moves into the struct.
        //
        // "Empty" must consider SEALED SEGMENTS, not just the log/tail: a store
        // opened from disk (`Store::open_paged_dir`) has an empty log — its data
        // lives in sealed segments, never replayed through the log — so checking
        // only the log misjudges it as fresh, leaves the count store live-empty,
        // and every label count reads 0. That makes property-seek decline and
        // anchored top-k chains fan out to a cross-product. A store with any
        // sealed segment is pre-populated.
        let fresh_store =
            store.log_len() == 0 && store.unlogged_count() == 0 && store.segment_count() == 0;
        let p = Partition(0);
        Graph {
            nodes: KeyPrefix {
                realm,
                namespace: ns,
                kind: Kind::NODE,
                partition: p,
            },
            rels: KeyPrefix {
                realm,
                namespace: ns,
                kind: Kind::EDGE,
                partition: p,
            },
            index: KeyPrefix {
                realm,
                namespace: ns,
                kind: Kind::INDEX_ENTRY,
                partition: p,
            },
            kv: KeyPrefix {
                realm,
                namespace: ns,
                kind: Kind::KV,
                partition: p,
            },
            store,
            wall_ms: OptI64Cell::new(None),
            row_budget: OptUsizeCell::new(None),
            // Range-scan-vs-point-gather crossover: a range-scan entry is ~10x
            // cheaper than a scattered point-get, so a range scan over up to ~8x
            // the member count still beats gathering members one by one. 4 was
            // too low - it point-gathered dense-label columns (IC9's creationDate
            // over ~100k of 518k messages) that a range scan loads far cheaper.
            columnar_column_budget_factor: UsizeCell::new(8),
            columnar_scans: BoolCell::new(true),
            columnar_agg_batch: BoolCell::new(true),
            columnar_agg_batch_size: SyncRefCell::new(0),
            agg_native_key: BoolCell::new(true),
            degree_aggregate: BoolCell::new(true),
            ic2_ordered: BoolCell::new(true),
            ic11_semijoin: BoolCell::new(true),
            bi7_rollup: BoolCell::new(true),
            parallel_expand: BoolCell::new(false),
            ic3_datewindow: BoolCell::new(true),
            edge_probe: BoolCell::new(true),
            hop_reversal: BoolCell::new(true),
            scope_pruning: BoolCell::new(true),
            late_projection: BoolCell::new(true),
            frontier_expand: BoolCell::new(true),
            property_seek: BoolCell::new(true),
            chain_count_fold: BoolCell::new(true),
            lean_subquery_seed: BoolCell::new(true),
            pattern_map_seek: BoolCell::new(true),
            detach_via_rel_ids: BoolCell::new(true),
            label_epoch_atomics: BoolCell::new(true),
            volatile_guards: BoolCell::new(true),
            label_scoped_indexes: BoolCell::new(true),
            read_set_bindings_only: BoolCell::new(false),
            stats_delta: BoolCell::new(true),
            compaction_csr: BoolCell::new(true),
            demote_adj_rebuild: BoolCell::new(true),
            reader_rebuild_admission: BoolCell::new(true),
            single_flight_repair: BoolCell::new(false),
            lazy_stale_serve: BoolCell::new(true),
            adj_change_filter_on: BoolCell::new(true),
            adj_overlay_fold: UsizeCell::new(ADJ_OVERLAY_FOLD),
            hop_membership_contains: BoolCell::new(true),
            members_bitmap_after: UsizeCell::new(MEMBERS_BITMAP_AFTER),
            single_node_stale_walk: BoolCell::new(true),
            persist_derived: BoolCell::new(true),
            persisted_vintage: std::sync::atomic::AtomicU64::new(0),
            persisted_sealed_id: std::sync::atomic::AtomicU64::new(0),
            persisted_at_secs: std::sync::atomic::AtomicU64::new(0),
            last_tick_secs: std::sync::atomic::AtomicU64::new(0),
            persist_growth_interval_secs: std::sync::atomic::AtomicU64::new(600),
            precision_locking: BoolCell::new(false),
            multistage_topk_batch: BoolCell::new(true),
            degree_tables: arc_swap::ArcSwap::from_pointee(DegreeTables::new()),
            degree_probes: ProbeGate::new(),
            degree_table_after: U64Cell::new(DEGREE_TABLE_AFTER),
            stats: SyncRefCell::new(if fresh_store {
                Some(Stats::default()) // empty store: maintain from the first write
            } else {
                None // pre-populated: one rebuild on first read
            }),
            constraint_cache: SyncRefCell::new(None),
            range_index_cache: SyncRefCell::new(None),
            conflict_escalation: BoolCell::new(true),
            exec: SyncRefCell::new(None),
            parallel_min_rows: UsizeCell::new(256),
            adj_tables: arc_swap::ArcSwap::from_pointee(AdjTables::new()),
            adj_snap_memo: BoolCell::new(true),
            agg_topk_before_project: BoolCell::new(true),
            const_projection_fold: BoolCell::new(true),
            merge_race_hook: std::sync::RwLock::new(None),
            checkpoint_hook: std::sync::RwLock::new(None),
            directed_bound_probe: BoolCell::new(true),
            parallel_fold: BoolCell::new(false),
            hop_count_memo: SyncRefCell::new(BTreeMap::new()),
            hop_count_memo_on: BoolCell::new(true),
            hop_estimate_memo: SyncRefCell::new(BTreeMap::new()),
            estimate_sample_budget: UsizeCell::new(ESTIMATE_SAMPLE_BUDGET),
            graph_id: NEXT_GRAPH_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            rel_members_cache: arc_swap::ArcSwap::from_pointee(RelMembersCache::new()),
            adj_table_max_entries: UsizeCell::new(ADJ_TABLE_MAX_ENTRIES),
            adj_cache: SyncRefCell::new(AdjCache::new()),
            bfs_memo: SyncRefCell::new(BfsMemo::new()),
            end_set_memo: SyncRefCell::new(Vec::new()),
            members_cache: arc_swap::ArcSwap::from_pointee(MembersCache::new()),
            label_log: Default::default(),
            prop_log: Default::default(),
            adj_epoch: std::sync::atomic::AtomicU64::new(0),
            adj_type_epoch: arc_swap::ArcSwap::from_pointee(BTreeMap::new()),
            label_epoch_map: arc_swap::ArcSwap::from_pointee(BTreeMap::new()),
            adj_log: Default::default(),
            adj_change_filter: Default::default(),
            inflight: std::sync::Mutex::new(BTreeMap::new()),
            selective_anchor: BoolCell::new(true),
            incremental_caches: BoolCell::new(true),
            adj_cost_repair: BoolCell::new(true),
            guard_put_put_exempt: BoolCell::new(true),
            constraint_epoch_cache: BoolCell::new(true),
            id_reservation: UsizeCell::new(SERVING_ID_RANGE),
            // 250k rows: above a healthy write burst's changed set (SF1's
            // write-only levels move ~40-47k rows) so the common case still
            // catches up in ONE pass, and far below a full-table walk
            // (17.26M rows for the untyped span), which is the case that
            // stalled the writers.
            refresh_pass_rows: UsizeCell::new(250_000),
            members_batch_fold: BoolCell::new(true),
            scan_resistant_rebuild: BoolCell::new(true),
            bulk_ingest: BoolCell::new(false),
            id_reservations: SyncRefCell::new(BTreeMap::new()),
            range_cache: Default::default(),
            prop_columns: std::sync::Mutex::new(PropColumnCache::new(PROP_COLUMN_BUDGET_BYTES)),
            ann_cache: Default::default(),
            creator_msgs: Default::default(),
            vector_deltas: Default::default(),
            vector_index_list: Default::default(),
            zone_provider: SyncRefCell::new(None),
            vector_exact_max: UsizeCell::new(2048),
            alloc: std::sync::Mutex::new(()),
            entity_latches: (0..ENTITY_LATCH_STRIPES)
                .map(|_| std::sync::Mutex::new(()))
                .collect(),
            entity_latching: BoolCell::new(true),
            serialisable_autocommit: BoolCell::new(true),
            labels: Default::default(),
            types: Default::default(),
            props: Default::default(),
            rev_names: Default::default(),
        }
    }

    /// The store's current timestamp — the interpreter stamps results with it.
    pub fn now_ts(&self) -> u64 {
        self.store.now_ts()
    }

    /// The date-ordered per-creator message index at the current epoch, if a
    /// build for this epoch is cached.
    pub(crate) fn creator_msgs_get(&self) -> Option<std::sync::Arc<CreatorMsgs>> {
        if self.in_txn_with_writes() {
            return None; // committed state: the ordinary path sees the overlay
        }
        let epoch = self.now_ts();
        let cache = self.creator_msgs.borrow();
        cache
            .as_ref()
            .and_then(|(at, m)| (*at == epoch).then(|| std::sync::Arc::clone(m)))
    }

    /// Cache the date-ordered per-creator message index at the current epoch.
    pub(crate) fn creator_msgs_set(&self, m: std::sync::Arc<CreatorMsgs>) {
        if self.in_txn_with_writes() {
            return; // built over a private view: never shared
        }
        let epoch = self.now_ts();
        *self.creator_msgs.borrow_mut() = Some((epoch, m));
    }

    /// Inject the wall clock (epoch milliseconds). Un-set, `datetime()` and
    /// `timestamp()` refuse by name rather than reading an ambient clock.
    /// Equality probe against a derived range index over the node
    /// partition — the seed a `(n:L {prop: $x})` point lookup wants.
    ///
    /// `None` means the index cannot SERVE this probe (non-scalar value,
    /// unknown property): the caller falls back to a scan. `Some(ids)` is a
    /// CANDIDATE set, never an oracle: every id is re-verified against the
    /// pattern afterwards, which is what makes the index's typed buckets
    /// safe against Cypher's cross-type numeric equality — a probe for
    /// `Int(2)` unions the `Float(2.0)` bucket and vice versa, and the
    /// verifier keeps only what `eq3` accepts.
    ///
    /// The index is built on first use and cached against the store's
    /// commit clock, exactly like the ANN cache: any write invalidates,
    /// a warm probe costs microseconds, and which happened is countable.
    pub fn index_probe_eq(
        &self,
        prop: &str,
        value: &Value,
        cap: Option<usize>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        self.index_probe_eq_scoped(prop, value, cap, None)
    }

    /// [`Graph::index_probe_eq`], optionally against a LABEL-SCOPED index.
    ///
    /// The label must be one the pattern REQUIRES: a scoped index holds only
    /// that label's members, so probing it for a pattern that does not require
    /// the label would return a subset, not a superset — and the candidate set
    /// must always be a superset, because `node_satisfies` can only remove rows.
    pub fn index_probe_eq_scoped(
        &self,
        prop: &str,
        value: &Value,
        cap: Option<usize>,
        label: Option<&str>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        use engram_store::IndexKey;
        let probes: Vec<IndexKey> = match value {
            Value::Int(i) => {
                let mut v = vec![IndexKey::Int(*i)];
                v.push(IndexKey::Float(*i as f64));
                v
            }
            Value::Float(f) => {
                let mut v = vec![IndexKey::Float(*f)];
                if f.fract() == 0.0 && f.abs() < 9.0e18 {
                    v.push(IndexKey::Int(*f as i64));
                }
                v
            }
            Value::Str(t) => vec![IndexKey::Str(t.as_bytes().to_vec())],
            _ => return Ok(None), // not index-servable — scan instead
        };
        let Some(idx) = self.ensure_range_index_scoped(prop, label) else {
            return Ok(Some(Vec::new())); // property never minted: nothing can match
        };
        let mut ids = Self::probe_ids(&idx, &probes, cap);
        // Inside a transaction with buffered writes, every node it has
        // written is a CANDIDATE too: the shared index is committed state
        // and cannot know the transaction's values, and the caller verifies
        // every candidate against the record it can see — so a superset is
        // correct and a private write is never missed.
        if let (Some(ids), Some(pending)) = (ids.as_mut(), self.txn_pending(&self.nodes, &[])) {
            let mut extra = 0usize;
            for (body, is_put) in pending {
                if !is_put {
                    continue;
                }
                if let Ok(b) = <[u8; 8]>::try_from(body.as_slice()) {
                    let id = u64::from_be_bytes(b);
                    if !ids.contains(&id) {
                        ids.push(id);
                        extra += 1;
                    }
                }
            }
            if extra > 0 {
                counted!("graph.index probe overlaid a transaction's writes");
            }
        }
        Ok(ids)
    }

    /// The ids whose `prop` STARTS WITH `prefix`, from the range index —
    /// `[prefix, next(prefix))` where `next` bumps the prefix's last byte
    /// below 0xFF (a prefix of nothing but 0xFF bytes declines). Scoped to
    /// `label`'s declared index when given, as [`Graph::index_probe_eq_scoped`];
    /// `None` when over `cap`, or when the property was never minted. The
    /// ids are a CANDIDATE set — the caller re-checks the predicate.
    /// `g.eventId STARTS WITH 'edgar-8k-'` walked 44k events per statement
    /// (11 ms against Neo4j's index prefix seek, 1.7) for want of this.
    pub fn index_probe_prefix_scoped(
        &self,
        prop: &str,
        prefix: &str,
        cap: Option<usize>,
        label: Option<&str>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        use engram_store::IndexKey;
        let lo = prefix.as_bytes().to_vec();
        let mut hi = lo.clone();
        loop {
            match hi.pop() {
                Some(b) if b < 0xFF => {
                    hi.push(b + 1);
                    break;
                }
                Some(_) => continue,
                None => return Ok(None), // every byte 0xFF, or empty: no bounded range
            }
        }
        let Some(idx) = self.ensure_range_index_scoped(prop, label) else {
            return Ok(Some(Vec::new()));
        };
        let (lo, hi) = (IndexKey::Str(lo), IndexKey::Str(hi));
        if let Some(c) = cap {
            if idx.range_count(&lo, &hi) > c {
                return Ok(None);
            }
        }
        let mut ids: Vec<u64> = idx
            .range(&lo, &hi)
            .bodies
            .iter()
            .filter_map(|body| <[u8; 8]>::try_from(body.as_slice()).ok())
            .map(u64::from_be_bytes)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        counted!("graph.index prefix probes");
        Ok(Some(ids))
    }

    /// Fix 47: the ids whose STRING `prop` satisfies `prop <op> value` for a
    /// comparison operator, from the scoped range index — the range
    /// `[v++0, MAX)` for `>`, `[v, MAX)` for `>=`, `[MIN, v)` for `<` and
    /// `[MIN, v++0)` for `<=` in the index's bytewise key order, where
    /// `v ++ 0x00` is the smallest string above `v`, MIN the empty string and
    /// MAX a single 0xFF byte (no UTF-8 string carries one, so every string
    /// sorts below it). Exact for string keys: a non-string member is
    /// outside every string range, as the comparison answers null for it.
    /// `None` for a non-string value (the walk judges it), or over `cap`.
    pub fn index_probe_range_scoped(
        &self,
        prop: &str,
        op: engram_cypher::BinOp,
        value: &Value,
        cap: Option<usize>,
        label: Option<&str>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        use engram_cypher::BinOp;
        use engram_store::IndexKey;
        let Value::Str(v) = value else {
            return Ok(None);
        };
        let exact = v.as_bytes().to_vec();
        let mut above = exact.clone();
        above.push(0);
        let min = IndexKey::Str(Vec::new());
        let max = IndexKey::Str(vec![0xFF]);
        let (lo, hi) = match op {
            BinOp::Gt => (IndexKey::Str(above), max),
            BinOp::Ge => (IndexKey::Str(exact), max),
            BinOp::Lt => (min, IndexKey::Str(exact)),
            BinOp::Le => (min, IndexKey::Str(above)),
            _ => return Ok(None),
        };
        let Some(idx) = self.ensure_range_index_scoped(prop, label) else {
            return Ok(Some(Vec::new()));
        };
        if let Some(c) = cap {
            if idx.range_count(&lo, &hi) > c {
                return Ok(None);
            }
        }
        let mut ids: Vec<u64> = idx
            .range(&lo, &hi)
            .bodies
            .iter()
            .filter_map(|body| <[u8; 8]>::try_from(body.as_slice()).ok())
            .map(u64::from_be_bytes)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        counted!("graph.index range probes");
        Ok(Some(ids))
    }

    /// The range index for `prop`, current at the property's epoch. `None`
    /// when the property was never minted. This is the single build+cache
    /// point both the equality seek ([`Graph::index_probe_eq`]) and the
    /// index-ordered top-k share.
    ///
    /// The reader protocol of `derived.rs`: a lock-free hit when the snapshot
    /// is at or past the property's epoch; otherwise the entries the log holds
    /// since the snapshot are applied in O(delta) and the result published at
    /// the log's epoch, fenced; otherwise one worker rebuilds while the rest
    /// wait for its publish — and catch THAT up from the log when its fenced
    /// stamp is below their epoch, rather than scanning again.
    pub(crate) fn ensure_range_index(
        &self,
        prop: &str,
    ) -> Option<std::sync::Arc<engram_store::RangeIndex>> {
        self.ensure_range_index_scoped(prop, None)
    }

    /// Arm the MERGE race window once: the hook runs inside the next MERGE
    /// that finds nothing, after the match and before the create, as if
    /// another writer committed there. A hook that creates the merged value
    /// makes that MERGE's create meet the uniqueness marker and converge
    /// through the re-match — the state the sim sweep must reach and cannot
    /// race real threads for while staying deterministic. Consumed on first
    /// use, so a hook that itself runs statements cannot recurse.
    pub fn set_merge_race_hook_for_test(&self, hook: Option<MergeRaceHook>) {
        *self.merge_race_hook.write().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    /// See [`Graph::set_merge_race_hook_for_test`].
    pub(crate) fn take_merge_race_hook(&self) -> Option<MergeRaceHook> {
        self.merge_race_hook.write().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Fix 75: record the entity a uniqueness refusal named (the constraint
    /// check calls this right before it returns the violation).
    pub(crate) fn note_unique_refusal(is_rel: bool, id: u64) {
        LAST_UNIQUE_REFUSAL.with(|c| c.set(Some((is_rel, id))));
    }

    /// Fix 75: take (and clear) the entity the last uniqueness refusal on
    /// this thread named.
    pub(crate) fn take_unique_refusal(&self) -> Option<(bool, u64)> {
        LAST_UNIQUE_REFUSAL.with(std::cell::Cell::take)
    }

    /// Fix 75: wait, bounded, until no writer is between its publish and
    /// the recording of its log entries.
    ///
    /// A commit publishes its rows and THEN records the change-log entries
    /// that advance the property and label epochs (`touch_after_commit`),
    /// holding the write fence across both. A reader in that window sees
    /// the rows through a record read but judges its cached index "still
    /// current" (`snap.at >= prop_epoch`) and misses them. A MERGE that lost
    /// its create race is exactly that reader: its create met the winner's
    /// uniqueness marker (a record read proved the winner live), and its
    /// re-match through the scoped index found nothing — so the violation
    /// surfaced as genuine, about 1–3 % of the racing-merge test's runs.
    /// Every in-flight writer drops its fence within microseconds of
    /// publishing; the bound is a guard against a stalled one, not a wait
    /// anyone is expected to hit.
    pub(crate) fn settle_in_flight_writers(&self) {
        let mut waited = false;
        for _ in 0..SETTLE_SPINS {
            let busy = !self
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty();
            if !busy {
                return;
            }
            if !waited {
                counted!("graph.merge settled behind an in-flight writer");
                waited = true;
            }
            std::thread::yield_now();
        }
    }

    /// Install what `CALL engram.checkpoint()` runs: seal the unsealed tail
    /// and spill every sealed segment to the paged directory, answering with
    /// what is on disk afterwards.
    ///
    /// # Why a statement, not a signal
    ///
    /// A paged store's durability is at seal boundaries — the unsealed tail,
    /// and any sealed segment not yet spilled, are lost when the process
    /// dies. The maintenance thread seals a QUIESCENT tail on its own tick,
    /// but it is the same thread that runs compaction, and at SF3 a
    /// compaction is 150–180 s: a loader that finished its last statement,
    /// slept 25 s and killed the server lost 742,615 relationships the first
    /// time and 92,910 the second, in both cases the LAST groups sent, with
    /// the server's own log saying so in a line nobody was reading. A
    /// SIGTERM is the same crash from the store's point of view, so a pod
    /// restart would lose its tail the same way. A statement is what a
    /// client can await and what a `preStop` hook can run: the reply means
    /// "on disk", and `tail == 0 && resident == 0` is the whole claim.
    ///
    /// Installed by the server at graph construction, so an embedded graph
    /// or a resident (WAL-backed) server refuses the call instead of
    /// answering "durable" about a store whose durability is elsewhere.
    pub fn set_checkpoint_hook(&self, hook: Option<CheckpointHook>) {
        *self.checkpoint_hook.write().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    /// See [`Graph::set_checkpoint_hook`].
    pub(crate) fn checkpoint_hook(&self) -> Option<CheckpointHook> {
        self.checkpoint_hook.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The prefix under which relationship RECORDS live — for a test that
    /// removes one through the raw store to leave its adjacency rows
    /// orphaned, the state `rels_of`'s silent drop and DETACH DELETE's skip
    /// both handle and the FSCK reports.
    pub fn rel_prefix_for_test(&self) -> KeyPrefix {
        self.rels
    }

    /// The PUBLISHED adjacency table for one side and type set, taken apart —
    /// exposed for tests that must assert a table's LAYOUT rather than its
    /// answers.
    ///
    /// `None` when nothing is published, and it never builds one: a
    /// differential over how a table was PRODUCED must not be able to trigger
    /// the production it is measuring the absence of.
    ///
    /// The bar for §5.2 is byte-identical `offsets` and `entries`, not merely
    /// equal traversal results. Two tables can answer every query alike and
    /// still differ in layout, and the layout is what the NEXT repair builds
    /// on — so a differential that only compares answers passes on a table
    /// whose successor will be wrong.
    pub fn adj_table_parts_for_test(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
    ) -> Option<AdjTableParts> {
        let key = (tag, type_tokens.clone().unwrap_or_default());
        let map = self.adj_tables.load();
        let snap = map.get(&key)?.load()?;
        Some(AdjTableParts {
            offsets: snap.value.index.to_dense(),
            entries: (*snap.value.entries).clone(),
            sorted_by_peer: snap.value.sorted_by_peer,
            overlay: snap
                .value
                .overlay
                .iter()
                .map(|(n, row)| (*n, row.to_vec()))
                .collect(),
            at: snap.at,
        })
    }

    /// The stamp a label's membership snapshot is published at, and its ids —
    /// `None` if nothing is published. Never builds, for
    /// [`Graph::adj_table_parts_for_test`]'s reason.
    pub fn members_snapshot_for_test(&self, label: &str) -> Option<(u64, Vec<u64>)> {
        let token = self.token_peek("lbl:", &self.labels, label)?;
        let map = self.members_cache.load();
        let snap = map.get(&token)?.load()?;
        Some((snap.at, (*snap.value.to_arc_vec()).clone()))
    }

    /// `Graph::ensure_range_index_scoped`, exposed for tests that need to
    /// assert an index's SIZE rather than its answers — the difference between
    /// a scoped and an unscoped index is invisible from the answers alone,
    /// which is exactly why it went unnoticed.
    pub fn ensure_range_index_for_test(
        &self,
        prop: &str,
        label: Option<&str>,
    ) -> Option<std::sync::Arc<engram_store::RangeIndex>> {
        self.ensure_range_index_scoped(prop, label)
    }

    /// [`Graph::ensure_range_index`], optionally scoped to a LABEL.
    ///
    /// A scoped index covers only that label's members, which is what
    /// `CREATE INDEX ... FOR (n:L) ON (n.p)` declares. Unscoped (`None`) keeps
    /// the partition-wide index — what an undeclared probe still gets, since
    /// nobody told us which label to scope it to.
    ///
    /// Staleness is still judged by the PROPERTY's epoch: any write to `p`
    /// makes every index over `p` stale, scoped or not. That is conservative
    /// (a write to another label's node invalidates this one) and correct;
    /// judging it per label would need a per-(label, property) log, which is a
    /// second bookkeeping structure for a second-order saving.
    pub(crate) fn ensure_range_index_scoped(
        &self,
        prop: &str,
        label: Option<&str>,
    ) -> Option<std::sync::Arc<engram_store::RangeIndex>> {
        let token = self.token_peek("prop:", &self.props, prop)?;
        let label_token = match label {
            Some(l) if self.label_scoped_indexes.get() => {
                // A label never minted has no members, so a scoped index over
                // it would be empty — but so would the answer, and falling back
                // to the partition-wide index is the safe (merely slower)
                // choice rather than inventing an empty index.
                self.token_peek("lbl:", &self.labels, l)
            }
            _ => None,
        };
        let key = range_key(label_token, token);
        let slot = slot_in(&self.range_cache, &key, RANGE_CACHE_MAX);
        let incremental = self.incremental_caches.get();
        // The snapshot a catch-up was already attempted on and declined —
        // see `adj_table_snapshot_reporting`: not retried behind the build
        // guard, but a DIFFERENT snapshot there is the winner's fenced
        // publish and is caught up rather than rebuilt.
        let mut tried: Option<std::sync::Arc<Snapshot<engram_store::RangeIndex>>> = None;
        if let Some(snap) = slot.load() {
            if snap.at >= self.store.now_ts() {
                // Counted, because this is the path that SHOULD dominate —
                // an index serving a read with no work at all. It went
                // uncounted until the differential test needed to prove the
                // index had been consulted and could not tell this apart
                // from the planner never asking.
                counted!("graph.range index cache hit");
                return Some(std::sync::Arc::clone(&snap.value));
            }
            if incremental {
                // The clock moved, but an index covers ONE property: it is
                // only stale if THAT property was written since it was built.
                if snap.at >= self.prop_epoch(token) {
                    counted!("graph.range index still current");
                    return Some(std::sync::Arc::clone(&snap.value));
                }
                // Genuinely stale — but the rows that made it stale were
                // logged, so it is carried forward rather than rebuilt from a
                // partition scan.
                if let Some(next) = self.range_index_caught_up(token, label, &slot, &snap) {
                    return Some(next);
                }
                // The log no longer reaches this snapshot: another worker has
                // published past it, or the log overflowed. Reload before
                // paying for a build.
                if let Some(snap) = slot.load() {
                    if snap.at >= self.prop_epoch(token) {
                        counted!("graph.range index republished");
                        return Some(std::sync::Arc::clone(&snap.value));
                    }
                }
                tried = Some(snap);
            }
        }
        // Build, single-flight PER SLOT. The log is created BEFORE the scan so
        // that no write can land between the scan and the log; `at` is read
        // before the scan so that every change stamped at or below it has
        // committed rows the scan sees. A change stamped after `at` that the
        // scan also saw is simply re-applied — idempotent, last write wins.
        let _build = slot.enter_build();
        if let Some(snap) = slot.load() {
            let epoch = if incremental {
                self.prop_epoch(token)
            } else {
                self.store.now_ts()
            };
            if snap.at >= epoch {
                counted!("graph.range index built by another worker");
                return Some(std::sync::Arc::clone(&snap.value));
            }
            // The loser's case under the write fence: the winner published
            // below the epoch because a writer was in flight, and that
            // writer's rows are the log's entries above the winner's stamp.
            // Catch up from there instead of scanning the partition again
            // (see `adj_table_snapshot_reporting`).
            if incremental && !tried.as_ref().is_some_and(|t| std::sync::Arc::ptr_eq(t, &snap)) {
                if let Some(next) = self.range_index_caught_up(token, label, &slot, &snap) {
                    counted!("graph.range index caught up behind the build guard");
                    return Some(next);
                }
            }
        }
        self.prop_log
            .borrow_mut()
            .entry(token)
            .or_insert_with(|| ChangeLog::new(PROP_LOG_CAP));
        let at = self.store.now_ts();
        // A persisted index (loaded at open, index-at-seal) serves without a
        // rebuild — but only while it is still current. Its vintage is the clock
        // it was built at; if any write has advanced `now_ts` past it, the on-disk
        // index predates rows it must cover, so fall through and rebuild.
        if let Some(idx) = self.store.persisted_index(token) {
            if idx.as_of() == at {
                let idx = match label {
                    // Fix 54: the persisted index covers the WHOLE partition
                    // (`build`), and this slot is a LABEL'S. Served as-is, a
                    // scoped probe for a common value walked every label's
                    // rows — on the read-only mirror the clock never moves,
                    // so every scoped slot took the partition's index and
                    // `{status: "pending"}` over a 701-node label cost 4.8 ms
                    // (a value nobody carries: 0.4). Restricting it here is
                    // one membership test per entry, no store read, once per
                    // slot; the unscoped slot still takes it whole.
                    Some(l) if label_token.is_some() => {
                        let members = self.members(Some(l)).ok()?.to_arc_vec();
                        let scoped = idx.restricted_to(&mut |body: &[u8]| {
                            <[u8; 8]>::try_from(body)
                                .ok()
                                .map(u64::from_be_bytes)
                                .is_some_and(|id| members.binary_search(&id).is_ok())
                        });
                        counted!("graph.range index served from disk, restricted to the label");
                        std::sync::Arc::new(scoped)
                    }
                    _ => {
                        counted!("graph.range index served from disk");
                        idx
                    }
                };
                slot.publish(self.fenced(at), std::sync::Arc::clone(&idx));
                return Some(idx);
            }
        }
        counted!("graph.range index builds");
        let def = engram_store::IndexDef::new(token, engram_store::PropertyId(token));
        let idx = std::sync::Arc::new(match label {
            // SCOPED: build over the label's members only. `members` is the
            // id-sorted membership snapshot, so this is O(label) where the
            // partition scan is O(every node carrying the property).
            Some(l) if label_token.is_some() => {
                let view = self.members(Some(l)).ok()?;
                let bodies: Vec<Vec<u8>> = view.iter().map(|id| id.to_be_bytes().to_vec()).collect();
                engram_store::RangeIndex::build_over(&self.store, &self.nodes, def, at, bodies)
            }
            _ => engram_store::RangeIndex::build(&self.store, &self.nodes, def, at),
        });
        // Fenced AFTER the scan (see `fenced`): the scan holds every row at
        // or below `at`; an entry a still in-flight writer will record is
        // stamped above the clamp and inside the next catch-up.
        let at = self.fenced(at);
        slot.publish(at, std::sync::Arc::clone(&idx));
        self.prune_prop_log(token, at);
        Some(idx)
    }

    /// Carry `snap` forward over the property log's entries since it and
    /// publish the result, or `None` when the log no longer reaches it or
    /// the index declines the delta (`with_changes`) — a rebuild is due.
    ///
    /// Last write per row wins, in stamp order; the stamp is the log's epoch
    /// read in the SAME critical section as the entries, fenced below every
    /// in-flight writer in that section too (`fenced`). Served from the
    /// handle whether or not the publish won (a fenced stamp can land ON the
    /// slot's current one); pruned only behind a publish that won.
    fn range_index_caught_up(
        &self,
        token: u32,
        label: Option<&str>,
        slot: &Slot<engram_store::RangeIndex>,
        snap: &Snapshot<engram_store::RangeIndex>,
    ) -> Option<std::sync::Arc<engram_store::RangeIndex>> {
        let (at, changes) = {
            let logs = self.prop_log.borrow();
            let log = logs.get(&token).filter(|log| log.covers(snap.at))?;
            let mut changes: BTreeMap<Vec<u8>, Option<engram_store::IndexKey>> = BTreeMap::new();
            for (_, (body, key)) in log.since(snap.at) {
                changes.insert(body.clone(), key.clone());
            }
            (self.fenced(log.epoch()), changes)
        };
        // The filter that used to stand here is GONE, and its absence is the
        // point. It existed because a scoped index was carried forward from a
        // log covering every label, so it had to reject rows that were not its
        // own — and it cost a `members(label)` lookup on every catch-up to do
        // it. Keying the log `(label, property)` means the log holds only this
        // label's rows by construction, so there is nothing to reject.
        //
        // A guard that has become unreachable is not free: it is read as
        // protection. This one was removed rather than left inert, and the
        // property it protected is now held by `scoped_index_catch_up.rs`
        // against the LOG KEYING instead — which is the thing that actually
        // provides it.
        let _ = label;
        // NOTHING FOR THIS LABEL: re-stamp, do not rebuild.
        //
        // Filtering fixed WHAT a scoped index takes; it did not change WHETHER
        // a catch-up runs, because staleness is still tested against
        // `prop_epoch(token)` — a clock keyed on the property NAME, which every
        // `:Message` write advances. So a `Person.id` index was still carried
        // forward on every read, now correctly applying an empty change set:
        // the pollution stopped and the work did not. Measured, 3 interleaved
        // pairs: 878 vs 881 ops/s, `idx_catchups` unchanged at ~5,400.
        //
        // When the filter leaves nothing, the index is UNCHANGED — so publish
        // the same one at the newer stamp. That is an `Arc` clone against
        // `with_changes`, which clones and re-sorts `added` (up to `FOLD_AT`
        // pairs, each body a fresh allocation) to produce an identical index.
        // It also advances `snap.at`, so the log prunes behind it and the next
        // reader can take the `snap.at >= prop_epoch` fast path outright.
        //
        // The `freshprops` control is what says this is where the cost is: the
        // same node written under property names that miss the `id` log took
        // `idx_catchups` to 0 and throughput +17%.
        if changes.is_empty() {
            counted!("graph.range index catch-up had nothing for this label");
            let same = std::sync::Arc::clone(&snap.value);
            if slot.publish(at, std::sync::Arc::clone(&same)) {
                self.prune_prop_log(token, at);
            }
            return Some(same);
        }
        let next = snap.value.with_changes(&changes, at)?;
        counted!("graph.range index caught up");
        let next = std::sync::Arc::new(next);
        if slot.publish(at, std::sync::Arc::clone(&next)) {
            self.prune_prop_log(token, at);
        } else {
            counted!("graph.range index catch-up publish lost, slot unchanged");
        }
        Some(next)
    }

    /// Drop a property log's entries behind a PUBLISHED snapshot at `at`.
    fn prune_prop_log(&self, token: u32, at: u64) {
        if let Some(log) = self.prop_log.borrow_mut().get_mut(&token) {
            log.prune_below(at);
        }
    }

    /// Drop a label log's entries behind a PUBLISHED snapshot at `at`.
    fn prune_label_log(&self, token: u32, at: u64) {
        if let Some(log) = self.label_log.borrow_mut().get_mut(&token) {
            log.prune_below(at);
        }
    }

    /// **Index-at-seal** — build (if needed) and persist the range index for each
    /// named property to `dir`, so a later [`engram_store::Store::open_paged_dir`]
    /// on that directory loads it warm instead of rebuilding on the first query.
    ///
    /// Each index is stamped with the current clock; the reader
    /// (`Graph::ensure_range_index`) discards a persisted index whose vintage no
    /// longer matches the snapshot, so persisting is always safe — a stale sidecar
    /// costs a rebuild, never a wrong answer. A property that was never minted is
    /// skipped (nothing to index). Returns the number of indexes written.
    /// The property names covered by a DECLARED range index, deduplicated and
    /// in a deterministic order.
    ///
    /// This is what the maintenance thread persists: an index the operator
    /// asked for, rather than whatever a query happened to build. Returns empty
    /// (never an error) when the catalogue cannot be read — a sidecar is an
    /// optimisation, and failing a maintenance pass over one would trade a lost
    /// speed-up for a lost pass.
    pub fn declared_index_props(&self) -> Vec<String> {
        let Ok(defs) = self.declared_range_indexes() else {
            return Vec::new();
        };
        let mut out: Vec<String> = defs
            .iter()
            .flat_map(|d| d.props.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        out.sort();
        out
    }

    /// Write the named properties' range indexes to sidecars under `dir`.
    pub fn persist_indexes(&self, dir: &std::path::Path, props: &[&str]) -> std::io::Result<usize> {
        let mut written = 0;
        for prop in props {
            let Some(token) = self.token_peek("prop:", &self.props, prop) else {
                continue; // property never minted — nothing to persist
            };
            let Some(idx) = self.ensure_range_index(prop) else {
                continue;
            };
            engram_store::Store::write_index_sidecar(dir, token, &idx)?;
            written += 1;
        }
        Ok(written)
    }

    /// **Index-ordered top-k with a semijoin filter** — the "friends' newest
    /// messages" shape (IC9 stage 2), served by scanning a sorted property index
    /// newest-first instead of expanding every candidate and sorting.
    ///
    /// Returns the top `limit` node ids carrying integer property `order_prop`
    /// with value `< upper`, whose `edge_types`-neighbour in `dir` lies in
    /// `filter_set`, ranked by `(order_prop DESC, tie_prop ASC)`. It walks the
    /// index DESC (`iter_desc_below`), semijoin-filters each candidate against
    /// `filter_set` (one adjacency probe), and stops as soon as the buffer is
    /// full and the current key falls strictly below the K-th best — so a
    /// NON-selective filter (IC9's dense friend set) touches only ~`limit /
    /// selectivity` candidates, not the whole fan-out.
    ///
    /// `(order_prop, tie_prop)` is a total order (`tie_prop` = `message.id` is
    /// unique), so the result is the unique true top-k — byte-identical to the
    /// expand-then-`native_topk` path it replaces. `None` if `order_prop` is not
    /// minted (nothing to rank).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn index_ordered_topk_semijoin(
        &self,
        order_prop: &str,
        upper: i64,
        edge_types: &Option<Vec<u32>>,
        dir: Dir,
        filter_set: &std::collections::BTreeSet<u64>,
        tie_prop: &str,
        limit: usize,
        scan_budget: usize,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        use engram_store::IndexKey;
        let Some(idx) = self.ensure_range_index(order_prop) else {
            return Ok(Some(Vec::new()));
        };
        if limit == 0 {
            return Ok(Some(Vec::new()));
        }
        counted!("graph.index-ordered topk ran");
        // Buffer `(order_date, node_id)` for qualifying candidates in scan
        // (date-DESC) order. The tie property is NOT read here — reading it per
        // candidate would materialise a node for every scanned message. Instead
        // keep EVERY candidate whose date is >= the K-th largest date buffered
        // (so a boundary-date tie group is complete), then a SINGLE bulk gather
        // reads the tie key for just this bounded set.
        let mut buf: Vec<(i64, u64)> = Vec::new();
        let mut scanned = 0usize;
        for (key, body) in idx.iter_desc_below(&IndexKey::Int(upper)) {
            // Cost-model bail: if the buffer is not even filled to K after
            // `scan_budget` index entries, the semijoin filter is too SELECTIVE
            // for the index-ordered scan to pay off (IC2's sparse friend set) —
            // decline so the caller uses expand-then-topk instead.
            scanned += 1;
            if scanned > scan_budget && buf.len() < limit {
                counted!("graph.index-ordered topk bailed (filter too selective)");
                return Ok(None);
            }
            let IndexKey::Int(ord) = key else {
                // A non-integer key (mixed-type index) — this operator ranks
                // integers; decline to the general path.
                return Ok(None);
            };
            let ord = *ord;
            // Descending scan: once K are buffered and this date is strictly
            // below the K-th largest buffered date, nothing later can qualify
            // (every remaining date is <= this one). Ties on that date are still
            // captured — they compare EQUAL here and are appended above.
            if buf.len() >= limit && ord < buf[limit - 1].0 {
                break;
            }
            let Ok(idb) = <[u8; 8]>::try_from(body) else {
                continue;
            };
            let node_id = u64::from_be_bytes(idb);
            // Semijoin: COUNT this node's edges into the filter set. Exactly one
            // is a single (friend, message) path — the per-message row this
            // operator emits. MORE than one (a message with parallel edges into
            // the set, or two distinct in-set creators) would be >1 path in the
            // expand semantics, which a per-message scan cannot reproduce — so
            // DECLINE to the general expand+topk (byte-identical). Real LDBC
            // HAS_CREATOR is 1:1, so IC9 never triggers this.
            let mut in_set = 0usize;
            self.adjacent_slim_for_each(node_id, dir, edge_types, |e| {
                if filter_set.contains(&e.peer) {
                    in_set += 1;
                }
            });
            if in_set > 1 {
                counted!("graph.index-ordered topk declined (edge multiplicity)");
                return Ok(None);
            }
            if in_set == 1 {
                buf.push((ord, node_id)); // date-DESC by construction
            }
        }
        // ONE bulk gather of the tie key over the bounded buffer — point-gather,
        // not a per-candidate node materialisation (the late-materialise budget
        // the expand path also respects).
        let node_ids: Vec<u64> = buf.iter().map(|&(_, n)| n).collect();
        let tie_col = self.column_entries_gather(ColumnFamily::Nodes, tie_prop, &node_ids)?;
        let mut tie_of: std::collections::BTreeMap<u64, i64> = std::collections::BTreeMap::new();
        for (nid, v) in tie_col {
            if let Value::Int(i) = v {
                tie_of.insert(nid, i);
            }
        }
        // Rank by (date DESC, tie ASC) — a total order — and take K.
        let mut ranked: Vec<(i64, i64, u64)> = buf
            .into_iter()
            .map(|(date, nid)| (date, tie_of.get(&nid).copied().unwrap_or(i64::MAX), nid))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ranked.truncate(limit);
        Ok(Some(ranked.into_iter().map(|(_, _, n)| n).collect()))
    }

    /// **Anchored hierarchy collect** — IC12's stage-1 lever. Computes the same
    /// set as `MATCH (a:LA)-[:edge_types*0..]->(b:LB) WHERE a.name=V OR
    /// b.name=V WITH collect(a.collect_prop)` but ANCHORED on the value `V`
    /// instead of scanning every `a`:
    ///
    /// - **Set A** — `a`-nodes named `V` that have ≥1 outgoing edge into a
    ///   `b`-node (so a length-≥1 path to a `b:LB` exists, which the `*0..`
    ///   pattern requires since a length-0 path would need `a` to carry `LB`).
    /// - **Set B** — every `a`-node that reaches a `b`-node named `V`: a REVERSE
    ///   BFS from the `V`-named `b`-nodes over `edge_types` (In), collecting the
    ///   `a`-labelled nodes it reaches (the `b`-hierarchy is walked DOWN, not
    ///   every `a` walked up).
    ///
    /// Returns the `collect_prop` values of `A ∪ B`, deduped by node. Order is
    /// unspecified (the caller uses the result as an `IN` set), so this is
    /// membership-identical to the general traversal — the byte-identity that
    /// matters for the downstream query. `None` if `name_prop` was never minted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn collect_anchored_hierarchy(
        &self,
        a_label: &str,
        b_label: &str,
        edge_types: &Option<Vec<u32>>,
        name_prop: &str,
        name_val: &Value,
        collect_prop: &str,
    ) -> Result<Option<Vec<Value>>, GraphError> {
        let Some(anchors) = self.index_probe_eq(name_prop, name_val, None)? else {
            return Ok(None); // name never minted / not index-servable
        };
        counted!("graph.anchored hierarchy collect ran");
        let a_members = self.members_all(&[a_label.to_string()])?;
        let b_members = self.members_all(&[b_label.to_string()])?;
        let is_a = |id: u64| a_members.contains(id);
        let is_b = |id: u64| b_members.contains(id);

        let mut tags: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut frontier: Vec<u64> = Vec::new();
        for id in anchors {
            if is_a(id) {
                // Set A: an `a` named V, but only if a length-≥1 path to a `b`
                // exists (an out-edge into a `b`-node), matching the pattern.
                let mut reaches_b = false;
                self.adjacent_slim_for_each(id, Dir::Out, edge_types, |e| {
                    reaches_b = reaches_b || is_b(e.peer);
                });
                if reaches_b {
                    tags.insert(id);
                }
            }
            if is_b(id) {
                frontier.push(id); // Set B seed: a `b` named V
            }
        }
        // Reverse BFS from the V-named `b`-nodes; collect `a`-labelled nodes.
        let mut visited: std::collections::BTreeSet<u64> = frontier.iter().copied().collect();
        while let Some(node) = frontier.pop() {
            let mut peers: Vec<u64> = Vec::new();
            self.adjacent_slim_for_each(node, Dir::In, edge_types, |e| peers.push(e.peer));
            for p in peers {
                if visited.insert(p) {
                    if is_a(p) {
                        tags.insert(p); // an `a` reaching a V-named `b`
                    }
                    frontier.push(p); // walk further (a `b` subclass continues; an `a` is a leaf)
                }
            }
        }

        // The collect property for the result set — one bulk gather.
        let ids: Vec<u64> = tags.into_iter().collect();
        let col = self.column_entries_gather(ColumnFamily::Nodes, collect_prop, &ids)?;
        Ok(Some(col.into_iter().map(|(_, v)| v).collect()))
    }

    /// The ids matching any of `probes`, or `None` when the match set would
    /// exceed `cap` - a seek over a match set that large loses to the column
    /// scan, so it is not worth even MATERIALISING the ids (14,807 of them
    /// cost ~30 ms to gather only to be discarded). `None` cap gathers all.
    /// The union of the ids matching ANY of `values` (the `prop IN [...]`
    /// seek), or `None` when that union would exceed `cap` or a value is not
    /// index-servable. Each value reuses the single-value probe; the union is
    /// deduped and re-checked against the cap.
    pub fn index_probe_in(
        &self,
        prop: &str,
        values: &[Value],
        cap: Option<usize>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        self.index_probe_in_scoped(prop, values, cap, None)
    }

    /// [`Graph::index_probe_in`] against a LABEL-SCOPED index — the one a
    /// `CREATE INDEX … FOR (n:Label) ON (n.prop)` declared. The label must be
    /// one the pattern REQUIRES (see [`Graph::index_probe_eq_scoped`]).
    pub fn index_probe_in_scoped(
        &self,
        prop: &str,
        values: &[Value],
        cap: Option<usize>,
        label: Option<&str>,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        let mut all: Vec<u64> = Vec::new();
        for v in values {
            match self.index_probe_eq_scoped(prop, v, cap, label)? {
                Some(ids) => all.extend(ids),
                None => return Ok(None),
            }
            if let Some(c) = cap {
                if all.len() > c {
                    return Ok(None);
                }
            }
        }
        all.sort_unstable();
        all.dedup();
        Ok(Some(all))
    }

    fn probe_ids(
        idx: &engram_store::RangeIndex,
        probes: &[engram_store::IndexKey],
        cap: Option<usize>,
    ) -> Option<Vec<u64>> {
        use engram_store::IndexKey;
        let hi_of = |k: &IndexKey| -> IndexKey {
            match k {
                IndexKey::Int(i) if *i < i64::MAX => IndexKey::Int(i + 1),
                IndexKey::Int(_) => IndexKey::Float(f64::from_bits(u64::MAX)), // smallest float in total_cmp order sorts above every Int
                IndexKey::Float(f) => IndexKey::Float(next_total_cmp(*f)),
                IndexKey::Str(b) => {
                    let mut nb = b.clone();
                    nb.push(0);
                    IndexKey::Str(nb)
                }
            }
        };
        // Pre-count via binary search and bail before cloning a single body:
        // `WHERE nodeType = 'email'` matches 14,807 entries, and gathering
        // them only to discard the seek cost ~30 ms.
        if let Some(c) = cap {
            let mut total = 0usize;
            for k in probes {
                total += idx.range_count(k, &hi_of(k));
                if total > c {
                    return None;
                }
            }
        }
        let mut ids: Vec<u64> = Vec::new();
        for k in probes {
            for body in idx.range(k, &hi_of(k)).bodies {
                if let Ok(b) = <[u8; 8]>::try_from(body.as_slice()) {
                    ids.push(u64::from_be_bytes(b));
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    }

    /// A token's id if it EXISTS — never minting. A count over a label
    /// nobody ever wrote must answer 0, not create the label.
    fn token_peek(
        &self,
        family: &'static str,
        cache: &arc_swap::ArcSwap<BTreeMap<String, u32>>,
        name: &str,
    ) -> Option<u32> {
        // Lock-free read — on the count/probe hot path.
        if let Some(t) = cache.load().get(name) {
            return Some(*t);
        }
        let mut body = family.as_bytes().to_vec();
        body.extend_from_slice(name.as_bytes());
        let bytes = self.store.get(&self.kv, &body)?;
        let t = u32::from_le_bytes(bytes.as_slice().try_into().ok()?);
        cache.rcu(|old| {
            let mut new: BTreeMap<String, u32> = (**old).clone();
            new.insert(name.to_string(), t);
            std::sync::Arc::new(new)
        });
        Some(t)
    }

    /// A label's members (or every node), as a [`MembersView`] snapshot
    /// current at the label's epoch. Seeds, counts and column batches all read
    /// one walk's result until THAT label's membership changes — a write to
    /// any other label, or to any property, leaves it untouched (a hot-key
    /// `SET n.hits` used to rebuild the whole label on the next read: 0.25 ms
    /// to 120 ms on the label-scanning aggregate, 480x).
    ///
    /// The reader protocol of `derived.rs`: lock-free on a hit; O(delta) to
    /// catch up from the label's change log, the ids that joined and left
    /// layered over the shared base; one build across all workers on a miss.
    /// Sound because `note_membership_of` is the only writer of the log and
    /// all four mutation sites route through it.
    pub fn members(&self, label: Option<&str>) -> Result<MembersView, GraphError> {
        let committed = self.members_committed(label)?;
        // Inside a transaction with buffered writes, overlay ITS membership
        // changes — the ids its buffered rows add to or remove from this
        // label — on the committed snapshot. The shared slot keeps the
        // committed view; the overlay is this call's, and it is what lets a
        // statement's later clause MATCH a node an earlier clause created.
        if !self.in_txn_with_writes() {
            return Ok(committed);
        }
        let pending = match label {
            None => self.txn_pending(&self.nodes, &[]),
            Some(l) => self
                .token_peek("lbl:", &self.labels, l)
                .and_then(|t| self.txn_pending(&self.index, &membership_prefix(t))),
        };
        let Some(pending) = pending else {
            return Ok(committed);
        };
        let changes: Vec<(u64, bool)> = pending
            .into_iter()
            .filter_map(|(body, is_put)| {
                let id_bytes = &body[body.len().checked_sub(8)?..];
                let id = u64::from_be_bytes(id_bytes.try_into().ok()?);
                Some((id, is_put))
            })
            .collect();
        if changes.is_empty() {
            return Ok(committed);
        }
        counted!("graph.membership overlaid a transaction's writes");
        Ok(committed.apply_with(changes, self.members_batch_fold.get()))
    }

    /// The COMMITTED membership of a label — the shared snapshot, never a
    /// transaction's private view. See [`Graph::members`].
    fn members_committed(&self, label: Option<&str>) -> Result<MembersView, GraphError> {
        let token = match label {
            None => u32::MAX,
            Some(l) => match self.token_peek("lbl:", &self.labels, l) {
                Some(t) => t,
                None => return Ok(MembersView::empty()),
            },
        };
        self.members_at_token(token, label, true).map(|(v, _)| v)
    }

    /// Bring the membership snapshot of `token` current for the maintenance
    /// refresh — the read path's own catch-up-or-rebuild, driven by token.
    /// A rebuild needs the label's NAME (the walk is by name); a token this
    /// graph instance has never resolved is left to the reader that will.
    /// `may_rebuild` is the refresh's rebuild budget: `false` catches up
    /// only, and reports `Deferred` where a walk would have been needed.
    /// What a catch-up of this label's snapshot would cost, in changes
    /// folded. `None` when no catch-up is available (the log does not reach
    /// the snapshot), which belongs to the rebuild budget, not the row budget
    /// — the same rule the adjacency half follows.
    ///
    /// The fold is the cost: batched it is O(k log k), and the serial arm it
    /// replaced was a sorted `Vec::insert` per change, i.e. O(k^2). Either
    /// way k is what a pass should be metering.
    fn members_catch_up_cost(&self, token: u32, at: u64) -> Option<usize> {
        let logs = self.label_log.borrow();
        let log = logs.get(&token).filter(|log| log.covers(at))?;
        Some(log.since(at).count())
    }

    fn members_refresh_token(&self, token: u32, may_rebuild: bool) -> Result<MembersOutcome, GraphError> {
        let name: Option<String> = if token == u32::MAX {
            None
        } else {
            match self
                .labels
                .load()
                .iter()
                .find(|(_, t)| **t == token)
                .map(|(n, _)| n.clone())
            {
                Some(n) => Some(n),
                None => return Ok(MembersOutcome::Current),
            }
        };
        self.members_at_token(token, name.as_deref(), may_rebuild)
            .map(|(_, outcome)| outcome)
    }

    /// The committed membership snapshot of `token` (`u32::MAX` = every node)
    /// and how it was reached. `label` is the token's name, needed only if a
    /// walk of the label must rebuild it. With `may_rebuild` false a snapshot
    /// the log cannot catch up is returned AS IT IS with `Deferred` — the
    /// refresh's budget; every reader passes `true`.
    fn members_at_token(
        &self,
        token: u32,
        label: Option<&str>,
        may_rebuild: bool,
    ) -> Result<(MembersView, MembersOutcome), GraphError> {
        let slot = slot_in(&self.members_cache, &token, MEMBERS_CACHE_MAX);
        let incremental = self.incremental_caches.get();
        // The snapshot a catch-up was already attempted on and found
        // uncovered — see `adj_table_snapshot_reporting`: not retried behind
        // the build guard (a floor only rises), but a DIFFERENT snapshot
        // there is the winner's and is caught up rather than rebuilt.
        let mut tried: Option<std::sync::Arc<Snapshot<MembersView>>> = None;
        if let Some(snap) = slot.load() {
            if snap.at >= self.store.now_ts() {
                counted!("graph.membership snapshots current");
                return Ok(((*snap.value).clone(), MembersOutcome::Current));
            }
            if incremental {
                if snap.at >= self.label_epoch(token) {
                    counted!("graph.membership snapshots still current");
                    return Ok(((*snap.value).clone(), MembersOutcome::Current));
                }
                // Genuinely stale — a snapshot plus the ids that changed since
                // is O(delta), against a walk of the label.
                if let Some((next, outcome)) = self.members_caught_up(token, &slot, &snap) {
                    return Ok(((*next).clone(), outcome));
                }
                if let Some(snap) = slot.load() {
                    if snap.at >= self.label_epoch(token) {
                        counted!("graph.membership snapshots republished");
                        return Ok(((*snap.value).clone(), MembersOutcome::Current));
                    }
                }
                tried = Some(std::sync::Arc::clone(&snap));
            }
            if !may_rebuild {
                counted!("graph.membership rebuild deferred by the refresh budget");
                return Ok(((*snap.value).clone(), MembersOutcome::Deferred));
            }
        } else if !may_rebuild {
            // Nothing published: a reader is building it right now, or it
            // was retracted. Not this pass's work either way.
            return Ok((MembersView::empty(), MembersOutcome::Deferred));
        }
        // Build, single-flight PER SLOT; `at` read BEFORE the walk so that
        // every change stamped at or below it has rows the walk sees.
        let _build = slot.enter_build();
        if let Some(snap) = slot.load() {
            let epoch = if incremental {
                self.label_epoch(token)
            } else {
                self.store.now_ts()
            };
            if snap.at >= epoch {
                counted!("graph.membership snapshots built by another worker");
                return Ok(((*snap.value).clone(), MembersOutcome::Current));
            }
            // The loser's case under the write fence: the winner's publish
            // was clamped below the epoch by a writer in flight, and the ids
            // that writer committed are exactly the log's entries above the
            // winner's stamp. Catch up from there — O(delta) — rather than
            // walk the label again. (Same defect and fix as the adjacency
            // build guard; see `adj_table_snapshot_reporting`.)
            if incremental && !tried.as_ref().is_some_and(|t| std::sync::Arc::ptr_eq(t, &snap)) {
                if let Some((next, outcome)) = self.members_caught_up(token, &slot, &snap) {
                    counted!("graph.membership snapshots caught up behind the build guard");
                    return Ok(((*next).clone(), outcome));
                }
            }
        }
        let at = self.store.now_ts();
        counted!("graph.membership snapshots built");
        counters::MEMBERS_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let next = std::sync::Arc::new(MembersView::from_base(std::sync::Arc::new(
            self.nodes_by_label_committed(label)?,
        )));
        // Fenced AFTER the walk — see `fenced`.
        let at = self.fenced(at);
        slot.publish(at, std::sync::Arc::clone(&next));
        self.prune_label_log(token, at);
        Ok(((*next).clone(), MembersOutcome::Rebuilt))
    }

    /// Catch `snap` up from the label's change log and publish the result,
    /// or `None` when the log no longer reaches it (a rebuild is due).
    ///
    /// The stamp is the log's epoch read in the SAME critical section as the
    /// entries, fenced below every in-flight writer in that same section
    /// (`fenced`). The caught-up view is returned whether or not the publish
    /// won — it is at least as current as whatever it lost to — but the
    /// outcome says which: `CaughtUp` only when the slot advanced, else
    /// `Deferred` (the fenced stamp landed ON the slot's current one while
    /// a writer registered there is in flight). The refresh reported the
    /// lost case as caught up, bumped its counter, and re-caught the same
    /// snapshot up on every pass. Pruned only behind a publish that won.
    fn members_caught_up(
        &self,
        token: u32,
        slot: &Slot<MembersView>,
        snap: &Snapshot<MembersView>,
    ) -> Option<(std::sync::Arc<MembersView>, MembersOutcome)> {
        let batch = self.members_batch_fold.get();
        let (at, next) = {
            let logs = self.label_log.borrow();
            let log = logs.get(&token).filter(|log| log.covers(snap.at))?;
            let next = snap
                .value
                .apply_with(log.since(snap.at).map(|(_, e)| *e), batch);
            (self.fenced(log.epoch()), next)
        };
        counted!("graph.membership snapshots caught up");
        counters::MEMBERS_CAUGHT_UP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let next = std::sync::Arc::new(next);
        if slot.publish(at, std::sync::Arc::clone(&next)) {
            self.prune_label_log(token, at);
            return Some((next, MembersOutcome::CaughtUp));
        }
        counted!("graph.membership catch-up publish lost, slot unchanged");
        Some((next, MembersOutcome::Deferred))
    }

    /// A label's members as a plain sorted vector — for the consumers that
    /// need a slice. Free when the snapshot carries no overlay; otherwise one
    /// O(n) materialisation. Prefer [`Graph::members`] on any path that only
    /// iterates or tests membership.
    pub fn members_ids(&self, label: Option<&str>) -> Result<std::sync::Arc<Vec<u64>>, GraphError> {
        Ok(self.members(label)?.to_arc_vec())
    }

    /// Is `id` in `m`? — a hop's label filter, which is a membership TEST and
    /// nothing more.
    ///
    /// `MembersView::contains` answers it against the base and the overlay
    /// separately, in O(log n), touching neither. The A/B arm materialises the
    /// view first — the path the pipeline used to take — which merges the whole
    /// label the first time each published snapshot is asked. That memo is per
    /// SNAPSHOT, and a concurrent write stream publishes tens of snapshots a
    /// second, so it degrades to a full merge per query on the largest label in
    /// the corpus. See `set_hop_membership_contains`.
    pub(crate) fn members_contains(&self, m: &MembersView, id: u64) -> bool {
        if self.hop_membership_contains.get() {
            return m.contains_with(id, self.members_bitmap_after.get());
        }
        counted!("graph.hop label filter materialised the label");
        m.to_arc_vec().binary_search(&id).is_ok()
    }

    /// Apply a write to the count store if it is live; a store still
    /// awaiting its rebuild ignores writes (the rebuild sees them).
    /// Apply an EXPLICIT change to the counts.
    ///
    /// Inside a transaction this accumulates the signed change directly, where
    /// `stats_apply` had to clone the counts TWICE — once to seed `before` and
    /// once to make `after` — to discover a closure's effect by difference.
    /// Two `Stats` clones is four `BTreeMap` deep copies, six times for a
    /// `CREATE (a)-[:R]->(b)`.
    ///
    /// Outside a transaction it applies through `bump`, exactly as before —
    /// including `bump`'s creation of a zero entry for a decrement on an absent
    /// key. The two paths already differed there and each keeps its behaviour;
    /// `StatsDelta::apply` carries the in-transaction rule.
    fn stats_change(&self, c: StatsChange) {
        if self.stats_delta.get() && self.in_txn() {
            counted!("graph.stats applied as a delta");
            self.txn_touch(|t| t.stats.add_change(&c));
            return;
        }
        // The differential arm, and the out-of-transaction path: express the
        // change as the closure `stats_apply` would have received.
        self.stats_apply(|st| {
            st.nodes = st.nodes.saturating_add_signed(c.nodes);
            st.rels = st.rels.saturating_add_signed(c.rels);
            for (t, d) in &c.by_label {
                bump(&mut st.by_label, *t, *d);
            }
            for (t, d) in &c.by_type {
                bump(&mut st.by_type, *t, *d);
            }
        });
    }

    fn stats_apply(&self, f: impl FnOnce(&mut Stats)) {
        // A write buffered in a transaction must NOT move the shared counts —
        // they reflect COMMITTED state, and the write may yet roll back or
        // lose its validation. Its effect is recorded as a DELTA on the
        // transaction instead (the closure applied to a copy of the current
        // counts, and the difference kept), applied at commit, dropped at
        // rollback. `count()` inside the transaction adds the same effect
        // from the buffered rows themselves (`txn_row_delta`).
        if self.in_txn() {
            let mut before = self.with_stats(Stats::clone);
            self.txn_touch(|t| {
                // Seeded with the transaction's OWN accumulated delta first:
                // a delete of a node this transaction created must see the
                // count that create produced, or the saturating decrement of
                // a committed count of zero records nothing and the store
                // drifts up by one at commit, for ever.
                t.stats.apply(&mut before);
                let mut after = before.clone();
                f(&mut after);
                t.stats.add_diff(&before, &after);
            });
            return;
        }
        if let Some(st) = self.stats.borrow_mut().as_mut() {
            f(st);
        }
    }

    /// The count store rebuilt from the store: one membership-prefix walk
    /// (keys only) plus one O-prefix walk (keys only).
    fn rebuild_stats(&self) -> Stats {
        let mut st = Stats {
            nodes: self.store.count_at(&self.nodes, &[], u64::MAX),
            ..Stats::default()
        };
        for body in self.store.scan_bodies_prefix(&self.index, b"L") {
            if body.len() == 1 + 4 + 8 {
                let t = u32::from_be_bytes(body[1..5].try_into().expect("4"));
                bump(&mut st.by_label, t, 1);
            }
        }
        for body in self.store.scan_bodies_prefix(&self.index, b"O") {
            if body.len() == 1 + 8 + 4 + 8 + 8 {
                let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                bump(&mut st.by_type, t, 1);
                st.rels += 1;
            }
        }
        counted!("graph.stats rebuilt");
        st
    }

    /// Read the count store, rebuilding it first if this graph was opened
    /// over data it did not write.
    fn with_stats<R>(&self, f: impl FnOnce(&Stats) -> R) -> R {
        sometimes!("graph.count answered from maintained stats", true);
        if let Some(st) = self.stats.borrow().as_ref() {
            return f(st);
        }
        let rebuilt = self.rebuild_stats();
        // Another worker may have rebuilt meanwhile; whichever is in the slot
        // now is what this read answers from — never a panic on an empty
        // slot, which a concurrent drop between check and read once caused.
        let mut slot = self.stats.borrow_mut();
        f(slot.get_or_insert(rebuilt))
    }

    /// The ids carrying EVERY label — a merge-intersection of the per-label
    /// membership snapshots (each id-sorted), walked from the smallest.
    /// `(s:Bio:Species)` declined every count fast path on its second
    /// label and paid a full stream of the larger label instead (5 s on
    /// the production port); the intersection is O(Σ|members|) once per
    /// epoch per label set, then a length.
    pub fn members_all(&self, labels: &[String]) -> Result<MembersView, GraphError> {
        match labels {
            [] => self.members(None),
            [one] => self.members(Some(one)),
            many => {
                let mut sets = Vec::with_capacity(many.len());
                for l in many {
                    sets.push(self.members(Some(l))?);
                }
                sets.sort_by_key(MembersView::len);
                let (smallest, rest) = sets.split_first().expect("two or more labels");
                let acc: Vec<u64> = smallest
                    .iter()
                    .filter(|id| rest.iter().all(|s| s.contains(*id)))
                    .collect();
                sometimes!("graph.multi-label membership intersected", true);
                Ok(MembersView::from_base(std::sync::Arc::new(acc)))
            }
        }
    }

    /// Count live nodes carrying every label in `labels`.
    pub fn count_labels_nodes(&self, labels: &[String]) -> Result<u64, GraphError> {
        match labels {
            [] => Ok(self.count_all_nodes()),
            [one] => Ok(self.count_label_nodes(one)),
            many => Ok(self.members_all(many)?.len() as u64),
        }
    }

    /// Count every live node — `MATCH (n) RETURN count(n)` without
    /// materialising one. The interpreter's general path clones each node's
    /// full property map into a row to throw it away; on the full
    /// production port that was the difference between an answer and the
    /// OOM killer. Every real engine keeps this path cheap; so does this one.
    pub fn count_all_nodes(&self) -> u64 {
        counted!("graph.count fast paths");
        let committed = self.with_stats(|st| st.nodes);
        committed.saturating_add_signed(self.txn_row_delta(&self.nodes, &[]))
    }

    /// Count live nodes carrying a label, via the membership rows (keys
    /// only, no records touched). A label never minted counts 0.
    /// Whether a property-equality SEEK beats scanning `label` for a probe
    /// of `probe` matches. Two conditions, both learned from the production
    /// port: the probe must be selective enough that materialising its
    /// matches one at a time is cheaper than reading the property column
    /// over the whole label - a full node decode costs many times a column
    /// entry, so `probe < label` alone is far too weak (`WHERE nodeType =
    /// 'email'` matched ~500k of ~600k UserDataNode and the per-id path cost
    /// 1155 ms against the column scan's 91 ms) - and the label must be big
    /// enough that avoiding the scan is worth the index at all. A selective
    /// `abuseStatus = 'quarantined'` over many articles is the case it
    /// serves: 122 ms to 6 ms. A missing label (a bare `MATCH (n)`) never
    /// seeks - there is no scan to beat, only every node to build over.
    /// Whether a property-equality on `label` is even worth PROBING - the
    /// cheap pre-check made BEFORE building/reading the range index, because
    /// the probe itself is not free. `WHERE status = 'pending'` returns
    /// 106,667 ids across every label; extracting them cost 25 ms only to be
    /// discarded when the ResearchTask label is 422 nodes. A label below the
    /// floor scans in well under a millisecond, so it never probes.
    pub(crate) fn property_seek_worth_probing(&self, label: Option<&str>) -> bool {
        match label {
            Some(l) => self.count_label_nodes(l) >= PROPERTY_SEEK_MIN_LABEL,
            None => false,
        }
    }

    /// Whether the SEEK beats the scan once the probe has answered `probe`
    /// matches: few in absolute terms (a full node decode dwarfs a column
    /// entry) and a small fraction of the label. `WHERE nodeType = 'email'`
    /// was 14,807 of 34,407 UserDataNode - 43% - and materialising them one
    /// at a time cost 1155 ms against the column scan's 125 ms.
    pub(crate) fn property_seek_wins(&self, label: Option<&str>, probe: usize) -> bool {
        self.property_seek_wins_under(label, probe, PROPERTY_SEEK_MAX_PROBE, PROPERTY_SEEK_SELECTIVITY)
    }

    /// [`Graph::property_seek_wins`] under a caller's own cap and
    /// selectivity: the per-id paths keep the defaults (a full node decode
    /// per id), while a walk over the sought ids — its columns from the
    /// cache or one gather — costs about a column entry per id and wins on
    /// any real reduction, so it may take a wider seek.
    pub(crate) fn property_seek_wins_under(
        &self,
        label: Option<&str>,
        probe: usize,
        cap: usize,
        selectivity: u64,
    ) -> bool {
        let Some(l) = label else {
            return false;
        };
        let size = self.count_label_nodes(l);
        probe <= cap
            && size >= PROPERTY_SEEK_MIN_LABEL
            && (probe as u64).saturating_mul(selectivity) < size
    }

    /// The label a property-equality seek on `prop` should probe SCOPED — the
    /// label of a DECLARED range index whose first property is `prop` and whose
    /// label the pattern REQUIRES (one of `labels`) — or `None` to probe the
    /// partition-wide index as before.
    ///
    /// ONE rule for every seek site (the anchored pattern map, the pipeline's
    /// anchored seed, the columnar count/projection): the operator declared
    /// `CREATE INDEX … FOR (n:Label) ON (n.prop)`, so that is the index a
    /// pattern on `Label` is served from. Two sites choosing differently is
    /// not merely inconsistent — both indexes share one change log, and the
    /// one that catches up prunes the log behind the other's snapshot, so the
    /// other REBUILDS on its next probe (`derived_structures` pinned exactly
    /// that leapfrog when the columnar seek went scoped and the anchored one
    /// had not). On the production mirror the partition-wide `userId` index
    /// covers ~5M nodes; the scoped one covers the label.
    ///
    /// Deterministic given the data: the catalogue is sorted, and the first
    /// declaration that fits wins.
    pub(crate) fn declared_scope_for(
        &self,
        labels: &[String],
        prop: &str,
    ) -> Result<Option<String>, GraphError> {
        if labels.is_empty() {
            return Ok(None);
        }
        let declared = self.declared_range_indexes()?;
        // Fix 47: a TRAILING key of a declared composite counts as declared
        // too. Neo4j's composite index carries every key and answers a range
        // on a trailing one from its entries (`NewsStory(status,
        // lastUpdatedAt)` for `lastUpdatedAt > $cutoff`); here a composite
        // was only ever its leading key's scoped index, so the mirror
        // declared the same catalogue and read the whole label.
        Ok(declared
            .iter()
            .find(|d| labels.contains(&d.label) && d.props.iter().any(|p| p == prop))
            .map(|d| d.label.clone()))
    }

    /// What this graph's derived structures hold in memory right now — the
    /// attribution a memory limit has to be sized from. `kubectl top` gave a
    /// number (9.8 GiB idle, 25 GiB under shadow reads on the production
    /// mirror) and nothing said which structure it was; this says. Sizes are
    /// the payload vectors (an entry's size times its count), not allocator
    /// slack, so they are a floor on what the structures cost.
    pub fn memory_report(&self) -> MemoryReport {
        let mut r = MemoryReport::default();
        for slot in self.adj_tables.load().values() {
            if let Some(snap) = slot.load() {
                r.adjacency_tables += 1;
                r.adjacency_bytes += snap.value.index.bytes()
                    + snap.value.entries.len() * std::mem::size_of::<SlimAdj>();
            }
        }
        for slot in self.members_cache.load().values() {
            if let Some(snap) = slot.load() {
                r.memberships += 1;
                r.membership_bytes += snap.value.len() * std::mem::size_of::<u64>();
            }
        }
        for slot in self.range_cache.load().values() {
            if let Some(snap) = slot.load() {
                r.range_indexes += 1;
                // An entry is an IndexKey (a small enum, or a string's bytes)
                // plus an 8-byte body plus the vector headers: ~64 B is the
                // integer/short-string shape the platform's keys take.
                r.range_index_bytes += snap.value.len() * 64;
            }
        }
        {
            let cache = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
            r.prop_columns = cache.entries.len();
            r.prop_column_bytes = cache.bytes;
        }
        r
    }

    /// The commit clock a column read is stamped with — read BEFORE the
    /// read, so that a column is current only while nothing has committed
    /// since it was assembled (`Store::now_ts` moves on every write).
    pub(crate) fn column_stamp(&self) -> u64 {
        self.store.now_ts()
    }

    /// The property-column cache's byte budget.
    pub(crate) fn prop_column_budget(&self) -> usize {
        self.prop_columns.lock().unwrap_or_else(|e| e.into_inner()).budget
    }

    /// The byte budget of the property-column cache (default
    /// [`PROP_COLUMN_BUDGET_BYTES`]); `0` disables caching.
    pub fn set_prop_column_budget(&self, bytes: usize) {
        let mut cache = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
        cache.budget = bytes;
        cache.evict_to(0);
    }

    /// A cached whole-label PROPERTY COLUMN — the `(id, value)` entries of
    /// `prop` over `label`'s members (or the ids carrying it, for a presence
    /// read) — when one is current, i.e. nothing has committed since it was
    /// read.
    ///
    /// # Why
    ///
    /// A paged segment carries no column blocks, so a columnar count, filter
    /// or projection over a wide label assembled its columns by ONE POINT
    /// READ PER MEMBER on every execution: the NewsArticle enrichment count
    /// paid 300k reads (836 ms against Neo4j's 191) for the same 150k values
    /// each time, the UserDataNode classified count 76k (82 ms against 4)
    /// — every wide-label shape the production corpus runs was 3–20× behind
    /// on nothing but re-reading what it had read a statement earlier. The
    /// walk that assembled the column now hands it here; the next walk over
    /// the same label reads it back at memory speed and binds it exactly as
    /// it would its own (the entries are the gather's, unchanged).
    ///
    /// # Currency
    ///
    /// A column is stamped with the commit clock read before its gather and
    /// is current while the clock has not moved: any commit — a node write,
    /// a relationship, a label — retires every column, which is the one
    /// rule that cannot serve a stale value without a change log per
    /// property (only indexed properties keep one). On a read-mostly store
    /// that is a rebuild per burst of writes; on a write-heavy one it is the
    /// cost the walk paid before, plus a lookup. Inside a transaction with
    /// buffered writes the columnar paths are off, so nothing here is ever
    /// read against a private write.
    ///
    /// # Budget
    ///
    /// Least-recently-used under `set_prop_column_budget` (512 MB default);
    /// a column wider than the whole budget is handed back but not kept.
    pub(crate) fn prop_column(&self, label: &str, prop: &str, presence: bool) -> Option<PropColumn> {
        let lt = self.token_peek("lbl:", &self.labels, label)?;
        let pt = self.token_peek("prop:", &self.props, prop)?;
        let mut cache = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
        if cache.budget == 0 {
            return None;
        }
        let now = self.store.now_ts();
        let key = (lt, pt, presence);
        let current = cache.entries.get(&key).is_some_and(|e| e.at >= now);
        if !current {
            if let Some(e) = cache.entries.remove(&key) {
                cache.bytes = cache.bytes.saturating_sub(e.bytes);
                counted!("graph.property column retired by a commit");
            }
            return None;
        }
        cache.tick += 1;
        let tick = cache.tick;
        let e = cache.entries.get_mut(&key).expect("present");
        e.used = tick;
        counted!("graph.property column served");
        Some(e.col.clone())
    }

    /// Keep a whole-label column a walk assembled (see [`Graph::prop_column`]):
    /// `at` is the stamp read before the gather. Returns whether it was kept.
    pub(crate) fn keep_prop_column(&self, label: &str, prop: &str, at: u64, col: PropColumn) -> bool {
        let Some(lt) = self.token_peek("lbl:", &self.labels, label) else {
            return false;
        };
        let Some(pt) = self.token_peek("prop:", &self.props, prop) else {
            return false;
        };
        let (presence, bytes) = match &col {
            PropColumn::Values(v) => (
                false,
                v.len() * 40 + v.iter().map(|(_, x)| value_heap_bytes(x)).sum::<usize>(),
            ),
            PropColumn::Presence(ids) => (true, ids.len() * 8),
        };
        let mut cache = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
        if cache.budget == 0 || bytes > cache.budget {
            counted!("graph.property column not kept: over budget");
            return false;
        }
        let key = (lt, pt, presence);
        if let Some(old) = cache.entries.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(old.bytes);
        }
        cache.evict_to(bytes);
        cache.tick += 1;
        let used = cache.tick;
        cache.entries.insert(
            key,
            PropColumnEntry {
                at,
                col,
                aligned: None,
                bytes,
                used,
            },
        );
        cache.bytes += bytes;
        counted!("graph.property column kept");
        true
    }

    /// The cached value column of `prop` over `label` ALIGNED to `members`
    /// — the label's full membership, ascending, as `members_all` answers
    /// it — or `None` when the column is not cached (or not current). The
    /// aligned vector is built ONCE (`vectorized::align`) and kept beside the
    /// column under the same budget, so a column-at-a-time count reads it
    /// without copying a value: `count_over_cached_columns` aligned every
    /// column per statement, copying 44k lists for `$a IN
    /// coalesce(g.affectedCountries, [])` (17.8 ms against Neo4j's 6.9).
    /// The column is current only while nothing has committed since it was
    /// read, and a label's membership changes only by a commit, so an
    /// aligned vector of the members' length is aligned to THESE members.
    pub(crate) fn prop_column_aligned(
        &self,
        label: &str,
        prop: &str,
        members: &[u64],
    ) -> Option<std::sync::Arc<Vec<Value>>> {
        let lt = self.token_peek("lbl:", &self.labels, label)?;
        // A property NOTHING EVER WROTE has no token, no column and no cache
        // entry: it is Null on every member, and that is an aligned column.
        // `$a IN coalesce(g.affectedCountries, [])` on the mirror named such
        // a property — the gather answered "absent everywhere" without a
        // read, nothing was kept (no token to key it), and every run fell
        // to the per-member walk: 44k evaluations, 15 ms against Neo4j's 6.
        let Some(pt) = self.token_peek("prop:", &self.props, prop) else {
            counted!("graph.property column absent everywhere");
            return Some(std::sync::Arc::new(vec![Value::Null; members.len()]));
        };
        let mut guard = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
        let cache: &mut PropColumnCache = &mut guard;
        if cache.budget == 0 {
            return None;
        }
        let now = self.store.now_ts();
        let key = (lt, pt, false);
        let current = cache.entries.get(&key).is_some_and(|e| e.at >= now);
        if !current {
            if let Some(e) = cache.entries.remove(&key) {
                cache.bytes = cache.bytes.saturating_sub(e.bytes);
                counted!("graph.property column retired by a commit");
            }
            return None;
        }
        cache.tick += 1;
        let tick = cache.tick;
        let budget = cache.budget;
        let held = cache.bytes;
        let e = cache.entries.get_mut(&key).expect("present");
        e.used = tick;
        counted!("graph.property column served");
        if let Some(a) = &e.aligned {
            if a.len() == members.len() {
                counted!("graph.property column served aligned");
                return Some(std::sync::Arc::clone(a));
            }
        }
        let PropColumn::Values(col) = &e.col else {
            return None;
        };
        let aligned = std::sync::Arc::new(crate::vectorized::align(members, col));
        counted!("graph.property column aligned");
        let bytes = aligned.len() * 32 + aligned.iter().map(value_heap_bytes).sum::<usize>();
        // Kept beside the column when it fits the budget as it stands (no
        // eviction to make room for a derived copy); served once otherwise.
        if held.saturating_add(bytes) <= budget {
            e.aligned = Some(std::sync::Arc::clone(&aligned));
            e.bytes += bytes;
            cache.bytes += bytes;
            counted!("graph.property column kept aligned");
        }
        Some(aligned)
    }

    /// Whether every one of `props` (values) and `presence` (presence-only)
    /// has a CURRENT cached column over `label` — the aggregate's batch
    /// decision: a first read over a wide label walks it whole so that the
    /// columns it assembles are kept; later reads batch against the cache.
    pub(crate) fn prop_columns_current(&self, label: &str, props: &[String], presence: &[String]) -> bool {
        let Some(lt) = self.token_peek("lbl:", &self.labels, label) else {
            return props.is_empty() && presence.is_empty();
        };
        let cache = self.prop_columns.lock().unwrap_or_else(|e| e.into_inner());
        if cache.budget == 0 {
            return false;
        }
        let now = self.store.now_ts();
        let has = |p: &String, presence: bool| -> bool {
            match self.token_peek("prop:", &self.props, p) {
                Some(pt) => cache.entries.get(&(lt, pt, presence)).is_some_and(|e| e.at >= now),
                None => true, // a property nothing ever wrote — no column to read
            }
        };
        props.iter().all(|p| has(p, false)) && presence.iter().all(|p| has(p, true))
    }

    /// The number of live nodes carrying `label` - an O(1) stat lookup.
    pub fn count_label_nodes(&self, label: &str) -> u64 {
        counted!("graph.count fast paths");
        let Some(t) = self.token_peek("lbl:", &self.labels, label) else {
            return 0;
        };
        let committed = self.with_stats(|st| st.by_label.get(&t).copied().unwrap_or(0));
        committed.saturating_add_signed(self.txn_row_delta(&self.index, &membership_prefix(t)))
    }

    /// Count every live relationship — `MATCH ()-[r]->() RETURN count(r)`.
    pub fn count_all_rels(&self) -> u64 {
        counted!("graph.count fast paths");
        let committed = self.with_stats(|st| st.rels);
        committed.saturating_add_signed(self.txn_row_delta(&self.rels, &[]))
    }

    /// Enter or leave BULK-INGEST mode. On: graph writes skip the commit
    /// log (recovery-by-replay will not see them — the bulk contract is
    /// durability by re-ingest, exactly as every serious importer defines
    /// it) and entity ids reserve in ranges of 4096 (one counter write per
    /// range; a crash abandons the unused tail as id gaps, never reuse).
    /// Off (the default): every write is logged, ids allocate one at a
    /// time, semantics identical to before this mode existed.
    pub fn set_bulk_ingest(&self, on: bool) -> Result<(), GraphError> {
        self.bulk_ingest.set(on);
        if !on {
            // Abandon reservations: the next allocation re-reads the
            // counter row, which the last reservation already advanced.
            self.id_reservations.borrow_mut().clear();
            // Markers written during bulk went through `put_unlogged` like
            // the entities they describe; a crash mid-bulk can strand an
            // entity without its marker, and a marker MISS must never read
            // as "no duplicate". Rebuild every v2 family from the loaded
            // population — the bulk contract (durability by re-ingest),
            // applied to the markers too.
            self.rebuild_constraint_markers_after_bulk()?;
        }
        Ok(())
    }

    /// The one write funnel. An active transaction BUFFERS the write (published
    /// atomically at commit); otherwise it autocommits, and bulk mode decides
    /// whether the log hears it. The returned ts is a placeholder under a
    /// transaction (no commit ts exists yet) — no caller reads it.
    fn store_put(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<u64, engram_store::StoreError> {
        // Hand `value` to the transaction, or take it back for autocommit —
        // moved, never cloned.
        let value = ACTIVE_TXN.with(|t| match t.borrow_mut().as_mut() {
            Some(txn) => txn.put(prefix, body, value).map(|()| None),
            None => Ok(Some(value)),
        })?;
        match value {
            None => Ok(0), // buffered in the transaction
            Some(value) if self.bulk_ingest.get() => self.store.put_unlogged(prefix, body, value),
            Some(value) => self.store.put(prefix, body, value),
        }
    }

    /// [`Graph::store_put`] for a row that must be VISIBLE but need not be
    /// DURABLE — the adjacency guard row, and nothing else.
    ///
    /// The guard exists so a relationship write and a node DELETE conflict in
    /// either commit order. Its content is never read: `guard_row` appears only
    /// at its write sites and its own definition. So it needs to reach the tail
    /// (a concurrent validator must see it) but not the log — after recovery
    /// there are no in-flight transactions, and an absent guard is
    /// indistinguishable from a present one.
    ///
    /// Only inside a TRANSACTION. Outside one there is no buffered write-set to
    /// elide from, and an autocommit guard put is its own commit; routing that
    /// through `put_unlogged` would raise `unlogged_count`, which means "log
    /// replay is not a complete recovery of this store" — a durability alarm,
    /// turned into noise. Bolt statements run in autocommit transactions, so
    /// the saving lands where the write path actually is.
    fn store_put_volatile(
        &self,
        prefix: &KeyPrefix,
        body: &[u8],
        value: StoredValue,
    ) -> Result<u64, engram_store::StoreError> {
        if !self.volatile_guards.get() {
            return self.store_put(prefix, body, value);
        }
        let value = ACTIVE_TXN.with(|t| match t.borrow_mut().as_mut() {
            Some(txn) => {
                counted!("graph.guard rows written volatile");
                txn.put_volatile(prefix, body, value).map(|()| None)
            }
            None => Ok(Some(value)),
        })?;
        match value {
            None => Ok(0), // buffered in the transaction, and not logged
            Some(value) if self.bulk_ingest.get() => self.store.put_unlogged(prefix, body, value),
            Some(value) => self.store.put(prefix, body, value),
        }
    }

    /// Set the adjacency-table entry budget (default 64M entries); `0`
    /// declines every table, which the sim uses to reach the decline path.
    pub fn set_adj_table_max_entries(&self, n: usize) {
        self.adj_table_max_entries.set(n);
    }

    /// Set how many direct degree probes an epoch tolerates before a
    /// degree table is built (default 1024; `0` builds on the first probe).
    pub fn set_degree_table_after(&self, n: u64) {
        self.degree_table_after.set(n);
    }

    /// Set how many column entries the columnar aggregate scan may read
    /// per member of the scanned label before declining to the per-id
    /// path (default 4). `usize::MAX` never declines.
    pub fn set_columnar_column_budget_factor(&self, factor: usize) {
        self.columnar_column_budget_factor.set(factor.max(1));
    }

    /// Switch the columnar paths off (or back on): every statement then
    /// takes the general path. A kill switch, and the honest forcing for
    /// tests that pin the general path's own mechanisms.
    /// Toggle driving a bound-end single hop from adjacency. Default on;
    /// off forces the full-scan path, for the differential test.
    pub fn set_hop_reversal(&self, on: bool) {
        self.hop_reversal.set(on);
    }

    /// Build the derived structures a first query would otherwise build inline,
    /// and report what it cost.
    ///
    /// # Why a database needs this
    ///
    /// The label-membership snapshots and the adjacency CSR are built lazily on
    /// first use. That is the right default for a short-lived process, and the
    /// wrong one for a server: the first query after a restart pays for the
    /// whole corpus. Measured on the LDBC SNB scale sweep, read-heavy at one
    /// client, the FIRST operation took **5.85 s against a 1.48M-node /
    /// 6.66M-relationship graph**, and the run's first ten seconds produced
    /// almost nothing (the harness's trend metric read 71x, meaning the second
    /// half did 71 times the work of the first).
    ///
    /// An operator restarting a database does not expect the first user query
    /// to take six seconds, and no amount of steady-state throughput makes that
    /// acceptable — it is a latency cliff at exactly the moment a service is
    /// least able to absorb one.
    ///
    /// # What it builds
    ///
    /// The all-nodes membership snapshot, and every adjacency table for both
    /// directions — the untyped one and one per relationship type — from a
    /// single pass per direction.
    ///
    /// Warming only the UNTYPED tables was tried first and helped nothing: every
    /// traversal in a real workload names a type (`-[:KNOWS]->`), and a typed
    /// table is a different table. The 1.48M-node run warmed in 5.5 s and then
    /// still stalled 5.0 s on its first query. Warming what the workload does
    /// not use is indistinguishable from not warming at all, and it is worse,
    /// because it looks like it worked.
    ///
    /// The type set is a schema property — small, bounded, and knowable from
    /// the data — so this is not the unbounded speculation it might sound like.
    /// `Graph::build_adj_tables_all_types` fills every bucket in one scan, so
    /// warming N types costs one pass, not N.
    ///
    /// Cost is bounded by the corpus, so a caller that would rather start fast
    /// than answer fast can skip it (`ServerConfig::warm_caches`).
    pub fn warm(&self) -> WarmReport {
        let started = self.store.now_ts();
        let members = self.members(None).map(|m| m.len()).unwrap_or(0);
        let epoch = self.store.now_ts();
        let mut edges = [0usize; 2];
        let mut tables = 0usize;
        let (mut table_bytes, mut table_capacity_bytes) = (0usize, 0usize);
        for (i, tag) in [b'O', b'I'].into_iter().enumerate() {
            let Some(built) = self.build_adj_tables_all_types(tag) else {
                continue;
            };
            for (key, table) in built {
                if key.is_none() {
                    edges[i] = table.len();
                }
                table_bytes +=
                    table.entries.len() * std::mem::size_of::<SlimAdj>() + table.index.bytes();
                table_capacity_bytes += table.entries.capacity() * std::mem::size_of::<SlimAdj>()
                    + table.index.capacity_bytes();
                // Cached under the SAME key the query path will look up:
                // `type_tokens.clone().unwrap_or_default()`, so an untyped
                // table is the empty vector and a typed one is `[token]`.
                // Stamped at the clock read before the walk, fenced below
                // any writer in flight across it (`fenced`).
                let cache_key = (tag, key.map(|t| vec![t]).unwrap_or_default());
                slot_in(&self.adj_tables, &cache_key, ADJ_TABLE_CACHE_MAX)
                    .publish(self.fenced(epoch), std::sync::Arc::new(table));
                tables += 1;
            }
        }
        counted!("graph.warmed");
        WarmReport {
            nodes: members,
            out_edges: edges[0],
            in_edges: edges[1],
            tables,
            table_bytes,
            table_capacity_bytes,
            at: started,
        }
    }

    /// Toggle **selectivity-based anchor choice** (default on).
    ///
    /// Off, a fresh pattern drives from whichever endpoint was written first —
    /// the behaviour before `reverse_to_selective_end` /
    /// `reroot_to_selective_end`. Both executors honour it, so the two arms are
    /// comparable.
    ///
    /// This exists so the improvement can be MEASURED rather than asserted:
    /// with it, one run on one host produces both the before and the after, and
    /// a performance claim stops depending on remembering which machine the
    /// baseline came from. Same reason `set_property_seek` and
    /// `set_columnar_scans` exist.
    pub fn set_selective_anchor(&self, on: bool) {
        self.selective_anchor.set(on);
    }

    /// Whether selectivity-based anchor choice is on.
    pub fn selective_anchor_enabled(&self) -> bool {
        self.selective_anchor.get()
    }

    /// Toggle **incremental cache maintenance** (default on).
    ///
    /// Off, the range index, the label-membership snapshots and the adjacency
    /// tables all revert to being rebuilt whenever the commit clock has moved —
    /// which is to say, after every write. That is the behaviour these three
    /// fixes replaced, and having it behind one switch is what makes the
    /// before/after a measurement instead of a memory.
    pub fn set_incremental_caches(&self, on: bool) {
        self.incremental_caches.set(on);
    }

    /// Whether incremental cache maintenance is on.
    pub fn incremental_caches_enabled(&self) -> bool {
        self.incremental_caches.get()
    }

    /// Toggle **cost-based adjacency repair** (default on).
    ///
    /// Off, a stale adjacency table is repaired only while fewer than
    /// `ADJ_REPAIR_MAX` (4,096) nodes changed and rebuilt otherwise — the gate
    /// that declined repair on 9,892 changed persons and rescanned a 17M-row
    /// span. On, repair is chosen whenever re-reading the changed rows costs
    /// less than half the span walk a rebuild is (see `repaired_adj_table`).
    /// Switchable so the crossover is a measurement, not a memory.
    pub fn set_adj_cost_repair(&self, on: bool) {
        self.adj_cost_repair.set(on);
    }

    /// Whether a guard row's PUT-vs-PUT write-write conflict is exempted
    /// (RC1 / O3 of `docs/write-concurrency-ceiling.md`). Default ON; the OFF
    /// arm is the pre-exemption behaviour and the A/B every guard test runs
    /// against.
    ///
    /// The guard row (`'G' | node id`) exists to make a relationship write and
    /// a node DELETE a write-write conflict in either commit order. Two
    /// relationship writes touching one node both PUT it and so abort each
    /// other — ~48% of the re-runs on the `rel-hub` shape (measured by the
    /// conflict-class counter), and semantically nothing. The exemption fires
    /// only when BOTH the committed version and this transaction's intent are
    /// puts, so a tombstone on either side still conflicts and the guarantee
    /// the guard was built for is untouched.
    pub fn set_guard_put_put_exempt(&self, on: bool) {
        self.guard_put_put_exempt.set(on);
    }

    /// Whether a constraint-list cache hit skips the schema-epoch store probe.
    ///
    /// Every constrained write calls `constraints_snapshot`, which read
    /// `kv/con\0epoch` through the transaction to decide whether its cached
    /// list was current. That key is ALWAYS ABSENT until the first constraint
    /// DDL, and an absent KV key is the worst case for the read path: the
    /// sparse index cannot reject it (`covering_block` cannot exclude a `KV`
    /// key, whose kind byte sorts above NODE/EDGE/INDEX_ENTRY), so the probe
    /// descends every sealed segment — on paged SF1, up to ~117 block-cache
    /// acquisitions, `pread`s and BLAKE3 verifications per write.
    ///
    /// With the lever on, a cache hit registers the key in the read set
    /// instead of reading it. **The abort behaviour is identical**: validation
    /// asks only whether the key moved since the snapshot, so a registered key
    /// and a read key give the same verdict, and a constraint DDL committing
    /// after our snapshot still aborts every in-flight enforcing writer. What
    /// disappears is a point read whose answer we already held.
    ///
    /// Off restores the probe, and is the differential arm.
    pub fn set_constraint_epoch_cache(&self, on: bool) {
        self.constraint_epoch_cache.set(on);
    }

    /// Ids a serving session reserves per counter write. `0` or `1` restores
    /// one LOGGED counter write per entity — the differential arm, and the
    /// behaviour before this existed.
    ///
    /// The counter row always holds the reserved END, so a crash abandons the
    /// unused tail as gaps and an id is never reused. Ids stay dense within a
    /// run (the first is still 1); only a restart or a bulk-mode exit shows a
    /// gap, which is the same contract bulk ingest has always had.
    pub fn set_id_reservation(&self, ids: usize) {
        self.id_reservation.set(ids);
    }

    /// Rows one maintenance refresh pass may re-read before deferring the
    /// rest. `0` means unbounded — the pre-budget behaviour, kept as the A/B
    /// arm and as the escape hatch if a corpus ever needs a whole pass.
    pub fn set_refresh_pass_rows(&self, rows: usize) {
        self.refresh_pass_rows.set(rows);
    }

    /// The current refresh pass budget, in rows.
    pub fn refresh_pass_rows(&self) -> usize {
        self.refresh_pass_rows.get()
    }

    /// Whether the cost-based repair gate is on.
    pub fn adj_cost_repair_enabled(&self) -> bool {
        self.adj_cost_repair.get()
    }

    /// Toggle the **batched membership fold** (default on). Off is the sorted
    /// insert per change — O(k²) in the pending changes — kept as the
    /// differential arm proving the two folds reach the same overlay.
    pub fn set_members_batch_fold(&self, on: bool) {
        self.members_batch_fold.set(on);
    }

    /// Whether the batched membership fold is on.
    pub fn members_batch_fold_enabled(&self) -> bool {
        self.members_batch_fold.get()
    }

    /// Toggle the **scan-policy rebuild walk** (default on). Off, an adjacency
    /// table's (re)build walks the span through the plain block-cache path —
    /// promoting every block it crosses and admitting each into the probation
    /// queue. Same rows either way (resident stores are byte-identical); the
    /// lever exists so the paged cache's behaviour under a rebuild is measured
    /// on and off in one run.
    pub fn set_scan_resistant_rebuild(&self, on: bool) {
        self.scan_resistant_rebuild.set(on);
    }

    /// Whether adjacency rebuilds walk the span under the scan policy.
    pub fn scan_resistant_rebuild_enabled(&self) -> bool {
        self.scan_resistant_rebuild.get()
    }

    /// Bring every CACHED derived structure that is behind its source's epoch
    /// current NOW — the reader-independent publish.
    ///
    /// Every adjacency table and membership snapshot catches up lazily, on
    /// the first read that needs it. A write burst with no reader between
    /// therefore accumulates its whole changed set for that one reader:
    /// pruning happens only behind a publish (`prune_adj_logs`,
    /// `prune_label_log`), and a publish happens only on a read. SF1's
    /// `contention` level stalled 25 s on its 12th read for exactly that
    /// reason — two write-only levels before it, 41-47k relationship writes
    /// across all 9,892 persons, and the first `HAS_CREATOR` read paid.
    ///
    /// This is that read's work, done by whoever calls it (the server's
    /// maintenance thread, after N acknowledged writes or on its tick) so the
    /// next reader finds current structures. It takes exactly the paths and
    /// latches a reader would — the same repair-or-rebuild under the same
    /// single-flight, the same monotone publish — so a concurrent reader that
    /// races it either finds the published snapshot or builds one at a
    /// later-or-equal epoch; nothing here can regress a slot. Structures that
    /// have never been built are NOT built: this refreshes what readers have
    /// shown they use, it does not warm.
    ///
    /// A no-op with incremental caches off: that arm treats every write as
    /// invalidating everything, and a refresh under it would rebuild the
    /// world on every tick.
    ///
    /// # Off the read path
    ///
    /// A refresh must never make a reader wait. Three rules keep it so:
    ///
    /// 1. **Repairs are unbounded, rebuilds are budgeted: ONE per pass.** A
    ///    repair is O(changed rows) and takes no build guard; a rebuild is a
    ///    walk of the whole span (25 s for an untyped table on SF1 paged),
    ///    and a pass that rebuilt every stale table in a row would hold the
    ///    maintenance thread — and the store's read latches — for minutes.
    ///    What one pass defers, the next pass or the next reader takes.
    /// 2. **Untyped tables are never REBUILT here.** They are cached only
    ///    because the warm built them (a reader asks for a typed table); a
    ///    stale one is repaired if its logs allow and otherwise left to the
    ///    reader that actually wants it — which, on the measured runs, was
    ///    nobody, while the refresh rebuilt two of them every pass.
    /// 3. **Build guards are per table** (`Slot::enter_build`), so the one
    ///    rebuild a pass does blocks a reader building THAT table (which
    ///    then finds it published — and, when the publish was fenced below
    ///    the reader's epoch by a writer in flight, repairs it from the log
    ///    rather than walking the span again) and no other.
    ///
    /// A pass reports a structure refreshed only when its publish WON. A
    /// repair or catch-up whose fenced stamp lands on the slot's current one
    /// (a writer registered at that stamp still in flight) advances the slot
    /// by nothing and is reported deferred — see `AdjOutcome::Deferred`.
    pub fn refresh_stale_derived(&self) -> RefreshReport {
        let mut report = RefreshReport::default();
        if !self.incremental_caches.get() {
            return report;
        }
        // The pass's rebuild budget, shared by both families.
        let mut rebuilds_left = 1usize;
        // The pass's WORK budget, in rows re-read. `0` disables it (the
        // pre-budget behaviour, kept as the A/B arm). What this pass defers,
        // the next one takes: the tick and the write-count trigger both fire
        // again, and a deferred table is still stale, so it is still a
        // candidate. Deferring is therefore a delay, never a drop.
        let budget_rows = self.refresh_pass_rows.get();
        let mut rows_left = if budget_rows == 0 {
            usize::MAX
        } else {
            budget_rows
        };
        // Adjacency tables: the map is loaded once; a slot inserted meanwhile
        // belongs to a reader that is building it right now.
        let tables = self.adj_tables.load();
        for ((tag, types), slot) in tables.iter() {
            let Some(snap) = slot.load() else {
                continue; // a build in flight, or declined by the budget
            };
            let type_tokens = if types.is_empty() {
                None
            } else {
                Some(types.clone())
            };
            let epoch = self.adjacency_epoch(&type_tokens);
            if snap.at >= epoch {
                continue;
            }
            if !self.adj_tables_usable() {
                break; // a zero budget: tables are not served at all
            }
            // PRICE THE REPAIR BEFORE PAYING FOR IT. A rebuild is already
            // budgeted at one per pass; repairs were not, and a pass that
            // repaired every stale table in turn is what taxed the writers.
            // `None` here means no repair is available (the log does not
            // reach, or a cap refuses), so the rebuild budget decides — the
            // pass never skips a table it could only rebuild on the grounds
            // of a repair cost that does not apply.
            if rows_left != usize::MAX {
                if let Some(cost) =
                    self.adj_repair_cost_rows(*tag, &type_tokens, snap.at, snap.value.len())
                {
                    // THE BUDGET IS WORK-CONSERVING: it bounds what a pass does
                    // ON TOP of one unavoidable item, never what it can do at
                    // all.
                    //
                    // "Deferring is a delay, never a drop" holds only while a
                    // later pass can afford what this one skipped. A table
                    // whose repair ALONE exceeds the whole budget breaks that:
                    // every future pass declines it for the same reason and its
                    // delta only grows, so the delay is permanent and the table
                    // comes back only when the change log finally overflows and
                    // some reader rebuilds it — 262,144 entries later, with
                    // every read walking the span meanwhile.
                    //
                    // §8 is what made this reachable. Readers used to repair on
                    // every stale read, which kept the delta small for the pass
                    // as a side effect nobody had named; now they decline the
                    // expensive ones, and the pass has to be able to finish the
                    // job it was already nominally responsible for.
                    // `review_repair_over_cap_differential` caught it: five
                    // tables deferred, nothing repaired, nothing rebuilt.
                    //
                    // Taking it only when the pass has spent nothing yet keeps
                    // the bound meaningful — one oversized item per pass, not
                    // an unbounded run of them.
                    let untouched = rows_left == budget_rows;
                    if cost > rows_left && !untouched {
                        counted!("graph.derived refresh deferred by the row budget");
                        report.adjacency_deferred += 1;
                        continue;
                    }
                    if cost > rows_left {
                        counted!("graph.derived refresh took a repair over its whole budget");
                    }
                    rows_left = rows_left.saturating_sub(cost);
                }
            }
            // §5.3 — THE REBUILD IS DEMOTED TO A FALLBACK, not deleted. A
            // rebuild here is a walk of the whole span (25 s for an untyped
            // table on SF1 paged) done to bring a table current that a full
            // compaction now produces as a by-product of work it must do
            // anyway. So the maintenance pass stops paying for it: a stale
            // table repairs from the log if the log reaches, and otherwise
            // waits for the next compaction.
            //
            // THE HONEST CAVEAT. `build_adj_table` still runs — for a cold
            // start, and when a log overflows before a compaction arrives —
            // and it now runs on the READER'S thread rather than the
            // maintenance one. That trades a background cost for a tail
            // latency, and it is only a good trade while full compactions are
            // frequent enough to keep the fallback rare. That frequency is not
            // assumed here: it is the measured quantity §5.5's decision rule
            // turns on, and `graph.adjacency tables built` is what reports it.
            let may_rebuild =
                type_tokens.is_some() && rebuilds_left > 0 && !self.demote_adj_rebuild.get();
            let (_, outcome) =
                self.adj_table_snapshot_reporting(*tag, &type_tokens, epoch, false, may_rebuild, false);
            let refreshed = match outcome {
                AdjOutcome::Repaired => {
                    report.adjacency_repaired += 1;
                    true
                }
                AdjOutcome::Rebuilt => {
                    rebuilds_left -= 1;
                    report.adjacency_rebuilt += 1;
                    true
                }
                AdjOutcome::Declined => {
                    report.adjacency_declined += 1;
                    false
                }
                // Budget-deferred, or a repair whose fenced publish lost:
                // either way the slot was not advanced by this pass, so it
                // is not "refreshed" — the counter below would otherwise
                // climb once per pass for a table that never moved.
                AdjOutcome::Deferred => {
                    report.adjacency_deferred += 1;
                    false
                }
                AdjOutcome::Current => false,
            };
            if refreshed {
                counted!("graph.derived refreshed by maintenance");
                counters::DERIVED_REFRESHED_BY_MAINTENANCE
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        // Membership snapshots, likewise.
        let members = self.members_cache.load();
        for (token, slot) in members.iter() {
            let Some(snap) = slot.load() else {
                continue;
            };
            if snap.at >= self.label_epoch(*token) {
                continue;
            }
            // The membership half is budgeted on the same rule as the
            // adjacency half above, and for the same reason: without it a
            // pass folds every stale label's whole change set, and the
            // O(k log k) fold over a write burst's labels is the other half
            // of the write tax the adjacency budget alone did not remove.
            if rows_left != usize::MAX {
                if let Some(cost) = self.members_catch_up_cost(*token, snap.at) {
                    if cost > rows_left {
                        counted!("graph.derived refresh deferred by the row budget");
                        report.members_deferred += 1;
                        continue;
                    }
                    rows_left -= cost;
                }
            }
            let refreshed = match self.members_refresh_token(*token, rebuilds_left > 0) {
                Ok(MembersOutcome::CaughtUp) => {
                    report.members_caught_up += 1;
                    true
                }
                Ok(MembersOutcome::Rebuilt) => {
                    rebuilds_left -= 1;
                    report.members_rebuilt += 1;
                    true
                }
                Ok(MembersOutcome::Deferred) => {
                    report.members_deferred += 1;
                    false
                }
                Ok(MembersOutcome::Current) | Err(_) => false,
            };
            if refreshed {
                counted!("graph.derived refreshed by maintenance");
                counters::DERIVED_REFRESHED_BY_MAINTENANCE
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        report
    }

    /// Whether bound-end hop reversal is on.
    pub fn hop_reversal_enabled(&self) -> bool {
        self.hop_reversal.get()
    }

    /// Toggle dead-variable scope pruning across an UNWIND fan-out (default
    /// on). Off keeps every variable in the row, for the differential test.
    pub fn set_scope_pruning(&self, on: bool) {
        self.scope_pruning.set(on);
    }

    /// Whether dead-variable scope pruning is on.
    pub fn scope_pruning_enabled(&self) -> bool {
        self.scope_pruning.get()
    }

    /// Toggle late projection - materialising an output-only property only
    /// for the k survivors of an ORDER BY .. LIMIT (default on). Off binds
    /// every projected property eagerly, for the differential test.
    pub fn set_late_projection(&self, on: bool) {
        self.late_projection.set(on);
    }

    /// Whether late projection is on.
    pub fn late_projection_enabled(&self) -> bool {
        self.late_projection.get()
    }

    /// Toggle frontier-BFS variable-length expansion (default on). Off forces
    /// the enumerating DFS path, for the differential test that the frontier
    /// result equals the enumerated-then-DISTINCT result.
    pub fn set_frontier_expand(&self, on: bool) {
        self.frontier_expand.set(on);
    }

    /// Whether frontier-BFS variable-length expansion is on.
    pub fn frontier_expand_enabled(&self) -> bool {
        self.frontier_expand.get()
    }

    /// Toggle bounded-memory batching of the two-stage top-k tail. Default on;
    /// off forces the whole-chunk expand (the A/B byte-identity lever).
    pub fn set_multistage_topk_batch(&self, on: bool) {
        self.multistage_topk_batch.set(on);
    }

    /// Whether the two-stage top-k tail batches its stage-2 expansion.
    pub fn multistage_topk_batch_enabled(&self) -> bool {
        self.multistage_topk_batch.get()
    }

    /// Toggle seeking a property index for `WHERE n.prop = x`. Default on;
    /// off forces the label scan, for the differential test.
    pub fn set_property_seek(&self, on: bool) {
        self.property_seek.set(on);
    }

    /// Whether property-equality index seeks are on.
    pub fn property_seek_enabled(&self) -> bool {
        self.property_seek.get()
    }

    /// Toggle fix 72's count-over-chain fold (a `count(<chain var>)` over
    /// the chain a MATCH binds becomes `sum(COUNT { <chain> })` in its
    /// projection). Default on; off keeps the clause, for the differential
    /// test.
    pub fn set_chain_count_fold(&self, on: bool) {
        self.chain_count_fold.set(on);
    }

    /// Whether the count-over-chain fold is on.
    pub fn chain_count_fold_enabled(&self) -> bool {
        self.chain_count_fold.get()
    }

    /// Fix 76: whether a subquery body's seed row carries each bound node
    /// trimmed to the properties the body reads (the general matcher clones
    /// the seed row several times per evaluation, and a fat outer node made
    /// every pattern comprehension of the KM work-item listing cost its
    /// property count: 22 of the listing's 34 ms on the mirror). Default
    /// on; off seeds the whole row, for the differential test.
    pub fn set_lean_subquery_seed(&self, on: bool) {
        self.lean_subquery_seed.set(on);
    }

    /// Whether the lean subquery seed is on.
    pub fn lean_subquery_seed_enabled(&self) -> bool {
        self.lean_subquery_seed.get()
    }

    /// Whether a MULTI-KEY pattern map may seek a DECLARED index.
    pub fn pattern_map_seek_enabled(&self) -> bool {
        self.pattern_map_seek.get()
    }

    /// Whether a multi-key pattern map (`{id: X, nonce: M}`) may probe a
    /// declared index and let `node_satisfies` filter the rest, instead of
    /// scanning the label.
    ///
    /// Off restores the arity gate exactly — one-key maps seek, everything
    /// else scans — and is the differential arm. The fallback is byte-identical
    /// including row ORDER, because both candidate sources are ascending by id.
    pub fn set_pattern_map_seek(&self, on: bool) {
        self.pattern_map_seek.set(on);
    }

    /// Whether `delete_node` enumerates incident relationship IDS rather than
    /// decoding every incident relationship record.
    ///
    /// The loop only ever read `r.id`, and `delete_rel` fetches and decodes the
    /// record again under its own latch — so the first decode was waste. Off
    /// restores `rels_of` and is the differential arm.
    pub fn set_detach_via_rel_ids(&self, on: bool) {
        self.detach_via_rel_ids.set(on);
    }

    /// Whether a label's change epoch is read from an ATOMIC beside the log
    /// rather than from the log itself.
    ///
    /// Reading it from the log takes a read lock on the same lock every
    /// membership write takes in write mode, so every `members()` read
    /// serialised against every node create and delete. Off restores that and
    /// is the differential arm.
    pub fn set_label_epoch_atomics(&self, on: bool) {
        self.label_epoch_atomics.set(on);
    }

    /// Whether the adjacency guard row's PUTS are written volatile — published
    /// to the tail and validated as puts, but not appended to the commit log.
    ///
    /// A node delete's guard write stays a real, logged TOMBSTONE whatever this
    /// is set to: RC1's put-vs-put exemption is sound only because the second
    /// committer sees a non-put, so making the delete volatile would trade the
    /// dangling-edge guarantee for a number.
    ///
    /// Off logs the guard puts too, and is the differential arm.
    pub fn set_volatile_guards(&self, on: bool) {
        self.volatile_guards.set(on);
    }

    /// Whether a DECLARED index is built over its LABEL'S MEMBERS rather than
    /// the whole node partition.
    ///
    /// `CREATE INDEX ... FOR (n:L) ON (n.p)` names a label and Cypher means it.
    /// Unscoped, an index the operator scoped to a few hundred nodes covered
    /// every node in the partition carrying `p` — 3.18M entries on official
    /// SF1, where `id` is a shared property.
    ///
    /// Off restores the partition-wide index and is the differential arm. The
    /// two must answer identically for a pattern that requires the label; a
    /// scoped index simply cannot be consulted for one that does not.
    pub fn set_label_scoped_indexes(&self, on: bool) {
        self.label_scoped_indexes.set(on);
    }

    /// Whether a MATCH candidate enters the OCC read set only once it BECOMES
    /// a binding, rather than when it is materialised. **Default OFF.**
    ///
    /// The read set exists to stop a write being computed from a stale value —
    /// a DATA-FLOW property. A candidate rejected by `node_satisfies`
    /// contributes nothing but its absence, so recording it is conservative
    /// rather than necessary. Narrowing removes it, which on a label scan is
    /// the difference between O(label) and O(1) read-set entries, and
    /// validation walks that set under the global commit latch.
    ///
    /// **What it admits, precisely:** an anti-dependency on a PREDICATE. T
    /// evaluates "no `:Churn` has `id = 7`", T2 makes an existing node satisfy
    /// it, and T no longer aborts.
    ///
    /// That class is already admitted three ways, which is why this is off by
    /// choice rather than by necessity: if T2 CREATES a matching node instead
    /// of mutating one, T does not abort today either (phantoms are a
    /// documented limitation); the anomaly is already reachable by a PLAN
    /// change on the identical statement, since a one-key probe over a large
    /// label materialises only its hits; and a label's columns are read with
    /// zero read-set entries.
    ///
    /// It stays OFF until that anomaly is named in
    /// `docs/concurrency-direction.md`'s Known limitations, and MERGE keeps
    /// full recording regardless — MERGE is the "write on the basis of
    /// absence" shape, and narrowing it would be unsound rather than merely
    /// looser.
    pub fn set_read_set_bindings_only(&self, on: bool) {
        self.read_set_bindings_only.set(on);
    }

    /// Whether candidates are recorded only once they become bindings.
    pub fn read_set_bindings_only(&self) -> bool {
        self.read_set_bindings_only.get()
    }

    /// Whether an in-transaction count change is accumulated as a signed DELTA
    /// rather than discovered by cloning the counts twice and differencing.
    ///
    /// Off restores the clone-and-diff path and is the differential arm. The
    /// two must leave BYTE-IDENTICAL `Stats`, which is the whole bar: the
    /// counts feed the planner, so a divergence here is a plan change, not a
    /// number nobody reads.
    pub fn set_stats_delta(&self, on: bool) {
        self.stats_delta.set(on);
    }

    /// Whether a full paged compaction EMITS the derived bases it walked past
    /// (§5.2) — the adjacency CSRs and membership id-sets — rather than leaving
    /// them to `build_adj_table` and `nodes_by_label_committed`, which each
    /// rescan the whole corpus.
    ///
    /// Off makes `compact_paged_emitting` exactly `Store::compact_paged_to_dir`
    /// and is the differential arm. The bar is BYTE-IDENTICAL `offsets` and
    /// `entries` — not merely equal traversal results — because a CSR that
    /// answers the same queries by a different layout is a CSR whose next
    /// repair diverges.
    ///
    /// The emitted structure is a BASE, not a cache: it covers the merged
    /// segments and nothing above them, and a reader catches up from the change
    /// log exactly as it does for a table it built itself.
    pub fn set_compaction_csr(&self, on: bool) {
        self.compaction_csr.set(on);
    }

    /// Whether the maintenance refresh DECLINES to rebuild a stale adjacency
    /// table (§5.3), leaving it to a log repair, to the next compaction's emit,
    /// or to a reader that actually wants it.
    ///
    /// The pass's rebuild was one per tick and each one is a walk of the whole
    /// span. With `set_compaction_csr` on, a full compaction produces that same
    /// base from work it must do anyway, so paying for the walk separately is
    /// paying twice.
    ///
    /// What this does NOT do is delete the rebuild. It moves it: a cold start
    /// and a log that overflows before a compaction arrives still walk the
    /// span, now on the reader's thread. Off restores the one-per-pass budget
    /// and is the differential arm — the two must answer identically, since a
    /// stale table is a performance state and never a correctness one.
    pub fn set_demote_adjacency_rebuild(&self, on: bool) {
        self.demote_adj_rebuild.set(on);
    }

    /// Whether the probe-count admission gate also applies to a READER that
    /// holds a STALE snapshot it cannot repair.
    ///
    /// Off, such a reader falls through to a full span rebuild on its own query
    /// thread — 25 s for an untyped table at SF1 — because `tried.is_some()`
    /// bypasses the gate. On, it declines and the query is served from the
    /// direct span walk instead: the same answer, built from the store.
    ///
    /// This is the other half of `set_demote_adjacency_rebuild`. Demoting the
    /// maintenance pass's rebuild made READERS the only ones who rebuild, which
    /// turned that bypass from a corner into the whole policy.
    ///
    /// The two arms must answer IDENTICALLY — a table is a performance
    /// structure and the walk is the same truth — so the differential's bar is
    /// byte-identical results, with a counter half showing the rebuild actually
    /// stopped happening.
    pub fn set_reader_rebuild_admission(&self, on: bool) {
        self.reader_rebuild_admission.set(on);
    }

    /// Whether a reader's adjacency REPAIR runs behind the per-table build
    /// guard — single-flight, the way a rebuild already is.
    ///
    /// Off, the repair is attempted before the guard, so every reader that
    /// finds the table stale repairs it on its own thread and only the first
    /// publish wins. On a mixed profile that is not a corner: a write makes the
    /// table stale, and every reader arriving before the next publish repeats
    /// the same work. Measured on the local attribution harness (`balattr`,
    /// 8 clients, 50/50): 37,686 repairs of which 20,787 published into an
    /// already-advanced slot — **55% of the repair work discarded**, while the
    /// mix ran 5x slower than the write-only arm it should have bracketed.
    ///
    /// On, the guard is taken before the repair and the slot re-read behind
    /// it, so a reader that waited finds the winner's table and returns
    /// instead of rebuilding the same overlay. The wait is the point: a repair
    /// is short, and one repair plus N-1 cheap re-reads beats N repairs.
    ///
    /// The two arms must answer IDENTICALLY — this changes who does the work
    /// and not what the work produces, and the repair itself is unchanged —
    /// so the differential's bar is byte-identical `AdjTableParts`, with a
    /// counter half showing the discarded publishes actually stopped.
    /// Ships OFF, and the measurement is the reason rather than caution.
    ///
    /// On, the redundancy fell from 54.6% to 3.2% and the repair count with it
    /// (39,579 → 6,576 over the same 3 s) — the fix did exactly what it was
    /// built to do. Throughput went the other way: 26,986 → 16,190 ops/s, 40%
    /// WORSE. Removing six sevenths of the work made the mix slower, because
    /// the readers that used to duplicate it in parallel now queue on one
    /// mutex, and the repair sits on the critical path of every read.
    ///
    /// That is the finding worth keeping: the redundancy was never the cost.
    /// It is left in, defaulted off, because it is the control that says so —
    /// and because a future in which the repair is no longer on the read path
    /// would change the verdict, at which point this arm is how it is re-asked.
    /// The lever that acts on the real mechanism is `set_lazy_stale_serve`.
    pub fn set_single_flight_repair(&self, on: bool) {
        self.single_flight_repair.set(on);
    }

    /// Whether a SINGLE-NODE reader may be served from a stale adjacency table
    /// when the change set since that table was built does not touch its node.
    ///
    /// Off, any write makes the table stale for everyone, and every reader
    /// arriving before the next publish repairs the entire change set on its
    /// own query thread — re-reading rows for nodes it will never look at,
    /// carrying an overlay bounded only by `ADJ_OVERLAY_FOLD`, and publishing
    /// a table most of its peers will discard.
    ///
    /// On, such a reader asks the far narrower question it actually has —
    /// *did MY node move?* — and when the answer is no, reads the row the
    /// table already holds. The row is the row a repair would have re-read, so
    /// this is the same answer at the same vintage; see
    /// `Graph::adj_node_moved_since` for why that is identity and not
    /// approximation. When the answer is yes, or the delta is too long to scan
    /// per read, it falls through to today's repair — which is also what keeps
    /// the delta bounded.
    ///
    /// The differential's bar is therefore byte-identical ROWS on both arms
    /// under a write stream, with a counter half showing the stale serve
    /// actually fired — without it the two arms are the same code path and
    /// agree about nothing.
    pub fn set_lazy_stale_serve(&self, on: bool) {
        self.lazy_stale_serve.set(on);
    }

    /// Whether the lock-free change filter fronts the single-node staleness
    /// check that `set_lazy_stale_serve` enables.
    ///
    /// Off, that check is answered from the change log — correct, exact, and
    /// taken under the same lock every write holds exclusively. On, it is
    /// answered first by one atomic load against a stamp table the writers
    /// update with one `fetch_max` per changed row, and the log is consulted
    /// only when that says "maybe".
    ///
    /// The filter can say "maybe" for a node that did not change (it ignores
    /// relationship type, and its slots collide); it cannot say "unchanged"
    /// for one that did. So the two arms differ in how much work they do and
    /// not in what they conclude, and the differential's bar is byte-identical
    /// rows with a counter half showing the filter actually cleared reads.
    pub fn set_adj_change_filter(&self, on: bool) {
        self.adj_change_filter_on.set(on);
    }

    /// Overlay rows a repaired adjacency table may carry before folding.
    ///
    /// `AdjTable::slice` is the hottest read in the engine — one call per
    /// expanded node — and its first act is `if !self.overlay.is_empty()`. A
    /// freshly built or folded table takes a null-root test; a table carrying
    /// an overlay takes a `BTreeMap` DESCENT, on every hop.
    ///
    /// That is the per-hop half of §9's interference, and it is why the damage
    /// splits the way it does: `ic6-friend-tags` expands on the order of a
    /// million nodes and goes 97.6 -> 140.9 ms mixed, while the disjoint
    /// control — same reads, writes on a type no read traverses, so no table
    /// ever carries an overlay — sits at 97.8 ms, exactly its solo latency.
    ///
    /// `0` folds on EVERY repair, which trades an O(table) fold per repair for
    /// an always-empty overlay. At the observed repair rate (17 in a 30 s
    /// `balanced` run) that trade is cheap, and it is the arm that separates
    /// the per-hop cost from the per-read staleness check, which the disjoint
    /// control removes together.
    pub fn set_adj_overlay_fold(&self, n: usize) {
        self.adj_overlay_fold.set(n);
    }

    /// Restore the pre-fix path for every label MEMBERSHIP TEST: materialise
    /// the label, then `binary_search` it. The A/B arm for
    /// `hop_membership_contains` — off, a test over a label with an overlay
    /// costs O(|label|) per snapshot instead of O(log n) per probe, and under a
    /// write stream a new snapshot is published tens of times a second.
    ///
    /// It covers the hop filter and the anchored SEED's probe-vs-label
    /// intersection, which is the same test at the other end of the plan.
    pub fn set_hop_membership_contains(&self, on: bool) {
        self.hop_membership_contains.set(on);
    }

    /// Restore the pre-fix adjacency-table resolution: build the `(tag, types)`
    /// map key and walk the `BTreeMap` on EVERY probe, instead of re-serving
    /// the snapshot this thread just resolved.
    ///
    /// The A/B arm for `adj_snap_memo`. Off, a typed hop heap-allocates its map
    /// key once per row and compares it through that allocation — work that is
    /// identical for every row of the hop, because only the node varies.
    ///
    /// It changes cost, never answers: the memo re-serves a snapshot only when
    /// it still satisfies the same `at >= epoch` rule the walk applies, so both
    /// arms return the same table. That is what makes the differential a
    /// measurement of the lookup and not of the plan.
    pub fn set_adj_snap_memo(&self, on: bool) {
        self.adj_snap_memo.set(on);
    }

    /// Restore the pre-fix DIRECTED close probe: read the LEVEL var's row — a
    /// different CSR line every call — instead of the bound endpoint's hot row
    /// with the direction flipped.
    ///
    /// The A/B arm for the directed bound-side probe. The undirected close has
    /// probed from the bound side since v55; this extends the same locality to
    /// directed closes, which is sound because a directed edge set is the same
    /// set from either endpoint once the direction flips with the row
    /// (`Dir::flipped`). Priced at ~166 ns/row by prof-q2c before it was built.
    pub fn set_directed_bound_probe(&self, on: bool) {
        self.directed_bound_probe.set(on);
    }

    /// See [`Graph::set_directed_bound_probe`].
    pub(crate) fn directed_bound_probe(&self) -> bool {
        self.directed_bound_probe.get()
    }

    /// Enable the morsel-parallel COUNT FOLD (P-2): `fold_tail` splits its
    /// driving rows across the installed [`ScopedExec`], one `FoldState` per
    /// worker, partials merged in row order — byte-identical to the serial
    /// loop, which the differential canary proves. Off by default for the same
    /// reason `parallel_expand` is: the engine never spawns, and every
    /// published single-thread number stays comparable.
    pub fn set_parallel_fold(&self, on: bool) {
        self.parallel_fold.set(on);
    }

    /// See [`Graph::set_parallel_fold`].
    pub(crate) fn parallel_fold_enabled(&self) -> bool {
        self.parallel_fold.get()
    }

    /// The freshness `with_adj_table` would demand for these type tokens.
    ///
    /// A test seam, because the epoch's KEYING is not observable through query
    /// answers: a relationship write calls `retract_adj_tables`, which drops the
    /// type's table outright, so the next read rebuilds and is correct whatever
    /// epoch it asked for. Retraction masks the fault the memo could introduce.
    ///
    /// That is precisely why this exists. A canary that cannot bite is not
    /// evidence, and the two query-level attempts at one — a token-blind memo
    /// run against interleaved reads and writes — both passed. Read the epochs
    /// directly and the keying is observable in one line.
    pub fn adjacency_epoch_for_test(&self, type_tokens: &[u32]) -> u64 {
        self.adjacency_epoch(&Some(type_tokens.to_vec()))
    }

    /// Restore projecting EVERY group of an aggregating ORDER BY + LIMIT
    /// projection before the tail truncates — the A/B arm for
    /// `agg_topk_before_project`. Both arms sort with the same comparator and
    /// the same stable tie rule, so the rows are byte-identical; the
    /// differential measures the per-group projection work alone (ic6:
    /// 9,599 groups × 3 expression evaluations to keep ten rows).
    pub fn set_agg_topk_before_project(&self, on: bool) {
        self.agg_topk_before_project.set(on);
    }

    /// See [`Graph::set_agg_topk_before_project`].
    pub(crate) fn agg_topk_before_project(&self) -> bool {
        self.agg_topk_before_project.get()
    }

    /// Restore enumerating a `MATCH … RETURN <literals/params> [SKIP] [LIMIT]`
    /// statement in source order — the A/B arm for `const_projection_fold`.
    /// Every row such a statement emits is the same constant row, so the
    /// output is fixed by the match COUNT alone and the fold answers it
    /// through the count-only join reorder; OFF, the pattern is walked as
    /// written, which for LSQB q3's existence probe is cubic in the persons
    /// per country (SF0.1 2 s, SF1 180 s, against a 4.5 s count).
    pub fn set_const_projection_fold(&self, on: bool) {
        self.const_projection_fold.set(on);
    }

    /// See [`Graph::set_const_projection_fold`].
    pub(crate) fn const_projection_fold(&self) -> bool {
        self.const_projection_fold.get()
    }


    /// Restore the pre-fix cardinality estimation: every `count_hop` with a
    /// labelled end WALKS the smaller label — 2M nodes for `(:Comment)` at SF1,
    /// through the degree table or the adjacency table — on every call.
    ///
    /// The A/B arm for the hop-count memo. The planner asks the same handful of
    /// (labels, dir, types) questions on every plan build, LSQB q2's two-path
    /// pattern asks ~16 of them across THREE builds of one statement, and each
    /// answer is identical until a relationship of those types or a membership
    /// of those labels changes — which is exactly the pair of clocks the memo
    /// keys on. OFF, q2 pays ~1 s of estimation PER EXECUTION.
    pub fn set_hop_count_memo(&self, on: bool) {
        self.hop_count_memo_on.set(on);
    }

    /// The estimator's sample budget — see `count_hop_estimate`. Settable so a
    /// small-fixture canary can force the sampled path; clamped to 1.
    pub fn set_estimate_sample_budget(&self, n: usize) {
        self.estimate_sample_budget.set(n.max(1));
    }

    /// A hop count for the PLANNER: `count_hop`'s question, answered by a
    /// deterministic stride-sample when the walked label is large.
    ///
    /// # Why this exists beside `count_hop`
    ///
    /// `count_hop` also answers real `count(*)` queries (`try_count_fast`), so
    /// it must stay exact. The planner's `hop_fanout` only needs a FLOAT — an
    /// ordering decision turns on orders of magnitude — and LSQB is
    /// single-shot: the hop-count MEMO removes repeat walks, but a first
    /// execution still paid ~8 unique 2M-node walks (~450 ms of q2's total)
    /// that Neo4j's maintained statistics never pay. Sampling bounds the first
    /// call too: a stride over the membership view, scaled back up, ~4K probes
    /// instead of 2M.
    ///
    /// Deterministic BY CONSTRUCTION — a fixed stride from a sorted view, no
    /// randomness — so two calls agree, plans are stable, and the determinism
    /// gate holds. Memoised in `hop_estimate_memo`, a SEPARATE map from the
    /// exact one: an estimate served where an answer was asked would be a
    /// correctness bug, and two maps make the confusion unrepresentable.
    pub fn count_hop_estimate(
        &self,
        start_labels: &[String],
        dir: Dir,
        types: &[String],
        end_labels: &[String],
    ) -> Result<u64, GraphError> {
        debug_assert!(!matches!(dir, Dir::Both), "undirected hops double-count");
        // Both ends free: `count_hop`'s stats path is already O(1) and exact.
        if start_labels.is_empty() && end_labels.is_empty() {
            return self.count_hop(start_labels, dir, types, end_labels);
        }
        let mut type_tokens: Option<Vec<u32>> = None;
        if !types.is_empty() {
            let mut v = Vec::with_capacity(types.len());
            for t in types {
                if let Some(tok) = self.token_peek("typ:", &self.types, t) {
                    v.push(tok);
                }
            }
            if v.is_empty() {
                return Ok(0); // no named type was ever minted
            }
            v.sort_unstable();
            type_tokens = Some(v);
        }
        let use_memo = self.hop_count_memo_on.get() && !self.in_txn_with_writes();
        let dir_b = match dir {
            Dir::Out => 0u8,
            Dir::In => 1,
            Dir::Both => 2,
        };
        let key: HopCountKey = (
            start_labels.to_vec(),
            dir_b,
            types.to_vec(),
            end_labels.to_vec(),
        );
        let adj_epoch = self.adjacency_epoch(&type_tokens);
        let label_epoch = self.labels_epoch_max(start_labels, end_labels);
        if use_memo {
            if let Some(e) = self.hop_estimate_memo.borrow().get(&key) {
                if e.adj_epoch == adj_epoch && e.label_epoch == label_epoch {
                    counted!("graph.hop count estimate served from the memo");
                    return Ok(e.count);
                }
            }
        }
        // Iterate the smaller labelled side, exactly as `count_hop` orients.
        let start_members = if start_labels.is_empty() {
            None
        } else {
            Some(self.members_all(start_labels)?)
        };
        let end_members = if end_labels.is_empty() {
            None
        } else {
            Some(self.members_all(end_labels)?)
        };
        let iterate_start = match (&start_members, &end_members) {
            (Some(a), Some(b)) => a.len() <= b.len(),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => unreachable!("handled above"),
        };
        let (iter, walk_dir, far) = if iterate_start {
            (start_members.as_ref().expect("labelled"), dir, end_members.as_ref())
        } else {
            let flipped = match dir {
                Dir::Out => Dir::In,
                Dir::In => Dir::Out,
                Dir::Both => Dir::Both,
            };
            (end_members.as_ref().expect("labelled"), flipped, start_members.as_ref())
        };
        let n = iter.len();
        let budget = self.estimate_sample_budget.get().max(1);
        let count = if n <= budget {
            // Small enough to answer exactly — and the exact path's own memo
            // then serves any `count(*)` that asks the same question.
            return self.count_hop(start_labels, dir, types, end_labels);
        } else {
            let stride = n.div_ceil(budget);
            let mut sampled: u64 = 0;
            let mut sum: u64 = 0;
            for node in iter.iter().step_by(stride) {
                sampled += 1;
                match far {
                    // The far side is a membership TEST (§11), never a
                    // materialised set — `count_hop` builds a BTreeSet of up
                    // to 2M ids here, which is itself most of a walk's cost.
                    Some(fm) => {
                        self.adjacent_slim_for_each(node, walk_dir, &type_tokens, |e| {
                            if self.members_contains(fm, e.peer) {
                                sum += 1;
                            }
                        });
                    }
                    None => sum += self.count_adjacent_memo(node, walk_dir, &type_tokens),
                }
            }
            sometimes!("graph.hop count estimate sampled the label", true);
            counted!("graph.hop count estimated by sampling");
            u64::try_from(u128::from(sum) * n as u128 / u128::from(sampled.max(1)))
                .unwrap_or(u64::MAX)
        };
        if use_memo {
            let mut m = self.hop_estimate_memo.borrow_mut();
            if m.len() >= HOP_COUNT_MEMO_MAX {
                m.clear();
            }
            m.insert(
                key,
                HopCountEntry {
                    adj_epoch,
                    label_epoch,
                    count,
                },
            );
        }
        Ok(count)
    }

    /// Base probes after which a membership base is answered from a presence
    /// bitmap; `0` never builds one.
    ///
    /// The threshold is a probe count, not a size, because a big label that a
    /// workload barely touches must not pay a build. See
    /// `MembersView::contains_with`.
    pub fn set_members_bitmap_after(&self, n: usize) {
        self.members_bitmap_after.set(n);
    }

    /// Whether a single-node reader whose node the staleness check could NOT
    /// clear declines the table and walks its own span.
    ///
    /// Off, it repairs: it re-reads a row for every node any writer touched
    /// since the table was built, carries an overlay, and publishes — all to
    /// answer about ONE node. That cost is proportional to the write stream,
    /// so under a mixed load it grows exactly when it can least be afforded,
    /// and it is paid per read.
    ///
    /// On, it falls back to the direct prefix walk of its own node's span. The
    /// walk is the same truth read from the store and it already exists — it
    /// is what a reader declined by `set_reader_rebuild_admission` does — so
    /// this extends a policy rather than inventing one. Republishing becomes
    /// the maintenance pass's job, which is what §5.3 made it.
    ///
    /// The arms must answer IDENTICALLY: a table is a performance structure
    /// and the walk is the same rows in the same order. The differential's bar
    /// is byte-identical adjacency under a concurrent write stream, with a
    /// counter half showing the decline actually fired.
    pub fn set_single_node_stale_walk(&self, on: bool) {
        self.single_node_stale_walk.set(on);
    }

    /// Whether the derived bases are PERSISTED (§5.4): written to a sidecar
    /// beside the segments after a compaction publishes them, and adopted from
    /// one at open.
    ///
    /// Without this, §5.2 and §5.3 make a healthy server stop rebuilding
    /// *during* a process's life and do nothing for its START — a cold SF1
    /// paged server pays ~43.2 s walking 17.26M adjacency rows plus the
    /// membership bases before it answers.
    ///
    /// A sidecar is only ever adopted when its vintage still holds: the sealed
    /// set must be exactly the one it was produced from, the body must hash to
    /// its header, and the store's clock must not have moved past its stamp.
    /// Any of those failing costs a REBUILD, which is the behaviour without
    /// this item — the failure degrades, it does not lie. Off is the
    /// differential arm.
    /// Whether the derived bases are PERSISTED (§5.4): written to a sidecar
    /// beside the segments after a compaction publishes them, and adopted from
    /// one at open. Off is the differential arm.
    pub fn set_persist_derived(&self, on: bool) {
        self.persist_derived.set(on);
    }

    /// Minimum seconds between two sidecar writes whose only reason is that
    /// more bases were published on an unchanged sealed set (default 600). A
    /// sealed-set move always writes at once. Zero means "every tick", which
    /// is what a test wants and what production must not have: v84 rewrote a
    /// 1.3 GB file nine times in four minutes at zero.
    pub fn set_persist_growth_interval(&self, secs: u64) {
        self.persist_growth_interval_secs
            .store(secs, std::sync::atomic::Ordering::Relaxed);
    }

    /// §7 — PRECISION LOCKING. Whether commit validates this transaction's
    /// node-pattern PREDICATES against the rows committed since its snapshot,
    /// in addition to validating its read set.
    ///
    /// **Default OFF, deliberately.** It closes phantoms, which is an
    /// isolation UPGRADE — `docs/concurrency-direction.md` lists phantoms as a
    /// known limitation today — and it is still a behaviour change: it makes
    /// the engine abort statements that currently commit. The plan's sequence
    /// is a full TCK pass and a soak on both arms before this flips, and
    /// flipping `set_read_set_bindings_only` on is downstream of it (narrowing
    /// the recorded set admits an anti-dependency on a predicate, and this is
    /// what makes that predicate checked rather than assumed).
    ///
    /// Coverage is incremental: an unbound scan whose pattern is a label set
    /// plus row-independent property equalities is checked; anything else
    /// keeps today's read-set-only rule. Absent coverage can only admit the
    /// anomaly the engine already admits, so partial coverage is sound.
    pub fn set_precision_locking(&self, on: bool) {
        self.precision_locking.set(on);
    }

    /// Whether precision locking is on — the retry loop and the tests both
    /// need to know which guarantee is in force.
    pub fn precision_locking_enabled(&self) -> bool {
        self.precision_locking.get()
    }

    /// §7 — register the restriction an unbound scan's node pattern imposes.
    ///
    /// Silently records nothing when the pattern is beyond
    /// `Restriction::extract`, which is the fallback the whole design rests on:
    /// an unrepresentable predicate keeps read-set validation and nothing
    /// claims otherwise.
    pub(crate) fn note_restriction(
        &self,
        pat: &engram_cypher::stmt::NodePattern,
        params: &BTreeMap<String, Value>,
    ) {
        if !self.precision_locking.get() {
            return;
        }
        let Some(r) = crate::precision::Restriction::extract(self, pat, params) else {
            counted!("graph.a pattern was beyond the restriction extractor");
            sometimes!("graph.a predicate fell back to read-set validation", true);
            return;
        };
        TXN_TOUCHED.with(|t| {
            if let Some(touched) = t.borrow_mut().as_mut() {
                // Deduplicated: a correlated MATCH re-runs the same pattern
                // once per driving row, and a guard holding thousands of
                // identical restrictions would test each of them against every
                // changed row, under the commit latch.
                if !touched.restrictions.contains(&r) {
                    counted!("graph.predicate restrictions recorded");
                    touched.restrictions.push(r);
                }
            }
        });
    }

    /// The honest test forcing: with the columnar scans off, a query that
    /// would take one takes the general path instead — same answer, so a
    /// differential test proves equivalence.
    pub fn set_columnar_scans(&self, enabled: bool) {
        self.columnar_scans.set(enabled);
    }

    /// Whether the columnar paths are on. Off inside a transaction that has
    /// buffered writes: the columnar scans read the committed store's columns
    /// directly and would not see the transaction's own rows; the row paths
    /// they decline to overlay them.
    pub(crate) fn columnar_scans_enabled(&self) -> bool {
        self.columnar_scans.get() && !self.in_txn_with_writes()
    }

    /// Toggle the columnar-aggregate member-scan batching (default on).
    pub fn set_columnar_agg_batch(&self, enabled: bool) {
        self.columnar_agg_batch.set(enabled);
    }

    pub(crate) fn columnar_agg_batch_enabled(&self) -> bool {
        self.columnar_agg_batch.get()
    }

    /// Override the columnar-aggregate member-batch size (a test lowers it to force
    /// a multi-batch split); `0` restores the default.
    pub fn set_columnar_agg_batch_size(&self, n: usize) {
        *self.columnar_agg_batch_size.borrow_mut() = n;
    }

    pub(crate) fn columnar_agg_batch_size(&self) -> usize {
        let n = *self.columnar_agg_batch_size.borrow();
        if n == 0 {
            crate::batch::COLUMNAR_AGG_BATCH
        } else {
            n
        }
    }

    /// The honest test forcing: with the native-key aggregate fast path off, a
    /// single-primitive-key group-by takes the general `agg_key_of` path
    /// instead — same groups in the same first-seen order, so a differential
    /// test proves the two are byte-identical.
    pub fn set_agg_native_key(&self, enabled: bool) {
        self.agg_native_key.set(enabled);
    }

    /// Whether the single-primitive-key aggregate fast path is on.
    pub(crate) fn agg_native_key_enabled(&self) -> bool {
        self.agg_native_key.get()
    }

    /// The honest test forcing: with the degree short-circuit off, a
    /// `count(src) GROUP BY dst` aggregate builds the full chunk and reduces it
    /// instead — same groups, so a differential test proves equivalence.
    pub fn set_degree_aggregate(&self, enabled: bool) {
        self.degree_aggregate.set(enabled);
    }

    /// Whether the degree short-circuit is on.
    pub(crate) fn degree_aggregate_enabled(&self) -> bool {
        self.degree_aggregate.get()
    }

    /// The honest test forcing: with IC2's ordered k-way-merge fast path off, the
    /// query runs the ordinary expand + gather + top-k instead — same rows, so a
    /// differential test proves the two are byte-identical.
    pub fn set_ic2_ordered(&self, enabled: bool) {
        self.ic2_ordered.set(enabled);
    }

    /// Whether IC2's date-ordered k-way-merge fast path is on.
    pub(crate) fn ic2_ordered_enabled(&self) -> bool {
        self.ic2_ordered.get()
    }

    /// Enable morsel-parallel `expand` (default OFF — see the field docs). The
    /// A/B differential toggles it to prove parallel == serial byte-identical.
    pub fn set_parallel_expand(&self, enabled: bool) {
        self.parallel_expand.set(enabled);
    }

    /// Whether morsel-parallel `expand` is on.
    pub(crate) fn parallel_expand_enabled(&self) -> bool {
        self.parallel_expand.get()
    }

    /// Force-build and cache the adjacency table(s) for `dir` × `type_tokens` at
    /// the current epoch, so a subsequent morsel-parallel scan reads them
    /// lock-free (arc-swap) rather than each worker redundantly rebuilding on a
    /// concurrent miss. A no-op if the table is absent-by-design (declined) — the
    /// workers then fall through to the direct prefix scan, still correct.
    pub(crate) fn warm_adjacency(&self, dir: Dir, type_tokens: &Option<Vec<u32>>) {
        let epoch = self.store.now_ts();
        let sides: &[u8] = match dir {
            Dir::Out => b"O",
            Dir::In => b"I",
            Dir::Both => b"OI",
        };
        for &tag in sides {
            self.with_adj_table(tag, type_tokens, epoch, true, None, |_| {});
        }
    }

    /// The honest test forcing: with IC11's semijoin fast path off, the query runs
    /// the ordinary multistage expand + country filter instead — same rows, so a
    /// differential test proves the two are byte-identical.
    pub fn set_ic11_semijoin(&self, enabled: bool) {
        self.ic11_semijoin.set(enabled);
    }

    /// Whether IC11's anchored-endpoint semijoin fast path is on.
    pub(crate) fn ic11_semijoin_enabled(&self) -> bool {
        self.ic11_semijoin.get()
    }

    /// The honest test forcing: with BI7's rollup off, the query runs the ordinary
    /// 2-hop expand + reduce instead — same rows, so a differential test proves the
    /// two are byte-identical.
    pub fn set_bi7_rollup(&self, enabled: bool) {
        self.bi7_rollup.set(enabled);
    }

    /// Toggle IC3's date-windowed HAS_CREATOR seek (default on).
    pub fn set_ic3_datewindow(&self, enabled: bool) {
        self.ic3_datewindow.set(enabled);
    }

    pub(crate) fn ic3_datewindow_enabled(&self) -> bool {
        self.ic3_datewindow.get()
    }

    /// The honest test forcing: with the sorted-CSR edge probe off,
    /// `edge_count_slim` walks the whole adjacency row instead of
    /// binary-searching it — same count, so a differential test proves the
    /// two are identical. Default on.
    pub fn set_edge_probe(&self, enabled: bool) {
        self.edge_probe.set(enabled);
    }

    /// Whether BI7's 2-hop count-rollup fast path is on.
    pub(crate) fn bi7_rollup_enabled(&self) -> bool {
        self.bi7_rollup.get()
    }

    /// Cap the intermediate rows one statement may materialise. `None`
    /// (the default) means unbounded. The interpreter materialises row sets
    /// rather than streaming them, so a cartesian product or an unbounded
    /// expansion does not get slow — it exhausts memory and the OOM killer
    /// takes the PROCESS. The full-DB port benchmark died exactly that way.
    /// With a budget set, such a statement REFUSES with a named error
    /// instead, which is a property of the engine a caller can rely on.
    pub fn set_row_budget(&self, budget: Option<usize>) {
        self.row_budget.set(budget);
    }

    /// The configured row budget, if any.
    pub fn row_budget(&self) -> Option<usize> {
        self.row_budget.get()
    }

    /// Set the wall clock the temporal constructors read (milliseconds
    /// since the epoch). Time is injected, never ambient.
    pub fn set_wall_ms(&self, ms: i64) {
        self.wall_ms.set(Some(ms));
    }

    /// The injected wall clock, if any.
    pub fn wall_ms(&self) -> Option<i64> {
        self.wall_ms.get()
    }

    /// Install a timezone-rules provider. Without one, only UTC and the
    /// fixed Etc/GMT family resolve; named IANA zones refuse by name.
    pub fn set_zone_provider(&self, p: std::sync::Arc<dyn engram_cypher::ZoneProvider>) {
        *self.zone_provider.borrow_mut() = Some(p);
    }

    /// The installed provider, if any.
    pub fn zone_provider(&self) -> Option<std::sync::Arc<dyn engram_cypher::ZoneProvider>> {
        self.zone_provider.borrow().clone()
    }

    /// Set the exact-vs-ANN crossover (see `vector_exact_max` on the struct).
    pub fn set_vector_exact_max(&self, n: usize) {
        self.vector_exact_max.set(n);
    }

    /// The ANN staleness signal: the STORE's commit clock, so the signal is
    /// shared across every Graph handle over one store — a write through any
    /// session invalidates every session's cache. Coarse on purpose: a stale
    /// approximate index returning a deleted or outdated row is a
    /// correctness bug, and read-heavy retrieval pays nothing.
    pub fn mutation_epoch(&self) -> u64 {
        self.store.now_ts()
    }

    /// This graph's realm and namespace — its federation coordinate.
    pub fn realm(&self) -> Realm {
        self.nodes.realm
    }

    /// This graph's namespace.
    pub fn namespace(&self) -> Namespace {
        self.nodes.namespace
    }

    /// The underlying store — what a second Graph handle shares.
    pub fn shared_store(&self) -> Store {
        self.store.clone()
    }

    // ── The catalog ─────────────────────────────────────────────────────

    fn token(
        &self,
        family: &'static str,
        cache: &arc_swap::ArcSwap<BTreeMap<String, u32>>,
        name: &str,
    ) -> Result<u32, GraphError> {
        // Fast path: lock-free load. This is on EVERY query (label/prop
        // resolution), so a Mutex/RwLock here serialised the read path.
        if let Some(t) = cache.load().get(name) {
            return Ok(*t);
        }
        // Miss. Take the alloc latch and RE-CHECK (double-checked): another
        // thread may mint the same new name concurrently, and a duplicate token
        // would corrupt every row that later names it by number. The latch makes
        // "look up, else mint" atomic; the fast cache-hit path above stays
        // lock-free, so only genuinely-new names serialize.
        let _alloc = self.alloc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = cache.load().get(name) {
            return Ok(*t); // minted by whoever won the latch first
        }
        let mut body = family.as_bytes().to_vec();
        body.extend_from_slice(name.as_bytes());
        if let Some(bytes) = self.store.get(&self.kv, &body) {
            let t = u32::from_le_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| GraphError::Corrupt("token width".into()))?,
            );
            cache.rcu(|old| {
                let mut new: BTreeMap<String, u32> = (**old).clone();
                new.insert(name.to_string(), t);
                std::sync::Arc::new(new)
            });
            return Ok(t);
        }
        // Mint, under the alloc latch taken above: the counter read-modify-write
        // is atomic against other threads (the D2-revision replacement for the
        // old "one shard, no await between" argument, which real threads break).
        let counter_body = format!("next:{family}").into_bytes();
        let next = match self.store.get(&self.kv, &counter_body) {
            Some(b) => u32::from_le_bytes(
                b.as_slice()
                    .try_into()
                    .map_err(|_| GraphError::Corrupt("counter width".into()))?,
            ),
            None => FIRST_USER_TOKEN,
        };
        // Token mints ALWAYS log, even in bulk mode: a token is shared
        // infrastructure that later LOGGED writes will reference by number.
        // A replay that lost the mint would corrupt every surviving row
        // naming it — so mints stay outside the bulk funnel.
        self.store
            .put(
                &self.kv,
                &counter_body,
                StoredValue::Plain((next + 1).to_le_bytes().to_vec()),
            )
            .map_err(GraphError::Store)?;
        self.store
            .put(
                &self.kv,
                &body,
                StoredValue::Plain(next.to_le_bytes().to_vec()),
            )
            .map_err(GraphError::Store)?;
        // The reverse row, for rendering tokens back to names.
        let mut rev = format!("rev:{family}").into_bytes();
        rev.extend_from_slice(&next.to_le_bytes());
        self.store
            .put(&self.kv, &rev, StoredValue::Plain(name.as_bytes().to_vec()))
            .map_err(GraphError::Store)?;
        cache.rcu(|old| {
            let mut new: BTreeMap<String, u32> = (**old).clone();
            new.insert(name.to_string(), next);
            std::sync::Arc::new(new)
        });
        counted!("graph.tokens minted");
        Ok(next)
    }

    fn token_name(&self, family: &'static str, token: u32) -> Result<String, GraphError> {
        if let Some(name) = self.rev_names.load().get(&(family, token)) {
            return Ok(name.clone());
        }
        let mut rev = format!("rev:{family}").into_bytes();
        rev.extend_from_slice(&token.to_le_bytes());
        let bytes = self
            .store
            .get(&self.kv, &rev)
            .ok_or_else(|| GraphError::Corrupt(format!("unnamed {family} token {token}")))?;
        let name =
            String::from_utf8(bytes).map_err(|_| GraphError::Corrupt("token name utf8".into()))?;
        self.rev_names.rcu(|old| {
            let mut new = (**old).clone();
            new.insert((family, token), name.clone());
            std::sync::Arc::new(new)
        });
        Ok(name)
    }

    fn next_id(&self, what: &str) -> Result<u64, GraphError> {
        // Serialize the counter read-modify-write against other threads: the
        // latch is uncontended (and event-free) on the single-threaded path, so
        // the trace is unchanged, but two concurrent inserts can no longer read
        // the same counter and mint a duplicate id. Held only for the mint.
        let _alloc = self.alloc.lock().unwrap_or_else(|e| e.into_inner());
        // Reserve a RANGE instead of minting one id per entity: one counter
        // write per `range` ids.
        //
        // Bulk mode has always done this (4096 at a time, unlogged). Serving
        // did not, and paid for it: the non-bulk path below holds this global
        // `alloc` mutex across a FULL LOGGED `store.put` — which itself takes
        // the commit-log mutex, allocates a timestamp, BLAKE3-hashes, writes
        // the WAL buffer, takes a tail shard latch and spins on the visibility
        // barrier. `CREATE (a)-[:R]->(b)` pays that three times, and every OCC
        // retry re-mints DURABLY: measured at 1.80 `alloc` acquisitions per
        // acked op with OCC on against 1.00 with it off, i.e. ~0.8 wasted
        // durable mints per write.
        //
        // The serving reservation is LOGGED (`store.put`), unlike bulk's
        // `put_unlogged`: the WAL's contract is that replay restores every
        // acknowledged write, and a counter advanced only in memory would let
        // a replayed store re-mint ids it had already handed out. One log
        // write per `range` ids, not one per id.
        //
        // The counter row always holds the reserved END, so a crash abandons
        // the unused tail as GAPS and ids are never reused. Ids stay dense
        // WITHIN a run — the first is still 1 — so only a restart shows a gap.
        let bulk = self.bulk_ingest.get();
        let range: u64 = if bulk {
            BULK_ID_RANGE
        } else {
            self.id_reservation.get() as u64
        };
        if range > 1 {
            let mut res = self.id_reservations.borrow_mut();
            if let Some((next, end)) = res.get_mut(what) {
                if next < end {
                    let id = *next;
                    *next += 1;
                    counted!("graph.id served from a reservation");
                    return Ok(id);
                }
            }
            let body = format!("next:{what}").into_bytes();
            let start = match self.store.get(&self.kv, &body) {
                Some(b) => u64::from_le_bytes(
                    b.as_slice()
                        .try_into()
                        .map_err(|_| GraphError::Corrupt("id width".into()))?,
                ),
                None => 1,
            };
            // Id allocation NEVER buffers into a write transaction: the counter
            // is a monotonic allocator shared across sessions, so a buffered
            // bump would let a concurrent transaction read the same committed
            // counter and mint a duplicate id, and a rollback would recycle ids
            // the aborted work already handed out. It autocommits.
            let end_value = StoredValue::Plain((start + range).to_le_bytes().to_vec());
            if bulk {
                // The bulk contract: durability by re-ingest, so the counter
                // rides the same unlogged path as the entities it numbers.
                self.store
                    .put_unlogged(&self.kv, &body, end_value)
                    .map_err(GraphError::Store)?;
            } else {
                // Serving: LOGGED, so a replayed store cannot re-mint ids it
                // already handed out.
                self.store
                    .put(&self.kv, &body, end_value)
                    .map_err(GraphError::Store)?;
            }
            res.insert(what.to_string(), (start + 1, start + range));
            counted!("graph.id reservations minted");
            return Ok(start);
        }
        let body = format!("next:{what}").into_bytes();
        let next = match self.store.get(&self.kv, &body) {
            Some(b) => u64::from_le_bytes(
                b.as_slice()
                    .try_into()
                    .map_err(|_| GraphError::Corrupt("id width".into()))?,
            ),
            None => 1,
        };
        self.store
            .put(
                &self.kv,
                &body,
                StoredValue::Plain((next + 1).to_le_bytes().to_vec()),
            )
            .map_err(GraphError::Store)?;
        Ok(next)
    }

    // ── Mutations ───────────────────────────────────────────────────────

    /// The vector indexes, cached. The write path consults this on every
    /// node mutation, so it must not scan the schema each time.
    fn vector_indexes(&self) -> Result<std::sync::Arc<Vec<VecIndex>>, GraphError> {
        if let Some(list) = self.vector_index_list.borrow().as_ref() {
            return Ok(std::sync::Arc::clone(list));
        }
        let rc = std::sync::Arc::new(self.scan_vector_indexes());
        *self.vector_index_list.borrow_mut() = Some(std::sync::Arc::clone(&rc));
        Ok(rc)
    }

    /// Drop the cached vector-index list — a schema change may have moved it.
    pub(crate) fn invalidate_vector_indexes(&self) {
        *self.vector_index_list.borrow_mut() = None;
    }

    /// Record, for every vector index whose label this node carries, that
    /// its embedding was written (an upsert) or is absent (a delete). Cheap
    /// when there are no vector indexes.
    fn note_vector_write(&self, id: u64, labels: &[String], props: &BTreeMap<String, Value>) {
        let list = match self.vector_indexes() {
            Ok(l) if !l.is_empty() => l,
            _ => return,
        };
        // Inside a transaction the change is REMEMBERED, not published — see
        // `TxnTouched::vectors`. The per-index resolution happens here, where
        // the buffered labels and properties are in hand.
        if self.in_txn() {
            self.txn_touch(|t| {
                for vi in list.iter() {
                    if !labels.iter().any(|l| l == &vi.label) {
                        continue;
                    }
                    let has_vec = matches!(props.get(&vi.prop), Some(Value::List(_)));
                    t.vectors.entry(vi.name.clone()).or_default().push((id, has_vec));
                }
            });
            return;
        }
        let mut deltas = self.vector_deltas.borrow_mut();
        for vi in list.iter() {
            if !labels.iter().any(|l| l == &vi.label) {
                continue; // the node is not in this index
            }
            let has_vec = matches!(props.get(&vi.prop), Some(Value::List(_)));
            let d = deltas.entry(vi.name.clone()).or_default();
            if has_vec {
                d.note_upsert(id);
            } else {
                d.note_delete(id);
            }
        }
    }

    // ── Source change logs — the write side of `derived.rs` ─────────────────
    //
    // Every mutation site records WHAT changed, stamped with the commit clock
    // read AFTER the rows it describes were written, into the log of the
    // source it changed. Readers catch up from these logs and never consume
    // them; the source's epoch is the log's epoch.
    //
    // A write buffered in a TRANSACTION is not committed and must not reach
    // any log yet — a reader on another session would apply a change it cannot
    // see in the store. It is remembered instead (`txn_touch`) and applied at
    // COMMIT as a `touch` stamped with the commit clock, which sends every
    // snapshot older than the commit to a rebuild. The previous design marked
    // the delta overflowed at NOTE time, stamped BEFORE the commit: a snapshot
    // rebuilt in that window carried a stamp at or past the change it did not
    // contain, and was judged current for ever after. Touching at commit is
    // what closes that.

    /// The epoch of a label's membership: the stamp of its newest change.
    fn label_epoch(&self, token: u32) -> u64 {
        if self.label_epoch_atomics.get() {
            return self
                .label_epoch_map
                .load()
                .get(&token)
                .map_or(0, |c| c.load(std::sync::atomic::Ordering::Acquire));
        }
        self.label_log.borrow().get(&token).map_or(0, ChangeLog::epoch)
    }

    /// Raise a label's epoch to `now`.
    ///
    /// A `fetch_max`, not a store: epochs are monotone and two writers may
    /// stamp out of order. Mirrors `bump_adj_epoch`.
    fn bump_label_epoch(&self, token: u32, now: u64) {
        use std::sync::atomic::Ordering::AcqRel;
        if let Some(c) = self.label_epoch_map.load().get(&token) {
            c.fetch_max(now, AcqRel);
            return;
        }
        let fresh = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now));
        self.label_epoch_map.rcu(|old| {
            if let Some(c) = old.get(&token) {
                c.fetch_max(now, AcqRel);
                return std::sync::Arc::clone(old);
            }
            let mut new = (**old).clone();
            new.insert(token, std::sync::Arc::clone(&fresh));
            std::sync::Arc::new(new)
        });
    }

    /// The epoch of a property's values. `0` until an index exists for it —
    /// nothing derives from an unindexed property, so nothing is logged.
    fn prop_epoch(&self, token: u32) -> u64 {
        self.prop_log.borrow().get(&token).map_or(0, ChangeLog::epoch)
    }

    /// The epoch of the adjacency a table over `type_tokens` derives from: the
    /// newest change to any of those types, or to any type at all for the
    /// untyped table.
    fn adjacency_epoch(&self, type_tokens: &Option<Vec<u32>>) -> u64 {
        if !self.incremental_caches.get() {
            // pre-fix: any write invalidates every adjacency-derived structure
            return self.store.now_ts();
        }
        match type_tokens {
            None => self.adj_epoch_now(),
            Some(ts) => {
                // One `ArcSwap` guard + a walk of a map holding one entry per
                // relationship type. A thread-local memo of the counter CELLS
                // (v60, `adj_epoch_value`) sat here until P-6 measured it:
                // ON was SLOWER on every heavy LSQB query (q9 +20.6%, q3
                // +15.5%, q8 +15.2%, q5 +9.2%; N=3 interleaved, throttle
                // flat) — the way-scan cost more than the map walk it saved,
                // exactly as the family's first cut (the Arc-clone version)
                // had. Deleted, not just defaulted off: unmeasured complexity
                // on the hottest read path is debt, and this was measured.
                let m = self.adj_type_epoch.load();
                ts.iter()
                    .map(|t| {
                        m.get(t)
                            .map_or(0, |c| c.load(std::sync::atomic::Ordering::Acquire))
                    })
                    .max()
                    .unwrap_or(0)
            }
        }
    }

    /// The clock of the newest relationship change of any type.
    fn adj_epoch_now(&self) -> u64 {
        self.adj_epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise the global and the per-type adjacency epochs to `now` — AFTER the
    /// change is logged, so a reader that sees the epoch move finds the entry
    /// it must apply. A max, not a store: see `adj_type_epoch`.
    fn bump_adj_epoch(&self, type_token: u32, now: u64) {
        use std::sync::atomic::Ordering::AcqRel;
        self.adj_epoch.fetch_max(now, AcqRel);
        if let Some(c) = self.adj_type_epoch.load().get(&type_token) {
            c.fetch_max(now, AcqRel);
            return;
        }
        let fresh = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now));
        self.adj_type_epoch.rcu(|old| {
            if let Some(c) = old.get(&type_token) {
                c.fetch_max(now, AcqRel);
                return std::sync::Arc::clone(old);
            }
            let mut new = (**old).clone();
            new.insert(type_token, std::sync::Arc::clone(&fresh));
            std::sync::Arc::new(new)
        });
    }

    /// Record WHAT changed about an indexed property.
    ///
    /// `key` is `None` when the row leaves the index: the property was removed,
    /// the entity was deleted, or the value is one the index cannot order.
    ///
    /// A property has a log only once an index for it has been built
    /// (`ensure_range_index` creates it BEFORE scanning, so no write can land
    /// between the scan and the log). Writes to a property nothing derives
    /// from record nothing — the overwhelmingly common case, and the reason
    /// this costs an unindexed write one map lookup rather than an allocation.
    ///
    /// `ts` is the commit ts of the row write this describes (ignored while
    /// a transaction is installed — the commit stamps the replay).
    fn note_prop_change(
        &self,
        token: u32,
        body: &[u8],
        key: Option<engram_store::IndexKey>,
        ts: u64,
    ) {
        if self.in_txn() {
            // ALWAYS carried, even for a property with no log yet — unlike the
            // direct path's "record only into an existing log". The direct
            // path may skip because its write is COMMITTED: an index built
            // later scans the store and sees it. A buffered write is not in
            // the store, so if a probe builds this property's index during
            // the transaction (MERGE does exactly that), the build cannot see
            // the row and only the commit replay can deliver it — dropping
            // the entry here left the row out of the index for ever. The
            // replay still applies entries only where a log exists, so an
            // unindexed property's entries are carried and then discarded.
            self.txn_touch(|t| {
                t.props.entry(token).or_default().push((body.to_vec(), key));
            });
            return;
        }
        let poisoned = {
            let mut logs = self.prop_log.borrow_mut();
            match logs.get_mut(&token) {
                Some(log) => log.record(ts, (body.to_vec(), key)),
                None => false,
            }
        };
        if poisoned {
            self.retract_range_index(token);
        }
    }

    /// Remember a source a buffered transaction changed, to be touched at
    /// commit. A no-op outside a transaction.
    fn txn_touch(&self, f: impl FnOnce(&mut TxnTouched)) {
        TXN_TOUCHED.with(|t| {
            if let Some(t) = t.borrow_mut().as_mut() {
                f(t);
            }
        });
    }

    /// Read the active transaction's accumulated stats delta — the count
    /// changes its buffered writes will apply at commit. `None` outside a
    /// transaction; the delta is empty until the transaction writes.
    fn with_txn_stats<R>(&self, f: impl FnOnce(&StatsDelta) -> R) -> Option<R> {
        TXN_TOUCHED.with(|t| t.borrow().as_ref().map(|t| f(&t.stats)))
    }

    /// Apply a committed transaction's remembered changes to the logs, stamped
    /// with the transaction's commit ts `ts` — the one stamp every row of its
    /// write-set carries. Called by the transaction's commit path only,
    /// inside its write fence; a rollback drops the set unapplied because
    /// nothing changed.
    fn touch_after_commit(&self, touched: TxnTouched, ts: u64) {
        if touched.is_empty() {
            return;
        }
        // The count store first: the committed rows are visible, so the
        // counts must move with them. A store still awaiting its rebuild
        // ignores the delta (the rebuild sees the rows).
        if !touched.stats.is_empty() {
            if let Some(st) = self.stats.borrow_mut().as_mut() {
                touched.stats.apply(st);
            }
        }
        // The entries the transaction's writes would have logged had they
        // been direct, replayed now — stamped with the commit ts, so a
        // snapshot older than the commit applies them and one built after it
        // (which read the committed rows) skips them. Replaying a change onto
        // a snapshot that already holds it is harmless: a membership entry is
        // a set insert or remove, an index entry replaces one body's key, an
        // adjacency entry re-reads one node's row.
        //
        // A poisoned log (an invariant violation, see `ChangeLog::record`)
        // is answered AFTER the lock is released: the retract touches the
        // slot maps, never the logs.
        let mut poisoned_labels: Vec<u32> = Vec::new();
        let mut poisoned_props: Vec<u32> = Vec::new();
        let mut poisoned_adj: Vec<(u8, u32)> = Vec::new();
        {
            let mut l = self.label_log.borrow_mut();
            for (t, entries) in touched.labels {
                let log = l.entry(t).or_insert_with(|| ChangeLog::new(LABEL_LOG_CAP));
                for e in entries {
                    if log.record(ts, e) {
                        poisoned_labels.push(t);
                    }
                }
                // The TRANSACTIONAL publish path, and it needs the epoch bump
                // for exactly the same reason the autocommit one does — and it
                // is the path every buffered write takes. Without it a
                // transaction's membership change lands in the log while the
                // epoch stays behind, so a reader judges its snapshot current
                // and MISSES the change: a silent wrong answer, not a slow one.
                // Inside the critical section, as there.
                self.bump_label_epoch(t, ts);
            }
        }
        {
            let mut l = self.prop_log.borrow_mut();
            for (t, entries) in touched.props {
                if let Some(log) = l.get_mut(&t) {
                    for e in entries {
                        if log.record(ts, e) {
                            poisoned_props.push(t);
                        }
                    }
                }
            }
        }
        if !touched.adj.is_empty() {
            let types: std::collections::BTreeSet<u32> =
                touched.adj.keys().map(|(_, ty)| *ty).collect();
            {
                let mut l = self.adj_log.borrow_mut();
                for (k, entries) in touched.adj {
                    let log = l.entry(k).or_insert_with(|| ChangeLog::new(ADJ_LOG_CAP));
                    for e in entries {
                        // BEFORE the log entry, so the filter is never the
                        // staler of the two — see `AdjChangeFilter`.
                        self.adj_change_filter.note(k.0, e, ts);
                        if log.record(ts, e) {
                            poisoned_adj.push(k);
                        }
                    }
                }
            }
            for ty in types {
                self.bump_adj_epoch(ty, ts);
            }
        }
        for t in poisoned_labels {
            self.retract_members(t);
        }
        for t in poisoned_props {
            self.retract_range_index(t);
        }
        for (tag, ty) in poisoned_adj {
            self.retract_adj_tables(tag, ty);
        }
        if !touched.vectors.is_empty() {
            let mut deltas = self.vector_deltas.borrow_mut();
            for (name, ops) in touched.vectors {
                let d = deltas.entry(name).or_default();
                for (id, upsert) in ops {
                    if upsert {
                        d.note_upsert(id);
                    } else {
                        d.note_delete(id);
                    }
                }
            }
        }
        counted!("graph.transaction replayed its changes at commit");
    }

    /// The index key a property VALUE presents, or `None` when this index
    /// cannot order it. One conversion point, so a value that is indexable on
    /// the write path is indexable on the build path and vice versa.
    fn index_key_of(v: &Value) -> Option<engram_store::IndexKey> {
        encode_prop(v)
            .ok()
            .and_then(|tagged| engram_store::IndexKey::from_tagged(&tagged))
    }

    /// Record that `id` joined (or left) the membership of `tokens`.
    ///
    /// Also updates the ALL-NODES snapshot (`u32::MAX`), which is what an
    /// unlabelled `MATCH (n)` reads — forgetting it would leave that one path
    /// rebuilding while every labelled path caught up incrementally, which is
    /// the sort of partial fix that reads as done.
    fn note_membership(&self, id: u64, tokens: &[u32], added: bool, ts: u64) {
        self.note_membership_of(id, tokens, added, true, ts);
    }

    /// Record that a relationship of type `type_token` was created or deleted.
    ///
    /// Must be called by every site that writes or deletes an `adjacency_row`,
    /// and by nothing else — the adjacency tables are reused whenever these
    /// clocks have not passed their build clock, so a missed call serves a
    /// table that is missing an edge. There are exactly two such sites
    /// (`create_rel`, `delete_rel`); `delete_node` reaches them through
    /// `delete_rel` on its detach path.
    ///
    /// `ts` is the commit ts of the LATER of the two adjacency-row writes it
    /// describes — never a clock read before them, for the reason spelled out
    /// in `delete_node`: a stamp taken before the writes is one the writes
    /// advance past. Called inside the caller's write fence.
    fn note_adjacency_changed(&self, type_token: u32, src: u64, dst: u64, ts: u64) {
        if self.in_txn() {
            self.txn_touch(|t| {
                t.adj.entry((b'O', type_token)).or_default().push(src);
                t.adj.entry((b'I', type_token)).or_default().push(dst);
            });
            return;
        }
        // Exactly two rows moved: `src`'s OUT row and `dst`'s IN row — the same
        // two `adjacency_row` writes the caller just made. Each goes to the log
        // of the (side, type) it belongs to; a table over that side and type
        // replaces exactly those rows to catch up.
        let (poisoned_o, poisoned_i) = {
            let mut l = self.adj_log.borrow_mut();
            // BEFORE the log entries — see `AdjChangeFilter`.
            self.adj_change_filter.note(b'O', src, ts);
            self.adj_change_filter.note(b'I', dst, ts);
            let o = l
                .entry((b'O', type_token))
                .or_insert_with(|| ChangeLog::new(ADJ_LOG_CAP))
                .record(ts, src);
            let i = l
                .entry((b'I', type_token))
                .or_insert_with(|| ChangeLog::new(ADJ_LOG_CAP))
                .record(ts, dst);
            (o, i)
        };
        self.bump_adj_epoch(type_token, ts);
        if poisoned_o {
            self.retract_adj_tables(b'O', type_token);
        }
        if poisoned_i {
            self.retract_adj_tables(b'I', type_token);
        }
    }

    /// Whether adjacency tables may be used at all.
    ///
    /// A zero entry budget means "do not use a table" — not merely "do not
    /// build one". The distinction did not matter while any write invalidated
    /// every cached table, because setting the budget to zero was always
    /// preceded by a write that had already emptied the cache. Now that a
    /// table survives writes that do not touch adjacency, a cached table would
    /// go on serving a caller that asked for the per-node walk, and the
    /// budget knob would silently mean something narrower than it says.
    fn adj_tables_usable(&self) -> bool {
        if self.adj_table_max_entries.get() == 0 {
            // The SAME declaration `build_adj_table` makes when the data
            // exceeds a non-zero budget, because this is the same decision:
            // no table, fall back to the per-node walk. Declining here without
            // saying so silently retired the event for the zero-budget case,
            // and the simulation's coverage floor — which fails when a declared
            // state is never reached — caught it.
            sometimes!("graph.adjacency table declined by the entry budget", true);
            return false;
        }
        true
    }

    /// Record a membership change for `tokens`: one log entry per label, which
    /// is at once the change a snapshot catches up with and the epoch that says
    /// it must. The four mutation sites (`create_node`, `add_labels`,
    /// `remove_labels`, `delete_node`) all route here, so a site cannot record
    /// the one without the other — that is what makes "no entry newer than my
    /// snapshot" a proof the snapshot is good, rather than a rule to remember.
    ///
    /// `touch_all` distinguishes a change to the node's EXISTENCE from a change
    /// to its labels: `remove_labels` leaves the node in the graph, so the
    /// all-nodes snapshot (`u32::MAX`) is untouched, while create and delete
    /// must move it.
    ///
    /// `ts` is the commit ts of the last membership-row write this describes
    /// (`0`, ignored, while a transaction is installed). Called inside the
    /// caller's write fence.
    fn note_membership_of(&self, id: u64, tokens: &[u32], added: bool, touch_all: bool, ts: u64) {
        let all = if touch_all {
            Some(u32::MAX)
        } else {
            None
        };
        if self.in_txn() {
            self.txn_touch(|t| {
                for token in tokens.iter().copied().chain(all) {
                    t.labels.entry(token).or_default().push((id, added));
                }
            });
            return;
        }
        let mut poisoned: Vec<u32> = Vec::new();
        {
            let mut l = self.label_log.borrow_mut();
            for t in tokens.iter().copied().chain(all) {
                if l.entry(t)
                    .or_insert_with(|| ChangeLog::new(LABEL_LOG_CAP))
                    .record(ts, (id, added))
                {
                    poisoned.push(t);
                }
                // INSIDE the log's write critical section, deliberately. If the
                // epoch were raised after the guard dropped, a reader could see
                // an epoch that does not yet name an entry the log already
                // holds, judge its own snapshot current, and MISS the change —
                // the silent-wrong-answer direction. `record` folds backwards
                // stamps forward, so this stays monotone.
                self.bump_label_epoch(t, ts);
            }
        }
        for t in poisoned {
            self.retract_members(t);
        }
    }

    /// Record that a node left every vector index carrying one of `labels`
    /// (a delete, or a label removal that drops it from the index).
    fn note_vector_removed(&self, id: u64, labels: &[String]) {
        let list = match self.vector_indexes() {
            Ok(l) if !l.is_empty() => l,
            _ => return,
        };
        if self.in_txn() {
            // Remembered, not published — as in `note_vector_write`.
            self.txn_touch(|t| {
                for vi in list.iter() {
                    if labels.iter().any(|l| l == &vi.label) {
                        t.vectors.entry(vi.name.clone()).or_default().push((id, false));
                    }
                }
            });
            return;
        }
        let mut deltas = self.vector_deltas.borrow_mut();
        for vi in list.iter() {
            if labels.iter().any(|l| l == &vi.label) {
                deltas.entry(vi.name.clone()).or_default().note_delete(id);
            }
        }
    }

    /// The current embedding stored for `id` under `prop`, as `f64`s, or
    /// `None` when the node or the vector is gone or not a numeric list.
    fn node_vector(&self, id: u64, prop: &str) -> Result<Option<Vec<f64>>, GraphError> {
        let Some(token) = self.token_peek("prop:", &self.props, prop) else {
            return Ok(None);
        };
        // The write path's read: the vector being replaced may itself be a
        // buffered write of this transaction.
        let Some(bytes) = self.store_get_w(&self.nodes, &id.to_be_bytes()) else {
            return Ok(None);
        };
        let rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let Some(tagged) = rec.get(PropertyId(token)) else {
            return Ok(None);
        };
        match decode_prop_opt(tagged) {
            Some(Value::List(items)) => {
                let mut v = Vec::with_capacity(items.len());
                for it in &items {
                    match it {
                        Value::Float(f) => v.push(*f),
                        Value::Int(n) => v.push(*n as f64),
                        _ => return Ok(None),
                    }
                }
                Ok(Some(v))
            }
            _ => Ok(None),
        }
    }

    /// Create a node.
    pub fn create_node(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Result<u64, GraphError> {
        // The write fence: registered before the first row write, released
        // after the last entry is recorded (drop). See `Graph::fence`.
        let _fence = self.fence();
        // The id is minted BEFORE enforcement: the marker a refusal-free
        // enforcement writes carries the owner's id. An id leaked by a
        // refused create is identical to a crash-abandoned reservation —
        // ids never reuse and tolerate gaps.
        let id = self.next_id("node")?;
        self.enforce_constraints(id, labels, props)?;
        let mut rec = Record::new();
        let mut tokens = Vec::with_capacity(labels.len());
        for l in labels {
            tokens.push(self.token("lbl:", &self.labels, l)?);
        }
        tokens.sort_unstable();
        tokens.dedup();
        rec.set(P_LABELS, encode_label_set(&tokens));
        for (k, v) in props {
            if matches!(v, Value::Null) {
                continue; // a null property is an absent property
            }
            let t = self.token("prop:", &self.props, k)?;
            rec.set(PropertyId(t), encode_prop(v)?);
        }
        // The stamp the entries carry: the commit ts of the LAST row this
        // write makes (each put's ts is at or past the previous one's).
        let mut stamp = self
            .store_put(
                &self.nodes,
                &id.to_be_bytes(),
                StoredValue::Plain(rec.encode()),
            )
            .map_err(GraphError::Store)?;
        // A crash HERE leaves a node the label scans cannot see — found by
        // the record-vs-membership audit, never by a reader trusting either
        // side alone.
        crash_point("graph.between_node_and_membership");
        for t in &tokens {
            stamp = self
                .store_put(
                    &self.index,
                    &membership_row(*t, id),
                    StoredValue::Plain(Vec::new()),
                )
                .map_err(GraphError::Store)?
                .max(stamp);
        }
        self.stats_change(StatsChange {
            nodes: 1,
            by_label: tokens.iter().map(|t| (*t, 1)).collect(),
            ..Default::default()
        });
        self.note_membership(id, &tokens, true, stamp);
        // A NEW node carrying property `k` must invalidate the index on `k`, or
        // a seek would miss it. This is a correctness obligation, not an
        // optimisation: the property-scoped check is only sound if every path
        // that writes a property records it.
        //
        // The DELTA alongside it is the optimisation: it lets the next reader
        // carry the index forward over this one row instead of re-scanning the
        // partition. The epoch is what makes it safe; the delta only makes it
        // cheap.
        for (k, v) in props {
            if matches!(v, Value::Null) {
                continue;
            }
            if let Some(t) = self.token_peek("prop:", &self.props, k) {
                self.note_prop_change(t, &id.to_be_bytes(), Self::index_key_of(v), stamp);
            }
        }
        self.note_vector_write(id, labels, props);
        counted!("graph.nodes created");
        Ok(id)
    }

    /// Create a relationship.
    pub fn create_rel(
        &self,
        src: u64,
        rel_type: &str,
        dst: u64,
        props: &BTreeMap<String, Value>,
    ) -> Result<u64, GraphError> {
        let _fence = self.fence();
        if self.store_get_w(&self.nodes, &src.to_be_bytes()).is_none() {
            return Err(GraphError::Missing("node", src));
        }
        if self.store_get_w(&self.nodes, &dst.to_be_bytes()).is_none() {
            return Err(GraphError::Missing("node", dst));
        }
        // Relationship constraints bind BEFORE the edge is written — a refused
        // edge must leave no record, membership or adjacency behind. The id
        // is minted first: the marker carries the owner (see `create_node`).
        let id = self.next_id("rel")?;
        self.enforce_rel_constraints(id, rel_type, props)?;
        let t = self.token("typ:", &self.types, rel_type)?;
        let mut rec = Record::new();
        rec.set(P_SRC, encode_prop(&Value::Int(src as i64))?);
        rec.set(P_DST, encode_prop(&Value::Int(dst as i64))?);
        rec.set(P_TYPE, encode_prop(&Value::Int(t as i64))?);
        for (k, v) in props {
            if matches!(v, Value::Null) {
                continue;
            }
            let pt = self.token("prop:", &self.props, k)?;
            rec.set(PropertyId(pt), encode_prop(v)?);
        }
        self.store_put(
            &self.rels,
            &id.to_be_bytes(),
            StoredValue::Plain(rec.encode()),
        )
        .map_err(GraphError::Store)?;
        let stamp_o = self
            .store_put(
                &self.index,
                &adjacency_row(b'O', src, t, dst, id),
                StoredValue::Plain(Vec::new()),
            )
            .map_err(GraphError::Store)?;
        // Both endpoints' guards — see `guard_row`. Same-endpoint self-loops
        // dedup naturally (one key, the write funnel's map keeps one entry).
        self.store_put_volatile(&self.index, &guard_row(src), StoredValue::Plain(Vec::new()))
            .map_err(GraphError::Store)?;
        self.store_put_volatile(&self.index, &guard_row(dst), StoredValue::Plain(Vec::new()))
            .map_err(GraphError::Store)?;
        let stamp_i = self
            .store_put(
                &self.index,
                &adjacency_row(b'I', dst, t, src, id),
                StoredValue::Plain(Vec::new()),
            )
            .map_err(GraphError::Store)?;
        self.stats_change(StatsChange {
            rels: 1,
            by_type: vec![(t, 1)],
            ..Default::default()
        });
        self.note_adjacency_changed(t, src, dst, stamp_o.max(stamp_i));
        counted!("graph.rels created");
        Ok(id)
    }

    /// Set one property on a node or relationship. `Null` REMOVES — Cypher's
    /// rule, and the one that keeps "absent" and "stored null" one state.
    pub fn set_prop(&self, is_node: bool, id: u64, key: &str, v: &Value) -> Result<(), GraphError> {
        let (prefix, what): (&KeyPrefix, &'static str) = if is_node {
            (&self.nodes, "node")
        } else {
            (&self.rels, "relationship")
        };
        let _fence = self.fence();
        let _latch = self.entity_latch(is_node, id);
        let bytes = self
            .store_get_w(prefix, &id.to_be_bytes())
            .ok_or(GraphError::Missing(what, id))?;
        let mut rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        // The PRE-image, for the constraint-marker moves below — captured
        // before the mutation edits the record in place.
        let skip: &[PropertyId] = if is_node {
            &[P_LABELS]
        } else {
            &[P_SRC, P_DST, P_TYPE]
        };
        let pre = self.decode_props(&rec, skip)?;
        let t = self.token("prop:", &self.props, key)?;
        if matches!(v, Value::Null) {
            rec.remove(PropertyId(t));
            sometimes!("graph.null set removed a property", true);
        } else {
            rec.set(PropertyId(t), encode_prop(v)?);
        }
        if is_node {
            // Enforce against the POST-image: the write is what must satisfy
            // the constraints, not the state it replaces.
            let mut labels = Vec::new();
            for lt in decode_label_set(rec.get(P_LABELS))? {
                labels.push(self.token_name("lbl:", lt)?);
            }
            let post = self.decode_props(&rec, &[P_LABELS])?;
            self.enforce_constraints(id, &labels, &post)?;
            // Enforcement placed the post-tuple's marker; drop the
            // pre-tuple's where the tuple moved (AFTER enforcement, so a
            // refusal leaves every marker untouched).
            self.move_constraint_markers(id, false, &labels, &pre, &post)?;
            let stamp = self
                .store_put(prefix, &id.to_be_bytes(), StoredValue::Plain(rec.encode()))
                .map_err(GraphError::Store)?;
            // A `Null` set REMOVES the property, so the row leaves the index —
            // which is exactly what `None` means here. Passing the new value
            // blindly would leave a stale entry behind under its old key.
            self.note_prop_change(
                t,
                &id.to_be_bytes(),
                if matches!(v, Value::Null) {
                    None
                } else {
                    Self::index_key_of(v)
                },
                stamp,
            );
            self.note_vector_write(id, &labels, &post);
            return Ok(());
        }
        // Relationship: enforce rel constraints against the POST-image, exactly
        // as the node branch does — the write is what must satisfy them.
        let type_tok = match rec.get(P_TYPE).and_then(decode_prop_opt) {
            Some(Value::Int(v)) if v >= 0 => v as u32,
            _ => return Err(GraphError::Corrupt(format!("relationship {id} lacks type"))),
        };
        let rel_type = self.token_name("typ:", type_tok)?;
        let post = self.decode_props(&rec, &[P_SRC, P_DST, P_TYPE])?;
        self.enforce_rel_constraints(id, &rel_type, &post)?;
        self.move_constraint_markers(id, true, std::slice::from_ref(&rel_type), &pre, &post)?;
        self.store_put(prefix, &id.to_be_bytes(), StoredValue::Plain(rec.encode()))
            .map_err(GraphError::Store)?;
        Ok(())
    }

    /// Add labels to a node.
    pub fn add_labels(&self, id: u64, labels: &[String]) -> Result<(), GraphError> {
        let _fence = self.fence();
        let _latch = self.entity_latch(true, id);
        let bytes = self
            .store_get_w(&self.nodes, &id.to_be_bytes())
            .ok_or(GraphError::Missing("node", id))?;
        let mut rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let mut tokens = decode_label_set(rec.get(P_LABELS))?;
        {
            let mut post_labels = Vec::new();
            for lt in &tokens {
                post_labels.push(self.token_name("lbl:", *lt)?);
            }
            post_labels.extend(labels.iter().cloned());
            let props = self.decode_props(&rec, &[P_LABELS])?;
            self.enforce_constraints(id, &post_labels, &props)?;
        }
        // Only the tokens the node did NOT already carry are membership
        // changes; re-adding a label it already has must not appear in the
        // delta, or the snapshot gains a duplicate id.
        let mut added: Vec<u32> = Vec::new();
        let mut stamp = 0u64;
        for l in labels {
            let t = self.token("lbl:", &self.labels, l)?;
            if !tokens.contains(&t) {
                tokens.push(t);
                added.push(t);
                stamp = self
                    .store_put(
                        &self.index,
                        &membership_row(t, id),
                        StoredValue::Plain(Vec::new()),
                    )
                    .map_err(GraphError::Store)?
                    .max(stamp);
                self.stats_change(StatsChange {
                    by_label: vec![(t, 1)],
                    ..Default::default()
                });
            }
        }
        tokens.sort_unstable();
        rec.set(P_LABELS, encode_label_set(&tokens));
        let stamp = self
            .store_put(
                &self.nodes,
                &id.to_be_bytes(),
                StoredValue::Plain(rec.encode()),
            )
            .map_err(GraphError::Store)?
            .max(stamp);
        let mut names = Vec::with_capacity(tokens.len());
        for t in &tokens {
            names.push(self.token_name("lbl:", *t)?);
        }
        let props = self.decode_props(&rec, &[P_LABELS])?;
        self.note_membership(id, &added, true, stamp);
        self.note_vector_write(id, &names, &props);
        Ok(())
    }

    /// Remove labels from a node.
    pub fn remove_labels(&self, id: u64, labels: &[String]) -> Result<(), GraphError> {
        let _fence = self.fence();
        let _latch = self.entity_latch(true, id);
        let bytes = self
            .store_get_w(&self.nodes, &id.to_be_bytes())
            .ok_or(GraphError::Missing("node", id))?;
        let mut rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let mut tokens = decode_label_set(rec.get(P_LABELS))?;
        let mut removed = Vec::new();
        let mut removed_tokens: Vec<u32> = Vec::new();
        let mut stamp = 0u64;
        for l in labels {
            let t = self.token("lbl:", &self.labels, l)?;
            if let Some(i) = tokens.iter().position(|x| *x == t) {
                tokens.remove(i);
                stamp = self
                    .store_delete_w(&self.index, &membership_row(t, id))
                    .max(stamp);
                self.stats_change(StatsChange {
                    by_label: vec![(t, -1)],
                    ..Default::default()
                });
                removed.push(l.clone());
                removed_tokens.push(t);
            }
        }
        self.note_vector_removed(id, &removed);
        rec.set(P_LABELS, encode_label_set(&tokens));
        let stamp = self
            .store_put(
                &self.nodes,
                &id.to_be_bytes(),
                StoredValue::Plain(rec.encode()),
            )
            .map_err(GraphError::Store)?
            .max(stamp);
        // AFTER the last write, for the reason spelled out in `delete_node`.
        //
        // Only the labels this node actually LOST, and `touch_all = false`: the
        // node itself still exists, so the all-nodes snapshot is unchanged.
        // Routed through the shared helper so this site records its EPOCH as
        // well as its delta — it used to write the delta inline, which is
        // exactly the shape that goes stale once the epoch is load-bearing.
        if !removed_tokens.is_empty() {
            self.note_membership_of(id, &removed_tokens, false, false, stamp);
        }
        // Constraints on the LOST labels no longer apply to this node —
        // release its markers under them (ownership-checked).
        if !removed.is_empty() {
            let props = self.decode_props(&rec, &[P_LABELS])?;
            self.remove_constraint_markers(id, false, &removed, &props)?;
        }
        Ok(())
    }

    /// Delete a node. Without `detach`, a connected node REFUSES — deleting
    /// it would dangle every adjacent relationship.
    pub fn delete_node(&self, id: u64, detach: bool) -> Result<(), GraphError> {
        // The fence spans the detach too (the nested `delete_rel` fences
        // register their own, later stamps; this one stays the low-water
        // mark for the whole delete).
        let _fence = self.fence();
        // Held across the detach too: a `SET` on this node that read the record
        // before the delete must not write it back after (a resurrection).
        // Node stripe before relationship stripes — the one lock order.
        let _latch = self.entity_latch(true, id);
        let bytes = self
            .store_get_w(&self.nodes, &id.to_be_bytes())
            .ok_or(GraphError::Missing("node", id))?;
        // IDS only. `rels_of` would decode every incident relationship here and
        // `delete_rel` decodes it again below, so the first decode was pure
        // waste — this loop only ever read `r.id`.
        let rel_ids = if self.detach_via_rel_ids.get() {
            self.incident_rel_ids(id, Dir::Both, None)?
        } else {
            // The differential arm: the original path, decode and all.
            self.rels_of(id, Dir::Both, None)?
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        if !rel_ids.is_empty() {
            if !detach {
                // `StillConnected` must stay EXACT. `rels_of` silently drops an
                // adjacency row whose relationship record is absent, so it
                // refuses only when a LIVE relationship exists — and an orphan
                // row must not turn a legal delete into a refusal. Enumerating
                // ids does not know that, so the existence check is explicit
                // here, and it EARLY-EXITS on the first live one: strictly less
                // work than before, and unobservable because the statement
                // errors out and never commits.
                let mut connected = false;
                for r in &rel_ids {
                    if self.rel(*r)?.is_some() {
                        connected = true;
                        break;
                    }
                }
                if connected {
                    sometimes!("graph.delete refused a connected node", true);
                    return Err(GraphError::StillConnected(id));
                }
            } else {
                for r in rel_ids {
                    match self.delete_rel(r) {
                        Ok(()) => {}
                        // Reproduces `rels_of`'s silent drop: an adjacency row
                        // whose record is gone is not an error, it is an orphan
                        // row, and the FSCK is what reports those.
                        Err(GraphError::Missing(..)) => {
                            sometimes!("graph.detach skipped an orphan adjacency row", true);
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        let rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let gone = decode_label_set(rec.get(P_LABELS))?;
        let mut gone_names = Vec::with_capacity(gone.len());
        let mut stamp = 0u64;
        for t in &gone {
            gone_names.push(self.token_name("lbl:", *t)?);
            stamp = self
                .store_delete_w(&self.index, &membership_row(*t, id))
                .max(stamp);
        }
        self.note_vector_removed(id, &gone_names);
        // Its constraint markers leave with it too (ownership-checked).
        {
            let props = self.decode_props(&rec, &[P_LABELS])?;
            self.remove_constraint_markers(id, false, &gone_names, &props)?;
        }
        // The node's guard leaves with it — a buffered DELETE is a write,
        // so a racing `create_rel` on this endpoint conflicts in either
        // commit order (see `guard_row`), and nothing strands: the guard is
        // gone once the node is.
        self.store_delete_w(&self.index, &guard_row(id));
        let stamp = self.store_delete_w(&self.nodes, &id.to_be_bytes()).max(stamp);
        // EVERY entry is stamped with the commit ts of the last write it
        // describes, recorded AFTER that write.
        //
        // A cache is reused when its build clock is at or past that stamp.
        // Recording before the node's own delete stamped a clock the delete
        // then advanced past, so a snapshot taken at the stamped instant
        // looked current while missing the delete —
        // `the_count_store_agrees_with_the_membership_walk_under_every_write`
        // caught exactly that: the all-nodes snapshot kept a deleted node.
        self.note_membership(id, &gone, false, stamp);
        // Deleting the node removes every property it held, so every index over
        // those properties is stale. Same obligation as creation.
        let gone_props: Vec<u32> = rec.iter().map(|(pid, _)| pid.0).collect();
        // Every one of them leaves the index — `None`, unconditionally. A
        // deleted node holds no value under any property.
        for t in &gone_props {
            self.note_prop_change(*t, &id.to_be_bytes(), None, stamp);
        }
        self.stats_change(StatsChange {
            nodes: -1,
            by_label: gone.iter().map(|t| (*t, -1)).collect(),
            ..Default::default()
        });
        counted!("graph.nodes deleted");
        Ok(())
    }

    /// Delete a relationship.
    pub fn delete_rel(&self, id: u64) -> Result<(), GraphError> {
        let _fence = self.fence();
        let _latch = self.entity_latch(false, id);
        let Some(r) = self.rel(id)? else {
            return Err(GraphError::Missing("relationship", id));
        };
        let t = self.token("typ:", &self.types, &r.rel_type)?;
        // Its constraint markers leave with it (ownership-checked).
        self.remove_constraint_markers(id, true, std::slice::from_ref(&r.rel_type), &r.props)?;
        let stamp_o = self.store_delete_w(&self.index, &adjacency_row(b'O', r.src, t, r.dst, id));
        let stamp_i = self.store_delete_w(&self.index, &adjacency_row(b'I', r.dst, t, r.src, id));
        // Both endpoints' guards move — see `guard_row`. A PUT, not a
        // delete: the nodes still exist and later writers must keep
        // conflicting through the same key.
        self.store_put_volatile(&self.index, &guard_row(r.src), StoredValue::Plain(Vec::new()))
            .map_err(GraphError::Store)?;
        self.store_put_volatile(&self.index, &guard_row(r.dst), StoredValue::Plain(Vec::new()))
            .map_err(GraphError::Store)?;
        self.store_delete_w(&self.rels, &id.to_be_bytes());
        self.stats_change(StatsChange {
            rels: -1,
            by_type: vec![(t, -1)],
            ..Default::default()
        });
        self.note_adjacency_changed(t, r.src, r.dst, stamp_o.max(stamp_i));
        counted!("graph.rels deleted");
        Ok(())
    }

    // ── Reads ───────────────────────────────────────────────────────────

    /// [`Graph::node`] WITHOUT recording the read — for a candidate that may
    /// yet be rejected. See `set_read_set_bindings_only`.
    pub(crate) fn node_unrecorded(&self, id: u64) -> Result<Option<Value>, GraphError> {
        let Some(bytes) = self.store_get_peek_unrecorded(&self.nodes, &id.to_be_bytes()) else {
            return Ok(None);
        };
        self.node_from_bytes(id, &bytes)
    }

    /// Materialise a node as a value, or None.
    pub fn node(&self, id: u64) -> Result<Option<Value>, GraphError> {
        let Some(bytes) = self.store_get_peek(&self.nodes, &id.to_be_bytes()) else {
            return Ok(None);
        };
        self.node_from_bytes(id, &bytes)
    }

    /// The decode half of [`Graph::node`], shared with the unrecorded variant
    /// so the two cannot produce different values — only different read sets.
    fn node_from_bytes(&self, id: u64, bytes: &[u8]) -> Result<Option<Value>, GraphError> {
        counted!("graph.nodes materialised in full");
        let rec = Record::decode(bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let mut labels = Vec::new();
        for t in decode_label_set(rec.get(P_LABELS))? {
            labels.push(self.token_name("lbl:", t)?);
        }
        let props = self.decode_props(&rec, &[P_LABELS])?;
        Ok(Some(Value::Node { id, labels, props }))
    }

    /// Materialise a node with ONLY the named properties — projection
    /// pushdown at the materialisation site. The port benchmark measured
    /// what full materialisation costs where two properties are read: a
    /// `:Bio`-heavy scan cloned every multi-kilobyte property map to
    /// aggregate two scalars. Labels always come along (identity and label
    /// predicates need them); equality on nodes is by id, so a projected
    /// node compares exactly like a full one.
    pub fn node_projected(
        &self,
        id: u64,
        props: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Value>, GraphError> {
        // The demanded properties as tokens (a name with no token is a
        // property nothing ever wrote — absent everywhere), plus the labels.
        let mut want: Vec<u32> = props
            .iter()
            .filter_map(|p| self.token_peek("prop:", &self.props, p))
            .collect();
        want.push(P_LABELS.0);
        let body = id.to_be_bytes();
        // An active transaction's OWN buffered version wins (read-your-writes:
        // the streaming interpreter materialises every seed through this
        // projection, so a node the statement just created must be found
        // here, not only through `node`); a tombstone is absence. A miss is
        // served by the committed store and recorded, as `store_get_peek`
        // does, so a read that feeds a write is validated at commit.
        let buffered = ACTIVE_TXN.with(|t| {
            let mut t = t.borrow_mut();
            let txn = t.as_mut()?;
            match txn.peek(&self.nodes, &body) {
                Some(hit) => Some(hit),
                None => {
                    txn.note_read(&self.nodes, &body);
                    None
                }
            }
        });
        let got = match buffered {
            Some(None) => return Ok(None),
            Some(Some(bytes)) => engram_store::Projected::Record(bytes),
            None => match self.store.get_projected(&self.nodes, &body, &want) {
                Some(got) => got,
                None => return Ok(None),
            },
        };
        let mut labels = Vec::new();
        let mut out = BTreeMap::new();
        match got {
            engram_store::Projected::Record(bytes) => {
                // Only the wanted tokens are decoded — the rest of the record
                // is walked, not copied, and never NAMED: naming every
                // property to test whether it was wanted was thirty token
                // lookups and thirty `String`s per row on a wide label.
                let rec = Record::decode_projected(&bytes, &want)
                    .map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
                for t in decode_label_set(rec.get(P_LABELS))? {
                    labels.push(self.token_name("lbl:", t)?);
                }
                for (pid, tagged) in rec.iter() {
                    if pid == P_LABELS {
                        continue;
                    }
                    let name = self.token_name("prop:", pid.0)?;
                    if !props.contains(&name) {
                        continue; // a token the caller did not ask for cannot be here
                    }
                    let v = decode_prop_opt(tagged).ok_or_else(|| {
                        GraphError::Corrupt(format!("undecodable property {name}"))
                    })?;
                    out.insert(name, v);
                }
            }
            engram_store::Projected::Columns(cols) => {
                counted!("graph.projected gets served from columns");
                let mut seen_labels = false;
                for (pid, tagged) in cols {
                    if pid == P_LABELS.0 {
                        seen_labels = true;
                        for t in decode_label_set(Some(&tagged))? {
                            labels.push(self.token_name("lbl:", t)?);
                        }
                        continue;
                    }
                    let name = self.token_name("prop:", pid)?;
                    let v = decode_prop_opt(&tagged).ok_or_else(|| {
                        GraphError::Corrupt(format!("undecodable property {name}"))
                    })?;
                    out.insert(name, v);
                }
                if !seen_labels {
                    for t in decode_label_set(None)? {
                        labels.push(self.token_name("lbl:", t)?);
                    }
                }
            }
        }
        counted!("graph.projected node materialisations");
        Ok(Some(Value::Node {
            id,
            labels,
            props: out,
        }))
    }

    /// Stream every live relationship, optionally type-filtered — the seed
    /// a `()-[r:T]->()` pattern wants. Driving that shape from the NODES
    /// side materialised 1.79M start candidates to visit 5.3M rels; this
    /// walks the relationship partition once. The benchmark measured the
    /// difference as 415 seconds against the incumbent's milliseconds.
    pub fn for_each_rel(
        &self,
        types: Option<&[String]>,
        f: &mut dyn FnMut(RelRow) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        // Resolve the type filter to tokens WITHOUT minting: an unknown
        // type matches nothing.
        let filter: Option<Vec<u32>> = match types {
            None => None,
            Some(names) => {
                let mut toks = Vec::with_capacity(names.len());
                for n in names {
                    if let Some(t) = self.token_peek("typ:", &self.types, n) {
                        toks.push(t);
                    }
                }
                if toks.is_empty() {
                    return Ok(());
                }
                Some(toks)
            }
        };
        counted!("graph.rel-driven seeds");
        // The active transaction's buffered relationship rows overlay the
        // committed scan: a replaced record is visited with its buffered
        // bytes, a buffered delete is skipped, and buffered CREATIONS are
        // visited after the committed rows (order is not part of this
        // seed's contract).
        let pending: Option<BTreeMap<Vec<u8>, Option<Vec<u8>>>> = self
            .txn_pending_values(&self.rels, &[])
            .map(|v| v.into_iter().collect());
        let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        // The visitor scan: no wholesale clone of 5M relationship records.
        let mut gerr: Option<GraphError> = None;
        self.store
            .for_each_span(
                &self.rels,
                &[],
                u64::MAX,
                &mut |body, bytes| {
                    let over = pending.as_ref().and_then(|p| p.get(body));
                    let bytes: &[u8] = match over {
                        Some(None) => {
                            seen.insert(body.to_vec());
                            return true; // buffered delete: not visited
                        }
                        Some(Some(b)) => {
                            seen.insert(body.to_vec());
                            b
                        }
                        None => bytes,
                    };
                    match Self::rel_from_record(self, body, bytes, filter.as_deref()) {
                        Ok(Some(rel)) => match f(rel) {
                            Ok(()) => true,
                            Err(e) => {
                                gerr = Some(e);
                                false
                            }
                        },
                        Ok(None) => true,
                        Err(e) => {
                            gerr = Some(e);
                            false
                        }
                    }
                },
            );
        if let Some(e) = gerr {
            return Err(e);
        }
        if let Some(p) = pending {
            for (body, val) in p {
                if seen.contains(&body) {
                    continue;
                }
                let Some(bytes) = val else { continue };
                if let Some(rel) = Self::rel_from_record(self, &body, &bytes, filter.as_deref())? {
                    f(rel)?;
                }
            }
        }
        Ok(())
    }

    /// Decode one relationship record, applying the pre-resolved type
    /// filter; `None` = filtered out.
    fn rel_from_record(
        &self,
        body: &[u8],
        bytes: &[u8],
        filter: Option<&[u32]>,
    ) -> Result<Option<RelRow>, GraphError> {
        {
            let rec = Record::decode(bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
            let read_int = |p: PropertyId, what: &str| -> Result<u64, GraphError> {
                match rec.get(p).and_then(decode_prop_opt) {
                    Some(Value::Int(v)) if v >= 0 => Ok(v as u64),
                    _ => Err(GraphError::Corrupt(format!("relationship lacks {what}"))),
                }
            };
            let t = read_int(P_TYPE, "type")? as u32;
            if let Some(toks) = filter {
                if !toks.contains(&t) {
                    return Ok(None);
                }
            }
            let id = u64::from_be_bytes(
                body.try_into()
                    .map_err(|_| GraphError::Corrupt("rel key width".into()))?,
            );
            let src = read_int(P_SRC, "src")?;
            let dst = read_int(P_DST, "dst")?;
            let rel_type = self.token_name("typ:", t)?;
            let props = self.decode_props(&rec, &[P_SRC, P_DST, P_TYPE])?;
            Ok(Some(RelRow {
                id,
                src,
                dst,
                rel_type,
                props,
            }))
        }
    }

    /// Fix 73: the type name of a relationship type token — what a LEAN
    /// relationship binding (id, ends and type from the adjacency entry, no
    /// record) carries as `rel_type`.
    pub(crate) fn rel_type_name(&self, token: u32) -> Result<String, GraphError> {
        self.token_name("typ:", token)
    }

    /// Fix 73: the sorted ids of every `label` member whose `key` equals
    /// `value` — a STRING value only (the index probe and Cypher's `=`
    /// agree exactly on strings; an integer probe also answers the equal
    /// float, which `=` treats alike but a caller binding from the set
    /// alone could not tell apart), resolved through the label-scoped
    /// index and memoised on the property's epoch. `None` when the set
    /// would exceed `cap`, the value is not a string, a transaction holds
    /// buffered writes (the shared index cannot see them), or the index
    /// cannot serve the value. The answer is EXACT for committed state:
    /// the index is caught up to the property's epoch before it answers,
    /// and every id is re-tested against the label's membership.
    pub(crate) fn constant_end_ids(
        &self,
        label: &str,
        key: &str,
        value: &Value,
        cap: usize,
    ) -> Result<Option<std::sync::Arc<Vec<u64>>>, GraphError> {
        if self.in_txn_with_writes() || !matches!(value, Value::Str(_)) {
            return Ok(None);
        }
        let Some(token) = self.token_peek("prop:", &self.props, key) else {
            // A property never minted: nothing carries it.
            return Ok(Some(std::sync::Arc::new(Vec::new())));
        };
        let epoch = self.prop_epoch(token);
        {
            let memo = self.end_set_memo.borrow();
            for (l, k, v, e, ids) in memo.iter() {
                if *e == epoch && l == label && k == key && v == value {
                    counted!("graph.constant end set served from the memo");
                    return Ok(Some(std::sync::Arc::clone(ids)));
                }
            }
        }
        let Some(mut ids) = self.index_probe_eq_scoped(key, value, Some(cap), Some(label))? else {
            return Ok(None);
        };
        ids.sort_unstable();
        ids.dedup();
        // The scoped index restricts to the label's members; when scoping is
        // off the partition-wide index answered, so every id is re-tested.
        let members = self.members(Some(label))?;
        ids.retain(|id| self.members_contains(&members, *id));
        let ids = std::sync::Arc::new(ids);
        let mut memo = self.end_set_memo.borrow_mut();
        if memo.len() >= END_SET_MEMO_MAX {
            memo.clear();
        }
        memo.push((
            label.to_string(),
            key.to_string(),
            value.clone(),
            epoch,
            std::sync::Arc::clone(&ids),
        ));
        counted!("graph.constant end set resolved");
        Ok(Some(ids))
    }

    /// Materialise a relationship record.
    pub fn rel(&self, id: u64) -> Result<Option<RelRow>, GraphError> {
        let Some(bytes) = self.store_get_peek(&self.rels, &id.to_be_bytes()) else {
            return Ok(None);
        };
        let rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
        let read_int = |p: PropertyId, what: &str| -> Result<u64, GraphError> {
            match rec.get(p).and_then(decode_prop_opt) {
                Some(Value::Int(v)) if v >= 0 => Ok(v as u64),
                _ => Err(GraphError::Corrupt(format!(
                    "relationship {id} lacks {what}"
                ))),
            }
        };
        let src = read_int(P_SRC, "src")?;
        let dst = read_int(P_DST, "dst")?;
        let t = read_int(P_TYPE, "type")? as u32;
        let rel_type = self.token_name("typ:", t)?;
        let props = self.decode_props(&rec, &[P_SRC, P_DST, P_TYPE])?;
        // Mirrors `graph.nodes materialised in full`: this is a full record
        // decode plus a token lookup plus every property, and it is what the
        // delete path used to pay TWICE per incident relationship.
        counted!("graph.rels materialised in full");
        Ok(Some(RelRow {
            id,
            src,
            dst,
            rel_type,
            props,
        }))
    }

    /// Every relationship of `rel_type`, materialised — the population a
    /// relationship constraint validates against. O(all relationships); a rel
    /// constraint is rare, and this mirrors the node path's `nodes_by_label`
    /// scan rather than adding a per-type membership index nothing else needs.
    /// FSCK for the dangling-edge invariant: every live relationship's
    /// endpoints must exist. Returns the offending relationship ids —
    /// empty is the only healthy answer. Committed state only (no overlay):
    /// this is an integrity scan, not a query.
    pub fn verify_rel_endpoints(&self) -> Result<Vec<u64>, GraphError> {
        let mut bad = Vec::new();
        for (body, bytes) in self.store.scan_body_prefix(&self.rels, &[]) {
            let id = u64::from_be_bytes(
                body.as_slice()
                    .try_into()
                    .map_err(|_| GraphError::Corrupt("relationship id width".into()))?,
            );
            let rec = Record::decode(&bytes).map_err(|e| GraphError::Corrupt(format!("{e:?}")))?;
            let end = |p: PropertyId| -> Option<u64> {
                match rec.get(p).and_then(decode_prop_opt) {
                    Some(Value::Int(v)) if v >= 0 => Some(v as u64),
                    _ => None,
                }
            };
            let (Some(src), Some(dst)) = (end(P_SRC), end(P_DST)) else {
                bad.push(id);
                continue;
            };
            if self.store.get(&self.nodes, &src.to_be_bytes()).is_none()
                || self.store.get(&self.nodes, &dst.to_be_bytes()).is_none()
            {
                bad.push(id);
            }
        }
        Ok(bad)
    }

    pub(crate) fn rels_of_type(&self, rel_type: &str) -> Result<Vec<RelRow>, GraphError> {
        let mut ids: Vec<u64> = Vec::new();
        for (body, _bytes) in self.store.scan_body_prefix(&self.rels, &[]) {
            ids.push(u64::from_be_bytes(
                body.as_slice()
                    .try_into()
                    .map_err(|_| GraphError::Corrupt("relationship id width".into()))?,
            ));
        }
        // The active transaction's buffered CREATES are candidates too — a
        // uniqueness constraint must see the relationship an earlier clause
        // of the same statement created. (`rel` below already resolves each
        // id through the transaction's view, so replaced records and
        // buffered deletes are right without more work here.)
        if let Some(pending) = self.txn_pending(&self.rels, &[]) {
            for (body, is_put) in pending {
                if !is_put {
                    continue;
                }
                if let Ok(b) = <[u8; 8]>::try_from(body.as_slice()) {
                    ids.push(u64::from_be_bytes(b));
                }
            }
            ids.sort_unstable();
            ids.dedup();
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(r) = self.rel(id)? {
                if r.rel_type == rel_type {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn decode_props(
        &self,
        rec: &Record,
        reserved: &[PropertyId],
    ) -> Result<BTreeMap<String, Value>, GraphError> {
        let mut out = BTreeMap::new();
        for (pid, tagged) in rec.iter() {
            if reserved.contains(&pid) {
                continue;
            }
            let name = self.token_name("prop:", pid.0)?;
            let v = decode_prop_opt(tagged)
                .ok_or_else(|| GraphError::Corrupt(format!("undecodable property {name}")))?;
            out.insert(name, v);
        }
        Ok(out)
    }

    /// Every node id carrying `label` (or every node, if None) — the
    /// membership scan.
    pub fn nodes_by_label(&self, label: Option<&str>) -> Result<Vec<u64>, GraphError> {
        self.nodes_by_label_in(label, true)
    }

    /// The COMMITTED members of a label — what a shared snapshot is built
    /// from. A transaction's buffered rows never enter it.
    fn nodes_by_label_committed(&self, label: Option<&str>) -> Result<Vec<u64>, GraphError> {
        self.nodes_by_label_in(label, false)
    }

    /// `overlay`: whether the active transaction's buffered writes are laid
    /// over the committed rows (the query-answering read) or not (the
    /// snapshot-building read).
    fn nodes_by_label_in(&self, label: Option<&str>, overlay: bool) -> Result<Vec<u64>, GraphError> {
        match label {
            Some(l) => {
                // A label never seen has no token and therefore no members —
                // NOT an error; MATCH on it answers zero rows.
                if self
                    .store
                    .get(&self.kv, &[b"lbl:", l.as_bytes()].concat())
                    .is_none()
                    && !self.labels.load().contains_key(l)
                {
                    return Ok(Vec::new());
                }
                let t = self.token("lbl:", &self.labels, l)?;
                let mut out = Vec::new();
                let want = membership_prefix(t);
                // Bounded by the membership prefix: O(label size), never
                // O(index partition) — the bench harness's first finding.
                let bodies = if overlay {
                    self.index_bodies(&want)
                } else {
                    self.store.scan_bodies_prefix(&self.index, &want)
                };
                for body in bodies {
                    out.push(u64::from_be_bytes(
                        body[want.len()..]
                            .try_into()
                            .map_err(|_| GraphError::Corrupt("membership row".into()))?,
                    ));
                }
                Ok(out)
            }
            None => {
                let mut out: Vec<u64> = self
                    .store
                    .scan(&self.nodes)
                    .into_iter()
                    .map(|(body, _)| {
                        body.as_slice()
                            .try_into()
                            .map(u64::from_be_bytes)
                            .map_err(|_| GraphError::Corrupt("node key width".into()))
                    })
                    .collect::<Result<_, _>>()?;
                if overlay {
                    if let Some(pending) = self.txn_pending(&self.nodes, &[]) {
                        for (body, is_put) in pending {
                            let Ok(b) = <[u8; 8]>::try_from(body.as_slice()) else {
                                continue;
                            };
                            let id = u64::from_be_bytes(b);
                            match (is_put, out.binary_search(&id)) {
                                (true, Err(at)) => out.insert(at, id),
                                (false, Ok(at)) => {
                                    out.remove(at);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// A node's adjacency rows in `dir`, filtered by type tokens, read from
    /// KEY BYTES only — no relationship record is fetched. Served from the
    /// per-epoch adjacency table once this epoch has probed past the
    /// admission threshold (shared with the degree table), else from one
    /// bounded prefix walk. `Both` offers each relationship once: an O-side
    /// self-loop is not repeated from the I side.
    pub fn adjacent_slim(
        &self,
        node: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
    ) -> Vec<SlimAdj> {
        let mut out = Vec::new();
        self.adjacent_slim_visit(node, dir, type_tokens, false, &mut |e| out.push(*e));
        out
    }

    /// The same adjacency `adjacent_slim` produces, delivered to `f` WITHOUT
    /// allocating and copying a per-node `Vec`: the hot expansions hand the
    /// caller a slice they iterate exactly once, so materialising an owned
    /// `Vec` first is pure overhead (~2119 allocations + ~100k `SlimAdj`
    /// copies on IC9's friend→message hop). In the cached-table single-side
    /// case this calls `f` over `t.slice(node)` directly — the `Arc<AdjTable>`
    /// is held for the borrow; `Both` visits the O side then the I side; the
    /// pre-admission / oversized-id case falls back to the same bounded prefix
    /// walk. The visit order is byte-identical to `adjacent_slim`'s — O then I
    /// for `Both`, the I-side self-loop deduped, every type filter applied.
    pub fn adjacent_slim_for_each<F: FnMut(&SlimAdj)>(
        &self,
        node: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        mut f: F,
    ) {
        self.adjacent_slim_visit(node, dir, type_tokens, false, &mut f);
    }

    /// The single-source forward-BFS tree from `source` to depth `max` over `dir`
    /// with `type_tokens`, MEMOISED per commit epoch and `(source, dir, types,
    /// max)`. For a BOUNDED `shortestPath` whose source is reused across seeds
    /// (IC1's `firstName='Ana'` all share source p=10): the neighbourhood is built
    /// once, then each seed is an O(path) distance lookup. Bounded: clears at 4,096
    /// entries.
    pub(crate) fn forward_bfs_tree(
        &self,
        source: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        max: u64,
    ) -> std::sync::Arc<BfsTree> {
        let dir_tag: u8 = match dir {
            Dir::Out => 0,
            Dir::In => 1,
            Dir::Both => 2,
        };
        let key = (
            source,
            dir_tag,
            type_tokens.clone().unwrap_or_default(),
            max,
        );
        // Keyed on the adjacency epoch of these types (read BEFORE the walk):
        // a node or property write leaves a memoised tree current.
        let epoch = self.adjacency_epoch(type_tokens);
        // A memoised tree is committed state: neither served nor stored while
        // a transaction with buffered writes is active.
        let private = self.in_txn_with_writes();
        if !private {
            let cache = self.bfs_memo.borrow();
            if let Some((at, tree)) = cache.get(&key) {
                if *at >= epoch {
                    return std::sync::Arc::clone(tree);
                }
            }
        }
        counted!("graph.bfs forward trees built");
        let mut dist: BTreeMap<u64, u64> = BTreeMap::new();
        let mut parent: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        dist.insert(source, 0);
        let mut frontier: Vec<u64> = vec![source];
        let mut d = 0u64;
        while d < max && !frontier.is_empty() {
            d += 1;
            let mut next: Vec<u64> = Vec::new();
            for &node in &frontier {
                self.adjacent_slim_for_each(node, dir, type_tokens, |e| {
                    if let std::collections::btree_map::Entry::Vacant(slot) = dist.entry(e.peer) {
                        slot.insert(d);
                        parent.insert(e.peer, (node, e.rel));
                        next.push(e.peer);
                    }
                });
            }
            frontier = next;
        }
        let tree = std::sync::Arc::new(BfsTree { dist, parent });
        if !private {
            let mut cache = self.bfs_memo.borrow_mut();
            if cache.len() >= 4096 {
                cache.clear();
            }
            cache.insert(key, (epoch, std::sync::Arc::clone(&tree)));
        }
        tree
    }

    /// `adjacent_slim_for_each` in REVERSE — the exact sequence
    /// `adjacent_slim(node, dir, tokens).iter().rev()` visits, without the
    /// intervening `Vec`. The fixed-hop `expand` / `semijoin` and the
    /// vectorised collectors pop neighbours LIFO, so they consume the reversed
    /// order; this feeds them straight from the table slice (iterated
    /// back-to-front) with the identical self-loop / `Both` handling.
    pub fn adjacent_slim_rev_for_each<F: FnMut(&SlimAdj)>(
        &self,
        node: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        mut f: F,
    ) {
        self.adjacent_slim_visit(node, dir, type_tokens, true, &mut f);
    }

    /// The shared adjacency walk behind `adjacent_slim` and the two
    /// `*_for_each` accessors. `rev` reverses the visit — both the side order
    /// (`Both` becomes I then O) AND the within-side order — so the visited
    /// sequence is the exact reverse of the forward one. Exactly one degree
    /// probe is charged per call, as `adjacent_slim` always did, so `use_table`
    /// admission is unchanged; migrating a caller from `adjacent_slim` to a
    /// `*_for_each` is 1:1 in probes.
    fn adjacent_slim_visit<F: FnMut(&SlimAdj)>(
        &self,
        node: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        rev: bool,
        f: &mut F,
    ) {
        // The gate counts probes per ADJACENCY epoch of these types. Keyed on
        // the commit clock it reset on every write of any kind, and the first
        // `DEGREE_TABLE_AFTER` hops after each one walked the prefix past a
        // current table — 1,024 visitor scans per statement under a balanced
        // load, the largest single cost in the head-to-head. The gate governs
        // only whether a table is BUILT; a table that exists and is current
        // (or can be repaired) serves every hop, admitted or not.
        let epoch = self.adjacency_epoch(type_tokens);
        let table_ok = node <= DEGREE_TABLE_MAX_ID;
        // STOP COUNTING ONCE ADMITTED. `tick` is an atomic `fetch_add` and this
        // is EVERY adjacency visit of every read — one shared cache line that
        // all eight workers write on every hop.
        //
        // The count exists only to decide whether a table may be BUILT, and
        // admission is MONOTONE within an epoch: the counter never decreases,
        // so once it has reached the threshold `admit` stays true whether or not
        // anything further is recorded. Reading it and recording only while
        // BELOW the threshold therefore admits at exactly the same probe as
        // before — the same `degree_table_after`-th one — and then leaves the
        // line alone. A new epoch re-bases the counter and the gate is re-earned
        // exactly as it was.
        let probe_epoch = self.adj_epoch_now();
        let after = self.degree_table_after.get();
        let seen = self.degree_probes.peek(probe_epoch);
        let n = if seen >= after {
            seen
        } else {
            self.degree_probes.tick(probe_epoch)
        };
        let admit = n >= after && table_ok;
        let both = matches!(dir, Dir::Both);
        let sides: &[u8] = match (dir, rev) {
            (Dir::Out, _) => b"O",
            (Dir::In, _) => b"I",
            (Dir::Both, false) => b"OI",
            (Dir::Both, true) => b"IO",
        };
        for &tag in sides {
            // The adjacency key is a TAG BYTE and a big-endian node id — always
            // exactly nine bytes, so it belongs on the stack. It was a `vec!`,
            // which put a heap allocation on the hottest read in the engine:
            // once per side per node VISITED, so twice for every node of an
            // undirected hop. LSQB q3 walks ~12.8M nodes over undirected
            // `KNOWS` (~25M allocations) and a 30 s `read-only` profile records
            // 83M label probes over a comparable number of visits.
            //
            // It is also only READ on the cold path (`index_bodies` below, when
            // no table served) and, on the hot path, solely to ask whether an
            // active transaction has buffered rows for this key — a question
            // with no allocation-worthy answer.
            let mut want = [0u8; 1 + 8];
            want[0] = tag;
            want[1..].copy_from_slice(&node.to_be_bytes());
            let want = &want[..];
            // Inside a transaction that has buffered adjacency rows for THIS
            // node and side, the shared table (committed state) cannot serve
            // it: the prefix walk below overlays the transaction's rows.
            let overlaid = self
                .txn_pending(&self.index, want)
                .is_some_and(|p| !p.is_empty());
            // Borrow the CSR table from the arc-swap guard (no per-probe `Arc`
            // clone — that refcount RMW ping-pongs one cache line across all
            // workers and collapses concurrent complex-join reads). `handled` is
            // false only when the table is skipped or declined by the budget, in
            // which case we fall through to the direct prefix scan below.
            let handled = if table_ok && !overlaid {
                self.with_adj_table(tag, type_tokens, epoch, admit, Some(node), |tbl| match tbl {
                    Some(t) => {
                        // Zero-copy: iterate the cached CSR slice in place,
                        // holding the guard's borrow. No per-node Vec, no copy.
                        let slice = t.slice(node);
                        if rev {
                            for e in slice.iter().rev() {
                                if tag == b'I' && both && e.peer == node {
                                    continue; // the O side already offered this self-loop
                                }
                                f(e);
                            }
                        } else {
                            for e in slice {
                                if tag == b'I' && both && e.peer == node {
                                    continue; // the O side already offered this self-loop
                                }
                                f(e);
                            }
                        }
                        true
                    }
                    None => false,
                })
            } else {
                false
            };
            if !handled {
                {
                    // The prefix walk is forward-only, so `rev` buffers this
                    // side's rows and replays them reversed. This is the cold,
                    // pre-admission path (`Vec::new()` never allocates on the
                    // forward branch); the hot expansions run against the table.
                    let mut buf: Vec<SlimAdj> = Vec::new();
                    for body in self.index_bodies(want) {
                        if body.len() != 1 + 8 + 4 + 8 + 8 {
                            continue;
                        }
                        let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                        if let Some(tt) = type_tokens {
                            if tt.binary_search(&t).is_err() {
                                continue;
                            }
                        }
                        let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
                        if tag == b'I' && both && peer == node {
                            continue;
                        }
                        let rel = u64::from_be_bytes(body[21..29].try_into().expect("8"));
                        let e = SlimAdj {
                            rel,
                            type_token: t,
                            peer,
                        };
                        if rev {
                            buf.push(e);
                        } else {
                            f(&e);
                        }
                    }
                    if rev {
                        for e in buf.iter().rev() {
                            f(e);
                        }
                    }
                }
            }
        }
    }

    /// Build the CSR adjacency table for one side and type set from the store,
    /// or `None` when it would exceed the entry budget. No caching — the two
    /// accessors below cache the result.
    fn build_adj_table(&self, tag: u8, type_tokens: &Option<Vec<u32>>) -> Option<AdjTable> {
        let mut index = RowIndexBuilder::new();
        let mut entries: Vec<SlimAdj> = Vec::new();
        let max = self.adj_table_max_entries.get();
        // Stream the adjacency prefix through a callback rather than collecting every
        // body into an owned `Vec<Vec<u8>>` first (~125MB over 2.24M rows on the port
        // benchmark) — one body is held at a time, and the type filter keeps only the
        // matching entries. `body` is exactly what `scan_bodies_prefix` returned.
        let mut over_budget = false;
        // `sorted_by_peer` is ESTABLISHED here, one comparison per entry against
        // the previous entry of the same node — see the field's doc for why it
        // is checked rather than read off the key layout.
        let mut sorted = true;
        self.walk_adjacency_span(tag, &mut |body| {
            if body.len() != 1 + 8 + 4 + 8 + 8 {
                return true;
            }
            let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
            if let Some(tt) = type_tokens {
                if tt.binary_search(&t).is_err() {
                    return true;
                }
            }
            let node = u64::from_be_bytes(body[1..9].try_into().expect("8"));
            if node > DEGREE_TABLE_MAX_ID {
                return true;
            }
            if entries.len() >= max {
                over_budget = true;
                return false; // stop the scan
            }
            let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
            // The row's start is where this node's row began; an entry past
            // it is this node's previous entry.
            let row_start = index.note(node, entries.len());
            if entries.len() > row_start && entries[entries.len() - 1].peer > peer {
                sorted = false;
            }
            entries.push(SlimAdj {
                rel: u64::from_be_bytes(body[21..29].try_into().expect("8")),
                type_token: t,
                peer,
            });
            true
        });
        if over_budget {
            sometimes!("graph.adjacency table declined by the entry budget", true);
            return None;
        }
        counted!("graph.adjacency tables built");
        counters::ADJ_TABLES_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(AdjTable {
            index: std::sync::Arc::new(index.finish(entries.len())),
            entries: std::sync::Arc::new(entries),
            overlay: BTreeMap::new(),
            sorted_by_peer: sorted,
        })
    }

    /// Every adjacency table for `tag` — the untyped one and one per
    /// relationship type — from a SINGLE pass over the span.
    ///
    /// Building them one at a time costs one full span scan each. There are as
    /// many relationship types as the schema has, which is small and bounded,
    /// but 15 scans of 6.7M rows is still ~80 s where one is ~5 s. Since every
    /// row carries its own type token, one pass can fill every bucket at once,
    /// and the untyped table is simply the bucket that accepts everything.
    ///
    /// `None` when the untyped table would exceed the entry budget — the same
    /// refusal `build_adj_table` makes, and for the same reason.
    fn build_adj_tables_all_types(&self, tag: u8) -> Option<Vec<(Option<u32>, AdjTable)>> {
        let max = self.adj_table_max_entries.get();
        if max == 0 {
            sometimes!("graph.adjacency table declined by the entry budget", true);
            return None;
        }
        // token -> (directory builder, entries, sorted_by_peer); `None` is the
        // untyped bucket. The flag is established per bucket exactly as
        // `build_adj_table` establishes it, so a warmed table carries the
        // same claim a lazily built one does. The directory is built SPARSE
        // as the rows arrive: a dense offsets vector per bucket was ~6.5 GB
        // of transient for the ported corpus's 318 buckets.
        type Bucket = (RowIndexBuilder, Vec<SlimAdj>, bool);
        let mut buckets: BTreeMap<Option<u32>, Bucket> = BTreeMap::new();
        let mut over_budget = false;
        self.walk_adjacency_span(tag, &mut |body| {
            if body.len() != 1 + 8 + 4 + 8 + 8 {
                return true;
            }
            let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
            let node = u64::from_be_bytes(body[1..9].try_into().expect("8"));
            if node > DEGREE_TABLE_MAX_ID {
                return true;
            }
            let e = SlimAdj {
                rel: u64::from_be_bytes(body[21..29].try_into().expect("8")),
                type_token: t,
                peer: u64::from_be_bytes(body[13..21].try_into().expect("8")),
            };
            // The row goes into its own type's bucket AND the untyped one.
            // The span is walked in key order, which groups by node within
            // each bucket exactly as a single-type build would, so each
            // bucket ends up byte-identical to its `build_adj_table`.
            for key in [Some(t), None] {
                let (index, entries, sorted) = buckets
                    .entry(key)
                    .or_insert_with(|| (RowIndexBuilder::new(), Vec::new(), true));
                if entries.len() >= max {
                    // ANY bucket over budget declines the whole pass.
                    // Skipping just that bucket would publish a TRUNCATED
                    // table for that type — a table that answers, and
                    // answers short. Declining costs a lazy rebuild later,
                    // which is the behaviour without warming at all.
                    over_budget = true;
                    return false;
                }
                let row_start = index.note(node, entries.len());
                if entries.len() > row_start && entries[entries.len() - 1].peer > e.peer {
                    *sorted = false;
                }
                entries.push(e);
            }
            !over_budget
        });
        if over_budget {
            sometimes!("graph.adjacency table declined by the entry budget", true);
            return None;
        }
        counted!("graph.adjacency tables built in one pass");
        Some(
            buckets
                .into_iter()
                .map(|(key, (index, entries, sorted))| {
                    (
                        key,
                        AdjTable {
                            index: std::sync::Arc::new(index.finish(entries.len())),
                            entries: std::sync::Arc::new(entries),
                            overlay: BTreeMap::new(),
                            sorted_by_peer: sorted,
                        },
                    )
                })
                .collect(),
        )
    }

    /// One node's complete adjacency row for `(tag, type_tokens)`.
    ///
    /// Mirrors [`Graph::build_adj_table`]'s body decode, type filter and id
    /// bound EXACTLY, over the same index in the same order — a prefix scan of
    /// `[tag] + node` is a contiguous subrange of the `[tag]` span the base
    /// walks, so the row this returns is byte-identical to the row a rebuild
    /// would place at that node. That is what makes a repaired table
    /// substitutable for a rebuilt one rather than merely similar, and it is
    /// why this decodes the body here instead of borrowing a neighbouring
    /// helper whose visit order is only documented to match something else.
    fn adj_row_for(&self, tag: u8, node: u64, type_tokens: &Option<Vec<u32>>) -> Vec<SlimAdj> {
        let mut row = Vec::new();
        if node > DEGREE_TABLE_MAX_ID {
            return row; // the base omits these, so the overlay must too
        }
        let mut want = vec![tag];
        want.extend_from_slice(&node.to_be_bytes());
        for body in self.store.scan_bodies_prefix(&self.index, &want) {
            if body.len() != 1 + 8 + 4 + 8 + 8 {
                continue;
            }
            let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
            if let Some(tt) = type_tokens {
                if tt.binary_search(&t).is_err() {
                    continue;
                }
            }
            row.push(SlimAdj {
                rel: u64::from_be_bytes(body[21..29].try_into().expect("8")),
                type_token: t,
                peer: u64::from_be_bytes(body[13..21].try_into().expect("8")),
            });
        }
        row
    }

    /// The change set a repair of this table would re-read: the changed node
    /// ids, the log-entry count (one per changed row), and the fenced epoch
    /// the repair would be current at. `None` on the same refusals
    /// [`Graph::repaired_adj_table`] makes before doing any work — a log that
    /// no longer reaches the table's epoch, the fixed node cap with the cost
    /// model off, or the memory ceiling.
    ///
    /// Extracted so the maintenance refresh can PRICE a repair before paying
    /// for it (`refresh_pass_rows`). Both callers must see the same number or
    /// the budget would bound a different quantity than the one it meters;
    /// that is why this walk has one home rather than two.
    /// What a single-node reader should do with a table that is stale as a
    /// WHOLE — the question it actually has, which is narrower than the one
    /// the repair path answers.
    ///
    /// Three outcomes, because two would force the wrong trade in one of the
    /// two regimes this profile spans:
    ///
    /// - [`StaleProbe::Unmoved`] — nothing in the change set touches this node,
    ///   so the row the table already holds for it is exactly the row a repair
    ///   would re-read. Serving it is the same answer at the same vintage.
    /// - [`StaleProbe::MovedSmallDelta`] — it moved, but the delta is short
    ///   enough that repairing costs little and leaves a fresh table for every
    ///   reader behind us. This is today's behaviour and it is the RIGHT
    ///   behaviour under light write pressure; the interference this whole
    ///   change is about only exists when the delta is long.
    /// - [`StaleProbe::Unknown`] — it moved with a long delta, or a log no
    ///   longer covers `built_at`. Either way the reader should not pay here.
    ///
    /// The scan is capped at `ADJ_STALENESS_SCAN_MAX`, which is all the
    /// precision the decision needs: past it the answer is "ask the priced
    /// path", and how much further the delta runs changes nothing. So the
    /// COMMON case — a read whose own node did not move — costs a bounded scan
    /// and never touches the change set at all.
    ///
    /// **What this does NOT weaken.** A repair publishes at `fenced(at)`,
    /// clamped below every in-flight writer's stamp, and the reader then serves
    /// that table — so today's reader already does not see a writer that has
    /// allocated a stamp and not yet recorded. Serving an unmoved row from a
    /// table published at an earlier fenced stamp is the same class: committed
    /// state as of a fence below every in-flight writer. The rows are equal, so
    /// the answer is identical, not merely as good.
    ///
    /// The coverage guard is the same one [`Graph::adj_repair_change_set`]
    /// applies, so that argument is INHERITED rather than restated.
    fn adj_stale_probe(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        built_at: u64,
        node: u64,
    ) -> StaleProbe {
        let logs = self.adj_log.borrow();
        let mut scanned = 0usize;
        let mut moved = false;
        for ((t, ty), log) in logs.iter() {
            if *t != tag {
                continue;
            }
            if let Some(want) = type_tokens {
                if want.binary_search(ty).is_err() {
                    continue; // a type this table does not cover
                }
            }
            if !log.covers(built_at) {
                return StaleProbe::Uncovered;
            }
            for (_, n) in log.since(built_at) {
                moved = moved || *n == node;
                scanned += 1;
                if scanned > ADJ_STALENESS_SCAN_MAX {
                    // Past the budget the answer is the same whether or not
                    // this node is in the rest of the delta: a reader does not
                    // repair a change set this long, and a node that did NOT
                    // move is served from the stale table only when the scan
                    // completes and can vouch for it.
                    counted!("graph.adjacency staleness scan gave up on a long delta");
                    return StaleProbe::LongDelta;
                }
            }
        }
        if moved {
            StaleProbe::MovedSmallDelta
        } else {
            StaleProbe::Unmoved
        }
    }

    fn adj_repair_change_set(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        built_at: u64,
    ) -> Option<(std::collections::BTreeSet<u64>, usize, u64)> {
        let cost_based = self.adj_cost_repair.get();
        let logs = self.adj_log.borrow();
        let mut nodes: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut entries = 0usize;
        let mut at = built_at;
        for ((t, ty), log) in logs.iter() {
            if *t != tag {
                continue;
            }
            if let Some(want) = type_tokens {
                if want.binary_search(ty).is_err() {
                    continue; // a type this table does not cover
                }
            }
            if !log.covers(built_at) {
                return None;
            }
            for (_, n) in log.since(built_at) {
                entries += 1;
                nodes.insert(*n);
                if !cost_based && nodes.len() > ADJ_REPAIR_MAX {
                    counted!("graph.adjacency repair declined by the node cap");
                    return None;
                }
                if nodes.len() > ADJ_REPAIR_MAX_NODES {
                    return None; // the memory ceiling, whatever the cost says
                }
            }
            at = at.max(log.epoch());
        }
        Some((nodes, entries, self.fenced(at)))
    }

    /// What one repair of this table would cost, in rows re-read — but ONLY
    /// when a repair is what would actually happen.
    ///
    /// `None` means "do not price a repair here": either no repair is
    /// available (the log does not reach, or a cap refuses) or the
    /// repair-vs-rebuild gate would choose the REBUILD. Both cases belong to
    /// the rebuild budget, not to the row budget, and conflating them is a
    /// real defect rather than a conservative one: the first cut of this
    /// method priced every stale table and so deferred tables that could only
    /// ever be rebuilt, which `derived_refresh_offpath` caught immediately.
    ///
    /// The gate below is `repaired_adj_table`'s, on the same numbers from the
    /// same walk — if the two ever disagree, the budget meters a quantity
    /// nobody pays.
    fn adj_repair_cost_rows(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        built_at: u64,
        base_len: usize,
    ) -> Option<usize> {
        let (nodes, entries, _) = self.adj_repair_change_set(tag, type_tokens, built_at)?;
        let work = entries.saturating_add(nodes.len().saturating_mul(ADJ_REPAIR_SCAN_ROWS));
        if self.adj_cost_repair.get() && nodes.len() > ADJ_REPAIR_MAX && work >= base_len {
            return None; // the gate rebuilds; the rebuild budget owns it
        }
        Some(work)
    }

    /// Carry a cached table forward over the nodes whose rows changed since it
    /// was built — read from the adjacency change logs of the types it covers
    /// — or `None` when it must be rebuilt instead: a log no longer reaches
    /// the table's epoch (pruned past it, overflowed, or touched by a
    /// transaction), or more nodes changed than repair is cheaper for. Both
    /// conservative.
    ///
    /// Returns the table and the epoch it is current at: the logs' epoch, read
    /// in the SAME critical section as their entries and fenced below every
    /// in-flight writer in that section (`fenced`). Rows are re-read from the
    /// store, so a row may already carry a change stamped later than that
    /// epoch; the next repair re-reads it — idempotent.
    fn repaired_adj_table(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        base: &AdjTable,
        built_at: u64,
    ) -> Option<(AdjTable, u64)> {
        if !self.incremental_caches.get() {
            return None;
        }
        let cost_based = self.adj_cost_repair.get();
        let (nodes, entries, at) = self.adj_repair_change_set(tag, type_tokens, built_at)?;
        // THE GATE, additive: a change set the fixed cap admits is repaired
        // whatever the cost model would say (the old rule never declined it,
        // and on the small tables every repair test uses it never should);
        // past the cap the cost model decides. Repair re-reads each changed
        // node's row: the changed rows themselves (`entries`, exact from the
        // log) plus a fixed per-node scan setup. A rebuild walks at least
        // this table's rows (the whole side's span, filtered by type, is
        // more — so the table is a lower bound that errs toward rebuilding).
        // Repair wins below the table; no half — a repair reuses the base
        // and a rebuild pays for every row of it.
        if cost_based && nodes.len() > ADJ_REPAIR_MAX {
            let work = entries + nodes.len() * ADJ_REPAIR_SCAN_ROWS;
            if work >= base.len() {
                counted!("graph.adjacency repair declined by cost");
                return None;
            }
            counted!("graph.adjacency repair admitted by cost over the node cap");
        }
        // THE OVERLAY CARRY-OVER, instrumented.
        //
        // The overlay holds one COMPLETE row per node repaired since the base
        // was built, and this carries all of it into the repaired table. Under
        // a mixed workload the counters measured ~1 repair per write, so it
        // grows with the write stream: a repair that re-reads ONE changed node
        // still carries every row every earlier repair left behind.
        //
        // `ADJ_OVERLAY_FOLD` bounds how MANY rows that can be — the fold below
        // packs them into a fresh base past 4,096, and
        // `review_repair_over_cap_differential` drives a 40,000-node fixture
        // across it. What the fold cannot bound is what carrying one costs,
        // and that is the half this counter measures: as `Vec<SlimAdj>` each
        // carried row was an allocation and a memcpy, so the bound read as
        // "up to 4,096 allocations to re-read one node". The rows are `Arc`s
        // now, so the map copy is a spine walk with a refcount bump per row
        // and only the re-read rows allocate.
        //
        // The counter stays because the spine walk is still O(overlay) and the
        // fold's mean is still ~2,048 — a bound is not an absence, and the
        // number is how the next question gets asked.
        counters::ADJ_OVERLAY_ROWS_CLONED
            .fetch_add(base.overlay.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let mut overlay = base.overlay.clone();
        // The flag survives a repair only if every re-read row is itself
        // sorted: a single unsorted row anywhere in the table would make the
        // binary search over THAT node answer wrong, so the check is per row,
        // O(row), on rows this repair reads anyway.
        let mut sorted = base.sorted_by_peer;
        for n in nodes {
            let row = self.adj_row_for(tag, n, type_tokens);
            sorted = sorted && row_sorted_by_peer(&row);
            overlay.insert(n, row.into());
        }
        counted!("graph.adjacency tables repaired");
        counters::ADJ_TABLES_REPAIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fixed = AdjTable {
            index: std::sync::Arc::clone(&base.index),
            entries: std::sync::Arc::clone(&base.entries),
            overlay,
            sorted_by_peer: sorted,
        };
        if fixed.overlay.len() > self.adj_overlay_fold.get() {
            counted!("graph.adjacency table overlay folded");
            return Some((fixed.folded(), at));
        }
        Some((fixed, at))
    }

    /// Walk the `tag` side of the adjacency span for a table (re)build, under
    /// the scan policy the lever selects — see `scan_resistant_rebuild`.
    fn walk_adjacency_span(&self, tag: u8, f: &mut dyn FnMut(&[u8]) -> bool) {
        if self.scan_resistant_rebuild.get() {
            self.store
                .for_each_key_span_scan(&self.index, &[tag], u64::MAX, f);
        } else {
            self.store.for_each_key_span(&self.index, &[tag], u64::MAX, f);
        }
    }

    /// Drop adjacency-log entries no cached table of `tag` can still need:
    /// those at or below the oldest epoch published across the tag's tables.
    /// A slot with nothing published yet (a build in flight, or one the budget
    /// declined) pins the logs, which then age out at their cap instead —
    /// bounded either way.
    fn prune_adj_logs(&self, tag: u8) {
        let mut floor = u64::MAX;
        for ((t, _), slot) in self.adj_tables.load().iter() {
            if *t != tag {
                continue;
            }
            match slot.peek().as_ref() {
                Some(s) => floor = floor.min(s.at),
                None => return,
            }
        }
        if floor == u64::MAX {
            return;
        }
        let mut logs = self.adj_log.borrow_mut();
        for ((t, _), log) in logs.iter_mut() {
            if *t == tag {
                log.prune_below(floor);
            }
        }
    }

    /// The snapshot serving `(tag, type_tokens)` at `epoch` — the slow path
    /// behind [`Graph::with_adj_table`] and [`Graph::adj_table`], reached only
    /// when the lock-free hit missed.
    ///
    /// A stale table is repaired from the logs and republished (monotone: a
    /// slower worker's older repair cannot overwrite a newer one). A table
    /// that cannot be repaired is rebuilt, single-flight. A table that does
    /// not exist is built only if `admit` — the statement has probed enough
    /// to be worth one — else `None` and the caller scans directly.
    fn adj_table_snapshot(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        epoch: u64,
        admit: bool,
    ) -> Option<std::sync::Arc<Snapshot<AdjTable>>> {
        self.adj_table_snapshot_reporting(tag, type_tokens, epoch, admit, true, true)
            .0
    }

    /// [`Graph::adj_table_snapshot`] that also says WHAT it did — the
    /// maintenance refresh reports it; the read path discards it. With
    /// `may_rebuild` false (the refresh's budget) a stale table that cannot be
    /// repaired is left as it is and reported `Deferred`; every reader passes
    /// `true`.
    fn adj_table_snapshot_reporting(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        epoch: u64,
        admit: bool,
        may_rebuild: bool,
        // Whether the caller is a READER on a query thread, as opposed to the
        // maintenance pass. Only a reader is kept off a full-span rebuild by
        // admission: the pass passes `admit: false` to mean "do not build on my
        // account" and governs itself with `may_rebuild`, so applying the gate
        // to it would silence the one rebuild per tick it is allowed — which is
        // precisely what the first cut did, and what
        // `the_pass_stops_rebuilding_and_answers_do_not_change` caught.
        reader: bool,
    ) -> (Option<std::sync::Arc<Snapshot<AdjTable>>>, AdjOutcome) {
        let key = (tag, type_tokens.clone().unwrap_or_default());
        let slot = slot_in(&self.adj_tables, &key, ADJ_TABLE_CACHE_MAX);
        // The snapshot a repair was already attempted on and declined. A
        // repair declined once cannot succeed later on the SAME snapshot —
        // its log only loses coverage and only gains entries — so behind the
        // build guard that snapshot is not scanned again; a DIFFERENT one is
        // the winner's, and that one is repaired (see below).
        let mut tried: Option<std::sync::Arc<Snapshot<AdjTable>>> = None;
        // The build guard, if the repair below took it. Held for the rest of
        // the call so the rebuild path further down does not try to take a
        // second one — `enter_build` is a plain mutex and re-entering it on
        // one thread deadlocks.
        let mut held: Option<std::sync::MutexGuard<'_, ()>> = None;
        if let Some(mut snap) = slot.load() {
            if snap.at >= epoch {
                counted!("graph.adjacency tables reused");
                return (Some(snap), AdjOutcome::Current);
            }
            // SINGLE-FLIGHT THE REPAIR, the way the rebuild below already is.
            //
            // A repair used to run here unguarded, so every reader that found
            // the table stale repaired it on its own thread and all but one
            // publish landed in an already-advanced slot. That is the normal
            // case on a mixed profile rather than a race: one write makes the
            // table stale and every reader arriving before the next publish
            // repeats the same work. See `set_single_flight_repair` for the
            // measured redundancy.
            //
            // Behind the guard the slot is re-read, because the whole point of
            // waiting is that someone else may have finished: a reader that
            // waited then returns THEIR table instead of building the same
            // overlay again, and one that is still behind repairs from the
            // freshest base rather than the one it arrived with.
            if self.single_flight_repair.get() {
                held = Some(slot.enter_build());
                if let Some(fresh) = slot.load() {
                    if fresh.at >= epoch {
                        counted!("graph.adjacency tables repaired by another worker");
                        return (Some(fresh), AdjOutcome::Current);
                    }
                    snap = fresh;
                }
            }
            // PRICE THE REPAIR BEFORE A READER PAYS FOR IT (§8).
            //
            // The maintenance pass has done this since §5.3 —
            // `adj_repair_cost_rows` is its budget's meter — and a reader has
            // strictly more reason to: it is on a query thread, it wants ONE
            // node's row, and the repair it is about to run re-reads a row for
            // every node any writer touched since the base. That cost is
            // proportional to the write stream and it is paid per read, which
            // is the whole of §8's interference.
            //
            // `None` means NO REPAIR IS AVAILABLE — the log no longer reaches
            // the table, or the change set is past the repair caps — and it is
            // deliberately not a decline. Only a rebuild fixes that table, the
            // pass's rebuild is demoted (§5.3), so the reader is the only one
            // left who can, and declining would leave the table gone and its
            // span walked for ever. `demoted_adjacency_rebuild` states that as
            // an invariant; it is what caught the first cut of this.
            //
            // ONCE PER SNAPSHOT, not once per read. Pricing walks the change
            // set under the log's lock; a published snapshot is immutable, so
            // the verdict belongs to it and every reader after the first gets
            // it for two atomic loads. Doing it per read was correct and cost
            // the writers 2.7x (`Slot::priced`).
            if reader && self.single_node_stale_walk.get() {
                let decline = match slot.priced(snap.at) {
                    Some(d) => d,
                    None => {
                        let d = self
                            .adj_repair_cost_rows(tag, type_tokens, snap.at, snap.value.len())
                            .is_some_and(|rows| rows > ADJ_READER_REPAIR_MAX_ROWS);
                        slot.note_priced(snap.at, d);
                        d
                    }
                };
                if decline {
                    counted!("graph.adjacency stale table declined to a single-node reader");
                    counters::ADJ_STALE_DECLINED_TO_WALK
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return (None, AdjOutcome::Declined);
                }
            }
            // Genuinely stale — this table's own types changed. Repair the
            // rows that moved rather than rebuilding every row that did not; a
            // rebuild here walked all 447k relationships and showed up as a
            // 62 ms p95 on every traversal shape.
            if let Some((fixed, at)) =
                self.repaired_adj_table(tag, type_tokens, &snap.value, snap.at)
            {
                return self.publish_repaired_adj_table(&slot, tag, fixed, at);
            }
            if !may_rebuild {
                counted!("graph.adjacency rebuild deferred by the refresh budget");
                return (Some(snap), AdjOutcome::Deferred);
            }
            tried = Some(snap);
        }
        // ADMISSION CONTROL ON THE READER'S REBUILD.
        //
        // This guard read `if !admit && tried.is_none()`, so a reader holding a
        // STALE snapshot it could not repair turned the probe-count admission
        // gate OFF and fell through to a full span rebuild — 25 s for an
        // untyped table at SF1 — on its own query thread. A snapshot going
        // stale is not a reason to stop asking whether the table is worth
        // building.
        //
        // That was a corner while the maintenance pass also rebuilt. §5.3
        // demoted that pass, which made READERS the only ones who rebuild at
        // all, and promoted the bypass to the entire policy. This is the other
        // half of that change and should have landed with it.
        //
        // Declining serves the query from the direct span walk: the same truth,
        // read from the store. That fallback is also far cheaper than it was —
        // with the tail copy-out a span read no longer excludes every writer —
        // so the two changes compose rather than trade off.
        let gated = self.reader_rebuild_admission.get();
        if !admit && (tried.is_none() || (gated && reader)) {
            if tried.is_some() {
                counted!("graph.adjacency rebuild declined to a reader by admission");
                sometimes!("graph.a reader was kept off a full-span rebuild", true);
            }
            return (None, AdjOutcome::Declined);
        }
        // Single-flight PER TABLE: a build of another table never holds
        // this one up (the refresh's rebuild of A used to hold every worker
        // building B for the whole walk). Already held when the repair above
        // took it — taking it twice on one thread would deadlock.
        let _build = match held {
            Some(g) => g,
            None => slot.enter_build(),
        };
        if let Some(snap) = slot.load() {
            if snap.at >= epoch {
                counted!("graph.adjacency tables built by another worker");
                return (Some(snap), AdjOutcome::Current);
            }
            // The LOSER'S case under the write fence. The winner published at
            // `fenced(at)`, which is BELOW `epoch` whenever a writer was in
            // flight when it published — so "not at the epoch" does not mean
            // "not built": the slot holds a table walked a moment ago that
            // is short of exactly the in-flight writers' rows, and those are
            // the entries the log holds above the winner's stamp (it pruned
            // only below it). Repair from there; never walk the span again.
            // Measured before this: 4 readers missing at once with one
            // writer in flight did 4 full builds, serially, and a reader
            // waiting behind the refresh's rebuild rebuilt the same table
            // again (`tests/review_build_guard_fenced_loser.rs`).
            if !tried.as_ref().is_some_and(|t| std::sync::Arc::ptr_eq(t, &snap)) {
                if let Some((fixed, at)) =
                    self.repaired_adj_table(tag, type_tokens, &snap.value, snap.at)
                {
                    counted!("graph.adjacency tables repaired behind the build guard");
                    return self.publish_repaired_adj_table(&slot, tag, fixed, at);
                }
            }
            // The log does not reach it, or repair costs more than a walk:
            // build, as any reader would have.
        }
        // `at` BEFORE the walk: every change stamped at or below it has rows
        // the walk sees, because the store hands out stamps under the write
        // lock the walk takes to read. Fenced AFTER it — see `fenced`.
        let at = self.store.now_ts();
        let Some(built) = self.build_adj_table(tag, type_tokens) else {
            return (None, AdjOutcome::Declined);
        };
        let built = std::sync::Arc::new(Snapshot {
            at: self.fenced(at),
            value: std::sync::Arc::new(built),
        });
        slot.publish_snapshot(std::sync::Arc::clone(&built));
        self.prune_adj_logs(tag);
        (Some(built), AdjOutcome::Rebuilt)
    }

    /// Publish a repaired table at its fenced stamp `at` and say what that
    /// did. Served from the handle whether or not the publish won: the
    /// fenced stamp can land ON the slot's current stamp while a writer is
    /// in flight, and the slot then still holds the table this one was
    /// repaired FROM. That case is reported `Deferred`, not `Repaired`: the
    /// slot advanced by nothing, and the maintenance refresh used to count
    /// it as brought current (and bump `DERIVED_REFRESHED_BY_MAINTENANCE`)
    /// while re-repairing the same table on every pass the writer stayed in
    /// flight. The logs are pruned only behind a publish that WON — a lost
    /// one moved no snapshot for a prune to sit behind.
    fn publish_repaired_adj_table(
        &self,
        slot: &Slot<AdjTable>,
        tag: u8,
        fixed: AdjTable,
        at: u64,
    ) -> (Option<std::sync::Arc<Snapshot<AdjTable>>>, AdjOutcome) {
        let fixed = std::sync::Arc::new(Snapshot {
            at,
            value: std::sync::Arc::new(fixed),
        });
        if slot.publish_snapshot(std::sync::Arc::clone(&fixed)) {
            self.prune_adj_logs(tag);
            return (Some(fixed), AdjOutcome::Repaired);
        }
        counted!("graph.adjacency repair publish lost, slot unchanged");
        counters::ADJ_REPAIR_PUBLISH_LOST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (Some(fixed), AdjOutcome::Deferred)
    }

    /// The table for one side and type set at `epoch` — the adjacency epoch of
    /// those types — building it on first use; `None` when it would exceed the
    /// entry budget. Returns an OWNED `Arc` — for the caller that resolves the
    /// table ONCE and reuses it across many nodes (so one refcount bump, not
    /// per-probe). Per-probe expansion uses [`Graph::with_adj_table`] to avoid
    /// the shared-refcount ping-pong.
    fn adj_table(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        epoch: u64,
    ) -> Option<std::sync::Arc<AdjTable>> {
        if !self.adj_tables_usable() {
            return None;
        }
        self.adj_table_snapshot(tag, type_tokens, epoch, true)
            .map(|s| std::sync::Arc::clone(&s.value))
    }

    /// Borrow the `(tag, types)` adjacency table ONCE for a whole hop of
    /// `rows` driving rows (fix 39, `DataChunk::expand`). The single-node
    /// accessor resolves the epoch, the probe gate, the transaction overlay
    /// and the snapshot memo — two contended counters among them — per
    /// visit, ~400 ns of bookkeeping around a ~20 ns row lookup: the
    /// production `(n:UserDataNode {userId})-[r:REPLIED_TO]->(t) RETURN
    /// count(r)` expanded a 38k-email seed for 0 edges in 14–18 ms against
    /// Neo4j's 1.6–1.9. The gate is charged as `rows` visits would charge
    /// it (admission is unchanged). `f` gets `None` — the caller keeps its
    /// per-row accessor — inside a transaction with buffered writes (the
    /// per-row walk overlays them), when the tables are declined, or when
    /// no current table can be had; a stale table is repaired or rebuilt
    /// exactly as a single-node reader would have it.
    pub(crate) fn with_hop_table<R>(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        rows: usize,
        f: impl FnOnce(Option<&AdjTable>) -> R,
    ) -> R {
        if rows == 0 || self.in_txn_with_writes() || !self.adj_tables_usable() {
            return f(None);
        }
        let epoch = self.adjacency_epoch(type_tokens);
        let probe_epoch = self.adj_epoch_now();
        let after = self.degree_table_after.get();
        let mut seen = self.degree_probes.peek(probe_epoch);
        let mut charged = 0usize;
        while seen < after && charged < rows {
            seen = self.degree_probes.tick(probe_epoch) + 1;
            charged += 1;
        }
        let admit = seen >= after;
        self.with_adj_table(tag, type_tokens, epoch, admit, None, f)
    }

    /// Fix 48: the ids with at least one edge in the (tag, types) table —
    /// ascending — or `None` when no table may serve (a writing transaction,
    /// tables declined, or a table not yet admitted for a caller this small).
    pub(crate) fn hop_table_sources(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        rows: usize,
    ) -> Option<Vec<u64>> {
        self.with_hop_table(tag, type_tokens, rows, |tbl| tbl.map(AdjTable::sources))
    }

    /// Run `f` with a BORROWED reference to the (tag,type) table at `epoch`.
    /// The hit path borrows through the arc-swap guards and clones no `Arc`
    /// (16 workers RMWing one table's refcount is a cache-line ping-pong that
    /// collapses concurrent complex-join reads — see the concurrency memo).
    /// `admit` says whether a table that does not yet exist may be BUILT; one
    /// that exists is used either way. `f` gets `None` when there is no table
    /// to use (caller falls back to a direct scan).
    /// The adjacency snapshot this thread resolved last, IF it answers the same
    /// question — same graph, same table, and fresh enough for this caller.
    ///
    /// # Why re-serving one is sound
    ///
    /// A published [`Snapshot`] is IMMUTABLE, and `with_adj_table`'s entire
    /// acceptance rule is `snap.at >= epoch`. So a remembered snapshot that
    /// still clears that bar is precisely what the map walk would have handed
    /// back — at worst one publish behind the newest, which is a table the same
    /// rule admits and which the caller could equally have been handed by
    /// arriving a moment earlier.
    ///
    /// What it must NOT do is answer the questions the rule rejects. A snapshot
    /// with `at < epoch` misses here and falls through to the full path, which
    /// is what keeps the stale-table handling — the change filter, the stale
    /// probe, the repair pricing — reachable rather than quietly bypassed.
    ///
    /// # The invariant this rests on
    ///
    /// That a table's `at` identifies its CONTENT. `Slot::publish` is monotone,
    /// so every republish advances the stamp and the memo re-resolves on its
    /// own. The single exception is
    /// [`Graph::clear_adjacency_sorted_flags`], which republishes altered
    /// tables at their existing stamp on purpose; it drops the memo, and any
    /// future path that changes a table without advancing `at` must do the
    /// same. Keying validity on a clock that does not move with the data is
    /// `derived.rs`'s defect class #1.
    ///
    /// # It runs `f` INSIDE the memo's borrow, and hands `f` back on a miss
    ///
    /// The first cut returned a cloned `Arc<Snapshot>` and let the caller run
    /// `f` against it. That is two atomic read-modify-writes — the clone and
    /// the drop — on every hit, which is every one of q3's 107,386,468 probes,
    /// and under morsel parallelism they all land on ONE refcount line shared
    /// by every worker. Running `f` against the entry while the `Ref` is held
    /// touches no refcount at all. The `Ref` is shared, so the visitor's own
    /// nested probes (the fold recurses INSIDE `with_adj_table`'s closure) hit
    /// as before; only a nested MISS cannot memoise while an outer hit is
    /// live — `adj_snap_memo_put` declines with `try_borrow_mut` rather than
    /// panic, and the next outer miss files it.
    ///
    /// `Err(f)` on a miss returns the closure unconsumed, so the caller's full
    /// path runs it exactly as before. That is the whole reason this takes `f`
    /// rather than returning a reference: the borrow cannot outlive the call.
    fn adj_snap_memo_serve<R, F: FnOnce(Option<&AdjTable>) -> R>(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        epoch: u64,
        f: F,
    ) -> Result<R, F> {
        if !self.adj_snap_memo.get() {
            return Err(f);
        }
        let toks: &[u32] = type_tokens.as_deref().unwrap_or(&[]);
        ADJ_SNAP_MEMO.with(|set| {
            for w in &set.ways {
                // A way mutably borrowed by an outer `put` cannot also hold
                // this table (put holds it only while writing); skip it.
                let Ok(g) = w.try_borrow() else { continue };
                let Some(e) = g.as_ref() else { continue };
                if e.graph != self.graph_id || e.tag != tag || e.tokens.as_slice() != toks {
                    continue;
                }
                // Found the table, but this caller needs it fresher. Decline,
                // so the full path runs its stale-table handling as before.
                if e.snap.at < epoch {
                    counted!(
                        "graph.adjacency memo declined an entry older than the epoch asked for"
                    );
                    return Err(f);
                }
                counted!("graph.adjacency tables reused");
                counted!("graph.adjacency snapshot re-served to the same thread");
                return Ok(f(Some(&e.snap.value)));
            }
            counted!("graph.adjacency memo holds no entry for this table");
            Err(f)
        })
    }

    /// Remember `snap` as this thread's last resolved table. Called only where
    /// the full path just accepted it, so the memo can never hold a snapshot
    /// that resolution would have rejected.
    fn adj_snap_memo_put(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        snap: &std::sync::Arc<Snapshot<AdjTable>>,
    ) {
        if !self.adj_snap_memo.get() {
            return;
        }
        let toks: &[u32] = type_tokens.as_deref().unwrap_or(&[]);
        ADJ_SNAP_MEMO.with(|set| {
            // `Option`-wrapped so a write can MOVE it out inside a loop.
            let mut fresh = Some(AdjSnapMemo {
                graph: self.graph_id,
                tag,
                tokens: toks.to_vec(),
                snap: std::sync::Arc::clone(snap),
            });
            // Refresh the entry for this table if a way already holds one — it
            // went stale, which is why the caller reached the full path —
            // rather than filing a second entry for it and evicting a live one.
            // A way an outer frame is SERVING from cannot be written (its
            // shared borrow is live); skip it rather than panic. An outer serve
            // of a STALE entry cannot be live anyway — serve declines on
            // `at < epoch` — so the skip never strands a stale refresh.
            for w in &set.ways {
                let Ok(mut g) = w.try_borrow_mut() else { continue };
                if g.as_ref().is_some_and(|e| {
                    e.graph == self.graph_id && e.tag == tag && e.tokens.as_slice() == toks
                }) {
                    *g = fresh.take();
                    return;
                }
            }
            // A free writable way, else round-robin over the writable ones.
            // Recursion depth is bounded by the plan's hops against eight ways,
            // so a completely unwritable set does not arise; decline loudly if
            // it somehow does.
            for w in &set.ways {
                let Ok(mut g) = w.try_borrow_mut() else { continue };
                if g.is_none() {
                    *g = fresh.take();
                    return;
                }
            }
            for _ in 0..ADJ_SNAP_MEMO_WAYS {
                let v = set.next.get();
                set.next.set((v + 1) % ADJ_SNAP_MEMO_WAYS);
                if let Ok(mut g) = set.ways[v].try_borrow_mut() {
                    *g = fresh.take();
                    return;
                }
            }
            counted!("graph.adjacency memo found no writable way");
        });
    }

    fn with_adj_table<R>(
        &self,
        tag: u8,
        type_tokens: &Option<Vec<u32>>,
        epoch: u64,
        admit: bool,
        node: Option<u64>,
        f: impl FnOnce(Option<&AdjTable>) -> R,
    ) -> R {
        if !self.adj_tables_usable() {
            return f(None);
        }
        // WHAT THIS RESOLUTION COSTS, AND HOW OFTEN. Everything below finds a
        // table that is CONSTANT for a whole hop — only `node` varies — and it
        // was paid once per PROBE: a `Vec<u32>` heap allocation to build the
        // map key, a `BTreeMap` walk whose key comparison chases that vec's
        // pointer, and two `ArcSwap` guards. LSQB q2 calls this 5,091,585
        // times, 2,018,101 of them from a KNOWS close whose bound side is the
        // SEED, so ~102 consecutive probes ask the identical question.
        let f = match self.adj_snap_memo_serve(tag, type_tokens, epoch, f) {
            Ok(r) => return r,
            Err(f) => f,
        };
        let key = (tag, type_tokens.clone().unwrap_or_default());
        {
            let map = self.adj_tables.load();
            if let Some(slot) = map.get(&key) {
                let g = slot.peek();
                if let Some(snap) = g.as_ref() {
                    if snap.at >= epoch {
                        counted!("graph.adjacency tables reused");
                        self.adj_snap_memo_put(tag, type_tokens, snap);
                        return f(Some(&snap.value));
                    }
                    // STALE AS A TABLE, CURRENT FOR THIS NODE.
                    //
                    // `node` is `Some` only where `f` reads that ONE node's
                    // row — the two single-node accessors. That restriction is
                    // what makes serving a stale table sound here and is why
                    // this is a parameter rather than a flag: a caller that
                    // touches other rows must pass `None` and cannot opt in by
                    // accident.
                    //
                    // Without this, one write makes the table stale and EVERY
                    // reader arriving before the next publish repairs the
                    // whole change set on its own query thread. Measured on
                    // `balattr` at 8 clients 50/50, that was ~12.5k repairs/s
                    // carrying ~2,075 overlay rows each, and it made the mix
                    // 5x slower than the write-only arm that should have
                    // bracketed it. Single-flighting those repairs made it
                    // WORSE (readers block instead of duplicating), which is
                    // what says the repair does not belong on the read path at
                    // all rather than that it needs coordinating.
                    if let Some(n) = node {
                        if self.lazy_stale_serve.get() {
                            // THE LOCK-FREE FRONT. One atomic load. Answering
                            // this from the change log instead means taking the
                            // lock every write holds exclusively — see
                            // `AdjChangeFilter`.
                            if self.adj_change_filter_on.get()
                                && self.adj_change_filter.unchanged_since(tag, n, snap.at)
                            {
                                counted!("graph.adjacency change filter cleared a node");
                                counted!("graph.adjacency stale table served an unmoved node");
                                counters::ADJ_STALE_SERVED_UNMOVED
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                return f(Some(&snap.value));
                            }
                            // The filter said "maybe" (a real change, or a slot
                            // collision). Ask the log, which is exact.
                            match self.adj_stale_probe(tag, type_tokens, snap.at, n) {
                                StaleProbe::Unmoved => {
                                    counted!(
                                        "graph.adjacency stale table served an unmoved node"
                                    );
                                    counters::ADJ_STALE_SERVED_UNMOVED
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    return f(Some(&snap.value));
                                }
                                // A short delta: repairing is cheap and leaves
                                // a fresh table for the readers behind us, so
                                // fall through to it exactly as before.
                                StaleProbe::MovedSmallDelta => {}
                                // A long delta, or a log that cannot answer.
                                // Neither is decided here: both fall through to
                                // `adj_table_snapshot`, which PRICES the repair
                                // with the same function the maintenance pass
                                // uses and declines a reader whose repair would
                                // exceed `ADJ_READER_REPAIR_MAX_ROWS`.
                                //
                                // The first cut declined right here, and
                                // `demoted_adjacency_rebuild` caught what that
                                // costs: a table whose log no longer reaches it
                                // can only come back through a REBUILD, and
                                // with the pass's rebuild demoted (§5.3) the
                                // reader is the only one left who does that. A
                                // reader that declines without asking whether a
                                // repair was even available leaves the table
                                // gone and the span walked for ever.
                                StaleProbe::LongDelta | StaleProbe::Uncovered => {}
                            }
                        }
                    }
                }
            }
        }
        match self.adj_table_snapshot(tag, type_tokens, epoch, admit) {
            Some(snap) => f(Some(&snap.value)),
            None => f(None),
        }
    }

    /// Resolve type names to sorted tokens without minting; `None` = any
    /// type; `Some(empty)` = a named type that was never minted (no rows).
    pub fn type_tokens_peek(&self, types: &[String]) -> Option<Vec<u32>> {
        if types.is_empty() {
            return None;
        }
        let mut v: Vec<u32> = types
            .iter()
            .filter_map(|t| self.type_token_peek(t))
            .collect();
        v.sort_unstable();
        Some(v)
    }

    /// Whether `from` has a `types`-typed relationship in `dir` to ANY node
    /// carrying every one of `peer_labels` — `exists((e)-[:T]->(:Label))`
    /// with the far end unbound. Membership snapshots are id-sorted, so
    /// each label test is a binary search.
    pub fn adjacency_probe_labeled(
        &self,
        from: u64,
        dir: Dir,
        types: &[String],
        peer_labels: &[String],
    ) -> Result<bool, GraphError> {
        let tokens = self.type_tokens_peek(types);
        if matches!(&tokens, Some(v) if v.is_empty()) {
            return Ok(false);
        }
        let mut sets = Vec::with_capacity(peer_labels.len());
        for l in peer_labels {
            sets.push(self.members(Some(l))?);
        }
        for e in self.adjacent_slim(from, dir, &tokens) {
            if sets.iter().all(|m| m.contains(e.peer)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `from` has a `types`-typed relationship in `dir` to ANY node
    /// in `set` (sorted ascending) — the far end of `exists((e)-[:T]->(:L
    /// {k: v}))` resolved once into the ids of `:L` satisfying the map, so
    /// each member's probe is a walk of its adjacency with a binary search
    /// per neighbour, never a node read. Types resolve through `token_peek`
    /// (a probe never mints); a named type never seen contributes no edges.
    pub fn adjacency_probe_in_set(
        &self,
        from: u64,
        dir: Dir,
        types: &[String],
        set: &[u64],
    ) -> Result<bool, GraphError> {
        if set.is_empty() {
            return Ok(false);
        }
        let tokens = self.type_tokens_peek(types);
        if matches!(&tokens, Some(v) if v.is_empty()) {
            return Ok(false);
        }
        for e in self.adjacent_slim(from, dir, &tokens) {
            if set.binary_search(&e.peer).is_ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `from` has any `types`-typed relationship toward `to` in
    /// `dir` — the both-endpoints-bound existence probe. Answered from a
    /// per-(node, direction) adjacency snapshot cached on the commit
    /// clock: `exists((e)-[:T]->(c))` inside a cartesian WHERE evaluates
    /// per PAIR (1.16M times on the production risk-by-country statement),
    /// and a fresh O(degree)-with-setup scan per evaluation was that
    /// statement's second wall. One scan per probed node per epoch; the
    /// probe itself is a walk of a degree-sized vec.
    ///
    /// Types resolve through `token_peek` — a probe never mints. A type
    /// never seen contributes no edges.
    pub fn adjacency_probe(
        &self,
        from: u64,
        dir: Dir,
        types: &[String],
        to: u64,
    ) -> Result<bool, GraphError> {
        let mut type_tokens: Option<Vec<u32>> = None;
        if !types.is_empty() {
            let mut v = Vec::with_capacity(types.len());
            for t in types {
                if let Some(tok) = self.token_peek("typ:", &self.types, t) {
                    v.push(tok);
                }
            }
            if v.is_empty() {
                return Ok(false); // no named type has ever been minted
            }
            v.sort_unstable();
            type_tokens = Some(v);
        }
        let tags: &[u8] = match dir {
            Dir::Out => b"O",
            Dir::In => b"I",
            Dir::Both => b"OI",
        };
        // Current at the adjacency epoch of the probed types (read BEFORE the
        // scan): a node or property write leaves a per-node snapshot current.
        let epoch = self.adjacency_epoch(&type_tokens);
        for tag in tags {
            // The per-node snapshots are committed state: neither served nor
            // stored while a transaction with buffered writes is active.
            let private = self.in_txn_with_writes();
            let snapshot = if private {
                None
            } else {
                let cache = self.adj_cache.borrow();
                match cache.get(&(from, *tag)) {
                    Some((at, snap)) if *at >= epoch => Some(std::sync::Arc::clone(snap)),
                    _ => None,
                }
            };
            let snapshot = match snapshot {
                Some(s) => s,
                None => {
                    counted!("graph.adjacency snapshots built");
                    let mut pairs: Vec<(u32, u64)> = Vec::new();
                    let mut want = vec![*tag];
                    want.extend_from_slice(&from.to_be_bytes());
                    for body in self.index_bodies(&want) {
                        if body.len() != 1 + 8 + 4 + 8 + 8 {
                            continue;
                        }
                        let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                        let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
                        pairs.push((t, peer));
                    }
                    let rc = std::sync::Arc::new(pairs);
                    if !private {
                        let mut cache = self.adj_cache.borrow_mut();
                        if cache.len() >= 65_536 {
                            // A blunt, honest bound: the cache is a per-epoch
                            // accelerator, not a store. Clearing keeps memory
                            // proportional to the working set, never the graph.
                            cache.clear();
                        }
                        cache.insert((from, *tag), (epoch, std::sync::Arc::clone(&rc)));
                    }
                    rc
                }
            };
            for (t, peer) in snapshot.iter() {
                if *peer != to {
                    continue;
                }
                if let Some(tt) = &type_tokens {
                    if tt.binary_search(t).is_err() {
                        continue;
                    }
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// How many `type_tokens`-typed relationships `from` has toward `to` in
    /// `dir` — the both-endpoints-bound MULTIPLICITY probe (parallel edges
    /// count each), answered from the sorted CSR row without walking it.
    ///
    /// A single-type table's row for `from` is ordered by `(peer, rel)` — the
    /// key layout, carried through the k-way merge that builds the table and
    /// the prefix scan that repairs a row — and `sorted_by_peer` records that
    /// the build CHECKED it. So the count of rows whose peer is `to` is two
    /// `partition_point`s: O(log degree) where `adjacency_probe`'s per-node
    /// snapshot and the closing semijoin both walk the whole row. On a hub
    /// (the LDBC forum with 10^5 members) that is 17 comparisons instead of
    /// 10^5.
    ///
    /// The walk remains the answer whenever the search is not proven exact:
    /// untyped or multi-type tokens (the row is then ordered by type first),
    /// no table to search (a transaction with buffered rows for this node and
    /// side, an id above `DEGREE_TABLE_MAX_ID`, the entry budget, or an epoch
    /// that has not probed enough to admit a build), a table whose flag is
    /// false, or the lever off. Either way the number is the same; the
    /// `graph.edge probe binary search` / `graph.edge probe walked` counters
    /// say which path produced it, so a test can assert the path and not just
    /// the number.
    ///
    /// `Both` is the O-side count plus the I-side count, except that a
    /// self-loop (`to == from`) is offered ONCE — the O side's — which is the
    /// dedup rule `adjacent_slim_visit` applies row by row, applied here to
    /// the whole I side at once: with `to == from` every I-side match IS a
    /// self-loop, and every one of them is skipped there.
    ///
    /// Types are tokens, resolved by the caller through `type_tokens_peek`;
    /// `Some(empty)` (a named type never minted) has no rows.
    pub fn edge_count_slim(
        &self,
        from: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        to: u64,
    ) -> u64 {
        if matches!(type_tokens, Some(v) if v.is_empty()) {
            return 0;
        }
        let single_type = type_tokens.as_ref().is_some_and(|t| t.len() == 1);
        if !single_type || !self.edge_probe.get() {
            return self.edge_count_walked(from, dir, type_tokens, to);
        }
        // Admission mirrors `adjacent_slim_visit` — the same gate, the same
        // epoch, the same id bound — but PEEKS the probe count rather than
        // ticking it: the walk this falls back to ticks, so a call is charged
        // exactly one probe either way, as every adjacency read is.
        let epoch = self.adjacency_epoch(type_tokens);
        let n = self.degree_probes.peek(self.adj_epoch_now());
        let table_ok = from <= DEGREE_TABLE_MAX_ID;
        let admit = n >= self.degree_table_after.get() && table_ok;
        let both = matches!(dir, Dir::Both);
        let sides: &[u8] = match dir {
            Dir::Out => b"O",
            Dir::In => b"I",
            Dir::Both => b"OI",
        };
        let mut total = 0u64;
        for &tag in sides {
            if tag == b'I' && both && to == from {
                continue; // the O side already counted this self-loop
            }
            // Nine bytes on the stack — see `adjacent_slim_visit`. This is two
            // allocations per CALL on a probe the fold runs ~10^7 times.
            let mut key = [0u8; 1 + 8];
            key[0] = tag;
            key[1..].copy_from_slice(&from.to_be_bytes());
            // A transaction's buffered rows for this node and side are not in
            // the shared table; only the walk overlays them.
            let overlaid = self
                .txn_pending(&self.index, &key[..])
                .is_some_and(|p| !p.is_empty());
            if !table_ok || overlaid {
                return self.edge_count_walked(from, dir, type_tokens, to);
            }
            let side = self.with_adj_table(tag, type_tokens, epoch, admit, Some(from), |tbl| match tbl {
                Some(t) if t.sorted_by_peer => {
                    let row = t.slice(from);
                    let lo = row.partition_point(|e| e.peer < to);
                    let hi = row.partition_point(|e| e.peer <= to);
                    Some((hi - lo) as u64)
                }
                // No table, or one whose order was never established: the
                // whole call walks, so one counter fires per call, not one
                // per side.
                _ => None,
            });
            match side {
                Some(c) => total += c,
                None => return self.edge_count_walked(from, dir, type_tokens, to),
            }
        }
        counted!("graph.edge probe binary search");
        total
    }

    /// Visit exactly the adjacency entries from `from` whose peer is `to`.
    ///
    /// The same two `partition_point`s `edge_count_slim` uses, but yielding the
    /// matching ENTRIES rather than only how many there are — so a caller that
    /// must inspect each relationship (to exclude the ones its walk has already
    /// traversed, say) no longer has to choose between a count it cannot filter
    /// and a linear scan of the whole neighbour list.
    ///
    /// That choice was the fold's close: with a non-empty isomorphism set it
    /// walked all ~36 neighbours of every candidate to find at most one, ~12.8M
    /// times on LSQB q3. Pre-testing with a count first was measured and
    /// reverted — it bought q3 5% and cost 6-14% on four queries whose closes
    /// SUCCEED, where the extra probe is pure overhead. This has no such trade:
    /// a failing close costs the search alone, and a succeeding one costs the
    /// search plus its one or two matching entries.
    ///
    /// Falls back to the linear walk on exactly the conditions `edge_count_slim`
    /// does — multiple types, a table whose peer order was never established, a
    /// transaction overlay, an id past the table bound — so the visited set is
    /// identical either way.
    pub(crate) fn edges_to_peer_slim<F: FnMut(&SlimAdj)>(
        &self,
        from: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        to: u64,
        mut f: F,
    ) {
        if matches!(type_tokens, Some(v) if v.is_empty()) {
            return;
        }
        let single_type = type_tokens.as_ref().is_some_and(|t| t.len() == 1);
        if !single_type || !self.edge_probe.get() {
            self.adjacent_slim_for_each(from, dir, type_tokens, |e| {
                if e.peer == to {
                    f(e);
                }
            });
            return;
        }
        let epoch = self.adjacency_epoch(type_tokens);
        let n = self.degree_probes.peek(self.adj_epoch_now());
        let table_ok = from <= DEGREE_TABLE_MAX_ID;
        let admit = n >= self.degree_table_after.get() && table_ok;
        let both = matches!(dir, Dir::Both);
        let sides: &[u8] = match dir {
            Dir::Out => b"O",
            Dir::In => b"I",
            Dir::Both => b"OI",
        };
        for &tag in sides {
            if tag == b'I' && both && to == from {
                continue; // the O side already offered this self-loop
            }
            let mut key = [0u8; 1 + 8];
            key[0] = tag;
            key[1..].copy_from_slice(&from.to_be_bytes());
            let overlaid = self
                .txn_pending(&self.index, &key[..])
                .is_some_and(|p| !p.is_empty());
            if !table_ok || overlaid {
                self.adjacent_slim_for_each(from, dir, type_tokens, |e| {
                    if e.peer == to {
                        f(e);
                    }
                });
                return;
            }
            let served = self.with_adj_table(tag, type_tokens, epoch, admit, Some(from), |tbl| {
                match tbl {
                    Some(t) if t.sorted_by_peer => {
                        let row = t.slice(from);
                        let lo = row.partition_point(|e| e.peer < to);
                        let hi = row.partition_point(|e| e.peer <= to);
                        for e in &row[lo..hi] {
                            f(e);
                        }
                        true
                    }
                    _ => false,
                }
            });
            if !served {
                self.adjacent_slim_for_each(from, dir, type_tokens, |e| {
                    if e.peer == to {
                        f(e);
                    }
                });
                return;
            }
        }
        counted!("graph.edge entries by binary search");
    }

    /// `edge_count_slim`'s fallback: the existing linear walk of `from`'s
    /// adjacency (`adjacent_slim_for_each` — the table's slice when there is
    /// one, the prefix scan with the transaction's rows overlaid when there
    /// is not), counting the rows whose peer is `to`. The `Both` self-loop
    /// dedup is the walk's own.
    fn edge_count_walked(
        &self,
        from: u64,
        dir: Dir,
        type_tokens: &Option<Vec<u32>>,
        to: u64,
    ) -> u64 {
        counted!("graph.edge probe walked");
        let mut n = 0u64;
        self.adjacent_slim_for_each(from, dir, type_tokens, |e| {
            if e.peer == to {
                n += 1;
            }
        });
        n
    }

    /// The canary for the edge probe's differential: clear `sorted_by_peer`
    /// on every cached adjacency table, so the next `edge_count_slim` over any
    /// of them must WALK. A test then asserts the walk answers exactly what
    /// the search did, and that the walked counter — not the search counter —
    /// fired. Returns how many tables were republished unsorted, so the test
    /// can assert the canary landed rather than trust that it did: a canary
    /// over an empty cache flips nothing and proves nothing.
    ///
    /// Republishes into FRESH slots at each table's own epoch (a slot refuses
    /// a same-epoch publish — monotone by construction), so the tables remain
    /// current and nothing is rebuilt behind the test's back.
    ///
    /// # This is the one path where `at` does not identify the content
    ///
    /// Every other republish goes through `Slot::publish`, which is monotone,
    /// so a changed table always carries a changed stamp and any consumer that
    /// keys validity on `at` re-resolves by itself. This hook deliberately
    /// breaks that — same stamp, different table — which is `derived.rs`'s
    /// defect class #1, validity keyed on the wrong clock.
    ///
    /// So it must drop `ADJ_SNAP_MEMO`, whose whole acceptance rule is
    /// `at >= epoch`. Leaving that out did not fail loudly: the memo went on
    /// serving the SORTED snapshot, `edge_count_slim` went on binary-searching,
    /// and the walk canary this hook exists to arm stopped walking while still
    /// reporting that it had flipped its tables.
    ///
    /// It clears the CALLING thread's memo only, which is sufficient because
    /// the hook is a test instrument used from single-threaded tests — and is
    /// the reason it should stay one.
    pub fn clear_adjacency_sorted_flags(&self) -> usize {
        let mut flipped = 0usize;
        self.adj_tables.rcu(|old| {
            flipped = 0; // the closure may run more than once
            let mut new: AdjTables = BTreeMap::new();
            for (key, slot) in old.iter() {
                let fresh = Slot::default();
                if let Some(snap) = slot.load() {
                    fresh.publish(snap.at, std::sync::Arc::new(snap.value.unsorted()));
                    flipped += 1;
                }
                new.insert(key.clone(), std::sync::Arc::new(fresh));
            }
            std::sync::Arc::new(new)
        });
        ADJ_SNAP_MEMO.with(|set| {
            for w in &set.ways {
                if let Ok(mut g) = w.try_borrow_mut() {
                    *g = None;
                }
            }
        });
        flipped
    }

    /// Count a node's adjacency ROWS — the degree — from key bytes alone:
    /// no record decode, no allocation. `Both` dedups self-loops by
    /// arithmetic (a self-loop has one O row and one I row for the same
    /// node, so Both = |O| + |I| - |self-loops in O|), which is exactly
    /// what `rels_of(..).len()` answers, without materialising a `RelRow`
    /// per relationship — the cost that put the degree-histogram census
    /// statement at 812 s.
    pub fn count_adjacent(&self, node: u64, dir: Dir, type_tokens: &Option<Vec<u32>>) -> u64 {
        let count_side = |tag: u8, note_self_loops: bool| -> (u64, u64) {
            let mut n = 0u64;
            let mut self_loops = 0u64;
            let mut want = vec![tag];
            want.extend_from_slice(&node.to_be_bytes());
            for body in self.index_bodies(&want) {
                if body.len() != 1 + 8 + 4 + 8 + 8 {
                    continue;
                }
                if let Some(tt) = type_tokens {
                    let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                    if tt.binary_search(&t).is_err() {
                        continue;
                    }
                }
                n += 1;
                if note_self_loops {
                    let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
                    if peer == node {
                        self_loops += 1;
                    }
                }
            }
            (n, self_loops)
        };
        match dir {
            Dir::Out => count_side(b'O', false).0,
            Dir::In => count_side(b'I', false).0,
            Dir::Both => {
                let (o, loops) = count_side(b'O', true);
                let (i, _) = count_side(b'I', false);
                o + i - loops
            }
        }
    }

    /// `count_adjacent`, answered from a per-epoch degree table once the
    /// statement has proven it will ask many times. The first
    /// `DEGREE_TABLE_AFTER` probes of an epoch go direct; the next one
    /// builds the table for its (direction, types) in a single walk of the
    /// O (and I) prefix, and every later probe is an array read.
    pub fn count_adjacent_memo(&self, node: u64, dir: Dir, type_tokens: &Option<Vec<u32>>) -> u64 {
        // Keyed on the ADJACENCY epoch of these types, not the commit clock: a
        // node or property write leaves every degree table current, and the
        // probe count it gates keeps counting.
        // The gate itself counts per GLOBAL adjacency epoch (one atomic, and a
        // count that alternating type sets in one statement share), so only a
        // relationship write resets it.
        if self.in_txn_with_writes() {
            // A degree table is committed state; the direct count overlays
            // the transaction's own rows.
            return self.count_adjacent(node, dir, type_tokens);
        }
        let epoch = self.adjacency_epoch(type_tokens);
        let n = self.degree_probes.peek(self.adj_epoch_now());
        if n < self.degree_table_after.get() || node > DEGREE_TABLE_MAX_ID {
            self.degree_probes.tick(self.adj_epoch_now());
            return self.count_adjacent(node, dir, type_tokens);
        }
        let key_types = type_tokens.clone().unwrap_or_default();
        let tag = match dir {
            Dir::Out => b'O',
            Dir::In => b'I',
            Dir::Both => b'B',
        };
        let read_at = |table: &DegreeTable| -> u64 {
            sometimes!("graph.degree answered from a table", true);
            let idx = node as usize;
            let counts = table.counts.get(idx).copied().unwrap_or(0) as u64;
            let loops = table.loops.get(idx).copied().unwrap_or(0) as u64;
            counts - loops
        };
        // Fast path: borrow the degree table from the arc-swap guard WITHOUT
        // cloning the inner `Arc`. 16 workers each RMWing one shared table's
        // refcount is a cache-line ping-pong that collapses concurrent
        // complex-join reads (the memo HIT still contended before this).
        {
            let cache = self.degree_tables.load();
            if let Some((e, t)) = cache.get(&(tag, key_types.clone())) {
                if *e >= epoch {
                    return read_at(t);
                }
            }
        }
        // Miss: build once (owned), publish, read from the owned copy.
        let t = std::sync::Arc::new(self.build_degree_table(dir, type_tokens));
        counted!("graph.degree tables built");
        self.degree_tables.rcu(|old| {
            let mut new: DegreeTables = (**old).clone();
            if new.len() >= 64 {
                new.clear();
            }
            new.insert((tag, key_types.clone()), (epoch, std::sync::Arc::clone(&t)));
            std::sync::Arc::new(new)
        });
        read_at(&t)
    }

    /// One walk per direction side over the adjacency prefix. For `Both`
    /// the O and I rows are summed and the O-side self-loops recorded so
    /// the caller subtracts them once — identical arithmetic to
    /// `count_adjacent`.
    fn build_degree_table(&self, dir: Dir, type_tokens: &Option<Vec<u32>>) -> DegreeTable {
        let mut counts: Vec<u32> = Vec::new();
        let mut loops: Vec<u32> = Vec::new();
        let bump = |v: &mut Vec<u32>, id: u64| {
            let i = id as usize;
            if v.len() <= i {
                v.resize(i + 1, 0);
            }
            v[i] = v[i].saturating_add(1);
        };
        let sides: &[u8] = match dir {
            Dir::Out => b"O",
            Dir::In => b"I",
            Dir::Both => b"OI",
        };
        for &tag in sides {
            for body in self.store.scan_bodies_prefix(&self.index, &[tag]) {
                if body.len() != 1 + 8 + 4 + 8 + 8 {
                    continue;
                }
                if let Some(tt) = type_tokens {
                    let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                    if tt.binary_search(&t).is_err() {
                        continue;
                    }
                }
                let node = u64::from_be_bytes(body[1..9].try_into().expect("8"));
                if node > DEGREE_TABLE_MAX_ID {
                    continue;
                }
                bump(&mut counts, node);
                if matches!(dir, Dir::Both) && tag == b'O' {
                    let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
                    if peer == node {
                        bump(&mut loops, node);
                    }
                }
            }
        }
        DegreeTable { counts, loops }
    }

    /// Count the matches of a DIRECTED single-hop pattern
    /// `(:start?)-[:types?]->(:end?)` from adjacency keys and membership
    /// snapshots — no record decodes. Iterates the smaller labelled side
    /// (reversing direction when that side is the far end); with neither
    /// end labelled it walks the O prefix once, since every relationship
    /// has exactly one O row.
    pub fn count_hop(
        &self,
        start_labels: &[String],
        dir: Dir,
        types: &[String],
        end_labels: &[String],
    ) -> Result<u64, GraphError> {
        let start_label = (!start_labels.is_empty()).then_some(());
        let end_label = (!end_labels.is_empty()).then_some(());
        debug_assert!(!matches!(dir, Dir::Both), "undirected hops double-count");
        let mut type_tokens: Option<Vec<u32>> = None;
        if !types.is_empty() {
            let mut v = Vec::with_capacity(types.len());
            for t in types {
                if let Some(tok) = self.token_peek("typ:", &self.types, t) {
                    v.push(tok);
                }
            }
            if v.is_empty() {
                return Ok(0); // no named type was ever minted
            }
            v.sort_unstable();
            type_tokens = Some(v);
        }
        counted!("graph.hop counts answered from adjacency");
        // Neither end labelled: the count store has it — no walk. The active
        // transaction's buffered relationships are its stats delta.
        if start_label.is_none() && end_label.is_none() {
            let base = self.with_stats(|st| match &type_tokens {
                None => st.rels,
                Some(tt) => tt
                    .iter()
                    .map(|t| st.by_type.get(t).copied().unwrap_or(0))
                    .sum(),
            });
            let delta = self
                .with_txn_stats(|d| match &type_tokens {
                    None => d.rels,
                    Some(tt) => tt
                        .iter()
                        .map(|t| d.by_type.get(t).copied().unwrap_or(0))
                        .sum(),
                })
                .unwrap_or(0);
            return Ok(base.saturating_add_signed(delta));
        }
        // A LABELLED count walks the smaller label — 2M nodes for `(:Comment)`
        // at SF1 — and the planner asks the same handful of questions on every
        // plan build. LSQB q2's two-path pattern asked 16 of them across the
        // THREE builds of one statement, ~1 s per execution, and that second
        // was misread as ~1,000 ns per fold leaf until the event trace named
        // it. The answer is a pure function of two clocks: the types'
        // adjacency epoch and the named labels' membership epochs. Memoise on
        // exactly those; a transaction with buffered writes bypasses (its
        // overlay is thread-local and no clock sees it).
        if self.hop_count_memo_on.get() && !self.in_txn_with_writes() {
            let dir_b = match dir {
                Dir::Out => 0u8,
                Dir::In => 1,
                Dir::Both => 2,
            };
            let key: HopCountKey = (
                start_labels.to_vec(),
                dir_b,
                types.to_vec(),
                end_labels.to_vec(),
            );
            let adj_epoch = self.adjacency_epoch(&type_tokens);
            let label_epoch = self.labels_epoch_max(start_labels, end_labels);
            if let Some(e) = self.hop_count_memo.borrow().get(&key) {
                if e.adj_epoch == adj_epoch && e.label_epoch == label_epoch {
                    counted!("graph.hop count served from the memo");
                    return Ok(e.count);
                }
            }
            let count =
                self.count_hop_labelled(start_labels, dir, type_tokens, end_labels, start_label, end_label)?;
            let mut m = self.hop_count_memo.borrow_mut();
            if m.len() >= HOP_COUNT_MEMO_MAX {
                counted!("graph.hop count memo cleared at its cap");
                m.clear();
            }
            m.insert(
                key,
                HopCountEntry {
                    adj_epoch,
                    label_epoch,
                    count,
                },
            );
            return Ok(count);
        }
        self.count_hop_labelled(start_labels, dir, type_tokens, end_labels, start_label, end_label)
    }

    /// The newest membership epoch across every named label — the second clock
    /// a memoised hop count is valid under. A label never minted reads 0, and
    /// minting it later moves its epoch, so absence invalidates itself.
    fn labels_epoch_max(&self, start_labels: &[String], end_labels: &[String]) -> u64 {
        start_labels
            .iter()
            .chain(end_labels)
            .filter_map(|l| self.token_peek("lbl:", &self.labels, l))
            .map(|t| self.label_epoch(t))
            .max()
            .unwrap_or(0)
    }

    /// [`Graph::count_hop`]'s labelled body, unmemoised — one walk of the
    /// smaller labelled side. Extracted so the memo above wraps every return
    /// path at once instead of intercepting three.
    fn count_hop_labelled(
        &self,
        start_labels: &[String],
        dir: Dir,
        type_tokens: Option<Vec<u32>>,
        end_labels: &[String],
        start_label: Option<()>,
        end_label: Option<()>,
    ) -> Result<u64, GraphError> {
        // Iterate the smaller labelled side; the other side filters by
        // membership. Direction flips when iterating the far end.
        let start_members = match start_label {
            Some(()) => Some(self.members_all(start_labels)?),
            None => None,
        };
        let end_members = match end_label {
            Some(()) => Some(self.members_all(end_labels)?),
            None => None,
        };
        let iterate_start = match (&start_members, &end_members) {
            (Some(a), Some(b)) => a.len() <= b.len(),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => unreachable!("handled above"),
        };
        // The far side stays a MEMBERSHIP VIEW and is tested per peer with
        // `members_contains` (P-3). It used to be materialised into a
        // `BTreeSet` — up to 2M inserts per walk, itself most of a walk's
        // cost, the same defect §24 removed from the estimator's path.
        let (iter, walk_dir, far) = if iterate_start {
            (start_members.expect("labelled"), dir, end_members)
        } else {
            let flipped = match dir {
                Dir::Out => Dir::In,
                Dir::In => Dir::Out,
                Dir::Both => Dir::Both,
            };
            (end_members.expect("labelled"), flipped, start_members)
        };
        let tag = match walk_dir {
            Dir::Out => b'O',
            Dir::In => b'I',
            Dir::Both => unreachable!(),
        };
        // One end free: the count is the sum of the members' degrees, and
        // the degree table has those. `(g:GeopoliticalEvent)-[:T]->
        // (s:NewsStory) RETURN count(*)` measured 2.2 s on the production
        // port probing adjacency per member.
        if far.is_none() {
            let mut n = 0u64;
            for node in iter.iter() {
                n += self.count_adjacent_memo(node, walk_dir, &type_tokens);
            }
            sometimes!("graph.hop count summed from the degree table", true);
            return Ok(n);
        }
        // Both ends labelled: walk the adjacency table's slices against the
        // far membership (sorted; binary search) when the table is within
        // its entry budget.
        if let (Some(fm), false) = (&far, self.in_txn_with_writes()) {
            // The shared table is committed state; a transaction with
            // buffered adjacency rows falls through to the per-node probes
            // below, which overlay.
            let epoch = self.adjacency_epoch(&type_tokens);
            if let Some(table) = self.adj_table(tag, &type_tokens, epoch) {
                let mut n = 0u64;
                for node in iter.iter() {
                    for e in table.slice(node) {
                        if self.members_contains(fm, e.peer) {
                            n += 1;
                        }
                    }
                }
                sometimes!("graph.hop count walked the adjacency table", true);
                return Ok(n);
            }
        }
        sometimes!("graph.hop count fell back to per-node probes", true);
        let overlaid = self.in_txn_with_writes();
        let mut n = 0u64;
        for node in iter.iter() {
            let mut want = vec![tag];
            want.extend_from_slice(&node.to_be_bytes());
            let bodies = if overlaid {
                self.index_bodies(&want)
            } else {
                self.store.scan_bodies_prefix(&self.index, &want)
            };
            for body in bodies {
                if body.len() != 1 + 8 + 4 + 8 + 8 {
                    continue;
                }
                if let Some(tt) = &type_tokens {
                    let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                    if tt.binary_search(&t).is_err() {
                        continue;
                    }
                }
                if let Some(fm) = &far {
                    let peer = u64::from_be_bytes(body[13..21].try_into().expect("8"));
                    if !self.members_contains(fm, peer) {
                        continue;
                    }
                }
                n += 1;
            }
        }
        Ok(n)
    }

    /// The relationship-type histogram — (type name, count) over every
    /// live relationship — from ONE walk of the O adjacency side. The
    /// type token is in the key; the record is never touched.
    pub fn rel_type_histogram(&self) -> Result<Vec<(String, u64)>, GraphError> {
        let mut counts = self.with_stats(|st| st.by_type.clone());
        // The active transaction's buffered relationships move the histogram
        // too — the shared counts are committed state only.
        self.with_txn_stats(|d| {
            for (t, delta) in &d.by_type {
                let v = counts.entry(*t).or_insert(0);
                *v = v.saturating_add_signed(*delta);
            }
        });
        let mut out = Vec::with_capacity(counts.len());
        for (t, n) in counts {
            if n > 0 {
                out.push((self.token_name("typ:", t)?, n));
            }
        }
        Ok(out)
    }

    /// The label histogram — (label name, live node count) — the same shape
    /// as [`Graph::rel_type_histogram`] over the label stats. `db.labels`
    /// reads it; a label whose last node was deleted does not appear, which
    /// is Neo4j's behaviour for that procedure too.
    pub fn label_histogram(&self) -> Result<Vec<(String, u64)>, GraphError> {
        let mut counts = self.with_stats(|st| st.by_label.clone());
        self.with_txn_stats(|d| {
            for (t, delta) in &d.by_label {
                let v = counts.entry(*t).or_insert(0);
                *v = v.saturating_add_signed(*delta);
            }
        });
        let mut out = Vec::with_capacity(counts.len());
        for (t, n) in counts {
            if n > 0 {
                out.push((self.token_name("lbl:", t)?, n));
            }
        }
        Ok(out)
    }

    /// Every property key ever minted, sorted — `db.propertyKeys` reads it.
    ///
    /// Enumerated from the catalog's REVERSE rows (`rev:prop:<token>` → name)
    /// rather than the in-process forward cache, which only holds the names
    /// this process has touched and would under-report after a restart.
    /// Neo4j's procedure likewise lists keys ever created, deleted values
    /// included — a token, once minted, never unminted.
    pub fn property_key_names(&self) -> Result<Vec<String>, GraphError> {
        let mut out = Vec::new();
        for (_key, value) in self.store.scan_body_prefix(&self.kv, b"rev:prop:") {
            out.push(
                String::from_utf8(value)
                    .map_err(|_| GraphError::Corrupt("property key name utf8".into()))?,
            );
        }
        out.sort();
        Ok(out)
    }

    /// One property's column over the whole node partition, id-sorted and
    /// decoded — the columnar read the batch scan walks. A property never
    /// written is an empty column.
    /// The type name for a minted type token.
    pub(crate) fn type_name(&self, token: u32) -> Result<String, GraphError> {
        self.token_name("typ:", token)
    }

    /// Every live relationship id of the given types (any type when
    /// `types` is empty), id-sorted, with the type token aligned — one
    /// typed walk of the out-adjacency prefix, where each relationship
    /// appears exactly once under its source, cached per (types, epoch).
    /// `None` past the adjacency entry budget: memory is a budget, not a
    /// wish, and the caller has a per-id path.
    pub(crate) fn rel_members(
        &self,
        types: &[String],
    ) -> Result<Option<RelPopulation>, GraphError> {
        if self.in_txn_with_writes() {
            // The population is committed state; the caller's per-id path
            // reads through the transaction's overlay.
            return Ok(None);
        }
        let tokens = self.type_tokens_peek(types);
        if matches!(&tokens, Some(t) if t.is_empty()) {
            return Ok(Some((
                std::sync::Arc::new(Vec::new()),
                Vec::new(),
                std::sync::Arc::new(Vec::new()),
            ))); // never minted
        }
        // A relationship's id, type and endpoints change only through the
        // adjacency rows, so the population is current at the adjacency epoch
        // of its types (read BEFORE the walk).
        let epoch = self.adjacency_epoch(&tokens);
        let key = tokens.clone().unwrap_or_default();
        {
            let cache = self.rel_members_cache.load();
            if let Some((e, pop)) = cache.get(&key) {
                if *e >= epoch {
                    return Ok(Some((
                        std::sync::Arc::clone(&pop.0),
                        pop.1.clone(),
                        std::sync::Arc::clone(&pop.2),
                    )));
                }
            }
        }
        let mut pairs: Vec<(u64, u32, u64, u64)> = Vec::new();
        for body in self.store.scan_bodies_prefix(&self.index, &[TAG_OUT]) {
            if body.len() != 1 + 8 + 4 + 8 + 8 {
                continue;
            }
            let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
            if let Some(tt) = &tokens {
                if tt.binary_search(&t).is_err() {
                    continue;
                }
            }
            if pairs.len() >= self.adj_table_max_entries.get() {
                sometimes!(
                    "graph.relationship population declined by the entry budget",
                    true
                );
                return Ok(None);
            }
            pairs.push((
                u64::from_be_bytes(body[21..29].try_into().expect("8")),
                t,
                u64::from_be_bytes(body[1..9].try_into().expect("8")),
                u64::from_be_bytes(body[13..21].try_into().expect("8")),
            ));
        }
        pairs.sort_unstable();
        let mut ids = Vec::with_capacity(pairs.len());
        let mut toks = Vec::with_capacity(pairs.len());
        let mut ends = Vec::with_capacity(pairs.len());
        for (id, t, src, dst) in pairs {
            ids.push(id);
            toks.push(t);
            ends.push((src, dst));
        }
        counted!("graph.relationship populations built");
        let pop: RelPopulation = (std::sync::Arc::new(ids), toks, std::sync::Arc::new(ends));
        self.rel_members_cache.rcu(|old| {
            let mut new: RelMembersCache = (**old).clone();
            if new.len() >= 32 {
                new.clear();
            }
            new.insert(
                key.clone(),
                (
                    epoch,
                    (
                        std::sync::Arc::clone(&pop.0),
                        pop.1.clone(),
                        std::sync::Arc::clone(&pop.2),
                    ),
                ),
            );
            std::sync::Arc::new(new)
        });
        Ok(Some(pop))
    }

    /// The ids in `[lo, hi)` carrying `prop` — presence only, no value
    /// decoded; `None` past `budget`, as for the value read.
    pub(crate) fn column_presence_bounded_in(
        &self,
        family: ColumnFamily,
        prop: &str,
        lo: u64,
        hi: Option<u64>,
        budget: usize,
    ) -> Result<Option<Vec<u64>>, GraphError> {
        let prefix = match family {
            ColumnFamily::Nodes => &self.nodes,
            ColumnFamily::Rels => &self.rels,
        };
        let Some(token) = self.token_peek("prop:", &self.props, prop) else {
            return Ok(Some(Vec::new()));
        };
        let hi_body = hi.map(u64::to_be_bytes);
        let Some(keys) = self.store.scan_column_presence_at(
            prefix,
            &lo.to_be_bytes(),
            hi_body.as_ref().map(|b| b.as_slice()),
            token,
            u64::MAX,
            budget,
        ) else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(keys.len());
        for body in keys {
            if let Ok(b) = <[u8; 8]>::try_from(body.as_slice()) {
                out.push(u64::from_be_bytes(b));
            }
        }
        Ok(Some(out))
    }

    /// The ids carrying ANY of `labels` — the sorted union of the per-label
    /// membership snapshots. `MATCH (m) WHERE m:A OR (m:B AND …)` scanned
    /// every node for a population two labels bound.
    pub fn members_any(&self, labels: &[String]) -> Result<MembersView, GraphError> {
        let mut sets = Vec::with_capacity(labels.len());
        for l in labels {
            sets.push(self.members(Some(l))?.to_arc_vec());
        }
        let mut acc: Vec<u64> = Vec::new();
        for set in sets {
            if acc.is_empty() {
                acc = set.as_ref().clone();
                continue;
            }
            let mut merged = Vec::with_capacity(acc.len() + set.len());
            let (mut i, mut j) = (0, 0);
            while i < acc.len() && j < set.len() {
                match acc[i].cmp(&set[j]) {
                    std::cmp::Ordering::Less => {
                        merged.push(acc[i]);
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        merged.push(set[j]);
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        merged.push(acc[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            merged.extend_from_slice(&acc[i..]);
            merged.extend_from_slice(&set[j..]);
            acc = merged;
        }
        sometimes!("graph.label union population merged", true);
        Ok(MembersView::from_base(std::sync::Arc::new(acc)))
    }

    /// One property's column over a family, bounded to `[lo, hi)` and
    /// budgeted: `None` when the column holds more than `budget` entries
    /// there — the caller's signal that a per-id read is cheaper.
    pub(crate) fn column_entries_bounded_in(
        &self,
        family: ColumnFamily,
        prop: &str,
        lo: u64,
        hi: Option<u64>,
        budget: usize,
    ) -> Result<Option<Vec<(u64, Value)>>, GraphError> {
        let prefix = match family {
            ColumnFamily::Nodes => &self.nodes,
            ColumnFamily::Rels => &self.rels,
        };
        let Some(token) = self.token_peek("prop:", &self.props, prop) else {
            return Ok(Some(Vec::new()));
        };
        let hi_body = hi.map(u64::to_be_bytes);
        let mut out: Vec<(u64, Value)> = Vec::new();
        let mut undecodable = false;
        let visited = self.store.scan_column_range_with(
            prefix,
            &lo.to_be_bytes(),
            hi_body.as_ref().map(|b| b.as_slice()),
            token,
            u64::MAX,
            budget,
            &mut |body, tagged| {
                let Ok(b) = <[u8; 8]>::try_from(body) else {
                    return;
                };
                match decode_prop_opt(tagged) {
                    Some(v) => out.push((u64::from_be_bytes(b), v)),
                    None => undecodable = true,
                }
            },
        );
        if visited.is_none() {
            return Ok(None);
        }
        if undecodable {
            return Err(GraphError::Corrupt(format!("undecodable property {prop}")));
        }
        settle_column(&mut out);
        Ok(Some(out))
    }

    /// The POINT-GATHER counterpart of [`Graph::column_entries_bounded_in`]:
    /// fetch one property's value for EXACTLY the ids in `ids` by per-id point
    /// reads — O(ids), never a range scan — returning the present `(id, Value)`
    /// pairs, id-sorted, BYTE-IDENTICAL to what the range scan yields for the
    /// same ids and property. The caller falls here when the range scan DECLINES
    /// (its `[lo, hi)` span holds more entries than the budget because the id set
    /// is SPARSE): a scattered grouping key over a hash join spans most of the id
    /// space, and every node type carries `id`, so the range would visit the
    /// world to read a handful of rows. Point reads are the store's strength.
    ///
    /// Mirrors `column_entries_bounded_in` EXACTLY: the same `prop:` token peek
    /// (an unminted token ⇒ empty, a property nothing ever wrote is absent
    /// everywhere), the store's newest-version point `get` per id (the MVCC
    /// resolution `scan_column_range_with` performs, resolved directly), the same
    /// `get_property` tagged-value bytes the scan feeds its visitor (a block row
    /// reassembles to the canonical record whose `get_property` returns that
    /// column's bytes; a row-form record returns them directly), the same
    /// `decode_prop_opt` decode with the same `Corrupt` on an undecodable value,
    /// and the same `settle_column`. An id with no record, or a record without
    /// the property, is OMITTED — exactly as the scan omits an absent entry, so
    /// `align` fills it Null on both paths. Never declines; it may propagate a
    /// real `Err`, and it reads exactly `ids.len()` entries whereas the scan
    /// declined only after visiting more than `budget` (≥ `4 × members`), so the
    /// gather is never more work than the scan it replaces.
    pub(crate) fn column_entries_gather(
        &self,
        family: ColumnFamily,
        prop: &str,
        ids: &[u64],
    ) -> Result<Vec<(u64, Value)>, GraphError> {
        let prefix = match family {
            ColumnFamily::Nodes => &self.nodes,
            ColumnFamily::Rels => &self.rels,
        };
        let Some(token) = self.token_peek("prop:", &self.props, prop) else {
            return Ok(Vec::new()); // a property nothing ever wrote — absent everywhere
        };
        counted!("graph.column point-gather");
        let mut out: Vec<(u64, Value)> = Vec::with_capacity(ids.len());
        for &id in ids {
            let Some(bytes) = self.store.get(prefix, &id.to_be_bytes()) else {
                continue; // no record for this id — absent, as the scan omits it
            };
            let Some(tagged) = get_property(&bytes, PropertyId(token)) else {
                continue; // record present, property absent — absent, as the scan omits it
            };
            match decode_prop_opt(&tagged) {
                Some(v) => out.push((id, v)),
                None => return Err(GraphError::Corrupt(format!("undecodable property {prop}"))),
            }
        }
        settle_column(&mut out);
        Ok(out)
    }

    /// The PRESENCE form of [`Graph::column_entries_gather`]: which of `ids`
    /// carry `prop` at all, by one borrowed point read per member, never
    /// decoding a value — the same question `scan_column_presence_at` answers
    /// and the same tolerance of an undecodable value (neither looks at it).
    /// Until this existed a presence read whose range scan declined took the
    /// WHOLE columnar stage down with it, and the general path then
    /// materialised every member in full: the production NewsArticle
    /// enrichment count (`a.promptInjectionRisk IS NULL OR …`, 150k members)
    /// grew the resident set by 6.75 GB per execution that way.
    pub(crate) fn column_presence_gather(
        &self,
        family: ColumnFamily,
        prop: &str,
        ids: &[u64],
    ) -> Result<Vec<u64>, GraphError> {
        let prefix = match family {
            ColumnFamily::Nodes => &self.nodes,
            ColumnFamily::Rels => &self.rels,
        };
        let Some(token) = self.token_peek("prop:", &self.props, prop) else {
            return Ok(Vec::new()); // a property nothing ever wrote — absent everywhere
        };
        counted!("graph.column presence point-gather");
        let mut out: Vec<u64> = Vec::with_capacity(ids.len());
        for &id in ids {
            let carries = self
                .store
                .get_with(prefix, &id.to_be_bytes(), |bytes| {
                    get_property(bytes, PropertyId(token)).is_some()
                })
                .unwrap_or(false);
            if carries {
                out.push(id);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The RECORD-GATHER form of [`Graph::column_entries_gather`] for SEVERAL
    /// properties at once: each id's record is fetched ONCE and every
    /// requested property is taken from it, so a population whose range
    /// scans declined pays one point read per member rather than one per
    /// member per property. Per column byte-identical to
    /// `column_entries_gather` (same token peek, same `get_property`, same
    /// decode with the same `Corrupt`, same settle). On the paged production
    /// mirror a 143-node label read four properties as four gathers — 572
    /// point reads for 143 records — and a 2-hop's end nodes read two.
    pub(crate) fn column_entries_gather_many(
        &self,
        family: ColumnFamily,
        props: &[String],
        ids: &[u64],
    ) -> Result<Vec<Vec<(u64, Value)>>, GraphError> {
        let prefix = match family {
            ColumnFamily::Nodes => &self.nodes,
            ColumnFamily::Rels => &self.rels,
        };
        let tokens: Vec<Option<u32>> = props
            .iter()
            .map(|p| self.token_peek("prop:", &self.props, p))
            .collect();
        let mut out: Vec<Vec<(u64, Value)>> =
            props.iter().map(|_| Vec::with_capacity(ids.len())).collect();
        if tokens.iter().all(Option::is_none) {
            return Ok(out); // properties nothing ever wrote — absent everywhere
        }
        // Counted as a point-gather too: that is what it is, and the sparse-
        // population tests read that counter to prove the fallback fired.
        counted!("graph.column point-gather");
        counted!("graph.column record-gather");
        for &id in ids {
            // The record is BORROWED from the store (`get_with`), never copied:
            // a wide record — an email body, an embedding — is scanned in place
            // for the requested properties and only those values are copied.
            let found: Option<Result<Vec<(usize, Value)>, GraphError>> =
                self.store.get_with(prefix, &id.to_be_bytes(), |bytes| {
                    let mut got = Vec::with_capacity(tokens.len());
                    for (j, token) in tokens.iter().enumerate() {
                        let Some(token) = token else {
                            continue;
                        };
                        let Some(tagged) = get_property(bytes, PropertyId(*token)) else {
                            continue; // record present, property absent — absent, as the scan omits it
                        };
                        match decode_prop_opt(&tagged) {
                            Some(v) => got.push((j, v)),
                            None => {
                                return Err(GraphError::Corrupt(format!(
                                    "undecodable property {}",
                                    props[j]
                                )));
                            }
                        }
                    }
                    Ok(got)
                });
            let Some(found) = found else {
                continue; // no record for this id — absent, as the scan omits it
            };
            for (j, v) in found? {
                out[j].push((id, v));
            }
        }
        for col in &mut out {
            settle_column(col);
        }
        Ok(out)
    }

    /// The column budget for a scan over `members` ids: `factor ×
    /// members`, the most entries one column read may cost before the scan
    /// declines to the per-id path. Default factor 8 (see the field init: a range-scan entry is ~10x cheaper than a point-get).
    pub(crate) fn columnar_column_budget(&self, members: usize) -> usize {
        self.columnar_column_budget_factor
            .get()
            .saturating_mul(members.max(1))
    }

    /// Whether a relationship type has ever been minted — a read-side
    /// existence check that never mints.
    pub fn type_exists(&self, name: &str) -> bool {
        self.token_peek("typ:", &self.types, name).is_some()
    }

    /// A relationship type's token, if it was ever minted — never mints.
    pub fn type_token_peek(&self, name: &str) -> Option<u32> {
        self.token_peek("typ:", &self.types, name)
    }

    /// A property's token, if it was ever minted — never mints.
    ///
    /// The token is what names a persisted index sidecar (`idx-<token>.idx`),
    /// so this is how a caller asks whether the index it just persisted is the
    /// one a reopened store carries.
    pub fn prop_token_peek(&self, name: &str) -> Option<u32> {
        self.token_peek("prop:", &self.props, name)
    }

    /// Incident relationship IDS — the same rows [`Graph::rels_of`] walks, in
    /// the same order, WITHOUT fetching or decoding a single relationship
    /// record.
    ///
    /// `delete_node` used `rels_of` and then read only `r.id` from each result,
    /// so every incident relationship was decoded once to be thrown away and
    /// once more inside `delete_rel`. The id is already in the adjacency KEY
    /// (`tag | node | type | peer | rel`), so the fetch bought nothing.
    ///
    /// Order matters and is preserved: on the non-transactional path each
    /// `delete_rel` autocommits its own timestamp, so a different visit order
    /// would produce a different commit log byte-for-byte.
    ///
    /// Note what this deliberately does NOT do: it does not filter out ids
    /// whose relationship record is absent. `rels_of` drops those silently,
    /// and callers that need the distinction must ask — see `delete_node`,
    /// where `StillConnected` has to stay exact.
    pub fn incident_rel_ids(
        &self,
        node: u64,
        dir: Dir,
        types: Option<&[String]>,
    ) -> Result<Vec<u64>, GraphError> {
        let mut type_tokens = None;
        if let Some(ts) = types {
            let mut v = Vec::with_capacity(ts.len());
            for t in ts {
                v.push(self.token("typ:", &self.types, t)?);
            }
            v.sort_unstable();
            type_tokens = Some(v);
        }
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for tag in match dir {
            Dir::Out => &[b'O'][..],
            Dir::In => &[b'I'][..],
            Dir::Both => &[b'O', b'I'][..],
        } {
            let mut want = vec![*tag];
            want.extend_from_slice(&node.to_be_bytes());
            for body in self.index_bodies(&want) {
                if body.len() != 1 + 8 + 4 + 8 + 8 {
                    continue;
                }
                let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                if let Some(tt) = &type_tokens {
                    if tt.binary_search(&t).is_err() {
                        continue;
                    }
                }
                let rel_id = u64::from_be_bytes(body[21..29].try_into().expect("8"));
                if seen.insert(rel_id) {
                    out.push(rel_id);
                }
            }
        }
        counted!("graph.incident rel ids enumerated");
        Ok(out)
    }

    /// A node's relationships, optionally filtered by direction and types.
    pub fn rels_of(
        &self,
        node: u64,
        dir: Dir,
        types: Option<&[String]>,
    ) -> Result<Vec<RelRow>, GraphError> {
        let mut type_tokens = None;
        if let Some(ts) = types {
            let mut v = Vec::with_capacity(ts.len());
            for t in ts {
                v.push(self.token("typ:", &self.types, t)?);
            }
            v.sort_unstable();
            type_tokens = Some(v);
        }
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for tag in match dir {
            Dir::Out => &[b'O'][..],
            Dir::In => &[b'I'][..],
            Dir::Both => &[b'O', b'I'][..],
        } {
            let mut want = vec![*tag];
            want.extend_from_slice(&node.to_be_bytes());
            // Bounded by (direction, node): O(degree), never O(all edges).
            for body in self.index_bodies(&want) {
                if body.len() != 1 + 8 + 4 + 8 + 8 {
                    continue;
                }
                let t = u32::from_be_bytes(body[9..13].try_into().expect("4"));
                if let Some(tt) = &type_tokens {
                    if tt.binary_search(&t).is_err() {
                        continue;
                    }
                }
                let rel_id = u64::from_be_bytes(body[21..29].try_into().expect("8"));
                if seen.insert(rel_id) {
                    if let Some(r) = self.rel(rel_id)? {
                        out.push(r);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Traversal direction, from the matched pattern's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Outgoing.
    Out,
    /// Incoming.
    In,
    /// Either.
    Both,
}

impl Dir {
    /// The same edge set asked from the OTHER endpoint: `a-[T]->b` read from
    /// a's `O` row is the set `b-[T]<-a` read from b's `I` row. What flips is
    /// which ROW is looked up, never which edges qualify — the fold's
    /// bound-side probes rely on exactly that to read the hot row.
    pub(crate) fn flipped(self) -> Dir {
        match self {
            Dir::Out => Dir::In,
            Dir::In => Dir::Out,
            Dir::Both => Dir::Both,
        }
    }
}

/// A materialised relationship row.
#[derive(Debug, Clone, PartialEq)]
pub struct RelRow {
    /// Relationship id.
    pub id: u64,
    /// Source node.
    pub src: u64,
    /// Destination node.
    pub dst: u64,
    /// Type name.
    pub rel_type: String,
    /// Properties.
    pub props: BTreeMap<String, Value>,
}

impl RelRow {
    /// As a Cypher value.
    pub fn to_value(&self) -> Value {
        Value::Rel {
            id: self.id,
            src: self.src,
            dst: self.dst,
            rel_type: self.rel_type.clone(),
            props: self.props.clone(),
        }
    }
}

fn membership_prefix(label: u32) -> Vec<u8> {
    let mut v = vec![b'L'];
    v.extend_from_slice(&label.to_be_bytes());
    v
}

/// The successor of `f` in `total_cmp` order — the exclusive upper bound
/// an equality probe needs. Sign-aware bit arithmetic: positives ascend by
/// incrementing bits, negatives ascend (toward zero) by decrementing.
fn next_total_cmp(f: f64) -> f64 {
    let bits = f.to_bits();
    if bits >> 63 == 0 {
        f64::from_bits(bits + 1)
    } else if bits == 0x8000_0000_0000_0000 {
        0.0 // -0.0 steps to +0.0
    } else {
        f64::from_bits(bits - 1)
    }
}

fn membership_row(label: u32, node: u64) -> Vec<u8> {
    let mut v = membership_prefix(label);
    v.extend_from_slice(&node.to_be_bytes());
    v
}

fn adjacency_row(tag: u8, from: u64, rel_type: u32, to: u64, rel: u64) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&from.to_be_bytes());
    v.extend_from_slice(&rel_type.to_be_bytes());
    v.extend_from_slice(&to.to_be_bytes());
    v.extend_from_slice(&rel.to_be_bytes());
    v
}

/// The OCC GUARD row for a node's adjacency (W1.1 of
/// `docs/scale-and-integrity-plan.md`): `'G' ‖ node id`, in the index
/// partition beside the `'L'`/`'O'`/`'I'` families. `create_rel` and
/// `delete_rel` WRITE both endpoints' guards; `delete_node` writes a DELETE
/// of its own — so a relationship write racing a node delete is a
/// write-write conflict on this key in EITHER commit order, and OCC
/// validation aborts one side instead of committing a dangling edge. No
/// content is ever read from a guard; it exists so `current_commit_ts`
/// moves. The known, accepted cost: rel-writes sharing an endpoint
/// serialise through it (the hub case), measured rather than hidden.
fn guard_row(node: u64) -> Vec<u8> {
    let mut v = vec![b'G'];
    v.extend_from_slice(&node.to_be_bytes());
    v
}

// ─── The property codec: Value ↔ tagged bytes ──────────────────────────────

fn encode_label_set(tokens: &[u32]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(tokens.len() * 4);
    for t in tokens {
        payload.extend_from_slice(&t.to_le_bytes());
    }
    let mut out = vec![engram_key::value::Tag::BYTES.byte()];
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn decode_label_set(tagged: Option<&[u8]>) -> Result<Vec<u32>, GraphError> {
    let Some(tagged) = tagged else {
        return Ok(Vec::new());
    };
    if tagged.first() != Some(&engram_key::value::Tag::BYTES.byte()) || tagged.len() < 5 {
        return Err(GraphError::Corrupt("label set tag".into()));
    }
    let len = u32::from_le_bytes(tagged[1..5].try_into().expect("4")) as usize;
    let payload = tagged
        .get(5..5 + len)
        .ok_or_else(|| GraphError::Corrupt("label set length".into()))?;
    if payload.len() % 4 != 0 {
        return Err(GraphError::Corrupt("label set width".into()));
    }
    Ok(payload
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().expect("4")))
        .collect())
}

/// Encode a Cypher value as a tagged property value. Properties are scalars
/// and homogeneous scalar lists — Neo4j's rule, enforced here so the store
/// never holds a shape the wire cannot return.
pub fn encode_prop(v: &Value) -> Result<Vec<u8>, GraphError> {
    use engram_key::value::Tag;
    Ok(match v {
        Value::Bool(b) => vec![Tag::BOOL.byte(), u8::from(*b)],
        Value::Int(i) => {
            let mut out = vec![Tag::INT64.byte()];
            out.extend_from_slice(&i.to_le_bytes());
            out
        }
        Value::Float(f) => {
            let mut out = vec![Tag::FLOAT64.byte()];
            out.extend_from_slice(&f.to_le_bytes());
            out
        }
        Value::Str(s) => {
            let mut out = vec![Tag::STRING.byte()];
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            out
        }
        Value::List(items) => {
            // A TEMPORAL array is variable-width, so it uses the LIST_TEMPORAL
            // envelope: a u32 LENGTH (so the skip rule steps over it as a
            // length-prefixed value), then a u32 count and each element as its
            // own tagged encoding, length-framed. openCypher allows homogeneous
            // temporal arrays as property values.
            if items.iter().any(|v| {
                matches!(
                    v,
                    Value::Date(_)
                        | Value::Time { .. }
                        | Value::LocalTime(_)
                        | Value::DateTime { .. }
                        | Value::LocalDateTime { .. }
                        | Value::Duration { .. }
                )
            }) {
                let mut payload = (items.len() as u32).to_le_bytes().to_vec();
                for item in items {
                    let enc = encode_prop(item)?;
                    payload.extend_from_slice(&(enc.len() as u32).to_le_bytes());
                    payload.extend_from_slice(&enc);
                }
                let mut out = vec![Tag::LIST_TEMPORAL.byte()];
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(&payload);
                return Ok(out);
            }
            // Neo4j's rule, kept: property arrays are HOMOGENEOUS. Fixed-width
            // element types use the canonical packed LIST (count, element tag,
            // packed payloads — the only shape the skip rule accepts for
            // LIST); string arrays use a BYTES envelope with an internal
            // count+len framing, because packed variable-width lists are
            // unrepresentable on purpose at the key layer.
            #[derive(PartialEq, Clone, Copy)]
            enum Elem {
                Int,
                Float,
                Bool,
                Str,
            }
            let mut kind: Option<Elem> = None;
            for item in items {
                let k = match item {
                    Value::Int(_) => Elem::Int,
                    Value::Float(_) => Elem::Float,
                    Value::Bool(_) => Elem::Bool,
                    Value::Str(_) => Elem::Str,
                    other => {
                        return Err(GraphError::BadPropertyValue(format!(
                            "a list property may hold only scalars, got {}",
                            other.type_name()
                        )));
                    }
                };
                match (kind, k) {
                    (None, k) => kind = Some(k),
                    // Neo4j coerces mixed int/float arrays to double[] —
                    // `[1, 0.5]` is a legal property there and must be here.
                    // (Found by the sweep: a seeded float rendering as `1`
                    // produced exactly this literal.)
                    (Some(Elem::Int), Elem::Float) | (Some(Elem::Float), Elem::Int) => {
                        kind = Some(Elem::Float);
                    }
                    (Some(prev), k) if prev != k => {
                        return Err(GraphError::BadPropertyValue(
                            "property arrays are homogeneous (Neo4j's rule)".into(),
                        ));
                    }
                    _ => {}
                }
            }
            match kind.unwrap_or(Elem::Int) {
                Elem::Str => {
                    let mut payload = (items.len() as u32).to_le_bytes().to_vec();
                    for item in items {
                        let Value::Str(s) = item else {
                            unreachable!("checked above")
                        };
                        payload.extend_from_slice(&(s.len() as u32).to_le_bytes());
                        payload.extend_from_slice(s.as_bytes());
                    }
                    let mut out = vec![Tag::BYTES.byte()];
                    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                    out.extend_from_slice(&payload);
                    out
                }
                k => {
                    let elem_tag = match k {
                        Elem::Int => Tag::INT64,
                        Elem::Float => Tag::FLOAT64,
                        Elem::Bool => Tag::BOOL,
                        Elem::Str => unreachable!("handled above"),
                    };
                    let mut out = vec![Tag::LIST.byte()];
                    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                    out.push(elem_tag.byte());
                    for item in items {
                        match (item, k) {
                            // An int in a promoted double[] writes as f64 —
                            // writing i64 bits under a FLOAT64 element tag
                            // would decode as garbage floats.
                            (Value::Int(i), Elem::Float) => {
                                out.extend_from_slice(&(*i as f64).to_le_bytes());
                            }
                            (Value::Int(i), _) => out.extend_from_slice(&i.to_le_bytes()),
                            (Value::Float(f), _) => out.extend_from_slice(&f.to_le_bytes()),
                            (Value::Bool(b), _) => out.push(u8::from(*b)),
                            _ => unreachable!("checked above"),
                        }
                    }
                    out
                }
            }
        }
        Value::Date(days) => {
            let mut out = vec![Tag::DATE.byte()];
            out.extend_from_slice(&days.to_le_bytes());
            out
        }
        Value::Time {
            nanos,
            offset_seconds,
        } => {
            let mut out = vec![Tag::TIME.byte()];
            out.extend_from_slice(&nanos.to_le_bytes());
            out.extend_from_slice(&offset_seconds.to_le_bytes());
            out
        }
        Value::LocalTime(nanos) => {
            let mut out = vec![Tag::LOCAL_TIME.byte()];
            out.extend_from_slice(&nanos.to_le_bytes());
            out
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => match zone {
            None => {
                let mut out = vec![Tag::DATETIME_OFFSET.byte()];
                out.extend_from_slice(&epoch_seconds.to_le_bytes());
                out.extend_from_slice(&nanos.to_le_bytes());
                out.extend_from_slice(&offset_seconds.to_le_bytes());
                out
            }
            Some(z) => {
                // LENGTH-PREFIXED (the skip rule steps over the whole payload by a
                // u32 length right after the tag):
                // [epoch(8)][nanos(4)][offset(4)][zone bytes]. The offset is stored
                // so the id's resolved offset survives without a tz database at read.
                let mut payload = Vec::with_capacity(16 + z.len());
                payload.extend_from_slice(&epoch_seconds.to_le_bytes());
                payload.extend_from_slice(&nanos.to_le_bytes());
                payload.extend_from_slice(&offset_seconds.to_le_bytes());
                payload.extend_from_slice(z.as_bytes());
                let mut out = vec![Tag::DATETIME_ZONE_ID.byte()];
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(&payload);
                out
            }
        },
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            let mut out = vec![Tag::LOCAL_DATETIME.byte()];
            out.extend_from_slice(&epoch_seconds.to_le_bytes());
            out.extend_from_slice(&nanos.to_le_bytes());
            out
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => {
            let mut out = vec![Tag::DURATION.byte()];
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&seconds.to_le_bytes());
            out.extend_from_slice(&nanos.to_le_bytes());
            out
        }
        other => {
            return Err(GraphError::BadPropertyValue(format!(
                "a {} cannot be stored as a property",
                other.type_name()
            )));
        }
    })
}

/// A visited column into id order, one entry per id. The store hands
/// block rows over block by block — two signatures interleaved by id put
/// one property in two blocks — and a key's newest generation first, so a
/// STABLE sort plus a dedup keeps exactly the entry the map-based scan
/// returned. A column from one block is already in order and is not
/// sorted (counted either way).
fn settle_column<T>(col: &mut Vec<(u64, T)>) {
    if col.is_sorted_by_key(|(id, _)| *id) {
        counted!("graph.column visits already in id order");
    } else {
        counted!("graph.column visits sorted across blocks");
        col.sort_by_key(|(id, _)| *id);
    }
    col.dedup_by_key(|(id, _)| *id);
}

fn decode_prop_opt(tagged: &[u8]) -> Option<Value> {
    use engram_key::value::Tag;
    let tag = *tagged.first()?;
    let body = &tagged[1..];
    Some(match Tag::from_byte(tag) {
        t if t == Tag::BOOL => Value::Bool(*body.first()? != 0),
        t if t == Tag::INT64 => Value::Int(i64::from_le_bytes(body.get(..8)?.try_into().ok()?)),
        t if t == Tag::FLOAT64 => Value::Float(f64::from_le_bytes(body.get(..8)?.try_into().ok()?)),
        t if t == Tag::STRING => {
            let len = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
            Value::Str(String::from_utf8(body.get(4..4 + len)?.to_vec()).ok()?)
        }
        t if t == Tag::LIST => {
            let count = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
            let elem = Tag::from_byte(*body.get(4)?);
            let payload = &body[5..];
            let mut items = Vec::with_capacity(count);
            for i in 0..count {
                items.push(match elem {
                    e if e == Tag::INT64 => Value::Int(i64::from_le_bytes(
                        payload.get(i * 8..i * 8 + 8)?.try_into().ok()?,
                    )),
                    e if e == Tag::FLOAT64 => Value::Float(f64::from_le_bytes(
                        payload.get(i * 8..i * 8 + 8)?.try_into().ok()?,
                    )),
                    e if e == Tag::BOOL => Value::Bool(*payload.get(i)? != 0),
                    _ => return None,
                });
            }
            Value::List(items)
        }
        t if t == Tag::LIST_TEMPORAL => {
            // u32 total length (skip framing), then u32 count, then each element
            // as a u32 length + its own tagged encoding, decoded recursively.
            let total = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
            let payload = body.get(4..4 + total)?;
            let count = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
            let mut at = 4usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let len = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                at += 4;
                items.push(decode_prop_opt(payload.get(at..at + len)?)?);
                at += len;
            }
            Value::List(items)
        }
        t if t == Tag::BYTES => {
            // In a user property position, BYTES is the string-array
            // envelope: u32 count, then per string a u32 length + bytes.
            let total = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
            let payload = body.get(4..4 + total)?;
            let count = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
            let mut at = 4usize;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let len = u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?) as usize;
                at += 4;
                items.push(Value::Str(
                    String::from_utf8(payload.get(at..at + len)?.to_vec()).ok()?,
                ));
                at += len;
            }
            if at != payload.len() {
                return None;
            }
            Value::List(items)
        }
        t if t == Tag::DATE => Value::Date(i64::from_le_bytes(body.get(..8)?.try_into().ok()?)),
        t if t == Tag::TIME => Value::Time {
            nanos: i64::from_le_bytes(body.get(..8)?.try_into().ok()?),
            offset_seconds: i32::from_le_bytes(body.get(8..12)?.try_into().ok()?),
        },
        t if t == Tag::LOCAL_TIME => {
            Value::LocalTime(i64::from_le_bytes(body.get(..8)?.try_into().ok()?))
        }
        t if t == Tag::DATETIME_OFFSET => Value::DateTime {
            epoch_seconds: i64::from_le_bytes(body.get(..8)?.try_into().ok()?),
            nanos: u32::from_le_bytes(body.get(8..12)?.try_into().ok()?),
            offset_seconds: i32::from_le_bytes(body.get(12..16)?.try_into().ok()?),
            zone: None,
        },
        t if t == Tag::DATETIME_ZONE_ID => {
            // LENGTH-PREFIXED: [len(4)][epoch(8)][nanos(4)][offset(4)][zone bytes].
            let len = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
            let payload = body.get(4..4 + len)?;
            Value::DateTime {
                epoch_seconds: i64::from_le_bytes(payload.get(..8)?.try_into().ok()?),
                nanos: u32::from_le_bytes(payload.get(8..12)?.try_into().ok()?),
                offset_seconds: i32::from_le_bytes(payload.get(12..16)?.try_into().ok()?),
                zone: Some(String::from_utf8(payload.get(16..)?.to_vec()).ok()?),
            }
        }
        t if t == Tag::LOCAL_DATETIME => Value::LocalDateTime {
            epoch_seconds: i64::from_le_bytes(body.get(..8)?.try_into().ok()?),
            nanos: u32::from_le_bytes(body.get(8..12)?.try_into().ok()?),
        },
        t if t == Tag::DURATION => Value::Duration {
            months: i64::from_le_bytes(body.get(..8)?.try_into().ok()?),
            days: i64::from_le_bytes(body.get(8..16)?.try_into().ok()?),
            seconds: i64::from_le_bytes(body.get(16..24)?.try_into().ok()?),
            nanos: i32::from_le_bytes(body.get(24..28)?.try_into().ok()?),
        },
        _ => return None,
    })
}

/// One property read without decoding the whole record — the skip rule's
/// payoff surfaced at the facade.
pub fn point_read_prop(node_bytes: &[u8], token: u32) -> Option<Value> {
    get_property(node_bytes, PropertyId(token)).and_then(|t| decode_prop_opt(&t))
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The graph facade + interpreter, as a registered subsystem.
pub struct GraphLayer;

impl Subsystem for GraphLayer {
    const NAME: &'static str = "graph";

    fn register() -> Registration {
        Registration::new()
            .crash_point("graph.between_node_and_membership")
            .sometimes("graph.null set removed a property")
            .sometimes("interp.row budget refused a statement")
            .sometimes("interp.unbound WHERE variable refused")
            .sometimes("interp.query concluding with a reading clause refused")
            .sometimes("interp.streamed a read-only chain")
            .sometimes("interp.seed picked the smallest label")
            .sometimes("interp.seed drove from relationships")
            .sometimes("interp.seed probed a range index")
            .sometimes("interp.match_path seeded from a property index")
            .sometimes("interp.match_path sought a multi-key pattern map")
            .sometimes("interp.multi-key seek declined for the label scan")
            .sometimes("interp.merge converged after losing a create race")
            .sometimes("graph.detach skipped an orphan adjacency row")
            .sometimes("interp.pushed conjunct pruned a row")
            .sometimes("interp.top-k bounded the sort")
            .sometimes("interp.clause scan memo reused")
            .sometimes("interp.clause scan answered from the equality index")
            .sometimes("interp.cartesian MATCH split into clauses")
            .sometimes("interp.columnar aggregate scan ran")
            .sometimes("interp.columnar aggregate sought a property index")
            .sometimes("interp.columnar scan lifted an exists probe")
            .sometimes("interp.bare count stage answered from the count store")
            .sometimes("interp.constant count stage answered by the fast count")
            .sometimes("interp.constant count stage answered by the columnar scan")
            .sometimes("interp.constant count stage answered at the RETURN")
            .sometimes("interp.columnar scan took an inline node property map")
            .sometimes("interp.columnar projection scan ran")
            .sometimes("interp.columnar projection sought a property index")
            .sometimes("interp.columnar projection ran over relationships")
            .sometimes("interp.columnar projection materialised the winners late")
            .sometimes("interp.columnar scan read a column for presence only")
            .sometimes("interp.columnar scan narrowed an unlabelled match to a label disjunction")
            .sometimes("interp.columnar scan bound a label from membership")
            .sometimes("interp.seed filtered by columns")
            .sometimes("interp.columnar stage produced a WITH chain")
            .sometimes("interp.columnar stage left a bare carry to its selective seek")
            .sometimes("interp.columnar paths switched off")
            .sometimes("interp.columnar projection deduplicated")
            .sometimes("interp.subquery concluded with a synthesised RETURN")
            .sometimes("interp.columnar hop scan ran")
            .sometimes("interp.columnar hop scan filtered an end by label")
            .sometimes("interp.columnar stage unwound a list")
            .sometimes("interp.columnar stage folded an aggregating breaker")
            .counter("interp.columnar hop scan declined an end column")
            .sometimes("interp.columnar stage fused the next aggregating WITH")
            .sometimes("interp.columnar order sorted a primitive key")
            .sometimes("interp.columnar projection read items over the survivors")
            .sometimes("interp.columnar stage concluded at the RETURN")
            .sometimes("interp.columnar scan bound a degree from the adjacency table")
            .sometimes("interp.columnar scan declined a column wider than its label")
            .sometimes("interp.columnar scan ran over relationships")
            .sometimes("interp.columnar rel scan declined by the entry budget")
            .sometimes("interp.columnar scan bound type(r) from its token")
            .sometimes("graph.relationship population declined by the entry budget")
            .sometimes("interp.exists answered by the adjacency probe")
            .sometimes("interp.exists probed a labelled far end")
            .sometimes("interp.expansion read only adjacency keys")
            .sometimes("interp.count answered from the adjacency list")
            .sometimes("interp.plain limit stopped the producer")
            .sometimes("interp.dead projection demanded presence only")
            .counter("interp.live projection demanded only the properties read after it")
            .counter("interp.columnar column read skipped the span walk for a sparse label")
            .counter("graph.count fast paths")
            .counter("graph.projected node materialisations")
            .counter("graph.nodes materialised in full")
            .counter("graph.column visits already in id order")
            .counter("graph.column visits sorted across blocks")
            .counter("graph.column point-gather")
            .counter("graph.column record-gather")
            .counter("graph.column presence point-gather")
            .counter("interp.columnar projection single-phase nodes")
            .counter("interp.columnar projection stopped at the limit")
            .counter("interp.columnar projection predicate evaluated column-at-a-time")
            .counter("interp.pipeline hop borrowed its adjacency table once")
            .counter("interp.columnar probes answered over the population")
            .counter("interp.type filter folded into its hop")
            .counter("interp.columnar label test answered from membership")
            .counter("interp.expand parallel")
            .counter("graph.projected gets served from columns")
            .sometimes("interp.clause scan memo built without its correlated map")
            .sometimes("interp.clause scan memo joined by its map")
            .sometimes("interp.seed looked a node up by its id")
            .sometimes("interp.hop driven from its bound end")
            .sometimes("interp.var-length ran as a frontier BFS")
            .sometimes("interp.seed sought a property index")
            .counter("graph.rel-driven seeds")
            .counter("graph.range index builds")
            .counter("graph.range index catch-up had nothing for this label")
            .counter("graph.membership snapshots built")
            .counter("graph.adjacency snapshots built")
            .counter("graph.hop counts answered from adjacency")
            .counter("graph.degree tables built")
            .counter("graph.adjacency tables built")
            .counter("graph.adjacency tables published by compaction")
            .counter("graph.membership snapshots published by compaction")
            .counter("graph.adjacency rebuild declined to a reader by admission")
            .sometimes("graph.a reader was kept off a full-span rebuild")
            .counter("graph.adjacency tables adopted from disk")
            .counter("graph.membership snapshots adopted from disk")
            .counter("graph.derived sidecars written")
            .counter("graph.derived sidecars loaded")
            .counter("graph.derived sidecar write failed")
            .counter("graph.derived sidecar refused: body hash")
            .counter("graph.derived sidecar refused: vintage")
            .counter("graph.derived sidecar refused: store is newer")
            .counter("graph.derived sidecar refused: toc hash")
            .counter("graph.derived sidecar refused: record hash")
            .counter("graph.derived sidecar refused: record")
            .counter("graph.derived sidecar refused: unreadable")
            .counter("graph.derived sidecar skipped: the sealed set has not moved")
            .counter("graph.derived sidecar deferred: a base is stale")
            .counter("graph.derived sidecar deferred: the clock moved mid-write")
            .counter("graph.derived sidecar deferred: growth inside the rewrite interval")
            .sometimes("graph.derived sidecar refused for a moved sealed set")
            .sometimes("graph.derived sidecar refused for a store above its stamp")
            .counter("graph.adjacency tables repaired")
            .counter("graph.adjacency tables repaired by another worker")
            .counter("graph.adjacency repair publish lost, slot unchanged")
            .counter("graph.adjacency table overlay folded")
            .counter("graph.adjacency stale table served an unmoved node")
            .counter("graph.adjacency change filter cleared a node")
            .counter("graph.adjacency staleness scan gave up on a long delta")
            .counter("graph.adjacency stale table declined to a single-node reader")
            .counter("graph.adjacency repair declined by cost")
            .counter("graph.adjacency repair declined by the node cap")
            .counter("graph.derived refreshed by maintenance")
            .counter("graph.derived refresh took a repair over its whole budget")
            .counter("graph.edge probe binary search")
            .counter("graph.edge probe walked")
            .counter("graph.stats rebuilt")
            .sometimes("graph.count answered from maintained stats")
            .sometimes("graph.multi-label membership intersected")
            .sometimes("graph.label union population merged")
            .sometimes("graph.hop count summed from the degree table")
            .sometimes("graph.hop count walked the adjacency table")
            .sometimes("graph.hop count fell back to per-node probes")
            .sometimes("graph.adjacency table declined by the entry budget")
            .sometimes("graph.degree answered from a table")
            .sometimes("interp.count answered by the fast path")
            .sometimes("graph.delete refused a connected node")
            .sometimes("interp.optional match produced a null row")
            .sometimes("interp.merge created")
            .sometimes("interp.refused an unsupported construct")
            .sometimes("graph.constraint refused")
            .sometimes("graph.vector query skipped unindexable rows")
            .sometimes("graph.vector planner chose ann")
            .sometimes("graph.vector ann index built")
            .counter("graph.vector ann index builds")
            .counter("graph.vector exact index cached")
            .counter("graph.vector index incrementally maintained")
            .counter("graph.tokens minted")
            .counter("graph.nodes created")
            .counter("graph.rels created")
            .counter("graph.nodes deleted")
            .counter("graph.rels deleted")
            .counter("interp.statements run")
            .counter("interp.unwind pruned a dead scope var")
            .counter("interp.late projection deferred a property")
            .counter("interp.clause scan memos built")
            .counter("interp.clause scan memo indexed")
            .counter("interp.columnar aggregate scans")
            .counter("interp.columnar covered count")
            .counter("interp.bare count stages")
            .counter("interp.columnar rel aggregate scans")
            .counter("interp.columnar projection scans")
            .counter("interp.seeds filtered by columns")
            .counter("interp.columnar stages")
            .counter("interp.columnar hop aggregate scans")
            .counter("interp.columnar hop scan seeded from a sought end")
            .counter("interp.columnar stage hydrated a survivor projected to its continuation")
            .counter("graph.relationship populations built")
            .counter("graph.constraint epoch served from cache")
            .counter("graph.range index catalogue served from cache")
            .counter("interp.pattern map seeks")
            .counter("interp.pattern map seeks declined")
            .counter("interp.seed chose a later, more selective declared key")
            .counter("interp.clause scan memo declined for a declared correlated key")
            .counter("interp.columnar probe resolved its far-end map once")
            .counter("interp.columnar aggregate walked its probes over a seek")
            .counter("interp.columnar probe sought its far end")
            .counter("interp.columnar probe resolved its labelled far end once")
            .counter("interp.columnar seek probed a declared prefix")
            .counter("graph.index prefix probes")
            .counter("interp.columnar seek probed a declared range")
            .counter("graph.index range probes")
            .counter("interp.columnar covered count sought a range")
            .counter("interp.columnar id bound from the walk")
            .counter("interp.seed driven from an existence probe's constant end")
            .counter("interp.pipeline seed driven from the hop's table")
            .counter("interp.pipeline seed emptied by an edgeless type")
            .counter("interp.seed scan cut at the plain limit")
            .counter("interp.columnar projection walk cut at the plain limit")
            .counter("interp.matcher bound a hop end to its demand")
            .counter("interp.matcher bound a hop end from the label's cached columns")
            .counter("interp.stream matcher reused the bound start")
            .counter("interp.stage bound a whole-node output lean for its residual")
            .counter("interp.seed intersected a second declared key")
            .counter("interp.seed column filter evaluated column-at-a-time")
            .counter("interp.seed probe walked its path lean")
            .counter("interp.seed probe's conjunct pruned from the WHERE")
            .counter("interp.matcher bound a hop end bare")
            .counter("interp.constant conjunct folded")
            .counter("interp.subquery hop evaluated column-at-a-time")
            .counter("interp.seed undeclared probe capped at the label")
            .counter("interp.count folded a multi-hop chain")
            .counter("interp.chain count folded into its projection")
            .counter("graph.constant end set resolved")
            .counter("graph.constant end set served from the memo")
            .counter("interp.matcher bound a hop end from the resolved end set")
            .counter("interp.matcher bound a lean relationship")
            .counter("interp.matcher rejected a non-member hop end from membership")
            .counter("interp.merge converged on the refusing node by id")
            .counter("interp.subquery seeded with a lean row")
            .counter("interp.columnar population read its label whole to keep the columns")
            .counter("interp.subquery hop loaded its far end's column whole")
            .counter("interp.columnar whole-label read for a population declined")
            .counter("graph.merge settled behind an in-flight writer")
            .counter("interp.matcher reused the bound start")
            .counter("interp.columnar column read served from the property-column cache")
            .counter("interp.columnar cached column restricted to the population")
            .counter("interp.columnar multi-label column read through its smallest label")
            .counter("graph.property column aligned")
            .counter("graph.property column served aligned")
            .counter("graph.property column kept aligned")
            .counter("interp.columnar covered count sought a prefix")
            .counter("interp.seed column filter walked over a seek")
            .counter("interp.subquery path reversed to its constant end")
            .counter("interp.path driven from its bound end")
            .counter("interp.top-level path reversed to its seekable end")
            .counter("interp.seed starts bound from the label column")
            .counter("interp.comprehension beside a carry reads nothing of it")
            .counter("interp.columnar aggregate walked a selective seek instead of vectorising")
            .counter("interp.prefix projection demanded only the properties read after it")
            .counter("interp.seed column filter took the pattern map")
            .counter("interp.pipeline seed predicates filtered by columns")
            .counter("graph.property column absent everywhere")
            .counter("interp.agg bare group key gathered for its later reads")
            .counter("interp.breaker bound a bare carry lean for the RETURN's top-k")
            .counter("interp.late projection re-materialised a carried node for a survivor")
            .counter("interp.late full carry hydrated eagerly")
            .counter("interp.pipeline bound-var predicate filtered by columns")
            .counter("interp.pipeline bound-var predicate walked its whole label")
            .counter("interp.pipeline bound-var columns read from the label column")
            .counter("interp.seed column filter answered by its seek alone")
            .counter("interp.subquery operands ordered last")
            .counter("cypher.subquery operand skipped by a decided connective")
            .counter("graph.range index served from disk, restricted to the label")
            .counter("index.restricted to a label")
            .counter("interp.stage bound a whole-node output lean for the top-k")
            .counter("interp.top-k key read from the lean row")
            .counter("interp.columnar stage hydrated a bare node for a survivor")
            .counter("interp.pipeline optional admitted a nullable group key")
            .counter("interp.pipeline distinct WITH tail recognised at the top level")
            .counter("interp.pipeline reduce declined: a column over budget")
            .counter("interp.pipeline reduce declined: a group key the column path cannot evaluate")
            .counter("interp.pipeline reduce declined: an aggregate argument the column path cannot evaluate")
            .counter("interp.agg bare return item hydrated for a survivor")
            .counter("interp.where-first WITH tail fused into the RETURN")
            .counter("interp.columnar aggregate walked whole to keep its columns")
            .counter("interp.columnar aggregate counted over cached columns")
            .counter("graph.property column served")
            .counter("graph.property column kept")
            .counter("graph.property column retired by a commit")
            .counter("graph.property column evicted")
            .counter("graph.property column not kept: over budget")
            .counter("interp.merge races converged")
            .counter("graph.id served from a reservation")
            .counter("graph.id reservations minted")
            .counter("graph.incident rel ids enumerated")
            .counter("graph.rels materialised in full")
            .counter("graph.guard rows written volatile")
            .counter("index.label-scoped builds")
            .counter("graph.rejected candidates kept out of the read set")
            .counter("graph.stats applied as a delta")
            .counter("graph.indexes created")
            .counter("graph.constraints created")
            .counter("graph.vector queries")
            .counter("graph.fulltext queries")
            .gate(
                Gate::new(
                    "a null property is ABSENT, not stored",
                    Canary::new("store the null and assert IS NULL semantics diverge from a removed property"),
                ),
            )
            .gate(
                Gate::new(
                    "DELETE without DETACH refuses a connected node",
                    Canary::new("skip the connectivity check and assert the dangling adjacency is observable"),
                ),
            )
            .gate(
                Gate::new(
                    "label membership rows and the record's label set agree",
                    Canary::new("drop the membership write and assert a label scan misses the node a bound read shows"),
                ),
            )
            .gate(
                Gate::new(
                    "an unsupported construct refuses BY NAME",
                    Canary::new("return empty rows for procedures and assert the named refusal test fails"),
                ),
            )
            .gate(
                Gate::new(
                    "the ANN arm is never stale and scores match the exact arm",
                    Canary::new("skip the epoch check and assert a post-build write is invisible to the search"),
                )
                .and_canary(Canary::new("return the f32 dots and assert the arm-identical-scores test fails")),
            )
            .gate(
                Gate::new(
                    "uniqueness holds at the write and at creation",
                    Canary::new("skip the post-image check and assert a SET can duplicate a unique value"),
                )
                .and_canary(Canary::new("skip population validation and assert a constraint certifies violating data")),
            )
    }
}

#[cfg(test)]
mod index_topk_tests {
    //! The index-ordered top-k operator (IC9's lever) must equal the brute-force
    //! expand-then-sort result exactly — for a dense filter (where it wins) AND
    //! a sparse one (where it still must be correct), including ORDER-BY ties on
    //! the indexed key.

    use super::*;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;
    use std::collections::{BTreeMap, BTreeSet};

    /// (message node id, creationDate, message.id, creator node id).
    type Msg = (u64, i64, i64, u64);

    fn brute(msgs: &[Msg], friends: &BTreeSet<u64>, upper: i64, limit: usize) -> Vec<u64> {
        let mut v: Vec<(i64, i64, u64)> = msgs
            .iter()
            .filter(|(_, date, _, creator)| *date < upper && friends.contains(creator))
            .map(|(mid, date, msgid, _)| (*date, *msgid, *mid))
            .collect();
        // (creationDate DESC, message.id ASC)
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.truncate(limit);
        v.into_iter().map(|(_, _, mid)| mid).collect()
    }

    fn build() -> (Graph, Vec<u64>, Vec<Msg>, u32) {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let mut persons = Vec::new();
        for i in 0..50i64 {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(i));
            persons.push(g.create_node(&["Person".into()], &p).expect("person"));
        }
        let empty = BTreeMap::new();
        let mut msgs: Vec<Msg> = Vec::new();
        for i in 0..500i64 {
            // creationDate collides (× ties): the tiebreak on message.id must decide.
            let date = 1000 + (i * 7) % 300;
            let msg_id = 90000 - i; // NOT aligned with node-id order, so a real tiebreak
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(msg_id));
            m.insert("creationDate".to_string(), Value::Int(date));
            let mid = g.create_node(&["Message".into()], &m).expect("msg");
            let creator = persons[(i as usize * 13) % 50];
            g.create_rel(mid, "HAS_CREATOR", creator, &empty)
                .expect("hc");
            msgs.push((mid, date, msg_id, creator));
        }
        let hc = g.type_token_peek("HAS_CREATOR").expect("HAS_CREATOR token");
        (g, persons, msgs, hc)
    }

    #[test]
    fn index_ordered_topk_equals_brute_force_dense_and_sparse() {
        let (g, persons, msgs, hc) = build();
        let run = |friends: &BTreeSet<u64>, upper: i64| {
            g.index_ordered_topk_semijoin(
                "creationDate",
                upper,
                &Some(vec![hc]),
                Dir::Out,
                friends,
                "id",
                20,
                usize::MAX, // unbounded budget: exercise the pure result
            )
            .expect("run")
            .expect("some")
        };

        // Dense filter (40/50 persons) — the operator's win case.
        let dense: BTreeSet<u64> = persons.iter().take(40).copied().collect();
        assert_eq!(
            run(&dense, 1200),
            brute(&msgs, &dense, 1200, 20),
            "dense, upper=1200"
        );
        assert_eq!(
            run(&dense, 1300),
            brute(&msgs, &dense, 1300, 20),
            "dense, all dates"
        );

        // Sparse filter (3/50) — must still be exactly correct.
        let sparse: BTreeSet<u64> = persons.iter().skip(7).take(3).copied().collect();
        assert_eq!(
            run(&sparse, 1300),
            brute(&msgs, &sparse, 1300, 20),
            "sparse"
        );

        // Fewer than K qualifiers, and an empty filter.
        let tiny: BTreeSet<u64> = persons.iter().take(1).copied().collect();
        assert_eq!(
            run(&tiny, 1050),
            brute(&msgs, &tiny, 1050, 20),
            "few qualifiers"
        );
        assert_eq!(
            run(&BTreeSet::new(), 1300),
            Vec::<u64>::new(),
            "empty filter → empty"
        );

        // The cost-model bail: a SPARSE filter under a tight scan budget cannot
        // fill K, so the operator DECLINES (None) → the caller falls back to
        // expand+topk. A dense filter fills within the same budget and serves.
        let declined = g
            .index_ordered_topk_semijoin(
                "creationDate",
                1300,
                &Some(vec![hc]),
                Dir::Out,
                &sparse,
                "id",
                20,
                30,
            )
            .expect("run");
        assert_eq!(declined, None, "sparse under a tight budget must bail");
        let served = g
            .index_ordered_topk_semijoin(
                "creationDate",
                1300,
                &Some(vec![hc]),
                Dir::Out,
                &dense,
                "id",
                20,
                400,
            )
            .expect("run");
        assert_eq!(
            served,
            Some(brute(&msgs, &dense, 1300, 20)),
            "dense fills within budget"
        );
    }
}

#[cfg(test)]
mod anchored_hierarchy_tests {
    //! `collect_anchored_hierarchy` (IC12's stage-1 lever) must produce the same
    //! SET as the general `(tag)-[:HAS_TYPE|IS_SUBCLASS_OF*0..]->(class) WHERE
    //! tag.name='Music' OR class.name='Music'` traversal — anchored on 'Music'
    //! and walking the class hierarchy DOWN, not every tag up.

    use super::*;
    use engram_cypher::parse_statement;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;
    use std::collections::{BTreeMap, BTreeSet};

    fn tagset_from_general(g: &Graph) -> BTreeSet<i64> {
        let src = "MATCH (tag:Tag)-[:HAS_TYPE|IS_SUBCLASS_OF*0..]->(baseTagClass:TagClass) \
             WHERE tag.name = 'Music' OR baseTagClass.name = 'Music' \
             RETURN collect(tag.id) AS tags";
        let q = parse_statement(src).expect("parse");
        let rows = run_query(g, &q, BTreeMap::new()).expect("run").rows;
        let mut out = BTreeSet::new();
        if let Some(Value::List(items)) = rows.first().and_then(|r| r.first()) {
            for v in items {
                if let Value::Int(i) = v {
                    out.insert(*i);
                }
            }
        }
        out
    }

    #[test]
    fn anchored_hierarchy_matches_the_general_traversal() {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let cls = |name: &str| {
            let mut p = BTreeMap::new();
            p.insert("name".to_string(), Value::Str(name.into()));
            g.create_node(&["TagClass".into()], &p).expect("class")
        };
        let music = cls("Music");
        let rock = cls("Rock");
        let pop = cls("Pop");
        let jazz = cls("Jazz");
        let sports = cls("Sports");
        let tennis = cls("Tennis");
        let e = BTreeMap::new();
        // Subclass chains: rock⊂music, pop⊂music, jazz⊂rock (2 levels), tennis⊂sports.
        for (sub, sup) in [(rock, music), (pop, music), (jazz, rock), (tennis, sports)] {
            g.create_rel(sub, "IS_SUBCLASS_OF", sup, &e)
                .expect("subclass");
        }
        // Tags: (id, name, type-class).
        let tag = |id: i64, name: &str, cls: u64| {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(id));
            p.insert("name".to_string(), Value::Str(name.into()));
            let t = g.create_node(&["Tag".into()], &p).expect("tag");
            g.create_rel(t, "HAS_TYPE", cls, &e).expect("has_type");
            t
        };
        tag(101, "Guitar", rock); // rock⊂music ✓
        tag(102, "Synth", pop); // pop⊂music ✓
        tag(103, "Bebop", jazz); // jazz⊂rock⊂music ✓
        tag(104, "Melody", music); // directly music ✓
        tag(105, "Racket", tennis); // tennis⊂sports ✗ (not music)
        tag(106, "Music", sports); // NAMED 'Music' though under sports ✓ (Set A)

        let want = tagset_from_general(&g);
        assert_eq!(
            want,
            BTreeSet::from([101, 102, 103, 104, 106]),
            "sanity: the general traversal's expected set"
        );

        let edge_tokens = g.type_tokens_peek(&["HAS_TYPE".into(), "IS_SUBCLASS_OF".into()]);
        let got_vals = g
            .collect_anchored_hierarchy(
                "Tag",
                "TagClass",
                &edge_tokens,
                "name",
                &Value::Str("Music".into()),
                "id",
            )
            .expect("run")
            .expect("some");
        let got: BTreeSet<i64> = got_vals
            .into_iter()
            .filter_map(|v| match v {
                Value::Int(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(got, want, "anchored hierarchy set != general traversal set");
    }
}

#[cfg(test)]
mod edge_probe_flag_tests {
    //! `AdjTable::sorted_by_peer` is an ESTABLISHED flag, so the check that
    //! establishes it must be able to say `false`. The differential tests in
    //! `tests/adjacency_probe_slim.rs` prove the flag is CONSULTED (clearing
    //! it forces the walk); this proves the check COMPUTES something — a
    //! check that always answered `true` would pass every one of those tests
    //! and vouch for nothing. The untyped table of a two-type graph is the
    //! natural negative: its rows are ordered `(type, peer, rel)`, so a node
    //! whose lower-token type points at the HIGHER peer has a row whose peers
    //! descend across the type boundary.

    use super::*;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;

    #[test]
    fn row_check_says_false_for_a_descending_peer() {
        let e = |peer: u64| SlimAdj {
            rel: 0,
            type_token: 0,
            peer,
        };
        assert!(row_sorted_by_peer(&[]));
        assert!(row_sorted_by_peer(&[e(1)]));
        assert!(row_sorted_by_peer(&[e(1), e(1), e(2)]));
        assert!(!row_sorted_by_peer(&[e(2), e(1)]));
    }

    /// Typed tables come out flagged sorted; the untyped table of the same
    /// graph comes out flagged UNSORTED — by BOTH build paths (per-table and
    /// the one-pass warm), which must agree on the flag as they do on the rows.
    #[test]
    fn typed_tables_are_flagged_sorted_and_the_untyped_one_is_not() {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let none = BTreeMap::new();
        let n: Vec<u64> = (0..4)
            .map(|_| g.create_node(&["N".into()], &none).expect("node"))
            .collect();
        // Mint "A" before "B" so A's token is the lower one, then have A point
        // at the highest peer and B at the lowest: the untyped O row of n[0]
        // is [(A, n[3]), (B, n[1])] — peers descend.
        g.create_rel(n[0], "A", n[3], &none).expect("rel");
        g.create_rel(n[0], "B", n[1], &none).expect("rel");
        g.create_rel(n[0], "B", n[2], &none).expect("rel");
        let a = g.type_tokens_peek(&["A".into()]);
        let b = g.type_tokens_peek(&["B".into()]);
        assert!(a < b, "the fixture relies on A's token being the lower one");

        // Per-table build.
        let typed_a = g.build_adj_table(b'O', &a).expect("table");
        let typed_b = g.build_adj_table(b'O', &b).expect("table");
        let untyped = g.build_adj_table(b'O', &None).expect("table");
        assert!(typed_a.sorted_by_peer, "the :A table is sorted by peer");
        assert!(typed_b.sorted_by_peer, "the :B table is sorted by peer");
        assert!(
            !untyped.sorted_by_peer,
            "the untyped table's row for n[0] descends in peer across the type \
             boundary, and the check did not notice — it cannot say false"
        );

        // One-pass build (what `warm` uses) must establish the same flags.
        let all = g.build_adj_tables_all_types(b'O').expect("tables");
        for (key, table) in &all {
            match key {
                None => assert!(!table.sorted_by_peer, "one-pass untyped flagged sorted"),
                Some(_) => assert!(table.sorted_by_peer, "one-pass typed flagged unsorted"),
            }
        }
        assert_eq!(all.len(), 3, "untyped + :A + :B");

        // And the fold carries the flag rather than recomputing or resetting it.
        assert!(typed_a.folded().sorted_by_peer);
        assert!(!untyped.folded().sorted_by_peer);
        assert!(!typed_a.unsorted().sorted_by_peer);
    }
}
