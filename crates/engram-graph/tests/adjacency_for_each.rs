#![allow(non_snake_case)]
//! The zero-copy adjacency accessors MUST visit exactly what the owned-`Vec`
//! `adjacent_slim` produces. This is the byte-identity contract the traversal
//! hot path (`expand` / `semijoin` / the frontier BFS / the vectorised
//! collectors) leans on after migrating off the per-node `Vec`:
//!
//!   * `adjacent_slim_for_each` visits the FORWARD sequence, element for
//!     element, and
//!   * `adjacent_slim_rev_for_each` visits its exact REVERSE
//!     (`adjacent_slim(..).iter().rev()`),
//!
//! for `Out` / `In` / `Both`, with and without a type filter, over BOTH the
//! cached-table path (`set_degree_table_after(0)`) and the pre-admission
//! prefix-walk fallback (`set_degree_table_after(u64::MAX)`) — and the two
//! paths must agree with each other. The graph carries a self-loop so the
//! `Both`-side dedup (the O side offers a self-loop, the I side must not
//! repeat it) is exercised in every arm.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph, SlimAdj};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A star around `s` with an in/out mix, two types, and a self-loop.
///   OUT of s: s-[T1]->b, s-[T2]->c, s-[T1]->s (self-loop), s-[T1]->d
///   IN  to s: a-[T1]->s, e-[T2]->s
/// The multiple out-edges of two types plus both an in and an out edge make the
/// type filter and the `Both` ordering observable; the self-loop makes the
/// `Both` I-side dedup observable.
fn star() -> (Graph, u64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = || {
        g.create_node(&["N".into()], &BTreeMap::new())
            .expect("node")
    };
    let s = mk();
    let a = mk();
    let b = mk();
    let c = mk();
    let d = mk();
    let e = mk();
    let none = BTreeMap::new();
    g.create_rel(s, "T1", b, &none).expect("rel");
    g.create_rel(s, "T2", c, &none).expect("rel");
    g.create_rel(s, "T1", s, &none).expect("self-loop");
    g.create_rel(s, "T1", d, &none).expect("rel");
    g.create_rel(a, "T1", s, &none).expect("rel");
    g.create_rel(e, "T2", s, &none).expect("rel");
    (g, s)
}

fn collect_fwd(g: &Graph, s: u64, dir: Dir, tokens: &Option<Vec<u32>>) -> Vec<SlimAdj> {
    let mut v = Vec::new();
    g.adjacent_slim_for_each(s, dir, tokens, |e| v.push(*e));
    v
}

fn collect_rev(g: &Graph, s: u64, dir: Dir, tokens: &Option<Vec<u32>>) -> Vec<SlimAdj> {
    let mut v = Vec::new();
    g.adjacent_slim_rev_for_each(s, dir, tokens, |e| v.push(*e));
    v
}

/// For one (dir, tokens) pair: the forward accessor equals the owned `Vec`, the
/// reverse accessor equals its reverse, and the self-loop appears exactly once
/// under `Both`. Returns the owned `Vec` so the caller can compare paths.
fn check_arm(g: &Graph, s: u64, dir: Dir, tokens: &Option<Vec<u32>>, label: &str) -> Vec<SlimAdj> {
    let owned = g.adjacent_slim(s, dir, tokens);

    let fwd = collect_fwd(g, s, dir, tokens);
    assert_eq!(
        fwd, owned,
        "forward for_each must equal adjacent_slim [{label}]"
    );

    let rev = collect_rev(g, s, dir, tokens);
    let mut want_rev = owned.clone();
    want_rev.reverse();
    assert_eq!(
        rev, want_rev,
        "rev for_each must equal adjacent_slim(..).rev() [{label}]"
    );

    // The self-loop (peer == s) is offered exactly once regardless of direction:
    // once from whichever side carries it, and — critically — NOT twice under
    // `Both` where both the O and I sides list it.
    let loops = owned.iter().filter(|e| e.peer == s).count();
    assert!(
        loops <= 1,
        "self-loop must not be repeated [{label}], saw {loops}"
    );
    owned
}

/// Every (dir, tokens) arm, in a single admission mode. Returns the per-arm
/// owned `Vec`s so the two modes can be compared against each other.
fn all_arms(
    g: &Graph,
    s: u64,
    t1: &Option<Vec<u32>>,
    t1t2: &Option<Vec<u32>>,
    mode: &str,
) -> Vec<Vec<SlimAdj>> {
    let mut out = Vec::new();
    for (dir, dn) in [(Dir::Out, "Out"), (Dir::In, "In"), (Dir::Both, "Both")] {
        for (tok, tn) in [(&None, "any"), (t1, "T1"), (t1t2, "T1+T2")] {
            out.push(check_arm(g, s, dir, tok, &format!("{mode}/{dn}/{tn}")));
        }
    }
    out
}

#[test]
fn for_each_matches_adjacent_slim_in_both_admission_modes() {
    let (g, s) = star();
    let t1 = g.type_tokens_peek(&["T1".to_string()]);
    let t1t2 = g.type_tokens_peek(&["T1".to_string(), "T2".to_string()]);
    // Sanity: both named types were minted by the loader (else the filter arms
    // would silently test 'a type that has no rows'.
    assert!(matches!(&t1, Some(v) if v.len() == 1), "T1 minted");
    assert!(matches!(&t1t2, Some(v) if v.len() == 2), "T1+T2 minted");

    // Pre-admission: every call takes the bounded prefix walk (fallback).
    g.set_degree_table_after(u64::MAX);
    let fallback = all_arms(&g, s, &t1, &t1t2, "fallback");

    // Admitted: every call is served from the cached CSR table (zero-copy).
    g.set_degree_table_after(0);
    let table = all_arms(&g, s, &t1, &t1t2, "table");

    // The two adjacency paths must themselves agree, arm for arm.
    assert_eq!(
        fallback, table,
        "the prefix-walk fallback and the cached-table path must produce identical adjacency"
    );

    // The 'any' Both arm must contain the self-loop once and cover every
    // incident row: 4 out rows (b, c, the s->s loop, d) from the O side + 2 in
    // rows (a, e) from the I side, whose s->s repeat is deduped away = 6.
    let both_any = &table[6]; // index 6 = (Both, any) in all_arms order
    assert_eq!(both_any.len(), 6, "Both/any covers every incident row once");
    assert_eq!(
        both_any.iter().filter(|e| e.peer == s).count(),
        1,
        "the self-loop is present exactly once under Both"
    );
}
