#![allow(non_snake_case)]
//! The trigram index: the superset contract, and the incremental one.
//!
//! Two properties carry this structure, and both are asymmetric.
//!
//! **The answer is a SUPERSET.** Every candidate is re-verified by the real
//! predicate upstream, so a wide answer costs time and a narrow one loses rows
//! silently. Every test here that could be written either way is written to
//! catch the narrow direction.
//!
//! **A caught-up index equals a rebuilt one.** An incremental cache that
//! merely *approximates* a rebuild is a second implementation of the index
//! with its own bugs, and the point at which they diverge is the point at
//! which a query starts returning a different answer depending on how the
//! database got into its current state. That equality is asserted rather than
//! argued for.

use std::collections::{BTreeMap, BTreeSet};

use engram_key::value::Tag;
use engram_observe::with_trace;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::trigram::{Trigram, TrigramIndex, TrigramQuery, trigrams_of_value};
use engram_store::{IndexDef, PropertyId, Record, Store, StoredValue};

const PROP: PropertyId = PropertyId(7);

fn group() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn string(s: &str) -> Vec<u8> {
    let mut out = vec![Tag::STRING.byte()];
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

fn int64(v: i64) -> Vec<u8> {
    let mut out = vec![Tag::INT64.byte()];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

fn put_row(s: &Store, body: &[u8], value: Vec<u8>) -> u64 {
    let mut r = Record::new();
    r.set(PROP, value);
    s.put(&group(), body, StoredValue::Plain(r.encode()))
        .expect("row")
}

fn build(s: &Store, bodies: &[&[u8]], ts: u64) -> TrigramIndex {
    TrigramIndex::build_over(
        s,
        &group(),
        IndexDef::new(1, PROP),
        ts,
        bodies.iter().map(|b| b.to_vec()),
    )
}

fn tri(s: &str) -> Trigram {
    let c: Vec<char> = s.chars().collect();
    assert_eq!(c.len(), 3, "a trigram is three characters");
    Trigram(c[0], c[1], c[2])
}

fn lit(s: &str) -> TrigramQuery {
    TrigramQuery::Lit(tri(s))
}

fn ids(answer: Option<engram_store::trigram::TrigramAnswer>) -> Vec<Vec<u8>> {
    answer.expect("the index should have served this").bodies
}

// ─── Extraction ────────────────────────────────────────────────────────────

#[test]
fn a_value_contributes_its_windows_and_its_sentinels() {
    let t = trigrams_of_value("abc");
    // Two leading sentinels, the value's own window, two trailing ones.
    assert!(t.contains(&tri("\u{2}\u{2}a")));
    assert!(t.contains(&tri("\u{2}ab")));
    assert!(t.contains(&tri("abc")));
    assert!(t.contains(&tri("bc\u{3}")));
    assert!(t.contains(&tri("c\u{3}\u{3}")));
    assert_eq!(t.len(), 5);
}

#[test]
fn extraction_folds_case_so_a_query_can_too() {
    assert_eq!(trigrams_of_value("ABC"), trigrams_of_value("abc"));
}

#[test]
fn extraction_keeps_punctuation_because_that_is_the_point() {
    // THE DIFFERENCE FROM THE FULLTEXT ANALYZER. Trigrams run over the raw
    // character sequence, punctuation included, which is exactly what makes
    // code-shaped values searchable. A tokenizer would throw `->` away.
    let t = trigrams_of_value("a->b");
    assert!(t.contains(&tri("a->")));
    assert!(t.contains(&tri("->b")));
}

#[test]
fn a_value_shorter_than_a_trigram_still_indexes() {
    // The sentinels carry it: a one-character value has windows even though
    // the value itself has none.
    let t = trigrams_of_value("a");
    assert!(t.contains(&tri("\u{2}\u{2}a")));
    assert!(t.contains(&tri("\u{2}a\u{3}")));
    assert!(t.contains(&tri("a\u{3}\u{3}")));
    // And the empty value is still a document, not an absence.
    assert!(!trigrams_of_value("").is_empty());
}

#[test]
fn a_multibyte_value_indexes_by_characters_not_bytes() {
    // 'é' is two bytes and one character. A byte-window index would produce
    // windows straddling the encoding, which is where a narrowing bug lives.
    let t = trigrams_of_value("aéb");
    assert!(t.contains(&tri("aéb")));
    assert_eq!(t.len(), 5, "three characters, five windows");
}

// ─── Serving ───────────────────────────────────────────────────────────────

#[test]
fn a_conjunction_intersects_its_postings() {
    let s = Store::new();
    put_row(&s, b"a", string("foo and bar"));
    put_row(&s, b"b", string("foo only"));
    put_row(&s, b"c", string("bar only"));
    let idx = build(&s, &[b"a", b"b", b"c"], s.now_ts());

    let q = TrigramQuery::And(vec![lit("foo"), lit("bar")]);
    assert_eq!(ids(idx.query(&q, None)), vec![b"a".to_vec()]);
}

#[test]
fn a_disjunction_unions_its_postings() {
    let s = Store::new();
    put_row(&s, b"a", string("foo"));
    put_row(&s, b"b", string("bar"));
    put_row(&s, b"c", string("qux"));
    let idx = build(&s, &[b"a", b"b", b"c"], s.now_ts());

    let q = TrigramQuery::Or(vec![lit("foo"), lit("bar")]);
    assert_eq!(ids(idx.query(&q, None)), vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn a_count_prices_a_probe_without_materialising_it() {
    let s = Store::new();
    for i in 0..10u8 {
        put_row(&s, &[b'a' + i], string("shared prefix here"));
    }
    put_row(&s, b"z", string("unique zzz value"));
    let bodies: Vec<Vec<u8>> = (0..10u8).map(|i| vec![b'a' + i]).collect();
    let mut all: Vec<&[u8]> = bodies.iter().map(std::vec::Vec::as_slice).collect();
    all.push(b"z");
    let idx = build(&s, &all, s.now_ts());

    assert_eq!(idx.count(tri("sha")), 10);
    assert_eq!(idx.count(tri("zzz")), 1);
    // An AND is bounded by its smallest branch — which is what lets the
    // planner compare this index against a label scan before building either.
    let q = TrigramQuery::And(vec![lit("sha"), lit("zzz")]);
    assert_eq!(idx.estimate(&q), 1);
}

// ─── The declines, each of which must be a decline and not a wrong answer ──

#[test]
fn a_match_all_condition_declines_rather_than_answering_everything() {
    // Returning every document would be *correct* and useless: the planner
    // must be told the index cannot help, so that it compares a label scan
    // against something honest.
    let s = Store::new();
    put_row(&s, b"a", string("anything"));
    let idx = build(&s, &[b"a"], s.now_ts());
    assert!(idx.query(&TrigramQuery::All, None).is_none());
}

#[test]
fn a_probe_over_the_cap_declines() {
    let s = Store::new();
    for i in 0..10u8 {
        put_row(&s, &[b'a' + i], string("common text"));
    }
    let bodies: Vec<Vec<u8>> = (0..10u8).map(|i| vec![b'a' + i]).collect();
    let all: Vec<&[u8]> = bodies.iter().map(std::vec::Vec::as_slice).collect();
    let idx = build(&s, &all, s.now_ts());

    assert!(idx.query(&lit("com"), Some(3)).is_none(), "over the cap");
    assert!(idx.query(&lit("com"), Some(100)).is_some(), "under the cap");
}

#[test]
fn a_non_string_row_disables_the_index_entirely() {
    // THE DIVERGENCE FROM THE RANGE INDEX, and the reason for it. There, an
    // unindexable row makes the answer an honest FLOOR and the caller is told.
    // Here the answer is a CANDIDATE SET, and a candidate set that is a floor
    // is just a wrong answer — the row it omitted might have matched.
    let s = Store::new();
    put_row(&s, b"a", string("foo"));
    put_row(&s, b"b", int64(42));
    let idx = build(&s, &[b"a", b"b"], s.now_ts());

    assert_eq!(idx.unindexable(), 1);
    assert!(
        idx.query(&lit("foo"), None).is_none(),
        "one unindexable row must disable the index, not shrink its answer",
    );
}

#[test]
fn an_unconstrained_branch_of_a_disjunction_declines_the_whole_union() {
    // `(foo|.*)` can match anything. Dropping the unconstrained branch and
    // answering with `foo`'s postings would be a NARROWING — the direction
    // that loses rows.
    let s = Store::new();
    put_row(&s, b"a", string("foo"));
    put_row(&s, b"b", string("something else"));
    let idx = build(&s, &[b"a", b"b"], s.now_ts());

    let q = TrigramQuery::Or(vec![lit("foo"), TrigramQuery::All]);
    assert!(
        idx.query(&q, None).is_none(),
        "one unconstrained branch makes the whole union unconstrained",
    );
}

#[test]
fn an_unconstrained_branch_of_a_conjunction_is_simply_dropped() {
    // The mirror image, and it goes the other way: `foo.*` still requires
    // `foo`. Declining here would only cost a scan, but answering is correct
    // and the asymmetry is worth pinning so nobody "simplifies" it.
    let s = Store::new();
    put_row(&s, b"a", string("foo"));
    put_row(&s, b"b", string("something else"));
    let idx = build(&s, &[b"a", b"b"], s.now_ts());

    let q = TrigramQuery::And(vec![lit("foo"), TrigramQuery::All]);
    assert_eq!(ids(idx.query(&q, None)), vec![b"a".to_vec()]);
}

// ─── The incremental contract ──────────────────────────────────────────────

fn changes_of(pairs: &[(&[u8], Option<&str>)]) -> BTreeMap<Vec<u8>, Option<BTreeSet<Trigram>>> {
    pairs
        .iter()
        .map(|(b, text)| (b.to_vec(), text.map(trigrams_of_value)))
        .collect()
}

/// Every trigram the index can currently produce, as a comparable summary.
fn shape(idx: &TrigramIndex, probes: &[&str]) -> Vec<(String, Vec<Vec<u8>>)> {
    probes
        .iter()
        .map(|p| {
            let answer = idx
                .query(&lit(p), None)
                .map(|a| a.bodies)
                .unwrap_or_default();
            ((*p).to_string(), answer)
        })
        .collect()
}

#[test]
fn a_caught_up_trigram_index_equals_a_fresh_build() {
    // THE CONTRACT. Add, change and delete, then compare against a rebuild at
    // the same timestamp over the same rows.
    let s = Store::new();
    put_row(&s, b"a", string("alpha text"));
    put_row(&s, b"b", string("beta text"));
    put_row(&s, b"c", string("gamma text"));
    let t0 = s.now_ts();
    let idx = build(&s, &[b"a", b"b", b"c"], t0);

    // `b` is rewritten, `c` is deleted, `d` appears.
    put_row(&s, b"b", string("delta text"));
    put_row(&s, b"d", string("epsilon text"));
    let t1 = s.now_ts();

    let caught_up = idx
        .with_changes(
            &changes_of(&[
                (b"b", Some("delta text")),
                (b"c", None),
                (b"d", Some("epsilon text")),
            ]),
            t1,
        )
        .expect("the index should carry forward");

    let rebuilt = build(&s, &[b"a", b"b", b"d"], t1);

    let probes = [
        "alp",
        "bet",
        "gam",
        "del",
        "eps",
        "tex",
        "\u{2}\u{2}a",
        "\u{2}\u{2}b",
        "\u{2}\u{2}d",
    ];
    assert_eq!(
        shape(&caught_up, &probes),
        shape(&rebuilt, &probes),
        "a caught-up index must answer exactly as a rebuilt one",
    );
    assert_eq!(caught_up.as_of(), rebuilt.as_of());
    assert_eq!(caught_up.doc_count(), rebuilt.doc_count());
}

#[test]
fn a_reindexed_row_loses_its_old_trigrams_and_gains_its_new_ones() {
    // The subtle one: a body being re-indexed is removed AND re-added, and a
    // removal that outlived its re-addition would subtract the new entries
    // away again — leaving the row findable by nothing at all.
    let s = Store::new();
    put_row(&s, b"a", string("before"));
    let idx = build(&s, &[b"a"], s.now_ts());
    assert_eq!(ids(idx.query(&lit("bef"), None)), vec![b"a".to_vec()]);

    let next = idx
        .with_changes(&changes_of(&[(b"a", Some("after"))]), s.now_ts())
        .expect("carries forward");

    assert_eq!(
        ids(next.query(&lit("aft"), None)),
        vec![b"a".to_vec()],
        "the row must be findable by its NEW content",
    );
    assert!(
        ids(next.query(&lit("bef"), None)).is_empty(),
        "and must NOT be findable by its old content",
    );
}

#[test]
fn a_deleted_row_leaves_the_index() {
    let s = Store::new();
    put_row(&s, b"a", string("gone soon"));
    put_row(&s, b"b", string("gone soon"));
    let idx = build(&s, &[b"a", b"b"], s.now_ts());
    assert_eq!(idx.count(tri("gon")), 2);

    let next = idx
        .with_changes(&changes_of(&[(b"a", None)]), s.now_ts())
        .expect("carries forward");
    assert_eq!(ids(next.query(&lit("gon"), None)), vec![b"b".to_vec()]);
    assert_eq!(next.doc_count(), 1);
}

#[test]
fn a_catch_up_over_the_fold_threshold_declines_and_says_so_by_returning_none() {
    // Beyond the bound the overlay is no longer small, and a linear scan of it
    // on every read is worse than a rebuild. Declining is the honest answer.
    let s = Store::new();
    put_row(&s, b"a", string("text"));
    let idx = build(&s, &[b"a"], s.now_ts());

    let mut huge: BTreeMap<Vec<u8>, Option<BTreeSet<Trigram>>> = BTreeMap::new();
    for i in 0..5_000u32 {
        huge.insert(i.to_be_bytes().to_vec(), Some(trigrams_of_value("filler")));
    }
    assert!(
        idx.with_changes(&huge, s.now_ts()).is_none(),
        "a catch-up past the fold threshold must decline to a rebuild",
    );
}

#[test]
fn an_index_that_is_already_refusing_declines_to_carry_forward() {
    // It would only carry the refusal, and a caller that got `Some` back would
    // reasonably believe it had a usable index.
    let s = Store::new();
    put_row(&s, b"a", string("foo"));
    put_row(&s, b"b", int64(1));
    let idx = build(&s, &[b"a", b"b"], s.now_ts());
    assert_eq!(idx.unindexable(), 1);
    assert!(
        idx.with_changes(&changes_of(&[(b"a", Some("bar"))]), s.now_ts())
            .is_none()
    );
}

#[test]
fn a_rebuild_at_the_same_timestamp_is_identical() {
    // "Drop and rebuild" is the whole repair story for a derived structure, so
    // it has to be a repair rather than a gamble.
    let s = Store::new();
    put_row(&s, b"a", string("one"));
    put_row(&s, b"b", string("two"));
    let ts = s.now_ts();
    let probes = ["one", "two", "\u{2}\u{2}o"];
    assert_eq!(
        shape(&build(&s, &[b"a", b"b"], ts), &probes),
        shape(&build(&s, &[b"a", b"b"], ts), &probes),
    );
}

#[test]
fn a_catch_up_deep_copies_its_overlay_so_the_overlay_must_stay_small() {
    // WHY THIS TEST EXISTS, since the property it pins looks like an
    // implementation detail: `with_changes` begins `self.clone()`, so the
    // overlay is copied in full on EVERY stale read, each entry carrying its
    // own body allocation. That is what makes `FOLD_AT` a small constant
    // rather than a fraction of the base.
    //
    // The fraction was tried. It is the textbook amortisation — O(base) work
    // should buy O(base) insertions — and on a 500-pair benchmark it looked
    // like a 63x win on the maximum. Over 5,000 pairs it measured a 5.5x
    // MEDIAN regression and 2.6x the wall time, because the per-read copy it
    // enlarges is paid far more often than the fold it spares; and the maxima
    // that made it look good were runs in which no fold happened at all.
    //
    // So: assert the overlay stays bounded by the constant across a long run
    // of catch-ups. If someone raises the threshold, this fails and points at
    // the reason rather than at a benchmark nobody re-runs.
    let s = Store::new();
    let bodies: Vec<Vec<u8>> = (0..2_000u32).map(|i| format!("b{i}").into_bytes()).collect();
    for (i, b) in bodies.iter().enumerate() {
        put_row(&s, b, string(&format!("the base body number {i} in file{i}.rs")));
    }
    let refs: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect();
    let mut cur = build(&s, &refs, s.now_ts());
    let base = cur.entry_count();

    // Each chunk is ~100 bodies of ~30 trigrams: about 3,000 overlay pairs.
    let chunk = |n: u32| -> BTreeMap<Vec<u8>, Option<BTreeSet<Trigram>>> {
        (0..100u32)
            .map(|i| {
                (
                    format!("new{n}_{i}").into_bytes(),
                    Some(trigrams_of_value(&format!(
                        "an added body {n} number {i} in file{i}.rs"
                    ))),
                )
            })
            .collect()
    };

    let mut folds = 0usize;
    for n in 0..40u32 {
        let (next, trace) = with_trace(|| cur.with_changes(&chunk(n), s.now_ts()));
        cur = next.expect("carries forward");
        folds += trace
            .counters()
            .get("trigram.overlay folded")
            .copied()
            .unwrap_or(0) as usize;
        assert!(
            cur.overlay_len() <= 4_096 + 3_500,
            "the overlay reached {} entries after {} catch-ups; it is DEEP-COPIED on every              stale read, so it must stay near the fold threshold and not grow with the base              ({base} entries)",
            cur.overlay_len(),
            n + 1,
        );
    }
    assert!(
        folds > 0,
        "forty catch-ups of ~3,000 pairs each must have folded at least once, or this test          is asserting a bound nothing was pushing against",
    );
}
