//! A deterministic HNSW — the ANN arm behind `db.index.vector.queryNodes`.
//!
//! Pure Rust, zero dependencies, and DETERMINISTIC: level assignment draws
//! from a SplitMix64 seeded by the element's external id, so two builds over
//! one content produce byte-identical graphs and the DST's guarantee extends
//! through approximate search. No ambient randomness anywhere (D1).
//!
//! Cosine metric via normalize-at-insert: vectors are unit-scaled once, the
//! query once, and every comparison is a dot product. A zero vector cannot
//! be normalized and is the CALLER's refusal (the schema layer already
//! skips-and-counts unindexable rows).

/// Construction/search parameters — fixed, documented, and deliberately
/// conservative: recall over speed at this corpus's scale.
// Chosen by a swept measurement at the ADVERSARIAL regime (n=50k of
// UNIFORM random 128-d vectors — worse-conditioned than any real embedding
// corpus; production Voyage vectors measured crest factor 3.33):
//
//   m=16 m0=32 ef=200 -> 0.750    m=16 ef=400 -> 0.915
//   m=24 m0=48 ef=200 -> 0.885    m=24 ef=400 -> 0.990
//   m=32 m0=64 ef=400 -> 1.000 (double the build for the last 1%)
//
// The first cut (m=16, ef=64, naive nearest-M selection) measured 0.727 at
// n=10k — the high-dimensional regime the unit tests (n<=2.5k, dim<=16)
// cannot reach, exactly as the recorded canary limit predicted.
const M: usize = 24;
const M0: usize = 48;
const EF_CONSTRUCTION: usize = 200;
/// Search beam floor; the effective beam is `max(EF_SEARCH_MIN, 4 * k)`.
const EF_SEARCH_MIN: usize = 400;
/// Level factor `mL = 1 / ln(M)`.
const LEVEL_NORM: f64 = 0.360_673_760_222_240_9;

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The index.
#[derive(Clone)]
pub struct Hnsw {
    dim: usize,
    seed: u64,
    m: usize,
    m0: usize,
    ef_construction: usize,
    entry: Option<usize>,
    max_level: usize,
    /// Per element: its top level.
    levels: Vec<usize>,
    /// Per element, per level (0..=level): neighbour element indices.
    links: Vec<Vec<Vec<usize>>>,
    /// Unit-normalized vectors.
    vecs: Vec<Vec<f32>>,
    /// External ids, parallel to `vecs`.
    ids: Vec<u64>,
}

impl Hnsw {
    /// An empty index over `dim`-dimensional vectors, seeded (the seed folds
    /// into every level draw, so distinct indexes over one id set still get
    /// distinct — but reproducible — level structures).
    pub fn new(dim: usize, seed: u64) -> Hnsw {
        Self::with_params(dim, seed, M, M0, EF_CONSTRUCTION)
    }

    /// Explicit parameters — the bench harness's sweep instrument.
    pub fn with_params(dim: usize, seed: u64, m: usize, m0: usize, ef_construction: usize) -> Hnsw {
        Hnsw {
            dim,
            seed,
            m,
            m0,
            ef_construction,
            entry: None,
            max_level: 0,
            levels: Vec::new(),
            links: Vec::new(),
            vecs: Vec::new(),
            ids: Vec::new(),
        }
    }

