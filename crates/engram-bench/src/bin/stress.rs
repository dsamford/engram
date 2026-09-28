//! The stress / scalability harness — divergent workload mixes under load.
//!
//! `snbconc` measures ONE thing well: read throughput as client count grows,
//! with an optional uniform insert fraction. That is a concurrency ceiling
//! measurement, not a stress test. Three things it deliberately does not do,
//! and which a release needs:
//!
//! 1. **Divergent mixes.** Real workloads are not one ratio. A database that is
//!    fine at 95/5 read/write can collapse at 5/95, and the interesting failures
//!    (compaction cliffs, allocator pressure, lock convoys) live at the ratios
//!    nobody benchmarks.
//! 2. **Pattern variation.** A harness that replays one statement shape measures
//!    that shape's cache behaviour, not the engine. Query shape, parameter
//!    locality (uniform vs zipfian vs a single hot key) and result size all
//!    change which code path runs and which caches help.
//! 3. **Contention.** `snbconc` partitions writers into disjoint id spaces so
//!    they never collide — which measures insert throughput and says nothing
//!    about write-write conflict, the thing that actually decides whether
//!    concurrent writes work.
//!
//! # Determinism
//!
//! Every choice this harness makes is seeded: which shape a client runs, which
//! parameter it binds, whether an operation is a read or a write. Two runs with
//! the same seed issue the same operation sequence, so a regression is
//! reproducible rather than a story about a bad afternoon. The wall clock is
//! read only to MEASURE, never to decide.
//!
//! This binary needs real threads and a real clock, which the simulation layer's
//! `Runtime` deliberately does not provide — hence the lint waiver, the same one
//! `snbconc` carries and for the same reason.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engram_bolt::client::Client;

// ─── Seeded randomness ──────────────────────────────────────────────────────

/// SplitMix64 — the same generator the corpus generator uses, for the same
/// reason: no system entropy, no thread-local state, identical everywhere.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

// ─── Parameter locality ─────────────────────────────────────────────────────

/// How a client picks which key to touch.
///
/// This is the axis most load generators omit, and it dominates results: a
/// uniform pick over a large key space misses every cache, and a single hot key
/// measures the lock rather than the index. Real workloads are skewed, so
/// `Zipfian` is the default for reads.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Locality {
    /// Every key equally likely — worst case for caches.
    Uniform,
    /// Skewed: ~80% of picks land in ~20% of the space.
    Zipfian,
    /// One key, always — maximum contention.
    Hot,
}

impl Locality {
    fn pick(self, rng: &mut Rng, space: u64) -> u64 {
        match self {
            Locality::Uniform => rng.below(space),
            // A cheap, dependency-free skew: square a uniform draw in [0,1) and
            // scale. Not a true Zipf, and labelled as such — it produces the
            // heavy head that matters here without pulling in a distribution
            // crate for a load generator.
            Locality::Zipfian => {
                let u = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
                ((u * u) * space as f64) as u64 % space.max(1)
            }
            Locality::Hot => 0,
        }
    }
}

// ─── Query shapes ───────────────────────────────────────────────────────────

/// One read shape, weighted.
///
/// The weights matter as much as the shapes: an even mix over-represents the
/// expensive shapes relative to any real application, and the point of a mix is
/// to reproduce a plausible load, not to average unrelated numbers.
#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    weight: u32,
    locality: Locality,
}

const SYNTHETIC_SHAPES: &[Shape] = &[
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
///
/// These are not the official Interactive queries — those bind parameters from
/// a generated substitution file and several have no engine-independent
/// definition without it. They are the *traversal shapes* those queries impose:
/// an indexed person lookup, one and two hops over `KNOWS`, a reverse
/// `HAS_CREATOR` walk, a tag histogram over a friend neighbourhood, and a
/// whole-graph aggregate. That is what a stress mix needs — the mix of access
/// paths — and calling them by their family name says which is which without
/// claiming to be a conformant Interactive run.
const SNB_SHAPES: &[Shape] = &[
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
    // The SAME logical query as `is5-by-creator`, written from the other end.
    // Both name one indexed Person and ask for its messages; they differ only
    // in which side of the pattern is written first. A planner that anchors on
    // the selective, indexed endpoint answers them at the same speed. Keeping
    // both in the mix makes any divergence a standing measurement rather than
    // something a reader has to know to look for.
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

/// Which corpus the harness drives.
///
/// The synthetic dataset builds its own world so the harness runs anywhere
/// (including CI). The SNB dataset **attaches** to a server that already holds
/// an LDBC SNB corpus — `portserve <dir>` — because loading half a million
/// nodes over Bolt one statement at a time would measure the loader, and the
/// point of the LDBC lane is to measure the engine at LDBC scale.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dataset {
    Synthetic,
    Snb,
    /// The SNB corpus again, read through the PLATFORM's access shapes — the
    /// ones the production shadow reads (v163–v174) found engram behind Neo4j
    /// on, transcribed onto the SNB schema so both engines answer them from
    /// the same bytes in the same window: a two-key seek under a DECLARED
    /// COMPOSITE index, a bare-`LIMIT` listing, a sought pick with a `NOT …
    /// IN` list, an `IN`-list seek, a grouped hop aggregate and an `OPTIONAL
    /// MATCH … count` per end. Writes, seeding, the hot counter and the
    /// integrity check are the SNB dataset's own (`family`); only the read
    /// shapes and the declared indexes differ, so `--dataset snb` stays
    /// byte-for-byte the mix the 2026-09-01 verdict measured.
    SnbPlatform,
    /// LDBC FinBench: the transaction corpus `fbgen` emits, read through the
    /// traversal shapes its complex reads impose — an indexed account lookup,
    /// one hop out and the same hop walked in REVERSE, a two-hop transfer
    /// chain, the ownership edge from a Person, and a whole-graph aggregate.
    ///
    /// Its own family: nothing about SNB's writes, seeding or hot counter
    /// applies, because no label is shared. `:Person` exists in both and means
    /// different things, which is exactly why this must not fall through to
    /// the SNB arms on a `_ =>`.
    Finbench,
    /// LDBC Graphalytics: the `.v`/`.e` graphs `ga2jsonl` converts, read
    /// through the access paths its six kernels impose.
    ///
    /// # Why a concurrency lane for an ALGORITHM benchmark
    ///
    /// Graphalytics itself measures one kernel at a time on an idle engine,
    /// and that is the number its ranking uses. It says nothing about what
    /// happens when an algorithm runs while the graph underneath it is being
    /// written — which is the regime a database actually serves, and the one
    /// engram's projection cache makes interesting: the projection is keyed on
    /// the adjacency epoch, so ANY write invalidates it and the next kernel
    /// rebuilds the CSR from scratch.
    ///
    /// `algo-read`/`algo-mixed`/`algo-churn` already measured that on the
    /// SYNTHETIC corpus and found an algorithm under a 50% write stream costs
    /// ~3.3x its uncontended latency. This variant asks the same question on a
    /// real Graphalytics graph, where the projection is millions of vertices
    /// rather than thousands and the rebuild is not cheap.
    ///
    /// Its own family, like `Finbench`: the corpus is one `Vertex` label and
    /// one `LINK` type with nothing else in it, so none of SNB's writes,
    /// seeding or hot counter apply. `:Person` does not exist here at all,
    /// which is why this must not fall through to an SNB arm on a `_ =>`.
    Graphalytics,
}

/// `fbgen` mints account ids from 2^62 and NOT densely from zero, so an
/// account key is `FINBENCH_ACCOUNT_ID_BASE + n`. A harness that keyed
/// accounts by `n` alone would look healthy and measure nothing: every lookup
/// would miss, and a run whose reads all return zero rows reports the index's
/// NEGATIVE path as throughput. The attach probe below asserts this base
/// rather than trusting it.
const FINBENCH_ACCOUNT_ID_BASE: u64 = 1 << 62;

impl Dataset {
    fn parse(s: &str) -> Option<Dataset> {
        match s {
            "synthetic" => Some(Dataset::Synthetic),
            "snb" | "ldbc-snb" => Some(Dataset::Snb),
            "snb-platform" | "platform" => Some(Dataset::SnbPlatform),
            "finbench" | "fb" | "ldbc-finbench" => Some(Dataset::Finbench),
            "graphalytics" | "ga" | "ldbc-graphalytics" => Some(Dataset::Graphalytics),
            _ => None,
        }
    }

    /// The corpus a dataset is loaded from: everything that is not a READ
    /// SHAPE (writes, seeding, the hot counter, the stress relationship type)
    /// dispatches on this, so a shape set never has to restate them.
    fn family(self) -> Dataset {
        match self {
            Dataset::SnbPlatform => Dataset::Snb,
            d => d,
        }
    }
    fn shapes(self) -> &'static [Shape] {
        match self {
            Dataset::Synthetic => SYNTHETIC_SHAPES,
            Dataset::Snb => SNB_SHAPES,
            Dataset::SnbPlatform => SNB_PLATFORM_SHAPES,
            Dataset::Finbench => FINBENCH_SHAPES,
            Dataset::Graphalytics => GRAPHALYTICS_SHAPES,
        }
    }
    /// Indexes the read shapes require. Creating these is not tuning the
    /// benchmark: an unindexed lookup key turns every "point lookup" into a
    /// full label scan, so the fixture would BE the measurement — the same
    /// trap the synthetic lane already documents.
    fn indexes(self) -> &'static [&'static str] {
        match self {
            Dataset::Synthetic => &["CREATE INDEX stress_k IF NOT EXISTS FOR (n:Stress) ON (n.k)"],
            Dataset::Snb => &[
                "CREATE INDEX snb_person_id IF NOT EXISTS FOR (n:Person) ON (n.id)",
                "CREATE INDEX snb_message_id IF NOT EXISTS FOR (n:Message) ON (n.id)",
            ],
            // The platform declares COMPOSITE indexes (`UserDataNode(userId,
            // nodeType)`, `Commitment(userId, status)`, …) and single-key
            // ones beside them; the mirror loads that catalogue verbatim. The
            // same two kinds are declared here so a two-key seek is measured
            // as production runs it on BOTH engines, not as a scan.
            Dataset::SnbPlatform => &[
                "CREATE INDEX snb_person_id IF NOT EXISTS FOR (n:Person) ON (n.id)",
                "CREATE INDEX snb_message_id IF NOT EXISTS FOR (n:Message) ON (n.id)",
                "CREATE INDEX snb_person_first IF NOT EXISTS FOR (n:Person) ON (n.firstName)",
                "CREATE INDEX snb_person_name IF NOT EXISTS FOR (n:Person) ON (n.firstName, n.lastName)",
            ],
            // Account.id carries every point lookup; Person.id anchors the
            // ownership and loan shapes.
            Dataset::Finbench => &[
                "CREATE INDEX fb_account_id IF NOT EXISTS FOR (n:Account) ON (n.id)",
                "CREATE INDEX fb_person_id IF NOT EXISTS FOR (n:Person) ON (n.id)",
            ],
            // `vid` is the graph's OWN vertex id, which `ga2jsonl` writes as a
            // property because `gid` is reserved for the loader's key. Every
            // seeded read anchors on it, so without this index a "point
            // lookup" is a scan of the whole vertex set and the fixture is the
            // measurement.
            Dataset::Graphalytics => {
                &["CREATE INDEX ga_vertex_vid IF NOT EXISTS FOR (n:Vertex) ON (n.vid)"]
            }
        }
    }

    /// One seek per declared index, run before measuring so the index BUILD is
    /// timed as itself rather than charged to whichever operation drew it.
    fn index_probes(self) -> &'static [&'static str] {
        match self {
            // None: the synthetic index is created BEFORE its data, so it is
            // built incrementally by the seeding writes and there is no
            // deferred build to force. A probe here would seek an empty label
            // and time nothing, which is worse than no probe — it would look
            // like a measurement.
            Dataset::Synthetic => &[],
            Dataset::Snb => &[
                "MATCH (p:Person {id: 1}) RETURN p.id",
                "MATCH (m:Message {id: 1}) RETURN m.id",
            ],
            Dataset::SnbPlatform => &[
                "MATCH (p:Person {id: 1}) RETURN p.id",
                "MATCH (m:Message {id: 1}) RETURN m.id",
                "MATCH (p:Person {firstName: 'Jan'}) RETURN count(p)",
                "MATCH (p:Person {firstName: 'Jan', lastName: 'Chen'}) RETURN count(p)",
            ],
            Dataset::Finbench => &[
                "MATCH (a:Account {id: 4611686018427387904}) RETURN a.id",
                "MATCH (p:Person {id: 1}) RETURN p.id",
            ],
            Dataset::Graphalytics => &["MATCH (v:Vertex {vid: 1}) RETURN v.vid"],
        }
    }
}

/// Person names to seek by, cycled from the probe key. SNB Datagen draws
/// names from real place-weighted distributions, so these common ones each
/// name a handful of persons at SF1 and some pairs name none — a zero answer
/// is still a full index probe on both engines, and the same one.
const PLATFORM_FIRST: [&str; 8] = ["Jan", "Wei", "Chen", "Jun", "Ali", "Amit", "Hans", "Jose"];
const PLATFORM_LAST: [&str; 8] = [
    "Li", "Wang", "Zhang", "Khan", "Kumar", "Singh", "Silva", "Yang",
];

/// Fifty literal ids from `key` up — the `NOT s.storyId IN $existingIds` and
/// `p.id IN [...]` lists the platform sends (fix 110's held list, fix 114's
/// newest-first pick). Rendered as a literal because the harness renders
/// statements, not parameters, on both engines alike.
fn platform_id_list(key: u64, space: u64) -> String {
    let space = space.max(1);
    let ids: Vec<String> = (0..50u64)
        .map(|i| ((key + 1 + i * 13) % space).to_string())
        .collect();
    format!("[{}]", ids.join(", "))
}

/// The platform's read shapes on the SNB schema — see `Dataset::SnbPlatform`.
/// Each names the production fix it exercises so a regression in the
/// per-shape table points at a mechanism.
const SNB_PLATFORM_SHAPES: &[Shape] = &[
    // `MATCH (n:UserDataNode {userId, nodeType: 'contact'}) RETURN count(n)`
    // — one probe of a declared composite (fix 115).
    Shape {
        name: "plat-composite-count",
        weight: 20,
        locality: Locality::Zipfian,
    },
    // The same two-key seek as a listing (fix 115's columnar seek).
    Shape {
        name: "plat-composite-list",
        weight: 10,
        locality: Locality::Zipfian,
    },
    // `MATCH (a:NewsArticle) RETURN a.articleId, a.title LIMIT 5000` — the
    // bare-LIMIT listing whose walk keeps its columns (fixes 82, 112) and
    // which returned null rows through v168 (fix 109).
    Shape {
        name: "plat-limit-listing",
        weight: 10,
        locality: Locality::Uniform,
    },
    // The story pick: a sought population, an inequality, and `NOT id IN
    // [50 ids]`, five wanted (fixes 110, 114).
    Shape {
        name: "plat-notin-pick",
        weight: 15,
        locality: Locality::Zipfian,
    },
    // `WHERE p.id IN [50 ids]` — an IN-list seek over the declared key.
    Shape {
        name: "plat-in-list-seek",
        weight: 15,
        locality: Locality::Zipfian,
    },
    // The MENTIONS aggregate: a two-hop fan-out grouped by the end's
    // property, top 20 (fixes 105–107).
    Shape {
        name: "plat-hop-group",
        weight: 15,
        locality: Locality::Zipfian,
    },
    // The chat-membership shape: a hop, then `OPTIONAL MATCH … WHERE` and a
    // count per end (fix 113's fan-out probe; the per-end column binding).
    Shape {
        name: "plat-optional-count",
        weight: 15,
        locality: Locality::Zipfian,
    },
];

