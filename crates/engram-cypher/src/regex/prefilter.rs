//! Regex to a trigram query — what a matching value MUST contain.
//!
//! # The invariant, and it is the only thing that matters here
//!
//! **THE QUERY THIS MODULE DERIVES IS A *NECESSARY* CONDITION, NEVER A
//! SUFFICIENT ONE.** Every value the index returns for it is then re-verified
//! by actually running the regex. So the two ways to be wrong are not
//! symmetric:
//!
//! - A query WIDER than necessary costs time. The index returns extra
//!   candidates, the verifier rejects them, the answer is right.
//! - A query NARROWER than necessary loses rows, silently, with nothing
//!   anywhere looking wrong.
//!
//! The code is written accordingly: every rule returns [`TrigramQuery::All`] —
//! "I cannot constrain this, scan it" — the moment it is unsure, and the test
//! suite is built around generating patterns and strings and asserting that
//! *every genuine match satisfies the derived query*, rather than around
//! checking that the query looks plausible.
//!
//! # The analysis
//!
//! This is Cox's `RegexpQuery` from Google Code Search, computed bottom-up
//! over `regex_syntax`'s HIR — **the same tree the matcher itself is built
//! from**, which is the reason the analysis is written against it rather than
//! against a parser of our own. A second parser would be a second opinion
//! about what a pattern means, and the one thing this analysis cannot survive
//! is disagreeing with the matcher.
//!
//! Each node yields an `Info`:
//!
//! - `exact` — the complete set of strings this node matches, when that set is
//!   finite and small.
//! - `prefix` / `suffix` — the possible leading and trailing fragments, used
//!   when `exact` is not available.
//! - `query` — what is already known to be required.
//!
//! THE LOAD-BEARING RULE IS `Concat`'s CROSS TERM. Concatenating a node whose
//! suffixes are `{"ab"}` with one whose prefixes are `{"cd"}` does not merely
//! require each side's trigrams — it requires the ones that SPAN the join,
//! `abc` and `bcd`. Without that term a four-character literal split across
//! two nodes constrains nothing at all, because neither half reaches three
//! characters.

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind, Repetition};

/// The character that stands before the start of an indexed value.
///
/// **Padding is a deliberate divergence from Google Code Search, which does
/// not pad.** `=~` is a FULL match, so every pattern is implicitly anchored at
/// both ends, and the sentinels are what turn that anchoring into ordinary
/// trigrams: `^ab` becomes a requirement for `\x02\x02a`, as selective as any
/// other. They also make `ENDS WITH` indexable down to a single character,
/// which no range index can do at all — a suffix is not a contiguous range in
/// any sort order. The cost is four extra trigrams per indexed value.
pub const START: char = '\u{2}';

/// The character that stands after the end of an indexed value. See [`START`].
pub const END: char = '\u{3}';

/// How many exact strings a node may carry before its set is converted.
const EXACT_MAX: usize = 8;

/// How long an exact string may be before the set is converted.
const EXACT_LEN_MAX: usize = 12;

/// How many characters a class may hold and still be enumerated into strings.
/// `[a-z]` is worth expanding; `\w` is not, and `.` certainly is not.
const CLASS_MAX: usize = 16;

/// How many HIR nodes the analysis will visit before giving up.
///
/// Over budget yields `All` — a scan — rather than a partial analysis, because
/// a partial analysis is exactly how a query ends up narrower than it should
/// be.
const NODE_BUDGET: u32 = 10_000;

/// Three consecutive characters of an indexed value, after case folding.
///
/// **Characters and not bytes.** A three-BYTE window can begin or end in the
/// middle of a UTF-8 sequence, and the analysis would then have to reason
/// about the *encodings* of character classes to keep the superset argument
/// sound — which is precisely where a narrowing bug would live. Four times the
/// key width buys a proof, and a later on-disk version can delta-encode
/// without touching any of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Trigram(pub char, pub char, pub char);

