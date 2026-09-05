#![allow(non_snake_case)]
//! The GENERAL path's seed probes the DECLARED, label-scoped index on the MOST
//! SELECTIVE key a start offers — not the first entry of its pattern map, nor
//! the first equality of its WHERE.
//!
//! The production shape (2026-09-04), the email listing:
//!
//! ```text
//! MATCH (n:UserDataNode {nodeType: 'email', userId: $userId})
//! WHERE n.classified = true AND (n.abuseStatus IS NULL OR …)
//! WITH n ORDER BY n.createdAt DESC SKIP … LIMIT …
//! OPTIONAL MATCH (n)-[:HAS_ASK]->(a:EmailAsk) …
//! ```
//!
//! Every columnar operator declines it (a bare `n` leaves the stage), so the
//! general path seeds it — and the general path had three seed sites that each
//! probed ONE key: the map's first entry (`nodeType` → 38k of the label's 38.5k
//! ids, which "beat" the label), or the WHERE's first equality (`classified`,
//! a boolean no index orders). The 10-row `userId` key, declared by the
//! operator, was never asked. Measured on the mirror: 1,025 ms and 2.7 GB of
//! resident growth to page 9 rows; Neo4j 2.8 ms. The OPTIONAL MATCH, the
//! `EXISTS {}` and the pattern-comprehension forms of the same start paid the
//! same way, because the seed is chosen before any of them run.
//!
//! The contract: with the selective key declared, each of those shapes seeks
//! it (the counter fires) and answers exactly what the seek-less path answers;
//! with the composite alone declared its trailing key counts as declared too
//! (fix 47) and is chosen the same way; with nothing declared the rows are
//! the same and the counter stays at zero (the control).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("owner".to_string(), Value::Str("u8".to_string()));
    p.insert("skip".to_string(), Value::Int(0));
    p.insert("page".to_string(), Value::Int(10));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// `src` with the property seek ON, then OFF (the seek-less oracle).
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_property_seek(true);
    let on = rows(g, src);
    g.set_property_seek(false);
    let off = rows(g, src);
    g.set_property_seek(true);
    (on, off)
}

fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_property_seek(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

const CHOSE_LATER: &str = "interp.seed chose a later, more selective declared key";
/// The columnar aggregate's own seek counter: a shape the columnar path
/// claims (the `EXISTS { MATCH … }` count, once that body lifts to a probe)
/// seeks the same declared key there instead of on the general path.
const COLUMNAR_SCOPED: &str = "interp.columnar seek chose a declared scoped index";

#[derive(Clone, Copy)]
enum Declared {
    /// The production catalogue: the composite `(kind, owner)` AND the
    /// single-property `owner` index.
    CompositeAndOwner,
    /// Only the composite — `owner` is a TRAILING key, declared through it.
    CompositeOnly,
    /// Nothing declared.
    None,
}

/// 700 `:Doc` nodes, above the seek floor and dense in id space. `kind` is
/// UNSELECTIVE ('email' on 600) and always the map's FIRST entry, mirroring
/// `nodeType`; `owner` is SELECTIVE (2 per value), mirroring `userId`; `flag`
/// is a boolean the WHERE leads with, mirroring `classified`. Even-numbered
/// docs below 40 carry one or two `:Ask` children for the hop shapes.
fn corpus(declared: Declared) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    match declared {
        Declared::CompositeAndOwner => {
            ddl(&g, "CREATE INDEX doc_kind_owner IF NOT EXISTS FOR (n:Doc) ON (n.kind, n.owner)");
            ddl(&g, "CREATE INDEX doc_owner IF NOT EXISTS FOR (n:Doc) ON (n.owner)");
        }
        Declared::CompositeOnly => {
            ddl(&g, "CREATE INDEX doc_kind_owner IF NOT EXISTS FOR (n:Doc) ON (n.kind, n.owner)");
        }
        Declared::None => {}
    }
    let mut docs: Vec<u64> = Vec::with_capacity(700);
    for i in 0..700i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("owner".to_string(), Value::Str(format!("u{}", i % 350)));
        m.insert("n".to_string(), Value::Int(i));
        m.insert("flag".to_string(), Value::Bool(i % 2 == 0));
        if i % 3 == 0 {
            m.insert("status".to_string(), Value::Str("clean".to_string()));
        }
        docs.push(g.create_node(&["Doc".into()], &m).expect("node"));
    }
    for i in (0..40u64).step_by(2) {
        for k in 0..=(i % 2) {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int((i * 10 + k) as i64));
            m.insert("resolved".to_string(), Value::Bool(k == 1));
            let a = g.create_node(&["Ask".into()], &m).expect("ask");
            g.create_rel(docs[i as usize], "HAS", a, &BTreeMap::new())
                .expect("rel");
        }
    }
    g
}

