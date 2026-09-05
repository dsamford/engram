//! The pipeline shell: operators chained, reports AGGREGATED.
//!
//! A query's execution is a sequence of set transformations, and the plan's
//! rule for the layer is that every stage's effect on cardinality is a
//! MEASUREMENT the caller receives — not a log line, not an estimate, and
//! never silently missing. The pipeline is where that becomes one artifact:
//! run it, and the report says what every stage received, produced, and cost.
//!
//! R25's rule 4 has an engine-side echo here: a stage that is SKIPPED (its
//! input was already empty) is reported as skipped, because "ran and found
//! nothing" and "never ran" are different facts — collapsing them is the
//! house defect wearing an optimization's clothes.

use engram_store::{EdgeDir, EdgeType, Store};

use crate::{
    CandidateSource, ExpandError, GroupAt, MaskReport, RowDirectory, RowIdError, RowIdSet, expand,
    semi_mask,
};

/// One stage's contribution to the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    /// The stage's label — the planner's name for it, carried so a report is
    /// legible without the pipeline that produced it.
    pub label: String,
    /// Candidates entering.
    pub input: usize,
    /// Candidates leaving.
    pub output: usize,
    /// Stage-specific detail.
    pub detail: StageDetail,
}

/// What kind of work a stage did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageDetail {
    /// The initial seed set.
    Seed,
    /// A semi-mask application.
    Mask {
        /// Candidates the mask removed.
        masked_out: usize,
    },
    /// A one-hop expansion.
    Expand {
        /// Edges traversed — the cost number.
        edges: usize,
        /// Peers outside the destination group, carried.
        outside_group: usize,
    },
    /// The stage never ran: its input was empty. Distinct from running and
    /// producing nothing — different facts, different debugging.
    Skipped,
}

/// The whole run's account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineReport {
    /// Per-stage reports, in execution order.
    pub stages: Vec<StageReport>,
}

impl PipelineReport {
    /// The final output cardinality (the last non-skipped stage's output, or
    /// the seed's).
    pub fn final_output(&self) -> usize {
        self.stages
            .iter()
            .rev()
            .find(|s| s.detail != StageDetail::Skipped)
            .map_or(0, |s| s.output)
    }

    /// Total edges traversed across every expansion — the run's graph cost.
    pub fn total_edges(&self) -> usize {
        self.stages
            .iter()
            .map(|s| match s.detail {
                StageDetail::Expand { edges, .. } => edges,
                _ => 0,
            })
            .sum()
    }
}

/// A pipeline under construction: the current set, its directory, and the
/// report so far.
///
/// Stages consume `self` and return it, so a pipeline reads as the chain it
/// is. Every stage runs eagerly — the report is complete the moment the value
/// exists, and there is no deferred state to observe half-built.
pub struct Pipeline<'a> {
    store: &'a Store,
    set: RowIdSet,
    dir: &'a RowDirectory,
    report: PipelineReport,
}

impl std::fmt::Debug for Pipeline<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("candidates", &self.set.count())
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

/// Pipeline errors — every one a refusal from a layer below, with the stage
/// context attached so the report names where the run stopped.
#[derive(Debug)]
pub struct PipelineError {
    /// The stage that refused.
    pub stage: String,
    /// The refusal.
    pub cause: PipelineCause,
    /// The report up to the refusal — the run's partial account, kept because
    /// a failed run's shape is exactly what the debugger needs.
    pub report: PipelineReport,
}

/// The underlying refusal.
#[derive(Debug)]
pub enum PipelineCause {
    /// A row-set refusal.
    Rows(RowIdError),
    /// An expansion refusal.
    Expand(ExpandError),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            PipelineCause::Rows(e) => write!(f, "stage `{}` refused: {e}", self.stage),
            PipelineCause::Expand(e) => write!(f, "stage `{}` refused: {e}", self.stage),
        }
    }
}

impl std::error::Error for PipelineError {}

impl<'a> Pipeline<'a> {
    /// Start from a seed set over `dir`.
    pub fn seed(
        store: &'a Store,
        dir: &'a RowDirectory,
        set: RowIdSet,
        label: &str,
    ) -> Result<Self, PipelineError> {
        if set.capacity() != dir.len() {
            return Err(PipelineError {
                stage: label.to_string(),
                cause: PipelineCause::Rows(RowIdError::CapacityMismatch {
                    left: set.capacity(),
                    right: dir.len(),
                }),
                report: PipelineReport { stages: vec![] },
            });
        }
        let count = set.count();
        let report = PipelineReport {
            stages: vec![StageReport {
                label: label.to_string(),
                input: count,
                output: count,
                detail: StageDetail::Seed,
            }],
        };
        Ok(Pipeline {
            store,
            set,
            dir,
            report,
        })
    }

    /// Apply a semi-mask.
    pub fn mask(
        mut self,
        source: &impl CandidateSource,
        label: &str,
    ) -> Result<Self, PipelineError> {
        if self.set.count() == 0 {
            engram_observe::sometimes!("exec.pipeline skipped a stage", true);
            self.report.stages.push(StageReport {
                label: label.to_string(),
                input: 0,
                output: 0,
                detail: StageDetail::Skipped,
            });
            return Ok(self);
        }
        let (
            out,
            MaskReport {
                input,
                output,
                masked_out,
            },
        ) = semi_mask(&self.set, source).map_err(|e| PipelineError {
            stage: label.to_string(),
            cause: PipelineCause::Rows(e),
            report: self.report.clone(),
        })?;
        self.report.stages.push(StageReport {
            label: label.to_string(),
            input,
            output,
            detail: StageDetail::Mask { masked_out },
        });
        self.set = out;
        Ok(self)
    }

    /// Expand one hop into `dst_dir`. The pipeline's set and directory both
    /// move to the destination universe — the type-level fact that a traversal
    /// changes what offsets mean.
    #[allow(clippy::too_many_arguments)]
    pub fn expand_hop(
        mut self,
        group: GroupAt,
        dir_: EdgeDir,
        etype: EdgeType,
        dst_dir: &'a RowDirectory,
        dst_ns: engram_key::Namespace,
        label: &str,
    ) -> Result<Self, PipelineError> {
        if self.set.count() == 0 {
            engram_observe::sometimes!("exec.pipeline skipped a stage", true);
            self.report.stages.push(StageReport {
                label: label.to_string(),
                input: 0,
                output: 0,
                detail: StageDetail::Skipped,
            });
            // The universe still moves: downstream stages expect dst offsets.
            self.set = RowIdSet::empty(dst_dir.len());
            self.dir = dst_dir;
            return Ok(self);
        }
        let input = self.set.count();
        let (out, r) = expand(
            self.store, group, self.dir, &self.set, dir_, etype, dst_dir, dst_ns,
        )
        .map_err(|e| PipelineError {
            stage: label.to_string(),
            cause: PipelineCause::Expand(e),
            report: self.report.clone(),
        })?;
        self.report.stages.push(StageReport {
            label: label.to_string(),
            input,
            output: r.in_group,
            detail: StageDetail::Expand {
                edges: r.edges,
                outside_group: r.outside_group.len(),
            },
        });
        self.set = out;
        self.dir = dst_dir;
        Ok(self)
    }

    /// Finish: the set, its ids, and the full report.
    pub fn finish(self) -> Result<(Vec<u64>, PipelineReport), PipelineError> {
        let ids = self.dir.to_ids(&self.set).map_err(|e| PipelineError {
            stage: "finish".to_string(),
            cause: PipelineCause::Rows(e),
            report: self.report.clone(),
        })?;
        engram_observe::counted!("exec.pipelines finished");
        Ok((ids, self.report))
    }
}
