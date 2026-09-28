//! The workload generator, made dialect-neutral.
//!
//! # What moved here, and what did not
//!
//! `stress.rs` decides three things per operation — whether it is a read or a
//! write, which shape, and which key — and then renders Cypher. The first
//! three are the WORKLOAD; the fourth is a dialect. Splitting them is the
//! whole convergence: this module keeps the decisions, the catalogue keeps the
//! text, and a Postgres or LadybugDB backend replays the same decisions
//! against different text.
//!
//! Everything here is a transcription of `stress.rs`'s tables and arithmetic,
//! and `tests/a_converged_plan_replays_the_stress_op_sequence.rs` holds it to
//! that: a golden file, generated from the untouched `stress.rs` before this
//! module existed, that every op must still match byte for byte. The golden is
//! the point. A generator that is *nearly* the old one produces numbers that
//! are *nearly* comparable to four years of recorded runs, which is to say not
//! comparable at all, and nothing about the output would look wrong.
//!
//! # The one thing that cannot be pre-decided
//!
//! Delete-churn's op at index `i` depends on which of this worker's earlier
//! creates the SERVER acknowledged — a refused create leaves the live set
//! short, so an op that would have been a delete is a create instead. It is
//! therefore feedback-dependent and is emitted as an INTENT
//! ([`Op::ChurnStep`]) that each backend resolves against its own
//! [`ChurnSet`]. That is the single place equivalence rests on a pinned
//! specification rather than on a materialised list, and the reconciliation is
//! what catches a drifting one: a replayer whose ledger diverges cannot
//! balance acked creates minus acked deletes against a fresh survivor count.
//!
//! # Determinism
//!
//! SplitMix64, seeded per client from the run seed. No system entropy, no
//! thread-local state, no clock — the wall clock is read to MEASURE and never
//! to decide. Two runs with one seed issue one operation sequence.

use std::collections::BTreeMap;
use std::collections::VecDeque;

// ─── Seeded randomness ──────────────────────────────────────────────────────

/// SplitMix64 — the same generator the corpus generator uses, for the same
/// reason: no system entropy, no thread-local state, identical everywhere.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seed the stream.
    #[must_use]
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw below `n`; `0` when `n` is zero.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

/// The per-client seed. Same run seed reproduces the run; clients do not all
/// issue the identical sequence, which would be a lockstep artefact rather
/// than a workload.
#[must_use]
pub fn client_seed(seed: u64, cid: usize) -> u64 {
    seed ^ ((cid as u64 + 1).wrapping_mul(0x9E37_79B9))
}

// ─── Parameter locality ─────────────────────────────────────────────────────

/// How a client picks which key to touch.
///
/// The axis most load generators omit, and it dominates results: a uniform
/// pick over a large key space misses every cache, and a single hot key
/// measures the lock rather than the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Locality {
    /// Every key equally likely — worst case for caches.
    Uniform,
    /// Skewed: ~80% of picks land in ~20% of the space.
    Zipfian,
    /// One key, always — maximum contention.
    Hot,
}

impl Locality {
    /// Draw a key.
    pub fn pick(self, rng: &mut Rng, space: u64) -> u64 {
        match self {
            Locality::Uniform => rng.below(space),
            // A cheap, dependency-free skew: square a uniform draw in [0,1)
            // and scale. Not a true Zipf, and labelled as such.
            Locality::Zipfian => {
                let u = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                ((u * u) * space as f64) as u64 % space.max(1)
            }
            Locality::Hot => 0,
        }
    }
}

// ─── Shapes ─────────────────────────────────────────────────────────────────

/// One read shape, weighted.
///
/// The weights matter as much as the shapes: an even mix over-represents the
/// expensive shapes relative to any real application.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// The shape's catalogue name.
    pub name: &'static str,
    /// Its share of the read mix.
    pub weight: u32,
    /// How its key is drawn.
    pub locality: Locality,
}

/// The synthetic corpus's read shapes.
pub const SYNTHETIC_SHAPES: &[Shape] = &[
    Shape {
        name: "point-lookup",
        weight: 40,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "one-hop",
        weight: 25,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "two-hop",
        weight: 10,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "var-length",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "aggregate",
        weight: 10,
        locality: Locality::Uniform,
    },
    Shape {
        name: "top-k",
        weight: 5,
        locality: Locality::Uniform,
    },
    Shape {
        name: "scan-filter",
        weight: 5,
        locality: Locality::Uniform,
    },
];

