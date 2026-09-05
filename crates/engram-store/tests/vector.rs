#![allow(non_snake_case)]
//! The vector index: the X2 design's properties, on constructed data.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{
    PropertyId, Record, Store, StoredValue, VectorError, VectorIndex, encode_f32_vector,
};

const PROP: PropertyId = PropertyId(11);

fn group() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn put_vec(s: &Store, body: &[u8], v: &[f32]) {
    let mut r = Record::new();
    r.set(PROP, encode_f32_vector(v));
    s.put(&group(), body, StoredValue::Plain(r.encode()))
        .expect("row");
}

#[test]
fn search_matches_EXACT_brute_force_on_seeded_vectors() {
    // The X2 claim as a test: int8 scan + f32 rescore at oversample 2 must
    // agree with exact f32 ranking. Seeded, spread-out vectors; every query's
    // top-3 compared against the brute-force answer.
    let s = Store::new();
    let mut rng = 0x12345u64;
    let mut next = move || {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((rng >> 33) as f32 / (u32::MAX >> 1) as f32) - 1.0
    };
    let mut all: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
    for i in 0..40u8 {
        let v: Vec<f32> = (0..16).map(|_| next()).collect();
        put_vec(&s, &[i], &v);
        all.push((vec![i], v));
    }

    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    assert_eq!(idx.len(), 40);

    for qi in 0..5 {
        let q: Vec<f32> = (0..16).map(|_| next()).collect();
        let ans = idx.search(&q, 3).expect("search");

        // Brute force in f32 over unit vectors.
        let qn: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mut brute: Vec<(f32, Vec<u8>)> = all
            .iter()
            .map(|(b, v)| {
                let vn: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let dot: f32 = q.iter().zip(v).map(|(a, b)| a * b).sum();
                (dot / (qn * vn), b.clone())
            })
            .collect();
        brute.sort_by(|a, b| b.0.total_cmp(&a.0));

        // The X2-shaped claim, not a stronger one. X2's recall 1.0 was
        // EARNED on well-conditioned vectors — margin/error ratio 2.2× — and
        // its write-up says the recall is uninterpretable without that ratio.
        // Random 16-d vectors contain near-ties SMALLER than the int8 error,
        // where the two-stage search may legitimately pick the tied twin.
        //
        // The band is MEASURED per query, not guessed: the maximum absolute
        // difference between each entry's rescaled int8 dot and its exact f32
        // dot. A first version hardcoded 0.01 and was wrong by 2× — the same
        // instrument mistake this project keeps cataloguing, in miniature.
        let qn2: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
        let unit_q: Vec<f32> = q.iter().map(|x| x / qn2).collect();
        let q_max = unit_q.iter().fold(0f32, |m, x| m.max(x.abs()));
        let quant_band = all
            .iter()
            .map(|(_, v)| {
                let vn: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let unit_v: Vec<f32> = v.iter().map(|x| x / vn).collect();
                let v_max = unit_v.iter().fold(0f32, |m, x| m.max(x.abs()));
                let qi: Vec<i8> = unit_q
                    .iter()
                    .map(|x| ((x / q_max) * 127.0).round() as i8)
                    .collect();
                let vi: Vec<i8> = unit_v
                    .iter()
                    .map(|x| ((x / v_max) * 127.0).round() as i8)
                    .collect();
                let i8dot: i64 = qi
                    .iter()
                    .zip(&vi)
                    .map(|(a, b)| i64::from(*a) * i64::from(*b))
                    .sum();
                let rescaled = (i8dot as f32) * q_max * v_max / (127.0 * 127.0);
                let exact: f32 = unit_q.iter().zip(&unit_v).map(|(a, b)| a * b).sum();
                (rescaled - exact).abs()
            })
            .fold(0f32, f32::max)
            * 2.0;
        let kth = brute[2].0;
        for (rank, (body, _)) in ans.hits.iter().enumerate() {
            let brute_score = brute.iter().find(|(_, b)| b == body).expect("indexed").0;
            assert!(
                brute_score >= kth - quant_band,
                "query {qi} rank {rank}: {body:?} scores {brute_score}, below the top-3 band ({kth})",
            );
        }
        if brute[2].0 - brute[3].0 > quant_band {
            let got: Vec<&[u8]> = ans.hits.iter().map(|(b, _)| b.as_slice()).collect();
            let want: Vec<&[u8]> = brute[..3].iter().map(|(_, b)| b.as_slice()).collect();
            assert_eq!(
                got, want,
                "query {qi}: decisive margin, but the searches diverged"
            );
        }
    }
}