/// The graph-algorithm read shapes.
///
/// # Why a separate set rather than four more entries in `SYNTHETIC_SHAPES`
///
/// An algorithm is two to four ORDERS of magnitude more expensive than a point
/// lookup — PageRank over the default 20,000-node corpus is tens of
/// milliseconds against tens of microseconds. Mixed into the ordinary shape
/// table at any weight that made it appear at all, one call in a hundred would
/// dominate the wall clock and every existing level's throughput number would
/// become a measurement of how often an algorithm happened to be drawn.
///
/// That is not a reason to leave algorithms unstressed. It is a reason to
/// stress them in their OWN profiles, where their cost is the subject rather
/// than the noise — and to mark those profiles diagnostic, so the headline
/// sweep keeps its profile count and its mutation history unchanged.
///
/// # What the three profiles measured
///
/// ```text
///   algo-read  @2 / 4,000 nodes   213 ops/s   p50  9.31 ms   p99  28.77 ms   0 errors
///   algo-mixed @4 / 3,000 nodes   173 ops/s   p50 22.38 ms   p99  58.70 ms   0 errors
///   algo-churn @4 / 2,500 nodes   ~140 ops/s  p50 ~28 ms     p99  ~66 ms     0 errors
/// ```
///
/// The three differ in what the writers do to the PROJECTION, which is the
/// only thing that distinguishes them:
///
/// - `algo-read` leaves it alone.
/// - `algo-mixed` invalidates it AND changes its topology — the projections
///   name `['LINK', 'SLINK']` and the writer creates `SLINK`, which was not
///   true of the first version: it wrote `SLINK` while the projections named
///   `LINK` alone, so every write bumped the adjacency epoch and none of them
///   ever changed a single edge the algorithm could see. The cache-invalidation
///   cost was measured; the recomputation over genuinely different topology
///   was not.
/// - `algo-churn` DELETES nodes out from under it, and its integrity checks
///   reconcile: every survivor's anchor relationship still binds.
///
/// **An algorithm under a 50% write stream costs about 3.3x its uncontended
/// latency**, and the reason is structural rather than contention: the
/// projection is keyed on the adjacency epoch, so every write invalidates it
/// and each run rebuilds the CSR from scratch. There is no incremental
/// maintenance of a projection and this is what its absence costs.
///
/// That number is the one thing neither the unit tests nor `algowidth` can
/// produce. The tests run one algorithm on a quiet graph; `algowidth` measures
/// the width split on a static one. Only a concurrent mix shows what the cache
/// discipline costs when the graph is moving, which is the state a production
/// caller is actually in.
/// LDBC FinBench read shapes, named for the traversal each complex read
/// imposes rather than claiming to BE that read — the same honesty
/// `SNB_SHAPES` states: the official tcr reads bind parameters from a
/// substitution file and nine of the twelve are defined WITH truncation, so a
/// mix that reproduced them without it would be measuring a different query.
///
/// What a concurrency mix needs is the spread of access PATHS, and these are
/// FinBench's: an indexed account point lookup, a transfer hop out, the same
/// hop walked against its stored direction, a two-hop chain, the ownership
/// edge from a Person, the busiest edge type in the corpus (`withdraw`, at
/// 918k it outnumbers `transfer`), and one whole-graph aggregate.
const FINBENCH_SHAPES: &[Shape] = &[
    Shape {
        name: "fb-account",
        weight: 30,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "fb-transfer-out",
        weight: 20,
        locality: Locality::Zipfian,
    },
    // The REVERSE of `fb-transfer-out`: the same logical edge, walked against
    // its stored direction. Both name one indexed Account; a planner that
    // anchors on the bound endpoint answers them alike, so a divergence here
    // is a standing measurement rather than something a reader must know to
    // look for.
    Shape {
        name: "fb-transfer-in",
        weight: 15,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "fb-transfer-2hop",
        weight: 10,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "fb-owner",
        weight: 8,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "fb-withdraw",
        weight: 7,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "fb-signin",
        weight: 5,
        locality: Locality::Uniform,
    },
    Shape {
        name: "fb-loan-apply",
        weight: 3,
        locality: Locality::Zipfian,
    },
    // WEIGHT 1, AND IT STILL DOMINATES. Measured on the SF1 corpus: this
    // shape is ~450 ms where every other shape here is sub-millisecond, so at
    // weight 2 it took 95.3% of all read time and the mix's headline ops/s was
    // really reporting this one aggregate. That is not a defect — grouping
    // 813k transfer edges by account level is genuinely that much work — but
    // it means the PER-SHAPE table is the thing to read in this dataset, not
    // the throughput line. Kept because a whole-graph aggregate is a real
    // access path and dropping it would hide the cost entirely.
    Shape {
        name: "fb-amount-agg",
        weight: 1,
        locality: Locality::Uniform,
    },
];

/// Graphalytics read shapes: the access paths its six kernels impose, plus the
/// kernels themselves.
///
/// # Why the kernels carry so little weight
///
/// A Graphalytics kernel on a real graph is two to four ORDERS OF MAGNITUDE
/// more expensive than a vertex lookup — `kgs` BFS is 82 s where a seek is
/// sub-millisecond. `FINBENCH_SHAPES` already recorded what happens when that
/// is not respected: `fb-amount-agg` at weight 2 took 95.3% of all read time
/// and the headline ops/s was really reporting one aggregate.
///
/// So the kernels sit at weight 1 and the traversal shapes carry the mix. The
/// ops/s line then describes the traversal load, and the PER-SHAPE table is
/// where a kernel's cost is read. Dropping the kernels entirely would be
/// worse: the whole point of this dataset is what a projection rebuild costs
/// when writes are invalidating it, and only a kernel pays that.
const GRAPHALYTICS_SHAPES: &[Shape] = &[
    Shape {
        name: "ga-vertex",
        weight: 34,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ga-out",
        weight: 22,
        locality: Locality::Zipfian,
    },
    // The REVERSE of `ga-out`, against the stored direction. Graphalytics
    // reads several of its graphs undirected, so both directions are real
    // access paths and a divergence between them is worth standing evidence.
    Shape {
        name: "ga-in",
        weight: 16,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ga-2hop",
        weight: 12,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ga-degree",
        weight: 8,
        locality: Locality::Zipfian,
    },
    // The neighbourhood a triangle count walks -- LCC's inner loop, issued as
    // an ordinary query so its cost is visible without running the kernel.
    Shape {
        name: "ga-triangle-probe",
        weight: 5,
        locality: Locality::Zipfian,
    },
    // WEIGHT 1 EACH, for the reason in the doc comment above. These are the
    // shapes that pay the projection rebuild under a write stream.
    Shape {
        name: "ga-bfs",
        weight: 1,
        locality: Locality::Zipfian,
    },
    Shape {
        name: "ga-wcc",
        weight: 1,
        locality: Locality::Uniform,
    },
];

const ALGO_SHAPES: &[Shape] = &[
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
    // The PATH procedures, which are the ones with an endpoint that a
    // concurrent writer can move under them.
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
    // ── The modes that are not `stream` ───────────────────────────────────
    //
    // `stream` was the only mode any harness issued, which left the three
    // that do something else entirely untested under load:
    //
    // - `stats` runs the same computation and returns one row, so it is the
    //   arm that shows how much of a `stream` call is the COMPUTATION and how
    //   much is materialising a row per node.
    // - `mutate` publishes into the result cache, which has a byte budget and
    //   an eviction path. The key varies with the driving key, so repeated
    //   calls ACCUMULATE distinct results and the eviction actually fires —
    //   one fixed key would overwrite in place and never reach it.
    // - `write` is the only algorithm operation that touches the KEYSPACE. It
    //   writes a property to every projected node through the ordinary write
    //   path in short transactions, so under `algo-mixed` it runs concurrently
    //   with the harness's own writers. That is the contention case no other
    //   shape produces: two writers, one of them holding a computation's worth
    //   of results.
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
    // The heaviest shape here by far — one property per projected node — so
    // its weight is the smallest. It is present because it is the only one
    // that writes, not because it is representative.
    Shape {
        name: "algo-write",
        weight: 5,
        locality: Locality::Uniform,
    },
];

/// Render one graph-algorithm read.
///
/// Every one is bounded so a single call cannot run away under a profile that
/// is measuring throughput: the projection is the `:Stress` label the harness
/// seeds, `k` is small, and the path shapes name two concrete endpoints.
fn render_algo(shape: &Shape, key: u64, space: u64) -> String {
    let other = (key + space / 3 + 1) % space.max(1);
    match shape.name {
        "algo-pagerank" => "CALL engram.algo.pagerank.stream({nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK'], maxIterations: 5})              YIELD score RETURN count(score) AS c"
            .to_string(),
        "algo-wcc" => "CALL engram.algo.wcc.stream({nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK']}) YIELD componentId RETURN count(componentId) AS c"
            .to_string(),
        "algo-kshortest" => format!(
            "MATCH (a:Stress {{k: {key}}}), (b:Stress {{k: {other}}})              CALL engram.algo.kshortestpaths.stream({{nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK'], sourceNode: id(a), targetNode: id(b), k: 3}})              YIELD totalCost RETURN count(totalCost) AS c"
        ),
        // `stats` over the same projection: the computation without the rows.
        "algo-stats" => "CALL engram.algo.wcc.stats({nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK']}) YIELD nodeCount, relationshipCount              RETURN nodeCount"
            .to_string(),
        // A VARYING key, so results accumulate in the cache and its byte
        // budget is reached. A fixed key would overwrite in place and the
        // eviction path would never run.
        "algo-mutate" => format!(
            "CALL engram.algo.degree.mutate({{nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK'], mutateKey: 'm{}'}})              YIELD mutateKey RETURN mutateKey",
            key % 32
        ),
        // Read the cache back. `result.list()` and nothing else, deliberately.
        //
        // The comment here used to claim that `result.stream` on an
        // unpublished key was "exercised on purpose" — a claim of coverage in
        // a comment above code that did not provide it, which is the quietest
        // kind of wrong. It cannot be provided here either: a refusal is
        // counted by this harness as an error and fails the level, so a shape
        // that deliberately refused would make every run red and hide the real
        // failures it exists to find. The refusal path belongs in a functional
        // test, and is in one.
        //
        // `result.list()` under load is still worth having: it reads the cache
        // while `algo-mutate` is publishing into it and evicting from it,
        // which is the only concurrent reader that path gets.
        "algo-result" => format!(
            "CALL engram.algo.result.list() YIELD mutateKey, stale              RETURN count(mutateKey) AS c, {}",
            key % 2
        ),
        // THE ONLY ALGORITHM OPERATION THAT WRITES. A property per projected
        // node, through the ordinary write path, in short transactions —
        // concurrent with the harness's own writers under `algo-mixed`.
        "algo-write" => format!(
            "CALL engram.algo.degree.write({{nodeLabels: ['Stress'],              relationshipTypes: ['LINK', 'SLINK'], writeProperty: 'deg{}',              writeBatchSize: 256}}) YIELD nodesWritten RETURN nodesWritten",
            key % 4
        ),
        "algo-allshortest" => format!(
            "MATCH p = allShortestPaths((a:Stress {{k: {key}}})-[:LINK*1..4]->             (b:Stress {{k: {other}}})) RETURN count(p) AS c"
        ),
        other => unreachable!("unknown algorithm shape {other}"),
    }
}

fn render_read(ds: Dataset, shape: &Shape, key: u64, space: u64) -> String {
    if shape.name.starts_with("algo-") {
        return render_algo(shape, key, space);
    }
    match ds {
        Dataset::Synthetic => match shape.name {
            "point-lookup" => format!("MATCH (n:Stress {{k: {key}}}) RETURN n.k, n.pad"),
            "one-hop" => format!("MATCH (n:Stress {{k: {key}}})-[:LINK]->(m) RETURN m.k LIMIT 25"),
            "two-hop" => format!(
                "MATCH (n:Stress {{k: {key}}})-[:LINK]->()-[:LINK]->(m) RETURN m.k LIMIT 25"
            ),
            // Bounded deliberately: an unbounded `*` is a correctness question
            // for the row budget (covered by the server's own tests), not a
            // throughput measurement, and it would swamp every other shape.
            "var-length" => {
                format!("MATCH (n:Stress {{k: {key}}})-[:LINK*1..2]->(m) RETURN count(m) AS c")
            }
            "aggregate" => format!(
                "MATCH (n:Stress) WHERE n.b = {} RETURN n.b, count(*) AS c",
                key % 16
            ),
            "top-k" => "MATCH (n:Stress) RETURN n.k ORDER BY n.k DESC LIMIT 20".to_string(),
            "scan-filter" => format!("MATCH (n:Stress) WHERE n.k > {key} RETURN count(n) AS c"),
            other => unreachable!("unknown synthetic shape {other}"),
        },
        Dataset::Snb => match shape.name {
            "is1-profile" => format!(
                "MATCH (p:Person {{id: {key}}}) \
                 RETURN p.firstName, p.lastName, p.birthday, p.locationIP, p.browserUsed"
            ),
            "is3-friends" => format!(
                "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person) \
                 RETURN f.id, f.firstName LIMIT 25"
            ),
            "ic-foaf" => format!(
                "MATCH (p:Person {{id: {key}}})-[:KNOWS]-()-[:KNOWS]-(f:Person) \
                 RETURN count(DISTINCT f) AS c"
            ),
            // The REVERSE traversal — HAS_CREATOR points message → person, so
            // this walks it against its stored direction. A different index
            // path from every other shape here, and the one most likely to
            // regress silently.
            "is5-by-creator" => format!(
                "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {{id: {key}}}) \
                 RETURN m.id LIMIT 25"
            ),
            "is5-anchored" => format!(
                "MATCH (p:Person {{id: {key}}})<-[:HAS_CREATOR]-(m:Message) \
                 RETURN m.id LIMIT 25"
            ),
            "ic6-friend-tags" => format!(
                "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
                 MATCH (m)-[:HAS_TAG]->(t:Tag) \
                 RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10"
            ),
            "knows-var-length" => format!(
                "MATCH (p:Person {{id: {key}}})-[:KNOWS*1..2]-(f:Person) \
                 RETURN count(DISTINCT f) AS c"
            ),
            // The probed id stays inside 0..persons (space IS the Person key
            // space) — valid as a Message id only because both labels are
            // dense from 0 and messages outnumber persons. The multiply just
            // decorrelates the probe from the Person lookups in the same mix;
            // it does NOT reach ids past `persons`.
            "is7-replies" => format!(
                "MATCH (m:Message {{id: {}}})<-[:REPLY_OF]-(c:Comment) RETURN c.id LIMIT 25",
                key.wrapping_mul(7) % space.max(1)
            ),
            "agg-by-city" => "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) \
                 RETURN c.name, count(p) AS n ORDER BY n DESC LIMIT 10"
                .to_string(),
            other => unreachable!("unknown snb shape {other}"),
        },
        // Every account key is BASE + n (see `FINBENCH_ACCOUNT_ID_BASE`).
        // Person keys are dense from 1, so a person shape takes `key % persons
        // + 1` and never asks for id 0, which the corpus does not carry.
        Dataset::Finbench => {
            let acct = FINBENCH_ACCOUNT_ID_BASE + (key % space.max(1));
            let person = (key % space.max(1)) + 1;
            match shape.name {
                "fb-account" => format!(
                    "MATCH (a:Account {{id: {acct}}})                      RETURN a.accountLevel, a.createTime, a.isBlocked, a.nickname"
                ),
                "fb-transfer-out" => format!(
                    "MATCH (a:Account {{id: {acct}}})-[t:transfer]->(b:Account)                      RETURN b.id, t.amount LIMIT 25"
                ),
                "fb-transfer-in" => format!(
                    "MATCH (a:Account)-[t:transfer]->(b:Account {{id: {acct}}})                      RETURN a.id, t.amount LIMIT 25"
                ),
                "fb-transfer-2hop" => format!(
                    "MATCH (a:Account {{id: {acct}}})-[:transfer]->()-[:transfer]->(c:Account)                      RETURN count(DISTINCT c) AS c"
                ),
                "fb-owner" => format!(
                    "MATCH (p:Person {{id: {person}}})-[:own]->(a:Account)                      RETURN a.id LIMIT 25"
                ),
                "fb-withdraw" => format!(
                    "MATCH (a:Account {{id: {acct}}})-[w:withdraw]->(b:Account)                      RETURN b.id, w.amount LIMIT 25"
                ),
                // `count(m)` and not `m.id`: the Medium and Loan property sets
                // were never probed on the corpus, and a shape that names a
                // property the store does not carry returns null columns while
                // looking healthy. Counting binds the pattern without
                // asserting a schema this harness has not verified.
                "fb-signin" => format!(
                    "MATCH (m:Medium)-[:signIn]->(a:Account {{id: {acct}}}) RETURN count(m) AS c"
                ),
                "fb-loan-apply" => format!(
                    "MATCH (p:Person {{id: {person}}})-[:apply]->(l:Loan) RETURN count(l) AS c"
                ),
                "fb-amount-agg" => "MATCH (a:Account)-[t:transfer]->()                      RETURN a.accountLevel AS lvl, count(t) AS n ORDER BY n DESC LIMIT 10"
                    .to_string(),
                other => unreachable!("unknown finbench shape {other}"),
            }
        }
        Dataset::Graphalytics => {
            // `vid` is the graph's own vertex id and the corpus numbers them
            // from 0, so the key is used directly rather than offset the way
            // FinBench's accounts are.
            let vid = key % space.max(1);
            match shape.name {
                "ga-vertex" => format!("MATCH (v:Vertex {{vid: {vid}}}) RETURN v.vid"),
                "ga-out" => format!(
                    "MATCH (v:Vertex {{vid: {vid}}})-[:LINK]->(w) RETURN w.vid LIMIT 25"
                ),
                "ga-in" => format!(
                    "MATCH (v:Vertex {{vid: {vid}}})<-[:LINK]-(w) RETURN w.vid LIMIT 25"
                ),
                "ga-2hop" => format!(
                    "MATCH (v:Vertex {{vid: {vid}}})-[:LINK]->()-[:LINK]->(w)                      RETURN count(DISTINCT w) AS c"
                ),
                "ga-degree" => format!(
                    "MATCH (v:Vertex {{vid: {vid}}})-[e:LINK]-() RETURN count(e) AS d"
                ),
                // Two neighbours of one vertex that are themselves joined --
                // the membership test LCC performs per neighbour pair.
                "ga-triangle-probe" => format!(
                    "MATCH (v:Vertex {{vid: {vid}}})-[:LINK]-(a)-[:LINK]-(b)-[:LINK]-(v)                      RETURN count(*) AS t"
                ),
                // The kernels. `graphalytics: true` is NOT passed: this lane
                // measures the SHIPPED procedure surface under load, and the
                // conformance gate changes what the kernel computes. A
                // throughput number taken under one semantics and a
                // conformance result taken under the other are two different
                // measurements and must not be blended.
                "ga-bfs" => format!(
                    "MATCH (s:Vertex {{vid: {vid}}})                      CALL engram.algo.bfs.stream({{nodeLabels: ['Vertex'],                      relationshipTypes: ['LINK'], sourceNode: id(s)}})                      YIELD depth RETURN count(depth) AS c"
                ),
                "ga-wcc" => "CALL engram.algo.wcc.stream({nodeLabels: ['Vertex'],                              relationshipTypes: ['LINK']})                              YIELD componentId RETURN count(DISTINCT componentId) AS c"
                    .to_string(),
                other => unreachable!("unknown graphalytics shape {other}"),
            }
        }
        Dataset::SnbPlatform => {
            let first = PLATFORM_FIRST[(key % 8) as usize];
            let last = PLATFORM_LAST[((key / 8) % 8) as usize];
            match shape.name {
                "plat-composite-count" => format!(
                    "MATCH (p:Person {{firstName: '{first}', lastName: '{last}'}}) RETURN count(p) AS n"
                ),
                "plat-composite-list" => format!(
                    "MATCH (p:Person {{firstName: '{first}', lastName: '{last}'}}) \
                     RETURN p.id, p.birthday, p.locationIP"
                ),
                "plat-limit-listing" => {
                    "MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 5000".to_string()
                }
                "plat-notin-pick" => format!(
                    "MATCH (p:Person {{firstName: '{first}'}}) \
                     WHERE p.browserUsed <> 'Safari' AND NOT p.id IN {} \
                     RETURN p.id, p.lastName, p.birthday LIMIT 5",
                    platform_id_list(key, space)
                ),
                "plat-in-list-seek" => format!(
                    "MATCH (p:Person) WHERE p.id IN {} RETURN p.id, p.firstName",
                    platform_id_list(key, space)
                ),
                "plat-hop-group" => format!(
                    "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
                     RETURN f.firstName AS name, count(*) AS c ORDER BY c DESC LIMIT 20"
                ),
                "plat-optional-count" => format!(
                    "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person) \
                     OPTIONAL MATCH (f)<-[:HAS_CREATOR]-(m:Message) WHERE m.browserUsed = 'Chrome' \
                     WITH f, count(m) AS n RETURN f.id, n ORDER BY n DESC LIMIT 25"
                ),
                other => unreachable!("unknown snb-platform shape {other}"),
            }
        }
    }
}

