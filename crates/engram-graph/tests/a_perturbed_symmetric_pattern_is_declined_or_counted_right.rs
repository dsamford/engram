//! Fix 90, the PERTURBATION arm: a symmetric pattern with exactly one thing
//! changed is the family the recogniser has to get right, and the family a
//! hand-written suite is worst at covering.
//!
//! `a_symmetric_pattern_is_counted_in_one_order` proves each mechanism on a
//! corpus built for it, and
//! `a_symmetry_broken_count_agrees_on_random_graphs` varies the DATA under
//! eight fixed patterns. Neither varies the PATTERN, and the pattern is what
//! the recogniser reads. Fix 90's danger is a set the planner calls
//! interchangeable that is not: it then divides the enumeration by |S|! and
//! multiplies the answer back, so the wrong answer is plausible rather than
//! loud.
//!
//! So this generates patterns instead: a symmetric base over 3 or 4 members
//! (ring or clique) and then EXACTLY ONE perturbation — one edge retyped,
//! one edge given a direction, one edge widened to a type alternation, one
//! member given an extra label, one attachment reversed or retyped — plus a
//! coin-flip on whether the ring is written as one path or as separate
//! MATCHes (the same pattern, two ASTs, and the path partition is what the
//! relationship-isomorphism argument is enforced over). Each runs against
//! five corpora, two of which carry a self-loop on a joining type.
//!
//! Every case is asked all three ways — symmetry ON, symmetry OFF, fold off —
//! and the answers must agree. What makes that evidence rather than
//! reassurance is the two guards below: the symmetry must FIRE on a healthy
//! share of the cases, and every DECLINE reason must be reached. A generator
//! that drifted into patterns the recogniser always refuses would otherwise
//! agree three ways forever while testing nothing.
#![allow(non_snake_case)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, pipeline, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}
fn node(g: &Graph, labels: &[&str], key: i64) -> u64 {
    let mut p = BTreeMap::new();
    p.insert("k".to_string(), Value::Int(key));
    let ls: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
    g.create_node(&ls, &p).expect("node")
}
fn rel(g: &Graph, a: u64, t: &str, b: u64) {
    g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
}
fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}
fn counted(g: &Graph, src: &str) -> (i64, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let (rows, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e}"))
    });
    let n = match rows.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected int, got {other:?} for {src}"),
    };
    (n, trace.counters().clone())
}
fn three_arms(g: &Graph, src: &str) -> (i64, BTreeMap<String, u64>, i64, i64) {
    pipeline::set_fold_symmetry_breaking(true);
    pipeline::set_count_fold(true);
    let (on, on_c) = counted(g, src);
    pipeline::set_fold_symmetry_breaking(false);
    let (off, _) = counted(g, src);
    pipeline::set_count_fold(false);
    let (general, _) = counted(g, src);
    pipeline::set_count_fold(true);
    pipeline::set_fold_symmetry_breaking(true);
    (on, on_c, off, general)
}

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
        (self.next() % (n as u64)) as usize
    }
}

/// A corpus rich in triangles, parallel edges and mixed types.
fn corpus(seed: u64, np: usize, edges: usize, loops: &[&str]) -> Graph {
    let g = graph();
    let mut r = Rng(seed | 1);
    let ids: Vec<u64> = (0..np)
        .map(|i| match i % 5 {
            0 => node(&g, &["P", "R"], i as i64),
            4 => node(&g, &["Q"], i as i64),
            _ => node(&g, &["P"], i as i64),
        })
        .collect();
    for _ in 0..edges {
        let a = r.below(np);
        let mut b = r.below(np);
        if b == a {
            b = (a + 1) % np;
        }
        let t = ["K", "K", "L", "M"][r.below(4)];
        rel(&g, ids[a], t, ids[b]);
    }
    for t in loops {
        rel(&g, ids[0], t, ids[0]);
    }
    g
}

const DIRS: [(&str, &str); 3] = [("-", "-"), ("-", "->"), ("<-", "-")];