#[test]
fn the_RESCORE_breaks_quantization_ties_correctly() {
    // Two vectors that quantize IDENTICALLY but differ in f32: the int8 scan
    // cannot separate them, the rescore must. Skipping the rescore decides the
    // winner by scan tie-order — plausible and wrong.
    let s = Store::new();
    // Both quantize to [127, 0, ...]: the second's tiny y-component rounds to 0.
    put_vec(&s, b"exact", &[1.0, 0.0]);
    put_vec(&s, b"near", &[1.0, 0.003]);

    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    // The query leans slightly toward `near`.
    let ans = idx.search(&[1.0, 0.05], 1).expect("search");
    assert_eq!(
        ans.hits[0].0,
        b"near".to_vec(),
        "the f32 rescore must decide, not the int8 tie"
    );
}

#[test]
fn a_dimension_mismatch_is_REFUSED_as_a_different_space() {
    let s = Store::new();
    put_vec(&s, b"a", &[1.0, 0.0, 0.0]);
    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    assert_eq!(
        idx.search(&[1.0, 0.0], 1),
        Err(VectorError::DimensionMismatch { query: 2, index: 3 }),
    );
}

#[test]
fn a_zero_query_is_refused_not_NaN() {
    let s = Store::new();
    put_vec(&s, b"a", &[1.0, 0.0]);
    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    assert_eq!(idx.search(&[0.0, 0.0], 1), Err(VectorError::ZeroQuery));
}

#[test]
fn unindexable_rows_are_counted_on_every_answer() {
    // Zero-norm and wrong-dimension rows: the ranking is over a SUBSET, and
    // the answer says so itself.
    let s = Store::new();
    put_vec(&s, b"good", &[1.0, 0.0]);
    put_vec(&s, b"zero", &[0.0, 0.0]);
    put_vec(&s, b"wrongdim", &[1.0, 0.0, 0.0]);
    // A wrong-TAG row exercises the decode-failure arm — each unindexable
    // arm counts separately, and a canary proved this one was untested.
    let mut r = Record::new();
    r.set(PROP, {
        let mut v = vec![engram_key::value::Tag::STRING.byte()];
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(b"txt");
        v
    });
    s.put(&group(), b"stringrow", StoredValue::Plain(r.encode()))
        .expect("row");

    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    assert_eq!(idx.len(), 1);
    let ans = idx.search(&[1.0, 0.0], 1).expect("search");
    assert_eq!(ans.unindexable, 3, "all three unindexable arms must count");
    assert_eq!(ans.scanned, 1);
}

#[test]
fn the_answer_carries_its_vintage_and_counts() {
    let s = Store::new();
    put_vec(&s, b"a", &[1.0, 0.0]);
    let ts = s.now_ts();
    let idx = VectorIndex::build(&s, &group(), PROP, ts);
    put_vec(&s, b"later", &[0.0, 1.0]);

    let ans = idx.search(&[1.0, 0.0], 5).expect("search");
    assert_eq!(ans.as_of, ts);
    assert_eq!(ans.hits.len(), 1, "the index answers at its snapshot");
    assert_eq!(ans.rescored, 1.min(5 * engram_store::vector::OVERSAMPLE));
}

#[test]
fn scores_are_the_EXACT_f32_cosines() {
    // Returning quantized scores would leak the scan's approximation into
    // downstream fusion, where scores are compared across sources.
    let s = Store::new();
    put_vec(&s, b"a", &[3.0, 4.0]); // unit: [0.6, 0.8]
    let idx = VectorIndex::build(&s, &group(), PROP, s.now_ts());
    let ans = idx.search(&[3.0, 4.0], 1).expect("search");
    assert!(
        (ans.hits[0].1 - 1.0).abs() < 1e-6,
        "self-similarity must be exactly 1.0, got {}",
        ans.hits[0].1
    );
}

#[test]
fn builds_are_deterministic() {
    let s = Store::new();
    for i in 0..10u8 {
        put_vec(&s, &[i], &[f32::from(i), 1.0, -f32::from(i)]);
    }
    let ts = s.now_ts();
    let a = VectorIndex::build(&s, &group(), PROP, ts);
    let b = VectorIndex::build(&s, &group(), PROP, ts);
    let qa = a.search(&[1.0, 0.5, -1.0], 5).expect("a");
    let qb = b.search(&[1.0, 0.5, -1.0], 5).expect("b");
    assert_eq!(qa, qb);
}
