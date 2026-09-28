//! Compiling a pattern once per statement, not once per row.
//!
//! # Why a thread-local and not a field
//!
//! The evaluator's binary-operator dispatch is a FREE FUNCTION with no context
//! object — `bin(op, lhs, rhs)` — so there is nowhere to hang a cache without
//! threading a parameter through every arm of the expression evaluator for the
//! benefit of one operator. A thread-local is the smaller change, and it is
//! sound here for a reason worth stating: the engine runs one statement on one
//! thread, so a per-thread cache is per-statement-ish by construction and is
//! never shared between workers.
//!
//! # Why a `Vec` and not a map
//!
//! `HashMap` is a denied type in this workspace, for iteration-order reasons
//! that apply here too, and a `BTreeMap` would allocate a `String` key on
//! every miss. At [`CACHE_CAP`] entries a linear scan of `&str` comparisons
//! beats both, and move-to-front makes the eviction order a pure function of
//! the access sequence.
//!
//! # Why nothing here is COUNTED
//!
//! This cache outlives a statement — it is thread-local, so its contents
//! depend on everything that ran before on the same thread. Counting hits and
//! misses would therefore put execution HISTORY into the trace the determinism
//! gate hashes, and two runs of the same seed would disagree purely because
//! one of them found a pattern already compiled. The determinism gate caught
//! exactly that. The cache's effect is measured by the `--no-regex-cache`
//! lever and a benchmark, which is where a performance claim belongs anyway.

use std::cell::RefCell;
use std::sync::Arc;

use super::{Regex, RegexError};

/// How many compiled patterns to keep per thread.
///
/// A statement has a handful of distinct regex literals; sixty-four is far
/// above that and small enough that the linear scan is trivial.
const CACHE_CAP: usize = 64;

thread_local! {
    static CACHE: RefCell<Vec<(String, Arc<regex::Regex>)>> = const { RefCell::new(Vec::new()) };
    static ENABLED: RefCell<bool> = const { RefCell::new(true) };
}

/// Turn the compile cache off.
///
/// The lever behind the A/B that shows the cache is doing something. Without
/// it, "the cache helps" is an assertion rather than a measurement.
pub fn set_compile_cache(on: bool) {
    ENABLED.with(|e| *e.borrow_mut() = on);
}

/// Compile `pattern`, reusing an already-compiled program where possible.
pub fn compile_cached(pattern: &str) -> Result<Regex, RegexError> {
    if !ENABLED.with(|e| *e.borrow()) {
        return Regex::compile(pattern);
    }
    let hit = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let found = c.iter().position(|(p, _)| p == pattern);
        found.map(|i| {
            // Move to front, so the eviction order is a pure function of the
            // access sequence.
            let entry = c.remove(i);
            let inner = Arc::clone(&entry.1);
            c.insert(0, entry);
            inner
        })
    });
    if let Some(inner) = hit {
        return Ok(Regex::from_inner(inner));
    }
    let re = Regex::compile(pattern)?;
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.insert(0, (pattern.to_string(), re.inner_arc()));
        if c.len() > CACHE_CAP {
            // Not an event either, for the reason above: WHEN this happens is
            // a property of the thread's history, not of the statement.
            c.truncate(CACHE_CAP);
        }
    });
    Ok(re)
}