/// Emit a random pattern from the symmetric family + single perturbations.
fn genpat(r: &mut Rng) -> String {
    let nmem = 3 + r.below(2); // 3 or 4 members
    // Per-member label, per-edge type/direction, attachment kind.
    let mut mlabel: Vec<String> = (0..nmem).map(|_| "P".to_string()).collect();
    // Ring or clique over the members.
    let clique = nmem == 3 || r.below(2) == 0;
    let mut edges: Vec<(usize, usize, String, usize)> = Vec::new();
    for i in 0..nmem {
        for j in i + 1..nmem {
            if clique || j == (i + 1) % nmem || (i == 0 && j == nmem - 1) {
                edges.push((i, j, "K".to_string(), 0));
            }
        }
    }
    // Attachments: none / per-member own / shared.
    let att = r.below(3);
    // PERTURB exactly one thing (or nothing, index == 0).
    let nperturb = 1 + edges.len() + nmem * 2;
    let which = r.below(nperturb + 1);
    if which >= 1 && which <= edges.len() {
        let e = which - 1;
        match r.below(3) {
            0 => edges[e].2 = "L".to_string(),
            1 => edges[e].3 = 1 + r.below(2), // a direction
            _ => edges[e].2 = "K|L".to_string(),
        }
    } else if which > edges.len() && which <= edges.len() + nmem {
        let m = which - edges.len() - 1;
        mlabel[m] = "P:R".to_string();
    }
    let att_perturb_member = if which > edges.len() + nmem && which <= nperturb {
        Some((which - edges.len() - nmem - 1) % nmem)
    } else {
        None
    };
    // Path grouping: each edge its own MATCH, or chain them into one path.
    let grouped = r.below(2) == 0;
    let mut clauses: Vec<String> = Vec::new();
    let vname = |i: usize| format!("v{i}");
    let mut declared = vec![false; nmem];
    let decl = |i: usize, declared: &mut Vec<bool>, mlabel: &Vec<String>| {
        if declared[i] {
            format!("({})", vname(i))
        } else {
            declared[i] = true;
            format!("({}:{})", vname(i), mlabel[i])
        }
    };
    if grouped {
        // One path walking the ring, plus the remaining edges as own MATCHes.
        let mut s = decl(0, &mut declared, &mlabel);
        for i in 0..nmem {
            let j = (i + 1) % nmem;
            let e = edges
                .iter()
                .position(|(a, b, _, _)| (*a == i && *b == j) || (*a == j && *b == i))
                .expect("ring edge");
            let (l, rr) = DIRS[edges[e].3];
            let flip = edges[e].0 != i;
            let (l, rr) = if flip { flip_dir(l, rr) } else { (l, rr) };
            s.push_str(&format!("{l}[:{}]{rr}", edges[e].2));
            s.push_str(&decl(j, &mut declared, &mlabel));
        }
        clauses.push(format!("MATCH {s}"));
        for (idx, (a, b, t, d)) in edges.iter().enumerate() {
            let ring = (*b == (*a + 1) % nmem) || (*a == (*b + 1) % nmem);
            if ring {
                continue;
            }
            let _ = idx;
            let (l, rr) = DIRS[*d];
            let sa = decl(*a, &mut declared, &mlabel);
            let sb = decl(*b, &mut declared, &mlabel);
            clauses.push(format!("MATCH {sa}{l}[:{t}]{rr}{sb}"));
        }
    } else {
        for (a, b, t, d) in edges.iter() {
            let (l, rr) = DIRS[*d];
            let sa = decl(*a, &mut declared, &mlabel);
            let sb = decl(*b, &mut declared, &mlabel);
            clauses.push(format!("MATCH {sa}{l}[:{t}]{rr}{sb}"));
        }
    }
    // Attachments.
    match att {
        1 => {
            for m in 0..nmem {
                let (t, dl, dr) = if att_perturb_member == Some(m) {
                    ("M", "<-", "-")
                } else {
                    ("M", "-", "->")
                };
                let sm = decl(m, &mut declared, &mlabel);
                clauses.push(format!("MATCH {sm}{dl}[:{t}]{dr}(a{m}:Q)"));
            }
        }
        2 => {
            for m in 0..nmem {
                let t = if att_perturb_member == Some(m) {
                    "L"
                } else {
                    "M"
                };
                let sm = decl(m, &mut declared, &mlabel);
                let sh = if m == 0 { "(sh:Q)" } else { "(sh)" };
                clauses.push(format!("MATCH {sm}-[:{t}]->{sh}"));
            }
        }
        _ => {}
    }
    format!("{} RETURN count(*) AS c", clauses.join(" "))
}

