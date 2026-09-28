//! A WRITING statement's MATCH is seeded by its `WHERE id(n) = <expr>`.
//!
//! The streaming planner has always turned an identity equality into a
//! one-get seed. Writing statements take the per-row matcher instead, which
//! enumerated the start's whole label for every input row and filtered after.
//! Measured at SNB SF3: `UNWIND <2,000 pairs> AS t MATCH (a:Person) WHERE
//! id(a) = t[0] MATCH (a)-[k:KNOWS]-(b:Person) WHERE id(b) = t[1] SET …`
//! materialised 48.9M Person records and took 577 s; without the SET, 2.3 s.
//! Every weight precompute for SNB BI 15/19/20 hit it.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .unwrap_or_else(|e| panic!("`{src}`: {e}"))
        .rows
}

fn graph(n: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:P {{k: i}})", n - 1),
    );
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:P {{k: i}}), (b:P {{k: i + 1}}) CREATE (a)-[:R]->(b)",
            n - 2
        ),
    );
    g
}

fn ids(g: &Graph) -> Vec<(i64, i64)> {
    run(
        g,
        "MATCH (a:P)-[:R]->(b:P) RETURN id(a), id(b) ORDER BY id(a)",
    )
    .into_iter()
    .map(|r| match (&r[0], &r[1]) {
        (Value::Int(a), Value::Int(b)) => (*a, *b),
        other => panic!("{other:?}"),
    })
    .collect()
}

fn lit(pairs: &[(i64, i64)]) -> String {
    let body: Vec<String> = pairs.iter().map(|(a, b)| format!("[{a},{b}]")).collect();
    format!("[{}]", body.join(","))
}

fn materialised(t: &engram_observe::Trace) -> u64 {
    t.counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0)
}

fn seeded(t: &engram_observe::Trace) -> u64 {
    t.counters()
        .get("interp.matcher seeded its start from an id equality")
        .copied()
        .unwrap_or(0)
}

const SET: &str = "UNWIND __L__ AS t MATCH (a:P) WHERE id(a) = t[0] \
                   MATCH (a)-[r:R]-(b:P) WHERE id(b) = t[1] SET r.w = t[0] + t[1] RETURN count(r)";

#[test]
fn a_writing_match_looks_its_start_up_instead_of_scanning() {
    let g = graph(400);
    let pairs: Vec<(i64, i64)> = ids(&g).into_iter().take(50).collect();
    let (rows, t) = engram_observe::with_trace(|| run(&g, &SET.replace("__L__", &lit(&pairs))));
    assert_eq!(rows, vec![vec![Value::Int(50)]]);
    assert!(seeded(&t) >= 50, "the seed engaged: {:?}", t.counters());
    // 50 rows over a 400-node label: a scan per row is 20,000 records; the
    // seek is a handful per row
    assert!(
        materialised(&t) < 2_000,
        "the start was looked up, not scanned: {} records",
        materialised(&t)
    );
    // the hop's far end is pinned too, so its walk decodes only the edge
    // that reaches it -- through the full walk, or (an undirected hop whose
    // relationship binds lean) the adjacency cut to the bound peer
    let pinned = [
        "interp.full walk read only the edges to a bound peer",
        "interp.expansion read only the edges to a known peer",
    ]
    .iter()
    .map(|k| t.counters().get(*k).copied().unwrap_or(0))
    .sum::<u64>();
    assert!(pinned >= 50, "the pinned hop engaged: {:?}", t.counters());
    let rels = t
        .counters()
        .get("graph.rels materialised in full")
        .copied()
        .unwrap_or(0);
    assert!(
        rels <= 2 * 50,
        "one or two edges a pair, not the whole adjacency: {rels}"
    );
}

