//! The materialised plan — the workload as data, so equivalence is by
//! construction rather than by two implementations agreeing.
//!
//! # The argument
//!
//! A seeded operation sequence written twice will drift, and a drift is
//! indistinguishable from an engine difference once it is a number in a table.
//! `stress.rs` draws its ops from SplitMix64, a weighted shape pick and a
//! squared-uniform key skew; reimplementing those three in Python to drive
//! LadybugDB would put the workload's definition in two places and make "did
//! the engines differ, or did the generators?" unanswerable after the fact.
//!
//! So the generator runs ONCE, here, and writes the ordered operation list per
//! client with the parameters already bound. Every backend replays it
//! verbatim; the statement TEXT is per-dialect from the catalogue, the
//! SEQUENCE and the PARAMETERS are not per-anything.
//!
//! # The format is `engram-stress-plan` v1, and it is not this file's to change
//!
//! JSONL: line 1 a header, every later line one client's ordered op stream.
//! The format was fixed by the LadybugDB executor
//! (`docs/bench/ladybug-conc.py`), which is built and smoke-tested against it,
//! and this emitter writes it rather than a second one. JSONL rather than one
//! document so a 32-client plan streams instead of being parsed whole, and so
//! a truncated plan is detectable.
//!
//! Ops are engine-NEUTRAL — a name and its bound parameters, no query string —
//! because the four engines share no query language. An op MAY additionally
//! carry `stmt.<engine>` for verbatim replay; this emitter does not write one,
//! and the reason is in [`LEVEL_STRIDE`]: a pre-rendered statement cannot be
//! level-scoped, so a plan carrying one has to be emitted per level.
//!
//! # What that costs, said before it is discovered
//!
//! **Size.** A JSONL op is 60–90 bytes for the shapes a cross-engine run uses.
//! The highest aggregate rate this project has recorded is 3,302 ops/s
//! (`unique-create` at one client, SF1 paged); an in-memory synthetic corpus
//! runs faster. At a deliberately pessimistic 50,000 ops/s, 20 s × 32 clients
//! is 1,000,000 ops ≈ **60–90 MB**, and one plan covers every level of a sweep
//! because the levels are separated by [`LEVEL_STRIDE`] rather than by being
//! re-emitted. The `snb-platform` shapes are the exception —
//! `plat-notin-pick` binds a fifty-id list, ~350 bytes per op — and they are a
//! two-engine Bolt comparison anyway, never replayed from a plan.
//!
//! **Boundedness.** The workload is TIME-boxed, not op-boxed: a client loops
//! until the level's clock runs out, so how many ops it needs is a property of
//! how fast the engine is. A materialised plan has a length. Three ways out:
//!
//! - *Wrap to the start.* Silently changes what is measured — a wrapped ring
//!   re-touches keys it has already touched, so the fastest engine gets the
//!   best cache locality precisely because it is fastest. Rejected. The same
//!   refusal applies to reusing client streams to cover a higher client level.
//! - *Stop the client.* Silently shortens the level on the fastest engine.
//!   Rejected for the same reason, in the other direction.
//! - *Fail loudly.* [`PlanExhausted`] marks the level and the reporter refuses
//!   to quote it. Adopted: the harness says "emit a longer plan" rather than
//!   quietly measuring something else.
//!
//! # Seed-plus-spec, and why the plan carries both
//!
//! A materialised plan is bounded; a pinned generator specification is not.
//! The header carries [`GENERATOR_SPEC`] and the file carries a SHA-256 that
//! travels into every result row, so a replayer that regenerates from the seed
//! can be held to the same bytes: same seed, same spec, same hash, or the run
//! fails. The materialised stream is the witness, not the whole contract.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use crate::workload::{ChurnPlan, ChurnSet, ClientOps, LevelSpec, Op, Param, Profile, WriteKind};

/// The format tag every plan header carries.
pub const PLAN_FORMAT: &str = "engram-stress-plan";

/// The format version this emitter writes and this replayer implements.
pub const PLAN_VERSION: i64 = 1;

/// How far apart two client levels' generated identities are placed.
///
/// # Why this exists at all
///
/// A sweep replays ONE plan at every client level against ONE persistent
/// database, so a write id bound in the plan is issued again at the next
/// level. On an engine with a primary key the second issue is a duplicate. The
/// LadybugDB executor's first version did exactly that: by K=16 every bound id
/// already existed, every write was rejected, `write_ops` fell to 0 — and the
/// level reported 5,019 "ops/s" with an integrity check that PASSED, truthfully,
/// reconciling zero acked writes against zero new rows. The workload had
/// evaporated and every instrument still read green.
///
/// So every executor applies `scoped = bound + level_index * LEVEL_STRIDE`
/// before issuing a write, identically, to the `id` parameter of a node or
/// churn write and to `u` of `unique_create` — and to NOTHING else.
/// `rel_spread` endpoints name pre-existing corpus nodes and must not move;
/// `hot_update` has no identity at all.
///
/// # What it changes about the Bolt arm, said loudly
///
/// `stress.rs` separated levels with a per-level NONCE (`(nonce << 32) | seq`
/// for `unique_create`, a `nonce` property for churn) and did not separate
/// them at all for `node_create`, because engram and Neo4j accept a duplicate
/// `Message.id`. Under a plan, the nonce is fixed for the whole sweep and the
/// stride does the separating. The consequence, stated rather than discovered:
///
/// - `synthetic` `node_create` is UNAFFECTED — its Cypher writes `{c, s}` and
///   never interpolates `id`, so the scoped value is carried in the plan for
///   the engines that need a primary key and ignored on Bolt.
/// - `snb` `node_create`, `unique_create` and the churn ops DO write different
///   VALUES than a `stress.rs` run would. The phenomenon each profile measures
///   is unchanged — all clients still race one contested value space per
///   level, each worker still owns a disjoint churn population — but the
///   numbers are a NEW baseline, not a continuation of the recorded ones.
///   `stress.rs` stays in the tree to reproduce the old baseline.
pub const LEVEL_STRIDE: u64 = 1 << 48;

