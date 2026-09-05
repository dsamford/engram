//! Expand — the first traversal operator.
//!
//! `sources → their neighbours`, as sets over directories: the input is a
//! [`RowIdSet`] in the SOURCE group's offset space, the output is a
//! [`RowIdSet`] in the DESTINATION group's — two directories, because a
//! traversal changes universes and an operator that pretended otherwise would
//! AND bitmaps whose bits mean different nodes.
//!
//! Like the semi-mask, Expand's report is the point: edges traversed, distinct
//! peers, and — first-class — the peers that fell OUTSIDE the destination
//! group. A 1-hop expansion into a partitioned graph routinely reaches other
//! partitions; those peers are not errors, but a caller that never sees the
//! count cannot tell "this group covers the neighbourhood" from "two thirds of
//! the frontier silently vanished". That is the incumbent's assert-arrival
//! lesson applied to traversal.

use engram_store::{AdjacencyError, EdgeDir, EdgeType, NodeAt, Store, neighbors};

use crate::{RowDirectory, RowIdError, RowIdSet};

/// What one expansion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandReport {
    /// Source rows expanded (the input set's population).
    pub sources: usize,
    /// Edges traversed — the operator's actual work, and the number the
    /// planner's cost model calibrates against.
    pub edges: usize,
    /// Distinct peers that mapped into the destination group.
    pub in_group: usize,
    /// Peers outside the destination group, as (namespace, id) pairs —
    /// carried, not counted, so a multi-group executor can route them.
    pub outside_group: Vec<(u32, u64)>,
}

/// Errors an expansion can hit.
#[derive(Debug)]
pub enum ExpandError {
    /// The input set's capacity does not match the source directory.
    Rows(RowIdError),
    /// The adjacency layer refused (corrupt chunk…).
    Adjacency(AdjacencyError),
}

impl From<RowIdError> for ExpandError {
    fn from(e: RowIdError) -> Self {
        ExpandError::Rows(e)
    }
}

impl From<AdjacencyError> for ExpandError {
    fn from(e: AdjacencyError) -> Self {
        ExpandError::Adjacency(e)
    }
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExpandError::Rows(e) => write!(f, "{e}"),
            ExpandError::Adjacency(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ExpandError {}

/// Where a group lives — the coordinates Expand needs to read adjacency.
#[derive(Debug, Clone, Copy)]
pub struct GroupAt {
    /// Realm of the group's rows.
    pub realm: engram_key::Realm,
    /// Namespace.
    pub ns: engram_key::Namespace,
    /// Partition.
    pub partition: engram_key::Partition,
}

/// Expand `input` (over `src_dir` in `src_group`) one hop along `etype`/`dir`,
/// producing a set over `dst_dir` whose peers land in `dst_ns`.
///
/// Peers in namespaces other than `dst_ns`, and peers in `dst_ns` that the
/// destination directory does not hold, are returned in
/// [`ExpandReport::outside_group`] rather than dropped.
#[allow(clippy::too_many_arguments)]
pub fn expand(
    store: &Store,
    src_group: GroupAt,
    src_dir: &RowDirectory,
    input: &RowIdSet,
    dir: EdgeDir,
    etype: EdgeType,
    dst_dir: &RowDirectory,
    dst_ns: engram_key::Namespace,
) -> Result<(RowIdSet, ExpandReport), ExpandError> {
    if input.capacity() != src_dir.len() {
        return Err(ExpandError::Rows(RowIdError::CapacityMismatch {
            left: input.capacity(),
            right: src_dir.len(),
        }));
    }

    let mut out = RowIdSet::empty(dst_dir.len());
    let mut edges = 0usize;
    let mut outside = Vec::new();

    for offset in input.iter() {
        let id = src_dir
            .id_of(offset)
            .ok_or(ExpandError::Rows(RowIdError::OutOfRange {
                offset,
                capacity: src_dir.len(),
            }))?;
        let at = NodeAt {
            realm: src_group.realm,
            ns: src_group.ns,
            partition: src_group.partition,
            node: id,
        };
        for peer in neighbors(store, at, dir, etype)? {
            edges += 1;
            if peer.ns == dst_ns.0 {
                if let Some(o) = dst_dir.offset_of(peer.id) {
                    out.insert(o)?;
                    continue;
                }
            }
            outside.push((peer.ns, peer.id));
        }
    }

    engram_observe::counted!("exec.expand applied");
    if !outside.is_empty() {
        engram_observe::sometimes!("exec.expand reached outside the group", true);
    }
    let report = ExpandReport {
        sources: input.count(),
        edges,
        in_group: out.count(),
        outside_group: outside,
    };
    Ok((out, report))
}
