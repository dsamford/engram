//! `lsqbref <corpus dir> [--json <out>] [--expect <counts.json>]` — an
//! INDEPENDENT oracle for the nine LSQB counts.
//!
//! # Why an oracle, and why it uses no graph engine
//!
//! `lsqb` (the lane) runs the nine LSQB counting queries through the engine
//! and validates them with existence probes and an optional `--expect` map.
//! Neither validation can tell a WRONG count from a right one: a probe proves
//! the pattern exists, not that the engine counted every embedding exactly
//! once; and `--expect` from another engine only moves the trust. A count
//! query needs a second, independent derivation of the same number, computed
//! by a different method from a different representation. This binary is that
//! derivation: it reads the corpus JSONL (the same `nodes.jsonl` /
//! `rels.jsonl` contract `snbload` and `load_export` read) into compact typed
//! adjacency arrays and computes each count by hand — degree products and
//! Yannakakis-style semi-join sums for the acyclic shapes, sorted-vector
//! intersections for the cyclic ones, membership tests for the anti-joins. No
//! Cypher, no planner, no `Graph`. Where this and the engine agree, both are
//! very probably right; where they differ, the difference is a finding.
//!
//! # The nine queries, and what each one's Cypher actually means
//!
//! The query texts are the lane's table (`lsqb.rs`, the official
//! `ldbc/lsqb` cypher, line-joined); they are repeated in [`QUERIES`] here
//! and a unit test pins them to that file byte for byte, so this oracle can
//! never quietly implement a different query from the one measured.
//!
//! Semantics honoured, in the order they bite:
//!
//! - **`count(*)` counts ROWS**, so a `MATCH` with `n` embeddings of the
//!   pattern contributes `n`, and an `OPTIONAL MATCH` that finds nothing
//!   contributes ONE null-filled row (q7's `max(1, legs)`).
//! - **Undirected `-[:KNOWS]-` binds each relationship in BOTH
//!   orientations**: one KNOWS rel `a->b` gives a row with `person1=a,
//!   person2=b` and another with `person1=b, person2=a`. The KNOWS adjacency
//!   is therefore built with every rel entered under both endpoints, and
//!   `m(u,v)` — the number of KNOWS rels between `u` and `v` in either
//!   direction — is the multiplicity every KNOWS hop carries (the corpora hold
//!   parallel and reciprocal KNOWS rels; `snbgen` emits both).
//! - **Relationship isomorphism** (a single `MATCH` pattern may not bind the
//!   same relationship twice) is enforced per MATCH clause, NOT across
//!   clauses. Each query's implementation states where it could bite and why
//!   it does not here:
//!   - q1, q4: every hop has a different type — automatic.
//!   - q2: the two `HAS_CREATOR` hops start at `comment:Comment` and
//!     `post:Post`, which are different nodes (no `REPLY_OF` self-loops, and
//!     the labels are disjoint in the SNB schema) — automatic.
//!   - q3: the KNOWS triangle is ONE clause, so its three rels must be
//!     distinct — and they are, because the three persons are pairwise
//!     distinct (a repeated person would need a KNOWS self-loop, which this
//!     oracle REFUSES at load — see below), so the three rels join three
//!     different pairs. The three `IS_LOCATED_IN`/`IS_PART_OF` clauses are
//!     separate `MATCH`es, so no isomorphism holds between them: person1 and
//!     person2 sharing a city is fine.
//!   - q5, q8: the two `HAS_TAG` hops start at `message` and `comment`,
//!     different nodes (no `REPLY_OF` self-loops) — automatic.
//!   - q6, q9: `person1-KNOWS-person2-KNOWS-person3` is one clause; the two
//!     rels could coincide ONLY when `person1 = person3` (both rels join the
//!     same pair), and `WHERE person1 <> person3` removes every such row —
//!     including the parallel-edge rows isomorphism alone would have kept.
//!     Once `person1 <> person3`, the rels join different pairs — automatic.
//! - **KNOWS self-loops are refused, not modelled.** An undirected self-loop
//!   is the one case where "both orientations" and "distinct relationships"
//!   interact (Neo4j binds an undirected loop once, and a q3 triangle through
//!   a loop needs two distinct parallel rels on one pair). Neither corpus
//!   family this oracle exists for has one — LDBC Datagen does not emit them
//!   and `snbgen` skips `j == i` — so rather than encode a rule nothing here
//!   can check, a corpus with one fails loudly at load.
//! - **Every label test in the query is applied** — `(:Country)` admits only
//!   nodes carrying `Country`, `(message:Message)` admits both Posts and
//!   Comments (the corpora carry `Message` as a supertype label on both),
//!   `(:Post)` admits only Posts. A node matches every label it carries.
//!
//! # Gating the oracle itself
//!
//! An oracle is only as good as its own evidence. Two gates:
//!
//! 1. **Unit tests on a ten-node fixture** whose nine counts are derived by
//!    hand in the test (`tests` below), including parallel KNOWS rels, a
//!    reply-to-a-comment (counted by q5/q8, not by q1/q2), a dual-labelled
//!    node, and an open wedge (a non-zero q9 and the `q9 = q6 − closed`
//!    identity).
//! 2. **Agreement with the engine** on corpora small enough for the engine's
//!    general path to finish: `tests/lsqbref_agreement.rs` generates `snbgen`
//!    corpora (seed 1), runs this binary and the engine in-process, and
//!    requires all nine counts equal — on the general path AND the columnar
//!    path. A 40-person corpus runs on every `cargo test`; the 300- and
//!    1000-person pair runs under `ENGRAM_LSQBREF_AGREEMENT=full` (minutes,
//!    release build) and agreed on all nine on both, 2026-08-29.
//!
//! What this cannot do locally: reproduce the engine's official-SF1 numbers
//! (`q4 = 16312503, q5 = 12501170, q7 = 26097816, q8 = 6241640`, from
//! `measurements/sf1-lsqb-run2-v12.json`) — the converted official corpus
//! lives on the benchmark pod. On a host with it, the gate is one command
//! and it fails closed:
//!
//! ```text
//! lsqbref <sf1-dir> --expect measurements/sf1-lsqb-run2-v12.json
//! ```
//!
//! `--expect` reads a flat `{"q1": n, …}` map or the lane's own report (null
//! counts — the lane's timeouts — carry no expectation), exactly as `lsqb`'s
//! `--expect` does, and exits 1 on any mismatch. Until that has been run on
//! the official corpus, the other five SF1 numbers this oracle produces are
//! candidates, not references.
//!
//! # Memory
//!
//! SF1 is 3.18M nodes / 17.26M relationships. Nodes are dense `u32` indexes in
//! file order; labels are a bitmask per node; each relationship type of
//! interest is a CSR (`offsets` + sorted `u32` targets) in the direction(s)
//! the queries walk it, and the multi-edge KNOWS adjacency is a run-length
//! CSR (unique neighbour + multiplicity). Nothing is per-node heap-allocated.
//! The only string-keyed structure is the id → index map built while reading
//! `nodes.jsonl`, dropped before any query runs.
//!
//! This binary needs a real clock for the per-query timings, which the
//! simulation layer's `Runtime` deliberately does not provide — the same lint
//! waiver the other bench binaries carry, for the same reason.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::time::Instant;

use engram_cypher::Value;
use engram_cypher::json::{from_json, to_json};

// ─── The nine queries, as measured ──────────────────────────────────────────