/// A boolean condition over trigrams that a matching value must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrigramQuery {
    /// Every value is a candidate — the index cannot help, scan instead.
    All,
    /// No value can match. A provable empty answer.
    None,
    /// The value must contain this trigram.
    Lit(Trigram),
    /// Every branch must hold.
    And(Vec<TrigramQuery>),
    /// At least one branch must hold.
    Or(Vec<TrigramQuery>),
}

impl TrigramQuery {
    fn and(self, other: TrigramQuery) -> TrigramQuery {
        match (self, other) {
            (TrigramQuery::None, _) | (_, TrigramQuery::None) => TrigramQuery::None,
            (TrigramQuery::All, o) | (o, TrigramQuery::All) => o,
            (TrigramQuery::And(mut a), TrigramQuery::And(b)) => {
                a.extend(b);
                TrigramQuery::And(a)
            }
            (TrigramQuery::And(mut a), o) => {
                a.push(o);
                TrigramQuery::And(a)
            }
            (o, TrigramQuery::And(mut b)) => {
                b.insert(0, o);
                TrigramQuery::And(b)
            }
            (a, b) => TrigramQuery::And(vec![a, b]),
        }
    }

    fn or(self, other: TrigramQuery) -> TrigramQuery {
        match (self, other) {
            // AN `All` ON EITHER SIDE OF AN `Or` SWALLOWS THE WHOLE THING. If
            // one branch of an alternation constrains nothing, the alternation
            // constrains nothing — `(foo|.*)` can match anything at all. This
            // is the single easiest place to write a narrowing bug.
            (TrigramQuery::All, _) | (_, TrigramQuery::All) => TrigramQuery::All,
            (TrigramQuery::None, o) | (o, TrigramQuery::None) => o,
            (TrigramQuery::Or(mut a), TrigramQuery::Or(b)) => {
                a.extend(b);
                TrigramQuery::Or(a)
            }
            (TrigramQuery::Or(mut a), o) => {
                a.push(o);
                TrigramQuery::Or(a)
            }
            (o, TrigramQuery::Or(mut b)) => {
                b.insert(0, o);
                TrigramQuery::Or(b)
            }
            (a, b) => TrigramQuery::Or(vec![a, b]),
        }
    }

    /// Whether this query constrains nothing.
    #[must_use]
    pub fn is_all(&self) -> bool {
        matches!(self, TrigramQuery::All)
    }

    /// Whether a value holding exactly `have` satisfies this query.
    ///
    /// The reference implementation of what an index probe computes, and what
    /// the generative test checks a real match against.
    #[must_use]
    pub fn satisfied_by(&self, have: &BTreeSet<Trigram>) -> bool {
        match self {
            TrigramQuery::All => true,
            TrigramQuery::None => false,
            TrigramQuery::Lit(t) => have.contains(t),
            TrigramQuery::And(qs) => qs.iter().all(|q| q.satisfied_by(have)),
            TrigramQuery::Or(qs) => qs.iter().any(|q| q.satisfied_by(have)),
        }
    }

    /// Every trigram this query could ever ask an index for.
    ///
    /// Used to price a probe before running it.
    #[must_use]
    pub fn trigrams(&self) -> BTreeSet<Trigram> {
        fn walk(q: &TrigramQuery, out: &mut BTreeSet<Trigram>) {
            match q {
                TrigramQuery::Lit(t) => {
                    out.insert(*t);
                }
                TrigramQuery::And(qs) | TrigramQuery::Or(qs) => {
                    for q in qs {
                        walk(q, out);
                    }
                }
                TrigramQuery::All | TrigramQuery::None => {}
            }
        }
        let mut out = BTreeSet::new();
        walk(self, &mut out);
        out
    }
}

/// Fold one character the way the index folds it.
///
/// **Single scalar, deliberately**, and the same function the index extraction
/// uses — the two must agree exactly, or a query asks for trigrams the index
/// never stored.
#[must_use]
pub fn fold_scalar(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    c.to_lowercase().next().unwrap_or(c)
}