/// The generator specification, pinned as text and carried in every header.
///
/// Prose, because its only job is to let a human or a second implementation
/// check that a regenerated stream SHOULD match before the hash tells them
/// whether it does.
pub const GENERATOR_SPEC: &str = "splitmix64(state += 0x9E3779B97F4A7C15; z ^= z>>30; z *= \
     0xBF58476D1CE4E5B9; z ^= z>>27; z *= 0x94D049BB133111EB; z ^= z>>31); \
     client seed = run_seed ^ ((cid+1) * 0x9E3779B9); read/write interleave = exact-fraction \
     accumulator (wacc += write_pct; write iff wacc >= 100, then wacc -= 100) with NO rng draw; \
     write seq starts at cid<<40 and increments per write; on a READ the rng is drawn twice — \
     once for the weighted shape pick (below(sum of weights), then subtract weights in table \
     order) and once for the key (uniform: below(space); zipfian: u = (next()>>11)/2^53, \
     key = ((u*u)*space) as u64 % max(space,1); hot: 0). The rng is NOT advanced on a write. \
     Churn is pre-resolved under an all-acked assumption: even write sequences create, odd \
     ones delete the oldest live id once the population reaches 16.";

// ─── Sizing ─────────────────────────────────────────────────────────────────

/// The per-client rate a plan is sized against when nobody names one.
///
/// # Why this number, and why it is a CEILING rather than an observation
///
/// This module's boundedness argument already contains the arithmetic: a plan
/// must hold at least `rate x seconds` ops per client or the fastest arm runs
/// out mid-level. Nothing connected that sentence to `--ops`, so the emitter's
/// default was a bare 200,000 and the operator's was whatever they typed. The
/// four-engine dry run on 2026-09-09 replayed 4,000 ops into 20-second levels:
/// every level drained inside three seconds, every level was correctly refused,
/// and the sweep produced a complete set of NOT-QUOTABLE rows. The guard worked
/// perfectly and the ten hours were still gone.
///
/// 3,000 ops/s PER CLIENT is a sizing CEILING, not a measurement, and the
/// three numbers it has to sit above are all written down:
///
/// - The pessimistic aggregate this module's header already assumes — 50,000
///   ops/s over 32 clients — is **1,562/s** per client.
/// - The 2026-09-09 dry run's observed order (4,000 ops drained in under 3 s)
///   is **~1,333/s**.
/// - A live one-client `balanced` level against this engine over loopback,
///   taken while building this guard, ran at **2,421 ops/s** — and the level
///   that drained beside it measured **2,403 ops/s**. A first draft of this
///   constant at 2,000 was therefore BELOW the only rate anyone had measured,
///   which is exactly the mistake it exists to prevent.
///
/// The per-client rate FALLS as clients are added (2,421/s at K=1, 1,673/s at
/// K=2 on the same box), so the binding case is the lowest client level in the
/// sweep, not the highest — a ceiling has to clear the one-client number.
///
/// Being wrong LOW used to waste a sweep; since the run aborts at the first
/// drained level it wastes one level and names the `--ops` that would have
/// worked. Being wrong high costs disk. The default is set where it is because
/// the cheap direction is now cheap, not because the number is precise.
///
/// It is a default, not a rule: `--rate` overrides it, and an engine that
/// genuinely serves 10,000 ops/s to one client should be sized with `--rate
/// 10000` rather than by hoping. The run-time guard exists precisely because
/// this constant can be wrong — see
/// [`crate::report::LevelResult::sufficient_plan_ops`], which recomputes the
/// requirement from the rate the level ACTUALLY reached.
pub const ASSUMED_PEAK_OPS_PER_CLIENT_SEC: u64 = 3_000;

/// Headroom over `rate x seconds`, as a percentage.
///
/// A plan sized to exactly the expected rate exhausts the moment the engine
/// beats the expectation by one operation, and "the fastest arm ran out" is the
/// arm a comparison is about. 25% is cheap in bytes and buys the whole class of
/// near-misses.
pub const PLAN_SIZING_HEADROOM_PCT: u64 = 25;

/// How many ops per client a plan must hold to cover a level of `seconds` at
/// `peak_ops_per_client_sec`, with [`PLAN_SIZING_HEADROOM_PCT`] on top.
///
/// Integer arithmetic on purpose: this number is printed in a refusal and
/// typed back in as `--ops`, so it must not depend on how a float rounded.
#[must_use]
pub fn required_ops_per_client(seconds: u64, peak_ops_per_client_sec: u64) -> usize {
    let raw = u128::from(seconds.max(1)) * u128::from(peak_ops_per_client_sec.max(1));
    let with_headroom = raw * u128::from(100 + PLAN_SIZING_HEADROOM_PCT) / 100;
    usize::try_from(with_headroom.max(1)).unwrap_or(usize::MAX)
}

/// Why a plan of `ops_per_client` is too small for a `seconds`-long level, if
/// it is — with the `--ops` value that would be sufficient named in the text.
///
/// `None` means the plan is big enough. The caller decides whether an
/// insufficient plan is a refusal or a warning; this function only does the
/// arithmetic, in ONE place, so the emitter and the replayer cannot disagree
/// about what "big enough" means.
#[must_use]
pub fn undersized_because(
    ops_per_client: usize,
    seconds: u64,
    peak_ops_per_client_sec: u64,
) -> Option<String> {
    let need = required_ops_per_client(seconds, peak_ops_per_client_sec);
    if ops_per_client >= need {
        return None;
    }
    Some(format!(
        "a {seconds}s level at {peak_ops_per_client_sec} ops/s per client needs at least \
         {need} ops per client ({seconds} x {peak_ops_per_client_sec} plus \
         {PLAN_SIZING_HEADROOM_PCT}% headroom) and this plan holds {ops_per_client} — every \
         client would drain its stream after about {:.1}s of the {seconds}s window, and every \
         level of the sweep would be refused as NOT QUOTABLE. Re-emit with `--ops {need}`, or \
         say what the real per-client rate is with `--rate`",
        ops_per_client as f64 / peak_ops_per_client_sec.max(1) as f64,
    ))
}