/// The nine LSQB queries, byte for byte the `adapted` text of `lsqb.rs`'s
/// table (a unit test holds them there). They are here so the file that
/// implements a count also STATES the query it is a count of.
const QUERIES: [(&str, &str); 9] = [
    (
        "q1",
        "MATCH (:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(:Person)\
         <-[:HAS_MEMBER]-(:Forum)-[:CONTAINER_OF]->(:Post)<-[:REPLY_OF]-(:Comment)\
         -[:HAS_TAG]->(:Tag)-[:HAS_TYPE]->(:TagClass) RETURN count(*) AS count",
    ),
    (
        "q2",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person), \
         (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
         -[:HAS_CREATOR]->(person2) RETURN count(*) AS count",
    ),
    (
        "q3",
        "MATCH (country:Country) \
         MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
         MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
         MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
         MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
         RETURN count(*) AS count",
    ),
    (
        "q4",
        "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person), \
         (message)<-[:LIKES]-(liker:Person), \
         (message)<-[:REPLY_OF]-(comment:Comment) RETURN count(*) AS count",
    ),
    (
        "q5",
        "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
         -[:HAS_TAG]->(tag2:Tag) WHERE tag1 <> tag2 RETURN count(*) AS count",
    ),
    (
        "q6",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
         -[:HAS_INTEREST]->(tag:Tag) WHERE person1 <> person3 RETURN count(*) AS count",
    ),
    (
        "q7",
        "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
         OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
         OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
         RETURN count(*) AS count",
    ),
    (
        "q8",
        "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
         -[:HAS_TAG]->(tag2:Tag) \
         WHERE NOT (comment)-[:HAS_TAG]->(tag1) AND tag1 <> tag2 \
         RETURN count(*) AS count",
    ),
    (
        "q9",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
         -[:HAS_INTEREST]->(tag:Tag) \
         WHERE NOT (person1)-[:KNOWS]-(person3) AND person1 <> person3 \
         RETURN count(*) AS count",
    ),
];

// ─── Labels and relationship types the queries name ─────────────────────────

/// Label bits. A node's mask carries every label it has; a pattern's label
/// test is `mask & bit != 0`, so a node matches every label it carries.
const L_PERSON: u16 = 1 << 0;
const L_COUNTRY: u16 = 1 << 1;
const L_CITY: u16 = 1 << 2;
const L_FORUM: u16 = 1 << 3;
const L_POST: u16 = 1 << 4;
const L_COMMENT: u16 = 1 << 5;
const L_MESSAGE: u16 = 1 << 6;
const L_TAG: u16 = 1 << 7;
const L_TAGCLASS: u16 = 1 << 8;

fn label_bit(name: &str) -> u16 {
    match name {
        "Person" => L_PERSON,
        "Country" => L_COUNTRY,
        "City" => L_CITY,
        "Forum" => L_FORUM,
        "Post" => L_POST,
        "Comment" => L_COMMENT,
        "Message" => L_MESSAGE,
        "Tag" => L_TAG,
        "TagClass" => L_TAGCLASS,
        _ => 0, // Place, Continent, Organisation, … — no query names them
    }
}

/// The relationship types the nine queries walk, as indexes into the raw
/// edge lists. Every other type in the corpus (`HAS_MODERATOR`, `STUDY_AT`,
/// `WORK_AT`, `IS_SUBCLASS_OF`, …) is counted and dropped.
const T_IS_PART_OF: usize = 0;
const T_IS_LOCATED_IN: usize = 1;
const T_HAS_MEMBER: usize = 2;
const T_CONTAINER_OF: usize = 3;
const T_REPLY_OF: usize = 4;
const T_HAS_TAG: usize = 5;
const T_HAS_TYPE: usize = 6;
const T_KNOWS: usize = 7;
const T_HAS_CREATOR: usize = 8;
const T_LIKES: usize = 9;
const T_HAS_INTEREST: usize = 10;
const N_TYPES: usize = 11;

fn rel_type_index(t: &str) -> Option<usize> {
    Some(match t {
        "IS_PART_OF" => T_IS_PART_OF,
        "IS_LOCATED_IN" => T_IS_LOCATED_IN,
        "HAS_MEMBER" => T_HAS_MEMBER,
        "CONTAINER_OF" => T_CONTAINER_OF,
        "REPLY_OF" => T_REPLY_OF,
        "HAS_TAG" => T_HAS_TAG,
        "HAS_TYPE" => T_HAS_TYPE,
        "KNOWS" => T_KNOWS,
        "HAS_CREATOR" => T_HAS_CREATOR,
        "LIKES" => T_LIKES,
        "HAS_INTEREST" => T_HAS_INTEREST,
        _ => return None,
    })
}

// ─── Reading the corpus ─────────────────────────────────────────────────────

/// The corpus as read: labels per dense node index, and per relationship
/// type of interest the `(src, dst)` pairs in file order. The JSONL contract
/// is the one `snbload` / `load_export` read — a node is `{"i", "l":
/// [labels], "p": {…}}`, a relationship `{"s", "d", "t", "p"}` — and it is
/// parsed the same way (`from_json` per line, `get_str` / `get_list` on the
/// map), so an id the loader would resolve, this resolves.
struct Raw {
    labels: Vec<u16>,
    edges: Vec<Vec<(u32, u32)>>,
    /// Census the summary prints: nodes read, relationships read, of which
    /// dropped (a type no query walks) or dangling (an endpoint not in
    /// `nodes.jsonl` — the loader skips those too, and counts them).
    nodes: u64,
    rels: u64,
    dropped: u64,
    dangling: u64,
}