/// The write shape, per dataset.
///
/// The SNB insert is modelled on the Interactive update transactions: a new
/// message ATTACHED to an existing person, not a free-floating node. That
/// distinction is the whole point — an unattached `CREATE` never touches the
/// adjacency structures, so a harness built from them reports insert
/// throughput for a workload no application runs.
fn render_write(
    ds: Dataset,
    locality: Locality,
    kind: WriteKind,
    cid: usize,
    seq: u64,
    space: u64,
    nonce: u64,
) -> String {
    // Relationship writes: endpoints derived from `seq` (deterministic, no
    // RNG), distinct for `RelSpread`, pinned to node 0 for `RelHub`.
    match kind {
        WriteKind::Node => {}
        // Churn writes depend on per-worker ChurnSet state (which id to
        // delete), which a pure render function cannot hold — the worker
        // plans them via `ChurnSet::plan` and renders with `render_churn_*`.
        WriteKind::DeleteChurn => {
            unreachable!("delete-churn writes are planned per worker from ChurnSet state")
        }
        WriteKind::NodeOnly => {
            // The same node `balanced` creates, WITHOUT the HAS_CREATOR edge:
            // same labels, same indexed `id`, same property shape, so the
            // index and membership churn are identical and only the adjacency
            // invalidation is removed.
            return format!(
                "CREATE (m:Message:Comment {{id: {}, creationDate: {},                  content: 'stress', length: 6}})",
                (cid as u64) << 40 | seq,
                1_400_000_000_000i64 + seq as i64
            );
        }
        WriteKind::NodeOnlyFreshProps => {
            return format!(
                "CREATE (m:Message:Comment {{mid: {}, mdate: {},                  mtext: 'stress', mlen: 6}})",
                (cid as u64) << 40 | seq,
                1_400_000_000_000i64 + seq as i64
            );
        }
        WriteKind::NodeOnlyNoLabels => {
            // Identical to `NodeOnlyFreshProps` minus the two labels: same
            // property names (which no read seeks), same shape, same id space.
            return format!(
                "CREATE (m {{mid: {}, mdate: {}, mtext: 'stress', mlen: 6}})",
                (cid as u64) << 40 | seq,
                1_400_000_000_000i64 + seq as i64
            );
        }
        WriteKind::UniqueCreate => {
            // Mask the client id off `seq` so every client races the SAME
            // values; the nonce keeps levels from replaying spent values.
            let contested = (nonce << 32) | (seq & 0xFFFF_FFFF);
            return format!("CREATE (:Uniq {{u: {contested}}})");
        }
        WriteKind::RelSpread | WriteKind::RelHub => {
            let space = space.max(2);
            let a = 1 + (seq.wrapping_mul(2_654_435_761) % (space - 1));
            let b = if kind == WriteKind::RelHub {
                0
            } else {
                1 + ((a + 1 + seq % 97) % (space - 1))
            };
            return match ds.family() {
                Dataset::Snb => format!(
                    "MATCH (a:Person {{id: {a}}}), (b:Person {{id: {b}}}) \
                     CREATE (a)-[:STRESSED]->(b)"
                ),
                // Between two ACCOUNTS, which is what a transfer is. The edge
                // type stays `STRESSED` so the dangling-edge integrity check
                // covers this family unchanged.
                Dataset::Finbench => format!(
                    "MATCH (a:Account {{id: {}}}), (b:Account {{id: {}}}) CREATE (a)-[:STRESSED]->(b)",
                    FINBENCH_ACCOUNT_ID_BASE + a,
                    FINBENCH_ACCOUNT_ID_BASE + b
                ),
                // Between two VERTICES, which is the only node a Graphalytics
                // corpus has. Without this arm it fell to the catch-all below
                // and tried to match `:Stress` nodes, WHICH THAT CORPUS DOES
                // NOT CONTAIN — so every "write" matched nothing and created
                // nothing, and `algo-mixed`'s 50 % write stream was 50 %
                // no-ops. The throughput line still counted them, and the
                // rel-integrity probe then looked for the `:STRESSED` edges
                // that were never made, found no rows at all, and reported
                // "unreadable row" at every level.
                //
                // The edge type stays `STRESSED` so the dangling-edge check
                // covers this family unchanged, exactly as FinBench's does.
                Dataset::Graphalytics => format!(
                    "MATCH (a:Vertex {{vid: {a}}}), (b:Vertex {{vid: {b}}})                      CREATE (a)-[:STRESSED]->(b)"
                ),
                _ => format!(
                    "MATCH (a:Stress {{k: {a}}}), (b:Stress {{k: {b}}}) CREATE (a)-[:SLINK]->(b)"
                ),
            };
        }
    }
    match (ds.family(), locality) {
        (Dataset::Synthetic, Locality::Hot) => {
            "MATCH (n:Stress {k: 0}) SET n.hits = coalesce(n.hits, 0) + 1".to_string()
        }
        (Dataset::Synthetic, _) => format!("CREATE (:StressW {{c: {cid}, s: {seq}}})"),
        (Dataset::Snb, Locality::Hot) => {
            "MATCH (p:Person {id: 0}) SET p.hits = coalesce(p.hits, 0) + 1".to_string()
        }
        // Person ids are dense from ONE here, so the contended node is id 1.
        // `{id: 0}` would match nothing and the contention profile would report
        // a clean pass having contended over nothing at all.
        (Dataset::Finbench, Locality::Hot) => {
            "MATCH (p:Person {id: 1}) SET p.hits = coalesce(p.hits, 0) + 1".to_string()
        }
        // A standalone node, as the synthetic lane writes: it exercises the
        // write path without inventing FinBench entities whose schema this
        // harness would then be asserting.
        (Dataset::Finbench, _) => format!("CREATE (:StressW {{c: {cid}, s: {seq}}})"),
        (_, _) => {
            let author = seq % space.max(1);
            format!(
                "MATCH (p:Person {{id: {author}}}) \
                 CREATE (m:Message:Comment {{id: {}, creationDate: {}, \
                 content: 'stress', length: 6}})-[:HAS_CREATOR]->(p)",
                // Disjoint per client so two writers never mint the same id.
                (cid as u64) << 40 | seq,
                1_400_000_000_000i64 + seq as i64
            )
        }
    }
}

// ─── Delete churn ───────────────────────────────────────────────────────────
//
// The churn workload alternates create and delete over a PER-WORKER
// population. Both halves of that sentence are load-bearing:
//
// * per-worker, because a worker deleting another worker's node makes the
//   post-level reconciliation ambiguous — under OCC retries there is no way
//   to say whose ack a survivor contradicts. Worker id spaces are disjoint
//   (`seq` carries the `cid << 40` prefix) and the victim set is local, so
//   every acked op has exactly one accountable ledger.
// * alternates, because pure insertion is what every other write profile
//   already measures. The delete half — node removal, index maintenance
//   under removal, and rel cleanup via DETACH — is the path nothing else
//   exercises.
//
// The state machine below is deliberately free of I/O so it can be tested as
// arithmetic; the worker thread only glues it to the wire.

/// Live nodes a worker accumulates before its first delete, and roughly the
/// population it then oscillates on. Big enough that a victim was created
/// measurably earlier than its delete (create-then-LATER-delete), small
/// enough that the per-level survivor count stays trivial to verify.
const CHURN_FLOOR: usize = 16;

/// One planned churn write.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ChurnPlan {
    /// CREATE a node (wired to the worker's anchor) with this id.
    Create {
        /// The node id — the write sequence itself, worker-disjoint.
        id: u64,
    },
    /// DETACH DELETE the node with this id — popped from the live set
    /// BEFORE the send, and never restored.
    Delete {
        /// The victim's id, formerly the oldest entry of the live set.
        id: u64,
    },
}

/// Per-worker churn state: the ids this worker created (and the server
/// acked) but has not yet deleted, oldest first, plus the acked-op ledger
/// the post-level reconciliation reads.
#[derive(Default)]
struct ChurnSet {
    /// Created-and-acked ids not yet handed to a delete, oldest first.
    live: std::collections::VecDeque<u64>,
    /// Creates the server acked. Only these enter `live`.
    creates_acked: u64,
    /// Deletes the server acked — counted ONCE per acked statement. The
    /// server retries OCC conflicts internally, so one `Ok` is one delete
    /// regardless of how many attempts it took; a conflict that surfaces
    /// instead acked nothing and is counted nowhere.
    deletes_acked: u64,
}

impl ChurnSet {
    /// Plan the write op for sequence `seq`: odd sequences delete the oldest
    /// live node once the population has reached [`CHURN_FLOOR`]; everything
    /// else creates `seq` itself as the new id (worker-disjoint, because
    /// `seq` carries the `cid << 40` prefix).
    ///
    /// A delete victim is popped HERE, before the send. On a refusal or a
    /// transport error it is NOT restored: a double delete is therefore
    /// impossible by construction, and any discrepancy an unacked delete
    /// leaves behind must surface in the reconciliation rather than be
    /// papered over locally.
    fn plan(&mut self, seq: u64) -> ChurnPlan {
        if seq % 2 == 1 && self.live.len() >= CHURN_FLOOR {
            let victim = self.live.pop_front().expect("floor guarantees a victim");
            return ChurnPlan::Delete { id: victim };
        }
        ChurnPlan::Create { id: seq }
    }

    /// Record a server-acked op. Creates enter the live set; deletes only
    /// bump the ledger (the victim already left `live` in [`ChurnSet::plan`]).
    fn ack(&mut self, plan: ChurnPlan) {
        match plan {
            ChurnPlan::Create { id } => {
                self.live.push_back(id);
                self.creates_acked += 1;
            }
            ChurnPlan::Delete { .. } => self.deletes_acked += 1,
        }
    }
}

/// The reconciliation verdict: acked creates minus acked deletes against a
/// fresh count of survivors.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Reconciliation {
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