/// LDBC SNB read shapes, named after the Interactive workload they model.
/// Not the official Interactive queries — the traversal SHAPES those queries
/// impose, which is what a stress mix needs.
pub const SNB_SHAPES: &[Shape] = &[
    Shape {
        name: "is1-profile",
        weight: 35,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "is3-friends",
        weight: 25,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ic-foaf",
        weight: 10,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "is5-by-creator",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "is5-anchored",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ic6-friend-tags",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "knows-var-length",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "is7-replies",
        weight: 5,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "agg-by-city",
        weight: 5,
        locality: Locality::Uniform,
    },
];

/// The platform's read shapes on the SNB schema — a two-engine Bolt
/// comparison by construction, so the catalogue declares them unsupported in
/// every other dialect rather than half-mapping them.
pub const SNB_PLATFORM_SHAPES: &[Shape] = &[
    Shape {
        name: "plat-composite-count",
        weight: 20,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "plat-composite-list",
        weight: 10,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "plat-limit-listing",
        weight: 10,
        locality: Locality::Uniform,
    },
    Shape {
        name: "plat-notin-pick",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "plat-in-list-seek",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "plat-hop-group",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "plat-optional-count",
        weight: 15,
        locality: Locality::Zipfian,
    },
];

/// The graph-algorithm read shapes. A separate set rather than four more
/// entries in the ordinary table, because an algorithm is two to four orders
/// of magnitude more expensive than a point lookup and at any weight that made
/// it appear at all it would dominate every level's wall clock.
pub const ALGO_SHAPES: &[Shape] = &[
    Shape {
        name: "algo-pagerank",
        weight: 30,
        locality: Locality::Uniform,
    },
    Shape {
        name: "algo-wcc",
        weight: 30,
        locality: Locality::Uniform,
    },
    Shape {
        name: "algo-kshortest",
        weight: 20,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "algo-allshortest",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "algo-stats",
        weight: 15,
        locality: Locality::Uniform,
    },
    Shape {
        name: "algo-mutate",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "algo-result",
        weight: 10,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "algo-write",
        weight: 5,
        locality: Locality::Uniform,
    },
];

// ─── Datasets ───────────────────────────────────────────────────────────────

/// Which corpus the harness drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dataset {
    /// The harness seeds its own world — runs anywhere, including CI, and is
    /// the only dataset a four-engine concurrency run needs no corpus for.
    Synthetic,
    /// ATTACH to a server that already holds an LDBC SNB corpus.
    Snb,
    /// The SNB corpus read through the platform's access shapes.
    SnbPlatform,
}

impl Dataset {
    /// Parse a dataset name, or `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Dataset> {
        match s {
            "synthetic" => Some(Dataset::Synthetic),
            "snb" | "ldbc-snb" => Some(Dataset::Snb),
            "snb-platform" | "platform" => Some(Dataset::SnbPlatform),
            _ => None,
        }
    }

    /// The catalogue / CLI name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Dataset::Synthetic => "synthetic",
            Dataset::Snb => "snb",
            Dataset::SnbPlatform => "snb-platform",
        }
    }

    /// The corpus a dataset is loaded from: everything that is not a READ
    /// SHAPE dispatches on this, so a shape set never has to restate it.
    #[must_use]
    pub fn family(self) -> Dataset {
        match self {
            Dataset::SnbPlatform => Dataset::Snb,
            d => d,
        }
    }

    /// The dataset's own read shapes; a profile may override them.
    #[must_use]
    pub fn shapes(self) -> &'static [Shape] {
        match self {
            Dataset::Synthetic => SYNTHETIC_SHAPES,
            Dataset::Snb => SNB_SHAPES,
            Dataset::SnbPlatform => SNB_PLATFORM_SHAPES,
        }
    }

    /// The catalogue fixture group whose indexes and probes this dataset needs.
    #[must_use]
    pub fn fixture_group(self) -> &'static str {
        self.name()
    }
}

// ─── Write kinds and profiles ───────────────────────────────────────────────

/// What a write IS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteKind {
    /// A node create or hot-node update, per the profile's write locality.
    Node,
    /// A relationship between two DISTINCT pseudo-random endpoints.
    RelSpread,
    /// A relationship whose destination is always node 0 — every write
    /// serialises through one guard row.
    RelHub,
    /// A create under a UNIQUE constraint where every client races the SAME
    /// value sequence.
    UniqueCreate,
    /// A node create with no relationship — isolates index/membership churn
    /// from adjacency churn.
    NodeOnly,
    /// The same, on property names no read seeks — isolates the property-epoch
    /// collision.
    NodeOnlyFreshProps,
    /// The same, with no labels — isolates named-label membership churn.
    NodeOnlyNoLabels,
    /// Create-then-later-delete over a per-worker population.
    DeleteChurn,
}