/// The trigrams of an indexed value, padded with the sentinels.
///
/// This is the extraction the index performs and the one every query below is
/// written against; keeping both here means they cannot disagree about padding
/// or folding.
#[must_use]
pub fn trigrams_of_value(s: &str) -> BTreeSet<Trigram> {
    let mut chars = Vec::with_capacity(s.chars().count() + 4);
    chars.push(START);
    chars.push(START);
    chars.extend(s.chars().map(fold_scalar));
    chars.push(END);
    chars.push(END);
    let mut out = BTreeSet::new();
    for w in chars.windows(3) {
        out.insert(Trigram(w[0], w[1], w[2]));
    }
    out
}

fn trigrams_of_fragment(s: &[char]) -> Vec<Trigram> {
    s.windows(3).map(|w| Trigram(w[0], w[1], w[2])).collect()
}

fn require_all(s: &[char]) -> TrigramQuery {
    let mut q = TrigramQuery::All;
    for t in trigrams_of_fragment(s) {
        q = q.and(TrigramQuery::Lit(t));
    }
    q
}

/// What is known about one node of the pattern.
#[derive(Debug, Clone)]
struct Info {
    /// Whether this node can match the empty string.
    match_empty: bool,
    /// The complete set of strings this node matches, when finite and small.
    exact: Option<BTreeSet<Vec<char>>>,
    /// Possible leading fragments, when `exact` is unavailable.
    prefix: BTreeSet<Vec<char>>,
    /// Possible trailing fragments, when `exact` is unavailable.
    suffix: BTreeSet<Vec<char>>,
    /// What is already required.
    query: TrigramQuery,
}

impl Info {
    /// The node that constrains nothing. Every give-up path returns this.
    fn any() -> Info {
        Info {
            match_empty: true,
            exact: None,
            prefix: [Vec::new()].into_iter().collect(),
            suffix: [Vec::new()].into_iter().collect(),
            query: TrigramQuery::All,
        }
    }

    /// The node that matches nothing at all — a provable empty answer.
    fn never() -> Info {
        Info {
            match_empty: false,
            exact: None,
            prefix: BTreeSet::new(),
            suffix: BTreeSet::new(),
            query: TrigramQuery::None,
        }
    }

    fn exact_of(set: BTreeSet<Vec<char>>) -> Info {
        Info {
            match_empty: set.iter().any(std::vec::Vec::is_empty),
            exact: Some(set),
            prefix: BTreeSet::new(),
            suffix: BTreeSet::new(),
            query: TrigramQuery::All,
        }
    }

    fn exact_is_small(set: &BTreeSet<Vec<char>>) -> bool {
        set.len() <= EXACT_MAX && set.iter().all(|s| s.len() <= EXACT_LEN_MAX)
    }

    /// The trigrams a set of fragments guarantees.
    ///
    /// A fragment shorter than three characters implies no trigram, so a set
    /// containing even one short member contributes NOTHING — `(a|foo)` must
    /// not require `foo`, because a value of "a" matches it.
    fn fragments_require(set: &BTreeSet<Vec<char>>) -> TrigramQuery {
        if set.is_empty() || set.iter().any(|s| s.len() < 3) {
            return TrigramQuery::All;
        }
        let mut q = TrigramQuery::None;
        for s in set {
            q = q.or(require_all(s));
        }
        q
    }

    /// Everything this node guarantees: its accumulated query, plus what its
    /// own strings imply.
    ///
    /// Safe at any depth and not only at the top: a sub-expression's match is
    /// a substring of the value, so a trigram lying wholly inside a fragment
    /// the sub-expression must produce does appear in the value.
    fn required(&self) -> TrigramQuery {
        let own = match &self.exact {
            Some(x) => Info::fragments_require(x),
            None => {
                Info::fragments_require(&self.prefix).and(Info::fragments_require(&self.suffix))
            }
        };
        self.query.clone().and(own)
    }
}