/// Pure churn arithmetic: does the acked ledger match the measured corpus?
/// `i128` so a ledger that somehow acked more deletes than creates reports a
/// mismatch instead of panicking on unsigned underflow.
fn reconcile(creates_acked: u64, deletes_acked: u64, survivors: u64) -> Reconciliation {
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

/// The per-worker anchor every churn create wires a rel to. Nonce-scoped so
/// levels never bind each other's anchors.
fn render_churn_anchor(cid: usize, nonce: u64) -> String {
    format!("CREATE (:ChurnAnchor {{cid: {cid}, nonce: {nonce}}})")
}

/// A churn create: the node AND its anchor rel in one statement, so every
/// victim carries a rel for its later DETACH DELETE to clean up.
fn render_churn_create(cid: usize, id: u64, nonce: u64) -> String {
    format!(
        "MATCH (a:ChurnAnchor {{cid: {cid}, nonce: {nonce}}}) \
         CREATE (a)-[:CHURN]->(:Churn {{id: {id}, cid: {cid}, nonce: {nonce}}})"
    )
}

/// A churn delete. DETACH, because the node holds its anchor rel — rel
/// cleanup is the half of the delete path this profile exists to exercise
/// (a plain DELETE here would refuse with "still has relationships").
/// `nonce` is in the match because `id` alone repeats across levels: `seq`
/// restarts at `cid << 40` on every level.
fn render_churn_delete(id: u64, nonce: u64) -> String {
    format!("MATCH (n:Churn {{id: {id}, nonce: {nonce}}}) DETACH DELETE n")
}

// ─── Workload profiles ──────────────────────────────────────────────────────

/// A named mix. These are exactly the divergent workloads a release has to be
/// characterised on — quoting the requirement: high read/low write, high
/// read/high write, high write/low read, plus pattern variation.
#[derive(Clone, Copy)]
struct Profile {
    name: &'static str,
    /// Percent of operations that are writes.
    write_pct: u64,
    /// Where writes land. `Hot` is the contention case: every writer targets
    /// the same node, which is what `snbconc`'s disjoint id spaces cannot show.
    write_locality: Locality,
    /// What a write IS. `Node` is the classic insert/update mix; the `Rel*`
    /// kinds exist because no node profile ever touches `create_rel`, so the
    /// guard-row cost (W1.1) was structurally invisible to this harness.
    write_kind: WriteKind,
    what: &'static str,
    /// A DIAGNOSTIC control, excluded from `all`.
    ///
    /// The §9 legs (`balanced-nodeonly`, `balanced-freshprops`,
    /// `balanced-disjoint`) exist to remove one write-side effect at a time and
    /// are meaningless as headline numbers. Letting them into `all` would do
    /// two bad things at once: change the profile COUNT every recorded sweep is
    /// compared against, and change the mutation history each later profile
    /// inherits — the sweep's write profiles mutate the store in order, so
    /// inserting three profiles makes every profile after them a different
    /// measurement. Runnable by name; never part of the headline set.
    diagnostic: bool,
    /// Read shapes to use INSTEAD of the dataset's, when this profile is
    /// measuring something the ordinary mix cannot express.
    ///
    /// `None` for every profile that predates graph algorithms, so their
    /// numbers are byte-for-byte the numbers they were.
    shapes: Option<&'static [Shape]>,
}

#[derive(Clone, Copy, PartialEq)]
enum WriteKind {
    /// A node create or hot-node update, per `write_locality`.
    Node,
    /// A relationship between two DISTINCT pseudo-random endpoints.
    RelSpread,
    /// A relationship whose destination is always node 0 — every write
    /// serialises through one guard row, the documented hub cost.
    RelHub,
    /// A CREATE under a UNIQUE constraint where every client races the SAME
    /// value sequence — one winner per value, everyone else a clean
    /// constraint refusal, and the post-level check proves zero duplicates.
    UniqueCreate,
    /// A node create with NO relationship — the third leg of §9's control.
    ///
    /// `balanced` writes a node AND a `HAS_CREATOR` edge; `balanced-disjoint`
    /// writes only an edge, of a type no read traverses. Those two differ in
    /// TWO ways, so the pair cannot say which matters: creating the node also
    /// grows two label memberships and the `Message.id` range index, and
    /// creating the edge also invalidates an adjacency table the reads use.
    /// This isolates the node half.
    NodeOnly,
    /// The same node-only write with the property NAMES changed so they
    /// collide with nothing a read seeks — the fourth leg of §9's control.
    ///
    /// `NodeOnly` writes `id`, and the range-index EPOCH is keyed on the
    /// property-name token alone (`PropLogs = BTreeMap<u32, ChangeLog<..>>`,
    /// `prop_epoch(token)`) while the index CACHE is keyed per (label,
    /// property). So writing a `Message` with an `id` marks the `Person.id`
    /// index stale — for a label the write never touched — and nearly every
    /// read shape in the mix anchors on `Person {id: N}`.
    ///
    /// This writes `mid`/`mdate`/`mtext`/`mlen` instead. Same node, same two
    /// labels, same membership churn, same allocation, same commit path — only
    /// the property-name collision is removed.
    NodeOnlyFreshProps,
    /// The same node-only write with NO LABELS — the fifth leg of §9.
    ///
    /// `balanced-freshprops` (node + two labels) against `balanced-disjoint`
    /// (an edge, no node) was read as "label-membership churn ~34%". That delta
    /// contains FOUR differences, not one: named-label membership, the node
    /// record write, the property storage, and stats maintenance. Attributing
    /// all of it to membership is the same confound that made `freshprops`
    /// itself misattribute +17% to a defect worth ~0%.
    ///
    /// This leg removes exactly ONE of the four. What it does NOT remove is
    /// stated because it matters: `note_membership_of` also touches the
    /// ALL-NODES snapshot (`u32::MAX`) for every node regardless of labels, so
    /// that churn remains. No SNB read shape matches unlabelled, so the reads
    /// under test are unaffected by it — but the leg isolates NAMED-label
    /// membership, not membership in general.
    NodeOnlyNoLabels,
    /// Create-then-later-delete over a per-worker population: even write
    /// sequences CREATE a node wired by a rel to the worker's own anchor,
    /// odd sequences DETACH DELETE the OLDEST node the same worker created,
    /// once the live population reaches [`CHURN_FLOOR`]. No other profile
    /// ever runs the delete path, so node removal, index maintenance under
    /// removal, and rel cleanup were all structurally invisible to this
    /// harness before it.
    DeleteChurn,
}

const PROFILES: &[Profile] = &[
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
    // The rel profiles run LAST so the six classic profiles' corpus
    // trajectory stays comparable with pre-W1.1 sweeps.
    // THE DISJOINT CONTROL for §9. Same 50/50 shape as `balanced`, but the
    // write creates a `:STRESSED` relationship between two Persons instead of a
    // `:Message:Comment` node with a `HAS_CREATOR` edge — and `STRESSED` is a
    // type NO read shape traverses.
    //
    // Everything else is held: same client count, same read mix, same store,
    // same commit path, same tail, same global adjacency epoch (a rel write
    // bumps it either way). The ONLY thing removed is that the writes stop
    // invalidating the adjacency tables the reads use.
    //
    // Weighted by share of read time, ~80% of `balanced`'s read slowdown sits
    // in shapes that traverse HAS_CREATOR — `ic6-friend-tags` alone is 76% of
    // read time and goes 1.38x. If that is adjacency staleness, this profile
    // recovers most of the -40% interference. If it does not, the staleness
    // story is wrong and what remains is global to any write, which is a
    // different search and worth knowing in one run.
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
    // LAST, for the same corpus-trajectory reason as the rel profiles — and
    // doubly so: churn is the one profile that REMOVES data, so anything
    // running after it would see a corpus no earlier sweep ever saw.
    // ── The graph-algorithm profiles ──────────────────────────────────────
    // Diagnostic, so the headline sweep keeps its profile COUNT and the
    // mutation history each later profile inherits. Runnable by name.
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

// ─── Measurement ────────────────────────────────────────────────────────────

/// The hot node's counter value — the ground truth the contention profile's
/// acked writes must reconcile against. A throughput number over lost
/// updates must refuse to print as a PASS (P0.4 of the scale-and-integrity
/// plan): the incumbent's contention figure documents exactly that failure,
/// and this harness must not be able to reproduce it silently.
fn hot_counter(ds: Dataset, c: &mut Client) -> Option<i64> {
    let q = match ds.family() {
        Dataset::Synthetic => "MATCH (n:Stress {k: 0}) RETURN coalesce(n.hits, 0)",
        Dataset::Finbench => "MATCH (p:Person {id: 1}) RETURN coalesce(p.hits, 0)",
        _ => "MATCH (p:Person {id: 0}) RETURN coalesce(p.hits, 0)",
    };
    // Loud on every failure mode: a verification that silently skips reads
    // as a PASS, which is exactly the lie this check exists to prevent.
    match c.query(q) {
        Ok(rows) => match rows.first() {
            // The client hands back the record's field list; a single-column
            // row is a one-element list around the value.
            Some(engram_cypher::Value::Int(n)) => Some(*n),
            Some(engram_cypher::Value::List(fields)) => match fields.first() {
                Some(engram_cypher::Value::Int(n)) => Some(*n),
                other => {
                    eprintln!("[stress] hot-counter read returned {other:?}, not an Int — {q}");
                    None
                }
            },
            other => {
                eprintln!("[stress] hot-counter read returned {other:?}, not an Int — {q}");
                None
            }
        },
        Err(e) => {
            eprintln!("[stress] hot-counter read FAILED ({e}) — {q}");
            None
        }
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

#[derive(Default)]
struct Samples {
    reads: Vec<u64>,
    writes: Vec<u64>,
    errors: u64,
    refusals: u64,
    /// Latency per read shape. The aggregate percentiles answer "is it fast";
    /// only this answers "WHICH of the eight things it does is slow", and a mix
    /// whose cost is concentrated in one shape is indistinguishable from a
    /// uniformly mediocre engine until you split it out.
    per_shape: std::collections::BTreeMap<&'static str, Vec<u64>>,
    /// Acked churn creates (delete-churn levels only; zero elsewhere).
    churn_creates: u64,
    /// Acked churn deletes — one per server-acked DETACH DELETE statement.
    churn_deletes: u64,
}

struct LevelResult {
    clients: usize,
    secs: f64,
    r_ops: usize,
    w_ops: usize,
    r: Vec<u64>,
    w: Vec<u64>,
    errors: u64,
    refusals: u64,
    /// Throughput in each second of the run, to expose collapse and drift that
    /// a single average hides — a run that does 10k/s then 200/s averages to
    /// something that looks healthy and is not.
    per_sec: Vec<u64>,
    /// Fix 86: when this level's workers were released, as unix milliseconds
    /// — the same clock the server stamps its maintenance lines with, so
    /// `per_sec[i]` is the second `[started_unix_ms + 1000*i, +1000*(i+1))`
    /// and a stalled second can be aligned with the server's events.
    started_unix_ms: u64,
}

/// Fix 86: the wall clock, unix milliseconds. `0` if the clock is before the
/// epoch, which is a broken host rather than a case to handle.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fix 86: the latency past which a statement is logged individually, with
/// its unix-ms start, so the stalled seconds of a level can be attributed to
/// WHICH statements stalled — writes (a lock) or reads (the read path) — and
/// to which shape. `STRESS_SLOW_MS` overrides the default of 250.
fn slow_ms() -> u64 {
    static SLOW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SLOW.get_or_init(|| {
        std::env::var("STRESS_SLOW_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250)
    })
}

impl LevelResult {
    fn rps(&self) -> f64 {
        (self.r_ops + self.w_ops) as f64 / self.secs
    }

    /// Sustained degradation: the second half's mean throughput over the first
    /// half's. Near 1.0 is steady; well below means the server got slower as the
    /// run went on.
    ///
    /// This is the failure a stress test exists to find — a compaction cliff,
    /// unbounded memory, a lock convoy, a cache that stops paying. All of them
    /// show as a TREND, and a trend over halves is immune to one noisy second.
    fn trend(&self) -> f64 {
        if self.per_sec.len() < 4 {
            return 1.0;
        }
        let half = self.per_sec.len() / 2;
        let mean = |s: &[u64]| -> f64 { s.iter().sum::<u64>() as f64 / s.len().max(1) as f64 };
        let first = mean(&self.per_sec[..half]);
        let second = mean(&self.per_sec[half..]);
        if first == 0.0 { 1.0 } else { second / first }
    }

    /// Stall floor: the 10th-percentile second over the MEDIAN second.
    ///
    /// Catches "it stopped serving for a while" without being destroyed by a
    /// single slow second, which is what the previous metric (min over max)
    /// could not distinguish. On a shared host that difference is not academic:
    /// a read-only run measured seconds of
    /// `414 515 571 408 309 648 614 644 436 484 534 474 570 507 562` — no
    /// stall, no trend, ordinary jitter — and scored 0.48 by min/max and 0.24
    /// on another run, tripping a 0.25 threshold. It was flagging the
    /// workstation, not the server.
    fn floor(&self) -> f64 {
        if self.per_sec.len() < 4 {
            return 1.0;
        }
        let mut v = self.per_sec.clone();
        v.sort_unstable();
        let p10 = v[(v.len() as f64 * 0.10) as usize];
        let median = v[v.len() / 2];
        if median == 0 {
            1.0
        } else {
            p10 as f64 / median as f64
        }
    }

    /// Why this level's throughput may NOT be quoted, if it may not.
    ///
    /// A sweep prints ten profiles and, before this existed, could defend
    /// eight. The two it could not still printed a number in the same column
    /// as the eight it could, and those numbers reached a comparison table:
    ///
    ///  - `unique-create` against a database nobody reset acked ZERO writes
    ///    out of 135,383 attempts and reported `0.00 ops/s`. Read as a
    ///    throughput it says the other engine is infinitely slower. It is not
    ///    a throughput; it is a refusal count.
    ///  - `contention` at one client did 11 writes in 20 s with a single
    ///    operation taking 26.3 SECONDS. The mean over a window one op did not
    ///    finish inside is not a rate.
    ///
    /// So the verdict travels WITH the number rather than replacing it — the
    /// data stays visible as evidence, and the claim it can support is
    /// labelled. Silence would hide a real defect; an unlabelled number
    /// launders one into a benchmark win.
    /// `max_us` is the level's slowest sample — the caller holds the merged,
    /// sorted latency vector, so it is passed in rather than recomputed.
    fn not_quotable_because(&self, max_us: u64) -> Option<String> {
        if self.r_ops + self.w_ops == 0 {
            return Some(format!(
                "the level completed no operations at all ({} refusal(s), {} error(s))",
                self.refusals, self.errors
            ));
        }
        if self.w_ops == 0 && self.refusals > 0 {
            return Some(format!(
                "every write was refused ({} refusal(s), 0 acked): this measures \
                 refusal handling, not throughput — check that the corpus was \
                 reset for this run",
                self.refusals
            ));
        }
        // A single operation spanning a large fraction of the window means the
        // server stalled; the mean is then an artefact of where the stall fell.
        let window_us = (self.secs * 1_000_000.0) as u64;
        if window_us > 0 && max_us >= window_us / 2 {
            return Some(format!(
                "one operation took {:.1} s of a {:.1} s window: the server \
                 stalled, so the mean is not a rate",
                max_us as f64 / 1e6,
                self.secs
            ));
        }
        None
    }
}

/// The `(bare, bound)` pair out of a one-row, two-column result.
///
/// `Client::query` yields one `Value` per ROW, and a row is a `List` of its
/// columns — the counts are one level deeper than they look, and reading them
/// wrong is how a verifier silently stops verifying.
fn counts_pair(row: Option<&engram_cypher::Value>) -> Option<(u64, u64)> {
    match row? {
        engram_cypher::Value::List(cols) => match (cols.first(), cols.get(1)) {
            (Some(engram_cypher::Value::Int(x)), Some(engram_cypher::Value::Int(y))) => {
                Some((*x as u64, *y as u64))
            }
            _ => None,
        },
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "usage: stress <addr> <profile|all> <clients-csv> <seconds>
              [--seed N] [--keys N] [--json PATH]
              [--dataset synthetic|snb|snb-platform|finbench|graphalytics] [--shape NAME]

datasets:
   synthetic   the harness seeds its own world (default) — runs anywhere, incl. CI
   snb         ATTACH to a server already holding an LDBC SNB corpus
               (`portserve <corpus dir> <addr>`); --keys is probed, not seeded
   snb-platform  the SNB corpus read through the PLATFORM's access shapes
   finbench    LDBC FinBench; account ids are 2^62-based and the attach probe
               asserts that base rather than trusting it
   graphalytics  an LDBC Graphalytics graph (`ga2jsonl` then `snbload`), read
               through the access paths its kernels impose. --keys is the
               vertex count. The two KERNEL shapes sit at weight 1 because a
               kernel is orders of magnitude dearer than a seek; read the
               per-shape table, not the throughput line

profiles:"
        );
        for p in PROFILES {
            eprintln!("   {:<12} {:>3}% writes  — {}", p.name, p.write_pct, p.what);
        }
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let want = args[2].clone();
    let clients: Vec<usize> = args[3]
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .filter(|&n: &usize| n > 0)
        .collect();
    let seconds: u64 = args[4].parse().expect("seconds must be a number");
    let flag = |name: &str, dflt: u64| -> u64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(dflt)
    };
    let seed = flag("--seed", 424_242);
    let mut keys = flag("--keys", 20_000).max(1);
    let json_path = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let dataset = match args
        .iter()
        .position(|a| a == "--dataset")
        .and_then(|i| args.get(i + 1))
    {
        Some(s) => match Dataset::parse(s) {
            Some(d) => d,
            None => {
                eprintln!("unknown dataset `{s}`; try `synthetic`, `snb` or `snb-platform`");
                std::process::exit(2);
            }
        },
        None => Dataset::Synthetic,
    };

    let profiles: Vec<&Profile> = if want == "all" {
        PROFILES.iter().filter(|p| !p.diagnostic).collect()
    } else {
        match PROFILES.iter().find(|p| p.name == want) {
            Some(p) => vec![p],
            None => {
                eprintln!("unknown profile `{want}`; try one of, or `all`:");
                for p in PROFILES {
                    eprintln!("   {}", p.name);
                }
                std::process::exit(2);
            }
        }
    };

    // ── Seed the graph ──────────────────────────────────────────────────
    //
    // The harness builds its own corpus rather than depending on a downloaded
    // one, so it runs anywhere — including in CI, which is the only way a
    // stress test becomes a regression gate rather than an occasional ritual.
    let mut c = match Client::connect(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[stress] cannot reach {addr}: {e}");
            std::process::exit(1);
        }
    };
    let t0 = Instant::now();
    // The INDEXES the read shapes need, before anything is measured. See
    // `Dataset::indexes` for why this is a fixture requirement and not tuning.
    for stmt in dataset.indexes() {
        if let Err(e) = c.run(stmt) {
            eprintln!("[stress] could not create an index ({stmt}): {e}");
            std::process::exit(1);
        }
    }
    // FORCE the index builds, and time them separately.
    //
    // `CREATE INDEX` returns immediately; the range index is built by the first
    // query that seeks it. That is a one-time DDL cost, and charging it to the
    // steady-state throughput of whichever operation happens to run first is
    // simply wrong — at 1.48M nodes it was a single 5.0 s operation inside a
    // 20 s measurement, which dragged the whole level's floor to zero and
    // reported a stall that was really a build.
    //
    // Reported rather than hidden: "how long does an index take to become
    // usable on a corpus this size" is a real operational number, and it is one
    // a warm-up that quietly swallowed it would destroy.
    for probe in dataset.index_probes() {
        let t = Instant::now();
        if let Err(e) = c.run(probe) {
            eprintln!("[stress] index warm probe failed ({probe}): {e}");
            std::process::exit(1);
        }
        eprintln!(
            "[stress] index build (first seek): {:.0} ms  {probe}",
            t.elapsed().as_secs_f64() * 1e3
        );
    }

    match dataset.family() {
        Dataset::Synthetic => {
            eprintln!("[stress] seeding {keys} nodes at {addr}");
            // UNWIND-batched so seeding is not itself the bottleneck.
            let batch = 500u64;
            let mut made = 0u64;
            while made < keys {
                let n = batch.min(keys - made);
                let stmt = format!(
                    "UNWIND range({made}, {}) AS i \
                     CREATE (:Stress {{k: i, b: i % 16, pad: 'x'}})",
                    made + n - 1
                );
                if let Err(e) = c.run(&stmt) {
                    eprintln!("[stress] seed failed at {made}: {e}");
                    std::process::exit(1);
                }
                made += n;
            }
            // A sparse link layer so the hop shapes have edges to walk.
            for chunk in 0..(keys / batch).max(1) {
                let lo = chunk * batch;
                let hi = (lo + batch).min(keys);
                let stmt = format!(
                    "UNWIND range({lo}, {}) AS i \
                     MATCH (a:Stress {{k: i}}), (b:Stress {{k: (i * 7 + 1) % {keys}}}) \
                     CREATE (a)-[:LINK]->(b)",
                    hi.saturating_sub(1)
                );
                if let Err(e) = c.run(&stmt) {
                    eprintln!("[stress] link failed at {lo}: {e}");
                    std::process::exit(1);
                }
            }
            eprintln!("[stress] seeded in {:.1}s", t0.elapsed().as_secs_f64());
        }
        Dataset::Finbench => {
            // ATTACH, and ASSERT THE ID BASE rather than trusting it.
            //
            // `fbgen` mints account ids from 2^62. If that ever changes, every
            // account lookup in the mix misses, every read returns zero rows,
            // and the run reports the index's NEGATIVE path as throughput — a
            // green benchmark measuring nothing. One probe turns that silent
            // failure into a loud one.
            let accounts = match c.run("MATCH (a:Account) RETURN a.id") {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("[stress] could not probe the FinBench corpus at {addr}: {e}");
                    std::process::exit(1);
                }
            };
            if accounts == 0 {
                eprintln!(
                    "[stress] {addr} holds no :Account nodes — the finbench dataset ATTACHES to                      an already-loaded corpus."
                );
                std::process::exit(1);
            }
            let base_probe =
                format!("MATCH (a:Account {{id: {FINBENCH_ACCOUNT_ID_BASE}}}) RETURN a.id");
            match c.run(&base_probe) {
                Ok(1) => {}
                Ok(n) => {
                    eprintln!(
                        "[stress] {addr}: account id base {FINBENCH_ACCOUNT_ID_BASE} matched {n}                          node(s), expected exactly 1 — this corpus does not use the id scheme                          the harness keys on, so every lookup would miss. Refusing to measure."
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("[stress] could not verify the FinBench account id base: {e}");
                    std::process::exit(1);
                }
            }
            keys = accounts;
            eprintln!(
                "[stress] attached to a FinBench corpus at {addr}: {accounts} accounts (probed                  in {:.1}s); id base verified; --keys set from the corpus",
                t0.elapsed().as_secs_f64()
            );
        }
        Dataset::Graphalytics => {
            // ATTACH, and probe the VERTEX COUNT rather than trusting `--keys`.
            //
            // `ga2jsonl` writes the graph's own vertex id as `vid`, numbered
            // from 0, and every read shape anchors on it. A key space larger
            // than the graph makes most lookups miss, and a run whose reads
            // mostly return nothing reports the index's NEGATIVE path as
            // throughput — a green benchmark measuring nothing.
            //
            // This arm exists because the SNB probe below is a `_ =>`
            // catch-all: without it, a Graphalytics corpus was probed for
            // `:Person`, found none, and exited telling the operator to load
            // an SNB corpus. That is the fall-through the `Finbench` variant's
            // own doc comment warns about, arriving for the second time.
            let vertices = match c.run("MATCH (v:Vertex) RETURN v.vid") {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("[stress] could not probe the Graphalytics corpus at {addr}: {e}");
                    std::process::exit(1);
                }
            };
            if vertices == 0 {
                eprintln!(
                    "[stress] {addr} holds no :Vertex nodes — the graphalytics dataset ATTACHES                      to an already-loaded graph.
          Convert and load one with:                      ga2jsonl <graph dir> <out> --name G  &&  snbload <out> {addr} --match-on gid"
                );
                std::process::exit(1);
            }
            keys = vertices;
            eprintln!(
                "[stress] attached to a Graphalytics graph at {addr}: {vertices} vertices                  (probed in {:.1}s); --keys set from the graph",
                t0.elapsed().as_secs_f64()
            );
        }
        _ => {
            // ATTACH: the corpus is already there. Probe its size rather than
            // trusting `--keys` — a key space larger than the corpus makes most
            // lookups miss, and a run whose reads mostly return nothing measures
            // the index's negative path and reports it as throughput.
            //
            // The probe returns one ROW per person rather than a `count(p)`
            // scalar, because the client reports rows and not values — and the
            // row count is the answer. snbgen emits persons with dense ids
            // `0..persons`, which is the invariant the key space relies on.
            let persons = match c.run("MATCH (p:Person) RETURN p.id") {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("[stress] could not probe the SNB corpus at {addr}: {e}");
                    std::process::exit(1);
                }
            };
            if persons == 0 {
                eprintln!(
                    "[stress] {addr} holds no :Person nodes — the snb dataset ATTACHES to an \
                     already-loaded corpus.\n          Start one with:  portserve <corpus dir> {addr}"
                );
                std::process::exit(1);
            }
            keys = persons;
            eprintln!(
                "[stress] attached to an SNB corpus at {addr}: {persons} persons \
                 (probed in {:.1}s); --keys set from the corpus",
                t0.elapsed().as_secs_f64()
            );
        }
    }

    // `--shape NAME` narrows the mix to one shape. A mix is the right default —
    // it is what a workload looks like — but when the per-shape table names a
    // slow one, isolating it is the next question, and re-deriving it by hand
    // against a live corpus is how a diagnosis ends up measuring a different
    // query than the harness ran.
    let only_shape = args
        .iter()
        .position(|a| a == "--shape")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let all_shapes = dataset.shapes();
    let read_shapes: Vec<Shape> = match &only_shape {
        None => all_shapes.to_vec(),
        Some(name) => {
            let picked: Vec<Shape> = all_shapes
                .iter()
                .filter(|s| s.name == name)
                .copied()
                .collect();
            if picked.is_empty() {
                eprintln!("unknown shape `{name}` for dataset {dataset:?}; known shapes:");
                for s in all_shapes {
                    eprintln!("   {}", s.name);
                }
                std::process::exit(2);
            }
            eprintln!("[stress] restricted to shape `{name}`");
            picked
        }
    };
    // Leaked deliberately: the client threads borrow this for the life of the
    // process, and a table of a handful of shapes is the cheapest possible way
    // to hand them a `'static` slice without an Arc on every operation.
    let read_shapes: &'static [Shape] = Box::leak(read_shapes.into_boxed_slice());
    // The dataset's total is no longer used directly: a profile may override
    // the shape set (the algorithm levels do), so the weight total is computed
    // per profile from whichever set is in force.
    let _ = read_shapes;
    let mut all_rows: Vec<(String, LevelResult)> = Vec::new();
    // Integrity failures found by the self-verification reads — merged into
    // the verdict, so a lossy run FAILS regardless of its throughput.
    let mut integrity: Vec<String> = Vec::new();
    // A per-level nonce so contested-value profiles never replay a spent
    // value space across levels or profiles.
    let mut level_counter: u64 = 0;

    for prof in &profiles {
        // Whichever shape set this profile measures — see `Profile::shapes`.
        let prof_shapes: &'static [Shape] = prof.shapes.unwrap_or(read_shapes);
        println!(
            "\n=== profile: {} ({}% writes, {} writes) — {}",
            prof.name,
            prof.write_pct,
            match prof.write_locality {
                Locality::Hot => "HOT-KEY",
                _ => "spread",
            },
            prof.what
        );
        println!(
            "{:>7} {:>10} {:>10} {:>9} {:>9} {:>9} {:>10} {:>9} {:>7} {:>7} {:>7}",
            "clients",
            "ops/s",
            "r_ops/s",
            "p50(ms)",
            "p95(ms)",
            "p99(ms)",
            "p99.9(ms)",
            "max(ms)",
            "errors",
            "trend",
            "floor"
        );

        for &k in &clients {
            level_counter += 1;
            let level_nonce = level_counter;
            if prof.write_kind == WriteKind::UniqueCreate {
                // The constraint, then a clean slate for THIS level's value
                // range — for the same reason delete-churn pre-cleans below,
                // and it was missing here.
                //
                // Values are nonce-scoped (`(nonce << 32) | seq`), which stops
                // levels within one run from replaying each other. It does NOT
                // stop RUNS from colliding: `level_nonce` restarts at 0 every
                // process. Against a server that already ran this harness,
                // every attempt then hits a value the previous run inserted
                // and refuses — measured on the Neo4j baseline, whose database
                // is not reset between sweeps: by the third sweep all 135,383
                // attempts refused, 0 writes acked, reported as 0.00 ops/s.
                // The comparison table then read "engram 660x", which is not a
                // result about either engine.
                let lo = level_nonce << 32;
                let hi = lo.saturating_add(1u64 << 32);
                for stmt in [
                    "CREATE CONSTRAINT stress_u IF NOT EXISTS FOR (n:Uniq) REQUIRE n.u IS UNIQUE"
                        .to_string(),
                    format!("MATCH (n:Uniq) WHERE n.u >= {lo} AND n.u < {hi} DETACH DELETE n"),
                ] {
                    if let Err(e) = c.run(&stmt) {
                        integrity.push(format!(
                            "{}: unique level setup failed ({stmt}): {e}",
                            prof.name
                        ));
                    }
                }
            }
            if prof.write_kind == WriteKind::DeleteChurn {
                // Indexes, because the churn MATCHes are point lookups on
                // :Churn(id) and :ChurnAnchor(cid) — unindexed they are label
                // scans that grow with every level's survivors, and the
                // fixture would become the measurement. Then a clean slate
                // for THIS level's nonce: nonces restart every process, so
                // against a server that already ran this harness, leftovers
                // with the same nonce would double-bind anchors (one acked
                // create minting two nodes) and pollute the reconciliation
                // with survivors this run never created.
                for stmt in [
                    "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)".to_string(),
                    "CREATE INDEX churn_anchor_cid IF NOT EXISTS FOR (n:ChurnAnchor) ON (n.cid)"
                        .to_string(),
                    format!("MATCH (n:Churn {{nonce: {level_nonce}}}) DETACH DELETE n"),
                    format!("MATCH (a:ChurnAnchor {{nonce: {level_nonce}}}) DETACH DELETE a"),
                ] {
                    if let Err(e) = c.run(&stmt) {
                        integrity.push(format!(
                            "{}: churn level setup failed ({stmt}): {e}",
                            prof.name
                        ));
                    }
                }
            }
            // Hot-locality levels are self-verifying: every acked hot write
            // must appear in the counter, or the run is measuring loss. A
            // verification that CANNOT run fails the run — fail closed.
            let hot_before = if matches!(prof.write_locality, Locality::Hot) {
                let b = hot_counter(dataset, &mut c);
                if b.is_none() {
                    integrity.push(format!(
                        "{} @ {k} clients: the hot-counter baseline read failed — \
                         loss verification could not run",
                        prof.name
                    ));
                }
                b
            } else {
                None
            };
            let stop = Arc::new(AtomicBool::new(false));
            let ticker = Arc::new(AtomicU64::new(0));
            let mut handles = Vec::with_capacity(k);
            // Fix 86: the level's identity for the slow-statement log, and
            // its start on the wall clock. Printed once so a reader of the
            // log can bracket the level without the JSON.
            let level_tag: Arc<String> = Arc::new(format!("{}@{k}", prof.name));
            let started_unix_ms = unix_ms();
            eprintln!("[stress] level {level_tag} started t={started_unix_ms}");

            for cid in 0..k {
                let addr = addr.clone();
                let stop = Arc::clone(&stop);
                let ticker = Arc::clone(&ticker);
                let prof = **prof;
                let level_tag = Arc::clone(&level_tag);
                handles.push(std::thread::spawn(move || {
                    // Per-client seed: same global seed reproduces the run, but
                    // clients do not all issue the identical sequence (which
                    // would be a lockstep artefact, not a workload).
                    let mut rng = Rng(seed ^ ((cid as u64 + 1).wrapping_mul(0x9E37_79B9)));
                    let mut s = Samples::default();
                    let mut conn = Client::connect(&addr).ok();
                    let mut churn = ChurnSet::default();
                    // The per-worker anchor every churn create attaches to,
                    // made ONCE (a retried CREATE could mint two anchors and
                    // double every later create). If it fails, that is a
                    // transport error AND every later churn create matches
                    // nothing — acked with zero rows — which the
                    // reconciliation then reports. Loud twice over, never
                    // silent.
                    if prof.write_kind == WriteKind::DeleteChurn {
                        match conn.as_mut() {
                            Some(cn) => {
                                if cn.run(&render_churn_anchor(cid, level_nonce)).is_err() {
                                    s.errors += 1;
                                    conn = None;
                                }
                            }
                            None => s.errors += 1,
                        }
                    }
                    // Exact-fraction interleave: reproducible, no RNG needed
                    // for the read/write decision itself.
                    let mut wacc = 0u64;
                    let mut seq: u64 = (cid as u64) << 40;

                    while !stop.load(Ordering::Relaxed) {
                        wacc += prof.write_pct;
                        let do_write = wacc >= 100;
                        if do_write {
                            wacc -= 100;
                        }
                        let Some(cn) = conn.as_mut() else {
                            conn = Client::connect(&addr).ok();
                            s.errors += 1;
                            continue;
                        };

                        let mut shape_name: Option<&'static str> = None;
                        let mut churn_plan: Option<ChurnPlan> = None;
                        let (stmt, is_write) = if do_write {
                            // CONTENTION (`Hot`) has every writer update the
                            // SAME node — write-write conflict, which
                            // disjoint-id insert benchmarks cannot show.
                            let a = seq;
                            seq += 1;
                            let stmt = if prof.write_kind == WriteKind::DeleteChurn {
                                // Stateful by necessity: which id to delete
                                // depends on what this worker already
                                // created. The victim leaves the local set
                                // HERE, before the send — see ChurnSet::plan.
                                let plan = churn.plan(a);
                                churn_plan = Some(plan);
                                match plan {
                                    ChurnPlan::Create { id } => {
                                        render_churn_create(cid, id, level_nonce)
                                    }
                                    ChurnPlan::Delete { id } => {
                                        render_churn_delete(id, level_nonce)
                                    }
                                }
                            } else {
                                render_write(
                                    dataset,
                                    prof.write_locality,
                                    prof.write_kind,
                                    cid,
                                    a,
                                    keys,
                                    level_nonce,
                                )
                            };
                            (stmt, true)
                        } else {
                            // Weighted shape choice, then a locality-aware key.
                            //
                            // A profile may override the dataset's shapes —
                            // the algorithm levels do — so the weight total is
                            // recomputed from whichever set is in force rather
                            // than taken from the dataset's. Using the
                            // dataset's total against an overridden set would
                            // skew the draw toward the first shape and, if the
                            // override's total were smaller, could leave
                            // `chosen` at its initial value for most picks.
                            let shapes: &[Shape] = prof_shapes;
                            let weight: u32 = shapes.iter().map(|s| s.weight).sum();
                            let mut pickw = rng.below(u64::from(weight.max(1))) as u32;
                            let mut chosen = &shapes[0];
                            for sh in shapes {
                                if pickw < sh.weight {
                                    chosen = sh;
                                    break;
                                }
                                pickw -= sh.weight;
                            }
                            let key = chosen.locality.pick(&mut rng, keys);
                            shape_name = Some(chosen.name);
                            let stmt = render_read(dataset, chosen, key, keys);
                            // `STRESS_TRACE=1` prefixes every READ with the engine's
                            // per-statement trace marker, so a slow shape's counters
                            // land in the SERVER log for the statement AS THE HARNESS
                            // ISSUED IT — on its connection, in its mix, after its
                            // writes. The v174 is7-replies stall (5 s per call in the
                            // read-heavy level) reproduced under no hand-issued
                            // statement, traced or not; only the harness's own calls
                            // were slow. Reads only: a traced write would trace the
                            // commit path, which is not what a stall in a read mix asks.
                            // The SERVER must permit the marker (`ENGRAM_TRACE_MARKER=1`);
                            // without it the marker is an ordinary comment and the server
                            // log says so once (security plan §2.13).
                            static TRACE_READS: std::sync::OnceLock<bool> =
                                std::sync::OnceLock::new();
                            let traced = *TRACE_READS
                                .get_or_init(|| std::env::var_os("STRESS_TRACE").is_some());
                            (
                                if traced {
                                    format!("/* engram:trace */ {stmt}")
                                } else {
                                    stmt
                                },
                                false,
                            )
                        };

                        let t = Instant::now();
                        let issued_ms = unix_ms();
                        match cn.run(&stmt) {
                            Ok(_) => {
                                let us = t.elapsed().as_micros() as u64;
                                // Fix 86: a statement over the slow line is
                                // logged with its start on the wall clock,
                                // its kind and its shape — the evidence that
                                // says whether a stalled second stalled the
                                // WRITERS (a lock) or the READERS (the read
                                // path), and which shape paid.
                                if us / 1000 >= slow_ms() {
                                    eprintln!(
                                        "[slow] t={issued_ms} {}ms {level_tag} c{cid} {} {}",
                                        us / 1000,
                                        if is_write { "write" } else { "read" },
                                        shape_name.unwrap_or("-"),
                                    );
                                }
                                if is_write {
                                    s.writes.push(us);
                                    if let Some(p) = churn_plan {
                                        churn.ack(p);
                                    }
                                } else {
                                    s.reads.push(us);
                                    if let Some(n) = shape_name {
                                        s.per_shape.entry(n).or_default().push(us);
                                    }
                                }
                                ticker.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // A REFUSAL (budget, protocol) is a correct
                                // answer under load and is counted apart from a
                                // transport error — collapsing them would let a
                                // server that refuses everything look healthy.
                                let msg = e.to_string();
                                if msg.contains("budget")
                                    || msg.contains("refus")
                                    || msg.contains("already exists")
                                    || msg.contains("transaction conflict")
                                {
                                    // A constraint violation is a CORRECT
                                    // refusal — the unique-create profile
                                    // expects N-1 of them per value. A
                                    // surfaced OCC conflict is one too: the
                                    // engine published NOTHING and says
                                    // "retry", so the connection is fine and
                                    // the churn ledger, which counts only
                                    // acks, stays balanced (a conflicted
                                    // delete's victim simply survives for
                                    // the reconciliation to count).
                                    s.refusals += 1;
                                } else {
                                    s.errors += 1;
                                    conn = None;
                                }
                            }
                        }
                    }
                    s.churn_creates = churn.creates_acked;
                    s.churn_deletes = churn.deletes_acked;
                    s
                }));
            }

            // Sample throughput each second so collapse and drift are visible.
            let start = Instant::now();
            let mut per_sec = Vec::with_capacity(seconds as usize);
            let mut last = 0u64;
            for _ in 0..seconds {
                std::thread::sleep(Duration::from_secs(1));
                let now = ticker.load(Ordering::Relaxed);
                per_sec.push(now - last);
                last = now;
            }
            stop.store(true, Ordering::Relaxed);
            let mut agg = Samples::default();
            // Per-worker churn ledgers, in spawn order (index == cid). A
            // worker that failed to join leaves a hole here, and the
            // reconciliation below treats an incomplete ledger as a failure
            // — fail closed, not silently short.
            let mut churn_ledgers: Vec<(usize, u64, u64)> = Vec::new();
            for (wid, h) in handles.into_iter().enumerate() {
                if let Ok(s) = h.join() {
                    if prof.write_kind == WriteKind::DeleteChurn {
                        churn_ledgers.push((wid, s.churn_creates, s.churn_deletes));
                    }
                    agg.reads.extend(s.reads);
                    agg.writes.extend(s.writes);
                    agg.errors += s.errors;
                    agg.refusals += s.refusals;
                    agg.churn_creates += s.churn_creates;
                    agg.churn_deletes += s.churn_deletes;
                    for (k, v) in s.per_shape {
                        agg.per_shape.entry(k).or_default().extend(v);
                    }
                }
            }
            let secs = start.elapsed().as_secs_f64();
            agg.reads.sort_unstable();
            agg.writes.sort_unstable();
            let mut all: Vec<u64> = agg.reads.iter().chain(agg.writes.iter()).copied().collect();
            all.sort_unstable();

            let res = LevelResult {
                clients: k,
                secs,
                r_ops: agg.reads.len(),
                w_ops: agg.writes.len(),
                r: agg.reads,
                w: agg.writes,
                errors: agg.errors,
                refusals: agg.refusals,
                per_sec,
                started_unix_ms,
            };
            println!(
                "{:>7} {:>10.0} {:>10.0} {:>9.2} {:>9.2} {:>9.2} {:>10.2} {:>9.2} {:>7} {:>7.2} {:>7.2}",
                res.clients,
                res.rps(),
                res.r_ops as f64 / res.secs,
                pct(&all, 0.50) as f64 / 1000.0,
                pct(&all, 0.95) as f64 / 1000.0,
                pct(&all, 0.99) as f64 / 1000.0,
                pct(&all, 0.999) as f64 / 1000.0,
                all.last().copied().unwrap_or(0) as f64 / 1000.0,
                res.errors,
                res.trend(),
                res.floor(),
            );
            // Per-shape breakdown, ordered by total time spent — the column
            // that says where the mix's cost actually went. A shape at 2% of
            // operations and 80% of elapsed time is the finding; the aggregate
            // row above cannot show it.
            if !agg.per_shape.is_empty() {
                let mut rows: Vec<(&str, usize, u64, u64, u64, u64)> = agg
                    .per_shape
                    .iter()
                    .map(|(name, v)| {
                        let mut v = v.clone();
                        v.sort_unstable();
                        let total: u64 = v.iter().sum();
                        (
                            *name,
                            v.len(),
                            pct(&v, 0.50),
                            pct(&v, 0.95),
                            v.last().copied().unwrap_or(0),
                            total,
                        )
                    })
                    .collect();
                let grand: u64 = rows.iter().map(|r| r.5).sum::<u64>().max(1);
                rows.sort_by_key(|r| std::cmp::Reverse(r.5));
                for (name, n, p50, p95, mx, total) in rows {
                    println!(
                        "        {:<18} {:>7} ops {:>9.2} {:>9.2} {:>9.2}   {:>5.1}% of read time",
                        name,
                        n,
                        p50 as f64 / 1000.0,
                        p95 as f64 / 1000.0,
                        mx as f64 / 1000.0,
                        100.0 * total as f64 / grand as f64,
                    );
                }
            }
            // Unique-create levels are integrity-checked: zero duplicate
            // values, and the winners equal the acked writes. Delete-churn
            // levels re-run the probe as a cross-check that the delete path
            // left the constraint-guarded corpus consistent — but only when
            // a Uniq corpus EXISTS: standalone delete-churn has none, and
            // "zero duplicates" over an empty label is the negative
            // assertion that passes against an empty fixture. Say "not
            // applicable" instead of implying a verified invariant.
            let uniq_cross_check = if prof.write_kind == WriteKind::DeleteChurn {
                match c.run("MATCH (n:Uniq) RETURN n.u") {
                    Ok(0) => {
                        println!(
                            "        unique-integrity cross-check: not applicable — no Uniq \
                             corpus in this run"
                        );
                        false
                    }
                    Ok(_) => true,
                    Err(e) => {
                        // A probe that cannot run is a failure, not a skip —
                        // "not applicable" would discard the diagnosis.
                        integrity.push(format!(
                            "{} @ {} clients: Uniq population probe failed: {e}",
                            prof.name, res.clients
                        ));
                        false
                    }
                }
            } else {
                prof.write_kind == WriteKind::UniqueCreate
            };
            if uniq_cross_check {
                match c.run("MATCH (n:Uniq) WITH n.u AS u, count(*) AS c WHERE c > 1 RETURN u") {
                    Ok(0) if prof.write_kind == WriteKind::DeleteChurn => {
                        println!(
                            "        unique-integrity check: the Uniq corpus still holds zero \
                             duplicates"
                        );
                    }
                    Ok(0) => {
                        println!(
                            "        unique-integrity check: {} acked winner(s), {} refusal(s), \
                             zero duplicates",
                            res.w_ops, res.refusals
                        );
                    }
                    Ok(d) => integrity.push(format!(
                        "{} @ {} clients: {d} DUPLICATE unique value(s) committed",
                        prof.name, res.clients
                    )),
                    Err(e) => integrity.push(format!(
                        "{} @ {} clients: unique-integrity probe failed: {e}",
                        prof.name, res.clients
                    )),
                }
            }
            // Rel-write levels are integrity-checked: every stress edge must
            // bind BOTH endpoints. A dangling edge (the W1.1 corruption
            // class) shows as a count divergence or an error here.
            if prof.write_kind != WriteKind::Node {
                let ty = match dataset.family() {
                    Dataset::Synthetic => "SLINK",
                    _ => "STRESSED",
                };
                // ONE STATEMENT, so ONE SNAPSHOT.
                //
                // Two separate queries are two instants, and anything landing
                // between them shows as a count divergence — with the WRONG
                // SIGN. The SF1 w6 sweep reported "577054 edge(s) but only
                // 586126 bind both endpoints — DANGLING EDGES", where bound
                // EXCEEDED bare; a dangling edge can only make bound lower.
                //
                // The engine was fine. A quiescent re-query of that exact store
                // answered 585,765 both ways three times, and
                // `engram-graph/tests/rel_count_forms_agree.rs` drives both
                // forms against deliberately stale derived tables, across
                // deletes, across a compaction that emits the CSR, and against
                // a graph that adopted a persisted sidecar. The CHECK was
                // racing its own workload.
                //
                // A verifier that cries wolf is worse than no verifier: it
                // teaches the reader to discount the one time it is right.
                // The second half BINDS both endpoints — that is the whole
                // check. Binding forces each endpoint node to resolve, so an
                // edge whose endpoint is gone drops out of the bound count and
                // not the bare one. Written anonymously it would be the same
                // query twice and could never detect anything.
                let q = format!(
                    "MATCH ()-[r:{ty}]->() WITH count(r) AS bare \
                     MATCH (a)-[q:{ty}]->(b) RETURN bare, count(q) AS bound"
                );
                match c.query(&q) {
                    Ok(rows) => match counts_pair(rows.first()) {
                        Some((x, y)) if x == y => {
                            println!(
                                "        rel-integrity check: {x} edge(s), all endpoints bind"
                            );
                        }
                        Some((x, y)) => integrity.push(format!(
                            "{} @ {} clients: {x} edge(s) but only {y} bind both endpoints — \
                             DANGLING EDGES",
                            prof.name, res.clients
                        )),
                        None => integrity.push(format!(
                            "{} @ {} clients: rel-integrity probe returned an unreadable row",
                            prof.name, res.clients
                        )),
                    },
                    Err(e) => integrity.push(format!(
                        "{} @ {} clients: rel-integrity probe failed: {e}",
                        prof.name, res.clients
                    )),
                }
            }
            // Delete-churn levels reconcile, FAIL CLOSED: per worker and in
            // total, acked creates minus acked deletes MUST equal a fresh
            // count of survivors carrying this level's nonce. A probe that
            // cannot run, or a worker ledger that never arrived, is a
            // failure — a reconciliation that silently skips would read as a
            // PASS, the exact lie the hot-counter check exists to prevent.
            if prof.write_kind == WriteKind::DeleteChurn {
                if churn_ledgers.len() != k {
                    integrity.push(format!(
                        "{} @ {k} clients: only {} of {k} churn ledger(s) reported — \
                         reconciliation could not run",
                        prof.name,
                        churn_ledgers.len()
                    ));
                }
                let mut worker_mismatch = false;
                for &(wid, cr, de) in &churn_ledgers {
                    let probe =
                        format!("MATCH (n:Churn {{nonce: {level_nonce}, cid: {wid}}}) RETURN n.id");
                    match c.run(&probe) {
                        Ok(survivors) => {
                            if let Reconciliation::Mismatch { expected, measured } =
                                reconcile(cr, de, survivors)
                            {
                                worker_mismatch = true;
                                if res.errors > 0 {
                                    // A transport error after a server-side
                                    // commit loses the ack, not the write —
                                    // unattributable, and the transport
                                    // errors already fail the run on their
                                    // own.
                                    println!(
                                        "        churn worker {wid}: expected {expected} \
                                         survivor(s), measured {measured} — MISMATCH \
                                         (unattributable: transport errors ate acks)"
                                    );
                                } else {
                                    integrity.push(format!(
                                        "{} @ {} clients: churn worker {wid} acked {cr} \
                                         create(s), {de} delete(s), but {measured} node(s) \
                                         survive (expected {expected}) — CHURN LOSS",
                                        prof.name, res.clients
                                    ));
                                }
                            }
                        }
                        Err(e) => integrity.push(format!(
                            "{} @ {} clients: churn reconciliation probe for worker {wid} \
                             failed ({e}) — reconciliation could not run",
                            prof.name, res.clients
                        )),
                    }
                }
                // FAIL CLOSED on zero work: reconcile(0,0,0) balances, so a
                // level that never acked a single churn create would sail
                // through every probe above and print a PASS over nothing —
                // the vacuous verdict an empty corpus always produces. A
                // delete-churn level that acked zero creates did no
                // verifiable work; one that acked well past the floor but
                // zero deletes never engaged the path this profile exists to
                // test.
                if agg.churn_creates == 0 {
                    integrity.push(format!(
                        "{} @ {} clients: ZERO acked churn create(s) — the level did no \
                         verifiable churn work; refusing the vacuous pass",
                        prof.name, res.clients
                    ));
                } else if agg.churn_deletes == 0
                    && agg.churn_creates >= (k * 2 * CHURN_FLOOR) as u64
                {
                    integrity.push(format!(
                        "{} @ {} clients: {} acked create(s) but ZERO acked delete(s) — \
                         the delete path never engaged",
                        prof.name, res.clients, agg.churn_creates
                    ));
                }
                // The total is a FRESH query, not a sum of the per-worker
                // probes — a node minted with a wrong or missing cid hides
                // from every per-worker count and shows up only here.
                let total_survivors = match c.run(&format!(
                    "MATCH (n:Churn {{nonce: {level_nonce}}}) RETURN n.id"
                )) {
                    Ok(survivors) => {
                        match reconcile(agg.churn_creates, agg.churn_deletes, survivors) {
                            Reconciliation::Balanced(n) => {
                                if !worker_mismatch && churn_ledgers.len() == k {
                                    println!(
                                        "        churn-integrity check: {} created, {} deleted, \
                                         {n} survivor(s) — every worker reconciles",
                                        agg.churn_creates, agg.churn_deletes
                                    );
                                }
                            }
                            Reconciliation::Mismatch { expected, measured } => {
                                if res.errors > 0 {
                                    println!(
                                        "        churn total: expected {expected} survivor(s), \
                                         measured {measured} — MISMATCH (unattributable: \
                                         transport errors ate acks)"
                                    );
                                } else {
                                    integrity.push(format!(
                                        "{} @ {} clients: {} acked create(s) minus {} acked \
                                         delete(s), but {measured} node(s) survive (expected \
                                         {expected}) — CHURN LOSS",
                                        prof.name,
                                        res.clients,
                                        agg.churn_creates,
                                        agg.churn_deletes
                                    ));
                                }
                            }
                        }
                        Some(survivors)
                    }
                    Err(e) => {
                        integrity.push(format!(
                            "{} @ {} clients: the total churn reconciliation probe failed ({e}) \
                             — reconciliation could not run",
                            prof.name, res.clients
                        ));
                        None
                    }
                };
                // No churn id may commit twice within a level — the churn
                // complement of the unique-create duplicate probe (ids are
                // popped once by construction, so a duplicate here is the
                // ENGINE committing one create twice).
                match c.run(&format!(
                    "MATCH (n:Churn {{nonce: {level_nonce}}}) \
                     WITH n.id AS i, count(*) AS c WHERE c > 1 RETURN i"
                )) {
                    Ok(0) => {}
                    Ok(d) => integrity.push(format!(
                        "{} @ {} clients: {d} DUPLICATE churn id(s) committed",
                        prof.name, res.clients
                    )),
                    Err(e) => integrity.push(format!(
                        "{} @ {} clients: churn duplicate probe failed: {e}",
                        prof.name, res.clients
                    )),
                }
                // Rel cleanup: DETACH DELETE must have taken each victim's
                // anchor rel with it — this level's anchors hold exactly one
                // rel per survivor — and every CHURN rel corpus-wide must
                // bind both endpoints (the W1.1 dangling class, on the churn
                // type the generic SLINK/STRESSED probe above cannot see).
                let anchored = c.run(&format!(
                    "MATCH (a:ChurnAnchor {{nonce: {level_nonce}}})-[r:CHURN]->() RETURN id(r)"
                ));
                let bare = c.run("MATCH ()-[r:CHURN]->() RETURN id(r)");
                let bound = c.run("MATCH (a)-[r:CHURN]->(b) RETURN id(r)");
                match (anchored, bare, bound) {
                    (Ok(anch), Ok(x), Ok(y)) if x == y && Some(anch) == total_survivors => {
                        println!(
                            "        churn-rel check: {anch} anchor rel(s) == survivors, all \
                             CHURN endpoints bind"
                        );
                    }
                    (Ok(anch), Ok(x), Ok(y)) => {
                        if x != y {
                            integrity.push(format!(
                                "{} @ {} clients: {x} CHURN edge(s) but only {y} bind both \
                                 endpoints — DANGLING EDGES",
                                prof.name, res.clients
                            ));
                        }
                        match total_survivors {
                            Some(surv) if anch != surv => integrity.push(format!(
                                "{} @ {} clients: {surv} churn survivor(s) but {anch} anchor \
                                 rel(s) — deletes left rels behind, or took extra ones",
                                prof.name, res.clients
                            )),
                            // total_survivors == None already failed the run
                            // in the reconciliation above.
                            _ => {}
                        }
                    }
                    (a, b, r2) => integrity.push(format!(
                        "{} @ {} clients: churn rel probe failed ({a:?} / {b:?} / {r2:?})",
                        prof.name, res.clients
                    )),
                }
            }
            if let Some(before) = hot_before {
                match hot_counter(dataset, &mut c) {
                    Some(after) => {
                        let delta = after - before;
                        let acked = res.w_ops as i64;
                        let verdict = if delta == acked {
                            "every acked write landed"
                        } else if res.errors > 0 {
                            // A transport error after a server-side commit
                            // loses the ack, not the write — the check cannot
                            // distinguish that from loss, so it only reports.
                            "MISMATCH (unattributable: transport errors ate acks)"
                        } else {
                            integrity.push(format!(
                                "{} @ {} clients: {} acked hot write(s) but the counter moved {} \
                                 — LOST UPDATES",
                                prof.name, res.clients, acked, delta
                            ));
                            "LOST UPDATES"
                        };
                        println!(
                            "        hot-counter check: acked {acked}, counter moved {delta} — {verdict}"
                        );
                    }
                    None => integrity.push(format!(
                        "{} @ {} clients: the hot-counter FINAL read failed — \
                         loss verification could not run",
                        prof.name, res.clients
                    )),
                }
            }
            all_rows.push((prof.name.to_string(), res));
        }
    }

    // ── The verdict ─────────────────────────────────────────────────────
    //
    // A stress run that prints numbers and no judgement gets skimmed. These are
    // the two failures that matter and that a table hides: transport errors
    // (the server broke) and a throughput collapse within a level (it stopped
    // serving part way, which an average conceals).
    //
    // WHAT THIS BLOCK CANNOT SAY, and why it is not fixed here. Both checks
    // below require four one-second buckets, because `trend` halves them and
    // `floor` takes a 10th percentile over them. On a level of three seconds or
    // less they DO NOT RUN, and both statistics return 1.0 -- the value of a
    // perfectly steady level -- so the level does not merely go unjudged, it is
    // guaranteed to look clean.
    //
    // The converged harness closes that by REFUSING such a level
    // (`report::NotQuotable::TooShortToJudge`) and by refusing `--seconds` below
    // the threshold at the command line. This binary deliberately does not: it
    // stays in the tree to REPRODUCE the recorded `stress.rs` series
    // (docs/converged-harness.md 7.1), and a changed verdict here would change
    // what it reproduces -- which is the one thing it exists not to do.
    //
    // So the silence is REPORTED and not acted on. `unjudged` never enters
    // `bad`, so no PASS becomes a FAIL and no exit code moves; it only stops
    // the summary from claiming a check that did not happen. For a judged
    // verdict, run the converged harness.
    let mut unjudged: Vec<String> = Vec::new();
    let mut bad = integrity;
    for (p, r) in &all_rows {
        if r.per_sec.len() <= 3 {
            unjudged.push(format!(
                "{p} @ {} clients: a {:.1} s level produced {} one-second bucket(s); the \
                 DEGRADED and STALLED checks need at least 4 and DID NOT RUN. The \
                 trend/floor of 1.00 below is a default, not a measurement",
                r.clients,
                r.secs,
                r.per_sec.len()
            ));
        }
    }
    for (p, r) in &all_rows {
        if r.errors > 0 {
            bad.push(format!(
                "{p} @ {} clients: {} transport error(s) — the server dropped connections",
                r.clients, r.errors
            ));
        }
        // Two distinct failures, asserted separately because they have
        // different causes and a single combined number hides which one fired.
        if r.per_sec.len() > 3 && r.trend() < 0.5 {
            bad.push(format!(
                "{p} @ {} clients: throughput DEGRADED over the run (second half was {:.0}% of \
                 the first) — the server got slower as it ran",
                r.clients,
                r.trend() * 100.0
            ));
        }
        if r.per_sec.len() > 3 && r.floor() < 0.25 {
            bad.push(format!(
                "{p} @ {} clients: throughput STALLED within the level (10th-percentile second \
                 was {:.0}% of the median)",
                r.clients,
                r.floor() * 100.0
            ));
        }
        // A level whose number cannot be quoted must SAY SO here, next to the
        // number, not only in the JSON. The failure this prevents is a human
        // copying a row into a comparison table because it looked like the
        // rows either side of it.
        let mut all: Vec<u64> = r.r.iter().chain(r.w.iter()).copied().collect();
        all.sort_unstable();
        if let Some(why) = r.not_quotable_because(all.last().copied().unwrap_or(0)) {
            bad.push(format!(
                "{p} @ {} clients: NOT QUOTABLE ({:.2} ops/s must not be compared) — {why}",
                r.clients,
                r.rps()
            ));
        }
    }
    println!();
    for u in &unjudged {
        println!("NOT JUDGED — {u}");
    }
    if bad.is_empty() {
        // The sentence has to be true of the levels it covers. "No throughput
        // collapse" over a set that includes an unjudged level is a claim
        // nothing checked.
        let judged = all_rows.len() - unjudged.len();
        println!(
            "PASS — {judged} judged level(s) of {} across {} profile(s): no transport \
             errors, no throughput collapse{}",
            all_rows.len(),
            profiles.len(),
            if unjudged.is_empty() {
                String::new()
            } else {
                format!(
                    ". {} level(s) were NOT JUDGED (see above) and this sentence says \
                     nothing about them",
                    unjudged.len()
                )
            }
        );
    } else {
        println!("FAIL");
        for b in &bad {
            println!("   - {b}");
        }
    }

    if let Some(path) = json_path {
        let mut s = String::from("{\"levels\":[");
        for (i, (p, r)) in all_rows.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let mut all: Vec<u64> = r.r.iter().chain(r.w.iter()).copied().collect();
            all.sort_unstable();
            let max_us = all.last().copied().unwrap_or(0);
            // The quotability verdict rides WITH the row. A consumer that
            // builds a comparison table can then refuse the row instead of
            // discovering, three documents later, that a 0.00 was a refusal
            // count and a 0.9 was a stall.
            let quotable = r.not_quotable_because(max_us);
            let quotable_json = match &quotable {
                None => "true,\"not_quotable_because\":null".to_string(),
                Some(why) => format!(
                    "false,\"not_quotable_because\":\"{}\"",
                    why.replace('\\', "\\\\").replace('"', "\\\"")
                ),
            };
            s.push_str(&format!(
                "{{\"profile\":\"{p}\",\"clients\":{},\"seconds\":{:.3},\"read_ops\":{},\
                 \"write_ops\":{},\"ops_per_sec\":{:.2},\"p50_us\":{},\"p95_us\":{},\
                 \"p99_us\":{},\"p999_us\":{},\"max_us\":{},\"errors\":{},\"refusals\":{},\
                 \"trend\":{:.4},\"floor\":{:.4},\"quotable\":{quotable_json},\"started_unix_ms\":{},\"per_sec\":{:?}}}",
                r.clients,
                r.secs,
                r.r_ops,
                r.w_ops,
                r.rps(),
                pct(&all, 0.50),
                pct(&all, 0.95),
                pct(&all, 0.99),
                pct(&all, 0.999),
                all.last().copied().unwrap_or(0),
                r.errors,
                r.refusals,
                r.trend(),
                r.floor(),
                r.started_unix_ms,
                r.per_sec
            ));
        }
        s.push_str("],\"seed\":");
        s.push_str(&seed.to_string());
        s.push_str(",\"keys\":");
        s.push_str(&keys.to_string());
        s.push('}');
        if let Err(e) = std::fs::write(&path, s) {
            eprintln!("[stress] could not write {path}: {e}");
        } else {
            eprintln!("[stress] report written to {path}");
        }
    }

    if !bad.is_empty() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a set through `n` write sequences from `start`, acking every op
    /// (the all-server-said-yes path), returning the plans in order.
    fn drive(set: &mut ChurnSet, start: u64, n: u64) -> Vec<ChurnPlan> {
        (start..start + n)
            .map(|seq| {
                let p = set.plan(seq);
                set.ack(p);
                p
            })
            .collect()
    }

    #[test]
    fn churn_builds_the_floor_before_the_first_delete() {
        let mut set = ChurnSet::default();
        let plans = drive(&mut set, 0, 64);
        let first_delete = plans
            .iter()
            .position(|p| matches!(p, ChurnPlan::Delete { .. }))
            .expect("a 64-op run must reach the delete phase");
        assert!(
            plans[..first_delete]
                .iter()
                .all(|p| matches!(p, ChurnPlan::Create { .. })),
            "every op before the first delete is a create"
        );
        assert!(
            first_delete >= CHURN_FLOOR,
            "the first victim was created at least CHURN_FLOOR ops earlier"
        );
        // Steady state: the live population oscillates on the floor.
        assert!(set.live.len() == CHURN_FLOOR || set.live.len() == CHURN_FLOOR + 1);
        assert_eq!(set.creates_acked - set.deletes_acked, set.live.len() as u64);
    }

    #[test]
    fn churn_deletes_oldest_first_and_deterministically() {
        let mut a = ChurnSet::default();
        let mut b = ChurnSet::default();
        let pa = drive(&mut a, 1 << 40, 100); // worker 1's seq space
        let pb = drive(&mut b, 1 << 40, 100);
        assert_eq!(pa, pb, "same sequences, same outcomes — same plans");
        let creates: Vec<u64> = pa
            .iter()
            .filter_map(|p| match p {
                ChurnPlan::Create { id } => Some(*id),
                ChurnPlan::Delete { .. } => None,
            })
            .collect();
        let deletes: Vec<u64> = pa
            .iter()
            .filter_map(|p| match p {
                ChurnPlan::Delete { id } => Some(*id),
                ChurnPlan::Create { .. } => None,
            })
            .collect();
        assert!(!deletes.is_empty());
        assert_eq!(
            deletes,
            creates[..deletes.len()],
            "victims are the oldest creates, in creation order"
        );
    }

    #[test]
    fn churn_ids_stay_inside_the_workers_prefix() {
        // Worker id spaces are disjoint by the `cid << 40` prefix, which is
        // what makes per-worker reconciliation unambiguous: a worker can
        // only ever delete what it created.
        let cid: u64 = 7;
        let mut set = ChurnSet::default();
        for p in drive(&mut set, cid << 40, 200) {
            let (ChurnPlan::Create { id } | ChurnPlan::Delete { id }) = p;
            assert_eq!(id >> 40, cid, "every churn id carries the worker prefix");
        }
    }

    #[test]
    fn a_victim_is_popped_before_the_send_and_never_restored() {
        let mut set = ChurnSet::default();
        // Build past the floor: seqs 0..=16 are all creates (odd seqs below
        // the floor fall back to create), so seq 17 must plan a delete.
        drive(&mut set, 0, 17);
        let p = set.plan(17);
        let ChurnPlan::Delete { id: victim } = p else {
            panic!("op 17 must be a delete, got {p:?}");
        };
        assert!(
            !set.live.contains(&victim),
            "the victim leaves the set at plan time, before the send"
        );
        // The send is never acked (refusal or transport error): the victim
        // is NOT restored, the ledger does not move, and no later plan may
        // ever name that id again — the discrepancy belongs to the
        // reconciliation, not to a local re-push.
        assert_eq!(set.deletes_acked, 0, "an unacked delete is not counted");
        let later = drive(&mut set, 18, 200);
        assert!(
            later
                .iter()
                .all(|p| !matches!(p, ChurnPlan::Delete { id } if *id == victim)),
            "a popped victim is never offered twice"
        );
    }

    #[test]
    fn an_unacked_create_never_enters_the_live_set() {
        let mut set = ChurnSet::default();
        let p = set.plan(0);
        assert_eq!(p, ChurnPlan::Create { id: 0 });
        // No ack — the create was refused or the transport failed. The id
        // must not become a future victim (deleting a node whose creation
        // was never acked would double-count under retries).
        assert!(set.live.is_empty());
        assert_eq!(set.creates_acked, 0);
    }

    #[test]
    fn reconcile_balances_and_catches_loss_both_ways() {
        assert_eq!(reconcile(10, 4, 6), Reconciliation::Balanced(6));
        // The pure arithmetic balances on zero work — deliberately. Refusing
        // the vacuous pass is the LEVEL verdict's job (it requires acked
        // creates > 0 before this Balanced counts as evidence).
        assert_eq!(reconcile(0, 0, 0), Reconciliation::Balanced(0));
        // Loss: fewer survivors than the acked ledger promises.
        assert_eq!(
            reconcile(10, 4, 5),
            Reconciliation::Mismatch {
                expected: 6,
                measured: 5
            }
        );
        // Phantoms: more survivors than were ever acked.
        assert_eq!(
            reconcile(10, 4, 7),
            Reconciliation::Mismatch {
                expected: 6,
                measured: 7
            }
        );
        // A ledger that somehow acked more deletes than creates must report
        // a mismatch, not panic on unsigned underflow.
        assert_eq!(
            reconcile(2, 3, 0),
            Reconciliation::Mismatch {
                expected: -1,
                measured: 0
            }
        );
    }

    #[test]
    fn churn_cypher_binds_the_level_and_uses_detach_delete() {
        assert_eq!(
            render_churn_delete(5, 9),
            "MATCH (n:Churn {id: 5, nonce: 9}) DETACH DELETE n"
        );
        let create = render_churn_create(3, 42, 9);
        assert!(create.contains("MATCH (a:ChurnAnchor {cid: 3, nonce: 9})"));
        assert!(create.contains("CREATE (a)-[:CHURN]->(:Churn {id: 42, cid: 3, nonce: 9})"));
        assert_eq!(
            render_churn_anchor(3, 9),
            "CREATE (:ChurnAnchor {cid: 3, nonce: 9})"
        );
    }

    #[test]
    fn delete_churn_is_registered_and_runs_last() {
        // The CLI (`all`, by-name selection, the usage listing) all iterate
        // PROFILES, so registration in the table IS the wiring.
        let p = PROFILES
            .iter()
            .find(|p| p.name == "delete-churn")
            .expect("delete-churn must be selectable by name");
        assert!(matches!(p.write_kind, WriteKind::DeleteChurn));
        assert_eq!(p.write_pct, 100);
        // LAST: churn is the one profile that REMOVES data, so it must not
        // disturb the corpus trajectory the earlier profiles are compared
        // on (the same ordering rule the rel profiles document).
        assert_eq!(
            PROFILES.last().expect("PROFILES is non-empty").name,
            "delete-churn"
        );
    }

    // ── quotability ──────────────────────────────────────────────────────
    //
    // These pin the two shapes that reached a comparison table looking like
    // measurements. Each asserts BOTH arms: the bad shape is refused AND the
    // ordinary shape beside it is still quotable — a rule that refused
    // everything would be just as useless as one that refused nothing.

    fn level(w_ops: usize, refusals: u64, secs: f64, per_sec: Vec<u64>) -> LevelResult {
        LevelResult {
            clients: 1,
            secs,
            r_ops: 0,
            w_ops,
            r: Vec::new(),
            w: Vec::new(),
            errors: 0,
            refusals,
            per_sec,
            started_unix_ms: 0,
        }
    }

    #[test]
    fn a_level_that_acked_nothing_is_not_quotable() {
        // Neo4j's unique-create at 8 clients: 135,383 refusals, 0 acked,
        // printed as 0.00 ops/s next to engram's 2,255 — read as a 660x win.
        let r = level(0, 135_383, 20.0, vec![0; 20]);
        let why = r
            .not_quotable_because(0)
            .expect("a level that acked nothing must be refused");
        assert!(
            why.contains("no operations at all"),
            "the reason must name the cause, got: {why}"
        );
    }

    #[test]
    fn every_write_refused_is_not_quotable_even_with_reads() {
        let mut r = level(0, 135_383, 20.0, vec![5; 20]);
        r.r_ops = 100; // reads landed, writes did not
        let why = r
            .not_quotable_because(1_000)
            .expect("all-writes-refused must be refused");
        assert!(
            why.contains("every write was refused") && why.contains("reset"),
            "the reason must name the cause AND the likely fix, got: {why}"
        );
    }

    #[test]
    fn a_stalled_level_is_not_quotable() {
        // engram's contention at 1 client: 11 writes in 20 s with a single
        // operation taking 26.3 SECONDS. The mean over a window one op did
        // not finish inside is not a rate.
        let r = level(11, 0, 20.0, vec![22, 0, 0, 0, 0, 0, 0, 0]);
        let why = r
            .not_quotable_because(26_328_557)
            .expect("a 26 s operation in a 20 s window must be refused");
        assert!(
            why.contains("stalled") && why.contains("26.3"),
            "the reason must quote the stall, got: {why}"
        );
    }

    #[test]
    fn an_ordinary_level_is_quotable() {
        // THE CANARY. If this ever starts failing, the rule has widened into
        // refusing real measurements, which would quietly delete the
        // benchmark rather than qualify it.
        let r = level(26_583, 0, 20.0, vec![1_300; 20]);
        assert_eq!(
            r.not_quotable_because(14_140),
            None,
            "a healthy level must stay quotable"
        );
        // And a level with SOME refusals but real acked writes is fine —
        // unique-create is *supposed* to refuse most attempts.
        let r = level(45_108, 310_966, 20.0, vec![2_300; 20]);
        assert_eq!(
            r.not_quotable_because(622_082),
            None,
            "refusals alongside acked writes are the profile working, not a fault"
        );
    }
}