/// A parse error the corpus reader refuses on. The loaders panic on the same
/// conditions; an oracle should say what it saw and exit.
fn read_lines(reader: impl BufRead, what: &str, mut f: impl FnMut(BTreeMap<String, Value>)) -> Result<(), String> {
    for (n, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("{what} line {}: read error: {e}", n + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        match from_json(&line) {
            // A non-object line is skipped, as `load_export` skips it.
            Ok(Value::Map(m)) => f(m),
            Ok(_) => {}
            Err(e) => return Err(format!("{what} line {}: {e}", n + 1)),
        }
    }
    Ok(())
}

impl Raw {
    /// Read a corpus from its two streams. `nodes` is consumed fully first —
    /// relationships resolve endpoints through the id map it builds.
    fn read(nodes: impl BufRead, rels: impl BufRead) -> Result<Raw, String> {
        let mut labels: Vec<u16> = Vec::new();
        let mut index: HashMap<String, u32> = HashMap::new();
        read_lines(nodes, "nodes.jsonl", |m| {
            let id = engram_bench::get_str(&m, "i");
            let mut mask = 0u16;
            for l in engram_bench::get_list(&m, "l") {
                if let Value::Str(s) = l {
                    mask |= label_bit(s);
                }
            }
            let dense = u32::try_from(labels.len()).expect("more than 2^32 nodes");
            labels.push(mask);
            // A duplicated id: the LAST occurrence wins, which is what
            // `load_export`'s `id_map.insert` does too.
            index.insert(id, dense);
        })?;
        let n_nodes = labels.len() as u64;

        let mut edges: Vec<Vec<(u32, u32)>> = (0..N_TYPES).map(|_| Vec::new()).collect();
        let mut n_rels = 0u64;
        let mut dropped = 0u64;
        let mut dangling = 0u64;
        let mut knows_self_loops = 0u64;
        read_lines(rels, "rels.jsonl", |m| {
            n_rels += 1;
            let Some(t) = rel_type_index(&engram_bench::get_str(&m, "t")) else {
                dropped += 1;
                return;
            };
            let (Some(&s), Some(&d)) = (
                index.get(&engram_bench::get_str(&m, "s")),
                index.get(&engram_bench::get_str(&m, "d")),
            ) else {
                dangling += 1;
                return;
            };
            if t == T_KNOWS && s == d {
                knows_self_loops += 1;
            }
            edges[t].push((s, d));
        })?;
        if knows_self_loops > 0 {
            return Err(format!(
                "{knows_self_loops} KNOWS self-loop(s) in the corpus; this oracle does not model an \
                 undirected self-loop's binding count (see the module docs) and refuses rather than guess"
            ));
        }
        Ok(Raw {
            labels,
            edges,
            nodes: n_nodes,
            rels: n_rels,
            dropped,
            dangling,
        })
    }
}

// ─── Compact adjacency ──────────────────────────────────────────────────────

/// A CSR adjacency: `targets[off[u]..off[u+1]]` are `u`'s neighbours, SORTED,
/// one entry per relationship (parallel rels appear as repeated targets).
struct Csr {
    off: Vec<u32>,
    targets: Vec<u32>,
}

impl Csr {
    /// Build from `(from, to)` pairs over `n` nodes. Counting sort by source,
    /// then each slice sorted, so lookups can binary-search.
    fn build(n: usize, pairs: impl Iterator<Item = (u32, u32)> + Clone) -> Csr {
        let mut off = vec![0u32; n + 1];
        for (s, _) in pairs.clone() {
            off[s as usize + 1] += 1;
        }
        for i in 0..n {
            off[i + 1] += off[i];
        }
        let mut fill: Vec<u32> = off[..n].to_vec();
        let mut targets = vec![0u32; off[n] as usize];
        for (s, d) in pairs {
            let slot = &mut fill[s as usize];
            targets[*slot as usize] = d;
            *slot += 1;
        }
        for u in 0..n {
            targets[off[u] as usize..off[u + 1] as usize].sort_unstable();
        }
        Csr { off, targets }
    }

    fn of(&self, u: u32) -> &[u32] {
        &self.targets[self.off[u as usize] as usize..self.off[u as usize + 1] as usize]
    }
}

/// A run-length CSR for a multigraph: `nbr[i]` a unique neighbour and
/// `mult[i]` how many relationships join the pair. Built from a plain
/// `Csr` by collapsing each sorted slice's runs.
struct MultiCsr {
    off: Vec<u32>,
    nbr: Vec<u32>,
    mult: Vec<u32>,
}

impl MultiCsr {
    fn from_csr(csr: &Csr) -> MultiCsr {
        let n = csr.off.len() - 1;
        let mut off = Vec::with_capacity(n + 1);
        let mut nbr = Vec::new();
        let mut mult = Vec::new();
        off.push(0u32);
        for u in 0..n {
            let slice = csr.of(u as u32);
            let mut i = 0;
            while i < slice.len() {
                let v = slice[i];
                let mut j = i + 1;
                while j < slice.len() && slice[j] == v {
                    j += 1;
                }
                nbr.push(v);
                mult.push((j - i) as u32);
                i = j;
            }
            off.push(nbr.len() as u32);
        }
        MultiCsr { off, nbr, mult }
    }

    /// `(neighbour, multiplicity)` pairs of `u`, sorted by neighbour.
    fn of(&self, u: u32) -> (&[u32], &[u32]) {
        let (a, b) = (self.off[u as usize] as usize, self.off[u as usize + 1] as usize);
        (&self.nbr[a..b], &self.mult[a..b])
    }

    /// The number of relationships joining `u` and `v` (0 when not adjacent).
    fn mult_between(&self, u: u32, v: u32) -> u64 {
        let (nbrs, mults) = self.of(u);
        match nbrs.binary_search(&v) {
            Ok(i) => u64::from(mults[i]),
            Err(_) => 0,
        }
    }

    /// The sum of multiplicities at `u` — the number of `(u)-[:T]-(x)`
    /// bindings, counting both orientations of every rel when the CSR was
    /// built undirected.
    fn degree(&self, u: u32) -> u64 {
        self.of(u).1.iter().map(|&m| u64::from(m)).sum()
    }
}

/// The corpus in the shapes the queries walk. Direction is named per field
/// (`_out` = from the node the query starts the hop at, `_in` = the reverse),
/// and `knows` is undirected with both orientations of every rel entered.
struct Corpus {
    labels: Vec<u16>,
    is_part_of_out: Csr,
    is_located_in_out: Csr,
    has_member_out: Csr,
    container_of_in: Csr,
    reply_of_out: Csr,
    reply_of_in: Csr,
    has_tag_out: Csr,
    has_type_out: Csr,
    has_creator_out: Csr,
    likes_in: Csr,
    has_interest_out: Csr,
    /// Person–Person KNOWS only (every KNOWS hop in the nine queries joins
    /// two `:Person`-labelled variables), unique neighbours + multiplicity.
    knows: MultiCsr,
}

impl Corpus {
    fn from_raw(raw: &Raw) -> Corpus {
        let n = raw.labels.len();
        let fwd = |t: usize| raw.edges[t].iter().copied();
        let rev = |t: usize| raw.edges[t].iter().map(|&(s, d)| (d, s));
        let labels = &raw.labels;
        let person = |x: u32| labels[x as usize] & L_PERSON != 0;
        let knows_both = raw.edges[T_KNOWS]
            .iter()
            .filter(|&&(s, d)| person(s) && person(d))
            .flat_map(|&(s, d)| [(s, d), (d, s)]);
        Corpus {
            is_part_of_out: Csr::build(n, fwd(T_IS_PART_OF)),
            is_located_in_out: Csr::build(n, fwd(T_IS_LOCATED_IN)),
            has_member_out: Csr::build(n, fwd(T_HAS_MEMBER)),
            container_of_in: Csr::build(n, rev(T_CONTAINER_OF)),
            reply_of_out: Csr::build(n, fwd(T_REPLY_OF)),
            reply_of_in: Csr::build(n, rev(T_REPLY_OF)),
            has_tag_out: Csr::build(n, fwd(T_HAS_TAG)),
            has_type_out: Csr::build(n, fwd(T_HAS_TYPE)),
            has_creator_out: Csr::build(n, fwd(T_HAS_CREATOR)),
            likes_in: Csr::build(n, rev(T_LIKES)),
            has_interest_out: Csr::build(n, fwd(T_HAS_INTEREST)),
            knows: MultiCsr::from_csr(&Csr::build(n, knows_both)),
            labels: raw.labels.clone(),
        }
    }

    fn has(&self, x: u32, bit: u16) -> bool {
        self.labels[x as usize] & bit != 0
    }

    /// How many entries of `slice` carry `bit` — a hop's binding count under
    /// its label test, parallel rels included.
    fn count_labelled(&self, slice: &[u32], bit: u16) -> u64 {
        slice.iter().filter(|&&x| self.has(x, bit)).count() as u64
    }

    fn nodes_with(&self, bit: u16) -> impl Iterator<Item = u32> + '_ {
        (0..self.labels.len() as u32).filter(move |&x| self.has(x, bit))
    }

    /// `Σ_t m_a(t) · m_b(t)` over the `:Tag` targets both sorted HAS_TAG
    /// slices share — the number of `(tag1, tag2)` rel-pair bindings with
    /// `tag1 = tag2`. Run-length merge of two sorted slices.
    fn shared_tag_pairs(&self, a: &[u32], b: &[u32]) -> u64 {
        let (mut i, mut j, mut total) = (0, 0, 0u64);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    let t = a[i];
                    let (mut ma, mut mb) = (0u64, 0u64);
                    while i < a.len() && a[i] == t {
                        ma += 1;
                        i += 1;
                    }
                    while j < b.len() && b[j] == t {
                        mb += 1;
                        j += 1;
                    }
                    if self.has(t, L_TAG) {
                        total += ma * mb;
                    }
                }
            }
        }
        total
    }

    /// The HAS_TAG bindings of `a` whose tag `b` also has (a rel-weighted
    /// membership sum): `Σ_{t ∈ tags(b)} m_a(t)`.
    fn tag_bindings_shared_with(&self, a: &[u32], b: &[u32]) -> u64 {
        let mut j = 0;
        let mut total = 0u64;
        for &t in a {
            while j < b.len() && b[j] < t {
                j += 1;
            }
            if j < b.len() && b[j] == t && self.has(t, L_TAG) {
                total += 1;
            }
        }
        total
    }
}