/// Every way of following a string from `a` with one from `b`.
///
/// `None` when the product would be too large to carry, which the caller reads
/// as "stop being exact" — never as "there are none".
fn cross(a: &BTreeSet<Vec<char>>, b: &BTreeSet<Vec<char>>) -> Option<BTreeSet<Vec<char>>> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a.len().saturating_mul(b.len()) > EXACT_MAX * EXACT_MAX {
        return None;
    }
    let mut out = BTreeSet::new();
    for x in a {
        for y in b {
            if x.len() + y.len() > EXACT_LEN_MAX * 2 {
                return None;
            }
            let mut s = x.clone();
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    Some(out)
}

/// The trigrams that SPAN a join — the load-bearing term of the analysis.
///
/// For every way the left can end and the right can begin, the joined text
/// must appear. Only the windows straddling the boundary count; the ones
/// wholly inside either side are that side's own business. If ANY pairing is
/// too short to span, the whole join constrains nothing — that pairing is a
/// way the match could happen.
fn join_query(suffixes: &BTreeSet<Vec<char>>, prefixes: &BTreeSet<Vec<char>>) -> TrigramQuery {
    if suffixes.is_empty() || prefixes.is_empty() {
        return TrigramQuery::All;
    }
    let mut q = TrigramQuery::None;
    for s in suffixes {
        for p in prefixes {
            let mut joined = s.clone();
            let split = joined.len();
            joined.extend_from_slice(p);
            if joined.len() < 3 {
                return TrigramQuery::All;
            }
            let lo = split.saturating_sub(2);
            let hi = (split + 2).min(joined.len());
            let mut branch = TrigramQuery::All;
            if hi >= lo + 3 {
                for w in joined[lo..hi].windows(3) {
                    branch = branch.and(TrigramQuery::Lit(Trigram(w[0], w[1], w[2])));
                }
            }
            q = q.or(branch);
        }
    }
    q
}

/// The shortest member of a fragment set, or zero when it is empty.
fn min_len(set: &BTreeSet<Vec<char>>) -> usize {
    set.iter().map(std::vec::Vec::len).min().unwrap_or(0)
}

/// Whether a fragment set has grown past what is worth carrying.
fn over_cap(set: &BTreeSet<Vec<char>>) -> bool {
    set.len() > EXACT_MAX || set.iter().any(|s| s.len() > EXACT_LEN_MAX)
}

/// The fragment set that says "I know nothing about this end".
///
/// **`{""}` AND NOT `{}`.** The distinction is the single most dangerous thing
/// in this module. An EMPTY set is a claim that no fragment is possible, which
/// `alternate` reads as "this branch contributes no way for the value to
/// start" and `concat` reads as "there is nothing here to join across". A set
/// containing the EMPTY FRAGMENT is a claim that a fragment is possible and
/// unknown, which makes `fragments_require` and `join_query` degrade to `All`.
///
/// Three confirmed narrowing bugs came from writing `{}` where this was meant:
/// `(v[0-9][0-9]|beta)` silently lost the value `"v42"`, because the
/// over-large left branch stored `{}`, the alternation unioned it away, and
/// the whole pattern ended up requiring `beta`'s trigrams.
fn unknown_fragments() -> BTreeSet<Vec<char>> {
    [Vec::new()].into_iter().collect()
}

/// Keep a fragment set small, TRUNCATING TOWARDS "constrains less".
///
/// Dropping a member would make the join term claim fewer joins are possible,
/// which is a NARROWING — the direction that loses rows. So an oversized set
/// collapses to the empty fragment, which makes `join_query` return `All`.
fn cap(mut set: BTreeSet<Vec<char>>) -> BTreeSet<Vec<char>> {
    if over_cap(&set) {
        set = [Vec::new()].into_iter().collect();
    }
    set
}

/// The characters of a class, if there are few enough to enumerate.
///
/// A wide class constrains nothing and returns `None` rather than materialise
/// a million characters — which would also be an allocation whose size the
/// author of the pattern chooses.
fn class_chars(class: &Class) -> Option<Vec<char>> {
    match class {
        Class::Unicode(u) => {
            let mut out = Vec::new();
            for r in u.iter() {
                let (lo, hi) = (r.start() as u32, r.end() as u32);
                if hi.saturating_sub(lo) as usize > CLASS_MAX {
                    return None;
                }
                for cp in lo..=hi {
                    if out.len() >= CLASS_MAX {
                        return None;
                    }
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                    }
                }
            }
            Some(out)
        }
        // An EMPTY byte class is how the parser normalises a class that can
        // never match — `[^\s\S]`, say — so it is a provable empty answer
        // rather than something to decline. Reading it as "a byte class, do not
        // reason about it" is safe but throws away the best answer available.
        Class::Bytes(b) if b.ranges().is_empty() => Some(Vec::new()),
        // Any other byte class can name a value that is not a character on its
        // own, and reasoning about UTF-8 encodings is exactly where a narrowing
        // bug lives. Decline.
        Class::Bytes(_) => None,
    }
}