#[cfg(test)]
mod finbench_tests {
    use super::*;

    /// Every shape in the table must RENDER. `render_read` ends its match in
    /// `unreachable!`, so a name in the table with no arm is a panic the
    /// moment the mix picks it — at 32 clients, minutes into a run.
    #[test]
    fn every_declared_shape_renders() {
        for shape in FINBENCH_SHAPES {
            for key in [0u64, 1, 7, 12_345, 205_499] {
                let q = render_read(Dataset::Finbench, shape, key, 205_500);
                assert!(
                    q.contains("MATCH"),
                    "shape {} rendered no MATCH: {q}",
                    shape.name
                );
            }
        }
    }

    /// THE TRAP THIS DATASET EXISTS AROUND. `fbgen` mints account ids from
    /// 2^62; a shape keyed by a bare `n` would look healthy and match nothing,
    /// reporting the index's negative path as throughput. Every rendered
    /// account lookup must carry the base.
    #[test]
    fn account_lookups_carry_the_two_to_the_sixtytwo_id_base() {
        let base = FINBENCH_ACCOUNT_ID_BASE;
        assert_eq!(base, 4_611_686_018_427_387_904, "the id base moved");
        for shape in FINBENCH_SHAPES {
            let q = render_read(Dataset::Finbench, shape, 7, 205_500);
            if q.contains("Account {id:") {
                assert!(
                    q.contains(&format!("{}", base + 7)),
                    "shape {} keys an account without the id base: {q}",
                    shape.name
                );
            }
        }
    }

