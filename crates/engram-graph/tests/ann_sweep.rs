#![allow(non_snake_case)]
//! A manual parameter sweep — `cargo test --release --test ann_sweep -- --ignored --nocapture`.

use engram_graph::hnsw::Hnsw;

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn vecf(seed: u64, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|d| {
            let r = splitmix(seed.wrapping_mul(131).wrapping_add(d as u64));
            (((r >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0) as f32
        })
        .collect()
}

#[test]
#[ignore = "manual instrument: sweeps (m, ef) at n=50k, dim=128"]
fn sweep() {
    const N: usize = 50_000;
    const DIM: usize = 128;
    let data: Vec<Vec<f32>> = (0..N as u64).map(|i| vecf(i, DIM)).collect();
    let queries: Vec<Vec<f32>> = (0..20u64).map(|q| vecf(1_000_000 + q, DIM)).collect();
    // Exact ground truth.
    let exact: Vec<Vec<usize>> = queries
        .iter()
        .map(|q| {
            let qn: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
            let mut scored: Vec<(usize, f32)> = data
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let d: f32 = v.iter().zip(q).map(|(a, b)| a * b).sum();
                    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    (i, d / (n * qn))
                })
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            scored.truncate(10);
            scored.into_iter().map(|(i, _)| i).collect()
        })
        .collect();
    for (m, m0, efc) in [(16, 32, 200), (24, 48, 200), (32, 64, 300)] {
        let mut h = Hnsw::with_params(DIM, 42, m, m0, efc);
        for (i, v) in data.iter().enumerate() {
            h.insert(i as u64, v);
        }
        for ef in [200usize, 400, 800] {
            let mut hit = 0usize;
            for (q, want) in queries.iter().zip(&exact) {
                let got = h.search_with_ef(q, 10, ef);
                hit += want
                    .iter()
                    .filter(|w| got.iter().any(|(id, _)| *id == **w as u64))
                    .count();
            }
            eprintln!(
                "m={m} m0={m0} efc={efc} ef={ef}: recall@10 = {:.3}",
                hit as f64 / 200.0
            );
        }
    }
}