struct Analysis {
    budget: u32,
}

impl Analysis {
    fn info(&mut self, hir: &Hir) -> Info {
        if self.budget == 0 {
            return Info::any();
        }
        self.budget -= 1;
        match hir.kind() {
            HirKind::Empty => Info::exact_of([Vec::new()].into_iter().collect()),
            // A literal here is a whole RUN of characters, not one character —
            // which is one of the reasons this analysis is written against the
            // matcher's own tree. A parser that split `bar` into three nodes
            // would fold them pairwise into two-character fragments and
            // require nothing at all.
            HirKind::Literal(lit) => match std::str::from_utf8(&lit.0) {
                Ok(s) => {
                    let chars: Vec<char> = s.chars().map(fold_scalar).collect();
                    Info::exact_of([chars].into_iter().collect())
                }
                // Not valid UTF-8, so it cannot be part of a string value we
                // indexed. Decline rather than guess at its encoding.
                Err(_) => Info::any(),
            },
            HirKind::Class(class) => match class_chars(class) {
                Some(chars) if !chars.is_empty() => Info::exact_of(
                    chars
                        .into_iter()
                        .map(|c| vec![fold_scalar(c)])
                        .collect::<BTreeSet<_>>(),
                ),
                // An empty class matches nothing at all — a provable empty
                // answer rather than a scan.
                Some(_) => Info::never(),
                None => Info::any(),
            },
            // EVERY ASSERTION CONTRIBUTES THE EMPTY STRING, ANCHORS INCLUDED.
            // `query_for_regex` already wraps the pattern in sentinels because
            // `=~` is a full match, so an explicit `^` that ALSO emitted them
            // would require four in a row — a trigram no value can hold, which
            // is a query that matches nothing. The generative test caught
            // exactly that when this rule was first written the other way.
            HirKind::Look(_) => Info::exact_of([Vec::new()].into_iter().collect()),
            HirKind::Capture(c) => self.info(&c.sub),
            HirKind::Concat(parts) => {
                let mut acc = Info::exact_of([Vec::new()].into_iter().collect());
                for p in parts {
                    let next = self.info(p);
                    acc = Self::concat(acc, next);
                }
                acc
            }
            HirKind::Alternation(branches) => {
                let mut it = branches.iter();
                let Some(first) = it.next() else {
                    return Info::any();
                };
                let mut acc = self.info(first);
                for b in it {
                    let next = self.info(b);
                    acc = Self::alternate(acc, next);
                }
                acc
            }
            HirKind::Repetition(Repetition { min, sub, .. }) => {
                let inner = self.info(sub);
                if *min == 0 {
                    // `x*`, `x?`, `x{0,n}` all match the empty string, so they
                    // REQUIRE NOTHING. This is the rule that correctly turns
                    // `foo.*bar` into a requirement for both literals rather
                    // than into something that mentions the `.*`.
                    return Info::any();
                }
                // At least one copy must appear, so whatever one copy
                // guarantees is guaranteed. The exact set is dropped: an
                // unbounded repetition matches infinitely many strings even
                // though each single copy is known.
                let query = inner.required();
                let (prefix, suffix) = match &inner.exact {
                    Some(x) => (x.clone(), x.clone()),
                    None => (inner.prefix.clone(), inner.suffix.clone()),
                };
                Info {
                    match_empty: inner.match_empty,
                    exact: None,
                    prefix: cap(prefix),
                    suffix: cap(suffix),
                    query,
                }
            }
        }
    }