    /// FinBench persons are dense from ONE. A shape asking for id 0 matches
    /// nothing, and a mix of misses reports as throughput.
    #[test]
    fn person_shapes_never_ask_for_id_zero() {
        for shape in FINBENCH_SHAPES {
            for key in [0u64, 205_500, 411_000] {
                let q = render_read(Dataset::Finbench, shape, key, 205_500);
                assert!(
                    !q.contains("Person {id: 0}"),
                    "shape {} asked for Person id 0 at key {key}: {q}",
                    shape.name
                );
            }
        }
    }

    /// Only labels and edge types the corpus actually carries, probed on the
    /// SF1 store: Account/Loan/Medium/Person/Company and transfer/withdraw/
    /// deposit/repay/signIn/own/invest/apply/guarantee. A typo here is a
    /// silent zero-row shape.
    #[test]
    fn shapes_name_only_types_the_corpus_carries() {
        const LABELS: &[&str] = &["Account", "Loan", "Medium", "Person", "Company"];
        const EDGES: &[&str] = &[
            "transfer",
            "withdraw",
            "deposit",
            "repay",
            "signIn",
            "own",
            "invest",
            "apply",
            "guarantee",
        ];
        for shape in FINBENCH_SHAPES {
            let q = render_read(Dataset::Finbench, shape, 3, 205_500);
            for tok in q.split(|c: char| !c.is_ascii_alphanumeric()) {
                if tok.starts_with(|c: char| c.is_ascii_uppercase()) && tok.len() > 2 {
                    assert!(
                        LABELS.contains(&tok) || !q.contains(&format!(":{tok}")),
                        "shape {} names label {tok}, which the corpus does not carry: {q}",
                        shape.name
                    );
                }
            }
            for e in q.split("-[").skip(1) {
                let ty = e
                    .split(']')
                    .next()
                    .unwrap_or("")
                    .trim_start_matches(|c: char| c != ':')
                    .trim_start_matches(':');
                if !ty.is_empty() {
                    assert!(
                        EDGES.contains(&ty),
                        "shape {} walks edge type {ty}, which the corpus does not carry: {q}",
                        shape.name
                    );
                }
            }
        }
    }

