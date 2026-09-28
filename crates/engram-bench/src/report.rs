//! One result schema, and the reporter that turns several of them into a
//! comparison table.
//!
//! # Why one schema
//!
//! `lsqb` writes one JSON shape and `stress` writes another, by hand, with two
//! escaping strategies (`lsqb` goes through the engine's audited value writer;
//! `stress` does `replace('\\', "\\\\")` inline). Every comparison table this
//! project has produced was assembled by a script that knew both. A third
//! engine meant a third shape and a third script, and the script is where a
//! run's caveats go to be forgotten — which is how a `0.00 ops/s` that was a
//! refusal count became a "660×" in a table.
//!
//! So there is one document. Both workloads emit it, every backend emits it,
//! and the qualifications travel INSIDE it: `quotable`,
//! `not_quotable_because`, the per-entry catalogue `status`, the `fairness`
//! block, the catalogue digest, the `rig`. A reporter that builds a row out of
//! two documents whose fairness or catalogue digests differ is comparing two
//! configurations, and [`compare`] refuses rather than printing it.
//!
//! # Two measurement lanes, and why the reporter refuses across them
//!
//! Every LSQB, concurrency and stress number this project has recorded was
//! taken under a 6-CPU cgroup quota on a shared control-plane node. A second
//! lane — a dedicated 48-core machine with no quota — exists alongside it, and
//! BOTH are permanent: the pod lane is kept for continuity with the recorded
//! series, and the bench lane exists because a 6-CPU quota cannot answer what
//! the engine does with a real machine.
//!
//! So the document carries a [`Rig`], required with no default, exactly as
//! `writes_mode` is, and [`compare`] refuses a table whose documents disagree
//! about it. Refusing is the whole point. A footnote would not survive being
//! pasted into a summary, and a number that crossed lanes unnoticed reads as
//! an engine that got several times faster on a day nobody changed it.
//!
//! # Backward compatibility, deliberately
//!
//! `levels[]` carries `stress.rs`'s exact field names and `queries[]` carries
//! `lsqb.rs`'s, additions beside them rather than instead of them, so
//! `measurements/cmp-table.py` and every recorded report still parse. A schema
//! that renamed `ops_per_sec` would make four years of committed JSON
//! unreadable to the tool that reads it, which is a strange way to improve
//! comparability.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_cypher::json::to_json;

use crate::plan::Fairness;

/// The result schema's version.
pub const SCHEMA_VERSION: i64 = 1;

/// Profiles for which a high refusal share is the RESULT, not a fault.
///
/// `unique-create` exists to make every client race one value and see one
/// winner; `contention` exists to make every writer fight over one node. A
/// refusal-ratio rule that refused these would delete the two profiles it was
/// meant to protect.
pub const REFUSAL_IS_THE_MEASUREMENT: [&str; 2] = ["unique-create", "contention"];

// ─── The shape thresholds, in ONE place and mirrored in the Python arm ───────
//
// `docs/bench/ladybug-conc.py` is a SECOND implementation of these rules — the
// LadybugDB arm is embedded, out of process, with no harness binary to ask — so
// every number below is written there too, under the same name, and
// `tests/the_quotability_rule_is_one_rule_in_two_languages.rs` reads the Python
// source at run time and refuses a drift. Changing one alone is how four
// engines stop sharing a rule while every column still prints a number.

/// One-second buckets a `trend` or `floor` needs before it means anything.
///
/// `trend` splits the buckets in half and `floor` takes a 10th percentile over
/// them; with three buckets the halves are 1 and 2 and the "10th percentile" is
/// the minimum. Four is the smallest count at which both statistics describe a
/// sample rather than a coin flip — which is why the fix for a short level is
/// NOT a lower threshold. A level below this is not judged, and
/// [`NotQuotable::TooShortToJudge`] is how it says so instead of passing
/// quietly.
pub const MIN_JUDGED_BUCKETS: usize = 4;

/// `trend` below this is a COLLAPSE: the second half served less than half the
/// first.
///
/// Unchanged from `stress.rs`. It is deliberately loose because it is a claim
/// ABOUT THE ENGINE — a false positive sends a reader hunting a compaction
/// cliff that is not there — and retuning a guard the ledger has already
/// accepted is a separate change needing its own evidence.
pub const TREND_COLLAPSE: f64 = 0.5;

/// `trend` above this is warm-up contamination, and the level is REFUSED.
///
/// # Why the two directions do not get the same number
///
/// The low side is a claim about the engine; this side is a claim about the
/// MEASUREMENT. A level whose second half ran far faster than its first did not
/// measure a steady state at all: its mean is an average of two regimes and
/// UNDER-reports the engine. The arithmetic reciprocal of [`TREND_COLLAPSE`]
/// would be 2.0, and a K=1 level was observed at **1.65** — the second half at
/// 165% of the first, entirely warm-up — and passed. A reciprocal threshold
/// would have passed it too, so symmetry in the ratio is the wrong symmetry.
///
/// 1.5 is chosen because the cost of a false negative here is asymmetric in a
/// way it is not on the low side: warm-up contaminates the FIRST level of a
/// sweep, which is the K=1 baseline every scaling ratio is divided by. One
/// unrefused warm-up level therefore distorts every ratio in the table, not
/// only its own row. The cost of a false positive is one re-run of one level,
/// and the remedy is local and cheap — warm the corpus, or run the level
/// longer.
///
/// What this rule CANNOT tell you: whether the ramp is the page cache filling
/// or the engine genuinely taking that long to reach its own steady state. The
/// second would be a finding. One level cannot separate them, so the refusal
/// classifies as an operator error — "re-run it longer" — and a ramp that
/// SURVIVES a longer level is the finding, reported by the person who saw it
/// twice.
pub const TREND_WARMUP_REFUSE: f64 = 1.5;

/// `trend` above this and at or below [`TREND_WARMUP_REFUSE`] is WARNED about
/// and still quoted.
///
/// Some genuine warm-up is expected on the first level of a run, and refusing
/// it would delete the K=1 row the sweep is built on. The band exists so that
/// expectation is VISIBLE in the output rather than assumed by the reader.
pub const TREND_WARMUP_WARN: f64 = 1.25;

/// `floor` below this is a stall within the level: the 10th-percentile second
/// served less than a quarter of the median second. Unchanged from `stress.rs`.
pub const FLOOR_STALL: f64 = 0.25;

/// Refusals over write attempts above this is refusal-dominated — rule 4.
pub const REFUSAL_SHARE_MAX: f64 = 0.5;

/// Every cause this project can record, and which of the two kinds it is.
///
/// The Rust harness produces the first block; `docs/bench/ladybug-conc.py`
/// produces those AND the LadybugDB-only ones below, because that executor
/// reconciles the corpus and verifies the key space and the Rust arm does not.
/// The table is the shared vocabulary: a consumer filtering a sweep by
/// `not_quotable_class` reads it from here, and the drift test holds the Python
/// to it.
pub const NOT_QUOTABLE_CAUSES: &[(&str, &str)] = &[
    // Produced by both arms.
    ("plan_exhausted", "operator_error"),
    ("no_operations", "finding"),
    ("all_writes_refused", "finding"),
    ("stalled", "finding"),
    ("refusal_dominated", "finding"),
    ("no_concurrency", "finding"),
    ("too_short_to_judge", "operator_error"),
    ("warmup_ramp", "operator_error"),
    // LadybugDB-only: this executor has instruments the Rust arm has not.
    ("errored_run", "finding"),
    ("integrity_failure", "finding"),
    ("key_space_mismatch", "operator_error"),
];

/// The largest number of operations in flight at once, from their spans.
///
/// A sweep over start/end events. Ties are resolved by sorting the raw
/// `(t, delta)` pairs, which puts a `-1` before a `+1` at the same instant and
/// therefore UNDERSTATES rather than overstates overlap — the safe direction
/// for an instrument whose whole job is to refuse a level that did not
/// overlap.
#[must_use]
pub fn max_inflight(spans: &[(u64, u64)]) -> usize {
    let mut ev: Vec<(u64, i64)> = Vec::with_capacity(spans.len() * 2);
    for (s, e) in spans {
        ev.push((*s, 1));
        ev.push((*e, -1));
    }
    ev.sort_unstable();
    let mut cur = 0i64;
    let mut peak = 0i64;
    for (_, d) in ev {
        cur += d;
        peak = peak.max(cur);
    }
    peak.max(0) as usize
}

/// A percentile out of a SORTED sample vector.
///
/// Nearest-rank on the sorted index, as `stress.rs` and `snbconc.rs` both do
/// it. Transcribed rather than improved: a different interpolation rule
/// produces different p99s, and every recorded tail number was taken under
/// this one.
#[must_use]
pub fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// The five numbers a latency population is reported as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tail {
    /// Median, microseconds.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p999: u64,
    /// The slowest sample.
    pub max: u64,
}

impl Tail {
    /// Summarise a sorted sample vector.
    #[must_use]
    pub fn of(sorted: &[u64]) -> Tail {
        Tail {
            p50: pct(sorted, 0.50),
            p95: pct(sorted, 0.95),
            p99: pct(sorted, 0.99),
            p999: pct(sorted, 0.999),
            max: sorted.last().copied().unwrap_or(0),
        }
    }
}

/// One measured level of one profile.
///
/// Every field `stress.rs` recorded, plus the three convergence adds: the
/// separate read and write tails `snbconc` reported and `stress` did not, the
/// per-shape breakdown, and whether a replayed plan ran out.
#[derive(Clone, Debug)]
pub struct LevelResult {
    /// Which profile.
    pub profile: String,
    /// Concurrent clients.
    pub clients: usize,
    /// Wall seconds the level actually ran.
    pub secs: f64,
    /// Acked reads.
    pub r_ops: usize,
    /// Acked writes.
    pub w_ops: usize,
    /// Sorted read latencies, microseconds.
    pub r: Vec<u64>,
    /// Sorted write latencies, microseconds.
    pub w: Vec<u64>,
    /// Transport errors — the server dropped connections.
    pub errors: u64,
    /// Correct refusals under load.
    pub refusals: u64,
    /// Throughput in each second of the run.
    pub per_sec: Vec<u64>,
    /// When the workers were released, unix milliseconds — the same clock the
    /// server stamps its maintenance lines with, so a stalled second can be
    /// aligned with the server's own events.
    pub started_unix_ms: u64,
    /// Latency per read shape, microseconds, unsorted as collected.
    pub per_shape: BTreeMap<String, Vec<u64>>,
    /// Clients whose replayed plan ran out before the clock did.
    pub plan_exhausted: Vec<usize>,
    /// How long each of those clients lasted, microseconds since the level was
    /// released, in the same order as [`LevelResult::plan_exhausted`].
    ///
    /// The bare id list says a plan ran out; it cannot say what plan would NOT
    /// have. With the moment recorded, the level's own achieved rate is
    /// `plan_ops_per_client / this`, and the refusal can name the `--ops` value
    /// that would have covered the window instead of telling the operator to
    /// try a bigger number. See [`LevelResult::sufficient_plan_ops`].
    pub plan_exhausted_us: Vec<u64>,
    /// Ops each client stream held, when the level replayed a plan.
    ///
    /// `None` for a live-generated level, where the source is unbounded and
    /// the question does not arise.
    pub plan_ops_per_client: Option<usize>,
    /// WHICH refusal, not just how many.
    ///
    /// "Every write was refused" is three different findings depending on the
    /// message — a single-writer rule, an optimistic write-write conflict, or
    /// the plan colliding with itself — and a bare count cannot tell them
    /// apart. Not having this histogram is what made the LadybugDB
    /// level-scoping bug take a separate probe to diagnose.
    pub refusal_kinds: BTreeMap<String, u64>,
    /// The largest number of operations in flight at once.
    ///
    /// The instrument that answers "did K clients actually run concurrently,
    /// or did my own executor serialise them?" — a question no throughput
    /// number can answer, because an executor that serialises but is merely
    /// fast produces a perfectly healthy-looking rate.
    pub max_inflight: usize,
    /// `single` or `multi` — whether the engine was allowed more than one
    /// concurrent write transaction.
    ///
    /// Stamped on every row because the same plan produces categorically
    /// different results in the two modes: LadybugDB is single-writer by
    /// default and REFUSES the second writer rather than queueing it. A row
    /// without this stamp is uninterpretable.
    pub writes_mode: String,
}

/// Why a level's throughput may not be quoted — the CAUSE, apart from the prose.
///
/// # Two kinds of refusal, and why conflating them costs a re-run
///
/// `not_quotable_because` returned a sentence, and a sentence is a fine thing
/// for a person to read and a bad thing for a sweep to be triaged by. The two
/// kinds are not the same news:
///
/// - **A finding.** Throughput collapsed, every write was refused, the server
///   stalled, K clients never overlapped. The level did its job — it caught
///   something — and the result IS the finding.
/// - **An operator error.** The plan was emitted too small for the level it was
///   replayed at. Nothing was learned about any engine. The remedy is to
///   re-emit and run it again.
///
/// Both printed `NOT QUOTABLE` and both landed in `failures`, so a sweep that
/// was entirely one had to be read line by line to tell it from a sweep that
/// was entirely the other. The 2026-09-09 dry run was the first kind of nothing
/// and looked exactly like the second kind of something.
///
/// [`NotQuotable::code`] is the machine-readable name and
/// [`NotQuotable::is_operator_error`] is the split; both travel in the document.
#[derive(Clone, Debug, PartialEq)]
pub enum NotQuotable {
    /// A replayed plan ran out before the level's clock did.
    PlanExhausted {
        /// How many client streams ran dry.
        clients: usize,
        /// Ops each stream held.
        had: Option<usize>,
        /// The `--ops` that would have covered the window, from the measured
        /// rate.
        sufficient: Option<usize>,
        /// When the first stream ran dry, seconds into the level.
        drained_after_s: Option<f64>,
        /// The window the level claims to have measured.
        seconds: f64,
    },
    /// The level acked nothing at all.
    NoOperations {
        /// Correct refusals.
        refusals: u64,
        /// Transport errors.
        errors: u64,
    },
    /// Reads landed, every write was refused.
    AllWritesRefused {
        /// How many.
        refusals: u64,
    },
    /// One operation ate at least half the window.
    Stalled {
        /// The longest operation, microseconds.
        max_us: u64,
        /// The window.
        seconds: f64,
    },
    /// More than half the write attempts were refused, and a refusal is cheaper
    /// than a write — so the rate is inflated by the failures.
    RefusalDominated {
        /// Refused.
        refusals: u64,
        /// Attempted.
        attempts: u64,
        /// The rendered refusal histogram, or empty.
        kinds: String,
    },
    /// K clients that never overlapped measured no concurrency.
    NoConcurrency {
        /// How many were asked for.
        clients: usize,
    },
    /// The level was too short for the shape guards to run at all.
    ///
    /// `trend` and `floor` both need [`MIN_JUDGED_BUCKETS`] one-second buckets.
    /// Below that they return 1.0 — the value of a PERFECTLY STEADY level — and
    /// the two checks that read them are skipped. The level then prints a clean
    /// row, and that clean row is the absence of a check rather than the result
    /// of one. This variant is that silence, said out loud.
    TooShortToJudge {
        /// One-second buckets the level actually produced.
        buckets: usize,
        /// The window it claims to have measured.
        seconds: f64,
    },
    /// The second half ran far faster than the first: the level measured a
    /// warm-up, not a steady state.
    ///
    /// The counterpart of the DEGRADED check, which distrusts only the other
    /// direction. See [`TREND_WARMUP_REFUSE`] for why the two thresholds are
    /// not reciprocals of each other.
    WarmUpRamp {
        /// Second-half mean over first-half mean.
        trend: f64,
        /// Buckets the trend was taken over.
        buckets: usize,
    },
}

impl NotQuotable {
    /// The stable machine-readable name, carried in the document.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            NotQuotable::PlanExhausted { .. } => "plan_exhausted",
            NotQuotable::NoOperations { .. } => "no_operations",
            NotQuotable::AllWritesRefused { .. } => "all_writes_refused",
            NotQuotable::Stalled { .. } => "stalled",
            NotQuotable::RefusalDominated { .. } => "refusal_dominated",
            NotQuotable::NoConcurrency { .. } => "no_concurrency",
            NotQuotable::TooShortToJudge { .. } => "too_short_to_judge",
            NotQuotable::WarmUpRamp { .. } => "warmup_ramp",
        }
    }

    /// Whether the refusal is the operator having made a mistake rather than
    /// the engine having been caught doing something.
    ///
    /// The method exists rather than a `== "plan_exhausted"` at three call
    /// sites so that adding another one is a single edit where the taxonomy is
    /// defined — which has now happened twice.
    ///
    /// `TooShortToJudge` and `WarmUpRamp` join `PlanExhausted` here for the
    /// same reason it is there: nothing in such a level is a statement about
    /// the engine, and the remedy is a re-run the operator can make. A window
    /// too short to judge was chosen at the command line; a warm-up ramp is
    /// cured by warming the corpus or lengthening the level. A ramp that
    /// SURVIVES a longer level IS a finding — but one level cannot say that,
    /// and guessing in the direction of "finding" is what sends a reader
    /// hunting an engine defect that is really a cold page cache.
    #[must_use]
    pub fn is_operator_error(&self) -> bool {
        matches!(
            self,
            NotQuotable::PlanExhausted { .. }
                | NotQuotable::TooShortToJudge { .. }
                | NotQuotable::WarmUpRamp { .. }
        )
    }

    /// Which of the two kinds this is, as the word the document carries.
    #[must_use]
    pub fn class(&self) -> &'static str {
        if self.is_operator_error() {
            "operator_error"
        } else {
            "finding"
        }
    }

    /// The sentence a person reads.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            NotQuotable::PlanExhausted {
                clients,
                had,
                sufficient,
                drained_after_s,
                seconds,
            } => {
                let held = had.map_or_else(String::new, |n| format!(" of {n} op(s) each"));
                let when = drained_after_s.map_or_else(String::new, |t| {
                    format!(" after {t:.1}s of the {seconds:.1}s window")
                });
                let need = sufficient.unwrap_or_else(|| {
                    crate::plan::required_ops_per_client(
                        seconds.ceil().max(1.0) as u64,
                        crate::plan::ASSUMED_PEAK_OPS_PER_CLIENT_SEC,
                    )
                });
                let basis = if sufficient.is_some() {
                    "this level's own achieved rate, plus headroom"
                } else {
                    "the emitter's sizing default, because this level ran out too fast to \
                     measure a rate"
                };
                format!(
                    "{clients} client(s) exhausted their emitted plan{held} before the \
                     level's clock ran out{when}, so this level measured a shorter window \
                     than it reports. OPERATOR ERROR, not a finding: nothing here is a \
                     statement about the engine. Re-emit with `--ops {need}` ({basis}) and \
                     run it again"
                )
            }
            NotQuotable::NoOperations { refusals, errors } => format!(
                "the level completed no operations at all ({refusals} refusal(s), \
                 {errors} error(s))"
            ),
            NotQuotable::AllWritesRefused { refusals } => format!(
                "every write was refused ({refusals} refusal(s), 0 acked): this measures \
                 refusal handling, not throughput — check that the corpus was \
                 reset for this run"
            ),
            NotQuotable::Stalled { max_us, seconds } => format!(
                "one operation took {:.1} s of a {:.1} s window: the server \
                 stalled, so the mean is not a rate",
                *max_us as f64 / 1e6,
                seconds
            ),
            NotQuotable::RefusalDominated {
                refusals,
                attempts,
                kinds,
            } => format!(
                "{refusals} of {attempts} write attempts ({:.0}%) were REFUSED, so this rate is \
                 mostly refusal handling — and a refusal is cheaper than a write, which \
                 INFLATES the number. Quote it as a refusal rate and say so{kinds}",
                *refusals as f64 / (*attempts).max(1) as f64 * 100.0,
            ),
            NotQuotable::NoConcurrency { clients } => format!(
                "{clients} clients but never more than one operation in flight: this level did \
                 not measure concurrency, whatever its rate says"
            ),
            NotQuotable::TooShortToJudge { buckets, seconds } => format!(
                "a {seconds:.1} s level produced {buckets} one-second bucket(s), and the \
                 DEGRADED and STALLED checks both need at least {MIN_JUDGED_BUCKETS}: those \
                 two guards DID NOT RUN on this level, so a clean row here is the ABSENCE \
                 of a check and not the result of one. OPERATOR ERROR, not a finding: \
                 re-run with at least {MIN_JUDGED_BUCKETS} seconds per level \
                 (--allow-short-levels runs a smoke probe anyway, and every level it \
                 produces carries this refusal)"
            ),
            NotQuotable::WarmUpRamp { trend, buckets } => format!(
                "the second half of this level ran at {:.0}% of the first, over {buckets} \
                 one-second bucket(s): the first half was WARM-UP, not steady state, so the \
                 mean averages two regimes and UNDER-reports the engine. The DEGRADED check \
                 distrusts only the collapsing direction and passes this. OPERATOR ERROR, \
                 not a finding: warm the corpus or lengthen the level and run it again — a \
                 ramp that survives a longer level IS a finding, but one level cannot tell \
                 a cold cache from a slow-starting engine",
                trend * 100.0
            ),
        }
    }
}

impl LevelResult {
    /// Operations per second, reads and writes together.
    #[must_use]
    pub fn rps(&self) -> f64 {
        (self.r_ops + self.w_ops) as f64 / self.secs
    }

    /// Sustained degradation: the second half's mean throughput over the first
    /// half's. Near 1.0 is steady; well below means the server got slower as
    /// the run went on — a compaction cliff, unbounded memory, a lock convoy.
    /// A trend over halves is immune to one noisy second.
    #[must_use]
    pub fn trend(&self) -> f64 {
        if self.per_sec.len() < 4 {
            return 1.0;
        }
        let half = self.per_sec.len() / 2;
        let mean = |s: &[u64]| -> f64 { s.iter().sum::<u64>() as f64 / s.len().max(1) as f64 };
        let first = mean(&self.per_sec[..half]);
        let second = mean(&self.per_sec[half..]);
        if first == 0.0 { 1.0 } else { second / first }
    }

