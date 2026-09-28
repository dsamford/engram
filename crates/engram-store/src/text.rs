//! The full-text analyzer — ONE of it, below every consumer.
//!
//! Three things tokenise text in this engine: the index build, the incremental
//! catch-up, and the fallback scan that answers when the index declines. If any
//! two of them disagree about what a token is, a query returns different rows
//! depending on which path served it — and the difference would show up as a
//! ranking that drifts rather than as anything that looks like a bug.
//!
//! So there is one function, it lives below all three, and the crate that owns
//! the index owns it. This is unlike `trigram::fold_scalar`, which genuinely
//! must exist twice because `engram-cypher` cannot depend on `engram-store`;
//! here `engram-graph` depends on `engram-store` already, so a delegation costs
//! nothing and a duplicate would cost correctness.
//!
//! # What it does, and what it does not
//!
//! Split on anything that is not alphanumeric, drop the empties, lowercase.
//! That is all. **No stemming, no stop list, no synonyms, no length cap, no
//! configurable analyzer**, and the absences are stated here rather than
//! discovered: a user expecting `running` to match `run` will not get it.
//!
//! `char::is_alphanumeric` is Alphabetic, Nd, Nl or No — so `_` and `-` are
//! separators, and `é`, `Ω` and `٣` are token characters.

/// Split `text` into lowercased tokens.
///
/// **THE ORDER IS LOAD-BEARING: SPLIT FIRST, THEN LOWERCASE.**
///
/// `str::to_lowercase` performs full Unicode lowercasing, which can turn one
/// character into several: `İ` (U+0130) lowercases to `i` followed by U+0307,
/// and U+0307 is a combining mark, which is *not* alphanumeric. Lowercasing
/// before splitting would therefore cut that single token in two. The rejected
/// alternative — lowercase the whole string, then split — is named here so
/// nobody reorders these two lines for tidiness.
#[must_use]
pub fn analyse(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The distinct terms of `text` with their counts, ascending, and the field's
/// LENGTH in tokens.
///
/// Both halves come from one pass, and they are returned together on purpose:
/// the length is BM25's `dl` and it counts tokens **with** multiplicity, while
/// the postings need each term once. Computing them separately is how a build
/// and a catch-up come to disagree about what `dl` means.
#[must_use]
pub fn term_freqs(text: &str) -> (Vec<(String, u32)>, u32) {
    let mut terms: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut len = 0u32;
    for t in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        len = len.saturating_add(1);
        *terms.entry(t.to_lowercase()).or_insert(0) += 1;
    }
    (terms.into_iter().collect(), len)
}
