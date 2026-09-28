#![allow(non_snake_case)]
//! The BM25 term index: the scoring it fixes, and the incremental contract.
//!
//! The scan this replaces summed raw term frequencies. That has two consequences
//! nobody would choose: a term appearing in every document counts as much as one
//! appearing in three, and a long document outranks a short one for being long.
//! The first pair of tests below pins both, by showing the ranking the old
//! scorer produces and the ranking this one produces over the same corpus.
//!
//! The rest is the same equality contract the trigram index carries: a
//! caught-up index must be INDISTINGUISHABLE from a rebuilt one. An incremental
//! path that merely approximates a rebuild is a second implementation of the
//! index, and the point at which the two diverge is the point at which a query
//! starts answering differently depending on how the database got here.

use std::collections::BTreeMap;

use engram_key::value::Tag;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::term::{DocChange, FieldContent, Hit, TermIndex, idf};
use engram_store::{PropertyId, Record, Store, StoredValue};

const TITLE: PropertyId = PropertyId(7);
const BODY: PropertyId = PropertyId(8);

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

fn put(s: &Store, body: &[u8], title: &str, body_text: &str) {
    let mut r = Record::new();
    r.set(TITLE, string(title));
    r.set(BODY, string(body_text));
    s.put(&group(), body, StoredValue::Plain(r.encode()))
        .expect("row");
}

fn build(s: &Store, bodies: &[&[u8]], ts: u64) -> TermIndex {
    TermIndex::build_over(
        s,
        &group(),
        vec![TITLE, BODY],
        ts,
        bodies.iter().map(|b| b.to_vec()),
    )
}

fn order(hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| String::from_utf8_lossy(&h.body).into_owned())
        .collect()
}

// ─── What BM25 fixes ───────────────────────────────────────────────────────

#[test]
fn a_rare_term_outranks_a_common_one() {
    // THE HEADLINE. Under raw term frequency these two documents score
    // identically — each contains one query term once. Under BM25 the document
    // carrying the RARE term wins, because the common term tells you almost
    // nothing about which document you wanted.
    let s = Store::new();
    put(&s, b"rare", "quasar", "");
    put(&s, b"common", "database", "");
    for i in 0..20u8 {
        put(&s, &[b'a' + i], "database", "");
    }
    let mut all: Vec<Vec<u8>> = (0..20u8).map(|i| vec![b'a' + i]).collect();
    all.push(b"rare".to_vec());
    all.push(b"common".to_vec());
    let refs: Vec<&[u8]> = all.iter().map(std::vec::Vec::as_slice).collect();
    let idx = build(&s, &refs, s.now_ts());

    let hits = idx.query("quasar database");
    assert_eq!(
        order(&hits).first().map(String::as_str),
        Some("rare"),
        "the rare term must win: {:?}",
        order(&hits),
    );
}

#[test]
fn a_short_document_outranks_a_long_one_carrying_the_term_as_often() {
    // Length normalisation. Both documents contain "graph" once; the shorter
    // one is more ABOUT it. Raw term frequency cannot tell them apart at all.
    let s = Store::new();
    put(&s, b"short", "graph", "");
    put(
        &s,
        b"long",
        "graph and a great many other unrelated words padding this title out",
        "",
    );
    let idx = build(&s, &[b"short", b"long"], s.now_ts());
    let hits = idx.query("graph");
    assert_eq!(order(&hits), vec!["short".to_string(), "long".into()]);
}

#[test]
fn a_term_in_every_document_ranks_nothing_above_anything() {
    // idf approaches zero, so the term stops discriminating rather than going
    // NEGATIVE and penalising the documents that carry it — which is what the
    // classic idf form does past half the corpus.
    let s = Store::new();
    for i in 0..8u8 {
        put(&s, &[b'a' + i], "the same words everywhere", "");
    }
    let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i]).collect();
    let refs: Vec<&[u8]> = bodies.iter().map(std::vec::Vec::as_slice).collect();
    let idx = build(&s, &refs, s.now_ts());
    for h in idx.query("same") {
        assert!(h.score >= 0.0, "a score must never be negative: {h:?}");
    }
}

#[test]
fn the_idf_is_never_negative_however_common_the_term() {
    // The property the Lucene form has and the classic one does not. A negative
    // contribution would let a document rank BELOW one matching less of the
    // query, purely for containing a query term.
    for n in [1u64, 2, 10, 1000] {
        for df in 0..=n {
            let v = idf(n, df);
            assert!(
                v >= 0.0 && v.is_finite(),
                "idf(N={n}, df={df}) = {v} must be finite and non-negative",
            );
        }
    }
    // And still strictly decreasing in df, so it discriminates.
    assert!(idf(1000, 1) > idf(1000, 10));
    assert!(idf(1000, 10) > idf(1000, 500));
    assert!(idf(1000, 500) > idf(1000, 999));
}