    /// The dataset must be reachable from the command line, and must NOT fall
    /// through to the SNB arms — `:Person` exists in both corpora and means
    /// different things, so a `_ =>` catch-all would silently key FinBench
    /// runs off SNB's assumptions.
    #[test]
    fn the_dataset_parses_and_keeps_its_own_family() {
        assert_eq!(Dataset::parse("finbench"), Some(Dataset::Finbench));
        assert_eq!(Dataset::parse("fb"), Some(Dataset::Finbench));
        assert_eq!(Dataset::Finbench.family(), Dataset::Finbench);
        assert_ne!(Dataset::Finbench.family(), Dataset::Snb);
        assert!(!Dataset::Finbench.shapes().is_empty());
        assert!(
            Dataset::Finbench
                .indexes()
                .iter()
                .any(|i| i.contains("Account")),
            "Account.id carries every point lookup and must be declared"
        );
    }
}

/// The Graphalytics dataset's shapes, and the two traps it shares with
/// FinBench.
#[cfg(test)]
mod graphalytics_tests {
    use super::*;

    /// Every shape in the table must RENDER. `render_read` ends its match in
    /// `unreachable!`, so a name in the table with no arm is a panic the
    /// moment the mix picks it — at 32 clients, minutes into a run, after the
    /// corpus has been loaded and the level released.
    #[test]
    fn every_declared_shape_renders() {
        for shape in GRAPHALYTICS_SHAPES {
            for key in [0u64, 1, 7, 12_345, 832_246] {
                let q = render_read(Dataset::Graphalytics, shape, key, 832_247);
                assert!(
                    q.contains("MATCH") || q.contains("CALL"),
                    "shape {} rendered neither MATCH nor CALL: {q}",
                    shape.name
                );
            }
        }
    }