    /// Concatenation — where the analysis earns its keep.
    ///
    /// Exactness is carried THROUGH the fold rather than collapsed at each
    /// step, so a literal run that follows a `.*` still accumulates into a
    /// fragment long enough to imply a trigram.
    fn concat(x: Info, y: Info) -> Info {
        let match_empty = x.match_empty && y.match_empty;
        if let (Some(xe), Some(ye)) = (&x.exact, &y.exact) {
            if let Some(prod) = cross(xe, ye) {
                if Info::exact_is_small(&prod) {
                    let mut out = Info::exact_of(prod);
                    out.query = x.query.and(y.query);
                    return out;
                }
            }
        }
        // EACH SIDE'S OWN GUARANTEE, not merely its accumulated query.
        //
        // Exactness is about to be abandoned, and with it the strings each side
        // is known to match. Those strings must still be REQUIRED — both sides
        // of a concatenation have to match, so a trigram lying inside either
        // one appears in the value. Taking only `query` here loses them, and it
        // loses them silently: `.*zqx.*` derived nothing at all, because the
        // trailing `.*` unions the empty fragment into the suffix set and an
        // empty member makes `fragments_require` yield All. The index declined
        // a pattern it should have answered in one posting. Correct, useless,
        // and invisible without a benchmark - which is how it was found.
        let mut query = x.required().and(y.required());

        // A leading fragment of the whole is a leading fragment of the left,
        // extended through the right only when the left is exact (and so
        // contributes all of itself) or can vanish (and so may contribute
        // nothing).
        let mut prefix = match &x.exact {
            Some(xe) => cross(xe, &y.prefix).unwrap_or_else(unknown_fragments),
            None => {
                // A left side that knows nothing about its start says so with
                // `{""}`; extending THAT with the right side's prefixes would
                // promote them to the whole concatenation's, claiming the value
                // must begin the way the right side does. Only a left side with
                // real fragments may be extended.
                let mut p = if x.prefix.is_empty() {
                    unknown_fragments()
                } else {
                    x.prefix.clone()
                };
                if x.match_empty {
                    p.extend(y.prefix.iter().cloned());
                }
                p
            }
        };
        let mut suffix = match &y.exact {
            Some(ye) => cross(&x.suffix, ye).unwrap_or_else(unknown_fragments),
            None => {
                let mut sfx = if y.suffix.is_empty() {
                    unknown_fragments()
                } else {
                    y.suffix.clone()
                };
                if y.match_empty {
                    sfx.extend(x.suffix.iter().cloned());
                }
                sfx
            }
        };

        // THE CROSS TERM. Only when neither side is exact — an exact side has
        // already been folded into the fragments above — and only when the two
        // fragments together reach three characters, since otherwise nothing
        // spans the join.
        if x.exact.is_none() && y.exact.is_none() && min_len(&x.suffix) + min_len(&y.prefix) >= 3 {
            query = query.and(join_query(&x.suffix, &y.prefix));
        }

        // A set about to be dropped for size gives up what it knows FIRST.
        if over_cap(&prefix) {
            query = query.and(Info::fragments_require(&prefix));
            prefix = [Vec::new()].into_iter().collect();
        }
        if over_cap(&suffix) {
            query = query.and(Info::fragments_require(&suffix));
            suffix = [Vec::new()].into_iter().collect();
        }
        Info {
            match_empty,
            exact: None,
            prefix,
            suffix,
            query,
        }
    }