/// The production shapes over this start, each one the general path's.
const SHAPES: &[&str] = &[
    // the email listing: a bare `d` crosses an ordered, paged WITH
    "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE d.flag = true AND (d.status IS NULL OR d.status IN ['clean', 'approved']) WITH d ORDER BY d.n DESC SKIP toInteger($skip) LIMIT toInteger($page) RETURN d.n AS n",
    // … then an OPTIONAL MATCH and a CASE aggregate over the page
    "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE d.flag = true WITH d ORDER BY d.n DESC SKIP toInteger($skip) LIMIT toInteger($page) OPTIONAL MATCH (d)-[:HAS]->(a:Ask) WITH d, count(CASE WHEN a IS NOT NULL AND coalesce(a.resolved, false) = false THEN a END) AS open RETURN d.n AS n, open",
    // an OPTIONAL MATCH straight after the map
    "MATCH (d:Doc {kind: 'email', owner: $owner}) OPTIONAL MATCH (d)-[:HAS]->(a:Ask) RETURN d.n AS n, count(a) AS c ORDER BY n",
    // an EXISTS subquery on the map's rows
    "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE EXISTS { MATCH (d)-[:HAS]->(:Ask) } RETURN count(d) AS c",
    // a pattern comprehension in the projection
    "MATCH (d:Doc {kind: 'email', owner: $owner}) RETURN d.n AS n, [(d)-[:HAS]->(a:Ask) WHERE coalesce(a.resolved, false) = false | a.k] AS ks ORDER BY n",
    // the WHERE form: the boolean leads, the selective equality follows
    "MATCH (d:Doc) WHERE d.flag = true AND d.kind = 'email' AND d.owner = $owner WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
];

/// With the selective key declared, every shape seeks it — whichever entry
/// or conjunct comes first — and agrees with the seek-less path row for row.
#[test]
fn every_shape_on_the_start_seeks_the_declared_selective_key() {
    let g = corpus(Declared::CompositeAndOwner);
    for src in SHAPES {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs scan disagree on `{src}`");
        assert!(
            counter(&g, src, CHOSE_LATER) > 0 || counter(&g, src, COLUMNAR_SCOPED) > 0,
            "`{src}` must choose the later, declared `owner` key over the first `kind` entry"
        );
    }
    // Fixture sanity: u8 owns docs 8 and 358, both 'email', both even (flag),
    // 8 carries one Ask (k 80, unresolved), 358 none.
    assert_eq!(
        rows(&g, SHAPES[0]),
        vec![vec![Value::Int(358)], vec![Value::Int(8)]]
    );
    assert_eq!(
        rows(&g, SHAPES[2]),
        vec![
            vec![Value::Int(8), Value::Int(1)],
            vec![Value::Int(358), Value::Int(0)]
        ]
    );
    assert_eq!(rows(&g, SHAPES[3]), vec![vec![Value::Int(1)]]);
}

/// A count that lifts an existence PROBE seeks too: the columnar aggregate
/// used to skip its seek whenever a probe or degree was read (the per-id
/// path binds neither) and walked the whole label probing every member —
/// `{nodeType: 'email', userId: $u} WHERE exists((n)-[:HAS_ASK]->(:EmailAsk))`
/// probed 38k emails (96 ms) to answer for the 10 the index named. The
/// sought ids are now the WALK's population, which binds probes as the
/// whole-label walk does.
#[test]
fn a_count_with_a_probe_walks_the_probe_over_the_seek() {
    let g = corpus(Declared::CompositeAndOwner);
    const WALKED: &str = "interp.columnar aggregate walked its probes over a seek";
    for src in [
        "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE exists((d)-[:HAS]->(:Ask)) RETURN count(d) AS c",
        "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE NOT EXISTS { MATCH (d)-[:HAS]->(:Ask) } RETURN count(d) AS c",
        "MATCH (d:Doc {kind: 'email', owner: $owner}) RETURN count(d) AS c, sum(COUNT { (d)-[:HAS]->() }) AS asks",
    ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs scan disagree on `{src}`");
        assert!(
            counter(&g, src, WALKED) > 0,
            "`{src}` must walk its probe over the sought ids, not the label"
        );
    }
    // u8 owns docs 8 (one Ask) and 358 (none).
    assert_eq!(
        rows(
            &g,
            "MATCH (d:Doc {kind: 'email', owner: $owner}) WHERE exists((d)-[:HAS]->(:Ask)) RETURN count(d) AS c"
        ),
        vec![vec![Value::Int(1)]]
    );
}