#[test]
fn a_repeated_query_token_counts_twice() {
    // As both the term-frequency path this replaces and a Lucene boolean query
    // do. Deduplicating the query would silently change what a user asked.
    let s = Store::new();
    put(&s, b"a", "rust", "");
    put(&s, b"b", "other", "");
    let idx = build(&s, &[b"a", b"b"], s.now_ts());
    let one = idx.query("rust");
    let two = idx.query("rust rust");
    assert!(
        two[0].score > one[0].score,
        "a repeated token must weigh more: {} vs {}",
        two[0].score,
        one[0].score,
    );
}

// ─── The declines and the definitions ──────────────────────────────────────

#[test]
fn a_non_string_field_contributes_nothing_and_disables_nothing() {
    // THE DIVERGENCE FROM THE TRIGRAM INDEX, and its reason. That index is a
    // candidate generator whose omissions are unrecoverable, so any unindexable
    // value disables it. This one is an ANSWER producer that must agree with
    // the fallback scan — and the scan ignores a non-string property. Agreeing
    // with the definition is not a floor.
    let s = Store::new();
    put(&s, b"a", "findable", "");
    let mut r = Record::new();
    r.set(TITLE, int64(42));
    r.set(BODY, string("findable"));
    s.put(&group(), b"b", StoredValue::Plain(r.encode()))
        .expect("row");

    let idx = build(&s, &[b"a", b"b"], s.now_ts());
    assert_eq!(idx.non_string_fields(), 1);
    let hits = idx.query("findable");
    assert_eq!(hits.len(), 2, "both documents still answer: {:?}", order(&hits));
}

#[test]
fn a_query_with_no_analysable_token_answers_nothing() {
    let s = Store::new();
    put(&s, b"a", "content", "");
    let idx = build(&s, &[b"a"], s.now_ts());
    assert!(idx.query("").is_empty());
    assert!(idx.query("   ---   ").is_empty());
}

#[test]
fn ties_break_by_body_rather_than_by_iteration_order() {
    // The scan this replaces broke ties by label-scan order — an accident of
    // iteration that nothing declared. The order here is total and stated.
    let s = Store::new();
    put(&s, b"b", "identical text", "");
    put(&s, b"a", "identical text", "");
    put(&s, b"c", "identical text", "");
    let idx = build(&s, &[b"a", b"b", b"c"], s.now_ts());
    assert_eq!(
        order(&idx.query("identical")),
        vec!["a".to_string(), "b".into(), "c".into()],
    );
}

// ─── The incremental contract ──────────────────────────────────────────────

fn content(text: &str) -> Option<FieldContent> {
    Some(FieldContent::of(text))
}

/// Compare two indexes by everything a query can observe.
fn same_answers(a: &TermIndex, b: &TermIndex, queries: &[&str]) {
    assert_eq!(a.doc_count(), b.doc_count(), "document counts differ");
    assert_eq!(a.corpus(), b.corpus(), "corpus totals differ");
    for q in queries {
        let (x, y) = (a.query(q), b.query(q));
        assert_eq!(order(&x), order(&y), "`{q}` ordered differently");
        for (h1, h2) in x.iter().zip(y.iter()) {
            assert_eq!(
                h1.score.to_bits(),
                h2.score.to_bits(),
                "`{q}` scored {} differently: {} vs {}",
                String::from_utf8_lossy(&h1.body),
                h1.score,
                h2.score,
            );
        }
    }
}

const QUERIES: &[&str] = &["alpha", "beta", "gamma", "delta", "text", "alpha text"];

#[test]
fn a_caught_up_term_index_equals_a_fresh_build() {
    // THE CONTRACT, and it is compared BIT FOR BIT rather than with an epsilon:
    // an epsilon large enough to absorb a reordered summation is large enough
    // to stop catching a real scoring change.
    let s = Store::new();
    put(&s, b"a", "alpha text", "body one");
    put(&s, b"b", "beta text", "body two");
    put(&s, b"c", "gamma text", "body three");
    let t0 = s.now_ts();
    let idx = build(&s, &[b"a", b"b", b"c"], t0);

    // `b` is rewritten, `c` leaves, `d` joins.
    put(&s, b"b", "delta text", "body two");
    put(&s, b"d", "alpha text", "body four");
    let t1 = s.now_ts();

    let mut changes: BTreeMap<Vec<u8>, DocChange> = BTreeMap::new();
    changes.insert(
        b"b".to_vec(),
        DocChange {
            fields: vec![(0, content("delta text"))],
            ..DocChange::default()
        },
    );
    changes.insert(
        b"c".to_vec(),
        DocChange {
            gone: true,
            ..DocChange::default()
        },
    );
    changes.insert(
        b"d".to_vec(),
        DocChange {
            fields: vec![(0, content("alpha text")), (1, content("body four"))],
            whole: true,
            ..DocChange::default()
        },
    );

    let caught = idx.with_changes(&changes, t1).expect("carries forward");
    let rebuilt = build(&s, &[b"a", b"b", b"d"], t1);
    same_answers(&caught, &rebuilt, QUERIES);
    assert_eq!(caught.as_of(), rebuilt.as_of());
}