// ─── The nine counts ────────────────────────────────────────────────────────

/// q1 — a nine-hop CHAIN with the Post in the middle: a Yannakakis-style
/// semi-join sum. Fold each arm's per-node embedding counts toward the Post,
/// then multiply the two arms per Post. Every hop has a distinct type, so
/// relationship isomorphism holds automatically.
fn q1(c: &Corpus) -> u64 {
    let n = c.labels.len();
    // Left arm: Country <- City <- Person <- Forum -> Post.
    let mut w_city = vec![0u64; n];
    for x in c.nodes_with(L_CITY) {
        w_city[x as usize] = c.count_labelled(c.is_part_of_out.of(x), L_COUNTRY);
    }
    let mut w_person = vec![0u64; n];
    for p in c.nodes_with(L_PERSON) {
        w_person[p as usize] = c
            .is_located_in_out
            .of(p)
            .iter()
            .map(|&x| w_city[x as usize])
            .sum();
    }
    let mut w_forum = vec![0u64; n];
    for f in c.nodes_with(L_FORUM) {
        w_forum[f as usize] = c
            .has_member_out
            .of(f)
            .iter()
            .map(|&p| w_person[p as usize])
            .sum();
    }
    // Right arm: Post <- Comment -> Tag -> TagClass.
    let mut w_tag = vec![0u64; n];
    for t in c.nodes_with(L_TAG) {
        w_tag[t as usize] = c.count_labelled(c.has_type_out.of(t), L_TAGCLASS);
    }
    let mut w_comment = vec![0u64; n];
    for cm in c.nodes_with(L_COMMENT) {
        w_comment[cm as usize] = c.has_tag_out.of(cm).iter().map(|&t| w_tag[t as usize]).sum();
    }
    let mut total = 0u64;
    for post in c.nodes_with(L_POST) {
        let left: u64 = c
            .container_of_in
            .of(post)
            .iter()
            .map(|&f| w_forum[f as usize])
            .sum();
        if left == 0 {
            continue;
        }
        let right: u64 = c
            .reply_of_in
            .of(post)
            .iter()
            .map(|&cm| w_comment[cm as usize])
            .sum();
        total += left * right;
    }
    total
}

/// q2 — the CYCLE closes on person2: for every `(comment)-[:REPLY_OF]->(post)`
/// and every creator pair `(person1 of comment, person2 of post)`, the number
/// of KNOWS rels between them in either direction (`m(person1, person2)`,
/// the undirected binding count). `person1 = person2` needs a KNOWS self-loop
/// and contributes 0 (none exist — refused at load).
fn q2(c: &Corpus) -> u64 {
    let mut total = 0u64;
    for comment in c.nodes_with(L_COMMENT) {
        for &post in c.reply_of_out.of(comment) {
            if !c.has(post, L_POST) {
                continue;
            }
            for &p1 in c.has_creator_out.of(comment) {
                if !c.has(p1, L_PERSON) {
                    continue;
                }
                for &p2 in c.has_creator_out.of(post) {
                    if c.has(p2, L_PERSON) {
                        total += c.knows.mult_between(p1, p2);
                    }
                }
            }
        }
    }
    total
}

/// Per person, the `(country, paths)` pairs of
/// `(person)-[:IS_LOCATED_IN]->(:City)-[:IS_PART_OF]->(country:Country)`,
/// sorted by country — q3's three location clauses share `country`, so a
/// triangle contributes `Σ_c paths1(c)·paths2(c)·paths3(c)`.
fn person_countries(c: &Corpus) -> MultiCsr {
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for p in c.nodes_with(L_PERSON) {
        for &city in c.is_located_in_out.of(p) {
            if !c.has(city, L_CITY) {
                continue;
            }
            for &country in c.is_part_of_out.of(city) {
                if c.has(country, L_COUNTRY) {
                    pairs.push((p, country));
                }
            }
        }
    }
    MultiCsr::from_csr(&Csr::build(c.labels.len(), pairs.iter().copied()))
}

/// `Σ_c a(c)·b(c)·d(c)` over three sorted `(country, paths)` lists.
fn shared_country_paths(pc: &MultiCsr, u: u32, v: u32, w: u32) -> u64 {
    let (cu, mu) = pc.of(u);
    let (cv, mv) = pc.of(v);
    let (cw, mw) = pc.of(w);
    let (mut i, mut j, mut k, mut total) = (0, 0, 0, 0u64);
    while i < cu.len() && j < cv.len() && k < cw.len() {
        let m = cu[i].min(cv[j]).min(cw[k]);
        if cu[i] == m && cv[j] == m && cw[k] == m {
            total += u64::from(mu[i]) * u64::from(mv[j]) * u64::from(mw[k]);
            i += 1;
            j += 1;
            k += 1;
        } else {
            if cu[i] == m {
                i += 1;
            }
            if cv[j] == m {
                j += 1;
            }
            if cw[k] == m {
                k += 1;
            }
        }
    }
    total
}

/// q3 — the KNOWS TRIANGLE, all three persons in one country. Enumerate each
/// unordered triangle `u < v < w` once by intersecting sorted neighbour
/// lists; every unordered triangle is 3! = 6 ordered `(person1, person2,
/// person3)` rows, each weighted by the same `m(u,v)·m(v,w)·m(w,u)` (the three
/// rels join three different pairs, so isomorphism is automatic) and the
/// same shared-country path product.
fn q3(c: &Corpus) -> u64 {
    let pc = person_countries(c);
    let mut total = 0u64;
    for u in c.nodes_with(L_PERSON) {
        let (nu, mu) = c.knows.of(u);
        for (iv, &v) in nu.iter().enumerate() {
            if v <= u {
                continue;
            }
            let (nv, mv) = c.knows.of(v);
            // Intersect the parts of N(u) and N(v) above v.
            let (mut i, mut j) = (iv + 1, nv.partition_point(|&x| x <= v));
            while i < nu.len() && j < nv.len() {
                match nu[i].cmp(&nv[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        let w = nu[i];
                        let rels = u64::from(mu[iv]) * u64::from(mv[j]) * u64::from(mu[i]);
                        total += 6 * rels * shared_country_paths(&pc, u, v, w);
                        i += 1;
                        j += 1;
                    }
                }
            }
        }
    }
    total
}