    /// Stall floor: the 10th-percentile second over the median second.
    ///
    /// Catches "it stopped serving for a while" without being destroyed by a
    /// single slow second, which min-over-max could not distinguish — on a
    /// shared host that scored ordinary jitter at 0.24 and flagged the
    /// workstation, not the server.
    #[must_use]
    pub fn floor(&self) -> f64 {
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

    /// Whether this level produced enough one-second buckets for [`trend`] and
    /// [`floor`] to be statistics rather than noise.
    ///
    /// The two guards that read them are written `if r.judged() && …` so that
    /// the length rule lives in ONE place. It used to be an inline
    /// `per_sec.len() > 3` at four call sites, and the consequence was not that
    /// a short level was mis-judged — it was that a short level was NOT judged
    /// and printed a clean row anyway.
    ///
    /// [`trend`]: LevelResult::trend
    /// [`floor`]: LevelResult::floor
    #[must_use]
    pub fn judged(&self) -> bool {
        self.per_sec.len() >= MIN_JUDGED_BUCKETS
    }

    /// A warm-up that is worth SAYING but not worth refusing.
    ///
    /// The band between [`TREND_WARMUP_WARN`] and [`TREND_WARMUP_REFUSE`]. Some
    /// genuine warm-up is expected on the first level of a run and refusing it
    /// would delete the K=1 row every scaling ratio is divided by; but a reader
    /// who is told nothing assumes nothing happened, which is the same failure
    /// this whole file exists to close, one notch quieter.
    #[must_use]
    pub fn warm_up_warning(&self) -> Option<String> {
        if !self.judged() {
            return None;
        }
        let t = self.trend();
        if t > TREND_WARMUP_WARN && t <= TREND_WARMUP_REFUSE {
            return Some(format!(
                "the second half ran at {:.0}% of the first — some warm-up, below the \
                 {:.0}% at which the level is REFUSED. Quotable, but the first level of \
                 a run is where a cold cache shows up, and this is the row every \
                 scaling ratio is divided by",
                t * 100.0,
                TREND_WARMUP_REFUSE * 100.0
            ));
        }
        None
    }

    /// Why this level's throughput may NOT be quoted, if it may not.
    ///
    /// The verdict travels WITH the number rather than replacing it: the data
    /// stays visible as evidence and the claim it can support is labelled.
    /// Silence would hide a real defect; an unlabelled number launders one
    /// into a benchmark win. The first three shapes below each reached a
    /// comparison table looking like a measurement, and are `stress.rs`'s
    /// rules unchanged.
    ///
    /// **Rule 4 is the LadybugDB executor's, adopted here for every engine.**
    /// `stress.rs` refuses a level where EVERY write was refused
    /// (`w_ops == 0 && refusals > 0`), which does not catch a level that acks
    /// a FEW writes and refuses the rest — measured on LadybugDB at K=8, 242
    /// acked against 2,158 refused, so `w_ops` is not zero and rule 2 never
    /// fires. Worse, a refusal is CHEAPER than a write, so the 90%-refused
    /// level posts a HIGHER ops/s than the level that did the work and the
    /// engine appears to scale beautifully. That is the single most dangerous
    /// number this convergence could produce.
    ///
    /// `unique-create` and `contention` are exempt, because for them refusal
    /// IS the measured phenomenon: every client racing one value is supposed
    /// to produce one winner and many clean refusals, and refusing to quote
    /// that would discard the result the profile exists to produce.
    ///
    /// **Rule 5 is the harness auditing itself.** K clients that never
    /// overlapped measured no concurrency, whatever rate they posted — and an
    /// executor that serialises its own clients but is merely fast produces a
    /// perfectly healthy-looking number.
    ///
    /// **Rule 6 is convergence's own**: a level in which a replayed plan ran
    /// out measured a shorter window than it reports, on whichever arm was
    /// fastest — which is precisely the arm a comparison is about.
    #[must_use]
    pub fn not_quotable_because(&self, max_us: u64) -> Option<String> {
        self.not_quotable(max_us).map(|c| c.explain())
    }

    /// The same verdict as [`LevelResult::not_quotable_because`], as a value.
    ///
    /// ONE rule set, two renderings. The prose is what a person reads; the
    /// [`NotQuotable`] code is what a script filters on, and the two cannot
    /// drift because the prose is produced from the code.
    #[must_use]
    pub fn not_quotable(&self, max_us: u64) -> Option<NotQuotable> {
        if !self.plan_exhausted.is_empty() {
            return Some(NotQuotable::PlanExhausted {
                clients: self.plan_exhausted.len(),
                had: self.plan_ops_per_client,
                sufficient: self.sufficient_plan_ops(),
                drained_after_s: self
                    .plan_exhausted_us
                    .iter()
                    .copied()
                    .min()
                    .map(|us| us as f64 / 1e6),
                seconds: self.secs,
            });
        }
        if self.r_ops + self.w_ops == 0 {
            return Some(NotQuotable::NoOperations {
                refusals: self.refusals,
                errors: self.errors,
            });
        }
        if self.w_ops == 0 && self.refusals > 0 {
            return Some(NotQuotable::AllWritesRefused {
                refusals: self.refusals,
            });
        }
        let window_us = (self.secs * 1_000_000.0) as u64;
        if window_us > 0 && max_us >= window_us / 2 {
            return Some(NotQuotable::Stalled {
                max_us,
                seconds: self.secs,
            });
        }
        let attempts = self.w_ops as u64 + self.refusals;
        if !REFUSAL_IS_THE_MEASUREMENT.contains(&self.profile.as_str()) && attempts > 0 {
            let share = self.refusals as f64 / attempts as f64;
            if share > 0.5 {
                let kinds = if self.refusal_kinds.is_empty() {
                    String::new()
                } else {
                    let named: Vec<String> = self
                        .refusal_kinds
                        .iter()
                        .map(|(k, n)| format!("{k}={n}"))
                        .collect();
                    format!(" (kinds: {})", named.join(", "))
                };
                return Some(NotQuotable::RefusalDominated {
                    refusals: self.refusals,
                    attempts,
                    kinds,
                });
            }
        }
        if self.clients > 1 && self.max_inflight <= 1 {
            return Some(NotQuotable::NoConcurrency {
                clients: self.clients,
            });
        }
        // Rules 7 and 8 are the SHAPE rules, and they are checked LAST on
        // purpose. Every rule above is backed by evidence the level actually
        // produced — no ops, every write refused, one operation eating the
        // window — and a level that has one of those should report it, not
        // report that its buckets were hard to read. These two are what is left
        // when nothing else fired, which is exactly the position the reader is
        // in: a clean-looking row.
        //
        // Rule 7. `trend` and `floor` return 1.0 below MIN_JUDGED_BUCKETS, and
        // 1.0 is the value of a perfectly steady level, so the DEGRADED and
        // STALLED checks are not merely inconclusive on a short level — they
        // are guaranteed to pass it. Lowering the threshold would not fix that;
        // the statistic needs the buckets. Saying so does.
        if !self.judged() {
            return Some(NotQuotable::TooShortToJudge {
                buckets: self.per_sec.len(),
                seconds: self.secs,
            });
        }
        // Rule 8. The other half of the trend guard. DEGRADED fires below
        // TREND_COLLAPSE; nothing looked upward, and a K=1 level at trend 1.65
        // — 165% of the first half, all of it warm-up — passed clean.
        let trend = self.trend();
        if trend > TREND_WARMUP_REFUSE {
            return Some(NotQuotable::WarmUpRamp {
                trend,
                buckets: self.per_sec.len(),
            });
        }
        None
    }

    /// The `--ops` a plan would have needed to cover this level, measured from
    /// the level's own achieved rate.
    ///
    /// `None` when the level did not replay a plan, or ran out at t=0 with no
    /// usable rate — in which case the refusal falls back to the emitter's
    /// sizing default rather than inventing a number.
    ///
    /// The EARLIEST exhaustion is used, not the mean: the fastest client is
    /// the one that sets the requirement, and sizing to the average leaves the
    /// fast one running out again on the re-run.
    #[must_use]
    pub fn sufficient_plan_ops(&self) -> Option<usize> {
        let had = self.plan_ops_per_client?;
        let earliest = self
            .plan_exhausted_us
            .iter()
            .copied()
            .filter(|us| *us > 0)
            .min()?;
        let rate = had as f64 / (earliest as f64 / 1e6);
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        Some(crate::plan::required_ops_per_client(
            self.secs.ceil().max(1.0) as u64,
            rate.ceil() as u64,
        ))
    }

    /// Both latency populations, merged and sorted — what the headline
    /// percentiles are taken over, as they always have been.
    #[must_use]
    pub fn all_latencies(&self) -> Vec<u64> {
        let mut all: Vec<u64> = self.r.iter().chain(self.w.iter()).copied().collect();
        all.sort_unstable();
        all
    }

    fn to_value(&self, engine: &str) -> Value {
        let all = self.all_latencies();
        let tail = Tail::of(&all);
        let cause = self.not_quotable(tail.max);
        let quotable = cause.as_ref().map(NotQuotable::explain);
        let mut m = BTreeMap::new();
        m.insert("profile".into(), Value::Str(self.profile.clone()));
        m.insert("engine".into(), Value::Str(engine.to_string()));
        m.insert("clients".into(), Value::Int(self.clients as i64));
        m.insert("seconds".into(), Value::Float(round3(self.secs)));
        m.insert("read_ops".into(), Value::Int(self.r_ops as i64));
        m.insert("write_ops".into(), Value::Int(self.w_ops as i64));
        m.insert("ops_per_sec".into(), Value::Float(round2(self.rps())));
        m.insert("p50_us".into(), Value::Int(tail.p50 as i64));
        m.insert("p95_us".into(), Value::Int(tail.p95 as i64));
        m.insert("p99_us".into(), Value::Int(tail.p99 as i64));
        m.insert("p999_us".into(), Value::Int(tail.p999 as i64));
        m.insert("max_us".into(), Value::Int(tail.max as i64));
        // The split `snbconc` reported and `stress` did not: a mix whose write
        // tail is the whole story looks identical, in the combined numbers, to
        // one whose read tail is.
        let rt = Tail::of(&self.r);
        let wt = Tail::of(&self.w);
        for (prefix, t) in [("read", rt), ("write", wt)] {
            m.insert(format!("{prefix}_p50_us"), Value::Int(t.p50 as i64));
            m.insert(format!("{prefix}_p95_us"), Value::Int(t.p95 as i64));
            m.insert(format!("{prefix}_p99_us"), Value::Int(t.p99 as i64));
            m.insert(format!("{prefix}_p999_us"), Value::Int(t.p999 as i64));
            m.insert(format!("{prefix}_max_us"), Value::Int(t.max as i64));
        }
        m.insert("errors".into(), Value::Int(self.errors as i64));
        m.insert("refusals".into(), Value::Int(self.refusals as i64));
        // Refusals apart from errors, and WHICH refusal apart from how many.
        m.insert(
            "refusal_kinds".into(),
            Value::Map(
                self.refusal_kinds
                    .iter()
                    .map(|(k, n)| (k.clone(), Value::Int(*n as i64)))
                    .collect(),
            ),
        );
        m.insert("max_inflight".into(), Value::Int(self.max_inflight as i64));
        m.insert("writes_mode".into(), Value::Str(self.writes_mode.clone()));
        m.insert("trend".into(), Value::Float(round4(self.trend())));
        m.insert("floor".into(), Value::Float(round4(self.floor())));
        // Whether the two numbers above were JUDGED. Below MIN_JUDGED_BUCKETS
        // they are both hard-coded 1.0 — the value of a perfectly steady level
        // — so a document that carried them alone told a reader the level was
        // steady when what happened is that nobody looked. The flag rides
        // beside them so the two states are distinguishable in the JSON, not
        // only in the refusal prose.
        m.insert("trend_floor_judged".into(), Value::Bool(self.judged()));
        // A warm-up that is worth saying and not worth refusing. `null` when
        // there is none — never absent, so a consumer can tell "no warning"
        // from "this document predates the check".
        m.insert(
            "trend_warning".into(),
            self.warm_up_warning()
                .map(Value::Str)
                .unwrap_or(Value::Null),
        );
        m.insert("quotable".into(), Value::Bool(quotable.is_none()));
        m.insert(
            "not_quotable_because".into(),
            quotable.map(Value::Str).unwrap_or(Value::Null),
        );
        // The cause apart from the prose, so a sweep is triaged by grep rather
        // than by reading. `not_quotable_cause` is the stable code;
        // `not_quotable_class` splits an OPERATOR ERROR (the plan was emitted
        // too small — nothing was measured) from a FINDING (the engine was
        // caught doing something). Conflating them is what makes a whole sweep
        // of nothing look like a whole sweep of something.
        m.insert(
            "not_quotable_cause".into(),
            cause
                .as_ref()
                .map(|c| Value::Str(c.code().to_string()))
                .unwrap_or(Value::Null),
        );
        m.insert(
            "not_quotable_class".into(),
            cause
                .as_ref()
                .map(|c| Value::Str(c.class().to_string()))
                .unwrap_or(Value::Null),
        );
        m.insert(
            "started_unix_ms".into(),
            Value::Int(self.started_unix_ms as i64),
        );
        m.insert(
            "per_sec".into(),
            Value::List(
                self.per_sec
                    .iter()
                    .map(|n| Value::Int(*n as i64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "plan_exhausted".into(),
            Value::List(
                self.plan_exhausted
                    .iter()
                    .map(|c| Value::Int(*c as i64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        // The evidence the sizing advice is computed FROM, recorded beside the
        // advice so the reader can check the arithmetic rather than trust it.
        m.insert(
            "plan_exhausted_us".into(),
            Value::List(
                self.plan_exhausted_us
                    .iter()
                    .map(|us| Value::Int(*us as i64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "plan_ops_per_client".into(),
            self.plan_ops_per_client
                .map_or(Value::Null, |n| Value::Int(n as i64)),
        );
        m.insert(
            "plan_ops_sufficient".into(),
            self.sufficient_plan_ops()
                .map_or(Value::Null, |n| Value::Int(n as i64)),
        );
        // Per-shape: the column that says WHERE the mix's cost went. A shape
        // at 2% of operations and 80% of elapsed time is the finding, and the
        // aggregate row cannot show it.
        let mut shapes = BTreeMap::new();
        for (name, v) in &self.per_shape {
            let mut v = v.clone();
            v.sort_unstable();
            let t = Tail::of(&v);
            let total: u64 = v.iter().sum();
            let mut s = BTreeMap::new();
            s.insert("ops".to_string(), Value::Int(v.len() as i64));
            s.insert("p50_us".to_string(), Value::Int(t.p50 as i64));
            s.insert("p95_us".to_string(), Value::Int(t.p95 as i64));
            s.insert("max_us".to_string(), Value::Int(t.max as i64));
            s.insert("total_us".to_string(), Value::Int(total as i64));
            shapes.insert(name.clone(), Value::Map(s));
        }
        m.insert("per_shape".into(), Value::Map(shapes));
        Value::Map(m)
    }
}

/// One LSQB query's outcome. The statuses are `lsqb.rs`'s, unchanged, because
/// each names a distinct way a count can be wrong and collapsing any two loses
/// the diagnosis.
#[derive(Clone, Debug)]
pub struct QueryResult {
    /// `q1`..`q9`.
    pub query: String,
    /// The statement as sent, in whatever dialect the backend speaks.
    pub statement: Option<String>,
    /// The measured count.
    pub count: Option<i64>,
    /// Wall milliseconds.
    pub millis: Option<f64>,
    /// `ok`, `timeout`, `error`, `zero_on_populated`, `unverified_zero`,
    /// `mismatch`, `inconsistent`, `unmappable`.
    pub status: String,
    /// What the existence probe established, with its own wall time.
    pub probe: String,
    /// Anything the status alone does not say.
    pub detail: Option<String>,
    /// The expected count, when one was recorded.
    pub expected: Option<i64>,
    /// The catalogue's confidence in this dialect's text.
    pub catalogue_status: String,
}

impl QueryResult {
    fn to_value(&self, engine: &str) -> Value {
        let mut m = BTreeMap::new();
        m.insert("query".into(), Value::Str(self.query.clone()));
        m.insert("engine".into(), Value::Str(engine.to_string()));
        let stmt = self
            .statement
            .clone()
            .map(Value::Str)
            .unwrap_or(Value::Null);
        m.insert("statement".into(), stmt.clone());
        // The old key, kept so every committed report and every script that
        // reads one still parses. Same value, not a second measurement.
        m.insert("adapted_cypher".into(), stmt);
        m.insert(
            "count".into(),
            self.count.map(Value::Int).unwrap_or(Value::Null),
        );
        m.insert(
            "millis".into(),
            self.millis.map(Value::Float).unwrap_or(Value::Null),
        );
        m.insert("status".into(), Value::Str(self.status.clone()));
        m.insert("probe".into(), Value::Str(self.probe.clone()));
        m.insert(
            "detail".into(),
            self.detail.clone().map(Value::Str).unwrap_or(Value::Null),
        );
        m.insert(
            "expected".into(),
            self.expected.map(Value::Int).unwrap_or(Value::Null),
        );
        m.insert(
            "catalogue_status".into(),
            Value::Str(self.catalogue_status.clone()),
        );
        // `lsqb.rs` emits this and its `--expect` parser reads reports back;
        // absent here it would refuse a converged report as malformed.
        m.insert("divergence".into(), Value::Null);
        Value::Map(m)
    }
}

// ─── The rig ────────────────────────────────────────────────────────────────

/// A machine numbers can be taken on, and the quota they were taken under.
///
/// Not a knob — a physical fact about where a run happened. [`Fairness`] says
/// what the harness ASKED FOR (the thread cap, the cache budget); a rig says
/// what the machine was in a position to give. The two are independent, and
/// the pair that will break a table is `thread_cap: 6` on both arms where one
/// arm is a pod capped at 6 CPU on a node running the rest of the platform and
/// the other is a 48-core box with nothing else on it. Those agree on every
/// fairness field. The client threads on the second one never queue behind
/// each other, the page cache is thirty times larger, and nothing else on the
/// machine competes for memory bandwidth. Same fairness, different
/// measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RigMachine {
    /// The lane's stable name — the value `--rig` takes.
    pub name: &'static str,
    /// The machine type, e.g. `ccx43`. Recorded rather than inferred from the
    /// core count, because two machine types can share a core count and not a
    /// memory bandwidth.
    pub node_type: &'static str,
    /// Cores the MACHINE has, whatever the process was allowed to use.
    pub node_cores: u32,
    /// The CPU quota in whole cores, or `None` when no quota applied and the
    /// process had the machine.
    ///
    /// `None` means "no quota was applied", never "nobody wrote it down". The
    /// unknown case is deliberately not representable: a rig that cannot say
    /// whether it was throttled describes nothing.
    pub cpu_quota_cores: Option<u32>,
    /// The memory ceiling in MiB — the cgroup limit where there is one, the
    /// machine's RAM where there is not. Either way it is the number a run
    /// could not exceed, which is what an OOM-shaped result has to be read
    /// against.
    pub mem_limit_mb: u32,
    /// What this lane IS, in a sentence. Carried into the CLI's listing and
    /// into a refusal's text, so a reader does not have to look the name up in
    /// this file to know what was blended.
    pub what: &'static str,
}

/// The measurement lanes this project runs, and will keep running.
///
/// **There are two, they are not comparable in either direction, and both are
/// permanent.** The pod lane is kept "for consistency" — every LSQB,
/// concurrency and stress number in `measurements/` was taken there, and a
/// series is worth more than the machine it runs on. The bench lane exists
/// because a 6-CPU quota cannot answer what the engine does with a real
/// machine. Neither replaces the other, so both stamps must exist for as long
/// as both lanes do.
///
/// The failure this table exists to prevent is not exotic. A number from the
/// bench lane gets pasted into a table built from the pod lane, and the engine
/// appears to have got several times faster on a day nobody changed it. This
/// project's ledger already carries results retracted for MILDER versions of
/// that — comparisons taken in different windows on the same rig. A cross-rig
/// blend would be worse, and unlike a window it leaves no trace in the numbers
/// themselves.
///
/// Adding an entry here is a REVIEWED edit, on purpose. A lane that will be
/// measured more than once belongs in this table, where its facts are written
/// down once; the inline `--rig` form exists for a machine that is not part of
/// this estate, and spells out every field precisely so nothing is defaulted.
pub const KNOWN_RIGS: [RigMachine; 3] = [
    // The lane every recorded number came from. The node is a 16-core machine
    // that ALSO runs the control plane and every CPU app workload, so the
    // 6-core quota is a ceiling on contended cores and not an allocation of
    // quiet ones. The numbers are read off the pod specs rather than
    // remembered: `measurements/pod/portbench-pod.yaml`,
    // the Neo4j and Kuzu benchmark pods' manifests
    // all set `cpu: 2 request / 6 limit` and `memory: 40Gi limit`, and the
    // Neo4j one says in its own comment that it is IDENTICAL to the bench pod
    // on purpose, because a comparison is 1:1 or it is not one. (Their memory
    // REQUESTS differ — 16Gi against Kuzu's 2Gi — which does not move the
    // ceiling a run could reach, so it is not part of the rig.)
    RigMachine {
        name: "main-pod-6cpu",
        node_type: "ccx43",
        node_cores: 16,
        cpu_quota_cores: Some(6),
        mem_limit_mb: 40 * 1024,
        what: "a pod on the shared control-plane node, capped at 6 CPU / 40 GiB, \
               contending with the platform",
    },
    // The lane that does not exist yet. The shape is settled — 48 dedicated
    // cores, 192 GB, created for a sweep and destroyed after, with a
    // persistent corpus volume so a large scale factor loads once and is
    // measured many times; all four engines run on it one at a time, each
    // getting the whole machine, so a cross-engine table at one scale is
    // comparable.
    //
    // NO NUMBER HAS BEEN TAKEN ON THIS RIG. The entry is here so the first one
    // is STAMPED rather than retro-fitted — a rig written down after the fact
    // is a reconstruction, and a reconstruction is what this whole mechanism
    // exists to refuse. `cpu_quota_cores: None` is the point of the lane: no
    // cgroup, one engine, the whole box.
    // The figures are what the POD gets, not what the box has. A CCX63 is 48
    // cores and 192 GiB, but the kubelet reserves for itself and the system
    // daemons, and the full-node pod manifests leave four cores and ~23.5 GiB
    // of deliberate insulation on top of that. Recording 48/192 here would
    // describe a machine no measurement ever ran on, and the whole point of
    // this registry is that a rig row is checkable against the thing that
    // produced the number.
    RigMachine {
        name: "bench-ccx63",
        node_type: "ccx63",
        node_cores: 48,
        cpu_quota_cores: Some(44),
        mem_limit_mb: 160 * 1024,
        what: "a dedicated bench node, one engine at a time, whole machine less kubelet reserve and insulation",
    },
    // The lane the WORKING pods land in once the everyday rig moves off the
    // cluster's control-plane node. Added 2026-09-09 with the bench-node
    // migration.
    //
    // THIS ENTRY EXISTS BECAUSE THE MOVE ENDS A SERIES, AND SOMETHING HAD TO
    // SAY SO. The four benchmark pods (engram, Neo4j, PostgreSQL, Kuzu) keep
    // the same ceiling they always had — cpu 6, 40 GiB — but
    // they now sit on a quiet, dedicated 48-core machine instead of a
    // contended 16-core control plane. A run there stamped `main-pod-6cpu`
    // would be caught by `RigCheck`, which reads
    // /sys/devices/system/cpu/online, observes 48 against the declared 16, and
    // returns `Mismatch` — the check doing exactly its job. But a refusal with
    // no legal alternative is a harness nobody can use, so the alternative is
    // written down here.
    //
    // IT IS A THIRD LANE, NOT A CONTINUATION OF THE FIRST. The quota is the
    // same number and the machine underneath it is not: 6 contended cores
    // beside the cluster's API server, its storage layer and every other
    // workload is a different rig from 6 quiet cores with 42 idle beside them,
    // and memory bandwidth, LLC pressure and disk contention all differ even
    // when the quota does not. `compare` refuses a row built across the two,
    // which is correct and is the whole reason this table exists.
    //
    // THE CONSEQUENCE, STATED PLAINLY: every LSQB, concurrency and stress
    // number in `measurements/` is `main-pod-6cpu`, and that series can no
    // longer be EXTENDED — only re-taken. Extending it requires a pod back on
    // the control-plane node, which is the node the migration exists to get
    // off. This
    // is a real cost of the move and it is not recoverable by relabelling.
    //
    // `mem_limit_mb` is 40 GiB, the pods' unchanged cgroup limit. `node_cores`
    // is the CCX63's 48, which is what the kernel reports and therefore what
    // `RigCheck` will observe — not the 6 the process may use.
    RigMachine {
        name: "bench-pod-6cpu",
        node_type: "ccx63",
        node_cores: 48,
        cpu_quota_cores: Some(6),
        mem_limit_mb: 40 * 1024,
        what: "a resident working pod on the dedicated bench node, capped at 6 CPU / 40 GiB \
               with the rest of the machine idle — the old pod shape, a new machine, and NOT \
               comparable with main-pod-6cpu",
    },
];

/// The rig a result was taken on: the machine, its quota, and the scale.
///
/// Required on every result document with NO DEFAULT, for the same reason
/// [`LevelResult::writes_mode`] is: a row that cannot say which rig produced
/// it is not an unlabelled row, it is an uninterpretable one — and the moment
/// it sits next to a labelled row, somebody reads a ratio off the pair.
///
/// The corpus scale travels here rather than beside it because it is the same
/// kind of fact and it fails the same way. `corpus` has always been recorded
/// and has never been COMPARED, so until now the reporter would happily put an
/// SF1 number next to an SF10 one. Folding the scale into the rig closes that
/// at the same time and by the same rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rig {
    /// The lane's name — a [`KNOWN_RIGS`] entry, or an inline spec's first
    /// field.
    pub name: String,
    /// The machine type, e.g. `ccx43`.
    pub node_type: String,
    /// Cores the machine has.
    pub node_cores: u32,
    /// The CPU quota in whole cores, `None` when none applied.
    pub cpu_quota_cores: Option<u32>,
    /// The memory ceiling in MiB.
    pub mem_limit_mb: u32,
    /// The corpus scale the number was taken at — `sf1`, `sf10`, or
    /// `synthetic-keys-N` for the generated corpus, whose key count IS its
    /// scale.
    ///
    /// **Graphalytics uses `ga-<graph>`** — `ga-kgs`, `ga-cit-Patents` — and
    /// the prefix is not decoration. Its corpora are named graphs rather than
    /// scale factors, and a bare `kgs` beside an `sf10` would look like a
    /// third scale factor of the SNB corpus. The reporter compares scales as
    /// TEXT and refuses a table that spans two, so the prefix is what keeps a
    /// Graphalytics row from ever being averaged with an SNB one.
    pub scale: String,
}

/// Whether a string is usable as a rig name, machine type or scale.
///
/// Not an escaping measure — emission goes through the engine's audited JSON
/// writer, like everything else in this document. It is a TYPO measure: rigs
/// are compared as text, so `bench-ccx63 ` with a trailing space, or a name
/// carrying a colon that splits an inline spec into the wrong fields, would
/// silently become a third lane that agrees with nothing.
fn rig_token_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

impl Rig {
    /// Resolve `--rig`'s value against a scale.
    ///
    /// Two forms. A bare name looks up a [`KNOWN_RIGS`] entry. An inline
    /// `name:node_type:cores:quota:mem_mb` describes a machine that is not
    /// part of this estate — `quota` is a core count or the literal `none`,
    /// and every field is mandatory, because a rig with a defaulted field is
    /// the thing being prevented.
    ///
    /// # Errors
    /// A string naming what was wrong and, for an unknown name, what the known
    /// rigs are. Never a fallback: there is no rig this could guess.
    pub fn from_spec(spec: &str, scale: &str) -> Result<Rig, String> {
        if !rig_token_ok(scale) {
            return Err(format!(
                "the corpus scale `{scale}` is not a usable label (ASCII \
                 alphanumerics, `-`, `_`, `.` and `/`, 1-64 characters)"
            ));
        }
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() == 1 {
            let name = parts[0];
            let known = KNOWN_RIGS.iter().find(|r| r.name == name).ok_or_else(|| {
                let names: Vec<&str> = KNOWN_RIGS.iter().map(|r| r.name).collect();
                format!(
                    "unknown rig `{name}`. Known rigs: {}. A machine that is not \
                     part of this estate is described inline as \
                     `name:node_type:cores:quota|none:mem_mb`",
                    names.join(", ")
                )
            })?;
            return Ok(Rig {
                name: known.name.to_string(),
                node_type: known.node_type.to_string(),
                node_cores: known.node_cores,
                cpu_quota_cores: known.cpu_quota_cores,
                mem_limit_mb: known.mem_limit_mb,
                scale: scale.to_string(),
            });
        }
        if parts.len() != 5 {
            return Err(format!(
                "an inline rig is `name:node_type:cores:quota|none:mem_mb` (5 \
                 fields); `{spec}` has {}",
                parts.len()
            ));
        }
        let (name, node_type) = (parts[0], parts[1]);
        for (label, tok) in [("name", name), ("node_type", node_type)] {
            if !rig_token_ok(tok) {
                return Err(format!(
                    "rig {label} `{tok}` is not a usable label (ASCII \
                     alphanumerics, `-`, `_`, `.` and `/`, 1-64 characters)"
                ));
            }
        }
        // An inline rig may not take a known rig's name. Shadowing one with
        // different numbers is the worst outcome this type can produce: two
        // runs that agree on a NAME and disagree on the machine compare
        // cleanly, and are not a comparison.
        if KNOWN_RIGS.iter().any(|r| r.name == name) {
            return Err(format!(
                "`{name}` is a known rig; use it by name rather than \
                 redescribing it inline, or pick a different name if this is a \
                 different machine"
            ));
        }
        let n = |label: &str, tok: &str| -> Result<u32, String> {
            tok.parse::<u32>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| format!("rig {label} `{tok}` is not a positive whole number"))
        };
        let node_cores = n("cores", parts[2])?;
        let cpu_quota_cores = if parts[3] == "none" {
            None
        } else {
            Some(n("quota", parts[3])?)
        };
        let mem_limit_mb = n("mem_mb", parts[4])?;
        if let Some(q) = cpu_quota_cores {
            if q > node_cores {
                return Err(format!(
                    "rig quota {q} exceeds the machine's {node_cores} cores, which \
                     is a typo rather than a measurement — say `none` if there was \
                     no quota"
                ));
            }
        }
        Ok(Rig {
            name: name.to_string(),
            node_type: node_type.to_string(),
            node_cores,
            cpu_quota_cores,
            mem_limit_mb,
            scale: scale.to_string(),
        })
    }

    /// The rig as a document value.
    ///
    /// The ONE renderer. [`RunReport::render`] emits this, [`Comparable`]
    /// stringifies this, and [`parse`] reads the emitted form back through the
    /// same writer — so a rig built in this process and a rig read off disk
    /// produce byte-identical text, and equality means what it says. Two
    /// renderers would eventually disagree about a space and refuse a run
    /// against itself.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(self.name.clone()));
        m.insert("node_type".to_string(), Value::Str(self.node_type.clone()));
        m.insert(
            "node_cores".to_string(),
            Value::Int(i64::from(self.node_cores)),
        );
        m.insert(
            "cpu_quota_cores".to_string(),
            self.cpu_quota_cores
                .map_or(Value::Null, |q| Value::Int(i64::from(q))),
        );
        m.insert(
            "mem_limit_mb".to_string(),
            Value::Int(i64::from(self.mem_limit_mb)),
        );
        m.insert("scale".to_string(), Value::Str(self.scale.clone()));
        Value::Map(m)
    }

    /// The rig as document text — what a comparison compares.
    #[must_use]
    pub fn to_json(&self) -> String {
        to_json(&self.to_value())
    }

    /// One line a person can read.
    ///
    /// Echoed by the harness the moment `--rig` resolves, before a sweep that
    /// may run for twenty minutes. A mistyped inline rig is cheap to notice
    /// then and expensive to notice afterwards, when the only remedy is to
    /// take the measurement again.
    #[must_use]
    pub fn describe(&self) -> String {
        let quota = self
            .cpu_quota_cores
            .map_or_else(|| "no quota".to_string(), |q| format!("{q}-core quota"));
        format!(
            "{} ({}, {} cores, {}, {} MiB, scale {})",
            self.name, self.node_type, self.node_cores, quota, self.mem_limit_mb, self.scale
        )
    }
}

// ─── The rig, checked against the machine ───────────────────────────────────

/// What the machine ACTUALLY looked like while the run happened.
///
/// # Why a declared rig was not enough, and how the hole was found
///
/// [`Rig`] is a DECLARATION. `--rig` is required, has no default and refuses to
/// be guessed, and all of that is right — the operator knows which lane they
/// are on and the harness makes them say it. But a declaration that nothing
/// checks is a declaration that cannot be wrong, and the 2026-09-09 dry run
/// said so in one sentence: *"I stamped two different pods identically and
/// nothing objected."*
///
/// [`compare`] refuses two documents that DISAGREE about their rig. It has
/// never been able to see two documents that agree about a rig neither of them
/// ran on. That guard protects against honesty and not against error, and it
/// matters most exactly when the dedicated bench node exists — because that is
/// when a 48-core number and a 6-core number can plausibly wear the same label.
///
/// # What is observable, and from where
///
/// Under a container the process's own limits are readable facts, not
/// inferences:
///
/// - **cgroup v2** — `/sys/fs/cgroup/cpu.max` (`"600000 100000"` is a 6-core
///   quota; `"max 100000"` is none) and `/sys/fs/cgroup/memory.max` (bytes, or
///   `"max"`). Read on the real bench pod on 2026-09-09: `600000 100000` and
///   `42949672960`, which is exactly the `Some(6)` / 40960 MiB that
///   `main-pod-6cpu` declares.
/// - **cgroup v1** — `cpu/cpu.cfs_quota_us` over `cpu/cpu.cfs_period_us`, and
///   `memory/memory.limit_in_bytes`. A negative quota or a limit at the
///   kernel's no-limit sentinel means "no ceiling", not "unknown".
/// - **The machine's cores** — `/sys/devices/system/cpu/online`, which is the
///   NODE's cpu list and not the process's allowance. `available_parallelism`
///   is deliberately NOT used: on Linux it already accounts for the cgroup
///   quota, so on the pod lane it would return 6 and report a 16-core node as a
///   6-core one — an observation that disagrees with the truth is worse than no
///   observation.
///
/// # And when there is nothing to read
///
/// A developer's workstation has no cgroup, and Windows has no `/sys` at all.
/// That is `None`, recorded as `unobservable`, and it is NOT a pass: a run
/// there is stamped `"status": "unobservable"` and says so on stderr. The
/// distinction this type exists to keep is between *checked and agreed* and
/// *never checked*, which is the same distinction the rest of this file keeps
/// between a refusal and a silence.
///
/// # Testability, and why it is visible in the document
///
/// `ENGRAM_BENCH_SYS_ROOT` and `ENGRAM_BENCH_PROC_ROOT` replace `/sys` and
/// `/proc`, so the guard can be exercised against fixture directories on any
/// platform — including making it FAIL, which is the only way to know it is a
/// guard. They are not a bypass: an overridden read stamps `override:` into the
/// source string that travels in the result document, so an observation taken
/// from a fixture cannot pass itself off as one taken from a machine.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedMachine {
    /// The CPU quota in cores. `None` is TWO different facts — no quota
    /// applied, or nothing was readable — and
    /// [`ObservedMachine::cpu_quota_readable`] is which.
    ///
    /// The distinction is the whole point of the type. "Declared a 6-core
    /// quota, ran under none" is a mismatch that must stop a run; "declared a
    /// 6-core quota, could not tell" is a stamp nobody checked. Collapsing them
    /// into one `None` made a workstation run report a mismatch against a rig
    /// it was not contradicting — caught by the fixture test, which is what the
    /// fixture test is for.
    pub cpu_quota_cores: Option<f64>,
    /// Whether the quota was actually READ. `false` means nothing said.
    pub cpu_quota_readable: bool,
    /// Where the quota came from, or why it did not.
    pub cpu_quota_source: String,
    /// The memory ceiling in MiB.
    pub mem_limit_mb: Option<u64>,
    /// Where the ceiling came from, or why it did not.
    pub mem_limit_source: String,
    /// Cores the MACHINE has.
    pub node_cores: Option<u32>,
    /// Where the core count came from, or why it did not.
    pub node_cores_source: String,
}

impl ObservedMachine {
    /// Nothing was readable — the workstation case.
    #[must_use]
    pub fn unobservable(why: &str) -> ObservedMachine {
        ObservedMachine {
            cpu_quota_cores: None,
            cpu_quota_readable: false,
            cpu_quota_source: why.to_string(),
            mem_limit_mb: None,
            mem_limit_source: why.to_string(),
            node_cores: None,
            node_cores_source: why.to_string(),
        }
    }

    /// Whether any of the three facts was actually read.
    ///
    /// A source that begins `unobservable` is an absence; anything else — a
    /// cgroup file, a sysfs file, or the deliberate `no quota` reading — is an
    /// observation.
    #[must_use]
    pub fn observed_anything(&self) -> bool {
        self.cpu_quota_readable || self.mem_limit_mb.is_some() || self.node_cores.is_some()
    }

    /// As a document value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "cpu_quota_cores".to_string(),
            self.cpu_quota_cores
                .map_or(Value::Null, |q| Value::Float(round3(q))),
        );
        m.insert(
            "cpu_quota_readable".to_string(),
            Value::Bool(self.cpu_quota_readable),
        );
        m.insert(
            "cpu_quota_source".to_string(),
            Value::Str(self.cpu_quota_source.clone()),
        );
        m.insert(
            "mem_limit_mb".to_string(),
            self.mem_limit_mb
                .map_or(Value::Null, |n| Value::Int(n as i64)),
        );
        m.insert(
            "mem_limit_source".to_string(),
            Value::Str(self.mem_limit_source.clone()),
        );
        m.insert(
            "node_cores".to_string(),
            self.node_cores
                .map_or(Value::Null, |n| Value::Int(i64::from(n))),
        );
        m.insert(
            "node_cores_source".to_string(),
            Value::Str(self.node_cores_source.clone()),
        );
        Value::Map(m)
    }
}

/// The verdict of checking a declared [`Rig`] against an [`ObservedMachine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigStatus {
    /// Every field the rig declares was observed, and every one agreed.
    Verified,
    /// Something was observed and agreed; something else could not be read.
    Partial,
    /// Nothing could be read. NOT a pass — the stamp is simply unchecked.
    Unobservable,
    /// At least one observed fact contradicts the declaration.
    Mismatch,
}

impl RigStatus {
    /// The word the document carries.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            RigStatus::Verified => "verified",
            RigStatus::Partial => "partial",
            RigStatus::Unobservable => "unobservable",
            RigStatus::Mismatch => "mismatch",
        }
    }
}

/// A declared rig, the machine that was actually underneath it, and whether
/// they agree.
#[derive(Clone, Debug, PartialEq)]
pub struct RigCheck {
    /// The verdict.
    pub status: RigStatus,
    /// What was read off the machine.
    pub observed: ObservedMachine,
    /// The facts that were checked and agreed, in words.
    pub agreements: Vec<String>,
    /// The facts that were checked and did NOT agree, in words.
    pub disagreements: Vec<String>,
}

/// How far a quota may differ from its declaration and still be the same
/// quota. Kubernetes writes whole-core quotas exactly; this is slack for a
/// fractional limit, not licence for a different machine.
const QUOTA_TOLERANCE_CORES: f64 = 0.05;

/// How far a memory ceiling may differ, as a fraction.
///
/// A cgroup limit is exact. `MemTotal` is not — the kernel's own reservations
/// put a 192 GB box a few percent below 192 GiB — and the un-limited case falls
/// back to it, so the tolerance has to cover that case without covering the
/// distance between 40 GiB and 160 GiB, which it does by a factor of thirty.
const MEM_TOLERANCE: f64 = 0.05;

impl RigCheck {
    /// Read the machine and check the declaration against it.
    #[must_use]
    pub fn observe(declared: &Rig) -> RigCheck {
        RigCheck::of(declared, observe_machine())
    }