fn flip_dir(l: &str, rr: &str) -> (&'static str, &'static str) {
    match (l, rr) {
        ("-", "->") => ("<-", "-"),
        ("<-", "-") => ("-", "->"),
        _ => ("-", "-"),
    }
}

/// Patterns generated per corpus. 1,200 × 5 corpora × 3 executions is ~18,000
/// query runs in ~30 s. It was 4,000 while the fix was being written (20,000
/// checks, 100 s): that run is the one recorded in the ledger, and the guards
/// below are what make the smaller number safe to keep — shrink it further
/// and the decline-coverage assertion fails rather than the suite quietly
/// covering less.
const PATTERNS_PER_CORPUS: usize = 1_200;

#[test]
fn a_perturbed_symmetric_pattern_agrees_three_ways() {
    let corpora: Vec<(String, Graph)> = vec![
        ("c6".to_string(), corpus(101, 6, 22, &[])),
        ("c5_Lloop".to_string(), corpus(211, 5, 18, &["L"])),
        ("c4dense".to_string(), corpus(307, 4, 20, &[])),
        ("c8".to_string(), corpus(409, 8, 30, &[])),
        ("c5_Mloop".to_string(), corpus(503, 5, 18, &["M"])),
    ];
    let mut r = Rng(0x00C0_FFEE_1234_5678);
    let mut checked = 0usize;
    let mut fired = 0usize;
    let mut tally: BTreeMap<String, u64> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    for _ in 0..PATTERNS_PER_CORPUS {
        let src = genpat(&mut r);
        for (cn, g) in corpora.iter() {
            let (on, on_c, off, general) = three_arms(g, &src);
            checked += 1;
            for (kk, v) in on_c.iter() {
                if kk.contains("symmetry") {
                    *tally.entry(kk.clone()).or_default() += v;
                }
            }
            if count_of(&on_c, "interp.pipeline fold symmetry broken") >= 1 {
                fired += 1;
            }
            if on != off || on != general {
                let sym = count_of(&on_c, "interp.pipeline fold symmetry broken");
                failures.push(format!(
                    "{cn}: on={on} off={off} general={general} sym={sym}   {src}"
                ));
            }
        }
    }
    eprintln!("[fix 90] checked {checked} runs, symmetry fired on {fired}");
    for (k, v) in &tally {
        eprintln!("  {k}: {v}");
    }
    assert!(
        failures.is_empty(),
        "{} disagreement(s) between symmetry ON / OFF / fold off:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Guard 1: the mechanism under test must actually RUN, on a real share of
    // the cases. `fired > 0` would be satisfied by one lucky pattern in
    // thousands, and a generator that drifted toward shapes the recogniser
    // always refuses would then agree three ways while proving nothing about
    // the multiplier. The observed rate is ~21%.
    assert!(
        fired * 10 >= checked,
        "symmetry fired on only {fired} of {checked} cases (<10%) — the generator has drifted \
         away from the family this suite exists to cover, so the agreement above is mostly \
         between three executions that all declined"
    );

    // Guard 2: every DECLINE reason must be reached. The perturbations are
    // chosen to hit each one, and a decline path that stops being exercised
    // is a branch nothing checks — which is how a recogniser comes to accept
    // a set it should refuse. Self-loops are the rarest (two corpora carry
    // one), so it gets its own line rather than a shared floor.
    for reason in [
        "interp.pipeline fold symmetry declined: a transposition is not an automorphism",
        "interp.pipeline fold symmetry declined: two members are not adjacent",
        "interp.pipeline fold symmetry declined: a joining type carries self-loops",
    ] {
        assert!(
            tally.get(reason).copied().unwrap_or(0) > 0,
            "the decline `{reason}` was never reached in {checked} cases — either the \
             generator no longer produces that near miss, or the branch is unreachable"
        );
    }
    assert!(
        tally
            .get("interp.pipeline fold symmetry planned")
            .copied()
            .unwrap_or(0)
            > 0,
        "the recogniser never PLANNED a symmetry — the whole suite ran past the mechanism"
    );
}