// ─── SHA-256 ────────────────────────────────────────────────────────────────

/// SHA-256 of a byte string, lower-case hex.
///
/// Written out rather than taken from a crate because the workspace's
/// dependency rule is a real constraint and this is ninety lines of arithmetic
/// with published test vectors. The hash's job is provenance: it travels into
/// every result row, so "both engines ran the same plan" is checkable rather
/// than asserted.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bitlen = (bytes.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    let mut w = [0u32; 64];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = String::with_capacity(64);
    for v in h {
        let _ = write!(out, "{v:08x}");
    }
    out
}

// ─── The neutral op vocabulary ──────────────────────────────────────────────

/// The neutral name a write kind carries in a plan.
///
/// One per `WriteKind`, matching the vocabulary the LadybugDB executor
/// implements. The DATASET is not in the name — it is in the header — so the
/// same op name renders as `:StressW` or `:Message:Comment` depending on the
/// corpus, and a plan stays neutral.
#[must_use]
pub fn write_op_name(kind: WriteKind, locality: crate::workload::Locality) -> &'static str {
    use crate::workload::Locality;
    match kind {
        WriteKind::Node if locality == Locality::Hot => "hot_update",
        WriteKind::Node => "node_create",
        WriteKind::RelSpread => "rel_spread",
        WriteKind::RelHub => "rel_hub",
        WriteKind::UniqueCreate => "unique_create",
        WriteKind::NodeOnly => "node_only",
        WriteKind::NodeOnlyFreshProps => "node_only_fresh_props",
        WriteKind::NodeOnlyNoLabels => "node_only_no_labels",
        WriteKind::DeleteChurn => "churn",
    }
}

/// Whether a write op's identity parameter is level-scoped, and which one.
///
/// A closed list, deliberately: the failure this guards is a parameter that
/// SHOULD have moved and did not, and a rule that scoped "anything called id"
/// would also move `rel_spread`'s endpoints, which name pre-existing corpus
/// nodes.
#[must_use]
pub fn scoped_field(op: &str) -> Option<&'static str> {
    match op {
        "node_create"
        | "node_only"
        | "node_only_fresh_props"
        | "node_only_no_labels"
        | "churn_create"
        | "churn_delete" => Some("id"),
        "unique_create" => Some("u"),
        _ => None,
    }
}

/// Apply the level offset to the identity parameter, and only that.
///
/// Returns a NEW map: scoping the plan's own map in place would make the
/// second level scope an already-scoped value, which is the same class of bug
/// as not scoping at all and harder to see.
#[must_use]
pub fn scope_params(
    op: &str,
    params: &BTreeMap<String, Param>,
    level_index: usize,
) -> BTreeMap<String, Param> {
    let offset = level_index as u64 * LEVEL_STRIDE;
    if offset == 0 {
        return params.clone();
    }
    let Some(field) = scoped_field(op) else {
        return params.clone();
    };
    let mut out = params.clone();
    if let Some(Param::Uint(v)) = params.get(field) {
        out.insert(field.to_string(), Param::Uint(v.wrapping_add(offset)));
    }
    out
}

// ─── Ops as the plan carries them ───────────────────────────────────────────

/// One operation with its parameters already bound — what a replayer sees.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanOp {
    /// A read of a named catalogue shape.
    Read {
        /// Position in this client's stream, as `i` in the JSONL.
        i: usize,
        /// The `stress.read_shapes` key.
        shape: String,
        /// Bound parameters.
        params: BTreeMap<String, Param>,
    },
    /// A write of a named neutral op.
    Write {
        /// Position in this client's stream.
        i: usize,
        /// The neutral write-op name.
        op: String,
        /// Bound parameters.
        params: BTreeMap<String, Param>,
    },
}

impl PlanOp {
    /// Whether this op is filed as a write.
    #[must_use]
    pub fn is_write(&self) -> bool {
        matches!(self, PlanOp::Write { .. })
    }

    /// The read shape this is filed under, or `None` for a write.
    #[must_use]
    pub fn shape(&self) -> Option<&str> {
        match self {
            PlanOp::Read { shape, .. } => Some(shape),
            PlanOp::Write { .. } => None,
        }
    }

    /// The neutral write-op name, or `None` for a read.
    #[must_use]
    pub fn op(&self) -> Option<&str> {
        match self {
            PlanOp::Write { op, .. } => Some(op),
            PlanOp::Read { .. } => None,
        }
    }

    /// Its bound parameters.
    #[must_use]
    pub fn params(&self) -> &BTreeMap<String, Param> {
        match self {
            PlanOp::Read { params, .. } | PlanOp::Write { params, .. } => params,
        }
    }

    /// Render as one JSON object — the plan's on-disk form for an op.
    #[must_use]
    pub fn to_json(&self) -> String {
        let (i, kind, key, name, params) = match self {
            PlanOp::Read { i, shape, params } => (*i, "read", "shape", shape, params),
            PlanOp::Write { i, op, params } => (*i, "write", "op", op, params),
        };
        let mut s = String::with_capacity(96);
        let _ = write!(
            s,
            "{{\"i\":{i},\"kind\":\"{kind}\",\"{key}\":{}",
            json_str(name)
        );
        s.push_str(",\"params\":{");
        for (n, (k, v)) in params.iter().enumerate() {
            if n > 0 {
                s.push(',');
            }
            let _ = write!(s, "{}:{}", json_str(k), v.to_json());
        }
        s.push_str("}}");
        s
    }

