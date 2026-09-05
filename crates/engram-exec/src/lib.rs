//! L6, first operator: the ASP semi-mask.
//!
//! The plan's claim, verbatim: *"Tenant scoping, vector-candidate scoping and
//! selective-predicate scoping are the same bitmap-AND on dense offsets.
//! Shipping it as an operator makes scope a **measurement**; deferring it
//! makes scope an **estimate**, and every published result says graph
//! cardinality estimators are badly inaccurate."*
//!
//! So [`semi_mask`] does two things, and the second is the reason it exists as
//! an operator rather than an inline `&`:
//!
//!  1. the bitmap AND;
//!  2. it RETURNS THE MEASUREMENT — how many candidates entered, how many
//!     survived, how many the mask removed. Scope stops being a number the
//!     planner guessed and becomes a number the execution produced, on every
//!     query, for free.
//!
//! FC-8 is honoured at the signature: the mask side arrives as a
//! [`CandidateSource`], the trait a spatial index, an ANN index, a tenant
//! filter and a predicate scan all implement. The spatial reservation is
//! exactly this — a spatial predicate that produces a [`RowIdSet`] composes
//! with everything else *for free*, with no new execution path.

#![forbid(unsafe_code)]

pub mod directory;
pub mod expand;
pub mod pipeline;

pub use directory::RowDirectory;
pub use expand::{ExpandError, ExpandReport, GroupAt, expand};
pub use pipeline::{
    Pipeline, PipelineCause, PipelineError, PipelineReport, StageDetail, StageReport,
};

use engram_observe::{Canary, Gate, Registration, Subsystem, counted};

/// A set of dense row offsets within one group, as a bitmap.
///
/// Dense offsets, not entity ids: the whole economy of the AND is one machine
/// word covering 64 rows, and that requires a directory somewhere else to have
/// mapped ids into a dense range. The set does not know or care what mapped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowIdSet {
    words: Vec<u64>,
    /// The number of valid offsets. Bits at or past this are refused, so two
    /// sets over different-sized groups cannot be silently combined.
    capacity: usize,
}

impl RowIdSet {
    /// An empty set over `capacity` rows.
    pub fn empty(capacity: usize) -> Self {
        RowIdSet {
            words: vec![0; capacity.div_ceil(64)],
            capacity,
        }
    }

    /// A full set over `capacity` rows.
    pub fn full(capacity: usize) -> Self {
        let mut s = RowIdSet {
            words: vec![u64::MAX; capacity.div_ceil(64)],
            capacity,
        };
        s.clear_tail();
        s
    }

    /// Build from offsets. Offsets at or past `capacity` are REFUSED — an
    /// out-of-range candidate is a bug in the producer, and truncating it
    /// silently would turn that bug into a quietly smaller result set.
    pub fn from_offsets(capacity: usize, offsets: &[usize]) -> Result<Self, RowIdError> {
        let mut s = RowIdSet::empty(capacity);
        for &o in offsets {
            s.insert(o)?;
        }
        Ok(s)
    }

    /// Zero any bits past `capacity` in the last word, so `full` and
    /// complements never carry phantom rows.
    fn clear_tail(&mut self) {
        let tail = self.capacity % 64;
        if tail != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << tail) - 1;
            }
        }
    }

    /// The set's capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Insert one offset.
    pub fn insert(&mut self, offset: usize) -> Result<(), RowIdError> {
        if offset >= self.capacity {
            return Err(RowIdError::OutOfRange {
                offset,
                capacity: self.capacity,
            });
        }
        self.words[offset / 64] |= 1u64 << (offset % 64);
        Ok(())
    }

    /// Membership.
    pub fn contains(&self, offset: usize) -> bool {
        offset < self.capacity && (self.words[offset / 64] >> (offset % 64)) & 1 == 1
    }

    /// Population count.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Iterate set offsets in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, w)| {
            let mut w = *w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros() as usize;
                w &= w - 1;
                Some(wi * 64 + bit)
            })
        })
    }

    /// In-place AND. Capacities must match — see [`RowIdError::CapacityMismatch`].
    pub fn and_assign(&mut self, other: &RowIdSet) -> Result<(), RowIdError> {
        self.check_capacity(other)?;
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= b;
        }
        Ok(())
    }

    /// In-place OR.
    pub fn or_assign(&mut self, other: &RowIdSet) -> Result<(), RowIdError> {
        self.check_capacity(other)?;
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
        Ok(())
    }

    /// Complement within the capacity.
    pub fn complement(&self) -> RowIdSet {
        let mut out = RowIdSet {
            words: self.words.iter().map(|w| !w).collect(),
            capacity: self.capacity,
        };
        out.clear_tail();
        out
    }

    fn check_capacity(&self, other: &RowIdSet) -> Result<(), RowIdError> {
        if self.capacity != other.capacity {
            return Err(RowIdError::CapacityMismatch {
                left: self.capacity,
                right: other.capacity,
            });
        }
        Ok(())
    }
}

