//! The fairness knobs, checked against the engine that ran under them.
//!
//! # The hole this closes, and how it was found
//!
//! [`crate::plan::Fairness`] is a DECLARATION, exactly as [`crate::report::Rig`]
//! is. `harness report` refuses to build a row out of two documents whose
//! fairness blocks DISAGREE, and that refusal has always worked. What it has
//! never been able to see is a block that AGREES with another document while
//! describing a server neither of them ran on.
//!
//! It did not stay hypothetical. The 2026-09-08/09 Neo4j window passed
//! `--cache-mb 10240` on its two stress arms — the pod's own
//! `NEO4J_server_memory_pagecache_size: 10G` — and passed nothing on its three
//! LSQB batteries, so those three documents took the harness's silent default
//! and stamped `cache_budget_mb: 8192`. Three result documents describe a
//! server that was not running, and every check in the pipeline passed. It
//! surfaced only because the reporter later, and correctly, refused a
//! cross-engine table on Neo4j-at-10240 against LadybugDB-at-8192: the guard
//! worked, the stamping did not.
//!
//! # The rule
//!
//! A fairness figure is one of three things, and the document must say which:
//!
//! - **Observed** — read back off the engine that is about to produce the
//!   numbers. `SHOW shared_buffers` beats `--cache-mb 8192` because a server
//!   cannot be asked a question and answer about a different server.
//! - **Applied** — set BY this process ON the engine it is measuring. An
//!   embedded engine's `Database(buffer_pool_size=)` cannot describe another
//!   process's configuration, because there is no other process.
//! - **Declared** — a claim about a server configured out of band, with no way
//!   to ask. engram's `--paged-cache-mb` on a server somebody else started is
//!   this, and so is a Neo4j page cache on a build whose config procedure is
//!   unavailable. Declared is legitimate; declared-and-unlabelled is not.
//!
//! `Declared` is NOT a failure and is not treated as one. It is the same
//! position [`crate::report::RigStatus::Unobservable`] holds: the stamp is
//! exactly as trusted as it always was, and now SAYS so instead of looking
//! like a check that passed.
//!
//! # Why this is a second block and not a stricter `fairness`
//!
//! Provenance must NOT enter the compared text. Postgres can observe its cache
//! budget and engram cannot, so a fairness block carrying its own provenance
//! would refuse a Postgres run against an engram run configured identically —
//! a refusal naming the wrong thing, which is how somebody ends up "fixing" a
//! correct arm. So the shape is [`crate::report::Rig`]'s exactly: the
//! DECLARATION is compared, the CHECK travels beside it, and only a
//! CONTRADICTION refuses.

use std::collections::BTreeMap;

use engram_cypher::Value;

/// Where one fairness figure came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// Read back off the engine. The string says which question was asked and
    /// what came back, verbatim — so a value this module parsed WRONG is
    /// self-diagnosing rather than an unexplained refusal.
    Observed(String),
    /// Set by this process on the engine it measures.
    Applied(String),
    /// A claim about something configured elsewhere, with the reason no
    /// observation was possible.
    Declared(String),
}

impl Provenance {
    /// The word the document carries.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Provenance::Observed(_) => "observed",
            Provenance::Applied(_) => "applied",
            Provenance::Declared(_) => "declared",
        }
    }

    /// The detail — the question and its answer, or why there was none.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Provenance::Observed(s) | Provenance::Applied(s) | Provenance::Declared(s) => s,
        }
    }

    /// Whether this figure was actually checkable.
    ///
    /// `Applied` counts: a value this process set on the engine in this
    /// process cannot describe a different engine. `Declared` does not.
    #[must_use]
    pub fn is_evidence(&self) -> bool {
        !matches!(self, Provenance::Declared(_))
    }
}

/// One fairness figure, as the ENGINE answers for it.
///
/// `value` is `None` when the engine was asked and could not answer, which is
/// a different fact from "the answer disagreed" and is kept separate for the
/// same reason [`crate::report::ObservedMachine::cpu_quota_readable`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Figure {
    /// What the engine says the value is, in the document's units.
    pub value: Option<u32>,
    /// Where that came from, or why it did not.
    pub provenance: Provenance,
}

impl Figure {
    /// A value read off the engine.
    #[must_use]
    pub fn observed(value: u32, how: impl Into<String>) -> Figure {
        Figure {
            value: Some(value),
            provenance: Provenance::Observed(how.into()),
        }
    }