/// A named mix.
#[derive(Clone, Copy)]
pub struct Profile {
    /// The profile name, as the CLI and every report spell it.
    pub name: &'static str,
    /// Percent of operations that are writes.
    pub write_pct: u64,
    /// Where writes land.
    pub write_locality: Locality,
    /// What a write is.
    pub write_kind: WriteKind,
    /// One line saying what this profile exists to measure.
    pub what: &'static str,
    /// A DIAGNOSTIC control, excluded from `all` — letting one into `all`
    /// would change the profile COUNT every recorded sweep is compared
    /// against AND the mutation history each later profile inherits.
    pub diagnostic: bool,
    /// Read shapes to use INSTEAD of the dataset's.
    pub shapes: Option<&'static [Shape]>,
}

/// Every profile, in the order a sweep runs them. The order is load-bearing:
/// write profiles mutate the store, so moving one changes what every profile
/// after it measures.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "read-only",
        write_pct: 0,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "the concurrency ceiling with no write interference",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "read-heavy",
        write_pct: 5,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high read, low write — the common production shape",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "balanced",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high read, high write — both paths contending",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "write-heavy",
        write_pct: 95,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high write, low read — ingest under query load",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "write-only",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "raw insert throughput",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "contention",
        write_pct: 50,
        write_locality: Locality::Hot,
        write_kind: WriteKind::Node,
        what: "write-WRITE conflict on one hot node — not insert throughput",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "balanced-nodeonly",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::NodeOnly,
        what: "50/50 with a node-only write — isolates index/membership churn from adjacency churn",
        diagnostic: true,
        shapes: None,
    },
    Profile {
        name: "balanced-freshprops",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::NodeOnlyFreshProps,
        what: "50/50 node-only write on property names no read seeks — isolates the property-epoch collision",
        diagnostic: true,
        shapes: None,
    },
    Profile {
        name: "balanced-nolabels",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::NodeOnlyNoLabels,
        what: "50/50 node-only write with no labels — isolates named-label membership churn",
        diagnostic: true,
        shapes: None,
    },
    Profile {
        name: "balanced-disjoint",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelSpread,
        what: "50/50 with writes on a relationship type no read traverses — the interference control",
        diagnostic: true,
        shapes: None,
    },
    Profile {
        name: "rel-create",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelSpread,
        what: "distinct-endpoint relationship inserts — the guard-row overhead, spread",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "rel-hub",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelHub,
        what: "every relationship lands on ONE endpoint — guard serialisation, measured",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "unique-create",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::UniqueCreate,
        what: "every client races the same UNIQUE values — one winner each, zero duplicates",
        diagnostic: false,
        shapes: None,
    },
    Profile {
        name: "algo-read",
        write_pct: 0,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "graph algorithms, no write interference — the computation's own cost",
        diagnostic: true,
        shapes: Some(ALGO_SHAPES),
    },
    Profile {
        name: "algo-churn",
        write_pct: 60,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::DeleteChurn,
        what: "algorithms while nodes are being DELETED out from under the projection",
        diagnostic: true,
        shapes: Some(ALGO_SHAPES),
    },
    Profile {
        name: "algo-mixed",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelSpread,
        what: "algorithms CONCURRENT with writes — every write invalidates the projection",
        diagnostic: true,
        shapes: Some(ALGO_SHAPES),
    },
    Profile {
        name: "delete-churn",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::DeleteChurn,
        what: "create-then-delete churn per worker — DETACH DELETE and rel cleanup under load",
        diagnostic: false,
        shapes: None,
    },
];

/// Look a profile up by name.
#[must_use]
pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

// ─── Delete churn ───────────────────────────────────────────────────────────

/// Live nodes a worker accumulates before its first delete.
pub const CHURN_FLOOR: usize = 16;

/// One planned churn write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChurnPlan {
    /// CREATE a node (wired to the worker's anchor) with this id.
    Create {
        /// The node id — the write sequence itself, worker-disjoint.
        id: u64,
    },
    /// DETACH DELETE the node with this id — popped from the live set BEFORE
    /// the send, and never restored.
    Delete {
        /// The victim's id, formerly the oldest entry of the live set.
        id: u64,
    },
}