/// A CORRELATED map (`{owner: o}` with `o` from the row) on a key with a
/// declared index seeks per row instead of memoising a scan of the whole
/// label; an undeclared correlated key keeps the memo. Both agree with the
/// seek-less path. On the mirror `UNWIND o.watchlist AS ticker OPTIONAL
/// MATCH (c:Company {primaryTicker: ticker})` built its memo over every
/// Company for 31 tickers — 34.7 s against Neo4j's 2.6 ms.
#[test]
fn a_declared_correlated_key_seeks_per_row_instead_of_memoising_the_label() {
    const MEMOS: &str = "interp.clause scan memos built";
    const DECLINED: &str = "interp.clause scan memo declined for a declared correlated key";
    const PROBED: &str = "interp.seed probed a declared scoped index";
    let g = corpus(Declared::CompositeAndOwner);
    let declared = "UNWIND ['u8', 'u9', 'u10', 'u11'] AS o OPTIONAL MATCH (d:Doc {kind: 'email', owner: o}) RETURN o, count(d) AS c ORDER BY o";
    let (on, off) = both(&g, declared);
    assert_eq!(on, off, "seek vs memo disagree");
    assert!(counter(&g, declared, DECLINED) > 0, "the declared key must decline the memo");
    assert_eq!(counter(&g, declared, MEMOS), 0, "…and build no memo");
    assert!(counter(&g, declared, PROBED) >= 4, "…probing the declared index per row");
    // u8 → docs 8, 358 (both email); u9 → 9, 359; u10 → 10, 360; u11 → 11, 361.
    assert_eq!(
        on,
        vec![
            vec![Value::Str("u10".into()), Value::Int(2)],
            vec![Value::Str("u11".into()), Value::Int(2)],
            vec![Value::Str("u8".into()), Value::Int(2)],
            vec![Value::Str("u9".into()), Value::Int(2)],
        ]
    );
    // CONTROL: `n` is undeclared — the memo is built once, as before.
    let undeclared = "UNWIND [8, 9, 10] AS x OPTIONAL MATCH (d:Doc {n: x}) RETURN x, count(d) AS c ORDER BY x";
    let (on, off) = both(&g, undeclared);
    assert_eq!(on, off);
    assert_eq!(counter(&g, undeclared, DECLINED), 0);
    assert_eq!(counter(&g, undeclared, MEMOS), 1, "an undeclared correlated key keeps the memo");
}

/// A composite declares EVERY key it carries (fix 47): Neo4j's composite
/// index answers a trailing key from its entries, so the mirror's declared
/// catalogue — composites and all — makes `owner` seekable even when no
/// single-property index names it. The selective trailing key is chosen
/// and the rows agree; before fix 47 only the leading `kind` counted and the
/// probe stayed on the unselective first entry.
#[test]
fn with_only_the_composite_declared_its_trailing_key_is_probed_and_rows_agree() {
    let g = corpus(Declared::CompositeOnly);
    for src in SHAPES {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs scan disagree on `{src}`");
        assert!(
            counter(&g, src, CHOSE_LATER) > 0 || counter(&g, src, COLUMNAR_SCOPED) > 0,
            "`{src}`: the composite's trailing `owner` key is declared too"
        );
    }
}

/// CONTROL: nothing declared, nothing chosen, same rows.
#[test]
fn without_a_declared_index_nothing_is_chosen_and_rows_agree() {
    let g = corpus(Declared::None);
    for src in SHAPES {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs scan disagree on `{src}`");
        assert_eq!(counter(&g, src, CHOSE_LATER), 0);
    }
}

/// The chosen seek is a CANDIDATE set, never an oracle: a second key that
/// excludes everything, a value of the wrong type, and a key on the wrong
/// label all still answer exactly what the scan answers.
#[test]
fn the_seek_narrows_the_stream_and_never_decides_membership() {
    let g = corpus(Declared::CompositeAndOwner);
    ddl(&g, "CREATE (:Other {kind: 'email', owner: 'u8', n: -1})");
    for src in [
        "MATCH (d:Doc {kind: 'note', owner: $owner}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
        "MATCH (d:Doc {kind: 'email', owner: 8}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
        "MATCH (d:Doc {owner: $owner, kind: 'email'}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
        "MATCH (d:Other {kind: 'email', owner: $owner}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
        "MATCH (d {kind: 'email', owner: $owner}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n",
    ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "seek vs scan disagree on `{src}`");
    }
    assert_eq!(
        rows(
            &g,
            "MATCH (d:Doc {kind: 'note', owner: $owner}) WITH d ORDER BY d.n LIMIT 5 RETURN d.n AS n"
        ),
        Vec::<Vec<Value>>::new()
    );
}