/// Row-set errors. Every one is a producer bug surfaced, never absorbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowIdError {
    /// An offset at or past the capacity.
    OutOfRange {
        /// The offending offset.
        offset: usize,
        /// The set's capacity.
        capacity: usize,
    },
    /// Two sets over different-sized groups.
    ///
    /// Refused rather than truncated-to-the-smaller: the sets describe
    /// DIFFERENT row universes, and an AND across universes yields offsets
    /// that mean one thing on the left and another on the right — plausible
    /// rows, arbitrary identity. The historical shape of that bug in the
    /// incumbent is a filter joined across mismatched index partitions.
    CapacityMismatch {
        /// Left capacity.
        left: usize,
        /// Right capacity.
        right: usize,
    },
}

impl std::fmt::Display for RowIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RowIdError::OutOfRange { offset, capacity } => {
                write!(
                    f,
                    "offset {offset} is outside the group (capacity {capacity})"
                )
            }
            RowIdError::CapacityMismatch { left, right } => {
                write!(
                    f,
                    "row sets describe different groups: {left} vs {right} rows"
                )
            }
        }
    }
}

impl std::error::Error for RowIdError {}

/// FC-8 — anything that can produce a candidate set.
///
/// A tenant scope, an ANN result, a predicate scan and (one day) a spatial
/// cell cover all implement this one trait, which is what makes "geo-filtered
/// similarity search" a composition instead of an execution path.
pub trait CandidateSource {
    /// Produce this source's candidates over a group of `capacity` rows.
    fn candidates(&self, capacity: usize) -> Result<RowIdSet, RowIdError>;
}

/// A source that is already a set.
impl CandidateSource for RowIdSet {
    fn candidates(&self, capacity: usize) -> Result<RowIdSet, RowIdError> {
        if self.capacity != capacity {
            return Err(RowIdError::CapacityMismatch {
                left: self.capacity,
                right: capacity,
            });
        }
        Ok(self.clone())
    }
}

/// A source from a sorted-or-not offset list — the ANN-candidate shape.
pub struct OffsetList<'a>(pub &'a [usize]);

impl CandidateSource for OffsetList<'_> {
    fn candidates(&self, capacity: usize) -> Result<RowIdSet, RowIdError> {
        RowIdSet::from_offsets(capacity, self.0)
    }
}

/// What a semi-mask application MEASURED.
///
/// This struct is the operator's reason to exist. `masked_out` is the scope's
/// actual selectivity on this query — not an estimate, a count — and it is
/// produced by construction, so it cannot be skipped on the fast path and
/// sampled on the slow one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskReport {
    /// Candidates entering the operator.
    pub input: usize,
    /// Candidates surviving the mask.
    pub output: usize,
    /// `input - output`.
    pub masked_out: usize,
}

/// The ASP semi-mask: `input ∧ mask`, with the measurement.
pub fn semi_mask(
    input: &RowIdSet,
    mask: &impl CandidateSource,
) -> Result<(RowIdSet, MaskReport), RowIdError> {
    let mask_set = mask.candidates(input.capacity())?;
    let before = input.count();
    let mut out = input.clone();
    out.and_assign(&mask_set)?;
    let after = out.count();
    counted!("exec.semi_mask applied");
    if after == 0 && before > 0 {
        // The empty intersection is the case worth observing: it is where a
        // too-selective scope silently answers "nothing found" — the exact
        // reading R26 measured as recall 0.000 that still returned ten rows.
        engram_observe::sometimes!("exec.mask removed every candidate", true);
    }
    Ok((
        out,
        MaskReport {
            input: before,
            output: after,
            masked_out: before - after,
        },
    ))
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The executor's operator layer, as a registered subsystem.
pub struct ExecOperators;

impl Subsystem for ExecOperators {
    const NAME: &'static str = "exec-operators";

    fn register() -> Registration {
        Registration::new()
            .crash_point("exec.before_mask_apply")
            .sometimes("exec.mask removed every candidate")
            .sometimes("exec.expand reached outside the group")
            .sometimes("exec.pipeline skipped a stage")
            .counter("exec.expand applied")
            .counter("exec.semi_mask applied")
            .counter("exec.pipelines finished")
            .gate(
                Gate::new(
                    "a mask never widens and never invents a row",
                    Canary::new("OR instead of AND and assert the output is a subset of the input"),
                )
                .and_canary(Canary::new("accept a capacity mismatch and assert offsets keep their identity")),
            )
            .gate(Gate::new(
                "scope is a measurement, not an estimate",
                Canary::new("report masked_out from a stored selectivity estimate instead of the counts"),
            ))
            .gate(
                Gate::new(
                    "a stage that never ran is reported, never silent",
                    Canary::new("drop the Skipped report on the empty path and assert the report names every stage"),
                )
                .and_canary(Canary::new("leave the universe behind on an empty expand and assert downstream refuses")),
            )
    }
}