/// Per-worker churn state: the ids this worker created and the server acked
/// but has not yet deleted, oldest first, plus the acked-op ledger the
/// post-level reconciliation reads.
#[derive(Default)]
pub struct ChurnSet {
    live: VecDeque<u64>,
    creates_acked: u64,
    deletes_acked: u64,
}

impl ChurnSet {
    /// Plan the write op for sequence `seq`.
    ///
    /// A delete victim is popped HERE, before the send. On a refusal or a
    /// transport error it is NOT restored: a double delete is impossible by
    /// construction, and any discrepancy an unacked delete leaves behind must
    /// surface in the reconciliation rather than be papered over locally.
    pub fn plan(&mut self, seq: u64) -> ChurnPlan {
        if seq % 2 == 1 && self.live.len() >= CHURN_FLOOR {
            let victim = self.live.pop_front().expect("floor guarantees a victim");
            return ChurnPlan::Delete { id: victim };
        }
        ChurnPlan::Create { id: seq }
    }

    /// Record a server-acked op.
    pub fn ack(&mut self, plan: ChurnPlan) {
        match plan {
            ChurnPlan::Create { id } => {
                self.live.push_back(id);
                self.creates_acked += 1;
            }
            ChurnPlan::Delete { .. } => self.deletes_acked += 1,
        }
    }

    /// Creates the server acknowledged.
    #[must_use]
    pub fn creates_acked(&self) -> u64 {
        self.creates_acked
    }

    /// Deletes the server acknowledged.
    #[must_use]
    pub fn deletes_acked(&self) -> u64 {
        self.deletes_acked
    }

    /// How many ids are live in this worker's local set.
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.live.len()
    }
}

/// The reconciliation verdict: acked creates minus acked deletes against a
/// fresh count of survivors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reconciliation {
    /// The ledger and the corpus agree; carries the survivor count.
    Balanced(u64),
    /// They do not — lost writes, phantom acks, or leftover state.
    Mismatch {
        /// What the ledger promises: creates_acked − deletes_acked.
        expected: i128,
        /// What the corpus actually holds.
        measured: u64,
    },
}

/// Pure churn arithmetic. `i128` so a ledger that somehow acked more deletes
/// than creates reports a mismatch instead of panicking on unsigned underflow.
#[must_use]
pub fn reconcile(creates_acked: u64, deletes_acked: u64, survivors: u64) -> Reconciliation {
    let expected = i128::from(creates_acked) - i128::from(deletes_acked);
    if expected == i128::from(survivors) {
        Reconciliation::Balanced(survivors)
    } else {
        Reconciliation::Mismatch {
            expected,
            measured: survivors,
        }
    }
}

// ─── The operation model ────────────────────────────────────────────────────

/// A bound parameter. Every value a statement interpolates is one of these, so
/// a plan is self-describing and an out-of-process replayer needs no arithmetic
/// of its own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Param {
    /// An unsigned integer, rendered in decimal.
    Uint(u64),
    /// Pre-rendered text — a name, or a bracketed id list. Rendered verbatim,
    /// so whatever quoting the statement needs is already in it.
    Text(String),
}

impl Param {
    /// The exact characters this parameter contributes to a statement.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Param::Uint(n) => n.to_string(),
            Param::Text(s) => s.clone(),
        }
    }
}

/// One operation, before any dialect touches it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// A read of a named catalogue shape.
    Read {
        /// The catalogue's `stress.read_shapes` key.
        shape: &'static str,
        /// Its bound parameters.
        params: BTreeMap<String, Param>,
    },
    /// A write of a named catalogue op.
    Write {
        /// The catalogue's `stress.write_ops` key.
        op: &'static str,
        /// Its bound parameters.
        params: BTreeMap<String, Param>,
    },
    /// A delete-churn step. The statement is resolved at replay from the
    /// worker's own [`ChurnSet`], because which op this is depends on which
    /// earlier creates the server acknowledged. See the module docs.
    ChurnStep {
        /// The write sequence, carrying the `cid << 40` worker prefix.
        seq: u64,
    },
}

impl Op {
    /// `read` or `write` — what a latency sample is filed under.
    #[must_use]
    pub fn is_write(&self) -> bool {
        !matches!(self, Op::Read { .. })
    }

