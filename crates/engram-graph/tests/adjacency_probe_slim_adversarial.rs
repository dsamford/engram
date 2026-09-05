//! Adversarial companion to `adjacency_probe_slim.rs`: DETERMINISTIC fixtures
//! whose expected counts are written in the test, not derived from an oracle,
//! plus the shapes the random sweep reaches only by luck — parallel
//! self-loops, both directions populated between one pair, a transaction's
//! buffered DELETE, a probe on a node the transaction did not touch, a node
//! created after the table was built, a row emptied by deletes — and, on every
//! typed call, the assertion that EXACTLY one of the two path counters fired
//! and that it was the expected one.

use std::collections::BTreeMap;

use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SEARCHED: &str = "graph.edge probe binary search";
const WALKED: &str = "graph.edge probe walked";

fn counter(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    g
}

fn node(g: &Graph) -> u64 {
    g.create_node(&["N".into()], &BTreeMap::new())
        .expect("node")
}

fn rel(g: &Graph, s: u64, ty: &str, d: u64) -> u64 {
    g.create_rel(s, ty, d, &BTreeMap::new()).expect("rel")
}

fn tok(g: &Graph, names: &[&str]) -> Option<Vec<u32>> {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let t = g.type_tokens_peek(&names);
    assert!(
        names.is_empty() || t.as_ref().is_some_and(|v| !v.is_empty()),
        "{names:?} must be minted before it is probed"
    );
    t
}

/// `rels_of` folded to the count of relationships whose other end is `to`.
fn oracle(g: &Graph, from: u64, dir: Dir, names: &[&str], to: u64) -> u64 {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let names: Option<&[String]> = if names.is_empty() { None } else { Some(&names) };
    g.rels_of(from, dir, names)
        .expect("rels_of")
        .iter()
        .filter(|r| match dir {
            Dir::Out => r.src == from && r.dst == to,
            Dir::In => r.dst == from && r.src == to,
            Dir::Both => (r.src == from && r.dst == to) || (r.dst == from && r.src == to),
        })
        .count() as u64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    Search,
    Walk,
}

/// One probe: the count, checked against `want` AND the `rels_of` oracle, and
/// the path, checked through the counters — exactly one fired, and it was
/// `path`.
fn probe(
    g: &Graph,
    from: u64,
    dir: Dir,
    names: &[&str],
    to: u64,
    want: u64,
    path: Path,
) -> engram_observe::Trace {
    let tokens = tok(g, names);
    let (got, trace) = engram_observe::with_trace(|| g.edge_count_slim(from, dir, &tokens, to));
    let s = counter(&trace, SEARCHED);
    let w = counter(&trace, WALKED);
    assert_eq!(
        s + w,
        1,
        "edge_count_slim({from}, {dir:?}, {names:?}, {to}) fired searched={s} walked={w}: not exactly one path counter"
    );
    let took = if s == 1 { Path::Search } else { Path::Walk };
    assert_eq!(
        took, path,
        "edge_count_slim({from}, {dir:?}, {names:?}, {to}) took {took:?}, expected {path:?}"
    );
    assert_eq!(
        got, want,
        "edge_count_slim({from}, {dir:?}, {names:?}, {to}) = {got}, the test expects {want}"
    );
    let o = oracle(g, from, dir, names, to);
    assert_eq!(
        got, o,
        "edge_count_slim({from}, {dir:?}, {names:?}, {to}) = {got}, rels_of says {o}"
    );
    trace
}

/// `counter` summed over the traces of several probes — `with_trace` installs
/// a FRESH trace and takes it, so it cannot nest; the probes hand theirs back.
fn total(traces: &[engram_observe::Trace], name: &str) -> u64 {
    traces.iter().map(|t| counter(t, name)).sum()
}

/// Three parallel A self-loops and one B self-loop on `a`; two A edges a->b,
/// one A edge b->a, one B edge a->b; a C self-loop on `c` so C is minted.
fn deterministic() -> (Graph, u64, u64, u64) {
    let g = graph();
    let (a, b, c) = (node(&g), node(&g), node(&g));
    rel(&g, a, "A", a);
    rel(&g, a, "A", a);
    rel(&g, a, "A", a);
    rel(&g, a, "B", a);
    rel(&g, a, "A", b);
    rel(&g, a, "A", b);
    rel(&g, b, "A", a);
    rel(&g, a, "B", b);
    rel(&g, c, "C", c);
    (g, a, b, c)
}