    /// A value this process set on the engine it measures.
    #[must_use]
    pub fn applied(value: u32, how: impl Into<String>) -> Figure {
        Figure {
            value: Some(value),
            provenance: Provenance::Applied(how.into()),
        }
    }

    /// A claim nothing could check, and why.
    #[must_use]
    pub fn declared(why: impl Into<String>) -> Figure {
        Figure {
            value: None,
            provenance: Provenance::Declared(why.into()),
        }
    }

    fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "value".to_string(),
            self.value.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        );
        m.insert(
            "provenance".to_string(),
            Value::Str(self.provenance.kind().to_string()),
        );
        m.insert(
            "source".to_string(),
            Value::Str(self.provenance.detail().to_string()),
        );
        Value::Map(m)
    }
}

/// What the ENGINE says about the two knobs the fairness block declares.
///
/// The clients and the seconds are not here on purpose: both are properties of
/// the HARNESS's own loop, not of the engine, so there is nothing to ask and
/// nothing that could disagree. Only the two figures a server owns — its
/// serving cache and its intra-query width — can be got wrong the way the
/// Neo4j window got them wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineFairness {
    /// The serving cache budget, in MiB.
    pub cache_budget_mb: Figure,
    /// The intra-query parallelism cap, as a thread/process count.
    pub thread_cap: Figure,
}

impl EngineFairness {
    /// Nothing about this engine's knobs can be asked, and why.
    #[must_use]
    pub fn declared(why: &str) -> EngineFairness {
        EngineFairness {
            cache_budget_mb: Figure::declared(why),
            thread_cap: Figure::declared(why),
        }
    }

    /// As a document value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "cache_budget_mb".to_string(),
            self.cache_budget_mb.to_value(),
        );
        m.insert("thread_cap".to_string(), self.thread_cap.to_value());
        Value::Map(m)
    }
}

/// The verdict of checking a declared fairness block against the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FairnessStatus {
    /// Both figures were observed or applied, and both agreed.
    Verified,
    /// Something agreed; something else could only be declared.
    Partial,
    /// Neither figure could be checked. NOT a pass — the stamp is unchecked,
    /// and the document says so where a reader will see it.
    Declared,
    /// At least one figure the engine answered for CONTRADICTS the stamp.
    Mismatch,
}

impl FairnessStatus {
    /// The word the document carries.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FairnessStatus::Verified => "verified",
            FairnessStatus::Partial => "partial",
            FairnessStatus::Declared => "declared",
            FairnessStatus::Mismatch => "mismatch",
        }
    }
}

/// A declared fairness block, the engine that was actually underneath it, and
/// whether they agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FairnessCheck {
    /// The verdict.
    pub status: FairnessStatus,
    /// What the engine answered.
    pub engine: EngineFairness,
    /// The figures that were checked and agreed, in words.
    pub agreements: Vec<String>,
    /// The figures that were checked and did NOT agree, in words.
    pub disagreements: Vec<String>,
}

impl FairnessCheck {
    /// Check a declared block against what the engine answered.
    ///
    /// Pure, and split from every way of obtaining an [`EngineFairness`] for
    /// the reason [`crate::report::RigCheck::of`] is split: the rule has to be
    /// testable without an engine that has the property under test, and the
    /// property under test here is *being wrong*.
    #[must_use]
    pub fn of(declared: &crate::plan::Fairness, engine: EngineFairness) -> FairnessCheck {
        let mut agreements = Vec::new();
        let mut disagreements = Vec::new();
        let mut unchecked = 0usize;

        let mut judge = |label: &str, want: u32, got: &Figure, unit: &str| match (
            got.provenance.is_evidence(),
            got.value,
        ) {
            (false, _) | (true, None) => unchecked += 1,
            (true, Some(v)) if v == want => agreements.push(format!(
                "{label} {v}{unit} {} ({})",
                got.provenance.kind(),
                got.provenance.detail()
            )),
            (true, Some(v)) => disagreements.push(format!(
                "the run stamps {label} {want}{unit} and the engine is running with \
                     {v}{unit} ({})",
                got.provenance.detail()
            )),
        };
        judge(
            "cache_budget_mb",
            declared.cache_budget_mb,
            &engine.cache_budget_mb,
            " MiB",
        );
        judge("thread_cap", declared.thread_cap, &engine.thread_cap, "");

        let status = if !disagreements.is_empty() {
            FairnessStatus::Mismatch
        } else if agreements.is_empty() {
            FairnessStatus::Declared
        } else if unchecked > 0 {
            FairnessStatus::Partial
        } else {
            FairnessStatus::Verified
        };
        FairnessCheck {
            status,
            engine,
            agreements,
            disagreements,
        }
    }