    /// The shape name a per-shape latency table files this under, or `None`
    /// for a write (writes are reported as one population, as they always
    /// have been).
    #[must_use]
    pub fn shape(&self) -> Option<&'static str> {
        match self {
            Op::Read { shape, .. } => Some(shape),
            _ => None,
        }
    }
}

// ─── Parameter binding ──────────────────────────────────────────────────────

/// Person names to seek by, cycled from the probe key.
pub const PLATFORM_FIRST: [&str; 8] = ["Jan", "Wei", "Chen", "Jun", "Ali", "Amit", "Hans", "Jose"];
/// Surnames to seek by, cycled from the probe key.
pub const PLATFORM_LAST: [&str; 8] = [
    "Li", "Wang", "Zhang", "Khan", "Kumar", "Singh", "Silva", "Yang",
];

/// Fifty literal ids from `key` up — the `NOT id IN [...]` and `id IN [...]`
/// lists the platform sends. Rendered as a literal because the harness renders
/// statements, not parameters, on every engine alike.
#[must_use]
pub fn platform_id_list(key: u64, space: u64) -> String {
    let space = space.max(1);
    let ids: Vec<String> = (0..50u64)
        .map(|i| ((key + 1 + i * 13) % space).to_string())
        .collect();
    format!("[{}]", ids.join(", "))
}

fn p(pairs: &[(&str, Param)]) -> BTreeMap<String, Param> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Bind a read shape's parameters from the drawn key.
///
/// Every derived value the old `render_read` computed inline lives here, and
/// only here: `is7-replies`'s decorrelating multiply, the synthetic
/// aggregate's `key % 16`, the platform name pair, the algorithm shapes'
/// second endpoint. That is what lets the plan carry bound values and the
/// Python executor carry no arithmetic at all.
#[must_use]
pub fn bind_read(shape: &str, key: u64, space: u64) -> BTreeMap<String, Param> {
    match shape {
        // No parameter at all — a whole-corpus shape.
        "top-k" | "agg-by-city" | "plat-limit-listing" | "algo-pagerank" | "algo-wcc"
        | "algo-stats" => BTreeMap::new(),
        "aggregate" => p(&[("b", Param::Uint(key % 16))]),
        "is7-replies" => p(&[("mid", Param::Uint(key.wrapping_mul(7) % space.max(1)))]),
        "algo-mutate" => p(&[("mk", Param::Uint(key % 32))]),
        "algo-result" => p(&[("par", Param::Uint(key % 2))]),
        "algo-write" => p(&[("dg", Param::Uint(key % 4))]),
        "algo-kshortest" | "algo-allshortest" => p(&[
            ("key", Param::Uint(key)),
            ("other", Param::Uint((key + space / 3 + 1) % space.max(1))),
        ]),
        "plat-composite-count" | "plat-composite-list" => p(&[
            (
                "first",
                Param::Text(PLATFORM_FIRST[(key % 8) as usize].to_string()),
            ),
            (
                "last",
                Param::Text(PLATFORM_LAST[((key / 8) % 8) as usize].to_string()),
            ),
        ]),
        "plat-notin-pick" => p(&[
            (
                "first",
                Param::Text(PLATFORM_FIRST[(key % 8) as usize].to_string()),
            ),
            ("idlist", Param::Text(platform_id_list(key, space))),
        ]),
        "plat-in-list-seek" => p(&[("idlist", Param::Text(platform_id_list(key, space)))]),
        _ => p(&[("key", Param::Uint(key))]),
    }
}