/// Every typed probe of the deterministic fixture, with the count each MUST
/// return. `Both` on a self-loop is the O side's count — once per
/// relationship, never once per side.
fn typed_expectations(
    a: u64,
    b: u64,
    c: u64,
) -> Vec<(u64, Dir, &'static [&'static str], u64, u64)> {
    vec![
        // Parallel self-loops: 3 A rows on the O side, 3 on the I side, 3 in Both.
        (a, Dir::Out, &["A"], a, 3),
        (a, Dir::In, &["A"], a, 3),
        (a, Dir::Both, &["A"], a, 3),
        // Both directions populated between one pair: Both = O + I.
        (a, Dir::Out, &["A"], b, 2),
        (a, Dir::In, &["A"], b, 1),
        (a, Dir::Both, &["A"], b, 3),
        (b, Dir::Out, &["A"], a, 1),
        (b, Dir::In, &["A"], a, 2),
        (b, Dir::Both, &["A"], a, 3),
        // A node with no self-loop probed against itself.
        (b, Dir::Out, &["A"], b, 0),
        (b, Dir::Both, &["A"], b, 0),
        // The other type does not leak into A's count, and vice versa.
        (a, Dir::Out, &["B"], a, 1),
        (a, Dir::In, &["B"], a, 1),
        (a, Dir::Both, &["B"], a, 1),
        (a, Dir::Out, &["B"], b, 1),
        (a, Dir::In, &["B"], b, 0),
        (a, Dir::Both, &["B"], b, 1),
        (b, Dir::Both, &["B"], a, 1),
        // A type with rows elsewhere but none here.
        (a, Dir::Out, &["C"], a, 0),
        (a, Dir::Both, &["C"], b, 0),
        (c, Dir::Both, &["C"], c, 1),
        (c, Dir::In, &["C"], c, 1),
        // A far end that is not an endpoint of anything.
        (a, Dir::Both, &["A"], c, 0),
        (c, Dir::Both, &["A"], a, 0),
    ]
}

#[test]
fn deterministic_multiplicities_search_and_match_the_written_counts() {
    let (g, a, b, c) = deterministic();
    for (from, dir, names, to, want) in typed_expectations(a, b, c) {
        probe(&g, from, dir, names, to, want, Path::Search);
    }
    // Untyped and multi-type: the same counts summed across types, walked.
    probe(&g, a, Dir::Both, &[], a, 4, Path::Walk);
    probe(&g, a, Dir::Out, &[], a, 4, Path::Walk);
    probe(&g, a, Dir::In, &[], a, 4, Path::Walk);
    probe(&g, a, Dir::Both, &[], b, 4, Path::Walk);
    probe(&g, a, Dir::Out, &[], b, 3, Path::Walk);
    probe(&g, a, Dir::In, &[], b, 1, Path::Walk);
    probe(&g, a, Dir::Both, &["A", "B"], a, 4, Path::Walk);
    probe(&g, a, Dir::Both, &["A", "B"], b, 4, Path::Walk);
    probe(&g, a, Dir::Both, &["A", "C"], b, 3, Path::Walk);
    probe(&g, b, Dir::In, &["A", "B"], a, 3, Path::Walk);
}

#[test]
fn the_canary_walks_the_same_written_counts() {
    let (g, a, b, c) = deterministic();
    let exp = typed_expectations(a, b, c);
    for &(from, dir, names, to, want) in &exp {
        probe(&g, from, dir, names, to, want, Path::Search);
    }
    let flipped = g.clear_adjacency_sorted_flags();
    assert!(
        flipped >= 6,
        "three types in two directions were probed, {flipped} table(s) flipped"
    );
    for &(from, dir, names, to, want) in &exp {
        probe(&g, from, dir, names, to, want, Path::Walk);
    }
}