    /// Parse one op object out of a plan line's `ops` array.
    ///
    /// # Errors
    /// Any malformed op, named. A plan that half-decodes is a workload nobody
    /// chose.
    pub fn from_value(v: &engram_cypher::Value) -> Result<PlanOp, String> {
        use engram_cypher::Value;
        let Value::Map(m) = v else {
            return Err(format!("plan op is not an object: {v:?}"));
        };
        let i = match m.get("i") {
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            other => {
                return Err(format!(
                    "plan op `i` must be a non-negative integer: {other:?}"
                ));
            }
        };
        let mut params = BTreeMap::new();
        if let Some(Value::Map(p)) = m.get("params") {
            for (k, pv) in p {
                params.insert(k.clone(), Param::from_value(pv)?);
            }
        }
        match m.get("kind") {
            Some(Value::Str(k)) if k == "read" => match m.get("shape") {
                Some(Value::Str(shape)) => Ok(PlanOp::Read {
                    i,
                    shape: shape.clone(),
                    params,
                }),
                other => Err(format!("a read op needs a `shape`: {other:?}")),
            },
            Some(Value::Str(k)) if k == "write" => match m.get("op") {
                Some(Value::Str(op)) => Ok(PlanOp::Write {
                    i,
                    op: op.clone(),
                    params,
                }),
                other => Err(format!("a write op needs an `op`: {other:?}")),
            },
            other => Err(format!("plan op `kind` must be read or write: {other:?}")),
        }
    }
}

impl Param {
    /// The JSON scalar this parameter is written as. Integers stay integers:
    /// the level-scoping arithmetic is done on them by every executor, and a
    /// quoted number would have to be parsed back by each one.
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            Param::Uint(n) => n.to_string(),
            Param::Text(s) => json_str(s),
        }
    }

    /// Read a parameter back.
    ///
    /// # Errors
    /// A value that is neither a non-negative integer nor a string.
    pub fn from_value(v: &engram_cypher::Value) -> Result<Param, String> {
        use engram_cypher::Value;
        match v {
            Value::Int(n) if *n >= 0 => Ok(Param::Uint(*n as u64)),
            Value::Str(s) => Ok(Param::Text(s.clone())),
            other => Err(format!(
                "plan parameter {other:?} is not an integer or a string"
            )),
        }
    }
}

// ─── Emitting ───────────────────────────────────────────────────────────────

/// One client's stream and the header facts a replayer needs.
#[derive(Clone, Debug)]
pub struct LoadedPlan {
    /// The profile this plan was emitted for.
    pub profile: String,
    /// The corpus family.
    pub dataset: String,
    /// The key space.
    pub keys: u64,
    /// The run seed.
    pub seed: u64,
    /// The sweep-wide nonce.
    pub nonce: u64,
    /// How many client streams the file holds.
    pub clients: usize,
    /// Ops per client stream.
    pub ops_per_client: usize,
    /// Who wrote it — `engram-bench/harness` for a measurement plan,
    /// `ladybug-conc.py` for the executor's own reference plan. A comparison
    /// built on a self-generated plan must be VISIBLE as one.
    pub emitter: String,
    /// SHA-256 of the file's bytes.
    pub sha256: String,
    /// The streams, indexed by client id.
    pub streams: Vec<Vec<PlanOp>>,
}

/// Emit a plan for one profile.
///
/// `clients` must be at least the highest client level the sweep will run:
/// wrapping streams to cover a higher level is refused, not silently reused,
/// because two clients replaying one stream is not the workload the plan
/// describes.
///
/// # Errors
/// Any I/O failure, named with the path.
pub fn emit_plan(
    path: &Path,
    spec: LevelSpec,
    prof: &Profile,
    clients: usize,
    ops_per_client: usize,
) -> std::io::Result<LoadedPlan> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut buf: Vec<u8> = Vec::with_capacity(ops_per_client * clients * 80);
    let mut header = String::with_capacity(512);
    let _ = write!(
        header,
        "{{\"format\":\"{PLAN_FORMAT}\",\"version\":{PLAN_VERSION},\
         \"emitter\":\"engram-bench/harness\",\"seed\":{},\"profile\":{},\
         \"dataset\":{},\"keys\":{},\"clients\":{clients},\"nonce\":{},\
         \"ops_per_client\":{ops_per_client},\"write_pct\":{},\"write_op\":{},\
         \"level_stride\":{LEVEL_STRIDE},\"generator_spec\":{}}}",
        spec.seed,
        json_str(prof.name),
        json_str(spec.dataset.name()),
        spec.keys,
        spec.nonce,
        prof.write_pct,
        json_str(write_op_name(prof.write_kind, prof.write_locality)),
        json_str(GENERATOR_SPEC),
    );
    buf.extend_from_slice(header.as_bytes());
    buf.push(b'\n');

    let mut streams = Vec::with_capacity(clients);
    for cid in 0..clients {
        let ops = client_stream(spec, prof, cid, ops_per_client);
        let mut line = String::with_capacity(ops_per_client * 80);
        let _ = write!(line, "{{\"client\":{cid},\"ops\":[");
        for (n, op) in ops.iter().enumerate() {
            if n > 0 {
                line.push(',');
            }
            line.push_str(&op.to_json());
        }
        line.push_str("]}");
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        streams.push(ops);
    }
    let sha = sha256_hex(&buf);
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(&buf)?;
    f.flush()?;
    Ok(LoadedPlan {
        profile: prof.name.to_string(),
        dataset: spec.dataset.name().to_string(),
        keys: spec.keys,
        seed: spec.seed,
        nonce: spec.nonce,
        clients,
        ops_per_client,
        emitter: "engram-bench/harness".to_string(),
        sha256: sha,
        streams,
    })
}