#[test]
fn a_pinned_hop_between_hubs_decodes_only_the_edges_between_them() {
    // Two hubs, each with many other neighbours, joined by two parallel
    // edges of the type and one of another type: exactly the two typed
    // edges, both directions of an undirected hop, and nothing else.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run(&g, "CREATE (:H {k: 0}), (:H {k: 1})");
    run(
        &g,
        "UNWIND range(0, 99) AS i MATCH (h:H) CREATE (h)-[:R]->(:Leaf {i: i})",
    );
    run(
        &g,
        "MATCH (a:H {k: 0}), (b:H {k: 1}) CREATE (a)-[:R {n: 1}]->(b), (b)-[:R {n: 2}]->(a), (a)-[:S]->(b)",
    );
    let ids: Vec<i64> = run(&g, "MATCH (h:H) RETURN id(h) ORDER BY h.k")
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            ref other => panic!("{other:?}"),
        })
        .collect();
    let q = format!(
        "MATCH (a:H) WHERE id(a) = {} MATCH (a)-[r:R]-(b) WHERE id(b) = {} \
         SET r.seen = true RETURN r.n ORDER BY r.n",
        ids[0], ids[1]
    );
    let (rows, t) = engram_observe::with_trace(|| run(&g, &q));
    assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    let rels = t
        .counters()
        .get("graph.rels materialised in full")
        .copied()
        .unwrap_or(0);
    assert!(rels <= 4, "the 200 leaf edges were not decoded: {rels}");
    assert_eq!(
        run(&g, "MATCH ()-[r]->() WHERE r.seen = true RETURN count(r)"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn the_seeded_write_writes_exactly_what_the_scan_wrote() {
    let pairs_of = |g: &Graph| -> Vec<(i64, i64)> { ids(g).into_iter().step_by(3).collect() };
    let a = graph(120);
    let b = graph(120);
    let pa = pairs_of(&a);
    run(&a, &SET.replace("__L__", &lit(&pa)));
    // the control: the same write through a WHERE the seek cannot use
    // (`+ 0` keeps the equality but reads nothing new — still seekable), so
    // spell it as a disjunction the extractor declines
    let unseekable = "UNWIND __L__ AS t MATCH (a:P) WHERE (id(a) = t[0] OR false) \
                      MATCH (a)-[r:R]-(b:P) WHERE (id(b) = t[1] OR false) \
                      SET r.w = t[0] + t[1] RETURN count(r)";
    let (_, t) = engram_observe::with_trace(|| run(&b, &unseekable.replace("__L__", &lit(&pa))));
    assert_eq!(seeded(&t), 0, "the control must take the scan");
    let read = "MATCH (a:P)-[r:R]->(b:P) RETURN id(a), r.w ORDER BY id(a)";
    assert_eq!(run(&a, read), run(&b, read));
}

#[test]
fn a_missing_or_non_id_value_matches_nothing_and_does_not_error() {
    let g = graph(10);
    let rows = run(
        &g,
        "UNWIND [999999, -1, null, 'x', 1.5] AS x MATCH (a:P) WHERE id(a) = x SET a.hit = true RETURN count(a)",
    );
    assert_eq!(rows, vec![vec![Value::Int(0)]]);
    assert_eq!(
        run(&g, "MATCH (a:P) WHERE a.hit = true RETURN count(a)"),
        vec![vec![Value::Int(0)]]
    );
}

#[test]
fn the_seek_keeps_the_patterns_own_tests() {
    // A node that exists but does not carry the pattern's label, or fails its
    // map, must not be matched just because its id was named.
    let g = graph(10);
    run(&g, "CREATE (:Q {k: 1})");
    let q = match &run(&g, "MATCH (q:Q) RETURN id(q)")[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    };
    let p0 = ids(&g)[0].0;
    assert_eq!(
        run(
            &g,
            &format!("MATCH (a:P) WHERE id(a) = {q} SET a.x = 1 RETURN count(a)")
        ),
        vec![vec![Value::Int(0)]]
    );
    assert_eq!(
        run(
            &g,
            &format!("MATCH (a:P {{k: 77}}) WHERE id(a) = {p0} SET a.x = 1 RETURN count(a)")
        ),
        vec![vec![Value::Int(0)]]
    );
}

#[test]
fn an_equality_over_a_later_variable_is_left_to_the_scan() {
    // `id(a) = id(b)` where `b` is bound by a LATER path: the row cannot
    // evaluate it yet, so the seek must decline and the answer stay right.
    let g = graph(10);
    let rows = run(
        &g,
        "MATCH (a:P), (b:P) WHERE id(a) = id(b) SET a.self = true RETURN count(*)",
    );
    assert_eq!(rows, vec![vec![Value::Int(10)]]);
}

#[test]
fn optional_match_keeps_its_null_row() {
    let g = graph(10);
    let rows = run(
        &g,
        "UNWIND [999999] AS x OPTIONAL MATCH (a:P) WHERE id(a) = x SET a.y = 1 \
         RETURN x, a IS NULL AS missing",
    );
    assert_eq!(rows, vec![vec![Value::Int(999999), Value::Bool(true)]]);
}

#[test]
fn randomised_seeded_writes_agree_with_the_scan() {
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for round in 0..12 {
        let n = 20 + (next() % 60) as usize;
        let a = graph(n);
        let b = graph(n);
        let all = ids(&a);
        let mut pairs = Vec::new();
        for _ in 0..(next() % 40) {
            let (x, y) = all[(next() as usize) % all.len()];
            // sometimes reversed, sometimes a non-edge, sometimes a bad id
            match next() % 4 {
                0 => pairs.push((y, x)),
                1 => pairs.push((x, x)),
                2 => pairs.push((x + 100_000, y)),
                _ => pairs.push((x, y)),
            }
        }
        if pairs.is_empty() {
            continue;
        }
        let r1 = run(&a, &SET.replace("__L__", &lit(&pairs)));
        let unseekable = SET
            .replace("WHERE id(a) = t[0]", "WHERE (id(a) = t[0] OR false)")
            .replace("WHERE id(b) = t[1]", "WHERE (id(b) = t[1] OR false)");
        let r2 = run(&b, &unseekable.replace("__L__", &lit(&pairs)));
        assert_eq!(r1, r2, "round {round}: counts");
        let read = "MATCH (a:P)-[r:R]->(b:P) RETURN id(a), r.w ORDER BY id(a)";
        assert_eq!(
            run(&a, read),
            run(&b, read),
            "round {round}: written values"
        );
    }
}