/// A buffered DELETE is a pending row with no value; the probe must treat it
/// as an overlay and walk, and the walk must not count the deleted edge. After
/// commit the tables are REPAIRED (not rebuilt) and the search answers the new
/// count.
#[test]
fn a_transactions_buffered_delete_forces_the_walk_and_the_commit_repairs() {
    let g = graph();
    let (a, b, c) = (node(&g), node(&g), node(&g));
    let r1 = rel(&g, a, "A", b);
    let _r2 = rel(&g, a, "A", b);
    rel(&g, c, "B", c);
    probe(&g, a, Dir::Out, &["A"], b, 2, Path::Search);
    probe(&g, b, Dir::In, &["A"], a, 2, Path::Search);
    probe(&g, a, Dir::Both, &["A"], b, 2, Path::Search);
    probe(&g, b, Dir::Both, &["A"], a, 2, Path::Search);

    g.begin_txn().expect("begin");
    g.delete_rel(r1).expect("delete");
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Walk);
    probe(&g, b, Dir::In, &["A"], a, 1, Path::Walk);
    probe(&g, a, Dir::Both, &["A"], b, 1, Path::Walk);
    probe(&g, b, Dir::Both, &["A"], a, 1, Path::Walk);
    // A node the transaction did not touch is still served by the table.
    probe(&g, c, Dir::Both, &["B"], c, 1, Path::Search);
    g.commit_txn().expect("commit");

    let traces = [
        probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search),
        probe(&g, b, Dir::In, &["A"], a, 1, Path::Search),
        probe(&g, a, Dir::Both, &["A"], b, 1, Path::Search),
    ];
    assert!(
        total(&traces, "graph.adjacency tables repaired") >= 1,
        "the commit did not repair the tables"
    );
    assert_eq!(
        total(&traces, "graph.adjacency tables built"),
        0,
        "the commit REBUILT a table rather than repairing it"
    );
}

/// Inside a transaction that buffered rows for OTHER nodes, a probe on an
/// untouched node searches the shared table; a probe on a touched node walks
/// and sees the buffered rows; rollback restores the committed counts.
#[test]
fn inside_a_transaction_only_the_touched_nodes_walk() {
    let g = graph();
    let (a, b, c, d) = (node(&g), node(&g), node(&g), node(&g));
    rel(&g, a, "A", b);
    rel(&g, c, "A", d);
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search);
    probe(&g, c, Dir::Out, &["A"], d, 1, Path::Search);

    g.begin_txn().expect("begin");
    rel(&g, c, "A", d);
    rel(&g, d, "A", d); // a buffered self-loop
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search);
    probe(&g, b, Dir::In, &["A"], a, 1, Path::Search);
    probe(&g, a, Dir::Both, &["A"], b, 1, Path::Search);
    probe(&g, c, Dir::Out, &["A"], d, 2, Path::Walk);
    probe(&g, d, Dir::In, &["A"], c, 2, Path::Walk);
    probe(&g, c, Dir::Both, &["A"], d, 2, Path::Walk);
    probe(&g, d, Dir::Both, &["A"], d, 1, Path::Walk);
    probe(&g, d, Dir::Out, &["A"], d, 1, Path::Walk);
    // `Both` on a node whose O side is clean but whose I side is buffered:
    // the O side's search must be discarded and the whole call walked.
    probe(&g, d, Dir::Both, &["A"], c, 2, Path::Walk);
    g.rollback_txn();

    probe(&g, c, Dir::Out, &["A"], d, 1, Path::Search);
    probe(&g, d, Dir::Both, &["A"], d, 0, Path::Search);
    probe(&g, d, Dir::Both, &["A"], c, 1, Path::Search);
}