/// The four per-message leg counts q4 and q7 multiply, each under its label
/// test: `(:Tag)<-[:HAS_TAG]-`, `-[:HAS_CREATOR]->(:Person)`,
/// `<-[:LIKES]-(:Person)`, `<-[:REPLY_OF]-(:Comment)`.
fn message_legs(c: &Corpus, m: u32) -> (u64, u64, u64, u64) {
    (
        c.count_labelled(c.has_tag_out.of(m), L_TAG),
        c.count_labelled(c.has_creator_out.of(m), L_PERSON),
        c.count_labelled(c.likes_in.of(m), L_PERSON),
        c.count_labelled(c.reply_of_in.of(m), L_COMMENT),
    )
}

/// q4 — a STAR on `message`: four independent legs, so the row count is the
/// product of the four degrees (all types distinct — isomorphism automatic).
fn q4(c: &Corpus) -> u64 {
    c.nodes_with(L_MESSAGE)
        .map(|m| {
            let (tags, creators, likes, replies) = message_legs(c, m);
            tags * creators * likes * replies
        })
        .sum()
}

/// q7 — the same star with the LIKES and REPLY_OF legs OPTIONAL: an optional
/// leg with no match keeps the row (one null-filled row), so each optional
/// leg multiplies by `max(1, degree)` where q4 multiplies by `degree`.
fn q7(c: &Corpus) -> u64 {
    c.nodes_with(L_MESSAGE)
        .map(|m| {
            let (tags, creators, likes, replies) = message_legs(c, m);
            tags * creators * likes.max(1) * replies.max(1)
        })
        .sum()
}

/// q5 — for every `(comment)-[:REPLY_OF]->(message:Message)` (Posts AND
/// Comments — the supertype label), the `(tag1, tag2)` rel-pair bindings with
/// `tag1 <> tag2`: all pairs minus the pairs on a shared tag. HAS_TAG rels are
/// counted with multiplicity on both sides. The two HAS_TAG hops start at
/// different nodes (no REPLY_OF self-loops), so isomorphism is automatic.
fn q5(c: &Corpus) -> u64 {
    let mut total = 0u64;
    for comment in c.nodes_with(L_COMMENT) {
        let ct = c.has_tag_out.of(comment);
        let n_ct = c.count_labelled(ct, L_TAG);
        if n_ct == 0 {
            continue;
        }
        for &message in c.reply_of_out.of(comment) {
            if !c.has(message, L_MESSAGE) {
                continue;
            }
            let mt = c.has_tag_out.of(message);
            let n_mt = c.count_labelled(mt, L_TAG);
            total += n_mt * n_ct - c.shared_tag_pairs(mt, ct);
        }
    }
    total
}

/// q8 — q5's shape with the ANTI-JOIN `NOT (comment)-[:HAS_TAG]->(tag1)`:
/// a `tag1` binding survives iff the comment has NO HAS_TAG to that tag
/// (membership, not multiplicity), and every `tag2` of the comment then
/// differs from it, so `tag1 <> tag2` is implied. Per reply:
/// `|tags(comment)| · (|tags(message)| − bindings of message on tags the
/// comment also has)`.
fn q8(c: &Corpus) -> u64 {
    let mut total = 0u64;
    for comment in c.nodes_with(L_COMMENT) {
        let ct = c.has_tag_out.of(comment);
        let n_ct = c.count_labelled(ct, L_TAG);
        if n_ct == 0 {
            continue;
        }
        for &message in c.reply_of_out.of(comment) {
            if !c.has(message, L_MESSAGE) {
                continue;
            }
            let mt = c.has_tag_out.of(message);
            let n_mt = c.count_labelled(mt, L_TAG);
            total += n_ct * (n_mt - c.tag_bindings_shared_with(mt, ct));
        }
    }
    total
}

/// `interest(p)` — the `(p)-[:HAS_INTEREST]->(:Tag)` bindings.
fn interest(c: &Corpus, p: u32) -> u64 {
    c.count_labelled(c.has_interest_out.of(p), L_TAG)
}

/// q6 — the two-hop KNOWS WEDGE centred on person2, `person1 <> person3`,
/// weighted by person3's interests. Per centre: every `(person1, person3)`
/// pair of neighbours is `m(p2,p1)·m(p2,p3)`, so summing over all pairs then
/// removing the `person1 = person3` diagonal gives
/// `deg(p2)·Σ_x m(p2,x)·interest(x) − Σ_x m(p2,x)²·interest(x)`.
/// The `<>` is what makes isomorphism automatic (see the module docs).
fn q6(c: &Corpus) -> u64 {
    let mut total = 0u64;
    for p2 in c.nodes_with(L_PERSON) {
        let (nbrs, mults) = c.knows.of(p2);
        let deg = c.knows.degree(p2);
        let mut weighted = 0u64;
        let mut diagonal = 0u64;
        for (&x, &m) in nbrs.iter().zip(mults) {
            let m = u64::from(m);
            let i = interest(c, x);
            weighted += m * i;
            diagonal += m * m * i;
        }
        total += deg * weighted - diagonal;
    }
    total
}