    /// A rendered vertex key must lie inside the graph.
    ///
    /// Graphalytics vertex ids are the graph's own and `ga2jsonl` writes them
    /// as `vid`. A shape keyed past the end matches nothing, and a mix of
    /// misses reports the index's NEGATIVE path as throughput — the same trap
    /// FinBench's 2^62 id base exists around, arriving from the other
    /// direction.
    #[test]
    fn a_rendered_vertex_key_is_inside_the_graph() {
        let space = 1_000u64;
        for shape in GRAPHALYTICS_SHAPES {
            for key in [0u64, 999, 1_000, 5_000, u64::MAX / 2] {
                let q = render_read(Dataset::Graphalytics, shape, key, space);
                for cap in q.split("vid: ").skip(1) {
                    let digits: String = cap.chars().take_while(char::is_ascii_digit).collect();
                    if digits.is_empty() {
                        continue;
                    }
                    let v: u64 = digits.parse().expect("a vid is numeric");
                    assert!(
                        v < space,
                        "shape {} keyed vid {v} outside a {space}-vertex graph: {q}",
                        shape.name
                    );
                }
            }
        }
    }

    /// The kernels must NOT ask for conformance semantics.
    ///
    /// `graphalytics: true` changes what BFS and WCC compute — sentinels for
    /// unreachable vertices, separate in/out counting, a fixed iteration
    /// count. That is the right mode for a CONFORMANCE run and the wrong one
    /// here: this lane measures the shipped procedure surface under write
    /// load. A throughput number taken under one semantics and a conformance
    /// result taken under the other are two different measurements, and
    /// blending them is how a table comes to compare two things.
    #[test]
    fn the_kernels_measure_the_shipped_semantics_not_the_conformance_gate() {
        for shape in GRAPHALYTICS_SHAPES {
            let q = render_read(Dataset::Graphalytics, shape, 7, 1_000);
            assert!(
                !q.contains("graphalytics"),
                "shape {} passes the conformance gate: {q}",
                shape.name
            );
        }
    }

    /// The kernels are weight 1, and that is load-bearing.
    ///
    /// A Graphalytics kernel on a real graph is two to four orders of
    /// magnitude dearer than a vertex seek — `kgs` BFS is 82 s against a
    /// sub-millisecond lookup. `FINBENCH_SHAPES` already recorded what happens
    /// when that is not respected: `fb-amount-agg` at weight 2 consumed 95.3%
    /// of all read time, and the headline ops/s was really reporting one
    /// aggregate. This asserts the arrangement rather than trusting the
    /// comment above the table.
    #[test]
    fn the_kernels_carry_the_smallest_weight_in_the_mix() {
        let kernels: Vec<&Shape> = GRAPHALYTICS_SHAPES
            .iter()
            .filter(|s| matches!(s.name, "ga-bfs" | "ga-wcc"))
            .collect();
        assert_eq!(kernels.len(), 2, "both kernels are declared");
        let lightest_traversal = GRAPHALYTICS_SHAPES
            .iter()
            .filter(|s| !matches!(s.name, "ga-bfs" | "ga-wcc"))
            .map(|s| s.weight)
            .min()
            .expect("the mix has traversal shapes");
        for k in kernels {
            assert!(
                k.weight <= lightest_traversal,
                "{} at weight {} outweighs the lightest traversal ({lightest_traversal})",
                k.name,
                k.weight
            );
        }
    }

    /// Graphalytics is its OWN family and must never fall through to SNB's
    /// writes, seeding or hot counter. Its corpus has one label and one type;
    /// `:Person` does not exist in it at all.
    #[test]
    fn graphalytics_is_its_own_family() {
        assert_eq!(Dataset::Graphalytics.family(), Dataset::Graphalytics);
        assert_eq!(Dataset::parse("graphalytics"), Some(Dataset::Graphalytics));
        assert_eq!(Dataset::parse("ga"), Some(Dataset::Graphalytics));
        // Its declared index is on the vertex key the shapes actually seek.
        assert!(
            Dataset::Graphalytics
                .indexes()
                .iter()
                .any(|i| i.contains("Vertex") && i.contains("vid")),
            "the vertex key is unindexed, so every lookup is a scan"
        );
    }
}

/// A write must address the nodes the DATASET ACTUALLY HAS.
///
/// `render_write`'s relationship arms were written per family with a `_`
/// catch-all, and Graphalytics fell into it: the catch-all matches
/// `(a:Stress {k: …})`, and a Graphalytics corpus has no `:Stress` node — it
/// has `:Vertex`. So every relationship write matched nothing and created
/// nothing, while the throughput line counted each one. `algo-mixed`'s 50 %
/// write stream was 50 % no-ops, and the rel-integrity probe — which looks for
/// the `:STRESSED` edges those writes were supposed to make — found NO ROWS
/// and reported "unreadable row" at every client level.
///
/// The read side is already guarded (`every_declared_shape_renders`, and the
/// `Dataset` enum is deliberately not `_`-matched there). The write side was
/// not, which is how a silent no-op survived a lane that reports numbers.
#[cfg(test)]
mod writes_address_the_datasets_own_nodes {
    use super::{Dataset, Locality, WriteKind, render_write};

    /// The node label each dataset's own reads bind.
    fn node_label(ds: Dataset) -> &'static str {
        match ds {
            Dataset::Synthetic => "Stress",
            Dataset::Snb | Dataset::SnbPlatform => "Person",
            Dataset::Finbench => "Account",
            Dataset::Graphalytics => "Vertex",
        }
    }

    #[test]
    fn every_datasets_relationship_write_matches_a_label_that_dataset_has() {
        for ds in [
            Dataset::Synthetic,
            Dataset::Snb,
            Dataset::SnbPlatform,
            Dataset::Finbench,
            Dataset::Graphalytics,
        ] {
            for kind in [WriteKind::RelSpread, WriteKind::RelHub] {
                let w = render_write(ds, Locality::Uniform, kind, 0, 7, 10_000, 1);
                let _ = kind;
                let want = node_label(ds);
                assert!(
                    w.contains(&format!(":{want}")),
                    "{ds:?} writes against a label this corpus does not have —                      it must address `:{want}`, got: {w}"
                );
            }
        }
    }

    #[test]
    fn a_graphalytics_relationship_write_creates_the_edge_the_probe_looks_for() {
        // The rel-integrity probe queries `:STRESSED` for every non-synthetic
        // dataset. A family whose writes create a different type — or none —
        // makes that probe report a finding about nothing.
        let w = render_write(
            Dataset::Graphalytics,
            Locality::Uniform,
            WriteKind::RelSpread,
            0,
            7,
            10_000,
            1,
        );
        assert!(
            w.contains(":Vertex"),
            "must bind the corpus's own nodes: {w}"
        );
        assert!(
            w.contains(":STRESSED"),
            "must create the type the integrity probe checks: {w}"
        );
    }
}
