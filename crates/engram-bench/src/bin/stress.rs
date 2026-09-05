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
}

impl Dataset {
    fn parse(s: &str) -> Option<Dataset> {
        match s {
            "synthetic" => Some(Dataset::Synthetic),
            "snb" | "ldbc-snb" => Some(Dataset::Snb),
            _ => None,
        }
    }
    fn shapes(self) -> &'static [Shape] {
        match self {
            Dataset::Synthetic => SYNTHETIC_SHAPES,
            Dataset::Snb => SNB_SHAPES,
        }
    }
    /// Indexes the read shapes require. Creating these is not tuning the
    /// benchmark: an unindexed lookup key turns every "point lookup" into a
    /// full label scan, so the fixture would BE the measurement — the same
    /// trap the synthetic lane already documents.
    fn indexes(self) -> &'static [&'static str] {
        match self {
            Dataset::Synthetic => {
                &["CREATE INDEX stress_k IF NOT EXISTS FOR (n:Stress) ON (n.k)"]
            }
            Dataset::Snb => &[
                "CREATE INDEX snb_person_id IF NOT EXISTS FOR (n:Person) ON (n.id)",
                "CREATE INDEX snb_message_id IF NOT EXISTS FOR (n:Message) ON (n.id)",
            ],
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
        }
    }
}

fn render_read(ds: Dataset, shape: &Shape, key: u64, space: u64) -> String {
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
            return match ds {
                Dataset::Synthetic => format!(
                    "MATCH (a:Stress {{k: {a}}}), (b:Stress {{k: {b}}}) CREATE (a)-[:SLINK]->(b)"
                ),
                Dataset::Snb => format!(
                    "MATCH (a:Person {{id: {a}}}), (b:Person {{id: {b}}}) \
                     CREATE (a)-[:STRESSED]->(b)"
                ),
            };
        }
    }
    match (ds, locality) {
        (Dataset::Synthetic, Locality::Hot) => {
            "MATCH (n:Stress {k: 0}) SET n.hits = coalesce(n.hits, 0) + 1".to_string()
        }
        (Dataset::Synthetic, _) => format!("CREATE (:StressW {{c: {cid}, s: {seq}}})"),
        (Dataset::Snb, Locality::Hot) => {
            "MATCH (p:Person {id: 0}) SET p.hits = coalesce(p.hits, 0) + 1".to_string()
        }
        (Dataset::Snb, _) => {
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
    },
    Profile {
        name: "read-heavy",
        write_pct: 5,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high read, low write — the common production shape",
        diagnostic: false,
    },
    Profile {
        name: "balanced",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high read, high write — both paths contending",
        diagnostic: false,
    },
    Profile {
        name: "write-heavy",
        write_pct: 95,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "high write, low read — ingest under query load",
        diagnostic: false,
    },
    Profile {
        name: "write-only",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::Node,
        what: "raw insert throughput",
        diagnostic: false,
    },
    Profile {
        name: "contention",
        write_pct: 50,
        write_locality: Locality::Hot,
        write_kind: WriteKind::Node,
        what: "write-WRITE conflict on one hot node — not insert throughput",
        diagnostic: false,
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
    },
    Profile {
        name: "balanced-freshprops",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::NodeOnlyFreshProps,
        what: "50/50 node-only write on property names no read seeks — isolates the property-epoch collision",
        diagnostic: true,
    },
    Profile {
        name: "balanced-nolabels",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::NodeOnlyNoLabels,
        what: "50/50 node-only write with no labels — isolates named-label membership churn",
        diagnostic: true,
    },
    Profile {
        name: "balanced-disjoint",
        write_pct: 50,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelSpread,
        what: "50/50 with writes on a relationship type no read traverses — the interference control",
        diagnostic: true,
    },
    Profile {
        name: "rel-create",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelSpread,
        what: "distinct-endpoint relationship inserts — the guard-row overhead, spread",
        diagnostic: false,
    },
    Profile {
        name: "rel-hub",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::RelHub,
        what: "every relationship lands on ONE endpoint — guard serialisation, measured",
        diagnostic: false,
    },
    Profile {
        name: "unique-create",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::UniqueCreate,
        what: "every client races the same UNIQUE values — one winner each, zero duplicates",
        diagnostic: false,
    },
    // LAST, for the same corpus-trajectory reason as the rel profiles — and
    // doubly so: churn is the one profile that REMOVES data, so anything
    // running after it would see a corpus no earlier sweep ever saw.
    Profile {
        name: "delete-churn",
        write_pct: 100,
        write_locality: Locality::Uniform,
        write_kind: WriteKind::DeleteChurn,
        what: "create-then-delete churn per worker — DETACH DELETE and rel cleanup under load",
        diagnostic: false,
    },
];