    /// The comparison alone, with the observation handed in.
    ///
    /// Split from [`RigCheck::observe`] so the RULE is testable without a
    /// machine that has the property under test — the 48-core no-quota case is
    /// exactly the one no test host is going to have.
    #[must_use]
    pub fn of(declared: &Rig, observed: ObservedMachine) -> RigCheck {
        let mut agreements = Vec::new();
        let mut disagreements = Vec::new();
        let mut unchecked = 0usize;

        // ── CPU quota ──────────────────────────────────────────────────────
        // The asymmetry is the point. "Declared a quota, ran without one" is
        // the pod stamp on the dedicated box; "declared no quota, ran under
        // one" is the bench stamp on the pod. Both are the same error and both
        // must be named, because either direction produces a number the label
        // cannot support.
        if !observed.cpu_quota_readable {
            unchecked += 1;
        } else {
            match (declared.cpu_quota_cores, observed.cpu_quota_cores) {
                (Some(d), Some(o)) if (o - f64::from(d)).abs() <= QUOTA_TOLERANCE_CORES => {
                    agreements.push(format!("cpu quota {o} core(s) as declared"));
                }
                (Some(d), Some(o)) => disagreements.push(format!(
                    "the rig declares a {d}-core CPU quota and this process is running under \
                     a {o}-core one ({})",
                    observed.cpu_quota_source
                )),
                (Some(d), None) => disagreements.push(format!(
                    "the rig declares a {d}-core CPU quota and this process is running under \
                     NO quota ({}) — a capped lane's label on an uncapped machine",
                    observed.cpu_quota_source
                )),
                (None, Some(o)) => disagreements.push(format!(
                    "the rig declares NO CPU quota and this process is running under a \
                     {o}-core one ({}) — an uncapped lane's label on a capped machine",
                    observed.cpu_quota_source
                )),
                (None, None) => {
                    agreements.push("no cpu quota, as declared".to_string());
                }
            }
        }

        // ── Memory ceiling ─────────────────────────────────────────────────
        match observed.mem_limit_mb {
            None => unchecked += 1,
            Some(o) => {
                let d = f64::from(declared.mem_limit_mb);
                if (o as f64 - d).abs() <= d * MEM_TOLERANCE {
                    agreements.push(format!("memory ceiling {o} MiB as declared"));
                } else {
                    disagreements.push(format!(
                        "the rig declares a {} MiB memory ceiling and this process is running \
                         under {o} MiB ({})",
                        declared.mem_limit_mb, observed.mem_limit_source
                    ));
                }
            }
        }

        // ── The machine's cores ────────────────────────────────────────────
        match observed.node_cores {
            None => unchecked += 1,
            Some(o) if o == declared.node_cores => {
                agreements.push(format!("node has {o} core(s) as declared"));
            }
            Some(o) => disagreements.push(format!(
                "the rig declares a {}-core machine and this one has {o} core(s) ({})",
                declared.node_cores, observed.node_cores_source
            )),
        }

        let status = if !disagreements.is_empty() {
            RigStatus::Mismatch
        } else if agreements.is_empty() {
            RigStatus::Unobservable
        } else if unchecked > 0 {
            RigStatus::Partial
        } else {
            RigStatus::Verified
        };
        RigCheck {
            status,
            observed,
            agreements,
            disagreements,
        }
    }

    /// A never-performed check, for a document built without one.
    #[must_use]
    pub fn not_observed() -> RigCheck {
        RigCheck {
            status: RigStatus::Unobservable,
            observed: ObservedMachine::unobservable("unobservable: not attempted"),
            agreements: Vec::new(),
            disagreements: Vec::new(),
        }
    }

    /// One or more lines a person reads before a sweep starts.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.status {
            RigStatus::Verified => format!(
                "rig VERIFIED against the machine: {}",
                self.agreements.join("; ")
            ),
            RigStatus::Partial => format!(
                "rig partially verified: {} — and {} could not be read, so the rest of the \
                 stamp is unchecked",
                self.agreements.join("; "),
                self.unreadable().join(", ")
            ),
            RigStatus::Unobservable => format!(
                "rig NOT verified — nothing about this machine was readable ({}). The stamp \
                 is taken on trust, which is what it has always been; it is now SAID so, \
                 rather than looking like a check that passed",
                self.observed.cpu_quota_source
            ),
            RigStatus::Mismatch => format!(
                "rig DISAGREES with the machine that is about to produce the numbers: {}",
                self.disagreements.join("; ")
            ),
        }
    }

    /// Which of the three facts could not be read.
    #[must_use]
    pub fn unreadable(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.observed.cpu_quota_readable {
            out.push("cpu quota");
        }
        if self.observed.mem_limit_mb.is_none() {
            out.push("memory ceiling");
        }
        if self.observed.node_cores.is_none() {
            out.push("node cores");
        }
        out
    }

    /// As a document value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "status".to_string(),
            Value::Str(self.status.name().to_string()),
        );
        m.insert("observed".to_string(), self.observed.to_value());
        m.insert(
            "agreements".to_string(),
            Value::List(
                self.agreements
                    .iter()
                    .cloned()
                    .map(Value::Str)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "disagreements".to_string(),
            Value::List(
                self.disagreements
                    .iter()
                    .cloned()
                    .map(Value::Str)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        Value::Map(m)
    }
}

/// Read `/sys` and `/proc` for the process's own limits and the machine's core
/// count. See [`ObservedMachine`] for what is read and why.
#[must_use]
pub fn observe_machine() -> ObservedMachine {
    let sys = std::env::var("ENGRAM_BENCH_SYS_ROOT").unwrap_or_else(|_| "/sys".to_string());
    let proc = std::env::var("ENGRAM_BENCH_PROC_ROOT").unwrap_or_else(|_| "/proc".to_string());
    let overridden = std::env::var_os("ENGRAM_BENCH_SYS_ROOT").is_some()
        || std::env::var_os("ENGRAM_BENCH_PROC_ROOT").is_some();
    observe_machine_at(&sys, &proc, overridden)
}

/// [`observe_machine`] with the two roots handed in.
///
/// The env vars are read exactly once, in the caller, so a test can drive this
/// against fixture directories without mutating process-global state — which
/// under a parallel test binary is a race, and which Rust 2024 makes `unsafe`
/// for that reason. `overridden` is not cosmetic: it stamps `override:` into
/// every source string, so an observation taken from a fixture cannot pass
/// itself off in a result document as one taken from a machine.
#[must_use]
pub fn observe_machine_at(sys: &str, proc: &str, overridden: bool) -> ObservedMachine {
    let tag = |s: String| -> String {
        if overridden {
            format!("override:{s}")
        } else {
            s
        }
    };

    let read = |p: String| -> Option<String> { std::fs::read_to_string(p).ok() };
    // The cgroup-v2 path a container sees for itself is `/`, so
    // `<sys>/fs/cgroup/<file>` is the usual answer; the nested form is for a
    // process that is NOT in its own cgroup namespace.
    let nested: Option<String> = read(format!("{proc}/self/cgroup")).and_then(|txt| {
        txt.lines()
            .find_map(|l| l.strip_prefix("0::").map(|p| p.trim().to_string()))
            .filter(|p| p != "/" && !p.is_empty())
    });
    let cg_v2 = |file: &str| -> Option<(String, String)> {
        let direct = format!("{sys}/fs/cgroup/{file}");
        if let Some(v) = read(direct) {
            return Some((v, format!("cgroup-v2:{file}")));
        }
        let path = nested.as_ref()?;
        let p = format!("{sys}/fs/cgroup{path}/{file}");
        read(p).map(|v| (v, format!("cgroup-v2:{path}/{file}")))
    };
    let cg_v1 = |ctrl: &str, file: &str| -> Option<(String, String)> {
        let direct = format!("{sys}/fs/cgroup/{ctrl}/{file}");
        if let Some(v) = read(direct) {
            return Some((v, format!("cgroup-v1:{ctrl}/{file}")));
        }
        let path = nested.as_ref()?;
        let p = format!("{sys}/fs/cgroup/{ctrl}{path}/{file}");
        read(p).map(|v| (v, format!("cgroup-v1:{ctrl}{path}/{file}")))
    };

    // ── CPU quota ──────────────────────────────────────────────────────────
    let (cpu_quota_cores, cpu_quota_readable, cpu_quota_source) = match cg_v2("cpu.max") {
        Some((txt, src)) => {
            let mut it = txt.split_whitespace();
            let quota = it.next().unwrap_or("max");
            let period: f64 = it.next().and_then(|p| p.parse().ok()).unwrap_or(100_000.0);
            if quota == "max" {
                (None, true, tag(format!("{src}: no quota")))
            } else {
                match quota.parse::<f64>() {
                    Ok(q) if period > 0.0 => (Some(q / period), true, tag(src)),
                    _ => (
                        None,
                        false,
                        tag(format!("unobservable: {src} unparseable ({txt:?})")),
                    ),
                }
            }
        }
        None => match (
            cg_v1("cpu", "cpu.cfs_quota_us"),
            cg_v1("cpu", "cpu.cfs_period_us"),
        ) {
            (Some((q, src)), Some((p, _))) => {
                let qv: f64 = q.trim().parse().unwrap_or(-1.0);
                let pv: f64 = p.trim().parse().unwrap_or(100_000.0);
                if qv <= 0.0 {
                    (None, true, tag(format!("{src}: no quota")))
                } else if pv > 0.0 {
                    (Some(qv / pv), true, tag(src))
                } else {
                    (None, false, tag(format!("unobservable: {src} period {pv}")))
                }
            }
            _ => (
                None,
                false,
                tag("unobservable: no cgroup cpu.max or cpu.cfs_quota_us".to_string()),
            ),
        },
    };

    // ── Memory ceiling ─────────────────────────────────────────────────────
    // v1's no-limit sentinel is "a very large number", not a named value, so
    // anything at or above this is read as absent rather than as a ceiling of
    // eight exabytes.
    const NO_MEM_LIMIT_AT_OR_ABOVE: u64 = 1 << 53;
    let mem_total_mb = || -> Option<u64> {
        read(format!("{proc}/meminfo"))?
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb / 1024)
    };
    let from_bytes = |txt: &str| -> Option<u64> {
        txt.trim()
            .parse::<u64>()
            .ok()
            .filter(|b| *b < NO_MEM_LIMIT_AT_OR_ABOVE)
            .map(|b| b / (1024 * 1024))
    };
    let (mem_limit_mb, mem_limit_source) =
        match cg_v2("memory.max").or_else(|| cg_v1("memory", "memory.limit_in_bytes")) {
            Some((txt, src)) if txt.trim() != "max" && from_bytes(&txt).is_some() => {
                (from_bytes(&txt), tag(src))
            }
            // A cgroup that exists and declares no ceiling: the machine's RAM IS
            // the ceiling, which is what the rig's field means.
            Some((_, src)) => match mem_total_mb() {
                Some(mb) => (
                    Some(mb),
                    tag(format!("{src}: unlimited, {proc}/meminfo MemTotal")),
                ),
                None => (
                    None,
                    tag(format!("unobservable: {src} unlimited and no MemTotal")),
                ),
            },
            None => match mem_total_mb() {
                Some(mb) => (
                    Some(mb),
                    tag(format!("{proc}/meminfo MemTotal (no cgroup)")),
                ),
                None => (
                    None,
                    tag("unobservable: no cgroup memory limit and no MemTotal".to_string()),
                ),
            },
        };

    // ── The machine's cores ────────────────────────────────────────────────
    let (node_cores, node_cores_source) =
        match read(format!("{sys}/devices/system/cpu/online")).and_then(|t| count_cpu_list(&t)) {
            Some(n) => (Some(n), tag("sysfs:devices/system/cpu/online".to_string())),
            None => match read(format!("{proc}/cpuinfo")) {
                Some(t) => {
                    let n = t.lines().filter(|l| l.starts_with("processor")).count();
                    if n > 0 {
                        (
                            u32::try_from(n).ok(),
                            tag(format!("{proc}/cpuinfo processor lines")),
                        )
                    } else {
                        (
                            None,
                            tag(format!("unobservable: {proc}/cpuinfo has no processors")),
                        )
                    }
                }
                // Deliberately NOT `available_parallelism`: on Linux it accounts
                // for the cgroup quota, so on the pod lane it answers 6 for a
                // 16-core node. An observation that contradicts the truth would
                // make the guard fire on a correct stamp.
                None => (
                    None,
                    tag("unobservable: no cpu online list and no cpuinfo".to_string()),
                ),
            },
        };

    ObservedMachine {
        cpu_quota_cores,
        cpu_quota_readable,
        cpu_quota_source,
        mem_limit_mb,
        mem_limit_source,
        node_cores,
        node_cores_source,
    }
}

