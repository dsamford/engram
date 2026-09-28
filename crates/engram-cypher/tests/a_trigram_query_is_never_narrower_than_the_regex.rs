//! The trigram prefilter's one invariant, tested asymmetrically.
//!
//! **THE DERIVED QUERY IS A *NECESSARY* CONDITION, NEVER A SUFFICIENT ONE.**
//! An index probe returns candidates and the regex then verifies each one, so:
//!
//! - a query WIDER than necessary costs time and returns the right answer;
//! - a query NARROWER than necessary loses rows, silently, for ever.
//!
//! The two failures are not symmetric, so the tests are not symmetric either.
//! The load-bearing test here is not the table of expected trigrams — that is
//! a readability check — it is `a_trigram_query_is_never_narrower_than_the_regex`,
//! which generates several thousand (pattern, string) pairs and asserts that
//! **every string the regex actually matches satisfies the derived query**.
//! That is the property the index depends on, stated directly.

use std::collections::BTreeSet;

use engram_cypher::regex::Regex;
use engram_cypher::regex::prefilter::hir_of;
use engram_cypher::regex::prefilter::{
    Trigram, TrigramQuery, query_for_contains, query_for_ends_with, query_for_regex,
    query_for_starts_with, trigrams_of_value,
};

fn q(pattern: &str) -> TrigramQuery {
    let hir = hir_of(pattern).unwrap_or_else(|e| panic!("parse `{pattern}`: {e}"));
    query_for_regex(&hir)
}

/// Whether the query requires the trigram spelled by `s`.
fn requires(query: &TrigramQuery, s: &str) -> bool {
    let want: Vec<char> = s.chars().collect();
    assert_eq!(want.len(), 3, "a trigram is three characters");
    let t = Trigram(want[0], want[1], want[2]);
    fn walk(q: &TrigramQuery, t: Trigram) -> bool {
        match q {
            TrigramQuery::Lit(x) => *x == t,
            // Only an `And` makes something REQUIRED; a branch of an `Or` is
            // optional by construction.
            TrigramQuery::And(qs) => qs.iter().any(|x| walk(x, t)),
            _ => false,
        }
    }
    walk(query, t)
}

// ─── The readable table ────────────────────────────────────────────────────

#[test]
fn a_regex_query_analysis_produces_the_expected_trigrams() {
    // `foo.*bar` — the canonical case. The `.*` constrains nothing, so what
    // survives is both literals.
    let query = q("foo.*bar");
    assert!(requires(&query, "foo"), "{query:?}");
    assert!(requires(&query, "bar"), "{query:?}");

    // A literal is anchored at both ends by `=~`'s full-match rule, so the
    // sentinels give it two more trigrams than its own characters.
    let query = q("abcd");
    assert!(requires(&query, "abc"));
    assert!(requires(&query, "bcd"));

    // The cross term: `ab` and `cd` as separate nodes still require the
    // trigrams that SPAN the join. Without it neither fragment reaches three
    // characters and the whole query would collapse to a scan.
    let query = q("(ab)(cd)");
    assert!(
        requires(&query, "abc") || requires(&query, "bcd"),
        "the join must contribute a spanning trigram: {query:?}",
    );
}

#[test]
fn an_unconstraining_pattern_declines_the_index() {
    // Each of these can match anything at all; requiring any trigram would
    // lose rows.
    for pattern in [".*", ".+", "(?s).*", "[a-z]*", "a|.*", "(foo|.*)"] {
        assert!(
            q(pattern).is_all(),
            "`{pattern}` must decline the index, got {:?}",
            q(pattern),
        );
    }
}

#[test]
fn an_alternation_requires_nothing_that_one_branch_can_avoid() {
    // `(foo|a)` matches "a", which contains no trigram of "foo". Requiring
    // `foo` would drop that row.
    let query = q("(foo|a)");
    assert!(
        !requires(&query, "foo"),
        "a short branch must stop the long one being required: {query:?}",
    );
}

#[test]
fn a_starred_group_requires_nothing_but_a_plussed_one_does() {
    assert!(q("(foo)*").is_all(), "`(foo)*` matches the empty string");
    let query = q("(foobar)+");
    assert!(
        requires(&query, "oob") || requires(&query, "foo"),
        "`(foobar)+` guarantees at least one copy: {query:?}",
    );
}

#[test]
fn an_empty_class_proves_an_empty_answer() {
    // `[^\s\S]` is the negation of "space or not space" — empty by
    // construction, and expressible in the subset this engine supports.
    assert_eq!(q("a[^\\s\\S]b"), TrigramQuery::None);
}

// ─── The string predicates ─────────────────────────────────────────────────