// ─── Measurement ────────────────────────────────────────────────────────────

/// The hot node's counter value — the ground truth the contention profile's
/// acked writes must reconcile against. A throughput number over lost
/// updates must refuse to print as a PASS (P0.4 of the scale-and-integrity
/// plan): the incumbent's contention figure documents exactly that failure,
/// and this harness must not be able to reproduce it silently.
fn hot_counter(ds: Dataset, c: &mut Client) -> Option<i64> {
    let q = match ds {
        Dataset::Synthetic => "MATCH (n:Stress {k: 0}) RETURN coalesce(n.hits, 0)",
        Dataset::Snb => "MATCH (p:Person {id: 0}) RETURN coalesce(p.hits, 0)",
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
        if median == 0 { 1.0 } else { p10 as f64 / median as f64 }
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
              [--seed N] [--keys N] [--dataset synthetic|snb] [--json PATH]

datasets:
   synthetic   the harness seeds its own world (default) — runs anywhere, incl. CI
   snb         ATTACH to a server already holding an LDBC SNB corpus
               (`portserve <corpus dir> <addr>`); --keys is probed, not seeded

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
                eprintln!("unknown dataset `{s}`; try `synthetic` or `snb`");
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

    match dataset {
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
        Dataset::Snb => {
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
    let total_weight: u32 = read_shapes.iter().map(|s| s.weight).sum();
    let mut all_rows: Vec<(String, LevelResult)> = Vec::new();
    // Integrity failures found by the self-verification reads — merged into
    // the verdict, so a lossy run FAILS regardless of its throughput.
    let mut integrity: Vec<String> = Vec::new();
    // A per-level nonce so contested-value profiles never replay a spent
    // value space across levels or profiles.
    let mut level_counter: u64 = 0;

    for prof in &profiles {
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

            for cid in 0..k {
                let addr = addr.clone();
                let stop = Arc::clone(&stop);
                let ticker = Arc::clone(&ticker);
                let prof = **prof;
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
                            let mut pickw = rng.below(u64::from(total_weight)) as u32;
                            let mut chosen = &read_shapes[0];
                            for sh in read_shapes {
                                if pickw < sh.weight {
                                    chosen = sh;
                                    break;
                                }
                                pickw -= sh.weight;
                            }
                            let key = chosen.locality.pick(&mut rng, keys);
                            shape_name = Some(chosen.name);
                            (render_read(dataset, chosen, key, keys), false)
                        };

                        let t = Instant::now();
                        match cn.run(&stmt) {
                            Ok(_) => {
                                let us = t.elapsed().as_micros() as u64;
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
                let ty = match dataset {
                    Dataset::Synthetic => "SLINK",
                    Dataset::Snb => "STRESSED",
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
                    let probe = format!(
                        "MATCH (n:Churn {{nonce: {level_nonce}, cid: {wid}}}) RETURN n.id"
                    );
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
    let mut bad = integrity;
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
    if bad.is_empty() {
        println!(
            "PASS — {} level(s) across {} profile(s): no transport errors, no throughput collapse",
            all_rows.len(),
            profiles.len()
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
                 \"trend\":{:.4},\"floor\":{:.4},\"quotable\":{quotable_json},\"per_sec\":{:?}}}",
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