#[test]
fn a_reindexed_document_loses_its_old_terms() {
    // The subtle one. A document being re-indexed is removed AND re-added, and
    // a removal that outlived its re-addition would subtract the new entries
    // away too, leaving the document findable by nothing at all.
    let s = Store::new();
    put(&s, b"a", "before", "");
    let idx = build(&s, &[b"a"], s.now_ts());
    assert_eq!(idx.query("before").len(), 1);

    let mut changes = BTreeMap::new();
    changes.insert(
        b"a".to_vec(),
        DocChange {
            fields: vec![(0, content("after"))],
            ..DocChange::default()
        },
    );
    let next = idx.with_changes(&changes, s.now_ts()).expect("carries");
    assert_eq!(next.query("after").len(), 1, "findable by its NEW text");
    assert!(next.query("before").is_empty(), "not by its old text");
}

#[test]
fn a_joining_document_does_not_inherit_a_field_it_never_had() {
    // `whole` is what distinguishes "this document changed one field" from
    // "this document is new and this is all of it". Without it, a joining
    // document with an empty title keeps whatever the index last held there.
    let s = Store::new();
    put(&s, b"a", "kept", "");
    let idx = build(&s, &[b"a"], s.now_ts());

    let mut gone = BTreeMap::new();
    gone.insert(
        b"a".to_vec(),
        DocChange {
            gone: true,
            ..DocChange::default()
        },
    );
    let empty = idx.with_changes(&gone, s.now_ts()).expect("carries");
    assert!(empty.query("kept").is_empty());

    let mut rejoin = BTreeMap::new();
    rejoin.insert(
        b"a".to_vec(),
        DocChange {
            fields: vec![(1, content("only a body now"))],
            whole: true,
            ..DocChange::default()
        },
    );
    let back = empty.with_changes(&rejoin, s.now_ts()).expect("carries");
    assert!(
        back.query("kept").is_empty(),
        "the rejoining document must not inherit its old title",
    );
    assert_eq!(back.query("body").len(), 1);
}

#[test]
fn the_corpus_totals_survive_a_document_leaving_and_returning() {
    let s = Store::new();
    put(&s, b"a", "one two three", "");
    put(&s, b"b", "four five", "");
    let idx = build(&s, &[b"a", b"b"], s.now_ts());
    let before = idx.corpus().clone();

    let mut gone = BTreeMap::new();
    gone.insert(
        b"a".to_vec(),
        DocChange {
            gone: true,
            ..DocChange::default()
        },
    );
    let without = idx.with_changes(&gone, s.now_ts()).expect("carries");
    assert_eq!(without.corpus().docs, 1);

    let mut back = BTreeMap::new();
    back.insert(
        b"a".to_vec(),
        DocChange {
            fields: vec![(0, content("one two three"))],
            whole: true,
            ..DocChange::default()
        },
    );
    let restored = without.with_changes(&back, s.now_ts()).expect("carries");
    assert_eq!(
        restored.corpus(),
        &before,
        "the totals must return to exactly where they were",
    );
}

#[test]
fn a_catch_up_over_the_fold_threshold_declines_to_a_rebuild() {
    let s = Store::new();
    put(&s, b"a", "text", "");
    let idx = build(&s, &[b"a"], s.now_ts());
    let mut huge: BTreeMap<Vec<u8>, DocChange> = BTreeMap::new();
    for i in 0..5_000u32 {
        huge.insert(
            i.to_be_bytes().to_vec(),
            DocChange {
                fields: vec![(0, content("filler"))],
                whole: true,
                ..DocChange::default()
            },
        );
    }
    assert!(idx.with_changes(&huge, s.now_ts()).is_none());
}

#[test]
fn a_field_outside_the_declaration_declines_to_a_rebuild() {
    // A field id past the declared list is a DEFINITION change, not a data
    // change. Guessing at the new shape is exactly what the house forbids.
    let s = Store::new();
    put(&s, b"a", "text", "");
    let idx = build(&s, &[b"a"], s.now_ts());
    let mut changes = BTreeMap::new();
    changes.insert(
        b"a".to_vec(),
        DocChange {
            fields: vec![(9, content("a field that does not exist"))],
            ..DocChange::default()
        },
    );
    assert!(idx.with_changes(&changes, s.now_ts()).is_none());
}

#[test]
fn a_rebuild_at_the_same_timestamp_is_identical() {
    let s = Store::new();
    put(&s, b"a", "alpha", "one");
    put(&s, b"b", "beta", "two");
    let ts = s.now_ts();
    same_answers(&build(&s, &[b"a", b"b"], ts), &build(&s, &[b"a", b"b"], ts), QUERIES);
}