#[test]
fn a_contains_query_needs_three_characters() {
    let query = query_for_contains("needle");
    assert!(requires(&query, "nee"));
    assert!(requires(&query, "dle"));
    // Under three characters no trigram is implied, so it must decline rather
    // than invent one.
    assert!(query_for_contains("ab").is_all());
    assert!(query_for_contains("a").is_all());
    assert!(query_for_contains("").is_all());
}

#[test]
fn an_ends_with_query_works_down_to_one_character() {
    // The end sentinels are what make this indexable at all — a suffix is not
    // a contiguous range in any sort order, so a range index cannot answer it.
    let query = query_for_ends_with("y");
    assert!(
        !query.is_all(),
        "one trailing character is enough: {query:?}"
    );
    let query = query_for_ends_with("xy");
    assert!(!query.is_all());
}

#[test]
fn a_starts_with_query_works_down_to_one_character() {
    let query = query_for_starts_with("a");
    assert!(!query.is_all(), "the start sentinels carry it: {query:?}");
}

#[test]
fn the_string_predicates_fold_case_like_the_index_does() {
    assert_eq!(query_for_contains("NEEDLE"), query_for_contains("needle"));
    assert_eq!(query_for_ends_with("XY"), query_for_ends_with("xy"));
}

// ─── THE LOAD-BEARING TEST ─────────────────────────────────────────────────

/// An in-tree xorshift. Deterministic, seeded, and not a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        let i = self.below(xs.len());
        &xs[i]
    }
}

#[test]
fn a_trigram_query_is_never_narrower_than_the_regex() {
    // THE PROPERTY THE INDEX DEPENDS ON. For every pattern and every string:
    // if the regex matches, the derived query MUST be satisfied by that
    // string's trigrams. A violation here is a row the index would silently
    // fail to return.
    const PATTERNS: &[&str] = &[
        "foo.*bar",
        "abcd",
        "(ab)(cd)",
        "a+b+c+",
        "(cat|dog)",
        "(cat|dogs)x",
        "foo(bar|baz)qux",
        "^abc",
        "abc$",
        "^abcdef$",
        "[a-c]xyz",
        "a{3}bcd",
        "a{2,4}xyz",
        "(abc)+",
        "(abc)*def",
        "x?abcdef",
        "ab.cd",
        "a.*b.*c",
        "hello world",
        "(?i)HeLLo",
        "\\d\\d\\dabc",
        "abc\\d+",
        "[abc][def][ghi]",
        "(foo|a)",
        "prefix.*",
        ".*suffix",
        "(ab|cd)(ef|gh)",
        "a(b(c(d)))",
        "colou?r",
        "\\w+@\\w+",
        "fn\\s+parse_\\w+",
        "->\\s*Result",
        "::[a-z]+::",
    ];
    const ALPHABET: &[char] = &[
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'x', 'y', 'z', '0', '1', ' ', '_', '@', ':', '-', '>',
        'H', 'L', 'é',
    ];
    // Values shaped for the alternation patterns above: each is matched by the
    // OVER-CAP branch, which is the branch a narrowing bug drops.
    const SEEDS: &[&str] = &[
        "v42", "v4", "01/02", "adg", "abc", "prefixv42", "v42suffix", "01:02", "xa42by", "123",
        "beta", "today", "zzzz", "word", "never", "qqq", "none", "zzzend", "123end", "prefixbeta",
        "betasuffix",
    ];

    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut checked = 0u32;
    let mut matched = 0u32;

    for pattern in PATTERNS {
        let re = Regex::compile(pattern).unwrap_or_else(|e| panic!("`{pattern}`: {e}"));
        let query = q(pattern);

        // The strings the pattern is made of, so that genuine matches actually
        // occur rather than the test proving a vacuous truth over noise.
        let mut candidates: Vec<String> = vec![
            String::new(),
            "abcd".into(),
            "foobar".into(),
            "fooXXXbar".into(),
            "cat".into(),
            "dog".into(),
            "hello world".into(),
            "HeLLo".into(),
            "colour".into(),
            "color".into(),
            "abc".into(),
            "abcdef".into(),
            "123abc".into(),
            "fn parse_expr".into(),
            "-> Result".into(),
            "::foo::".into(),
            "a@b".into(),
            "aaabcd".into(),
            "abcxyz".into(),
            "axyz".into(),
            "bxyz".into(),
            "cxyz".into(),
            "abcabc".into(),
            "abcdef".into(),
            "abcdefdef".into(),
            "adg".into(),
            "beh".into(),
            "cfi".into(),
            "abbccc".into(),
            "abcd".into(),
            "abxcd".into(),
            "prefix".into(),
            "prefixyz".into(),
            "suffix".into(),
            "xyzsuffix".into(),
            "abef".into(),
            "cdgh".into(),
            "dogsx".into(),
            "catx".into(),
            "foobarqux".into(),
            "foobazqux".into(),
            "123abc".into(),
            "abc111".into(),
            "aaaxyz".into(),
            "aaaaxyz".into(),
            "fn  parse_a".into(),
            "->Result".into(),
            "::ab::".into(),
            "ab_cd@ef_gh".into(),
        ];
        candidates.extend(SEEDS.iter().map(|s| (*s).to_string()));
        for _ in 0..120 {
            let len = rng.below(9);
            let s: String = (0..len).map(|_| *rng.pick(ALPHABET)).collect();
            candidates.push(s);
        }

        for s in &candidates {
            let is_match = re.is_full_match(s);
            checked += 1;
            if !is_match {
                continue;
            }
            matched += 1;
            let have: BTreeSet<Trigram> = trigrams_of_value(s);
            assert!(
                query.satisfied_by(&have),
                "NARROWING: `{pattern}` matches {s:?}, but the derived query does not \
                 admit it. This is the failure that loses rows silently.\n  query: {query:?}",
            );
        }
    }

    // The test must be able to fail: a run in which nothing ever matched would
    // pass vacuously and prove nothing at all.
    assert!(checked > 2_000, "only {checked} pairs checked");
    assert!(
        matched > 50,
        "only {matched} genuine matches; the corpus is not exercising the property",
    );
}