/// q9 — q6's wedge with the ANTI-TRIANGLE `NOT (person1)-[:KNOWS]-(person3)`
/// (membership: any rel in either direction closes it). Per `(p2, p3)`, the
/// surviving `person1` bindings are p2's bindings minus the `person1 = p3`
/// ones (`m(p2,p3)`, the `<>` filter) minus those on the common neighbours
/// of p2 and p3 (a sorted-vector intersection of two KNOWS lists) — the two
/// exclusions are disjoint because p3 is never its own neighbour.
fn q9(c: &Corpus) -> u64 {
    let mut total = 0u64;
    for p2 in c.nodes_with(L_PERSON) {
        let (n2, m2) = c.knows.of(p2);
        let deg = c.knows.degree(p2);
        for (&p3, &m23) in n2.iter().zip(m2) {
            let i3 = interest(c, p3);
            if i3 == 0 {
                continue;
            }
            let (n3, _) = c.knows.of(p3);
            let mut closed = 0u64;
            let (mut i, mut j) = (0, 0);
            while i < n2.len() && j < n3.len() {
                match n2[i].cmp(&n3[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        closed += u64::from(m2[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            total += u64::from(m23) * i3 * (deg - u64::from(m23) - closed);
        }
    }
    total
}

/// One count over the corpus.
type Count = fn(&Corpus) -> u64;

/// The nine, in table order, each with the method name the stderr line
/// reports.
const METHODS: [(&str, &str, Count); 9] = [
    ("q1", "yannakakis chain (semi-join sums folded onto Post)", q1),
    ("q2", "reply pairs x undirected KNOWS multiplicity lookup", q2),
    ("q3", "sorted-neighbour triangle intersection x 6 x shared-country paths", q3),
    ("q4", "per-message degree product (4 legs)", q4),
    ("q5", "per-reply |tags|x|tags| minus shared-tag run-length merge", q5),
    ("q6", "per-centre wedge sum minus diagonal", q6),
    ("q7", "per-message degree product, optional legs as max(1,deg)", q7),
    ("q8", "per-reply |tags| x (|tags| minus membership-shared bindings)", q8),
    ("q9", "per-edge wedge count minus common-neighbour intersection", q9),
];

// ─── `--expect` (the same document shapes `lsqb --expect` accepts) ──────────

/// Parse an `--expect` document: a flat `{"q1": 123, …}` map, or the lane's
/// own report (`{"queries":[{"query":…,"count":…}]}`) — mirrors
/// `lsqb.rs`'s `parse_expect`. Null counts carry no expectation; unknown
/// names and non-integers refuse.
fn parse_expect(src: &str) -> Result<BTreeMap<String, i64>, String> {
    let known = |k: &str| QUERIES.iter().any(|(name, _)| *name == k);
    let v = from_json(src).map_err(|e| format!("--expect is not JSON: {e}"))?;
    let Value::Map(m) = v else {
        return Err("--expect must be a JSON object".to_string());
    };
    let mut out = BTreeMap::new();
    if let Some(Value::List(entries)) = m.get("queries") {
        for e in entries {
            let Value::Map(em) = e else {
                return Err("--expect: entries under \"queries\" must be objects".to_string());
            };
            let Some(Value::Str(name)) = em.get("query") else {
                return Err("--expect: an entry under \"queries\" has no \"query\" name".to_string());
            };
            if !known(name) {
                return Err(format!("--expect names unknown query {name:?}"));
            }
            match em.get("count") {
                Some(Value::Int(n)) => {
                    out.insert(name.clone(), *n);
                }
                Some(Value::Null) | None => {}
                Some(other) => {
                    return Err(format!("--expect: {name} has a non-integer count {other:?}"));
                }
            }
        }
    } else {
        for (k, val) in &m {
            if !known(k) {
                return Err(format!("--expect names unknown query {k:?}"));
            }
            match val {
                Value::Int(n) => {
                    out.insert(k.clone(), *n);
                }
                other => return Err(format!("--expect: {k} must be an integer, got {other:?}")),
            }
        }
    }
    if out.is_empty() {
        return Err("--expect carries no usable counts".to_string());
    }
    Ok(out)
}

/// The flat `{"q1": n, …}` map `lsqb --expect` accepts.
fn render_counts(counts: &BTreeMap<&'static str, u64>) -> Result<String, String> {
    let mut m = BTreeMap::new();
    for (name, n) in counts {
        let n = i64::try_from(*n).map_err(|_| format!("{name} = {n} does not fit an i64"))?;
        m.insert((*name).to_string(), Value::Int(n));
    }
    Ok(to_json(&Value::Map(m)))
}

// ─── The run ────────────────────────────────────────────────────────────────

fn usage() -> ! {
    eprintln!("usage: lsqbref <corpus dir> [--json <out>] [--expect <counts.json>]");
    eprintln!("  computes the nine LSQB counts from nodes.jsonl + rels.jsonl with no graph engine.");
    eprintln!("  --expect: a flat {{\"q1\": 123, ...}} map, or an lsqb --json report; any mismatch exits 1.");
    std::process::exit(2);
}

fn open(path: &std::path::Path) -> std::io::BufReader<std::fs::File> {
    match std::fs::File::open(path) {
        Ok(f) => std::io::BufReader::with_capacity(1 << 20, f),
        Err(e) => {
            eprintln!("[lsqbref] cannot open {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut dir: Option<String> = None;
    let mut json_out: Option<String> = None;
    let mut expect: Option<BTreeMap<String, i64>> = None;
    let mut i = 1;
    while i < args.len() {
        let take = |i: usize| -> &str {
            args.get(i + 1).map(String::as_str).unwrap_or_else(|| {
                eprintln!("[lsqbref] {} needs a value", args[i]);
                usage()
            })
        };
        match args[i].as_str() {
            "--json" => {
                json_out = Some(take(i).to_string());
                i += 2;
            }
            "--expect" => {
                let path = take(i);
                let src = std::fs::read_to_string(path).unwrap_or_else(|e| {
                    eprintln!("[lsqbref] cannot read --expect {path}: {e}");
                    std::process::exit(2);
                });
                match parse_expect(&src) {
                    Ok(m) => expect = Some(m),
                    Err(e) => {
                        eprintln!("[lsqbref] {e}");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            flag if flag.starts_with("--") => {
                eprintln!("[lsqbref] unknown flag {flag}");
                usage();
            }
            positional => {
                if dir.is_some() {
                    eprintln!("[lsqbref] unexpected extra argument {positional:?}");
                    usage();
                }
                dir = Some(positional.to_string());
                i += 1;
            }
        }
    }
    let Some(dir) = dir else { usage() };
    let dir = std::path::PathBuf::from(dir);

    // ── Read ───────────────────────────────────────────────────────────────
    let t0 = Instant::now();
    let raw = match Raw::read(open(&dir.join("nodes.jsonl")), open(&dir.join("rels.jsonl"))) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[lsqbref] FAIL — {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[lsqbref] read {} node(s), {} relationship(s) ({} of a type no query walks, {} dangling) in {:.1}s",
        raw.nodes,
        raw.rels,
        raw.dropped,
        raw.dangling,
        t0.elapsed().as_secs_f64()
    );
    // An empty corpus answers 0 to everything and would "agree" with any
    // engine that also failed to load — the lane's census rule, applied here.
    let persons = raw.labels.iter().filter(|&&l| l & L_PERSON != 0).count();
    if raw.nodes == 0 || persons == 0 {
        eprintln!("[lsqbref] FAIL — corpus is empty ({} nodes, {persons} persons); nothing to count", raw.nodes);
        std::process::exit(1);
    }
    let t1 = Instant::now();
    let corpus = Corpus::from_raw(&raw);
    drop(raw);
    eprintln!(
        "[lsqbref] adjacency built in {:.1}s ({persons} persons; KNOWS bindings {})",
        t1.elapsed().as_secs_f64(),
        corpus.knows.mult.iter().map(|&m| u64::from(m)).sum::<u64>()
    );

    // ── Count ──────────────────────────────────────────────────────────────
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut mismatches = 0usize;
    for (name, method, f) in METHODS {
        let t = Instant::now();
        let n = f(&corpus);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let verdict = match expect.as_ref().and_then(|m| m.get(name)) {
            None => String::new(),
            Some(&e) if i64::try_from(n) == Ok(e) => format!("  == expected {e}"),
            Some(&e) => {
                mismatches += 1;
                format!("  MISMATCH: expected {e}")
            }
        };
        eprintln!("[lsqbref] {name:<3} count={n:<12} {ms:>10.1} ms  method={method}{verdict}");
        counts.insert(name, n);
    }

    // ── Emit ───────────────────────────────────────────────────────────────
    let doc = match render_counts(&counts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[lsqbref] FAIL — {e}");
            std::process::exit(1);
        }
    };
    if let Some(path) = &json_out {
        match std::fs::write(path, &doc) {
            Ok(()) => eprintln!("[lsqbref] counts written to {path}"),
            Err(e) => {
                eprintln!("[lsqbref] cannot write {path}: {e}");
                println!("{doc}");
                std::process::exit(1);
            }
        }
    }
    println!("{doc}");
    if mismatches > 0 {
        eprintln!("[lsqbref] FAIL — {mismatches} count(s) differ from --expect; the oracle and the reference disagree");
        std::process::exit(1);
    }
    if expect.is_some() {
        eprintln!("[lsqbref] PASS — every expected count reproduced");
    }
}

// ─── Tests: the arithmetic, on fixtures whose counts are derived by hand ────

#[cfg(test)]
mod tests {
    use super::*;

    fn all_counts(c: &Corpus) -> BTreeMap<&'static str, u64> {
        METHODS.iter().map(|(name, _, f)| (*name, f(c))).collect()
    }

    /// A corpus from in-memory JSONL — the same parse path the files take.
    fn corpus(nodes: &str, rels: &str) -> Corpus {
        let raw = Raw::read(nodes.as_bytes(), rels.as_bytes()).expect("read fixture");
        Corpus::from_raw(&raw)
    }

    fn node(id: &str, labels: &[&str]) -> String {
        let ls: Vec<String> = labels.iter().map(|l| format!("\"{l}\"")).collect();
        format!("{{\"i\":\"{id}\",\"l\":[{}],\"p\":{{}}}}\n", ls.join(","))
    }
    fn rel(s: &str, t: &str, d: &str) -> String {
        format!("{{\"s\":\"{s}\",\"d\":\"{d}\",\"t\":\"{t}\",\"p\":{{}}}}\n")
    }

    /// The ten-node fixture. Every count below is derived by hand in the
    /// test that asserts it.
    ///
    /// ```text
    /// country:0 <-IS_PART_OF- city:0 <-IS_LOCATED_IN- A, B, C   (Persons)
    /// forum:0  -HAS_MEMBER-> A, B;  forum:0 -CONTAINER_OF-> post:0
    /// post:0 [Message,Post]     -HAS_CREATOR-> A;  -HAS_TAG-> T1, T2
    /// comment:0 [Message,Comment] -REPLY_OF-> post:0; -HAS_CREATOR-> B; -HAS_TAG-> T1
    /// C -LIKES-> post:0;  A -LIKES-> comment:0
    /// T1 [Tag] -HAS_TYPE-> T2;   T2 [Tag, TagClass]  (dual-labelled: the
    ///   tenth node is both the second tag q5/q8 need and the TagClass q1
    ///   needs — a node matches every label it carries)
    /// KNOWS: A->B, A->B (parallel), B->C, C->A   — m(A,B)=2, m(B,C)=1, m(C,A)=1
    /// HAS_INTEREST: A->T1, B->T1, B->T2, C->T2   — interest A=1, B=2, C=1
    /// ```
    fn ten_nodes() -> (String, String) {
        let nodes = [
            node("country:0", &["Place", "Country"]),
            node("city:0", &["Place", "City"]),
            node("A", &["Person"]),
            node("B", &["Person"]),
            node("C", &["Person"]),
            node("forum:0", &["Forum"]),
            node("post:0", &["Message", "Post"]),
            node("comment:0", &["Message", "Comment"]),
            node("T1", &["Tag"]),
            node("T2", &["Tag", "TagClass"]),
        ]
        .concat();
        let rels = [
            rel("city:0", "IS_PART_OF", "country:0"),
            rel("A", "IS_LOCATED_IN", "city:0"),
            rel("B", "IS_LOCATED_IN", "city:0"),
            rel("C", "IS_LOCATED_IN", "city:0"),
            rel("forum:0", "HAS_MEMBER", "A"),
            rel("forum:0", "HAS_MEMBER", "B"),
            rel("forum:0", "CONTAINER_OF", "post:0"),
            rel("post:0", "HAS_CREATOR", "A"),
            rel("post:0", "HAS_TAG", "T1"),
            rel("post:0", "HAS_TAG", "T2"),
            rel("comment:0", "REPLY_OF", "post:0"),
            rel("comment:0", "HAS_CREATOR", "B"),
            rel("comment:0", "HAS_TAG", "T1"),
            rel("C", "LIKES", "post:0"),
            rel("A", "LIKES", "comment:0"),
            rel("T1", "HAS_TYPE", "T2"),
            rel("A", "KNOWS", "B"),
            rel("A", "KNOWS", "B"),
            rel("B", "KNOWS", "C"),
            rel("C", "KNOWS", "A"),
            rel("A", "HAS_INTEREST", "T1"),
            rel("B", "HAS_INTEREST", "T1"),
            rel("B", "HAS_INTEREST", "T2"),
            rel("C", "HAS_INTEREST", "T2"),
            // A type no query walks: counted, dropped, never miscounted.
            rel("forum:0", "HAS_MODERATOR", "A"),
        ]
        .concat();
        (nodes, rels)
    }

    #[test]
    fn ten_node_fixture_counts_by_hand() {
        let (nodes, rels) = ten_nodes();
        let c = corpus(&nodes, &rels);
        let got = all_counts(&c);
        // q1: post:0's left arm = w_forum = w_person(A) + w_person(B) = 1 + 1
        //     (each: one city, one country); right arm = w_comment(comment:0)
        //     = w_tag(T1) = one HAS_TYPE to a TagClass. 2 × 1.
        assert_eq!(got["q1"], 2, "q1");
        // q2: one reply (comment:0 -> post:0); person1 = B, person2 = A;
        //     m(B, A) = 2 (the parallel pair, undirected).
        assert_eq!(got["q2"], 2, "q2");
        // q3: one triangle {A,B,C}, all in country:0 → 3! orderings × 2·1·1.
        assert_eq!(got["q3"], 12, "q3");
        // q4: post:0 = 2 tags × 1 creator × 1 like × 1 reply = 2;
        //     comment:0 = 1 × 1 × 1 × 0 replies = 0.
        assert_eq!(got["q4"], 2, "q4");
        // q5: the reply's (tag1 ∈ {T1,T2}, tag2 ∈ {T1}) pairs with tag1 ≠ tag2:
        //     (T2, T1) only.
        assert_eq!(got["q5"], 1, "q5");
        // q6: centre A: (B,C): 2·1·int(C)=2, (C,B): 1·2·int(B)=4 → 6;
        //     centre B: (A,C): 2·1·1=2, (C,A): 1·2·1=2 → 4;
        //     centre C: (A,B): 1·1·2=2, (B,A): 1·1·1=1 → 3.  Total 13.
        assert_eq!(got["q6"], 13, "q6");
        // q7: post:0 = 2·1·max(1,1)·max(1,1) = 2; comment:0 = 1·1·1·max(1,0) = 1.
        assert_eq!(got["q7"], 3, "q7");
        // q8: tag1 must be absent from comment:0's tags → tag1 = T2; tag2 = T1. 1.
        assert_eq!(got["q8"], 1, "q8");
        // q9: every wedge is closed (a triangle), so the anti-triangle keeps
        //     nothing — a zero that is PROVEN by q6 = 13 open+closed wedges.
        assert_eq!(got["q9"], 0, "q9");
    }

    /// A fourth person D, adjacent to A only (an open wedge through A), with
    /// one interest: q9 becomes non-zero and the `q9 = q6 − closed` identity
    /// is checked against hand-derived numbers.
    #[test]
    fn open_wedge_makes_q9_non_zero() {
        let (mut nodes, mut rels) = ten_nodes();
        nodes.push_str(&node("D", &["Person"]));
        rels.push_str(&rel("D", "KNOWS", "A"));
        rels.push_str(&rel("D", "HAS_INTEREST", "T2"));
        let c = corpus(&nodes, &rels);
        let got = all_counts(&c);
        // q6: centre A now has B(2), C(1), D(1): deg 4; Σ m·int = 2·2+1+1 = 6;
        //     diagonal = 4·2+1+1 = 10 → 24 − 10 = 14. B, C unchanged (4, 3).
        //     D has one neighbour → 0. Total 21.
        assert_eq!(got["q6"], 21, "q6");
        // q9: only centre A has open wedges. p3=B (m 2, int 2): person1 ∈
        //     {C,D} minus N(B)={A,C} → D (m 1) → 2·2·1 = 4. p3=C (m 1, int 1):
        //     {B,D} minus N(C)={A,B} → D → 1. p3=D (m 1, int 1): {B,C} minus
        //     N(D)={A} → 2+1 = 3 → 3. Total 8.
        assert_eq!(got["q9"], 8, "q9");
        // Everything else is untouched by a person outside every other shape.
        assert_eq!(got["q1"], 2);
        assert_eq!(got["q2"], 2);
        assert_eq!(got["q3"], 12);
        assert_eq!(got["q4"], 2);
        assert_eq!(got["q5"], 1);
        assert_eq!(got["q7"], 3);
        assert_eq!(got["q8"], 1);
    }

    /// A comment replying to a COMMENT (created by C, tagged T2): q5/q8 count
    /// it (`message:Message`), q1/q2 do not (`:Post`), q4/q7 see comment:0's
    /// new reply leg.
    #[test]
    fn reply_to_a_comment_is_a_message_but_not_a_post() {
        let (mut nodes, mut rels) = ten_nodes();
        nodes.push_str(&node("comment:1", &["Message", "Comment"]));
        rels.push_str(&rel("comment:1", "REPLY_OF", "comment:0"));
        rels.push_str(&rel("comment:1", "HAS_CREATOR", "C"));
        rels.push_str(&rel("comment:1", "HAS_TAG", "T2"));
        let c = corpus(&nodes, &rels);
        let got = all_counts(&c);
        assert_eq!(got["q1"], 2, "q1 walks REPLY_OF into a Post only");
        assert_eq!(got["q2"], 2, "q2 needs post:Post — C knows B but the reply target is a Comment");
        // q4: comment:0 now has 1 reply → 1·1·1·1 = 1 more; comment:1 has no likes → 0.
        assert_eq!(got["q4"], 3, "q4");
        // q7: comment:1 = 1 tag · 1 creator · max(1,0) · max(1,0) = 1 more.
        assert_eq!(got["q7"], 4, "q7");
        // q5: the new reply pairs T(comment:0)={T1} with T(comment:1)={T2}: 1 more.
        assert_eq!(got["q5"], 2, "q5");
        // q8: tag1 = T1 is not on comment:1 → 1 · (1 − 0) = 1 more.
        assert_eq!(got["q8"], 2, "q8");
        assert_eq!(got["q3"], 12);
        assert_eq!(got["q6"], 13);
        assert_eq!(got["q9"], 0);
    }

    /// Label tests are applied at every hop: a LIKES from a non-Person and a
    /// HAS_TAG to a non-Tag are bindings the pattern refuses.
    #[test]
    fn label_tests_apply_at_every_hop() {
        let (mut nodes, mut rels) = ten_nodes();
        nodes.push_str(&node("bot", &["Organisation"]));
        rels.push_str(&rel("bot", "LIKES", "post:0"));
        rels.push_str(&rel("post:0", "HAS_TAG", "bot"));
        rels.push_str(&rel("comment:0", "HAS_TAG", "bot"));
        let c = corpus(&nodes, &rels);
        let got = all_counts(&c);
        assert_eq!(got["q4"], 2, "a non-Person liker and a non-Tag tag add no rows");
        assert_eq!(got["q7"], 3);
        assert_eq!(got["q5"], 1, "a shared non-Tag target is not a (tag1, tag2) pair");
        assert_eq!(got["q8"], 1);
    }

    #[test]
    fn knows_self_loop_is_refused_not_guessed() {
        let (nodes, mut rels) = ten_nodes();
        rels.push_str(&rel("A", "KNOWS", "A"));
        let err = Raw::read(nodes.as_bytes(), rels.as_bytes()).err().expect("must refuse");
        assert!(err.contains("self-loop"), "{err}");
    }

    #[test]
    fn dangling_and_unwalked_relationships_are_counted_not_dropped_silently() {
        let (nodes, mut rels) = ten_nodes();
        rels.push_str(&rel("A", "KNOWS", "nobody"));
        let raw = Raw::read(nodes.as_bytes(), rels.as_bytes()).expect("read");
        assert_eq!(raw.nodes, 10);
        assert_eq!(raw.rels, 26);
        assert_eq!(raw.dropped, 1, "HAS_MODERATOR");
        assert_eq!(raw.dangling, 1, "the KNOWS to nobody");
        // …and the dangling rel changed no count.
        assert_eq!(all_counts(&Corpus::from_raw(&raw))["q6"], 13);
    }

    /// The multiplicity lookup and the undirected degree the KNOWS arithmetic
    /// rests on.
    #[test]
    fn knows_multiplicity_is_symmetric_and_undirected() {
        let (nodes, rels) = ten_nodes();
        let c = corpus(&nodes, &rels);
        // Dense indexes are file order: A, B, C are the 3rd, 4th, 5th lines.
        let (a, b, cc) = (2u32, 3u32, 4u32);
        assert_eq!(c.knows.mult_between(a, b), 2);
        assert_eq!(c.knows.mult_between(b, a), 2);
        assert_eq!(c.knows.mult_between(cc, a), 1);
        assert_eq!(c.knows.mult_between(a, cc), 1);
        assert_eq!(c.knows.mult_between(a, a), 0);
        assert_eq!(c.knows.degree(a), 3, "A has 3 KNOWS bindings: B, B, C");
        assert_eq!(c.knows.degree(b), 3);
        assert_eq!(c.knows.degree(cc), 2);
    }

    /// The query texts here are byte for byte the lane's `adapted` texts —
    /// read from `lsqb.rs`'s SOURCE with the `\`-continuations joined, so
    /// an edit to the measured query that is not mirrored here fails.
    #[test]
    fn queries_match_the_lane_table_verbatim() {
        // CRs stripped first so a CRLF checkout still joins its continuations.
        let src: String = include_str!("lsqb.rs").chars().filter(|&c| c != '\r').collect();
        // Join Rust string continuations: a backslash, a newline, leading blanks.
        let mut joined = String::with_capacity(src.len());
        let mut rest = src.as_str();
        while let Some(i) = rest.find("\\\n") {
            joined.push_str(&rest[..i]);
            rest = rest[i + 2..].trim_start_matches([' ', '\t']);
        }
        joined.push_str(rest);
        for (name, text) in QUERIES {
            assert!(
                joined.contains(&format!("name: \"{name}\"")),
                "{name}: not in the lane table"
            );
            assert!(joined.contains(text), "{name}: text differs from the lane table:\n{text}");
        }
    }

    #[test]
    fn expect_parses_both_shapes_and_refuses_junk() {
        let flat = parse_expect(r#"{"q4": 16312503, "q5": 12501170}"#).unwrap();
        assert_eq!(flat.get("q4"), Some(&16312503));
        let own = parse_expect(
            r#"{"queries":[{"query":"q4","count":7},{"query":"q3","count":null}]}"#,
        )
        .unwrap();
        assert_eq!(own.get("q4"), Some(&7));
        assert!(!own.contains_key("q3"));
        assert!(parse_expect(r#"{"qX": 1}"#).is_err());
        assert!(parse_expect(r#"{"q1": "ten"}"#).is_err());
        assert!(parse_expect(r#"{"queries":[{"query":"q2","count":null}]}"#).is_err());
        assert!(parse_expect("[]").is_err());
    }

    #[test]
    fn rendered_counts_are_the_flat_map_the_lane_accepts() {
        let (nodes, rels) = ten_nodes();
        let doc = render_counts(&all_counts(&corpus(&nodes, &rels))).unwrap();
        let back = parse_expect(&doc).expect("lsqb --expect must accept our output");
        assert_eq!(back.len(), 9);
        assert_eq!(back.get("q3"), Some(&12));
        assert_eq!(back.get("q9"), Some(&0));
    }
}