/// Which NEUTRAL write op a `(kind, locality, dataset family)` triple names,
/// and its bound parameters.
///
/// The op name carries no dataset: `node_create` is `node_create` whether it
/// mints a `:StressW` or a `:Message:Comment`, because the corpus is named in
/// the plan header and a plan that named it twice could disagree with itself.
/// The catalogue does the dataset dispatch when it renders.
///
/// The `id` binding is the one the level scoping moves, so every op that mints
/// an identity carries one — including `synthetic` `node_create`, whose Cypher
/// writes `{c, s}` and never interpolates it. Carrying an id the Cypher
/// ignores is what lets the Bolt arm stay byte-identical to `stress.rs` while
/// the engines with a primary key get a value that does not collide across
/// levels.
///
/// # Panics
/// On [`WriteKind::DeleteChurn`], which is resolved per worker from
/// [`ChurnSet`] state. The op generator never calls this for it; a caller that
/// does has a bug worth stopping for.
#[must_use]
pub fn bind_write(
    ds: Dataset,
    locality: Locality,
    kind: WriteKind,
    cid: usize,
    seq: u64,
    space: u64,
    nonce: u64,
) -> (&'static str, BTreeMap<String, Param>) {
    let id = (cid as u64) << 40 | seq;
    let mdate = 1_400_000_000_000u64 + seq;
    match kind {
        WriteKind::DeleteChurn => {
            unreachable!("delete-churn writes are resolved per worker from ChurnSet state")
        }
        WriteKind::NodeOnly => (
            "node_only",
            p(&[("id", Param::Uint(id)), ("mdate", Param::Uint(mdate))]),
        ),
        WriteKind::NodeOnlyFreshProps => (
            "node_only_fresh_props",
            p(&[("id", Param::Uint(id)), ("mdate", Param::Uint(mdate))]),
        ),
        WriteKind::NodeOnlyNoLabels => (
            "node_only_no_labels",
            p(&[("id", Param::Uint(id)), ("mdate", Param::Uint(mdate))]),
        ),
        WriteKind::UniqueCreate => {
            // Mask the client id off `seq` so every client races the SAME
            // values; the nonce keeps one sweep's plans from replaying
            // another's, and the level stride keeps the levels apart.
            let contested = (nonce << 32) | (seq & 0xFFFF_FFFF);
            ("unique_create", p(&[("u", Param::Uint(contested))]))
        }
        WriteKind::RelSpread | WriteKind::RelHub => {
            let space = space.max(2);
            let a = 1 + (seq.wrapping_mul(2_654_435_761) % (space - 1));
            let b = if kind == WriteKind::RelHub {
                0
            } else {
                1 + ((a + 1 + seq % 97) % (space - 1))
            };
            let name = if kind == WriteKind::RelHub {
                "rel_hub"
            } else {
                "rel_spread"
            };
            (name, p(&[("a", Param::Uint(a)), ("b", Param::Uint(b))]))
        }
        WriteKind::Node => match (ds.family(), locality) {
            (_, Locality::Hot) => ("hot_update", BTreeMap::new()),
            (Dataset::Synthetic, _) => (
                "node_create",
                p(&[
                    ("id", Param::Uint(id)),
                    ("cid", Param::Uint(cid as u64)),
                    ("seq", Param::Uint(seq)),
                ]),
            ),
            (_, _) => (
                "node_create",
                p(&[
                    ("id", Param::Uint(id)),
                    ("cid", Param::Uint(cid as u64)),
                    ("seq", Param::Uint(seq)),
                    ("author", Param::Uint(seq % space.max(1))),
                    ("mdate", Param::Uint(mdate)),
                ]),
            ),
        },
    }
}

/// The neutral op and bound parameters of a resolved churn step.
#[must_use]
pub fn bind_churn(
    plan: ChurnPlan,
    cid: usize,
    nonce: u64,
) -> (&'static str, BTreeMap<String, Param>) {
    match plan {
        ChurnPlan::Create { id } => (
            "churn_create",
            p(&[
                ("cid", Param::Uint(cid as u64)),
                ("id", Param::Uint(id)),
                ("nonce", Param::Uint(nonce)),
            ]),
        ),
        ChurnPlan::Delete { id } => (
            "churn_delete",
            p(&[
                ("cid", Param::Uint(cid as u64)),
                ("id", Param::Uint(id)),
                ("nonce", Param::Uint(nonce)),
            ]),
        ),
    }
}

/// The per-worker anchor every churn create wires a rel to.
///
/// Issued once, before the level's loop, and therefore NOT a plan op: it is
/// setup, and a retried CREATE here would mint two anchors and double every
/// later create.
#[must_use]
pub fn bind_churn_anchor(cid: usize, nonce: u64) -> (&'static str, BTreeMap<String, Param>) {
    (
        "churn_anchor",
        p(&[
            ("cid", Param::Uint(cid as u64)),
            ("nonce", Param::Uint(nonce)),
        ]),
    )
}

// ─── The op generator ───────────────────────────────────────────────────────

/// Everything one client's op stream is a function of.
#[derive(Clone, Copy, Debug)]
pub struct LevelSpec {
    /// The run seed.
    pub seed: u64,
    /// Which corpus and shape vocabulary.
    pub dataset: Dataset,
    /// The key space — `--keys`, or the probed person count.
    pub keys: u64,
    /// The per-level nonce, so contested-value profiles never replay a spent
    /// value space across levels.
    pub nonce: u64,
}