    fn alternate(x: Info, y: Info) -> Info {
        if let (Some(xe), Some(ye)) = (&x.exact, &y.exact) {
            let mut u = xe.clone();
            u.extend(ye.iter().cloned());
            if Info::exact_is_small(&u) {
                let mut out = Info::exact_of(u);
                out.query = x.query.and(y.query);
                return out;
            }
        }
        // Neither side survives as exact, so each states what it guarantees
        // before the branches are OR-ed. An `All` from either branch swallows
        // the result, which is what stops `(foo|.*)` requiring anything.
        let xr = x.required();
        let yr = y.required();
        // THE UNION MUST NOT LOSE A BRANCH. A branch whose fragment set is
        // empty knows nothing about that end, and a value matching THAT branch
        // may start or end any way at all — so the alternation as a whole knows
        // nothing either. Unioning an empty set in silently drops that
        // possibility and leaves the other branch's fragments looking
        // mandatory, which is how `(v[0-9][0-9]|beta)` came to require `beta`.
        let ends = |exact: &Option<BTreeSet<Vec<char>>>, frag: &BTreeSet<Vec<char>>| match exact {
            Some(x) => x.clone(),
            None if frag.is_empty() => unknown_fragments(),
            None => frag.clone(),
        };
        let mut prefix = ends(&x.exact, &x.prefix);
        prefix.extend(ends(&y.exact, &y.prefix));
        let mut suffix = ends(&x.exact, &x.suffix);
        suffix.extend(ends(&y.exact, &y.suffix));
        Info {
            match_empty: x.match_empty || y.match_empty,
            exact: None,
            prefix: cap(prefix),
            suffix: cap(suffix),
            query: xr.or(yr),
        }
    }
}

/// Parse a pattern to the HIR this analysis reads.
///
/// The same parser the matcher uses, deliberately — see the module docs.
pub fn hir_of(pattern: &str) -> Result<Hir, super::RegexError> {
    regex_syntax::parse(pattern).map_err(|e| super::RegexError::Syntax {
        detail: e.to_string(),
    })
}

/// The trigram condition every value matching this pattern must satisfy.
///
/// The pattern is treated as ANCHORED at both ends, because `=~` is a full
/// match: the sentinels are applied around the analysed pattern, so `^ab` and
/// `ab` derive the same requirement.
#[must_use]
pub fn query_for_regex(hir: &Hir) -> TrigramQuery {
    let mut a = Analysis {
        budget: NODE_BUDGET,
    };
    let info = a.info(hir);
    // The sentinels are applied to the analysed fragments rather than to the
    // pattern TEXT, so that a pattern is parsed exactly once and by the same
    // parser the matcher used.
    let start: BTreeSet<Vec<char>> = [vec![START, START]].into_iter().collect();
    let end: BTreeSet<Vec<char>> = [vec![END, END]].into_iter().collect();
    let anchored = Analysis::concat(
        Analysis::concat(Info::exact_of(start), info),
        Info::exact_of(end),
    );
    // `required()` and not merely `query`: at the top, the accumulated prefix
    // and suffix are leading and trailing fragments OF THE WHOLE VALUE, so
    // their trigrams are exactly as required as anything already in the query.
    // This is what turns `foo.*bar` into a requirement for both literals
    // rather than for neither.
    anchored.required()
}

/// The trigram condition for `CONTAINS needle`.
///
/// No sentinels: the needle may appear anywhere. A needle under three
/// characters implies no trigram and yields `All`.
#[must_use]
pub fn query_for_contains(needle: &str) -> TrigramQuery {
    let chars: Vec<char> = needle.chars().map(fold_scalar).collect();
    require_all(&chars)
}

/// The trigram condition for `STARTS WITH prefix`.
///
/// The start sentinels make even a one-character prefix indexable. A range
/// index still answers a prefix better — as one contiguous range — and is
/// expected to win the planner's fewest-candidates comparison on its merits,
/// with no special case here.
#[must_use]
pub fn query_for_starts_with(prefix: &str) -> TrigramQuery {
    let mut chars = vec![START, START];
    chars.extend(prefix.chars().map(fold_scalar));
    require_all(&chars)
}

/// The trigram condition for `ENDS WITH suffix`.
///
/// Indexable down to a single character, which no range index can do at all.
#[must_use]
pub fn query_for_ends_with(suffix: &str) -> TrigramQuery {
    let mut chars: Vec<char> = suffix.chars().map(fold_scalar).collect();
    chars.push(END);
    chars.push(END);
    require_all(&chars)
}
