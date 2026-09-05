//! The fault-injecting tier — the simulation lane's network.
//!
//! Wraps any tier and injects the failures the taxonomy names, from a seeded
//! deterministic plan: outages, throttles, a flipped byte on read, a truncated
//! beacon listing, an under-answered presence query. Every guard in this crate
//! exists because one of these happens on a real network; the sweep drives
//! them every seed so the guards' events stay on the coverage floor rather
//! than in a code path nothing reaches.

use std::cell::RefCell;

use engram_observe::sometimes;

use crate::{ObjectName, ObjectTier, TierError};

/// Which faults the wrapper may inject, each as a per-operation probability
/// in [0, 255] (0 = never, 255 = always).
#[derive(Debug, Clone, Copy)]
pub struct FaultPlan {
    /// Puts and gets fail `Unavailable`.
    pub unavailable: u8,
    /// Puts and gets fail `Throttled`.
    pub throttled: u8,
    /// A fetched payload has one byte flipped — the integrity check's diet.
    pub flip_byte: u8,
    /// A beacon listing loses its LARGEST entry — the truncation that makes
    /// discovery see an older world.
    pub truncate_listing: u8,
    /// A presence answer is one short — the Lore fail-open shape.
    pub under_answer: u8,
}

impl FaultPlan {
    /// No faults — the wrapper becomes transparent.
    pub fn none() -> FaultPlan {
        FaultPlan {
            unavailable: 0,
            throttled: 0,
            flip_byte: 0,
            truncate_listing: 0,
            under_answer: 0,
        }
    }
}

/// A deterministic fault-injecting wrapper over any tier.
pub struct FaultTier<T> {
    inner: T,
    plan: FaultPlan,
    // SplitMix64 — the same generator the runtime uses, restated locally so
    // this crate needs no dev-only dependency in its library half.
    state: RefCell<u64>,
}

impl<T> FaultTier<T> {
    /// Wrap `inner` with `plan`, seeded.
    pub fn new(inner: T, plan: FaultPlan, seed: u64) -> FaultTier<T> {
        FaultTier {
            inner,
            plan,
            state: RefCell::new(seed),
        }
    }

    /// The wrapped tier.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    fn next(&self) -> u64 {
        let mut s = self.state.borrow_mut();
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn roll(&self, probability: u8) -> bool {
        // 255 is ALWAYS, not 255/256: the plans that pin a fault on rely on
        // it firing every time, and a 1-in-256 quiet pass would make one seed
        // in ~256 assert against an injection that never happened.
        probability == 255 || (probability > 0 && ((self.next() & 0xFF) as u8) < probability)
    }

    fn injected(&self, what: &str) -> TierError {
        sometimes!("objstore.fault injected", true);
        match what {
            "throttled" => TierError::Throttled,
            _ => TierError::Unavailable {
                detail: format!("injected {what}"),
            },
        }
    }
}

impl<T: ObjectTier> ObjectTier for FaultTier<T> {
    async fn put(&self, name: &ObjectName, bytes: Vec<u8>) -> Result<(), TierError> {
        if self.roll(self.plan.unavailable) {
            return Err(self.injected("outage"));
        }
        if self.roll(self.plan.throttled) {
            return Err(self.injected("throttled"));
        }
        self.inner.put(name, bytes).await
    }

    async fn get(&self, name: &ObjectName) -> Result<Vec<u8>, TierError> {
        if self.roll(self.plan.unavailable) {
            return Err(self.injected("outage"));
        }
        let mut bytes = self.inner.get(name).await?;
        if self.roll(self.plan.flip_byte) && !bytes.is_empty() {
            sometimes!("objstore.fault injected", true);
            let i = (self.next() as usize) % bytes.len();
            bytes[i] ^= 0x01;
        }
        Ok(bytes)
    }

    async fn head(&self, name: &ObjectName) -> Result<usize, TierError> {
        if self.roll(self.plan.unavailable) {
            return Err(self.injected("outage"));
        }
        self.inner.head(name).await
    }

    async fn list_beacons(&self) -> Result<Vec<u64>, TierError> {
        if self.roll(self.plan.unavailable) {
            return Err(self.injected("outage"));
        }
        let mut seqs = self.inner.list_beacons().await?;
        if self.roll(self.plan.truncate_listing) {
            sometimes!("objstore.fault injected", true);
            if let Some(max) = seqs.iter().copied().max() {
                seqs.retain(|&s| s != max);
            }
        }
        Ok(seqs)
    }

    async fn present(&self, names: &[ObjectName]) -> Result<Vec<bool>, TierError> {
        if self.roll(self.plan.unavailable) {
            return Err(self.injected("outage"));
        }
        let mut answers = self.inner.present(names).await?;
        if self.roll(self.plan.under_answer) && !answers.is_empty() {
            sometimes!("objstore.fault injected", true);
            answers.pop();
        }
        Ok(answers)
    }
}
