//! The `mutate` result cache.
//!
//! # Why a cached result is NEVER auto-invalidated
//!
//! The projection cache and this one have opposite rules, and the difference
//! is the whole design.
//!
//! A projection is a pure function of committed state, so a stale projection
//! is simply a WRONG one and it rebuilds.
//!
//! A `mutate` result is a function of the state at its `as_of` — a measurement
//! of a past graph, not an approximation of the present one. Auto-invalidating
//! it on an epoch change would make it vanish under any concurrent write,
//! which destroys the compute-once-read-many workflow it exists for; and the
//! only available "refresh" would be a silent multi-minute recompute inside
//! what the user wrote as a property read.
//!
//! **So it is explicitly versioned and never implicitly refreshed.** Every
//! read reports `asOf` alongside the graph's current epoch, so drift is
//! visible rather than surprising; `mutate` overwrites; `drop` removes. An
//! eviction under the memory budget makes the next read an ERROR NAMING THE
//! LEVER rather than an empty answer, because an empty answer would read as
//! "the algorithm found nothing".

use std::collections::BTreeMap;

use engram_observe::counted;

use super::modes::AlgoResult;

/// How many bytes of cached results to keep.
pub const CACHE_BYTES: usize = 512 * 1024 * 1024;

/// One cached result.
#[derive(Debug, Clone)]
pub struct Cached {
    /// The result itself.
    pub result: std::sync::Arc<AlgoResult>,
    /// Its size, for the budget.
    pub bytes: usize,
}

/// The cache: user-chosen key to result.
///
/// A `BTreeMap`, so eviction and listing are in a deterministic order.
#[derive(Debug, Default)]
pub struct ResultCache {
    entries: BTreeMap<String, Cached>,
    bytes: usize,
}

impl ResultCache {
    /// Publish `result` under `key`, overwriting any previous one.
    ///
    /// Returns the keys evicted to make room. A result larger than the whole
    /// budget is REFUSED rather than allowed to evict everything: emptying the
    /// cache to hold one thing that then does not fit either is strictly worse
    /// than declining.
    /// `budget` is passed in rather than read from [`CACHE_BYTES`] because
    /// the ceiling is settable: the constant is only the default, and the
    /// error below names `ENGRAM_ALGO_CACHE_BYTES` as the way to change it.
    /// Reading the constant here would make that advice false.
    pub fn publish(
        &mut self,
        key: &str,
        result: std::sync::Arc<AlgoResult>,
        budget: usize,
    ) -> Result<Vec<String>, String> {
        let bytes = result.bytes();
        if bytes > budget {
            return Err(format!(
                "this result is {bytes} bytes, over the whole cache budget of {budget}. \
                 Raise ENGRAM_ALGO_CACHE_BYTES, or use `stream` rather than `mutate`."
            ));
        }
        if let Some(old) = self.entries.remove(key) {
            self.bytes -= old.bytes;
        }
        let mut evicted = Vec::new();
        // Evict in ascending vintage, then key — a deterministic order, so a
        // trace of which keys went is reproducible.
        while self.bytes + bytes > budget {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(k, c)| (c.result.as_of, (*k).clone()))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(c) = self.entries.remove(&victim) {
                self.bytes -= c.bytes;
            }
            counted!("algo.result evicted for budget");
            evicted.push(victim);
        }
        self.bytes += bytes;
        self.entries
            .insert(key.to_string(), Cached { result, bytes });
        counted!("algo.result published");
        Ok(evicted)
    }

    /// The result under `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<std::sync::Arc<AlgoResult>> {
        self.entries
            .get(key)
            .map(|c| std::sync::Arc::clone(&c.result))
    }

    /// Remove `key`, reporting whether it was there.
    pub fn drop_key(&mut self, key: &str) -> bool {
        match self.entries.remove(key) {
            Some(c) => {
                self.bytes -= c.bytes;
                true
            }
            None => false,
        }
    }

    /// Every cached result, in key order.
    pub fn list(&self) -> impl Iterator<Item = (&String, &Cached)> {
        self.entries.iter()
    }
}