    /// Elements held.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The vector dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    fn dot(&self, a: usize, q: &[f32]) -> f32 {
        self.vecs[a].iter().zip(q).map(|(x, y)| x * y).sum()
    }

    /// Insert one element. The caller guarantees `v.len() == dim` and a
    /// nonzero norm (the schema layer's skip rules).
    pub fn insert(&mut self, id: u64, v: &[f32]) {
        debug_assert_eq!(v.len(), self.dim);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        debug_assert!(norm > 0.0);
        let vec: Vec<f32> = v.iter().map(|x| x / norm).collect();

        // The deterministic level draw: u in (0,1] from the id-seeded stream.
        let r = splitmix(self.seed ^ id.wrapping_mul(0xA24B_AED4_963E_E407));
        let u = ((r >> 11) as f64 + 1.0) / ((1u64 << 53) as f64);
        let level = (-u.ln() * LEVEL_NORM).floor() as usize;

        let idx = self.ids.len();
        self.ids.push(id);
        self.vecs.push(vec);
        self.levels.push(level);
        self.links.push(vec![Vec::new(); level + 1]);

        let Some(mut cur) = self.entry else {
            self.entry = Some(idx);
            self.max_level = level;
            return;
        };

        let q = self.vecs[idx].clone();
        // Greedy descent through the levels above the new element's.
        let mut lc = self.max_level;
        while lc > level {
            cur = self.greedy_step(cur, &q, lc);
            lc -= 1;
        }
        // Beam insert on each level from min(level, max_level) down to 0.
        let mut ep = vec![cur];
        let top = level.min(self.max_level);
        for l in (0..=top).rev() {
            let found = self.search_level(&q, &ep, self.ef_construction, l);
            let m = if l == 0 { self.m0 } else { self.m };
            let neighbours = self.select_diverse(&found, m);
            for &n in &neighbours {
                self.links[idx][l].push(n);
                self.links[n][l].push(idx);
                // Prune an overflowing neighbour with the SAME diversity
                // rule — naive keep-nearest clusters the links and starves
                // the beam (the measured 0.727).
                let cap = if l == 0 { self.m0 } else { self.m };
                if self.links[n][l].len() > cap {
                    let nv = self.vecs[n].clone();
                    let mut scored: Vec<(usize, f32)> = self.links[n][l]
                        .iter()
                        .map(|&x| (x, self.dot(x, &nv)))
                        .collect();
                    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                    self.links[n][l] = self.select_diverse(&scored, cap);
                }
            }
            ep = found.into_iter().map(|(i, _)| i).collect();
            if ep.is_empty() {
                ep = vec![cur];
            }
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(idx);
        }
    }

    /// Malkov's neighbour-selection heuristic (Alg. 4, simplified): walk the
    /// candidates best-first and keep one only if it is closer to the query
    /// point than to every neighbour already kept — diversity over pure
    /// proximity, which is what keeps the graph navigable in high dimension.
    /// Remaining slots fill with the nearest unchosen so degree stays full.
    fn select_diverse(&self, candidates: &[(usize, f32)], m: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(m);
        let mut rejected: Vec<usize> = Vec::new();
        for &(c, c_to_q) in candidates {
            if out.len() >= m {
                break;
            }
            let diverse = out.iter().all(|&s| self.dot(c, &self.vecs[s]) < c_to_q);
            if diverse {
                out.push(c);
            } else {
                rejected.push(c);
            }
        }
        for c in rejected {
            if out.len() >= m {
                break;
            }
            out.push(c);
        }
        out
    }

    fn greedy_step(&self, mut cur: usize, q: &[f32], level: usize) -> usize {
        let mut best = self.dot(cur, q);
        loop {
            let mut improved = false;
            for &n in &self.links[cur][level.min(self.levels[cur])] {
                let d = self.dot(n, q);
                if d > best {
                    best = d;
                    cur = n;
                    improved = true;
                }
            }
            if !improved {
                return cur;
            }
        }
    }

    /// Beam search on one level: returns up to `ef` candidates, best first.
    /// Ties break by element index so the result is order-deterministic.
    fn search_level(
        &self,
        q: &[f32],
        entry: &[usize],
        ef: usize,
        level: usize,
    ) -> Vec<(usize, f32)> {
        let mut visited = vec![false; self.ids.len()];
        // `candidates` explored best-first; `best` keeps the ef nearest.
        let mut candidates: Vec<(usize, f32)> = Vec::new();
        let mut best: Vec<(usize, f32)> = Vec::new();
        for &e in entry {
            if visited[e] {
                continue;
            }
            visited[e] = true;
            let d = self.dot(e, q);
            candidates.push((e, d));
            best.push((e, d));
        }
        let by_score = |a: &(usize, f32), b: &(usize, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        candidates.sort_by(by_score);
        best.sort_by(by_score);
        while let Some((c, cd)) = candidates.first().copied() {
            candidates.remove(0);
            let worst = best.last().map(|x| x.1).unwrap_or(f32::MIN);
            if best.len() >= ef && cd < worst {
                break;
            }
            for &n in &self.links[c][level.min(self.levels[c])] {
                if visited[n] {
                    continue;
                }
                visited[n] = true;
                let d = self.dot(n, q);
                let worst = best.last().map(|x| x.1).unwrap_or(f32::MIN);
                if best.len() < ef || d > worst {
                    best.push((n, d));
                    best.sort_by(by_score);
                    best.truncate(ef);
                    let pos = candidates
                        .binary_search_by(|x| by_score(x, &(n, d)))
                        .unwrap_or_else(|p| p);
                    candidates.insert(pos, (n, d));
                }
            }
        }
        best
    }

    /// k-nearest by cosine: `(external id, cosine)`, best first.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f64)> {
        self.search_with_ef(query, k, EF_SEARCH_MIN.max(4 * k))
    }

    /// k-nearest with an explicit beam width.
    pub fn search_with_ef(&self, query: &[f32], k: usize, ef: usize) -> Vec<(u64, f64)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        debug_assert_eq!(query.len(), self.dim);
        let norm = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            return Vec::new();
        }
        let q: Vec<f32> = query.iter().map(|x| x / norm).collect();
        let mut cur = entry;
        for l in (1..=self.max_level).rev() {
            cur = self.greedy_step(cur, &q, l);
        }
        let found = self.search_level(&q, &[cur], ef.max(k), 0);
        found
            .into_iter()
            .take(k)
            .map(|(i, d)| (self.ids[i], f64::from(d)))
            .collect()
    }
}