/// Generate one client's ops, churn pre-resolved.
///
/// # Churn, and the one place this is not `stress.rs`
///
/// `stress.rs` decides a churn op from the worker's LIVE ledger, which only
/// grows on a server ACK — so a refused create makes a later op a create
/// instead of a delete. That is feedback-dependent and cannot be materialised.
/// The plan format resolves churn at emit time under an all-acked assumption,
/// which is what makes the churn sequence identical on four engines.
///
/// The difference is exactly zero when no create is refused, which is the
/// normal case on Bolt and the case the golden test covers. When creates ARE
/// refused, a later delete names a node that was never created and removes
/// nothing — and the reconciliation reports that as loss, which is STRICTER
/// than `stress.rs`'s ledger, not weaker. Recorded here because "stricter" is
/// still "different", and a churn level that fails on a refusing engine should
/// be read as this, not as corruption.
#[must_use]
pub fn client_stream(
    spec: LevelSpec,
    prof: &Profile,
    cid: usize,
    ops_per_client: usize,
) -> Vec<PlanOp> {
    let mut ops = ClientOps::new(spec, prof, cid);
    let mut churn = ChurnSet::default();
    let mut out = Vec::with_capacity(ops_per_client);
    for i in 0..ops_per_client {
        match ops.next_op() {
            Op::Read { shape, params } => out.push(PlanOp::Read {
                i,
                shape: shape.to_string(),
                params,
            }),
            Op::Write { op, params } => out.push(PlanOp::Write {
                i,
                op: op.to_string(),
                params,
            }),
            Op::ChurnStep { seq } => {
                let plan = churn.plan(seq);
                churn.ack(plan);
                let (op, mut params) = crate::workload::bind_churn(plan, cid, spec.nonce);
                // The anchor key the LadybugDB executor binds its churn edge
                // to. Bolt matches the anchor by (cid, nonce) and ignores it.
                if matches!(plan, ChurnPlan::Create { .. }) {
                    params.insert(
                        "anchor".to_string(),
                        Param::Uint((spec.nonce << 8) | cid as u64),
                    );
                }
                out.push(PlanOp::Write {
                    i,
                    op: op.to_string(),
                    params,
                });
            }
        }
    }
    out
}

/// Read a plan back.
///
/// # Errors
/// A missing file, a wrong format tag, an unknown version, a malformed op, or
/// a client index that does not match its position. Every one refuses rather
/// than being repaired: a plan the replayer had to guess about is not the plan
/// the other engine ran.
pub fn load_plan(path: &Path) -> Result<LoadedPlan, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sha = sha256_hex(&bytes);
    let text = String::from_utf8(bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines();
    let head = lines
        .next()
        .ok_or_else(|| format!("{} is empty", path.display()))?;
    let hv = engram_cypher::json::from_json(head)
        .map_err(|e| format!("{}: header is not JSON: {e}", path.display()))?;
    let engram_cypher::Value::Map(h) = hv else {
        return Err(format!("{}: header is not an object", path.display()));
    };
    let s = |k: &str| -> Result<String, String> {
        match h.get(k) {
            Some(engram_cypher::Value::Str(v)) => Ok(v.clone()),
            other => Err(format!("{}: header.{k} = {other:?}", path.display())),
        }
    };
    let n = |k: &str| -> Result<i64, String> {
        match h.get(k) {
            Some(engram_cypher::Value::Int(v)) => Ok(*v),
            other => Err(format!("{}: header.{k} = {other:?}", path.display())),
        }
    };
    if s("format")? != PLAN_FORMAT {
        return Err(format!(
            "{} is not a {PLAN_FORMAT} file (format={:?})",
            path.display(),
            s("format")
        ));
    }
    if n("version")? != PLAN_VERSION {
        return Err(format!(
            "{}: plan version {} is not {PLAN_VERSION}, which is what this harness implements",
            path.display(),
            n("version")?
        ));
    }
    let clients = usize::try_from(n("clients")?).map_err(|e| format!("header.clients: {e}"))?;
    let ops_per_client =
        usize::try_from(n("ops_per_client")?).map_err(|e| format!("header.ops_per_client: {e}"))?;
    let mut streams: Vec<Vec<PlanOp>> = Vec::with_capacity(clients);
    for (row, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v = engram_cypher::json::from_json(line)
            .map_err(|e| format!("{}: stream line {}: {e}", path.display(), row + 2))?;
        let engram_cypher::Value::Map(m) = v else {
            return Err(format!(
                "{}: stream line {} is not an object",
                path.display(),
                row + 2
            ));
        };
        match m.get("client") {
            Some(engram_cypher::Value::Int(c)) if *c == row as i64 => {}
            other => {
                return Err(format!(
                    "{}: stream line {} says client {other:?}, expected {row} — a plan whose \
                     streams are out of order would replay one client's ops as another's",
                    path.display(),
                    row + 2
                ));
            }
        }
        let Some(engram_cypher::Value::List(ops)) = m.get("ops") else {
            return Err(format!(
                "{}: client {row} has no `ops` array",
                path.display()
            ));
        };
        streams.push(
            ops.iter()
                .map(PlanOp::from_value)
                .collect::<Result<Vec<PlanOp>, String>>()
                .map_err(|e| format!("{}: client {row}: {e}", path.display()))?,
        );
    }
    if streams.len() != clients {
        return Err(format!(
            "{}: header says {clients} client stream(s), the file holds {}",
            path.display(),
            streams.len()
        ));
    }
    Ok(LoadedPlan {
        profile: s("profile")?,
        dataset: s("dataset")?,
        keys: u64::try_from(n("keys")?).map_err(|e| format!("header.keys: {e}"))?,
        seed: u64::try_from(n("seed")?).map_err(|e| format!("header.seed: {e}"))?,
        nonce: u64::try_from(n("nonce")?).map_err(|e| format!("header.nonce: {e}"))?,
        clients,
        ops_per_client,
        emitter: s("emitter").unwrap_or_else(|_| "<unstamped>".to_string()),
        sha256: sha,
        streams,
    })
}