/// Count the CPUs in a Linux cpu-list such as `0-15` or `0,2-3,8`.
fn count_cpu_list(text: &str) -> Option<u32> {
    let mut total: u32 = 0;
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            None => {
                part.parse::<u32>().ok()?;
                total += 1;
            }
            Some((lo, hi)) => {
                let (lo, hi) = (
                    lo.trim().parse::<u32>().ok()?,
                    hi.trim().parse::<u32>().ok()?,
                );
                if hi < lo {
                    return None;
                }
                total += hi - lo + 1;
            }
        }
    }
    (total > 0).then_some(total)
}

/// Which workload produced a document.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    /// The nine-query analytical battery.
    Lsqb,
    /// The mixed-profile concurrency sweep.
    Stress,
    /// LDBC SNB Business Intelligence -- 20 parameterised analytical reads.
    SnbBi,
    /// LDBC SNB Interactive -- IC1-IC14 and IS1-IS7.
    SnbInteractive,
    /// LDBC FinBench -- tcr1-tcr12.
    Finbench,
}

impl Workload {
    /// The name in the document.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Workload::Lsqb => "lsqb",
            Workload::Stress => "stress",
            Workload::SnbBi => "snb-bi",
            Workload::SnbInteractive => "snb-interactive",
            Workload::Finbench => "finbench",
        }
    }
}

/// One run of one workload against one engine.
pub struct RunReport {
    /// Which workload.
    pub workload: Workload,
    /// The engine's name.
    pub engine: String,
    /// The version string it announced.
    pub engine_version: String,
    /// Which dialect it was driven in.
    pub dialect: String,
    /// Where it was reached.
    pub addr: String,
    /// Which corpus family — `synthetic`, `snb`, `snb-platform`.
    pub dataset: String,
    /// The corpus name expected counts are keyed on, e.g. `sf1`.
    pub corpus: String,
    /// The run seed.
    pub seed: u64,
    /// The key space.
    pub keys: u64,
    /// The catalogue the statements came from.
    pub catalogue_digest: u64,
    /// The SHA-256 of the plan file, when the ops were replayed rather than
    /// generated. Carried so a result can always be traced to the exact plan
    /// that produced it.
    pub plan_sha256: Option<String>,
    /// Who emitted that plan — `engram-bench/harness` for a measurement plan,
    /// `ladybug-conc.py` for the LadybugDB executor's own reference plan. A
    /// comparison built on a self-generated plan must be VISIBLE as one.
    pub plan_emitter: Option<String>,
    /// `live` or `plan`.
    pub op_source: String,
    /// `single` or `multi` — whether the engine was allowed more than one
    /// concurrent write transaction. See [`LevelResult::writes_mode`].
    pub writes_mode: String,
    /// The machine, the quota and the scale this number was taken under.
    ///
    /// Not an `Option`. A run that cannot say which rig produced it must fail
    /// to be CONSTRUCTED, not emit an unlabelled row — the same rule
    /// `writes_mode` follows and for the same reason, except that a rig is the
    /// coarser fact: `writes_mode` changes what the workload was, a rig
    /// changes what the machine was.
    pub rig: Rig,
    /// The declared rig, checked against the machine that produced the numbers.
    ///
    /// [`Rig`] says what the operator CLAIMED; this says what was underneath
    /// them and whether the two agree. A declaration nothing checks cannot be
    /// wrong, and `compare` refusing a declared mismatch while being blind to
    /// an undeclared one protects against honesty rather than against error.
    pub rig_check: RigCheck,
    /// The knobs that must match for a comparison to be one.
    pub fairness: Fairness,
    /// The declared fairness block, checked against the engine that ran under
    /// it.
    ///
    /// [`Fairness`] says what the operator CLAIMED the engine was configured
    /// with; this says what the engine ANSWERED and whether the two agree. It
    /// is [`RigCheck`]'s exact counterpart one level down — the rig is which
    /// machine, this is which configuration of the engine on it — and it
    /// exists for the same reason: `compare` refusing two documents that
    /// disagree cannot see two documents that agree about a server neither of
    /// them ran on. See [`crate::fairness`] for the two runs where that
    /// happened.
    pub fairness_check: crate::fairness::FairnessCheck,
    /// Stress levels, if any.
    pub levels: Vec<LevelResult>,
    /// LSQB outcomes, if any.
    pub queries: Vec<QueryResult>,
    /// Integrity findings — anything here fails the run regardless of
    /// throughput.
    pub integrity: Vec<String>,
    /// Whatever else made the run fail.
    pub failures: Vec<String>,
}

impl RunReport {
    /// Whether the run passed: no integrity finding, no other failure, and at
    /// least one thing actually measured.
    ///
    /// The last clause is the one that matters and the one that is easy to
    /// omit: a run of nine `unmappable` queries, or a sweep of zero levels,
    /// compared nothing and must not pass.
    #[must_use]
    pub fn pass(&self) -> bool {
        self.integrity.is_empty()
            && self.failures.is_empty()
            && (self.queries.iter().any(|q| q.status == "ok") || !self.levels.is_empty())
    }

    /// Render the document.
    ///
    /// Emission goes through the engine's own JSON writer, so escaping is the
    /// audited path — `stress.rs` escaped by hand and could only escape the
    /// two characters somebody thought of.
    #[must_use]
    pub fn render(&self) -> String {
        let mut m = BTreeMap::new();
        m.insert("schema_version".into(), Value::Int(SCHEMA_VERSION));
        m.insert("tool".into(), Value::Str("harness".into()));
        m.insert("workload".into(), Value::Str(self.workload.name().into()));
        m.insert("engine".into(), Value::Str(self.engine.clone()));
        m.insert(
            "engine_version".into(),
            Value::Str(self.engine_version.clone()),
        );
        m.insert("dialect".into(), Value::Str(self.dialect.clone()));
        m.insert("addr".into(), Value::Str(self.addr.clone()));
        m.insert("dataset".into(), Value::Str(self.dataset.clone()));
        m.insert("corpus".into(), Value::Str(self.corpus.clone()));
        m.insert("seed".into(), Value::Int(self.seed as i64));
        m.insert("keys".into(), Value::Int(self.keys as i64));
        m.insert(
            "catalogue_digest".into(),
            Value::Str(format!("{:016x}", self.catalogue_digest)),
        );
        // Per-family digests, alongside the whole-file one rather than
        // instead of it. The whole-file digest stays because every document
        // already written carries it and the comparison rule still falls back
        // to it; the per-family map is what lets a NEW battery arrive in a new
        // file without orphaning the numbers this file's families produced.
        // Every family the binary holds is stamped, not just the one driven —
        // a document that lists what the binary carried is one a later reader
        // can check a claim against.
        let mut fams = BTreeMap::new();
        for f in crate::catalogue::FAMILIES {
            fams.insert(
                f.name.to_string(),
                Value::Str(format!("{:016x}", f.digest())),
            );
        }
        m.insert("catalogue_families".into(), Value::Map(fams));
        m.insert(
            "plan_sha256".into(),
            self.plan_sha256
                .clone()
                .map(Value::Str)
                .unwrap_or(Value::Null),
        );
        m.insert(
            "plan_emitter".into(),
            self.plan_emitter
                .clone()
                .map(Value::Str)
                .unwrap_or(Value::Null),
        );
        m.insert("op_source".into(), Value::Str(self.op_source.clone()));
        m.insert("writes_mode".into(), Value::Str(self.writes_mode.clone()));
        m.insert("rig".into(), self.rig.to_value());
        m.insert("rig_check".into(), self.rig_check.to_value());
        let mut fair = BTreeMap::new();
        fair.insert(
            "thread_cap".to_string(),
            Value::Int(i64::from(self.fairness.thread_cap)),
        );
        fair.insert(
            "cache_budget_mb".to_string(),
            Value::Int(i64::from(self.fairness.cache_budget_mb)),
        );
        fair.insert(
            "clients".to_string(),
            Value::Int(self.fairness.clients as i64),
        );
        fair.insert(
            "seconds".to_string(),
            Value::Int(self.fairness.seconds as i64),
        );
        m.insert("fairness".into(), Value::Map(fair));
        m.insert("fairness_check".into(), self.fairness_check.to_value());
        m.insert(
            "levels".into(),
            Value::List(
                self.levels
                    .iter()
                    .map(|l| l.to_value(&self.engine))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "queries".into(),
            Value::List(
                self.queries
                    .iter()
                    .map(|q| q.to_value(&self.engine))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "integrity".into(),
            Value::List(
                self.integrity
                    .iter()
                    .cloned()
                    .map(Value::Str)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert(
            "failures".into(),
            Value::List(
                self.failures
                    .iter()
                    .cloned()
                    .map(Value::Str)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        m.insert("pass".into(), Value::Bool(self.pass()));
        to_json(&Value::Map(m))
    }
}

// ─── The reporter ───────────────────────────────────────────────────────────

/// One engine's row, as the comparison table reads it back.
#[derive(Clone, Debug)]
pub struct Row {
    /// Engine name.
    pub engine: String,
    /// Profile or query name.
    pub key: String,
    /// Client count for a stress row; 1 for an LSQB row.
    pub clients: usize,
    /// ops/s for stress; milliseconds for LSQB.
    pub value: f64,
    /// Whether the row may be quoted.
    pub quotable: bool,
    /// Why not, when it may not.
    pub why: Option<String>,
    /// The machine-readable cause code, when the row is not quotable.
    ///
    /// Carried so the TABLE can say which refusals are operator errors without
    /// pattern-matching English out of `why` — see [`NotQuotable::code`].
    pub cause: Option<String>,
    /// The LSQB count, when this is an LSQB row.
    pub count: Option<i64>,
    /// What the run BOUND for this row, as it recorded it (`bound country=
    /// Str("China") …`), when it recorded anything. Two runs that bound
    /// different values asked different questions under one key — see
    /// [`parameter_mismatches`].
    pub probe: Option<String>,
}

/// A run reduced to what a comparison needs: the rows, and the facts that say
/// whether the rows may be compared at all.
///
/// The reporter reads a run's RECORDED verdict rather than re-deriving it.
/// That is the point of putting `quotable` and `not_quotable_because` in the
/// document: the qualification was computed where the evidence was, and a
/// reporter that recomputed it from a lossy reconstruction could disagree with
/// the run that produced it — which is how a caveat gets lost between a
/// measurement and a table.
#[derive(Clone, Debug)]
pub struct Comparable {
    /// The engine's name.
    pub engine: String,
    /// Which workload.
    pub workload: String,
    /// The catalogue the statements came from.
    pub catalogue_digest: String,
    /// Each catalogue FAMILY the producing binary held, and that family's own
    /// digest — `lsqb-stress`, `snb-interactive`, …
    ///
    /// EMPTY for every document written before families existed, and that is
    /// not a defect: such a document is compared on `catalogue_digest` exactly
    /// as it always was. The map is additive, and the guard it feeds is never
    /// weaker — a run that cannot prove it used the same statement text is
    /// still refused, it is simply asked about the text it actually ran.
    pub catalogue_family_digests: BTreeMap<String, String>,
    /// The fairness block, as text — compared for equality, not interpreted.
    pub fairness: String,
    /// `single`, `multi`, or `n/a` for a read-only workload.
    pub writes_mode: String,
    /// The rig block, as text — compared for equality, not interpreted.
    ///
    /// `None` ONLY for a document written before the rig stamp existed. That
    /// is not a malformed document, so [`parse`] reads it; it is an
    /// uncomparable one, so [`compare`] refuses it by name. Keeping the two
    /// apart matters: `parse` says what a document IS and `compare` says what
    /// may be done with several of them, and folding the second into the first
    /// would put a comparison rule somewhere nobody looks for one.
    pub rig: Option<String>,
    /// Whether the rig stamp was checked against the machine, and how it came
    /// out — `verified`, `partial`, `unobservable`, `mismatch`.
    ///
    /// `None` for a document written before the check existed, which is the
    /// same position as an absent rig and is handled the same way: readable,
    /// and refused only by the rule that would blend it.
    pub rig_check: Option<String>,
    /// Whether the fairness stamp was checked against the ENGINE, and how it
    /// came out — `verified`, `partial`, `declared`, `mismatch`.
    ///
    /// `None` for every document written before the check existed, which is
    /// the same position an absent `rig_check` holds and is handled the same
    /// way: readable, and refused only by the rule that would blend it.
    pub fairness_check: Option<String>,
    /// The rig's declared CPU quota in whole cores, kept as a NUMBER beside
    /// the text.
    ///
    /// The text form above is compared for equality and never interpreted,
    /// which is right for deciding whether two documents describe the same
    /// machine and useless for [`narrow_width`], which has to relate this
    /// figure to a different block's. `None` where the rig is absent, or where
    /// it declares no quota (`cpu_quota_cores: None` — a lane that had the
    /// whole machine, which cannot be under-used by definition).
    pub declared_quota_cores: Option<u32>,
    /// The fairness block's declared thread cap, likewise as a number.
    ///
    /// `None` only where a document has no fairness block at all, which
    /// [`parse`] already refuses — so in practice this is `Some` wherever a
    /// document is comparable.
    pub declared_thread_cap: Option<u32>,
    /// The rows this run contributes.
    pub rows: Vec<Row>,
}

/// A document whose declared WIDTH is far below its declared QUOTA, and the
/// sentence to print about it.
///
/// # The hole this closes, and why neither existing check could see it
///
/// [`RigCheck`] asks "does the declared machine match the machine?" and
/// [`crate::fairness::FairnessCheck`] asks "does the declared engine
/// configuration match the engine?". Both are sound, both are checked against
/// the thing they describe, and both can return `verified` for the same
/// document while that document describes a SIX-THREAD engine on a
/// FORTY-FOUR-CORE machine.
///
/// Nothing was wrong with either answer. The two blocks describe different
/// things and no rule related them, so no rule could refuse the combination —
/// and a wrong-lane number wears exactly that shape: every check green, and
/// the green checks contradicting one another.
///
/// It is not hypothetical, and the scale is known. When the full-node lane was
/// specified, 150 scripts on the bench rig's `/work` carried a hardcoded
/// `--threads 6`, `--workers 6`, `max_num_threads=6` or
/// `ENGRAM_QUERY_PARALLELISM=6` — the old rig's 6-core quota written into the
/// scripts as well as the pod spec. Run one unchanged against a 44-core pod
/// and it produces a real, reproducible, correctly-stamped number that is not
/// a full-node number.
///
/// # Why a NOTE and not a refusal
///
/// The same reasoning [`crate::fairness::Provenance::Declared`] is given: a
/// deliberately narrow arm on a wide machine is a legitimate measurement. A
/// like-for-like against the recorded `main-pod-6cpu` series is the obvious
/// one, and it is exactly what somebody re-taking that series on the new node
/// would run. Refusing it would make the harness unusable for the comparison
/// most worth making.
///
/// What must not happen is running one WITHOUT KNOWING, and a note on the
/// table is what closes that. The threshold is deliberately loose — width
/// below half the quota — because this is not measuring efficiency, it is
/// catching a constant from another lane. 44 against 44 passes. 22 against 44
/// passes, because a half-width arm is a study somebody chose. 6 against 44
/// does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NarrowWidth {
    /// The engine whose document it is.
    pub engine: String,
    /// The declared thread cap.
    pub thread_cap: u32,
    /// The declared quota, in whole cores.
    pub quota_cores: u32,
}

/// Every run whose declared width is less than half its declared quota.
///
/// See [`NarrowWidth`] for why this exists and why it is not a refusal.
/// Documents that declare no quota (the whole-machine lanes) and documents
/// with no rig are skipped rather than guessed at: an absent figure is absent,
/// which is the same line every other check in this file holds.
///
/// Takes `&[&Comparable]`, matching [`compare`] and [`table`] rather than
/// [`parse`]: this is a question about a SET of documents, and the set is what
/// the reporter holds.
#[must_use]
pub fn narrow_width(runs: &[&Comparable]) -> Vec<NarrowWidth> {
    let mut out = Vec::new();
    for r in runs {
        let (Some(cap), Some(quota)) = (r.declared_thread_cap, r.declared_quota_cores) else {
            continue;
        };
        if quota > 0 && cap.saturating_mul(2) < quota {
            out.push(NarrowWidth {
                engine: r.engine.clone(),
                thread_cap: cap,
                quota_cores: quota,
            });
        }
    }
    out
}

impl From<&RunReport> for Comparable {
    fn from(r: &RunReport) -> Comparable {
        Comparable {
            engine: r.engine.clone(),
            workload: r.workload.name().to_string(),
            catalogue_digest: format!("{:016x}", r.catalogue_digest),
            // The same map `render` writes, built the same way, so an
            // in-process run and its own emitted document agree and never
            // refuse each other.
            catalogue_family_digests: crate::catalogue::FAMILIES
                .iter()
                .map(|f| (f.name.to_string(), format!("{:016x}", f.digest())))
                .collect(),
            fairness: r.fairness.to_json(),
            writes_mode: r.writes_mode.clone(),
            // Through `Rig::to_value` and the same writer `parse` reads with,
            // so an in-process run and its own emitted document produce the
            // same text and never refuse each other.
            rig: Some(r.rig.to_json()),
            rig_check: Some(r.rig_check.status.name().to_string()),
            fairness_check: Some(r.fairness_check.status.name().to_string()),
            declared_quota_cores: r.rig.cpu_quota_cores,
            declared_thread_cap: Some(r.fairness.thread_cap),
            rows: rows(r),
        }
    }
}

/// Read a result document back.
///
/// # Errors
/// Anything malformed, named. A reporter that repaired a document would be
/// building a table out of what it guessed the run meant.
pub fn parse(doc: &str) -> Result<Comparable, String> {
    let v = engram_cypher::json::from_json(doc).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Map(m) = v else {
        return Err("document is not an object".to_string());
    };
    let s = |k: &str| -> Result<String, String> {
        match m.get(k) {
            Some(Value::Str(v)) => Ok(v.clone()),
            other => Err(format!("{k} = {other:?}")),
        }
    };
    let engine = s("engine")?;
    let workload = s("workload")?;
    let fairness = match m.get("fairness") {
        Some(f) => to_json(f),
        None => return Err("no fairness block: this document cannot be compared".to_string()),
    };
    // The numeric halves of the two blocks, read once here so
    // [`narrow_width`] can relate them. A field that is absent, null, or not
    // an integer yields `None` and the check simply does not fire — the same
    // position `rig_check` holds for an absent block, and for the same reason:
    // a guessed figure would make a note that names the wrong document.
    let int_field = |block: &str, key: &str| -> Option<u32> {
        let Some(Value::Map(b)) = m.get(block) else {
            return None;
        };
        match b.get(key) {
            Some(Value::Int(v)) if *v >= 0 => u32::try_from(*v).ok(),
            _ => None,
        }
    };
    let declared_thread_cap = int_field("fairness", "thread_cap");
    let declared_quota_cores = int_field("rig", "cpu_quota_cores");
    // An absent rig is NOT a parse error. Every document committed before the
    // stamp existed lacks one, and they are still readable evidence — the
    // refusal belongs in `compare`, which is the thing that would blend them.
    // A `null` rig is treated as absent rather than as the text "null", so a
    // hand-written skeleton cannot pass the check with a placeholder.
    let rig = match m.get("rig") {
        None | Some(Value::Null) => None,
        Some(v) => Some(to_json(v)),
    };
    // The check's STATUS is what a comparison acts on; the observation itself
    // stays in the document for a reader. An absent block is absent, never
    // read as `verified` — the whole failure this closes is an unchecked stamp
    // that looked checked.
    let rig_check = match m.get("rig_check") {
        Some(Value::Map(rc)) => match rc.get("status") {
            Some(Value::Str(v)) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    };
    // Read exactly as `rig_check` is, and absent exactly as often: every
    // document committed before the fairness check existed lacks one, and an
    // absent block is absent rather than `verified`. The whole failure being
    // closed is an unchecked stamp that looked checked, so reading a missing
    // block as a pass would reproduce it inside the reader.
    let fairness_check = match m.get("fairness_check") {
        Some(Value::Map(fc)) => match fc.get("status") {
            Some(Value::Str(v)) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    };
    let mut rows = Vec::new();
    if let Some(Value::List(levels)) = m.get("levels") {
        for lv in levels.iter() {
            let Value::Map(l) = lv else {
                return Err("a level is not an object".to_string());
            };
            let get_str = |k: &str| match l.get(k) {
                Some(Value::Str(v)) => Some(v.clone()),
                _ => None,
            };
            let get_int = |k: &str| match l.get(k) {
                Some(Value::Int(v)) => Some(*v),
                _ => None,
            };
            rows.push(Row {
                engine: engine.clone(),
                key: get_str("profile").ok_or("a level has no profile")?,
                clients: get_int("clients").unwrap_or(1) as usize,
                value: match l.get("ops_per_sec") {
                    Some(Value::Float(f)) => *f,
                    Some(Value::Int(i)) => *i as f64,
                    _ => return Err("a level has no ops_per_sec".to_string()),
                },
                quotable: matches!(l.get("quotable"), Some(Value::Bool(true))),
                why: get_str("not_quotable_because"),
                // Absent on every document written before the taxonomy existed,
                // and on a document written by the LadybugDB executor until it
                // carries one. Absent is absent, never a guess derived from the
                // prose.
                cause: get_str("not_quotable_cause"),
                count: None,
                probe: None,
            });
        }
    }
    if let Some(Value::List(queries)) = m.get("queries") {
        for q in queries.iter() {
            let Value::Map(qm) = q else {
                return Err("a query is not an object".to_string());
            };
            let status = match qm.get("status") {
                Some(Value::Str(v)) => v.clone(),
                _ => return Err("a query has no status".to_string()),
            };
            rows.push(Row {
                engine: engine.clone(),
                key: match qm.get("query") {
                    Some(Value::Str(v)) => v.clone(),
                    _ => return Err("a query has no name".to_string()),
                },
                clients: 1,
                value: match qm.get("millis") {
                    Some(Value::Float(f)) => *f,
                    Some(Value::Int(i)) => *i as f64,
                    _ => f64::NAN,
                },
                quotable: status == "ok",
                why: if status == "ok" {
                    None
                } else {
                    Some(format!("status {status}"))
                },
                cause: if status == "ok" {
                    None
                } else {
                    Some(format!("query_status_{status}"))
                },
                count: match qm.get("count") {
                    Some(Value::Int(n)) => Some(*n),
                    _ => None,
                },
                probe: match qm.get("probe") {
                    Some(Value::Str(p)) => Some(p.clone()),
                    _ => None,
                },
            });
        }
    }
    Ok(Comparable {
        engine,
        workload,
        catalogue_digest: s("catalogue_digest")?,
        // Absent is EMPTY, never an error: documents predating families are
        // readable, and `compare` is where their consequence is decided. A
        // member that is not a string is dropped rather than guessed at — a
        // digest that is not a hex string proves nothing, and the fallback
        // then refuses on the whole file, which is the conservative answer.
        catalogue_family_digests: match m.get("catalogue_families") {
            Some(Value::Map(fm)) => fm
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Str(d) => Some((k.clone(), d.clone())),
                    _ => None,
                })
                .collect(),
            _ => BTreeMap::new(),
        },
        fairness,
        writes_mode: s("writes_mode").unwrap_or_else(|_| "n/a".to_string()),
        rig,
        rig_check,
        fairness_check,
        declared_quota_cores,
        declared_thread_cap,
        rows,
    })
}

/// What stopped a comparison from being one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompareRefusal {
    /// Two documents were driven by different statement text, and neither
    /// could name the family it ran — so the whole catalogue file is what
    /// disagreed.
    CatalogueDigest {
        /// The engines, and their digests.
        detail: String,
    },
    /// Two documents were driven by different text of the SAME family. The
    /// family is carried because "catalogue digest differs" without one sends
    /// the reader to whichever catalogue file they happen to open first.
    CatalogueFamilyDigest {
        /// Which family disagreed — `lsqb-stress`, `snb-interactive`, …
        family: String,
        /// The engines, and their digests for that family.
        detail: String,
    },
    /// Two documents were taken under different fairness knobs.
    Fairness {
        /// The engines, and their blocks.
        detail: String,
    },
    /// Two documents allowed different numbers of concurrent writers.
    WritesMode {
        /// The engines, and their modes.
        detail: String,
    },
    /// Two documents were taken on different rigs — different machines,
    /// different quotas, or different corpus scales.
    Rig {
        /// The engines, and their rigs.
        detail: String,
    },
    /// A document does not say which rig produced it.
    RigUnstamped {
        /// Which document.
        detail: String,
    },
    /// A document's declared rig CONTRADICTS the machine that produced it.
    RigMismatch {
        /// Which document, and what disagreed.
        detail: String,
    },
    /// A document's declared fairness block CONTRADICTS the engine that
    /// produced it.
    FairnessMismatch {
        /// Which document, and what disagreed.
        detail: String,
    },
    /// Fewer than two documents — nothing to compare.
    NotEnough,
}

impl std::fmt::Display for CompareRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompareRefusal::RigMismatch { detail } => write!(
                f,
                "this run's rig stamp CONTRADICTS the machine it ran on, so the label is \
                 wrong rather than missing: {detail}. A wrong stamp is worse than an absent \
                 one, because it COMPARES — two runs that agree on a name they did not both \
                 earn produce a clean table of two different machines. Fix the `--rig` and \
                 take the measurement again; do not edit the document"
            ),
            CompareRefusal::FairnessMismatch { detail } => write!(
                f,
                "this run's fairness stamp CONTRADICTS the engine it ran on, so the block is \
                 wrong rather than missing: {detail}. A wrong fairness stamp is worse than an \
                 absent one for the same reason a wrong rig is: it COMPARES. Two documents \
                 that agree on `cache_budget_mb: 8192` while one of them ran against 10 GiB \
                 of page cache build a clean table of two differently configured servers — \
                 which is what happened, and is why this check exists. Configure the engine \
                 to the stamp, or stamp what the engine is configured with, and take the \
                 measurement again; do not edit the document"
            ),
            CompareRefusal::CatalogueDigest { detail } => write!(
                f,
                "these runs were driven by DIFFERENT statement text, so a row built \
                 from them compares two catalogues and not two engines: {detail}"
            ),
            CompareRefusal::Fairness { detail } => write!(
                f,
                "these runs were taken under different fairness knobs, so a row built \
                 from them compares two machines and not two engines: {detail}"
            ),
            CompareRefusal::WritesMode { detail } => write!(
                f,
                "these runs allowed different numbers of concurrent writers, so the same \
                 plan produced categorically different workloads: {detail}"
            ),
            CompareRefusal::Rig { detail } => write!(
                f,
                "these runs were taken on DIFFERENT RIGS, so a row built from them \
                 compares two machines and not two engines. This project runs two \
                 measurement lanes side by side and will keep both: a pod under a \
                 6-CPU quota on a shared node, and a dedicated bench node with no \
                 quota at all. A number moved between them looks like an engine that \
                 got several times faster on a day nobody changed it, and nothing in \
                 the numbers themselves says otherwise. Quote each lane on its own, \
                 or re-run the arm you are missing on the other lane: {detail}"
            ),
            CompareRefusal::RigUnstamped { detail } => write!(
                f,
                "this run does not say which rig produced it, so it cannot enter a \
                 comparison: {detail}. Documents written before the rig stamp existed \
                 are in this position, and editing one in is NOT the remedy — the \
                 value would be a reconstruction, and a reconstructed rig is exactly \
                 the blend this refuses. Re-run it on a stamped harness, or quote it \
                 alone with its provenance read out of the ledger"
            ),
            CompareRefusal::CatalogueFamilyDigest { family, detail } => write!(
                f,
                "these two runs issued different `{family}` statement text, so the row \
                 between them would compare two catalogues and not two engines: {detail}. \
                 The file to open is `catalogue/{family}.json` (the `lsqb-stress` family \
                 lives in `catalogue/statements.json`) — no other family's digest is \
                 involved, and changing one of them is not what caused this. Re-run the \
                 older side against the current text, or quote the two numbers separately"
            ),
            CompareRefusal::NotEnough => {
                write!(f, "a comparison needs at least two runs")
            }
        }
    }
}

/// One key that got materially worse between two runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    /// The query or profile name.
    pub key: String,
    /// What the baseline recorded, when it recorded a number.
    pub was: Option<f64>,
    /// What the candidate recorded, when it recorded a number.
    pub now: Option<f64>,
    /// Why this counts as a regression, in the words a reader needs.
    pub why: String,
}