/// One client's operation stream, as the worker loop draws it.
///
/// A struct rather than a closure because the read/write interleave carries
/// state (`wacc`, `seq`) that must advance exactly as `stress.rs`'s worker
/// advances it, and a state machine that can be single-stepped is a state
/// machine a golden test can compare.
pub struct ClientOps {
    rng: Rng,
    wacc: u64,
    seq: u64,
    write_pct: u64,
    write_kind: WriteKind,
    write_locality: Locality,
    shapes: &'static [Shape],
    dataset: Dataset,
    keys: u64,
    nonce: u64,
    cid: usize,
}

impl ClientOps {
    /// Start client `cid`'s stream for a profile at a level.
    #[must_use]
    pub fn new(spec: LevelSpec, prof: &Profile, cid: usize) -> ClientOps {
        ClientOps {
            rng: Rng::new(client_seed(spec.seed, cid)),
            wacc: 0,
            seq: (cid as u64) << 40,
            write_pct: prof.write_pct,
            write_kind: prof.write_kind,
            write_locality: prof.write_locality,
            shapes: prof.shapes.unwrap_or(spec.dataset.shapes()),
            dataset: spec.dataset,
            keys: spec.keys,
            nonce: spec.nonce,
            cid,
        }
    }

    /// The next operation. Infinite: the workload is time-boxed, not
    /// op-boxed, and a generator that could run out would silently shorten a
    /// level on the fastest engine — the arm most likely to be the one under
    /// test.
    pub fn next_op(&mut self) -> Op {
        // Exact-fraction interleave: reproducible, no RNG for the read/write
        // decision itself.
        self.wacc += self.write_pct;
        let do_write = self.wacc >= 100;
        if do_write {
            self.wacc -= 100;
        }
        if do_write {
            let a = self.seq;
            self.seq += 1;
            if self.write_kind == WriteKind::DeleteChurn {
                return Op::ChurnStep { seq: a };
            }
            let (op, params) = bind_write(
                self.dataset,
                self.write_locality,
                self.write_kind,
                self.cid,
                a,
                self.keys,
                self.nonce,
            );
            return Op::Write { op, params };
        }
        // Weighted shape choice, then a locality-aware key. The weight total
        // is recomputed from whichever set is in force — a profile may
        // override the dataset's, and using the dataset's total against an
        // overridden set skews the draw toward the first shape.
        let weight: u32 = self.shapes.iter().map(|s| s.weight).sum();
        let mut pickw = self.rng.below(u64::from(weight.max(1))) as u32;
        let mut chosen = &self.shapes[0];
        for sh in self.shapes {
            if pickw < sh.weight {
                chosen = sh;
                break;
            }
            pickw -= sh.weight;
        }
        let key = chosen.locality.pick(&mut self.rng, self.keys);
        Op::Read {
            shape: chosen.name,
            params: bind_read(chosen.name, key, self.keys),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(set: &mut ChurnSet, start: u64, n: u64) -> Vec<ChurnPlan> {
        (start..start + n)
            .map(|seq| {
                let plan = set.plan(seq);
                set.ack(plan);
                plan
            })
            .collect()
    }

    #[test]
    fn churn_builds_the_floor_before_the_first_delete() {
        let mut set = ChurnSet::default();
        let plans = drive(&mut set, 0, 64);
        let first = plans
            .iter()
            .position(|p| matches!(p, ChurnPlan::Delete { .. }))
            .expect("a 64-op run must reach the delete phase");
        assert!(
            plans[..first]
                .iter()
                .all(|p| matches!(p, ChurnPlan::Create { .. }))
        );
        assert!(first >= CHURN_FLOOR);
        assert!(set.live_len() == CHURN_FLOOR || set.live_len() == CHURN_FLOOR + 1);
        assert_eq!(
            set.creates_acked() - set.deletes_acked(),
            set.live_len() as u64
        );
    }

    #[test]
    fn a_victim_is_popped_before_the_send_and_never_restored() {
        let mut set = ChurnSet::default();
        drive(&mut set, 0, 17);
        let ChurnPlan::Delete { id: victim } = set.plan(17) else {
            panic!("op 17 must be a delete");
        };
        assert_eq!(set.deletes_acked(), 0, "an unacked delete is not counted");
        let later = drive(&mut set, 18, 200);
        assert!(
            later
                .iter()
                .all(|p| !matches!(p, ChurnPlan::Delete { id } if *id == victim)),
            "a popped victim is never offered twice"
        );
    }

    #[test]
    fn reconcile_balances_and_catches_loss_both_ways() {
        assert_eq!(reconcile(10, 4, 6), Reconciliation::Balanced(6));
        assert_eq!(reconcile(0, 0, 0), Reconciliation::Balanced(0));
        assert_eq!(
            reconcile(10, 4, 5),
            Reconciliation::Mismatch {
                expected: 6,
                measured: 5
            }
        );
        assert_eq!(
            reconcile(10, 4, 7),
            Reconciliation::Mismatch {
                expected: 6,
                measured: 7
            }
        );
        assert_eq!(
            reconcile(2, 3, 0),
            Reconciliation::Mismatch {
                expected: -1,
                measured: 0
            }
        );
    }

    #[test]
    fn delete_churn_is_registered_and_runs_last() {
        let p = profile("delete-churn").expect("selectable by name");
        assert!(matches!(p.write_kind, WriteKind::DeleteChurn));
        assert_eq!(p.write_pct, 100);
        assert_eq!(PROFILES.last().expect("non-empty").name, "delete-churn");
    }

    #[test]
    fn the_headline_sweep_holds_ten_profiles() {
        // `all` selects the non-diagnostic set, and its COUNT is what every
        // recorded sweep is compared against. A diagnostic leaking into it
        // would also change the mutation history each later profile inherits.
        let headline: Vec<&str> = PROFILES
            .iter()
            .filter(|p| !p.diagnostic)
            .map(|p| p.name)
            .collect();
        assert_eq!(
            headline,
            vec![
                "read-only",
                "read-heavy",
                "balanced",
                "write-heavy",
                "write-only",
                "contention",
                "rel-create",
                "rel-hub",
                "unique-create",
                "delete-churn",
            ]
        );
    }

    #[test]
    fn every_shape_and_write_op_the_generator_can_emit_is_in_the_catalogue() {
        // The wiring check: a shape table entry with no catalogue text would
        // fail at the wire, on a pod, after a corpus load.
        let cat = crate::catalogue::Catalogue::load().expect("catalogue");
        let known = cat.read_shape_names().expect("read shapes");
        for set in [
            SYNTHETIC_SHAPES,
            SNB_SHAPES,
            SNB_PLATFORM_SHAPES,
            ALGO_SHAPES,
        ] {
            for s in set {
                assert!(
                    known.iter().any(|k| k == s.name),
                    "shape {} has no catalogue entry",
                    s.name
                );
            }
        }
        let write_ops = cat.write_op_names().expect("write ops");
        for name in [
            "hot_update",
            "node_create",
            "node_only",
            "node_only_fresh_props",
            "node_only_no_labels",
            "unique_create",
            "rel_spread",
            "rel_hub",
            "churn_anchor",
            "churn_create",
            "churn_delete",
        ] {
            assert!(
                write_ops.iter().any(|k| k == name),
                "write op {name} has no catalogue entry"
            );
        }
        // Every WriteKind must name an op the catalogue holds — the wiring
        // check that a new kind cannot skip.
        for kind in [
            WriteKind::Node,
            WriteKind::RelSpread,
            WriteKind::RelHub,
            WriteKind::UniqueCreate,
            WriteKind::NodeOnly,
            WriteKind::NodeOnlyFreshProps,
            WriteKind::NodeOnlyNoLabels,
        ] {
            for loc in [Locality::Uniform, Locality::Hot] {
                let (name, _) = bind_write(Dataset::Snb, loc, kind, 0, 0, 100, 1);
                assert!(
                    write_ops.iter().any(|k| k == name),
                    "{kind:?}/{loc:?} names {name}, which has no catalogue entry"
                );
            }
        }
    }

    #[test]
    fn the_op_stream_is_a_function_of_the_seed_alone() {
        let spec = LevelSpec {
            seed: 424_242,
            dataset: Dataset::Snb,
            keys: 10_000,
            nonce: 1,
        };
        let prof = profile("balanced").expect("balanced");
        let take = |n: usize| -> Vec<Op> {
            let mut c = ClientOps::new(spec, prof, 3);
            (0..n).map(|_| c.next_op()).collect()
        };
        assert_eq!(take(500), take(500), "same seed, same ops");
        // And a different client is a different stream — lockstep would be an
        // artefact, not a workload.
        let mut other = ClientOps::new(spec, prof, 4);
        let mine = take(50);
        let theirs: Vec<Op> = (0..50).map(|_| other.next_op()).collect();
        assert_ne!(mine, theirs);
    }
}