impl LoadedPlan {
    /// The stream for one client at one level, level-scoped.
    ///
    /// # Errors
    /// A client index the plan does not carry. Wrapping to cover a higher
    /// level is REFUSED: two clients replaying one stream is not the workload
    /// the plan describes, and the resulting number would be a measurement of
    /// a workload nobody chose.
    pub fn stream_for(&self, cid: usize, level_index: usize) -> Result<Vec<PlanOp>, String> {
        let Some(ops) = self.streams.get(cid) else {
            return Err(format!(
                "the plan holds {} client stream(s) and this level needs client {cid} — \
                 re-emit with --plan-clients >= the highest level, rather than wrapping \
                 (two clients replaying one stream is not this workload)",
                self.streams.len()
            ));
        };
        Ok(ops
            .iter()
            .map(|op| match op {
                PlanOp::Read { .. } => op.clone(),
                PlanOp::Write {
                    i,
                    op: name,
                    params,
                } => PlanOp::Write {
                    i: *i,
                    op: name.clone(),
                    params: scope_params(name, params, level_index),
                },
            })
            .collect())
    }
}

/// The knobs that must match for two engines' numbers to be comparable.
///
/// Carried in every result document, and [`crate::report::compare`] refuses to
/// build a comparison row out of two runs whose fairness blocks differ. The
/// rule exists because it was violated: LadybugDB left at its default ran 16
/// threads against a 6-core quota, which is a different machine from the one
/// engram runs on, and the resulting q4 number was not a comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fairness {
    /// Intra-query parallelism cap — engram's `--workers`, LadybugDB's
    /// `--threads`, Postgres's `max_parallel_workers_per_gather`.
    pub thread_cap: u32,
    /// Serving cache budget in MiB — engram's `--paged-cache-mb`, Postgres's
    /// `shared_buffers`, Neo4j's page cache.
    pub cache_budget_mb: u32,
    /// Concurrent clients in the level.
    pub clients: usize,
    /// Seconds the level runs.
    pub seconds: u64,
}

impl Fairness {
    /// Render as a JSON object.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"thread_cap\": {}, \"cache_budget_mb\": {}, \"clients\": {}, \"seconds\": {}}}",
            self.thread_cap, self.cache_budget_mb, self.clients, self.seconds
        )
    }
}

/// A client's plan ran out before its level's clock did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanExhausted {
    /// Which client.
    pub cid: usize,
    /// How many ops it had.
    pub had: usize,
}

/// The op stream for one client, from whichever source.
///
/// This is the ONE place the two op sources meet: a backend sees only
/// `next()`, so live generation and plan replay differ in nothing else.
pub enum OpSource {
    /// Generated live from the seed. Unbounded.
    ///
    /// Level-scoped exactly as a replay is. That is not an optimisation: the
    /// per-level integrity probes bound their search to the level's id range,
    /// so a live source that did not scope would write ids the probes cannot
    /// see and every churn level above the first would report total loss. It
    /// did, on the first run of this harness — the reconciliation caught its
    /// own harness, which is the job.
    Live {
        /// The generator.
        source: Box<ClientOps>,
        /// Per-worker churn ledger, for the profiles that need one.
        churn: Box<ChurnSet>,
        /// Which client.
        cid: usize,
        /// The level nonce.
        nonce: u64,
        /// The 0-based position of this level in the sweep.
        level_index: usize,
        /// How many ops have been handed out, for the `i` field.
        at: usize,
    },
    /// Replayed from a materialised plan. Bounded, and exhaustion is loud.
    Replay {
        /// The decoded, level-scoped ops.
        ops: Vec<PlanOp>,
        /// How many have been handed out.
        at: usize,
        /// Which client.
        cid: usize,
    },
}

impl OpSource {
    /// The next op.
    ///
    /// # Errors
    /// [`PlanExhausted`] when a replayed plan runs out.
    pub fn next_op(&mut self) -> Result<PlanOp, PlanExhausted> {
        match self {
            OpSource::Live {
                source,
                churn,
                cid,
                nonce,
                level_index,
                at,
            } => {
                let i = *at;
                *at += 1;
                let lvl = *level_index;
                let write = |op: &str, params: &BTreeMap<String, Param>| PlanOp::Write {
                    i,
                    op: op.to_string(),
                    // The SAME arithmetic a replay applies, in the same place
                    // in the pipeline. A live source that skipped it would
                    // write ids this level's integrity probes cannot see.
                    params: scope_params(op, params, lvl),
                };
                Ok(match source.next_op() {
                    Op::Read { shape, params } => PlanOp::Read {
                        i,
                        shape: shape.to_string(),
                        params,
                    },
                    Op::Write { op, params } => write(op, &params),
                    Op::ChurnStep { seq } => {
                        // The live path keeps `stress.rs`'s ack-dependent
                        // ledger; the plan path pre-resolves. They agree
                        // whenever no create is refused — see `client_stream`.
                        let plan = churn.plan(seq);
                        churn.ack(plan);
                        let (op, params) = crate::workload::bind_churn(plan, *cid, *nonce);
                        write(op, &params)
                    }
                })
            }
            OpSource::Replay { ops, at, cid } => {
                if *at >= ops.len() {
                    return Err(PlanExhausted {
                        cid: *cid,
                        had: ops.len(),
                    });
                }
                let op = ops[*at].clone();
                *at += 1;
                Ok(op)
            }
        }
    }
}

