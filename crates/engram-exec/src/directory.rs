//! The row directory: entity ids ↔ dense offsets, per group.
//!
//! [`crate::RowIdSet`] deliberately does not know what its offsets mean — this
//! is the thing that decides. One directory covers one GROUP (one partition's
//! rows of one kind), assigns offsets in first-seen order, and never reuses or
//! reorders them: an offset, once assigned, means that entity for the
//! directory's lifetime, because a bitmap that outlives a renumbering is a set
//! of plausible rows with scrambled identities.
//!
//! # Unmapped ids are REPORTED, never dropped
//!
//! [`RowDirectory::to_set`] returns the ids it could not map alongside the
//! set. An ANN result naming a node this group does not hold (deleted, other
//! partition, other tenant) is a fact the caller must see: silently dropping
//! it makes "the index is stale" and "the candidate was filtered" the same
//! observation, and R26 measured where that reading ends.

use std::collections::BTreeMap;

use crate::{RowIdError, RowIdSet};

/// A group's id↔offset mapping.
#[derive(Debug, Default, Clone)]
pub struct RowDirectory {
    by_id: BTreeMap<u64, usize>,
    by_offset: Vec<u64>,
}

impl RowDirectory {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from the group's entity ids in iteration order — typically a
    /// `Store::scan` of the group's NODE rows, whose body order is the
    /// memcomparable order and therefore deterministic.
    pub fn from_ids(ids: impl IntoIterator<Item = u64>) -> Self {
        let mut d = RowDirectory::new();
        for id in ids {
            d.intern(id);
        }
        d
    }

    /// The offset for `id`, assigning the next dense offset if new.
    pub fn intern(&mut self, id: u64) -> usize {
        if let Some(&o) = self.by_id.get(&id) {
            return o;
        }
        let o = self.by_offset.len();
        self.by_offset.push(id);
        self.by_id.insert(id, o);
        o
    }

    /// The offset for `id`, if mapped.
    pub fn offset_of(&self, id: u64) -> Option<usize> {
        self.by_id.get(&id).copied()
    }

    /// The id at `offset`, if assigned.
    pub fn id_of(&self, offset: usize) -> Option<u64> {
        self.by_offset.get(offset).copied()
    }

    /// Rows in the group — the capacity every [`RowIdSet`] over it must carry.
    pub fn len(&self) -> usize {
        self.by_offset.len()
    }

    /// Whether the directory is empty.
    pub fn is_empty(&self) -> bool {
        self.by_offset.is_empty()
    }

    /// Map entity ids into a set over this group.
    ///
    /// Returns the set AND the ids that are not in this group. The unmapped
    /// list is part of the result, not a log line: the caller decides whether
    /// "3 of 10 candidates are outside the group" is routine (a multi-group
    /// query) or a finding (a stale index).
    pub fn to_set(&self, ids: &[u64]) -> Result<(RowIdSet, Vec<u64>), RowIdError> {
        let mut set = RowIdSet::empty(self.len());
        let mut unmapped = Vec::new();
        for &id in ids {
            match self.offset_of(id) {
                Some(o) => set.insert(o)?,
                None => unmapped.push(id),
            }
        }
        Ok((set, unmapped))
    }

    /// The ids for a set's offsets, ascending by offset.
    ///
    /// An offset the directory never assigned is an ERROR, not a skip: the set
    /// was built against a different (or newer) directory, and resolving what
    /// can be resolved would return a plausible subset with no sign that the
    /// universes diverged.
    pub fn to_ids(&self, set: &RowIdSet) -> Result<Vec<u64>, RowIdError> {
        if set.capacity() != self.len() {
            return Err(RowIdError::CapacityMismatch {
                left: set.capacity(),
                right: self.len(),
            });
        }
        set.iter()
            .map(|o| {
                self.id_of(o).ok_or(RowIdError::OutOfRange {
                    offset: o,
                    capacity: self.len(),
                })
            })
            .collect()
    }
}