#[test]
fn the_generative_test_would_notice_a_narrowing() {
    // Proving the test above bites, without breaking the engine to do it: a
    // query that requires a trigram the matching string does not contain is
    // exactly the shape of a narrowing bug, and `satisfied_by` must reject it.
    let have = trigrams_of_value("foobar");
    let bogus = TrigramQuery::Lit(Trigram('z', 'z', 'z'));
    assert!(
        !bogus.satisfied_by(&have),
        "a wrongly-required trigram must be detectable, or the property test is vacuous",
    );
    let real = TrigramQuery::Lit(Trigram('f', 'o', 'o'));
    assert!(real.satisfied_by(&have));
}

#[test]
fn a_value_carrying_the_sentinel_characters_is_still_handled() {
    // The sentinels are real characters that a user may legitimately store.
    // Nothing may panic, and the superset property still has to hold.
    let weird = "a\u{2}b\u{3}c";
    let re = Regex::compile("a.b.c").expect("compiles");
    assert!(re.is_full_match(weird));
    let query = q("a.b.c");
    assert!(
        query.satisfied_by(&trigrams_of_value(weird)),
        "a value containing the sentinels must still satisfy its own query",
    );
}

#[test]
fn an_alternation_with_an_over_cap_branch_requires_nothing_from_the_other() {
    // THE THREE CRITICAL NARROWING BUGS AN AUDIT FOUND, PINNED BY NAME.
    //
    // `v[0-9][0-9]` expands to a ten-member exact product, which is over the
    // cap, so that branch loses its exactness. If the resulting fragment set is
    // written as the EMPTY set rather than as the set containing the EMPTY
    // FRAGMENT, the alternation's union drops the branch entirely and `beta`'s
    // trigrams are left looking mandatory — so a stored "v42" is never offered
    // as a candidate and the row is lost, silently.
    //
    // Every case below is a pattern-and-value pair where the pattern matches
    // and the derived query must therefore admit it.
    let cases: &[(&str, &str)] = &[
        ("(v[0-9][0-9]|beta)", "v42"),
        ("(v[0-9]|beta)", "v4"),
        ("(beta|v[0-9][0-9])", "v42"),
        ("([0-9][0-9]/[0-9][0-9]|today)", "01/02"),
        ("([abc][def][ghi]|zzzz)", "adg"),
        ("prefix(v[0-9][0-9]|beta)", "prefixv42"),
        ("(v[0-9][0-9]|beta)suffix", "v42suffix"),
        ("([0-9][0-9]:[0-9][0-9]|never)", "01:02"),
    ];
    for (pattern, value) in cases {
        let re = Regex::compile(pattern).unwrap_or_else(|e| panic!("`{pattern}`: {e}"));
        assert!(
            re.is_full_match(value),
            "the fixture is wrong: `{pattern}` must match {value:?}",
        );
        let query = q(pattern);
        assert!(
            query.satisfied_by(&trigrams_of_value(value)),
            "NARROWING: `{pattern}` matches {value:?} but the derived query rejects it.\n  \
             query: {query:?}",
        );
    }
}