    /// A never-performed check, for a document built without one.
    #[must_use]
    pub fn not_checked() -> FairnessCheck {
        FairnessCheck {
            status: FairnessStatus::Declared,
            engine: EngineFairness::declared("declared: not attempted"),
            agreements: Vec::new(),
            disagreements: Vec::new(),
        }
    }

    /// Which figures nothing could be asked about.
    #[must_use]
    pub fn unchecked(&self) -> Vec<&'static str> {
        self.unchecked_with_reason()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The unchecked figures, each with what the engine actually said.
    ///
    /// The reason is not decoration. "The cache budget was not checked" and
    /// "the engine answered that it has NO block cache in circuit" are the
    /// same status and completely different news, and the second is the one a
    /// reader has to see — it is how a `--data-dir` engram run stamped with a
    /// cache budget announces that the budget describes no knob.
    #[must_use]
    pub fn unchecked_with_reason(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        for (name, f) in [
            ("cache budget", &self.engine.cache_budget_mb),
            ("thread cap", &self.engine.thread_cap),
        ] {
            if !f.provenance.is_evidence() || f.value.is_none() {
                out.push((name, f.provenance.detail()));
            }
        }
        out
    }

    /// The unchecked figures and what the engine said about each, as prose.
    fn unchecked_reasons(&self) -> String {
        self.unchecked_with_reason()
            .into_iter()
            .map(|(name, why)| format!("{name} — {why}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// One or more lines a person reads before a sweep starts.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.status {
            FairnessStatus::Verified => format!(
                "fairness VERIFIED against the engine: {}",
                self.agreements.join("; ")
            ),
            FairnessStatus::Partial => format!(
                "fairness partially verified: {} — and the rest of the stamp is a claim: {}",
                self.agreements.join("; "),
                self.unchecked_reasons()
            ),
            FairnessStatus::Declared => format!(
                "fairness NOT verified — this engine answers for neither knob, so the \
                 numbers in the block are claims about a server configured elsewhere. That \
                 is what they have always been; they now SAY so rather than looking like a \
                 check that passed. {}",
                self.unchecked_reasons()
            ),
            FairnessStatus::Mismatch => format!(
                "the fairness stamp CONTRADICTS the engine that is about to produce the \
                 numbers: {}",
                self.disagreements.join("; ")
            ),
        }
    }

    /// As a document value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        m.insert(
            "status".to_string(),
            Value::Str(self.status.name().to_string()),
        );
        m.insert("engine".to_string(), self.engine.to_value());
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

/// Parse a memory setting the way PostgreSQL and Neo4j both write one, into
/// whole MiB.
///
/// Accepts a bare byte count (`10737418240`), a suffixed integer (`8GB`,
/// `10G`, `1048576kB`) and a suffixed decimal (`10.00GiB`), case-insensitively
/// and with optional space before the unit.
///
/// **`GB` means 2^30, not 10^9, and that is not a shortcut.** Both engines
/// whose settings this reads define their size units in binary multiples —
/// PostgreSQL's `shared_buffers=8GB` is 8192 MiB, and Neo4j's byte settings
/// parse `G` the same way. Reading either as 10^9 would put `shared_buffers`
/// at 7629 MiB and turn an engine that agrees with its stamp into a mismatch.
///
/// Returns `None` for anything it cannot read, which is recorded as
/// "the engine answered and this could not parse it" rather than as a value —
/// a mis-parse that produced a NUMBER would be the very failure this module
/// exists to prevent, arriving from the other side.
#[must_use]
pub fn parse_size_mb(text: &str) -> Option<u32> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let unit = unit.trim().to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let mb = (n * mult) / (1024.0 * 1024.0);
    if !mb.is_finite() || mb < 0.0 || mb > f64::from(u32::MAX) {
        return None;
    }
    Some(mb.round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Fairness;

    fn declared(cache: u32, threads: u32) -> Fairness {
        Fairness {
            thread_cap: threads,
            cache_budget_mb: cache,
            clients: 1,
            seconds: 20,
        }
    }

    #[test]
    fn a_size_is_read_in_binary_multiples_the_way_both_engines_write_one() {
        // PostgreSQL's own answer, read off the PostgreSQL benchmark pod on 2026-09-09.
        assert_eq!(parse_size_mb("8GB"), Some(8192));
        // Neo4j writes byte settings either way depending on build; both must
        // land on the same number or the probe is a coin flip.
        assert_eq!(parse_size_mb("10737418240"), Some(10240));
        assert_eq!(parse_size_mb("10G"), Some(10240));
        assert_eq!(parse_size_mb("10.00GiB"), Some(10240));
        assert_eq!(parse_size_mb("1048576kB"), Some(1024));
        assert_eq!(parse_size_mb("8192 MB"), Some(8192));
        // 10^9 would be 7629 MiB. If this ever passes, every Postgres arm
        // starts failing a check it should pass.
        assert_ne!(parse_size_mb("8GB"), Some(7629));
        // Unreadable is None, never a number.
        assert_eq!(parse_size_mb("no value"), None);
        assert_eq!(parse_size_mb(""), None);
        assert_eq!(parse_size_mb("8 parsecs"), None);
    }

    #[test]
    fn an_engine_that_agrees_verifies_and_one_that_disagrees_is_a_mismatch() {
        let want = declared(8192, 6);
        let agrees = EngineFairness {
            cache_budget_mb: Figure::observed(8192, "postgres: SHOW shared_buffers = `8GB`"),
            thread_cap: Figure::observed(
                6,
                "postgres: 1 leader + max_parallel_workers_per_gather 5",
            ),
        };
        let ok = FairnessCheck::of(&want, agrees);
        assert_eq!(ok.status, FairnessStatus::Verified, "{ok:?}");
        assert!(ok.disagreements.is_empty());

        // THE FAILURE THIS MODULE EXISTS FOR, in its own numbers: the Neo4j
        // window's three LSQB documents stamped 8192 against a server the pod
        // manifest gave 10 GiB of page cache.
        let neo4j = EngineFairness {
            cache_budget_mb: Figure::observed(
                10240,
                "neo4j: dbms.listConfig(server.memory.pagecache.size) = `10737418240`",
            ),
            thread_cap: Figure::declared(
                "declared: Neo4j Community has no intra-query parallelism setting",
            ),
        };
        let bad = FairnessCheck::of(&want, neo4j);
        assert_eq!(bad.status, FairnessStatus::Mismatch, "{bad:?}");
        assert!(
            bad.disagreements[0].contains("8192") && bad.disagreements[0].contains("10240"),
            "the refusal must name both numbers: {:?}",
            bad.disagreements
        );
        // And it must still be a mismatch when the OTHER figure is the one
        // that agrees — a single agreement never outvotes a contradiction.
        assert_eq!(
            FairnessCheck::of(
                &declared(10240, 6),
                EngineFairness {
                    cache_budget_mb: Figure::observed(10240, "x"),
                    thread_cap: Figure::observed(16, "y"),
                }
            )
            .status,
            FairnessStatus::Mismatch
        );
    }

    #[test]
    fn declared_is_recorded_as_unchecked_and_is_not_a_pass() {
        let want = declared(8192, 6);
        let engram = EngineFairness::declared(
            "declared: this engram server does not report its serving configuration",
        );
        let c = FairnessCheck::of(&want, engram);
        assert_eq!(c.status, FairnessStatus::Declared);
        assert!(c.agreements.is_empty() && c.disagreements.is_empty());
        assert_eq!(c.unchecked(), vec!["cache budget", "thread cap"]);
        assert!(
            c.describe().contains("NOT verified"),
            "a declared stamp must not read as a check that passed: {}",
            c.describe()
        );

        // One half asked, one half not.
        let half = EngineFairness {
            cache_budget_mb: Figure::applied(8192, "ladybug: Database(buffer_pool_size=)"),
            thread_cap: Figure::declared("declared: nothing to ask"),
        };
        let c = FairnessCheck::of(&want, half);
        assert_eq!(c.status, FairnessStatus::Partial);
        assert_eq!(c.unchecked(), vec!["thread cap"]);
    }

    #[test]
    fn an_engine_that_was_asked_and_could_not_answer_is_unchecked_not_agreed() {
        // The distinction ObservedMachine keeps between "no quota" and "could
        // not read", one level up: a probe that RAN and returned something
        // unparseable must not be read as agreement.
        let f = Figure {
            value: None,
            provenance: Provenance::Observed("neo4j: listConfig returned `?` (unparseable)".into()),
        };
        let c = FairnessCheck::of(
            &declared(8192, 6),
            EngineFairness {
                cache_budget_mb: f,
                thread_cap: Figure::observed(6, "x"),
            },
        );
        assert_eq!(c.status, FairnessStatus::Partial);
        assert_eq!(c.unchecked(), vec!["cache budget"]);
    }
}