/// Does a bigger `Row::value` mean a BETTER result for this workload?
///
/// `Row::value` is ops/s for a stress row and MILLISECONDS for every other
/// lane, so the direction is not a property of the row — it is a property of
/// the workload that produced it. A gate that assumed one direction would
/// report every speed-up as a regression on half the lanes and, worse, pass
/// every real regression on the other half.
fn bigger_is_better(workload: &str) -> bool {
    workload.starts_with("stress")
}

/// Compare a CANDIDATE run against a BASELINE and report what got worse.
///
/// `tolerance` is a fraction: 0.10 admits a 10 % drift before a key is called
/// a regression. Run-to-run noise is real and a gate that fires on it gets
/// switched off, which is the failure mode that matters most for a scheduled
/// lane nobody is watching.
///
/// # What counts as a regression, and why each rule is here
///
/// - **A key the baseline had and the candidate does NOT** is a regression.
///   A query that silently stopped being run is the absent-signal-read-as-good
///   shape this repo keeps finding: the table gets shorter and greener at the
///   same time.
/// - **A key that was quotable and is now not** (a timeout, an error) is a
///   regression whatever its number says. A killed query is not a timing, so
///   there is no value to compare — the status IS the result.
/// - **A key that was already not quotable** is NOT a regression. It was
///   failing before; this gate reports what THIS change broke, and a standing
///   failure reported every night is noise that trains people to ignore it.
/// - **A key only the candidate has** is NOT a regression. Adding coverage
///   must never fail the gate.
///
/// Returns every regression found, so one run names all of them rather than
/// the alphabetically-first.
pub fn regressions(
    baseline: &Comparable,
    candidate: &Comparable,
    tolerance: f64,
) -> Vec<Regression> {
    regressions_in_every(baseline, &[candidate], tolerance, 0.0)
}

/// [`regressions`] over several REPETITIONS of one candidate, with an absolute
/// floor under the relative tolerance: a key is a regression only when EVERY
/// repetition regresses it, and a millisecond key only when it is also slower
/// by more than `floor_ms`. One repetition and a zero floor is exactly
/// [`regressions`].
///
/// # Why each is here
///
/// The SF3 batteries time each query once per pass. rev54's Interactive pass,
/// gated against rev49's at 25 %, named IC1 (6.2 -> 9.0 ms) and IC3 (336.6 ->
/// 436.2 ms); repeated runs of that one binary put IC1 at 5.4-7.1 ms and IC3
/// at 253-512 — noise, and noise reported as a regression is the failure the
/// doc above names first. A 2.8 ms swing on a 6 ms query is past any relative
/// tolerance a gate could keep, so the ratio alone cannot tell it from a
/// regression; and a figure that rides on the server's history (IC3, IC9,
/// bi1: `measurements/baselines/README.md`) moves by more than the tolerance
/// between passes of ONE binary, so one pass cannot either. A regression
/// reproduces; noise does not.
///
/// What each costs, stated so nobody mistakes it for free: the floor passes a
/// slowdown smaller than itself whatever its ratio (a 1 ms query at 5 ms
/// passes a 5 ms floor), and the repetitions pass a regression that shows in
/// only some of them. The figure reported is the BEST repetition's — the one
/// that shows the regression beyond the noise.
///
/// The floor is milliseconds and applies to the millisecond lanes only; a
/// throughput lane (stress, ops/s) is judged on the tolerance alone.
pub fn regressions_in_every(
    baseline: &Comparable,
    candidates: &[&Comparable],
    tolerance: f64,
    floor_ms: f64,
) -> Vec<Regression> {
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    let better_up = bigger_is_better(&first.workload);
    let mut out = Vec::new();
    for base in &baseline.rows {
        let mut mildest: Option<Regression> = None;
        let mut in_every = true;
        for cand in candidates {
            let row = cand.rows.iter().find(|r| r.key == base.key);
            match judge(base, row, better_up, tolerance, floor_ms) {
                None => {
                    in_every = false;
                    break;
                }
                Some(r) => {
                    mildest = Some(match mildest {
                        None => r,
                        Some(m) => milder(m, r, better_up),
                    });
                }
            }
        }
        if let (true, Some(mut r)) = (in_every, mildest) {
            if candidates.len() > 1 {
                r.why = format!(
                    "{} -- in every one of {} runs; the best shown",
                    r.why,
                    candidates.len()
                );
            }
            out.push(r);
        }
    }
    out
}

/// One baseline key against one candidate's row for it (`None`: the candidate
/// did not run it) — the rules [`regressions`] lists.
fn judge(
    base: &Row,
    cand: Option<&Row>,
    better_up: bool,
    tolerance: f64,
    floor_ms: f64,
) -> Option<Regression> {
    // It was already broken. Not this change's doing.
    if !base.quotable {
        return None;
    }
    let Some(cand) = cand else {
        return Some(Regression {
            key: base.key.clone(),
            was: Some(base.value),
            now: None,
            why: "the baseline measured this and the candidate did not run it \
                  at all -- a shorter table is not a greener one"
                .to_string(),
        });
    };
    if !cand.quotable {
        return Some(Regression {
            key: base.key.clone(),
            was: Some(base.value),
            now: None,
            why: format!(
                "was quotable and is no longer: {}",
                cand.why
                    .clone()
                    .unwrap_or_else(|| "no reason recorded".into())
            ),
        });
    }
    let worse = if better_up {
        cand.value < base.value * (1.0 - tolerance)
    } else {
        cand.value > base.value * (1.0 + tolerance) && cand.value - base.value > floor_ms
    };
    if !worse {
        return None;
    }
    let unit = if better_up { "ops/s" } else { "ms" };
    let floor = if !better_up && floor_ms > 0.0 {
        format!(" and the {floor_ms} ms floor")
    } else {
        String::new()
    };
    Some(Regression {
        key: base.key.clone(),
        was: Some(base.value),
        now: Some(cand.value),
        why: format!(
            "{:.1} -> {:.1} {unit}, past the {:.0}% tolerance{floor}",
            base.value,
            cand.value,
            tolerance * 100.0
        ),
    })
}

/// Of two repetitions' regressions of one key, the milder: a timing over a
/// refusal (a run that answered, however slowly, is the better evidence),
/// and of two timings the better one.
fn milder(a: Regression, b: Regression, better_up: bool) -> Regression {
    match (a.now, b.now) {
        (Some(x), Some(y)) if (better_up && y > x) || (!better_up && y < x) => b,
        (None, Some(_)) => b,
        _ => a,
    }
}

/// The keys a BASELINE and a CANDIDATE measured with DIFFERENT bound
/// parameters, as `(key, baseline binding, candidate binding)`.
///
/// A key names a query, not a question: `bi20a` bound to one company and
/// person is a different question from `bi20a` bound to another, and the time
/// of one says nothing about the time of the other. The 2026-09-23 re-derived
/// SF3 parameter file changed bi20's pair and nothing else, and a gate that
/// matched rows by key alone would have scored the new question against the
/// old one's baseline. This is the REGRESSION gate's check, not [`compare`]'s:
/// a cross-engine table legitimately binds each engine its own id space.
///
/// Only rows that both recorded an actual binding (`bound …`) are compared;
/// `skipped`, `exists` and a document predating the field bind nothing.
#[must_use]
pub fn parameter_mismatches(
    baseline: &Comparable,
    candidate: &Comparable,
) -> Vec<(String, String, String)> {
    let bound = |r: &Row| r.probe.clone().filter(|p| p.starts_with("bound "));
    let mut out = Vec::new();
    for base in &baseline.rows {
        let Some(was) = bound(base) else { continue };
        let now = candidate
            .rows
            .iter()
            .find(|r| r.key == base.key)
            .and_then(bound);
        if let Some(now) = now {
            if now != was {
                out.push((base.key.clone(), was, now));
            }
        }
    }
    out
}

/// Check that a set of runs may be compared at all.
///
/// # Errors
/// [`CompareRefusal`] naming what differs. This is the rule the LadybugDB
/// thread-cap episode produced: an engine at its default ran 16 threads against
/// a 6-core quota, and the comparison that followed was between two machines.
/// The [`Rig`] check generalises it from "two engines configured differently on
/// one machine" to "two machines", which is the version that stops being
/// hypothetical the moment a second lane exists.
pub fn compare(runs: &[&Comparable]) -> Result<(), CompareRefusal> {
    let Some(first) = runs.first() else {
        return Err(CompareRefusal::NotEnough);
    };
    if runs.len() < 2 {
        return Err(CompareRefusal::NotEnough);
    }
    // The rig question comes FIRST, and it is asked of every document rather
    // than of the pairs, because "which machine was this" strictly precedes
    // "was it the same catalogue on that machine". A cross-rig pair that also
    // disagreed on the catalogue would otherwise be reported as a catalogue
    // problem, and somebody would go and fix the catalogue.
    for r in runs {
        if r.rig.is_none() {
            return Err(CompareRefusal::RigUnstamped {
                detail: format!("{} ({})", r.engine, r.workload),
            });
        }
        // A stamp that was CHECKED and lost is refused here. An unchecked one
        // (`unobservable`, or a document from before the check existed) is
        // not: refusing it would make the harness unusable off a container,
        // and it is not a false pass — `table` prints the caveat and the
        // document says `unobservable` where a reader will see it. The line
        // this holds is between contradicted and unverified, which are not the
        // same news.
        if r.rig_check.as_deref() == Some("mismatch") {
            return Err(CompareRefusal::RigMismatch {
                detail: format!(
                    "{} ({}) on {}",
                    r.engine,
                    r.workload,
                    r.rig.as_deref().unwrap_or("?")
                ),
            });
        }
        // The same line, one level down: CONTRADICTED is refused, UNVERIFIED
        // is not. `declared` is the ordinary state of an engine that answers
        // no question about itself, and refusing it would make the harness
        // unusable against three of the four engines it exists to compare.
        // `table` prints the caveat, and the document carries the word.
        if r.fairness_check.as_deref() == Some("mismatch") {
            return Err(CompareRefusal::FairnessMismatch {
                detail: format!("{} ({}) with {}", r.engine, r.workload, r.fairness),
            });
        }
    }
    for r in &runs[1..] {
        if r.rig != first.rig {
            return Err(CompareRefusal::Rig {
                detail: format!(
                    "{} on {}, {} on {}",
                    first.engine,
                    first.rig.as_deref().unwrap_or("?"),
                    r.engine,
                    r.rig.as_deref().unwrap_or("?")
                ),
            });
        }
        // Which catalogue text has to match is a question about the family
        // the two runs SHARE, not about every family the binaries happened to
        // carry. Adding `finbench.json` moves no digest an SNB run was
        // measured under, and must therefore refuse nothing.
        //
        // The fallback is the old rule, unchanged, and it is taken whenever
        // the family cannot be established on BOTH sides: a document from
        // before families existed, a workload this binary has no family for,
        // or two documents of different workloads. In each of those the
        // question "was it the same statement text" cannot be answered
        // per-family, and an unanswerable question is refused on the whole
        // file rather than waved through.
        let family = if r.workload == first.workload {
            crate::catalogue::family_for_workload(&first.workload).map(|f| f.name)
        } else {
            None
        };
        match family.and_then(|name| {
            let a = first.catalogue_family_digests.get(name)?;
            let b = r.catalogue_family_digests.get(name)?;
            Some((name, a, b))
        }) {
            Some((name, a, b)) => {
                if !crate::catalogue::same_statements(a, b) {
                    return Err(CompareRefusal::CatalogueFamilyDigest {
                        family: name.to_string(),
                        detail: format!("{} at {}, {} at {}", first.engine, a, r.engine, b),
                    });
                }
            }
            None => {
                if !crate::catalogue::same_statements(&r.catalogue_digest, &first.catalogue_digest) {
                    return Err(CompareRefusal::CatalogueDigest {
                        detail: format!(
                            "{} at {}, {} at {}",
                            first.engine, first.catalogue_digest, r.engine, r.catalogue_digest
                        ),
                    });
                }
            }
        }
        if r.fairness != first.fairness {
            return Err(CompareRefusal::Fairness {
                detail: format!(
                    "{} {}, {} {}",
                    first.engine, first.fairness, r.engine, r.fairness
                ),
            });
        }
        // Only for the stress workload: LSQB is read-only, so the writer
        // count cannot change what it measured.
        if first.workload == "stress" && r.writes_mode != first.writes_mode {
            return Err(CompareRefusal::WritesMode {
                detail: format!(
                    "{} {}, {} {}",
                    first.engine, first.writes_mode, r.engine, r.writes_mode
                ),
            });
        }
    }
    Ok(())
}

/// The rows a run contributes to a comparison table.
#[must_use]
pub fn rows(run: &RunReport) -> Vec<Row> {
    let mut out = Vec::new();
    for lv in &run.levels {
        let max = lv.all_latencies().last().copied().unwrap_or(0);
        let cause = lv.not_quotable(max);
        out.push(Row {
            engine: run.engine.clone(),
            key: lv.profile.clone(),
            clients: lv.clients,
            value: lv.rps(),
            quotable: cause.is_none(),
            why: cause.as_ref().map(NotQuotable::explain),
            cause: cause.as_ref().map(|c| c.code().to_string()),
            count: None,
            probe: None,
        });
    }
    for q in &run.queries {
        out.push(Row {
            engine: run.engine.clone(),
            key: q.query.clone(),
            clients: 1,
            value: q.millis.unwrap_or(f64::NAN),
            quotable: q.status == "ok",
            why: if q.status == "ok" {
                None
            } else {
                Some(format!("status {}", q.status))
            },
            cause: if q.status == "ok" {
                None
            } else {
                Some(format!("query_status_{}", q.status))
            },
            count: q.count,
            probe: Some(q.probe.clone()),
        });
    }
    out
}