/// Escape a string as a JSON scalar. Local because the plan is written and
/// read by Python before any engine is involved.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::{Dataset, profile};

    fn spec() -> LevelSpec {
        LevelSpec {
            seed: 424_242,
            dataset: Dataset::Synthetic,
            keys: 20_000,
            nonce: 1,
        }
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        // A hash written out by hand is only worth having if it is checked
        // against somebody else's arithmetic.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Past the one-block boundary, where the padding rule matters.
        assert_eq!(
            sha256_hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn an_op_round_trips_through_its_json() {
        let mut params = BTreeMap::new();
        params.insert("key".to_string(), Param::Uint(8123));
        let r = PlanOp::Read {
            i: 0,
            shape: "point-lookup".to_string(),
            params,
        };
        assert_eq!(
            r.to_json(),
            "{\"i\":0,\"kind\":\"read\",\"shape\":\"point-lookup\",\"params\":{\"key\":8123}}"
        );
        let back = engram_cypher::json::from_json(&r.to_json()).expect("json");
        assert_eq!(PlanOp::from_value(&back), Ok(r));

        // A textual binding — the platform id list, the value that would break
        // a whitespace-separated format.
        let mut params = BTreeMap::new();
        params.insert("idlist".to_string(), Param::Text("[1, 14, 27]".to_string()));
        let l = PlanOp::Read {
            i: 3,
            shape: "plat-in-list-seek".to_string(),
            params,
        };
        let back = engram_cypher::json::from_json(&l.to_json()).expect("json");
        assert_eq!(PlanOp::from_value(&back), Ok(l));
    }

    #[test]
    fn level_scoping_moves_the_identity_and_nothing_else() {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), Param::Uint(7));
        params.insert("cid".to_string(), Param::Uint(0));
        params.insert("seq".to_string(), Param::Uint(7));
        let scoped = scope_params("node_create", &params, 2);
        assert_eq!(scoped.get("id"), Some(&Param::Uint(7 + 2 * LEVEL_STRIDE)));
        assert_eq!(
            scoped.get("cid"),
            Some(&Param::Uint(0)),
            "cid must not move"
        );
        assert_eq!(
            scoped.get("seq"),
            Some(&Param::Uint(7)),
            "seq must not move"
        );
        // Level 0 is the identity — and it must be, or the first level would
        // already differ from an unscoped run.
        assert_eq!(scope_params("node_create", &params, 0), params);
        // `rel_spread` endpoints name pre-existing corpus nodes.
        let mut ends = BTreeMap::new();
        ends.insert("a".to_string(), Param::Uint(5));
        ends.insert("b".to_string(), Param::Uint(9));
        assert_eq!(scope_params("rel_spread", &ends, 3), ends);
        // `hot_update` has no identity at all.
        assert_eq!(
            scope_params("hot_update", &BTreeMap::new(), 3),
            BTreeMap::new()
        );
        // `unique_create` scopes `u`.
        let mut u = BTreeMap::new();
        u.insert("u".to_string(), Param::Uint(11));
        assert_eq!(
            scope_params("unique_create", &u, 1).get("u"),
            Some(&Param::Uint(11 + LEVEL_STRIDE))
        );
    }

    #[test]
    fn scoping_is_applied_to_the_plan_and_never_to_an_already_scoped_value() {
        // The bug this shape guards: scoping in place makes level 2 scope a
        // level-1 value. Two calls at the same level must be equal, and a
        // call at level 2 must be twice the stride from the original — not
        // three times.
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), Param::Uint(1));
        let one = scope_params("node_create", &params, 1);
        let one_again = scope_params("node_create", &params, 1);
        assert_eq!(one, one_again);
        let two = scope_params("node_create", &params, 2);
        assert_eq!(two.get("id"), Some(&Param::Uint(1 + 2 * LEVEL_STRIDE)));
    }

    #[test]
    fn the_same_seed_emits_a_byte_identical_plan() {
        let prof = profile("balanced").expect("balanced");
        let dir = std::env::temp_dir().join(format!("engram-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = emit_plan(&dir.join("a.jsonl"), spec(), prof, 4, 200).expect("emit");
        let b = emit_plan(&dir.join("b.jsonl"), spec(), prof, 4, 200).expect("emit");
        assert_eq!(a.sha256, b.sha256, "same seed, byte-identical plan");
        assert_eq!(
            std::fs::read(dir.join("a.jsonl")).unwrap(),
            std::fs::read(dir.join("b.jsonl")).unwrap()
        );
        // A different seed is a different plan — a hash that did not move
        // would be a hash of nothing.
        let mut other = spec();
        other.seed += 1;
        let c = emit_plan(&dir.join("c.jsonl"), other, prof, 4, 200).expect("emit");
        assert_ne!(a.sha256, c.sha256);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_plan_reads_back_as_the_stream_that_was_written() {
        let prof = profile("delete-churn").expect("delete-churn");
        let dir = std::env::temp_dir().join(format!("engram-plan-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("p.jsonl");
        let emitted = emit_plan(&path, spec(), prof, 3, 60).expect("emit");
        let loaded = load_plan(&path).expect("load");
        assert_eq!(loaded.sha256, emitted.sha256);
        assert_eq!(loaded.emitter, "engram-bench/harness");
        assert_eq!(loaded.streams, emitted.streams);
        // Level 0 replays the plan unchanged; a later level moves the ids.
        let l0 = loaded.stream_for(0, 0).expect("stream");
        assert_eq!(l0, emitted.streams[0]);
        let l2 = loaded.stream_for(0, 2).expect("stream");
        for (a, b) in l0.iter().zip(l2.iter()) {
            match (a.params().get("id"), b.params().get("id")) {
                (Some(Param::Uint(x)), Some(Param::Uint(y))) => {
                    assert_eq!(*y, x + 2 * LEVEL_STRIDE)
                }
                (None, None) => {}
                other => panic!("id binding diverged: {other:?}"),
            }
        }
        // A client the plan does not carry REFUSES rather than wrapping.
        let err = loaded.stream_for(3, 0).expect_err("must refuse");
        assert!(err.contains("wrapping"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_with_the_wrong_format_or_version_refuses() {
        let dir = std::env::temp_dir().join(format!("engram-plan-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let p = dir.join("bad.jsonl");
        std::fs::write(&p, "{\"format\":\"something-else\",\"version\":1}\n").expect("write");
        assert!(
            load_plan(&p)
                .unwrap_err()
                .contains("is not a engram-stress-plan")
        );
        std::fs::write(
            &p,
            "{\"format\":\"engram-stress-plan\",\"version\":99,\"clients\":0,\
             \"ops_per_client\":0,\"profile\":\"x\",\"dataset\":\"synthetic\",\
             \"keys\":1,\"seed\":1,\"nonce\":1}\n",
        )
        .expect("write");
        assert!(load_plan(&p).unwrap_err().contains("plan version 99"));
        // Streams out of order would replay one client's ops as another's.
        std::fs::write(
            &p,
            "{\"format\":\"engram-stress-plan\",\"version\":1,\"clients\":2,\
             \"ops_per_client\":0,\"profile\":\"x\",\"dataset\":\"synthetic\",\
             \"keys\":1,\"seed\":1,\"nonce\":1}\n{\"client\":1,\"ops\":[]}\n\
             {\"client\":0,\"ops\":[]}\n",
        )
        .expect("write");
        assert!(load_plan(&p).unwrap_err().contains("expected 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_exhausted_replay_says_so_rather_than_wrapping() {
        let mut src = OpSource::Replay {
            ops: vec![PlanOp::Read {
                i: 0,
                shape: "top-k".into(),
                params: BTreeMap::new(),
            }],
            at: 0,
            cid: 7,
        };
        assert!(src.next_op().is_ok());
        assert_eq!(src.next_op(), Err(PlanExhausted { cid: 7, had: 1 }));
        // Exhaustion is stable, not a one-shot warning the next call forgets.
        assert_eq!(src.next_op(), Err(PlanExhausted { cid: 7, had: 1 }));
    }

    #[test]
    fn a_plan_too_small_for_its_level_is_named_as_such_with_the_sufficient_ops() {
        // THE DEFECT, as arithmetic. The dry run emitted 4,000 ops per client
        // and replayed them into 20-second levels; every level drained in
        // under three seconds and every level was refused. Ten hours of
        // correctly-refused rows.
        let need = required_ops_per_client(20, ASSUMED_PEAK_OPS_PER_CLIENT_SEC);
        let why = undersized_because(4_000, 20, ASSUMED_PEAK_OPS_PER_CLIENT_SEC)
            .expect("4,000 ops cannot cover a 20-second level");
        assert!(
            why.contains(&format!("--ops {need}")),
            "the refusal must name the cure: {why}"
        );
        assert!(why.contains("4000"), "and what it had: {why}");
        // The task's own floor: >= 30,000 ops per client for a 20 s level on
        // the observed order. The default sizing must clear it, not scrape it
        // — and it must also clear the 2,421 ops/s actually measured at one
        // client, which the first draft of the constant did not.
        assert!(
            need >= 30_000,
            "20 s needs >= 30,000 ops/client, got {need}"
        );
        assert!(
            need >= (2_421 * 20),
            "the default must cover the fastest rate anyone has measured, got {need}"
        );
        // The worked example, pinned: the arithmetic is `seconds x rate` plus
        // 25%, and nothing else.
        assert_eq!(required_ops_per_client(20, 2_000), 50_000);
        // A plan that IS big enough is not nagged about, and one op short is.
        assert_eq!(
            undersized_because(need, 20, ASSUMED_PEAK_OPS_PER_CLIENT_SEC),
            None
        );
        assert!(undersized_because(need - 1, 20, ASSUMED_PEAK_OPS_PER_CLIENT_SEC).is_some());
        // The rate is a parameter, not a belief: an engine that really does
        // serve 10,000 ops/s to one client is sized for it.
        assert_eq!(required_ops_per_client(20, 10_000), 250_000);
        // Degenerate inputs do not produce a zero-op requirement, which would
        // make every plan "big enough".
        assert!(required_ops_per_client(0, 0) >= 1);
    }

    #[test]
    fn sizing_did_not_move_the_emitted_bytes() {
        // The sizing rule is arithmetic ABOUT a plan, never part of one. The
        // format is fixed by the LadybugDB executor, which is built against
        // it, so a sizing field in the header would be a format change wearing
        // a guard's clothes.
        let prof = profile("balanced").expect("balanced");
        let dir = std::env::temp_dir().join(format!("engram-plan-fmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("p.jsonl");
        let p = emit_plan(&path, spec(), prof, 2, 50).expect("emit");
        let head = std::fs::read_to_string(&path).expect("read");
        let head = head.lines().next().expect("header").to_string();
        assert!(
            !head.contains("seconds"),
            "no sizing field in the header: {head}"
        );
        assert!(
            !head.contains("rate"),
            "no sizing field in the header: {head}"
        );
        assert_eq!(p.ops_per_client, 50);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_live_source_and_the_plan_agree_op_for_op() {
        // The whole convergence claim, as an assertion: generating and
        // replaying are the same workload.
        let prof = profile("balanced").expect("balanced");
        let plan = client_stream(spec(), prof, 2, 300);
        let mut live = OpSource::Live {
            source: Box::new(ClientOps::new(spec(), prof, 2)),
            churn: Box::new(ChurnSet::default()),
            cid: 2,
            nonce: spec().nonce,
            level_index: 0,
            at: 0,
        };
        for want in &plan {
            assert_eq!(&live.next_op().expect("live is unbounded"), want);
        }
    }
}