/// Rows a repair must handle: a node CREATED after the table was built (past
/// the base's offsets), a row that went from empty to populated, a row that
/// went from populated to empty, and a row that gained parallel edges in the
/// middle of its peer order.
#[test]
fn a_repair_serves_new_nodes_emptied_rows_and_inserted_parallels_by_search() {
    let g = graph();
    let (a, b, c, d) = (node(&g), node(&g), node(&g), node(&g));
    let ab = rel(&g, a, "A", b);
    rel(&g, a, "A", d);
    rel(&g, c, "A", d);
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search);
    probe(&g, a, Dir::Out, &["A"], d, 1, Path::Search);
    probe(&g, d, Dir::In, &["A"], a, 1, Path::Search);
    probe(&g, c, Dir::Both, &["A"], d, 1, Path::Search);

    // A new node past the table's base, with edges to and from it.
    let e = node(&g);
    rel(&g, e, "A", a);
    rel(&g, e, "A", a);
    rel(&g, a, "A", e);
    // `a`'s O row gains parallels to `c` (between b and d in peer order).
    rel(&g, a, "A", c);
    rel(&g, a, "A", c);
    // `c`'s O row is emptied; `a`'s row loses its first entry.
    for r in g.rels_of(c, Dir::Out, None).expect("rels") {
        g.delete_rel(r.id).expect("delete");
    }
    g.delete_rel(ab).expect("delete");

    let traces = [
        probe(&g, e, Dir::Out, &["A"], a, 2, Path::Search),
        probe(&g, e, Dir::In, &["A"], a, 1, Path::Search),
        probe(&g, e, Dir::Both, &["A"], a, 3, Path::Search),
        probe(&g, a, Dir::Both, &["A"], e, 3, Path::Search),
        probe(&g, a, Dir::Out, &["A"], c, 2, Path::Search),
        probe(&g, a, Dir::Out, &["A"], b, 0, Path::Search),
        probe(&g, a, Dir::Out, &["A"], d, 1, Path::Search),
        probe(&g, c, Dir::Out, &["A"], d, 0, Path::Search),
        probe(&g, c, Dir::Both, &["A"], d, 0, Path::Search),
        probe(&g, c, Dir::In, &["A"], a, 2, Path::Search),
        probe(&g, d, Dir::In, &["A"], c, 0, Path::Search),
        probe(&g, b, Dir::In, &["A"], a, 0, Path::Search),
    ];
    assert!(
        total(&traces, "graph.adjacency tables repaired") >= 1,
        "no repair happened"
    );
    assert_eq!(
        total(&traces, "graph.adjacency tables built"),
        0,
        "a table was rebuilt rather than repaired"
    );
}

/// With incremental caches OFF every write invalidates every table; the probe
/// must keep answering (by rebuild-then-search) and never go stale.
#[test]
fn without_incremental_caches_the_probe_rebuilds_and_still_agrees() {
    let g = graph();
    g.set_incremental_caches(false);
    let (a, b) = (node(&g), node(&g));
    rel(&g, a, "A", b);
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search);
    rel(&g, a, "A", b);
    let traces = [
        probe(&g, a, Dir::Out, &["A"], b, 2, Path::Search),
        probe(&g, b, Dir::In, &["A"], a, 2, Path::Search),
    ];
    assert!(
        total(&traces, "graph.adjacency tables built") >= 1,
        "the tables were neither rebuilt nor repaired after the write"
    );
    g.delete_rel(g.rels_of(a, Dir::Out, None).expect("rels")[0].id)
        .expect("delete");
    probe(&g, a, Dir::Out, &["A"], b, 1, Path::Search);
    probe(&g, a, Dir::Both, &["A"], b, 1, Path::Search);
}

/// A zero entry budget: no table may be used, so every typed probe walks.
#[test]
fn a_zero_entry_budget_walks_every_probe() {
    let (g, a, b, c) = deterministic();
    g.set_adj_table_max_entries(0);
    for (from, dir, names, to, want) in typed_expectations(a, b, c) {
        probe(&g, from, dir, names, to, want, Path::Walk);
    }
}

/// A never-minted type: zero, no counter, no table built.
#[test]
fn a_never_minted_type_is_zero_without_a_path() {
    let (g, a, b, _) = deterministic();
    let never = g.type_tokens_peek(&["NEVER".into()]);
    assert_eq!(never, Some(vec![]));
    let (got, trace) = engram_observe::with_trace(|| g.edge_count_slim(a, Dir::Both, &never, b));
    assert_eq!(got, 0);
    assert_eq!(counter(&trace, SEARCHED) + counter(&trace, WALKED), 0);
    assert_eq!(counter(&trace, "graph.adjacency tables built"), 0);
    // And the untyped table was not poisoned by that call.
    probe(&g, a, Dir::Both, &[], b, 4, Path::Walk);
}