/// Render a comparison table across runs, one line per (key, clients).
///
/// A row whose engines disagree on the LSQB COUNT is marked and not scored:
/// two engines answering different questions have no ratio, and printing one
/// is how an adaptation that changed the question becomes a faster number.
#[must_use]
pub fn table(runs: &[&Comparable]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if let Err(e) = compare(runs) {
        let _ = writeln!(out, "REFUSED — {e}");
        return out;
    }
    // Not a refusal, and not silence either. A table built from stamps nobody
    // could check is still a table; it just must not read as one whose labels
    // were verified.
    let unchecked: Vec<&str> = runs
        .iter()
        .filter(|r| !matches!(r.rig_check.as_deref(), Some("verified")))
        .map(|r| r.engine.as_str())
        .collect();
    if !unchecked.is_empty() {
        let _ = writeln!(
            out,
            "NOTE — the rig stamp was not verified against the machine for: {}. The labels \
             are taken on trust here, not checked.",
            unchecked.join(", ")
        );
    }
    // The same note for the fairness block, and it is a SEPARATE line rather
    // than a clause on the one above, because the two are unchecked for
    // different reasons and get fixed in different places: an unverified rig
    // is a machine that says nothing about itself, an unverified fairness
    // block is an ENGINE that says nothing about itself. Folding them would
    // send a reader to the wrong one.
    let unfair: Vec<&str> = runs
        .iter()
        .filter(|r| !matches!(r.fairness_check.as_deref(), Some("verified")))
        .map(|r| r.engine.as_str())
        .collect();
    if !unfair.is_empty() {
        let _ = writeln!(
            out,
            "NOTE — the fairness block was not verified against the engine for: {}. The \
             cache budget and thread cap are claims there, not readings.",
            unfair.join(", ")
        );
    }
    // A THIRD note, and the only one that reads two blocks against each other.
    // The two above each ask whether one block was checked; this one asks
    // whether two checked blocks agree with one another, which is a question
    // nothing else in the pipeline asks. See [`NarrowWidth`].
    for n in narrow_width(runs) {
        let _ = writeln!(
            out,
            "NOTE — {} ran {} thread(s) under a {}-core quota. Both its stamps may be \
             VERIFIED and it is still not a full-machine number: the rig block and the \
             fairness block are each correct about the thing they describe, and between \
             them they describe a narrow engine on a wide machine. If that was the point \
             — a like-for-like against a narrower lane — say so beside the table. If it \
             was not, the width came from a script that outlived the rig it was written \
             for, and the run has to be taken again.",
            n.engine, n.thread_cap, n.quota_cores
        );
    }
    // Columns are POSITIONAL, not keyed on the engine name. Two runs of one
    // engine — the same binary at two worker counts, an A-B-A's two arms — are
    // a comparison people actually make, and a name-keyed lookup silently
    // printed the first run's number in both columns when it was tried.
    let mut labels: Vec<String> = Vec::with_capacity(runs.len());
    for (i, r) in runs.iter().enumerate() {
        let dup = runs.iter().filter(|o| o.engine == r.engine).count() > 1;
        labels.push(if dup {
            format!("{}#{}", r.engine, i + 1)
        } else {
            r.engine.clone()
        });
    }
    let mut keys: Vec<(String, usize)> = Vec::new();
    for r in runs.iter().flat_map(|r| r.rows.iter()) {
        let k = (r.key.clone(), r.clients);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let _ = write!(out, "{:<22}", "workload");
    for e in &labels {
        let _ = write!(out, "{e:>14}");
    }
    let _ = writeln!(out, "  note");
    for (key, clients) in keys {
        let label = if clients > 1 {
            format!("{key}@{clients}")
        } else {
            key.clone()
        };
        let _ = write!(out, "{label:<22}");
        let mut notes: Vec<String> = Vec::new();
        let mut counts: Vec<i64> = Vec::new();
        for (run, name) in runs.iter().zip(labels.iter()) {
            let row = run
                .rows
                .iter()
                .find(|r| r.key == key && r.clients == clients);
            match row {
                None => {
                    let _ = write!(out, "{:>14}", "-");
                }
                Some(r) => {
                    if let Some(c) = r.count {
                        counts.push(c);
                    }
                    if r.quotable {
                        let _ = write!(out, "{:>14.2}", r.value);
                    } else {
                        // An operator error is not a finding, and a table that
                        // spells both `NOT QUOTABLE` makes a sweep of nothing
                        // read exactly like a sweep of something. The cell says
                        // which, so the triage is a glance rather than a read.
                        // Each operator error gets its OWN cell, because they
                        // have different remedies and a shared word sends the
                        // reader to read the note to find out which. A finding
                        // keeps the bare `NOT QUOTABLE`: there the note is the
                        // point.
                        let cell = match r.cause.as_deref() {
                            Some("plan_exhausted") => "PLAN TOO SMALL",
                            Some("too_short_to_judge") => "NOT JUDGED",
                            Some("warmup_ramp") => "WARM-UP",
                            _ => "NOT QUOTABLE",
                        };
                        let _ = write!(out, "{cell:>14}");
                        if let Some(w) = &r.why {
                            notes.push(format!("{name}: {w}"));
                        }
                    }
                }
            }
        }
        if counts.len() > 1 && counts.iter().any(|c| *c != counts[0]) {
            notes.push(format!(
                "COUNTS DIFFER {counts:?} — the engines answered different questions; \
                 this row has no ratio"
            ));
        }
        let _ = writeln!(out, "  {}", notes.join("; "));
    }
    out
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(w_ops: usize, refusals: u64, secs: f64, per_sec: Vec<u64>) -> LevelResult {
        LevelResult {
            profile: "p".into(),
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
            per_shape: BTreeMap::new(),
            plan_exhausted: Vec::new(),
            plan_exhausted_us: Vec::new(),
            plan_ops_per_client: None,
            refusal_kinds: BTreeMap::new(),
            // 2 so rule 5 does not fire on the one-client fixtures; the rule
            // has its own test.
            max_inflight: 2,
            writes_mode: "multi".into(),
        }
    }

    #[test]
    fn a_level_that_acked_nothing_is_not_quotable() {
        // Neo4j's unique-create at 8 clients: 135,383 refusals, 0 acked,
        // printed as 0.00 ops/s next to engram's 2,255 — read as a 660x win.
        let r = level(0, 135_383, 20.0, vec![0; 20]);
        let why = r.not_quotable_because(0).expect("must be refused");
        assert!(why.contains("no operations at all"), "got: {why}");
    }

    #[test]
    fn every_write_refused_is_not_quotable_even_with_reads() {
        let mut r = level(0, 135_383, 20.0, vec![5; 20]);
        r.r_ops = 100;
        let why = r.not_quotable_because(1_000).expect("must be refused");
        assert!(
            why.contains("every write was refused") && why.contains("reset"),
            "got: {why}"
        );
    }

    #[test]
    fn a_stalled_level_is_not_quotable() {
        // engram's contention at 1 client: 11 writes in 20 s with a single
        // operation taking 26.3 SECONDS.
        let r = level(11, 0, 20.0, vec![22, 0, 0, 0, 0, 0, 0, 0]);
        let why = r.not_quotable_because(26_328_557).expect("must be refused");
        assert!(
            why.contains("stalled") && why.contains("26.3"),
            "got: {why}"
        );
    }

    #[test]
    fn a_level_whose_plan_ran_out_is_not_quotable() {
        // Convergence's own refusal: the fastest arm is the one that exhausts
        // a plan, which is precisely the arm a comparison is about.
        let mut r = level(26_583, 0, 20.0, vec![1_300; 20]);
        r.plan_exhausted = vec![3, 7];
        let why = r.not_quotable_because(14_140).expect("must be refused");
        assert!(why.contains("exhausted"), "got: {why}");
        // It used to say "re-emit with a larger --plan-ops", which is a flag
        // this harness has never had. The advice is now a number and the flag
        // is the real one.
        assert!(!why.contains("--plan-ops"), "no such flag: {why}");
        assert!(why.contains("--ops "), "the cure must be nameable: {why}");
    }

    #[test]
    fn an_exhausted_level_names_the_ops_its_own_rate_needed() {
        // THE DEFECT'S SECOND HALF. "Re-emit with a larger plan" is the advice
        // that produces the second undersized plan; the level knows its own
        // rate and can say the number.
        //
        // The dry run's shape, exactly: 4,000 ops per client drained 2.9 s into
        // a 20 s window. 4000/2.9 = 1,380 ops/s, over 20 s with headroom, is
        // 34,500 — above the 30,000 the sizing note derives independently.
        let mut r = level(4_000, 0, 20.0, vec![1_380; 20]);
        r.plan_exhausted = vec![0, 1];
        r.plan_exhausted_us = vec![2_900_000, 3_100_000];
        r.plan_ops_per_client = Some(4_000);
        let need = r.sufficient_plan_ops().expect("a rate was measurable");
        assert!(
            (30_000..=40_000).contains(&need),
            "the advice must come from the measured rate, got {need}"
        );
        let why = r.not_quotable_because(14_140).expect("must be refused");
        assert!(why.contains(&format!("--ops {need}")), "got: {why}");
        assert!(why.contains("2.9s of the 20.0s window"), "got: {why}");
        assert!(why.contains("OPERATOR ERROR"), "got: {why}");

        // The EARLIEST client sets the requirement. Sizing to the average
        // leaves the fastest one running out again on the re-run — which is
        // the same defect with one fewer digit.
        let mut slower = r.clone();
        slower.plan_exhausted_us = vec![2_900_000, 19_000_000];
        assert_eq!(
            slower.sufficient_plan_ops(),
            r.sufficient_plan_ops(),
            "a client that lasted longer must not lower the requirement"
        );

        // A level that ran out with no usable timing does not invent a number.
        let mut blind = r.clone();
        blind.plan_exhausted_us = vec![0];
        assert_eq!(blind.sufficient_plan_ops(), None);
        let why = blind.not_quotable_because(14_140).expect("still refused");
        assert!(why.contains("sizing default"), "got: {why}");
    }

    #[test]
    fn an_operator_error_is_not_reported_as_a_finding() {
        // The triage split. A sweep that measured nothing must not read like a
        // sweep that caught something — that is what costs the re-run, because
        // the two produce the same word in the same column.
        let mut drained = level(4_000, 0, 20.0, vec![1_380; 20]);
        drained.profile = "balanced".into();
        drained.plan_exhausted = vec![0];
        drained.plan_exhausted_us = vec![2_900_000];
        drained.plan_ops_per_client = Some(4_000);
        let c = drained.not_quotable(14_140).expect("refused");
        assert_eq!(c.code(), "plan_exhausted");
        assert_eq!(c.class(), "operator_error");
        assert!(c.is_operator_error());

        // Every other cause is a FINDING: the engine was caught doing
        // something, and the result is the result.
        let stalled = level(11, 0, 20.0, vec![22, 0, 0, 0, 0, 0, 0, 0]);
        let c = stalled.not_quotable(26_328_557).expect("refused");
        assert_eq!(c.code(), "stalled");
        assert_eq!(c.class(), "finding");
        assert!(!c.is_operator_error());

        let mut refused = level(242, 2_158, 20.0, vec![120; 20]);
        refused.profile = "balanced".into();
        let c = refused.not_quotable(1_000).expect("refused");
        assert_eq!(c.code(), "refusal_dominated");
        assert!(!c.is_operator_error());

        // And it reaches the TABLE, which is where the triage happens. One
        // engine drained its plan, the other stalled; the cells must not both
        // say the same thing.
        let mut a = run("engram", 1, 6);
        a.workload = Workload::Stress;
        a.levels.push(drained);
        let mut b = run("neo4j", 1, 6);
        b.workload = Workload::Stress;
        let mut st = stalled;
        st.profile = "balanced".into();
        // The table recomputes the verdict from the level's own latencies, so
        // the stall has to be IN them — a fixture that only passes `max_us` to
        // the rule tests a path the table does not take.
        st.w = vec![26_328_557];
        b.levels.push(st);
        let t = table(&[&Comparable::from(&a), &Comparable::from(&b)]);
        assert!(t.contains("PLAN TOO SMALL"), "got:\n{t}");
        assert!(t.contains("NOT QUOTABLE"), "got:\n{t}");

        // Through real documents, not just structs: the cause survives the
        // round trip, because that is where a caveat goes to be lost.
        let doc = engram_cypher::json::from_json(&a.render()).expect("its own document");
        let Value::Map(m) = doc else {
            panic!("not an object")
        };
        let Some(Value::List(levels)) = m.get("levels") else {
            panic!("no levels")
        };
        let Value::Map(lv) = &levels[0] else {
            panic!("not an object")
        };
        assert_eq!(
            lv.get("not_quotable_cause"),
            Some(&Value::Str("plan_exhausted".into()))
        );
        assert_eq!(
            lv.get("not_quotable_class"),
            Some(&Value::Str("operator_error".into()))
        );
        assert_eq!(lv.get("plan_ops_per_client"), Some(&Value::Int(4_000)));
        let back = parse(&a.render()).expect("parses");
        assert_eq!(back.rows[0].cause.as_deref(), Some("plan_exhausted"));
    }

    #[test]
    fn a_mostly_refused_level_is_not_quotable_even_though_some_writes_landed() {
        // LadybugDB single-writer at K=8: 242 acked against 2,158 refused.
        // `w_ops` is not zero, so the incumbent rule never fires — and the
        // level posts a HIGHER rate than the one that did the work, because a
        // refusal is cheaper than a write.
        let mut r = level(242, 2_158, 20.0, vec![120; 20]);
        r.clients = 8;
        r.refusal_kinds.insert("single-writer".to_string(), 2_158);
        let why = r.not_quotable_because(1_000).expect("must be refused");
        assert!(why.contains("90%") && why.contains("REFUSED"), "got: {why}");
        assert!(
            why.contains("single-writer=2158"),
            "the histogram must name WHICH refusal, got: {why}"
        );
    }

    #[test]
    fn the_two_profiles_whose_result_is_refusal_stay_quotable() {
        // THE OTHER HALF OF RULE 4. Refusing these would delete the two
        // profiles the rule exists to protect.
        for name in REFUSAL_IS_THE_MEASUREMENT {
            let mut r = level(242, 2_158, 20.0, vec![120; 20]);
            r.profile = name.to_string();
            r.clients = 8;
            assert_eq!(
                r.not_quotable_because(1_000),
                None,
                "{name}: refusal IS the measurement here"
            );
        }
    }

    #[test]
    fn a_level_whose_clients_never_overlapped_is_not_quotable() {
        // The harness auditing itself: K clients that never overlapped
        // measured no concurrency, whatever rate they posted.
        let mut r = level(26_583, 0, 20.0, vec![1_300; 20]);
        r.clients = 32;
        r.max_inflight = 1;
        let why = r.not_quotable_because(14_140).expect("must be refused");
        assert!(why.contains("did not measure concurrency"), "got: {why}");
        // One client cannot overlap with itself, so the rule must not fire.
        r.clients = 1;
        assert_eq!(r.not_quotable_because(14_140), None);
    }

    #[test]
    fn max_inflight_counts_overlap_and_never_overstates_it() {
        assert_eq!(max_inflight(&[]), 0);
        assert_eq!(max_inflight(&[(0, 10)]), 1);
        // Strictly sequential: the second starts exactly when the first ends.
        assert_eq!(max_inflight(&[(0, 10), (10, 20), (20, 30)]), 1);
        assert_eq!(max_inflight(&[(0, 10), (5, 15)]), 2);
        assert_eq!(max_inflight(&[(0, 10), (1, 3), (2, 9), (20, 30)]), 3);
    }

    #[test]
    fn a_level_too_short_to_judge_says_so_instead_of_passing_clean() {
        // DEFECT 1, DEMONSTRATED. The DEGRADED and STALLED checks are both
        // written `per_sec.len() > 3`, and a 3-second level has exactly three
        // one-second buckets — so on any level of 3 s or less the two guards
        // did not run and the level reported clean. Worse than inconclusive:
        // `trend` and `floor` return 1.0 below the threshold, which is the
        // value of a PERFECTLY STEADY level, so the row positively asserted
        // health that nothing had checked.
        //
        // A 3 s level whose throughput COLLAPSED to nothing, which the DEGRADED
        // check exists to catch and cannot see:
        let r = level(300, 0, 3.0, vec![300, 0, 0]);
        assert!(
            !r.judged(),
            "three buckets must not be judgeable, or this fixture proves nothing"
        );
        assert_eq!(r.trend(), 1.0, "the statistic reads as PERFECTLY STEADY");
        assert_eq!(r.floor(), 1.0, "and so does the floor");
        assert!(
            r.per_sec.len() <= 3 && r.trend() >= TREND_COLLAPSE && r.floor() >= FLOOR_STALL,
            "neither incumbent guard can fire on this level — that is the defect"
        );

        let c = r.not_quotable(1_000).expect("must now be refused");
        assert_eq!(c.code(), "too_short_to_judge");
        assert_eq!(c.class(), "operator_error");
        let why = c.explain();
        assert!(why.contains("DID NOT RUN"), "got: {why}");
        assert!(why.contains("ABSENCE"), "got: {why}");
        // The fix must NOT be a lowered threshold — the statistic genuinely
        // needs the buckets — so the refusal names the window, not the number.
        assert!(why.contains("re-run with at least 4 seconds"), "got: {why}");

        // The document says it too, in the machine-readable half AND beside the
        // two numbers that would otherwise read as steady.
        let mut rep = run("engram", 1, 6);
        rep.workload = Workload::Stress;
        rep.levels.push(r);
        let doc = engram_cypher::json::from_json(&rep.render()).expect("its own document");
        let Value::Map(m) = doc else {
            panic!("not an object")
        };
        let Some(Value::List(levels)) = m.get("levels") else {
            panic!("no levels")
        };
        let Value::Map(lv) = &levels[0] else {
            panic!("not an object")
        };
        assert_eq!(
            lv.get("not_quotable_cause"),
            Some(&Value::Str("too_short_to_judge".into()))
        );
        assert_eq!(
            lv.get("not_quotable_class"),
            Some(&Value::Str("operator_error".into()))
        );
        assert_eq!(lv.get("trend_floor_judged"), Some(&Value::Bool(false)));
        assert_eq!(lv.get("quotable"), Some(&Value::Bool(false)));

        // And in the TABLE, with its own cell: an unjudged level and a level
        // that was judged and refused are different news, and one word for both
        // is what makes a triage a read instead of a glance.
        let mut other = run("neo4j", 1, 6);
        other.workload = Workload::Stress;
        let t = table(&[&Comparable::from(&rep), &Comparable::from(&other)]);
        assert!(t.contains("NOT JUDGED"), "got:\n{t}");
    }

    #[test]
    fn a_level_long_enough_is_judged_and_the_incumbent_guards_still_fire() {
        // The other half of Defect 1: the refusal must not have eaten the two
        // guards it was added beside. Four buckets is the smallest judgeable
        // level, and a collapse in it is still a collapse.
        let r = level(300, 0, 4.0, vec![300, 300, 0, 0]);
        assert!(r.judged(), "four buckets is the threshold, not above it");
        assert!(
            r.trend() < TREND_COLLAPSE,
            "the DEGRADED check can now see this level: trend {}",
            r.trend()
        );
        // The shape rules did not fire: the level was judged, and judged badly
        // by a guard that lives in the sweep's failure list rather than here.
        // What must NOT happen is `too_short_to_judge` masking it.
        assert_ne!(
            r.not_quotable(1_000).map(|c| c.code().to_string()),
            Some("too_short_to_judge".to_string())
        );
    }

    #[test]
    fn a_short_level_reports_its_real_finding_and_not_the_short_window() {
        // Rule 7 is checked LAST on purpose. A level with actual evidence —
        // no operations, every write refused, a stall — must report THAT, or
        // the fix for a silent guard becomes a louder way to hide findings.
        let empty = level(0, 12, 3.0, vec![0, 0, 0]);
        assert_eq!(
            empty.not_quotable(0).map(|c| c.code().to_string()),
            Some("no_operations".to_string())
        );
        let stalled = level(2, 0, 3.0, vec![2, 0, 0]);
        assert_eq!(
            stalled
                .not_quotable(2_500_000)
                .map(|c| c.code().to_string()),
            Some("stalled".to_string())
        );
        let mut drained = level(4_000, 0, 3.0, vec![1_380, 1_380, 1_380]);
        drained.plan_exhausted = vec![0];
        assert_eq!(
            drained.not_quotable(1_000).map(|c| c.code().to_string()),
            Some("plan_exhausted".to_string())
        );
    }

    #[test]
    fn a_warmed_up_level_is_refused_in_the_direction_nothing_watched() {
        // DEFECT 2, DEMONSTRATED. `trend` is second-half over first-half and
        // the guard fired only BELOW 0.5. A K=1 level was observed at 1.65 —
        // the second half at 165% of the first, because the first half was
        // warming up — and nothing caught it. The mean of such a level averages
        // two regimes and UNDER-reports the engine; it is exactly as unquotable
        // as a collapsing one.
        let r = level(1_060, 0, 8.0, vec![80, 80, 80, 80, 132, 132, 132, 132]);
        assert!(
            (r.trend() - 1.65).abs() < 0.01,
            "the fixture must reproduce the observed ratio, got {}",
            r.trend()
        );
        assert!(
            r.trend() >= TREND_COLLAPSE && r.floor() >= FLOOR_STALL,
            "neither incumbent guard fires on a ramp — that is the defect"
        );
        let c = r.not_quotable(1_000).expect("must now be refused");
        assert_eq!(c.code(), "warmup_ramp");
        assert_eq!(c.class(), "operator_error");
        let why = c.explain();
        assert!(why.contains("165%"), "got: {why}");
        assert!(why.contains("WARM-UP"), "got: {why}");

        // The threshold is NOT the reciprocal of the collapse threshold. 2.0
        // would have passed the level that was actually observed, which is the
        // whole reason this rule exists.
        // A compile-time assertion, because it is a fact about the constants
        // and not about this fixture: 2.0 is the arithmetic reciprocal of the
        // 0.5 collapse threshold, and it would have passed the level that was
        // actually observed. Symmetry in the ratio is the wrong symmetry.
        const _: () = assert!(TREND_WARMUP_REFUSE < 2.0);

        // Its own cell in the table, for the same reason the plan-exhaustion
        // refusal has one: the remedy is different from every other refusal's.
        let mut rep = run("engram", 1, 6);
        rep.workload = Workload::Stress;
        rep.levels.push(r);
        let mut other = run("neo4j", 1, 6);
        other.workload = Workload::Stress;
        let t = table(&[&Comparable::from(&rep), &Comparable::from(&other)]);
        assert!(t.contains("WARM-UP"), "got:\n{t}");
    }

    #[test]
    fn the_warm_up_band_warns_where_it_does_not_refuse() {
        // Some genuine warm-up is expected on the first level of a run, and
        // refusing it would delete the K=1 row every scaling ratio is divided
        // by. So the band between TREND_WARMUP_WARN and TREND_WARMUP_REFUSE is
        // a WARNING and the row stays quotable — but the reader is told,
        // because a reader told nothing assumes nothing happened.
        let warned = level(1_000, 0, 8.0, vec![100, 100, 100, 100, 135, 135, 135, 135]);
        assert!(
            (warned.trend() - 1.35).abs() < 0.01,
            "got {}",
            warned.trend()
        );
        assert_eq!(
            warned.not_quotable_because(1_000),
            None,
            "the warning band must not refuse"
        );
        let note = warned.warm_up_warning().expect("but it must SAY so");
        assert!(note.contains("135%"), "got: {note}");
        assert!(note.contains("scaling ratio"), "got: {note}");

        // Below the warn edge there is nothing to say.
        let steady = level(1_000, 0, 8.0, vec![100, 100, 100, 100, 110, 110, 110, 110]);
        assert_eq!(steady.warm_up_warning(), None, "1.1 is not worth a line");

        // Above the refuse edge the warning stands down: the refusal is the
        // louder statement and printing both would read as two problems.
        let refused = level(1_000, 0, 8.0, vec![100, 100, 100, 100, 200, 200, 200, 200]);
        assert_eq!(refused.warm_up_warning(), None);
        assert!(refused.not_quotable(1_000).is_some());

        // And a level too short to judge warns about nothing, because its trend
        // is not a measurement — it is the hard-coded 1.0.
        let short = level(300, 0, 3.0, vec![100, 100, 100]);
        assert_eq!(short.warm_up_warning(), None);
    }

    #[test]
    fn an_ordinary_level_is_quotable() {
        // THE CANARY. If this starts failing, the rule has widened into
        // refusing real measurements, which would quietly delete the benchmark
        // rather than qualify it.
        let r = level(26_583, 0, 20.0, vec![1_300; 20]);
        assert_eq!(r.not_quotable_because(14_140), None);
        // The  shape: 45,108 winners against 310,966 clean
        // refusals. Rule 4 would refuse this — 87% refused — which is exactly
        // why its exemption is load-bearing rather than a convenience: without
        // it, convergence would have deleted the two profiles whose result IS
        // a refusal rate.
        let mut r = level(45_108, 310_966, 20.0, vec![2_300; 20]);
        r.profile = "unique-create".into();
        assert_eq!(
            r.not_quotable_because(622_082),
            None,
            "refusals alongside acked writes are the profile working, not a fault"
        );
    }

    // ─── The cross-block width check ────────────────────────────────────────

    #[test]
    fn a_narrow_width_under_a_wide_quota_is_noted_even_when_both_stamps_verify() {
        // The exact shape the full-node lane can produce and neither existing
        // check can see: a script carrying the old rig's 6 threads, run in a
        // pod holding the new lane's 44-core quota. The rig block is right
        // about the machine. The fairness block is right about the engine.
        // Between them they describe a six-thread engine on a forty-four-core
        // box, and no rule related them until this one.
        // Built through `render` and `parse` so the figures come off a
        // DOCUMENT, which is where every real one comes from, rather than
        // being set on the struct by the test that then checks them.
        let mut a = run("engram", 1, 6);
        a.rig = Rig::from_spec("bench-ccx63", "sf1").expect("a known rig");
        let mut b = run("neo4j", 1, 6);
        b.rig = Rig::from_spec("bench-ccx63", "sf1").expect("a known rig");
        let mut ca = parse(&a.render()).expect("parses");
        let mut cb = parse(&b.render()).expect("parses");
        assert_eq!(ca.declared_quota_cores, Some(44));
        assert_eq!(ca.declared_thread_cap, Some(6));

        // Both halves stamped as CHECKED AND PASSED, so this cannot be
        // mistaken for the unverified-stamp notes above catching it.
        for c in [&mut ca, &mut cb] {
            c.rig_check = Some("verified".into());
            c.fairness_check = Some("verified".into());
        }

        let found = narrow_width(&[&ca, &cb]);
        assert_eq!(
            found.len(),
            2,
            "6 threads under a 44-core quota must be noted"
        );
        assert_eq!(found[0].thread_cap, 6);
        assert_eq!(found[0].quota_cores, 44);

        // And the comparison itself does NOT refuse — which is the point. The
        // table is built, the row is real, and the note is what stops it being
        // read as a full-machine number.
        assert!(
            compare(&[&ca, &cb]).is_ok(),
            "a narrow arm is still comparable"
        );

        let t = table(&[&ca, &cb]);
        assert!(
            t.contains("ran 6 thread(s) under a 44-core quota"),
            "the note has to reach the table, not just the vec: {t}"
        );
        assert!(
            !t.contains("not verified against"),
            "both stamps verify here; if THAT note fires, this test is passing for \
             the wrong reason: {t}"
        );
    }

    #[test]
    fn a_matched_width_and_a_deliberate_half_width_are_not_noted() {
        // THE CANARY for this rule, and it matters more than usual because the
        // rule is a heuristic rather than a contradiction. A threshold that
        // fires on correct runs stops being read, and the runs it would fire
        // on here are the ones somebody chose deliberately.
        let mut full = Comparable::from(&run("engram", 1, 44));
        full.declared_quota_cores = Some(44);
        full.declared_thread_cap = Some(44);
        assert!(
            narrow_width(&[&full]).is_empty(),
            "44 under 44 is the lane working"
        );

        // Half width is a study, not an accident: it is exactly the arm you
        // run to show what the second half of the machine bought.
        let mut half = Comparable::from(&run("engram", 1, 22));
        half.declared_quota_cores = Some(44);
        half.declared_thread_cap = Some(22);
        assert!(
            narrow_width(&[&half]).is_empty(),
            "22 under 44 is deliberate"
        );

        // And the old lane, where 6 threads under a 6-core quota is simply
        // right. This is the case a naive "is the width small?" rule would
        // have flagged on every historical document in the repository.
        let mut pod = Comparable::from(&run("engram", 1, 6));
        pod.declared_quota_cores = Some(6);
        pod.declared_thread_cap = Some(6);
        assert!(
            narrow_width(&[&pod]).is_empty(),
            "6 under 6 is the pod lane"
        );
    }

    #[test]
    fn an_absent_quota_is_absent_and_not_a_pass_or_a_note() {
        // Two different absences, and neither may be guessed at.
        //
        // A whole-machine lane declares `cpu_quota_cores: None`, meaning no
        // quota applied — there is no ceiling for a width to fall short of, so
        // the question does not arise. A pre-rig document has no figure at
        // all. Both must be silent rather than noted, for the reason
        // `ObservedMachine::cpu_quota_readable` exists: "no quota" and "could
        // not tell" are different facts, and neither is a finding.
        let mut no_quota = Comparable::from(&run("engram", 1, 4));
        no_quota.declared_quota_cores = None;
        no_quota.declared_thread_cap = Some(4);
        assert!(narrow_width(&[&no_quota]).is_empty());

        let mut no_stamp = Comparable::from(&run("engram", 1, 6));
        no_stamp.declared_quota_cores = None;
        no_stamp.declared_thread_cap = None;
        assert!(narrow_width(&[&no_stamp]).is_empty());
    }

    #[test]
    fn the_numeric_halves_survive_a_round_trip_through_a_document() {
        // The check reads figures that `parse` has to recover from JSON. A
        // vec built in-process proves nothing about a document read off the
        // durable volume, which is where every real one comes from.
        let mut r = run("engram", 1, 6);
        r.rig = Rig::from_spec("bench-ccx63", "sf1").expect("a known rig");
        let back = parse(&r.render()).expect("round trip");
        assert_eq!(back.declared_thread_cap, Some(6));
        assert_eq!(
            back.declared_quota_cores,
            Some(44),
            "the quota has to come back off the document, not off the struct"
        );
        assert_eq!(narrow_width(&[&back]).len(), 1);
    }

    #[test]
    fn the_bench_pod_lane_is_a_third_rig_and_not_the_first_one_renamed() {
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("a known rig");
        let bench_pod = Rig::from_spec("bench-pod-6cpu", "sf1").expect("a known rig");

        // Same quota, different machine. That is the whole point: the reason
        // the move off the control-plane node ends a series rather than
        // continuing it.
        assert_eq!(pod.cpu_quota_cores, bench_pod.cpu_quota_cores);
        assert_eq!(pod.mem_limit_mb, bench_pod.mem_limit_mb);
        assert_ne!(pod.node_cores, bench_pod.node_cores);
        assert_eq!(bench_pod.node_cores, 48);

        // And the reporter must refuse a row across them, or the distinction
        // is decoration.
        let mut a = Comparable::from(&run("engram", 1, 6));
        a.rig = Some(pod.to_json());
        let mut b = Comparable::from(&run("neo4j", 1, 6));
        b.rig = Some(bench_pod.to_json());
        assert!(
            matches!(compare(&[&a, &b]), Err(CompareRefusal::Rig { .. })),
            "a 6-core pod on the control plane and a 6-core pod on the bench node \
             are different rigs"
        );
    }

    fn run(engine: &str, digest: u64, threads: u32) -> RunReport {
        RunReport {
            workload: Workload::Lsqb,
            engine: engine.into(),
            engine_version: "x".into(),
            dialect: "cypher".into(),
            addr: "a".into(),
            dataset: "snb".into(),
            corpus: "sf1".into(),
            seed: 1,
            keys: 10,
            catalogue_digest: digest,
            plan_sha256: None,
            plan_emitter: None,
            op_source: "live".into(),
            writes_mode: "multi".into(),
            rig: Rig::from_spec("main-pod-6cpu", "sf1").expect("a known rig"),
            // The fixtures declare a rig and observe nothing, which is the
            // developer-workstation case: unchecked, and saying so.
            rig_check: RigCheck::not_observed(),
            fairness: Fairness {
                thread_cap: threads,
                cache_budget_mb: 8192,
                clients: 1,
                seconds: 20,
            },
            // As with `rig_check`: the fixtures declare and check nothing,
            // which is the position an engine that answers no question about
            // itself leaves a run in.
            fairness_check: crate::fairness::FairnessCheck::not_checked(),
            levels: Vec::new(),
            queries: Vec::new(),
            integrity: Vec::new(),
            failures: Vec::new(),
        }
    }

    #[test]
    fn a_family_digest_decides_the_refusal_and_names_the_family() {
        // Two runs of the same workload that carry per-family digests are
        // judged on the family they share. Equal there, they compare — even
        // though `catalogue_digest` differs, which is exactly the case a new
        // family arriving in a new file used to produce.
        let mut a = Comparable::from(&run("engram", 1, 6));
        let mut b = Comparable::from(&run("neo4j", 2, 6));
        assert_ne!(a.catalogue_digest, b.catalogue_digest);
        assert!(
            compare(&[&a, &b]).is_ok(),
            "the shared family agrees, so the whole-file digest is not the question"
        );

        // Disagree on the shared family and the refusal names it. The guard
        // is not weakened by families; it is aimed.
        b.catalogue_family_digests
            .insert("lsqb-stress".to_string(), "dead0000dead0000".to_string());
        match compare(&[&a, &b]) {
            Err(CompareRefusal::CatalogueFamilyDigest { family, .. }) => {
                assert_eq!(family, "lsqb-stress");
            }
            other => panic!("expected a family refusal, got {other:?}"),
        }

        // One side missing the family falls back to the whole file rather
        // than passing: it cannot PROVE it ran the same text.
        a.catalogue_family_digests.clear();
        assert!(matches!(
            compare(&[&a, &b]),
            Err(CompareRefusal::CatalogueDigest { .. })
        ));
    }

    #[test]
    fn a_comparison_refuses_a_different_catalogue_or_a_different_machine() {
        let a = run("engram", 1, 6);
        let b = run("neo4j", 1, 6);
        let (ca, cb) = (Comparable::from(&a), Comparable::from(&b));
        assert!(compare(&[&ca, &cb]).is_ok());
        // A document with no per-family map is a document written before
        // families existed, and it is compared exactly as it always was: on
        // the whole file. Both sides are cleared, because the fallback is
        // taken when the family cannot be established on BOTH.
        let mut c = Comparable::from(&run("ladybug", 2, 6));
        c.catalogue_family_digests.clear();
        let mut ca_old = ca.clone();
        ca_old.catalogue_family_digests.clear();
        assert!(matches!(
            compare(&[&ca_old, &c]),
            Err(CompareRefusal::CatalogueDigest { .. })
        ));
        // The LadybugDB episode: same catalogue, 16 threads against a 6-core
        // quota. A different machine, not a different engine.
        let d = Comparable::from(&run("ladybug", 1, 16));
        assert!(matches!(
            compare(&[&ca, &d]),
            Err(CompareRefusal::Fairness { .. })
        ));
        assert!(matches!(compare(&[&ca]), Err(CompareRefusal::NotEnough)));
        assert!(matches!(compare(&[]), Err(CompareRefusal::NotEnough)));
        // Single-writer against multi-writer is the same plan producing two
        // categorically different workloads.
        let mut s1 = run("engram", 1, 6);
        let mut s2 = run("ladybug", 1, 6);
        s1.workload = Workload::Stress;
        s2.workload = Workload::Stress;
        s2.writes_mode = "single".into();
        let (cs1, cs2) = (Comparable::from(&s1), Comparable::from(&s2));
        assert!(matches!(
            compare(&[&cs1, &cs2]),
            Err(CompareRefusal::WritesMode { .. })
        ));
        // LSQB is read-only, so the writer count cannot have changed it.
        let mut l2 = run("ladybug", 1, 6);
        l2.writes_mode = "single".into();
        let cl2 = Comparable::from(&l2);
        assert!(compare(&[&ca, &cl2]).is_ok());
    }

    // ─── The fairness guard ─────────────────────────────────────────────────

    #[test]
    fn a_fairness_stamp_the_engine_contradicted_cannot_enter_a_table() {
        use crate::fairness::{EngineFairness, FairnessCheck, Figure};

        // Two arms that AGREE on the block, which is all `compare` could ever
        // see before. This is the state the Neo4j window's documents were in.
        let a = run("engram", 1, 6);
        let mut b = run("neo4j", 1, 6);
        let (ca, cb) = (Comparable::from(&a), Comparable::from(&b));
        assert!(
            compare(&[&ca, &cb]).is_ok(),
            "an unchecked stamp is not refused — only a contradicted one is"
        );

        // Now the engine is asked, and says 10 GiB against the block's 8192.
        b.fairness_check = FairnessCheck::of(
            &b.fairness,
            EngineFairness {
                cache_budget_mb: Figure::observed(
                    10240,
                    "neo4j: dbms.listConfig(server.memory.pagecache.size) = `10737418240`",
                ),
                thread_cap: Figure::declared("declared: no such setting in Community"),
            },
        );
        let cb = Comparable::from(&b);
        let refusal = compare(&[&ca, &cb]).expect_err("a contradicted stamp must be refused");
        assert!(
            matches!(refusal, CompareRefusal::FairnessMismatch { .. }),
            "got: {refusal:?}"
        );
        // And the refusal must survive the round trip through a document —
        // the reporter reads these off disk, never out of this process.
        let doc = b.render();
        let back = parse(&doc).expect("the document must parse");
        assert_eq!(back.fairness_check.as_deref(), Some("mismatch"));
        assert!(matches!(
            compare(&[&ca, &back]),
            Err(CompareRefusal::FairnessMismatch { .. })
        ));

        // A document from before the check existed carries no block, and that
        // is absent rather than verified: readable, comparable, and CALLED
        // OUT in the table's caveat instead of passing silently.
        let mut old = Comparable::from(&run("ladybug", 1, 6));
        old.fairness_check = None;
        assert!(compare(&[&ca, &old]).is_ok());
        let t = table(&[&ca, &old]);
        assert!(
            t.contains("the fairness block was not verified against the engine"),
            "an unverified block must be said out loud: {t}"
        );
    }

    // ─── The rig guard ──────────────────────────────────────────────────────

    #[test]
    fn a_rig_resolves_from_the_registry_and_refuses_everything_it_cannot_know() {
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("a known rig");
        assert_eq!(pod.node_cores, 16);
        assert_eq!(pod.cpu_quota_cores, Some(6));
        assert_eq!(pod.scale, "sf1");
        let bench = Rig::from_spec("bench-ccx63", "sf10").expect("a known rig");
        assert_eq!(bench.node_cores, 48);
        // This asserted `None` with the rationale "the bench lane's defining
        // property is that NOTHING throttles it". Both halves were wrong, and a
        // review caught it: `fullnode-engram-pod.yaml` sets `limits: cpu: "44"`,
        // which IS a cgroup quota, so `None` described a pod that does not
        // exist. The lane's defining property is not the absence of a quota — it
        // is that ONE engine gets the whole machine, uncontended. A 44-core
        // quota on a 48-core box running nothing else is still a quota; it
        // simply never binds, because the engine is given 44 threads to match.
        //
        // The distinction matters here more than it would elsewhere: this
        // registry exists so a recorded number can be checked against the
        // machine that produced it, and a row nothing ran on fails at exactly
        // that job.
        assert_eq!(
            bench.cpu_quota_cores,
            Some(44),
            "the bench rig must describe the pod that runs, and it is Guaranteed at 44 cores"
        );
        assert!(
            bench.cpu_quota_cores.unwrap() < bench.node_cores,
            "the quota sits below the box on purpose — the remainder is kubelet reserve \
             and deliberate insulation, and a rig claiming the whole 48 would overstate it"
        );

        // An unknown name names the known ones rather than guessing.
        let e = Rig::from_spec("the-fast-one", "sf1").expect_err("must not guess");
        assert!(
            e.contains("main-pod-6cpu") && e.contains("bench-ccx63"),
            "got: {e}"
        );

        // A machine outside the estate is describable, but only in full.
        let laptop = Rig::from_spec("laptop:m3max:14:none:36864", "sf1").expect("inline");
        assert_eq!(laptop.node_cores, 14);
        assert_eq!(laptop.mem_limit_mb, 36_864);
        for bad in [
            "laptop:m3max:14:none",        // a field short
            "laptop:m3max:14:none:36864:", // a field long
            "laptop:m3max:0:none:36864",   // zero cores
            "laptop:m3max:14:none:zero",   // unparseable memory
            "laptop:m3max:14:99:36864",    // a quota larger than the machine
        ] {
            assert!(
                Rig::from_spec(bad, "sf1").is_err(),
                "`{bad}` must not resolve to a rig"
            );
        }

        // THE WORST CASE, refused by name: an inline rig wearing a known rig's
        // name with different numbers would compare cleanly against the real
        // one and be a different machine.
        let e = Rig::from_spec("bench-ccx63:ccx63:8:none:8192", "sf1")
            .expect_err("shadowing a known rig must be refused");
        assert!(e.contains("is a known rig"), "got: {e}");

        // A scale that would not survive being compared as text.
        assert!(Rig::from_spec("main-pod-6cpu", "sf1 ").is_err());
        assert!(Rig::from_spec("main-pod-6cpu", "").is_err());
    }

    #[test]
    fn a_rig_renders_the_same_whether_it_came_from_memory_or_from_disk() {
        // The property the whole guard rests on: text equality means machine
        // equality. Two renderers would eventually disagree about a space and
        // a run would refuse itself.
        let r = run("engram", 1, 6);
        let in_process = Comparable::from(&r);
        let round_tripped = parse(&r.render()).expect("its own document parses");
        assert_eq!(
            in_process.rig, round_tripped.rig,
            "a run and its own emitted document must carry byte-identical rig text"
        );
        assert!(
            in_process
                .rig
                .as_deref()
                .unwrap_or("")
                .contains("main-pod-6cpu"),
            "got: {:?}",
            in_process.rig
        );
    }

    #[test]
    fn the_reporter_refuses_a_table_across_two_rigs_and_builds_one_within_a_rig() {
        // THE GUARD, exercised end to end through real documents rather than
        // through the structs: two runs are rendered, read back by `parse`,
        // and handed to the reporter exactly as `harness report a.json b.json`
        // hands them over.
        let mut pod = run("engram", 1, 6);
        let mut bench = run("engram", 1, 6);
        bench.rig = Rig::from_spec("bench-ccx63", "sf1").expect("a known rig");

        let (pd, bd) = (
            parse(&pod.render()).expect("parses"),
            parse(&bench.render()).expect("parses"),
        );
        let refusal = compare(&[&pd, &bd]).expect_err("two rigs is not a comparison");
        assert!(
            matches!(refusal, CompareRefusal::Rig { .. }),
            "got: {refusal:?}"
        );
        // The refusal has to be readable where it lands, which is the top of
        // the table somebody redirected into a file.
        let printed = table(&[&pd, &bd]);
        assert!(printed.starts_with("REFUSED"), "got: {printed}");
        assert!(
            printed.contains("main-pod-6cpu") && printed.contains("bench-ccx63"),
            "the refusal must name both lanes, got: {printed}"
        );

        // The other half, and the one that would be quietly deleted if the
        // rule widened: two runs on the SAME rig still compare.
        let other = run("neo4j", 1, 6);
        let od = parse(&other.render()).expect("parses");
        compare(&[&pd, &od]).expect("one rig, two engines, is exactly the table");

        // A scale change alone is a rig change. `corpus` was recorded for
        // years and never compared, so an SF1 number next to an SF10 one used
        // to print a ratio.
        pod.rig = Rig::from_spec("main-pod-6cpu", "sf1").expect("a known rig");
        let mut ten = run("engram", 1, 6);
        ten.rig = Rig::from_spec("main-pod-6cpu", "sf10").expect("a known rig");
        let (p1, p10) = (
            parse(&pod.render()).expect("parses"),
            parse(&ten.render()).expect("parses"),
        );
        assert!(
            matches!(compare(&[&p1, &p10]), Err(CompareRefusal::Rig { .. })),
            "sf1 against sf10 is not one measurement"
        );
    }

    #[test]
    fn a_document_from_before_the_stamp_is_readable_and_uncomparable() {
        // Every result committed before the rig existed is in this position.
        // It must still PARSE — it is evidence, not garbage — and it must not
        // enter a table, because nothing in it says which machine it came
        // from.
        // Built by REMOVING the key from a real document rather than by
        // hand-writing a fixture, so the shape stays the shape the emitter
        // actually produces.
        let without_rig = |doc: &str, replacement: Option<Value>| -> String {
            let Ok(Value::Map(mut m)) = engram_cypher::json::from_json(doc) else {
                panic!("the emitter produced something that is not an object")
            };
            m.remove("rig");
            if let Some(v) = replacement {
                m.insert("rig".to_string(), v);
            }
            to_json(&Value::Map(m))
        };
        let stripped = without_rig(&run("engram", 1, 6).render(), None);
        assert!(
            !stripped.contains("\"rig\""),
            "the fixture must have no rig"
        );
        let old = parse(&stripped).expect("an old document is readable, not malformed");
        assert_eq!(old.rig, None);
        let fresh = parse(&run("neo4j", 1, 6).render()).expect("parses");
        let refusal = compare(&[&old, &fresh]).expect_err("an unstamped run cannot be compared");
        assert!(
            matches!(refusal, CompareRefusal::RigUnstamped { .. }),
            "got: {refusal:?}"
        );
        // And it says what the remedy is NOT, because the obvious move is to
        // type a rig in and re-run the reporter.
        let text = refusal.to_string();
        assert!(text.contains("reconstruction"), "got: {text}");

        // A `null` rig is absence, not a value: a skeleton with the key
        // present must not slip past the check by comparing as the text
        // "null" against another skeleton.
        let nulled = without_rig(&run("engram", 1, 6).render(), Some(Value::Null));
        assert_eq!(parse(&nulled).expect("parses").rig, None);
    }

    /// A machine that reads back exactly like the real bench pod did on
    /// 2026-09-09: `cpu.max` `600000 100000`, `memory.max` 42949672960,
    /// `/sys/devices/system/cpu/online` `0-15`.
    fn pod_machine() -> ObservedMachine {
        ObservedMachine {
            cpu_quota_cores: Some(6.0),
            cpu_quota_readable: true,
            cpu_quota_source: "cgroup-v2:cpu.max".into(),
            mem_limit_mb: Some(40 * 1024),
            mem_limit_source: "cgroup-v2:memory.max".into(),
            node_cores: Some(16),
            node_cores_source: "sysfs:devices/system/cpu/online".into(),
        }
    }

    /// The dedicated bench node's pod: Guaranteed at 44 cores on a 48-core box.
    fn bench_machine() -> ObservedMachine {
        ObservedMachine {
            cpu_quota_cores: Some(44.0),
            cpu_quota_readable: true,
            cpu_quota_source: "cgroup-v2:cpu.max".into(),
            mem_limit_mb: Some(160 * 1024),
            mem_limit_source: "cgroup-v2:memory.max".into(),
            node_cores: Some(48),
            node_cores_source: "sysfs:devices/system/cpu/online".into(),
        }
    }

    #[test]
    fn a_rig_that_describes_the_machine_it_ran_on_verifies() {
        // Both lanes, against the facts they were measured from. A guard that
        // fired on a CORRECT stamp would be turned off within a week, so the
        // agreeing case is asserted as hard as the disagreeing one.
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("known");
        let c = RigCheck::of(&pod, pod_machine());
        assert_eq!(c.status, RigStatus::Verified, "{:?}", c.disagreements);
        assert!(c.disagreements.is_empty());
        assert_eq!(c.agreements.len(), 3, "all three facts checked: {c:?}");

        let bench = Rig::from_spec("bench-ccx63", "sf1").expect("known");
        let c = RigCheck::of(&bench, bench_machine());
        assert_eq!(c.status, RigStatus::Verified, "{:?}", c.disagreements);

        // The registry's `Some(44)` exists precisely so this passes: a
        // Guaranteed pod on the dedicated node STILL has a quota, and a rig
        // that said `none` there would be caught by its own guard.
        assert!(
            bench.cpu_quota_cores.is_some(),
            "the bench lane declares a quota because the pod really has one"
        );
    }

    #[test]
    fn the_stamp_the_dry_run_could_not_catch_is_caught() {
        // THE HOLE, made to fail. "I stamped two different pods identically
        // and nothing objected." Both directions, because both produce a
        // number the label cannot support.
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("known");
        let bench = Rig::from_spec("bench-ccx63", "sf1").expect("known");

        // The pod lane's label on the dedicated node — a 48-core number
        // wearing a 6-core lane's name, which is the one that reads as an
        // engine that got several times faster on a day nobody changed it.
        let c = RigCheck::of(&pod, bench_machine());
        assert_eq!(c.status, RigStatus::Mismatch);
        let said = c.disagreements.join(" | ");
        assert!(
            said.contains("6-core CPU quota") && said.contains("44-core"),
            "got: {said}"
        );
        assert!(
            said.contains("16-core machine") && said.contains("48 core"),
            "got: {said}"
        );
        assert!(
            said.contains("40960 MiB") && said.contains("163840 MiB"),
            "got: {said}"
        );

        // And the other way: the bench lane's label on the pod.
        let c = RigCheck::of(&bench, pod_machine());
        assert_eq!(c.status, RigStatus::Mismatch);
        assert!(c.describe().contains("DISAGREES"), "got: {}", c.describe());

        // An UNCAPPED declaration on a capped machine, and the reverse. These
        // are the two shapes an inline rig gets wrong, and neither is a
        // difference of degree the tolerance could swallow.
        let uncapped = Rig::from_spec("box:ccx63:48:none:163840", "sf1").expect("inline");
        let c = RigCheck::of(&uncapped, bench_machine());
        assert_eq!(c.status, RigStatus::Mismatch);
        assert!(
            c.disagreements
                .iter()
                .any(|d| d.contains("declares NO CPU quota")),
            "got: {:?}",
            c.disagreements
        );
        let capped = Rig::from_spec("box:ccx63:48:44:163840", "sf1").expect("inline");
        let mut no_quota = bench_machine();
        no_quota.cpu_quota_cores = None;
        no_quota.cpu_quota_readable = true;
        no_quota.cpu_quota_source = "cgroup-v2:cpu.max: no quota".into();
        let c = RigCheck::of(&capped, no_quota);
        assert_eq!(c.status, RigStatus::Mismatch);
        assert!(
            c.disagreements
                .iter()
                .any(|d| d.contains("running under NO quota")),
            "got: {:?}",
            c.disagreements
        );
    }

    #[test]
    fn a_machine_that_says_nothing_is_unchecked_and_never_a_pass() {
        // The workstation. Rust on Windows reads no cgroup at all, and the
        // requirement is that this degrade to "could not observe" — the one
        // outcome that must NOT happen is a green light.
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("known");
        let c = RigCheck::of(&pod, ObservedMachine::unobservable("unobservable: no /sys"));
        assert_eq!(c.status, RigStatus::Unobservable);
        assert_ne!(c.status, RigStatus::Verified, "silence is not agreement");
        assert!(
            c.disagreements.is_empty(),
            "nothing was contradicted either"
        );
        assert!(
            c.describe().contains("NOT verified"),
            "got: {}",
            c.describe()
        );
        assert_eq!(c.unreadable().len(), 3);

        // Half a machine: the quota is readable, the core count is not. That
        // is `partial` — one fact earned, the rest still on trust — and it is
        // reported as neither of the other three.
        let mut half = pod_machine();
        half.node_cores = None;
        half.node_cores_source = "unobservable: no cpu online list".into();
        let c = RigCheck::of(&pod, half);
        assert_eq!(c.status, RigStatus::Partial);
        assert!(
            c.describe().contains("could not be read"),
            "got: {}",
            c.describe()
        );
    }

    #[test]
    fn a_contradicted_stamp_cannot_enter_a_table_and_an_unchecked_one_is_flagged() {
        // The refusal, through real documents, exactly as `harness report`
        // hands them over. A wrong stamp is worse than an absent one because
        // it COMPARES — so `compare` has to see it, not just the harness.
        let mut wrong = run("engram", 1, 6);
        wrong.rig_check = RigCheck::of(&wrong.rig.clone(), bench_machine());
        assert_eq!(wrong.rig_check.status, RigStatus::Mismatch);
        let ok = run("neo4j", 1, 6);

        let (w, o) = (
            parse(&wrong.render()).expect("parses"),
            parse(&ok.render()).expect("parses"),
        );
        assert_eq!(w.rig_check.as_deref(), Some("mismatch"));
        let refusal = compare(&[&w, &o]).expect_err("a contradicted stamp is not comparable");
        assert!(
            matches!(refusal, CompareRefusal::RigMismatch { .. }),
            "got: {refusal:?}"
        );
        assert!(
            table(&[&w, &o]).starts_with("REFUSED"),
            "the refusal must land at the top"
        );

        // An UNCHECKED stamp still compares — refusing it would make the
        // harness unusable off a container — but the table says so, because a
        // caveat that is not printed is a caveat that is not read.
        let t = table(&[&o, &parse(&run("ladybug", 1, 6).render()).expect("parses")]);
        assert!(
            t.starts_with("NOTE — the rig stamp was not verified"),
            "got:\n{t}"
        );

        // A verified pair carries no note at all — and "verified" now means
        // BOTH stamps, the machine and the engine's configuration on it. The
        // fairness half was added after this test was written, and the honest
        // repair is to check it here too rather than to relax the assertion:
        // an engine nobody asked is exactly the caveat the note exists for.
        let verified = |engine: &str| {
            let mut r = run(engine, 1, 6);
            r.rig_check = RigCheck::of(&r.rig.clone(), pod_machine());
            assert_eq!(r.rig_check.status, RigStatus::Verified);
            r.fairness_check = crate::fairness::FairnessCheck::of(
                &r.fairness,
                crate::fairness::EngineFairness {
                    cache_budget_mb: crate::fairness::Figure::observed(8192, "fixture"),
                    thread_cap: crate::fairness::Figure::observed(6, "fixture"),
                },
            );
            assert_eq!(
                r.fairness_check.status,
                crate::fairness::FairnessStatus::Verified
            );
            parse(&r.render()).expect("parses")
        };
        let (a, b) = (verified("engram"), verified("neo4j"));
        let t = table(&[&a, &b]);
        assert!(!t.contains("NOTE —"), "got:\n{t}");
        assert!(!t.starts_with("REFUSED"), "got:\n{t}");
    }

    #[test]
    fn the_observation_reads_a_real_cgroup_layout_and_says_where_it_came_from() {
        // The READER, not just the rule. Fixture directories shaped like the
        // bench pod's own /sys and /proc — the numbers are the ones read off
        // engram benchmark pod on 2026-09-09, not invented.
        let dir = std::env::temp_dir().join(format!("engram-rig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (sys, procfs) = (dir.join("sys"), dir.join("proc"));
        std::fs::create_dir_all(sys.join("fs/cgroup")).expect("mkdir");
        std::fs::create_dir_all(sys.join("devices/system/cpu")).expect("mkdir");
        std::fs::create_dir_all(&procfs).expect("mkdir");
        std::fs::write(sys.join("fs/cgroup/cpu.max"), "600000 100000\n").expect("w");
        std::fs::write(sys.join("fs/cgroup/memory.max"), "42949672960\n").expect("w");
        std::fs::write(sys.join("devices/system/cpu/online"), "0-15\n").expect("w");
        std::fs::write(procfs.join("self/../cgroup"), "0::/\n").ok();
        std::fs::write(procfs.join("meminfo"), "MemTotal:       64295972 kB\n").expect("w");

        let (sys_s, proc_s) = (sys.display().to_string(), procfs.display().to_string());
        let m = observe_machine_at(&sys_s, &proc_s, true);
        assert_eq!(m.cpu_quota_cores, Some(6.0), "{m:?}");
        assert_eq!(m.mem_limit_mb, Some(40 * 1024), "{m:?}");
        assert_eq!(m.node_cores, Some(16), "{m:?}");
        // A fixture read is STAMPED as one, so it cannot forge a machine
        // observation in a committed document.
        assert!(m.cpu_quota_source.starts_with("override:"), "{m:?}");

        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("known");
        assert_eq!(RigCheck::of(&pod, m).status, RigStatus::Verified);

        // No quota is a READING, not an absence: `max` means the process had
        // the machine, and the memory ceiling falls back to MemTotal.
        std::fs::write(sys.join("fs/cgroup/cpu.max"), "max 100000\n").expect("w");
        std::fs::write(sys.join("fs/cgroup/memory.max"), "max\n").expect("w");
        let m = observe_machine_at(&sys_s, &proc_s, true);
        assert_eq!(m.cpu_quota_cores, None);
        assert!(
            m.cpu_quota_readable,
            "`max` is a reading: the process had the machine"
        );
        assert!(m.cpu_quota_source.contains("no quota"), "{m:?}");
        assert_eq!(m.mem_limit_mb, Some(64_295_972 / 1024));
        // ... and against the pod's declaration that is a MISMATCH, which is
        // the whole 6-core-stamp-on-an-uncapped-box case.
        assert_eq!(RigCheck::of(&pod, m).status, RigStatus::Mismatch);

        // An empty tree observes nothing rather than guessing.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).expect("mkdir");
        let e = empty.display().to_string();
        let m = observe_machine_at(&e, &e, true);
        assert_eq!(m.cpu_quota_cores, None);
        // The distinction the first version of this guard got wrong: an
        // unreadable quota and a quota of none are both `None`, and treating
        // the first as the second reported a MISMATCH on a machine that was
        // contradicting nothing.
        assert!(
            !m.cpu_quota_readable,
            "nothing was read, so nothing is known"
        );
        assert_eq!(m.mem_limit_mb, None);
        assert_eq!(m.node_cores, None);
        assert!(!m.observed_anything());
        assert_eq!(
            RigCheck::of(&pod, m).status,
            RigStatus::Unobservable,
            "an unreadable machine is unchecked, never verified"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_v1_and_nested_cgroup_fallbacks_are_read_rather_than_assumed() {
        // Two branches that a v2, own-namespace container never reaches, and
        // which were therefore about to ship never having run. An untested
        // fallback is a guard nobody has seen fail.
        let dir = std::env::temp_dir().join(format!("engram-rig-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (sys, procfs) = (dir.join("sys"), dir.join("proc"));
        std::fs::create_dir_all(sys.join("fs/cgroup/cpu")).expect("mkdir");
        std::fs::create_dir_all(sys.join("fs/cgroup/memory")).expect("mkdir");
        std::fs::create_dir_all(sys.join("devices/system/cpu")).expect("mkdir");
        std::fs::create_dir_all(procfs.join("self")).expect("mkdir");
        std::fs::write(sys.join("fs/cgroup/cpu/cpu.cfs_quota_us"), "600000\n").expect("w");
        std::fs::write(sys.join("fs/cgroup/cpu/cpu.cfs_period_us"), "100000\n").expect("w");
        std::fs::write(
            sys.join("fs/cgroup/memory/memory.limit_in_bytes"),
            "42949672960\n",
        )
        .expect("w");
        std::fs::write(sys.join("devices/system/cpu/online"), "0-15\n").expect("w");
        std::fs::write(procfs.join("meminfo"), "MemTotal:       64295972 kB\n").expect("w");
        let (sys_s, proc_s) = (sys.display().to_string(), procfs.display().to_string());

        let m = observe_machine_at(&sys_s, &proc_s, true);
        assert_eq!(m.cpu_quota_cores, Some(6.0), "{m:?}");
        assert!(m.cpu_quota_source.contains("cgroup-v1"), "{m:?}");
        assert_eq!(m.mem_limit_mb, Some(40 * 1024), "{m:?}");
        let pod = Rig::from_spec("main-pod-6cpu", "sf1").expect("known");
        assert_eq!(RigCheck::of(&pod, m).status, RigStatus::Verified);

        // v1's no-limit sentinel is a very large number, not a word. Read as a
        // ceiling it would be eight exabytes and would contradict every rig.
        std::fs::write(sys.join("fs/cgroup/cpu/cpu.cfs_quota_us"), "-1\n").expect("w");
        std::fs::write(
            sys.join("fs/cgroup/memory/memory.limit_in_bytes"),
            "9223372036854771712\n",
        )
        .expect("w");
        let m = observe_machine_at(&sys_s, &proc_s, true);
        assert_eq!(m.cpu_quota_cores, None);
        assert!(m.cpu_quota_readable, "-1 is a reading: no quota applied");
        assert_eq!(
            m.mem_limit_mb,
            Some(64_295_972 / 1024),
            "the sentinel must fall through to MemTotal, not become a ceiling"
        );

        // ── The nested form ────────────────────────────────────────────────
        // A process NOT in its own cgroup namespace sees the host tree, and
        // `/sys/fs/cgroup/cpu.max` is the ROOT cgroup's — which is `max`, i.e.
        // "no quota", on a machine where the process is very much capped. The
        // path from /proc/self/cgroup is what has to be followed.
        let dir2 = dir.join("nested");
        let (sys2, proc2) = (dir2.join("sys"), dir2.join("proc"));
        std::fs::create_dir_all(sys2.join("fs/cgroup/kubepods/podabc")).expect("mkdir");
        std::fs::create_dir_all(sys2.join("devices/system/cpu")).expect("mkdir");
        std::fs::create_dir_all(proc2.join("self")).expect("mkdir");
        std::fs::write(
            sys2.join("fs/cgroup/kubepods/podabc/cpu.max"),
            "600000 100000\n",
        )
        .expect("w");
        std::fs::write(
            sys2.join("fs/cgroup/kubepods/podabc/memory.max"),
            "42949672960\n",
        )
        .expect("w");
        std::fs::write(sys2.join("devices/system/cpu/online"), "0-15\n").expect("w");
        std::fs::write(proc2.join("self/cgroup"), "0::/kubepods/podabc\n").expect("w");
        std::fs::write(proc2.join("meminfo"), "MemTotal:       64295972 kB\n").expect("w");
        let m = observe_machine_at(
            &sys2.display().to_string(),
            &proc2.display().to_string(),
            true,
        );
        assert_eq!(
            m.cpu_quota_cores,
            Some(6.0),
            "the nested path must be followed: {m:?}"
        );
        assert!(m.cpu_quota_source.contains("/kubepods/podabc/"), "{m:?}");
        assert_eq!(m.mem_limit_mb, Some(40 * 1024), "{m:?}");
        assert_eq!(RigCheck::of(&pod, m).status, RigStatus::Verified);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cpu_list_is_counted_and_a_malformed_one_is_refused() {
        assert_eq!(count_cpu_list("0-15\n"), Some(16));
        assert_eq!(count_cpu_list("0"), Some(1));
        assert_eq!(count_cpu_list("0,2-3,8"), Some(4));
        assert_eq!(count_cpu_list("0-47"), Some(48));
        // Not a count of zero, not a panic: nothing readable.
        assert_eq!(count_cpu_list(""), None);
        assert_eq!(count_cpu_list("nonsense"), None);
        assert_eq!(count_cpu_list("9-2"), None);
    }

    #[test]
    fn a_run_that_measured_nothing_does_not_pass() {
        let mut r = run("engram", 1, 6);
        assert!(!r.pass(), "no levels and no ok queries compared nothing");
        r.queries.push(QueryResult {
            query: "q1".into(),
            statement: Some("x".into()),
            count: Some(1),
            millis: Some(1.0),
            status: "unmappable".into(),
            probe: "skipped".into(),
            detail: None,
            expected: None,
            catalogue_status: "unsupported".into(),
        });
        assert!(!r.pass(), "nine unmappable queries measured nothing");
        r.queries[0].status = "ok".into();
        assert!(r.pass());
        r.integrity.push("lost updates".into());
        assert!(!r.pass(), "an integrity finding fails the run");
    }

    #[test]
    fn the_document_escapes_hostile_strings_and_keeps_the_old_key_names() {
        let hostile = "he said \"MATCH\\ (n)\"\nthen\tcontrol:\u{1}";
        let mut r = run("engram", 7, 6);
        r.queries.push(QueryResult {
            query: "q1".into(),
            statement: Some("MATCH (n) RETURN count(*) AS count".into()),
            count: Some(3),
            millis: Some(12.5),
            status: "error".into(),
            probe: "exists".into(),
            detail: Some(hostile.into()),
            expected: Some(3),
            catalogue_status: "verified".into(),
        });
        let doc = r.render();
        let parsed = engram_cypher::json::from_json(&doc).expect("valid JSON");
        let Value::Map(m) = parsed else {
            panic!("not an object")
        };
        assert_eq!(m.get("pass"), Some(&Value::Bool(false)));
        let Some(Value::List(qs)) = m.get("queries") else {
            panic!("no queries")
        };
        let Value::Map(q) = &qs[0] else {
            panic!("not an object")
        };
        assert_eq!(q.get("detail"), Some(&Value::Str(hostile.to_string())));
        // The old name still carries the statement, so a committed report and
        // every script that reads one still parse.
        assert_eq!(q.get("adapted_cypher"), q.get("statement"));
    }

    #[test]
    fn a_table_marks_a_count_disagreement_instead_of_scoring_it() {
        let mut a = run("engram", 1, 6);
        let mut b = run("neo4j", 1, 6);
        let mk = |count: i64, ms: f64| QueryResult {
            query: "q1".into(),
            statement: Some("x".into()),
            count: Some(count),
            millis: Some(ms),
            status: "ok".into(),
            probe: "exists".into(),
            detail: None,
            expected: None,
            catalogue_status: "verified".into(),
        };
        a.queries.push(mk(100, 10.0));
        b.queries.push(mk(101, 40.0));
        let t = table(&[&Comparable::from(&a), &Comparable::from(&b)]);
        assert!(t.contains("COUNTS DIFFER"), "got:\n{t}");
        assert!(t.contains("no ratio"), "got:\n{t}");
    }

    #[test]
    fn two_runs_of_one_engine_get_their_own_columns() {
        // The A-B-A shape, and the shape a worker-count sweep produces. A
        // name-keyed column lookup printed the FIRST run's number in both
        // columns and looked entirely healthy doing it.
        let mk = |ops: f64| {
            let mut r = run("engram", 1, 6);
            r.workload = Workload::Stress;
            // A JUDGEABLE window: `level(.., 1.0, vec![100])` was one second
            // and one bucket, which the shape rule now correctly refuses as
            // never having been checked — and a refused row prints a word
            // instead of the number this test is about.
            let mut lv = level(100, 0, 4.0, vec![25, 25, 25, 25]);
            lv.r_ops = ops as usize;
            lv.profile = "balanced".into();
            r.levels.push(lv);
            Comparable::from(&r)
        };
        let (a, b) = (mk(10.0), mk(20.0));
        let t = table(&[&a, &b]);
        assert!(
            t.contains("engram#1") && t.contains("engram#2"),
            "got:\n{t}"
        );
        assert!(
            t.contains("27.50") && t.contains("30.00"),
            "each column must carry its own run's number, got:\n{t}"
        );
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod regression_gate_tests {
    use super::*;

    fn row(key: &str, value: f64, quotable: bool) -> Row {
        Row {
            engine: "engram".into(),
            key: key.into(),
            clients: 1,
            value,
            quotable,
            why: if quotable {
                None
            } else {
                Some("timeout".into())
            },
            cause: None,
            count: None,
            probe: None,
        }
    }

    fn doc(workload: &str, rows: Vec<Row>) -> Comparable {
        Comparable {
            engine: "engram".into(),
            workload: workload.into(),
            catalogue_digest: "0".into(),
            catalogue_family_digests: BTreeMap::new(),
            fairness: "{}".into(),
            writes_mode: "n/a".into(),
            rig: Some("{}".into()),
            rig_check: Some("verified".into()),
            fairness_check: Some("verified".into()),
            declared_quota_cores: Some(40),
            declared_thread_cap: Some(40),
            rows,
        }
    }

    #[test]
    fn a_query_past_the_tolerance_is_a_regression() {
        let base = doc("lsqb", vec![row("q1", 100.0, true)]);
        let cand = doc("lsqb", vec![row("q1", 130.0, true)]);
        let r = regressions(&base, &cand, 0.10);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].key, "q1");
    }

    #[test]
    fn drift_inside_the_tolerance_is_not() {
        let base = doc("lsqb", vec![row("q1", 100.0, true)]);
        let cand = doc("lsqb", vec![row("q1", 109.0, true)]);
        assert!(regressions(&base, &cand, 0.10).is_empty());
    }

    #[test]
    fn a_stress_row_regresses_when_ops_per_second_FALL() {
        // THE DIRECTION TEST. `Row::value` is milliseconds on every lane but
        // stress, where it is ops/s. A gate that assumed one direction would
        // call every stress speed-up a regression and pass every real one.
        let base = doc("stress-balanced", vec![row("p1", 1000.0, true)]);
        let slower = doc("stress-balanced", vec![row("p1", 500.0, true)]);
        let faster = doc("stress-balanced", vec![row("p1", 2000.0, true)]);
        assert_eq!(
            regressions(&base, &slower, 0.10).len(),
            1,
            "fewer ops/s is worse"
        );
        assert!(
            regressions(&base, &faster, 0.10).is_empty(),
            "more ops/s is BETTER and must never fail the gate"
        );
    }

    #[test]
    fn a_millisecond_lane_regresses_in_the_other_direction() {
        let base = doc("lsqb", vec![row("q1", 1000.0, true)]);
        let faster = doc("lsqb", vec![row("q1", 500.0, true)]);
        assert!(
            regressions(&base, &faster, 0.10).is_empty(),
            "fewer milliseconds is BETTER"
        );
    }

    #[test]
    fn a_query_the_candidate_stopped_running_is_a_regression() {
        // A shorter table is not a greener one.
        let base = doc("lsqb", vec![row("q1", 100.0, true), row("q2", 100.0, true)]);
        let cand = doc("lsqb", vec![row("q1", 100.0, true)]);
        let r = regressions(&base, &cand, 0.10);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].key, "q2");
        assert!(r[0].now.is_none());
    }

    #[test]
    fn a_query_that_became_unquotable_is_a_regression_whatever_its_number() {
        let base = doc("lsqb", vec![row("q1", 100.0, true)]);
        // A killed query is not a timing: the number is meaningless and only
        // the status is the result.
        let cand = doc("lsqb", vec![row("q1", 1.0, false)]);
        let r = regressions(&base, &cand, 0.10);
        assert_eq!(
            r.len(),
            1,
            "a faster-looking timeout must still fail: {r:?}"
        );
    }

    #[test]
    fn a_query_that_was_ALREADY_failing_is_not_reported() {
        let base = doc("lsqb", vec![row("q1", 0.0, false)]);
        let cand = doc("lsqb", vec![row("q1", 0.0, false)]);
        assert!(
            regressions(&base, &cand, 0.10).is_empty(),
            "a standing failure reported nightly trains people to ignore the gate"
        );
    }

    #[test]
    fn a_query_only_the_candidate_has_is_not_a_regression() {
        let base = doc("lsqb", vec![row("q1", 100.0, true)]);
        let cand = doc("lsqb", vec![row("q1", 100.0, true), row("q2", 999.0, true)]);
        assert!(
            regressions(&base, &cand, 0.10).is_empty(),
            "adding coverage must never fail the gate"
        );
    }

    #[test]
    fn a_slowdown_inside_the_floor_is_not_a_regression() {
        // rev54's IC1 against rev49's: 6.2 -> 9.0 ms, 45 % and 2.8 ms, where
        // repeated runs of the one binary spanned 5.4-7.1.
        let base = doc("snb-interactive", vec![row("IC1", 6.2, true)]);
        let cand = doc("snb-interactive", vec![row("IC1", 9.0, true)]);
        assert_eq!(regressions_in_every(&base, &[&cand], 0.25, 0.0).len(), 1);
        assert!(regressions_in_every(&base, &[&cand], 0.25, 5.0).is_empty());
    }

    #[test]
    fn the_floor_never_passes_a_slowdown_larger_than_itself() {
        let base = doc("snb-interactive", vec![row("IS2", 1.0, true)]);
        let cand = doc("snb-interactive", vec![row("IS2", 25.0, true)]);
        let r = regressions_in_every(&base, &[&cand], 0.25, 5.0);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].why.contains("5 ms floor"), "{}", r[0].why);
    }

    #[test]
    fn the_floor_is_milliseconds_and_never_judges_throughput() {
        // 1000 -> 996 ops/s is inside any tolerance; 1000 -> 500 is past it,
        // and a 5 "ms" floor must not be read as 5 ops/s of slack.
        let base = doc("stress-balanced", vec![row("p1", 1000.0, true)]);
        let slower = doc("stress-balanced", vec![row("p1", 500.0, true)]);
        assert_eq!(regressions_in_every(&base, &[&slower], 0.10, 1e9).len(), 1);
    }

    #[test]
    fn a_regression_must_show_in_every_repetition() {
        // IC3 rides on the server's history: 253-512 ms on one binary.
        let base = doc("snb-interactive", vec![row("IC3", 336.6, true)]);
        let slow = doc("snb-interactive", vec![row("IC3", 436.2, true)]);
        let slower = doc("snb-interactive", vec![row("IC3", 512.0, true)]);
        let usual = doc("snb-interactive", vec![row("IC3", 300.0, true)]);
        assert!(
            regressions_in_every(&base, &[&slow, &usual], 0.25, 5.0).is_empty(),
            "one run of two past the tolerance is noise"
        );
        let r = regressions_in_every(&base, &[&slower, &slow], 0.25, 5.0);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].now, Some(436.2), "the best repetition is the one shown");
        assert!(r[0].why.contains("every one of 2 runs"), "{}", r[0].why);
    }

    #[test]
    fn a_repetition_that_did_not_run_a_key_is_judged_with_the_others() {
        let base = doc("lsqb", vec![row("q1", 100.0, true)]);
        let ran = doc("lsqb", vec![row("q1", 100.0, true)]);
        let skipped = doc("lsqb", vec![]);
        let killed = doc("lsqb", vec![row("q1", 1.0, false)]);
        assert!(regressions_in_every(&base, &[&skipped, &ran], 0.10, 0.0).is_empty());
        let r = regressions_in_every(&base, &[&skipped, &killed], 0.10, 0.0);
        assert_eq!(r.len(), 1, "no repetition answered: {r:?}");
        let slow = doc("lsqb", vec![row("q1", 300.0, true)]);
        let r = regressions_in_every(&base, &[&killed, &slow], 0.10, 0.0);
        assert_eq!(r[0].now, Some(300.0), "a timing is the milder evidence: {r:?}");
    }

    #[test]
    fn one_repetition_and_no_floor_is_the_single_gate() {
        let base = doc("lsqb", vec![row("q1", 100.0, true), row("q2", 100.0, true)]);
        let cand = doc("lsqb", vec![row("q1", 130.0, true)]);
        assert_eq!(
            regressions(&base, &cand, 0.10),
            regressions_in_every(&base, &[&cand], 0.10, 0.0)
        );
    }

    #[test]
    fn every_regression_is_reported_not_just_the_first() {
        let base = doc(
            "lsqb",
            vec![
                row("q1", 100.0, true),
                row("q2", 100.0, true),
                row("q3", 100.0, true),
            ],
        );
        let cand = doc(
            "lsqb",
            vec![
                row("q1", 500.0, true),
                row("q2", 100.0, true),
                row("q3", 500.0, true),
            ],
        );
        let r = regressions(&base, &cand, 0.10);
        assert_eq!(r.len(), 2, "one run must name all of them: {r:?}");
    }

    fn bound(key: &str, value: f64, probe: &str) -> Row {
        Row {
            probe: Some(probe.into()),
            ..row(key, value, true)
        }
    }

    #[test]
    fn the_same_key_bound_to_different_parameters_is_a_different_question() {
        // bi20's SF3 parameters were re-derived on 2026-09-23: same key, a
        // new company and person. A 55 ms answer to the new question against
        // a 14 ms answer to the old one is not a regression, and not a pass.
        let base = doc(
            "snb-bi",
            vec![
                bound("bi20a", 14.0, "bound company=Str(\"Aerogaviota\") person2Id=Int(16128)"),
                bound("bi1", 18_000.0, "bound datetime=DateTime { epoch_seconds: 1 }"),
            ],
        );
        let cand = doc(
            "snb-bi",
            vec![
                bound("bi20a", 55.0, "bound company=Str(\"Air_India\") person2Id=Int(417)"),
                bound("bi1", 18_500.0, "bound datetime=DateTime { epoch_seconds: 1 }"),
            ],
        );
        let moved = parameter_mismatches(&base, &cand);
        assert_eq!(moved.len(), 1, "only the key whose binding moved: {moved:?}");
        assert_eq!(moved[0].0, "bi20a");
        assert!(moved[0].1.contains("Aerogaviota") && moved[0].2.contains("Air_India"));
        assert!(parameter_mismatches(&base, &base).is_empty(), "a run agrees with itself");
    }

    #[test]
    fn a_row_that_bound_nothing_is_not_a_parameter_mismatch() {
        // `skipped` (an unmappable query), `exists`, and a document written
        // before rows carried their binding: none bound anything to differ.
        let base = doc(
            "snb-bi",
            vec![bound("bi15", 0.0, "skipped"), row("bi2a", 9_000.0, true)],
        );
        let cand = doc(
            "snb-bi",
            vec![
                bound("bi15", 16_000.0, "bound person1Id=Int(1) person2Id=Int(2)"),
                bound("bi2a", 4_000.0, "bound date=DateTime { epoch_seconds: 1 }"),
            ],
        );
        assert!(parameter_mismatches(&base, &cand).is_empty());
    }
}
