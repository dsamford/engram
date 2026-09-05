#![allow(non_snake_case)]
//! UNWIND-led vectorized hop projection + ORDER BY/LIMIT top-k (Layer-4
//! increment 3b) — the LDBC IC9 stage-2 shape
//! `UNWIND $friends AS f MATCH (f)<-[:HAS_CREATOR]-(m:Message) WHERE ...
//! RETURN ... ORDER BY ... LIMIT k`, but with the friend list supplied as a
//! `$param` of NODE references (the collect-fed form declines — see the last
//! test).
//!
//! Differential: the same query with `set_columnar_scans(true)` (this operator)
//! must equal `set_columnar_scans(false)` (the general per-tuple path) — the
//! full ROW SET *and its order*. Production order is the UNWIND LIST ORDER over
//! `f` (duplicates kept) × REVERSE adjacency per `f` (the LIFO emission of
//! `expand_var_length` from a bound start). The tie test pins that: break the
//! `.rev()` and it diverges (the canary), which also proves the operator fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// f: Fr{fk}; m: Msg{mc int (ties + null), mn str (distinguishing, one null)}.
/// Incoming edges `(m)-[:C]->(f)` (so `(f)<-[:C]-(m)`): under f0 a THREE-member
/// tie group (m0,m1,m2 all mc=50, distinct mn) whose kept slice depends on
/// production order; under f1 a null-mc end, a doubled edge, a small key; f2
/// gets a null-mn end and a cross-edge from m0; f3 has no edges. Outgoing edges
/// `(f)-[:O]->(m)` for the outgoing leg.
struct G {
    g: Graph,
    f: [u64; 4],
    #[allow(dead_code)]
    m: [u64; 7],
}

fn gt() -> G {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_f = |fk: i64| {
        let mut p = BTreeMap::new();
        p.insert("fk".to_string(), Value::Int(fk));
        g.create_node(&["Fr".into()], &p).expect("f")
    };
    let f = [mk_f(1), mk_f(2), mk_f(3), mk_f(4)];
    let mk_m = |mc: Option<i64>, mn: Option<&str>| {
        let mut p = BTreeMap::new();
        if let Some(v) = mc {
            p.insert("mc".to_string(), Value::Int(v));
        }
        if let Some(s) = mn {
            p.insert("mn".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["Msg".into()], &p).expect("m")
    };
    let m = [
        mk_m(Some(50), Some("p")), // m0 — tie group under f0
        mk_m(Some(50), Some("q")), // m1 — tie group under f0
        mk_m(Some(50), Some("r")), // m2 — tie group under f0
        mk_m(Some(10), Some("a")), // m3
        mk_m(Some(20), Some("b")), // m4 (doubled edge)
        mk_m(None, Some("z")),     // m5 — mc NULL
        mk_m(Some(30), None),      // m6 — mn NULL
    ];
    // Incoming: (m)-[:C]->(f).
    for (s, d) in [
        (0, 0),
        (1, 0),
        (2, 0),
        (3, 1),
        (4, 1),
        (4, 1), // doubled
        (5, 1),
        (6, 2),
        (0, 2), // cross: m0 also feeds f2
    ] {
        g.create_rel(m[s], "C", f[d], &BTreeMap::new()).expect("C");
    }
    // Outgoing: (f)-[:O]->(m).
    for (s, d) in [(0, 3), (0, 4), (1, 6)] {
        g.create_rel(f[s], "O", m[d], &BTreeMap::new()).expect("O");
    }
    G { g, f, m }
}

fn node(g: &Graph, id: u64) -> Value {
    g.node(id).expect("node").expect("present")
}

fn list(g: &Graph, ids: &[u64]) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert(
        "list".to_string(),
        Value::List(ids.iter().map(|&i| node(g, i)).collect()),
    );
    p
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the vectorized operator ON and the general path OFF.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

#[test]
fn unwind_topk_matches_general_across_shapes() {
    let G { g, f, .. } = gt();
    let all = list(&g, &[f[0], f[1], f[2]]);
    let cases: &[&str] = &[
        // Total order (m.mc, m.mn, f.fk) — LIMIT slicing at several depths.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk LIMIT 1",
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk LIMIT 4",
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk LIMIT 100",
        // SKIP + LIMIT.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk SKIP 2 LIMIT 3",
        // DESC — NULLs sort FIRST under the reversed comparison.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc DESC, m.mn DESC LIMIT 5",
        // A NULL sort key (m5.mc null; m6.mn null), ASC — NULLs sort LAST.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn LIMIT 100",
        // ORDER BY over the f side.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mn AS n ORDER BY f.fk, m.mn LIMIT 6",
        // Compound WHERE over m (reuses eval_column) + ORDER BY + LIMIT.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) WHERE m.mc >= 20 AND m.mc < 60 RETURN m.mc AS c, m.mn AS n ORDER BY m.mc DESC, m.mn LIMIT 3",
        // A boolean ORDER BY key.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc IS NULL, m.mc LIMIT 8",
        // A const ORDER BY key (leaves production order intact).
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mn AS n ORDER BY 1, m.mn LIMIT 4",
        // Outgoing direction.
        "UNWIND $list AS f MATCH (f)-[:O]->(m:Msg) RETURN f.fk AS fk, m.mn AS n ORDER BY m.mn, f.fk LIMIT 3",
        // Unlabelled end (still valid — no m labels).
        "UNWIND $list AS f MATCH (f)<-[:C]-(m) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn LIMIT 4",
    ];
    for src in cases {
        let (on, off) = both(&g, src, all.clone());
        assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    }
}

#[test]
fn unwind_topk_exact_total_order_values() {
    // A value-determined slice (independent of production order): under
    // {f0,f1}, the three smallest mc are 10 (m3), then 20 (m4, DOUBLED) twice.
    let G { g, f, .. } = gt();
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) WHERE m.mc IS NOT NULL RETURN m.mc AS c ORDER BY m.mc, m.mn, f.fk LIMIT 3",
        list(&g, &[f[0], f[1]]),
    );
    assert_eq!(on, off, "columnar vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![Value::Int(10)],
            vec![Value::Int(20)],
            vec![Value::Int(20)],
        ],
        "value-determined slice"
    );
}

#[test]
fn unwind_topk_param_bounds() {
    // $s / $l SKIP and LIMIT resolve exactly as literals.
    let G { g, f, .. } = gt();
    let mut p = list(&g, &[f[0], f[1], f[2]]);
    p.insert("s".to_string(), Value::Int(1));
    p.insert("l".to_string(), Value::Int(3));
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk SKIP $s LIMIT $l",
        p,
    );
    assert_eq!(on, off, "param bounds");
    assert_eq!(on.len(), 3);
}

#[test]
fn unwind_topk_duplicate_in_list() {
    // A node repeated in the UNWIND list yields its matches TWICE, in the right
    // positions — the multiplicity the general path emits.
    let G { g, f, .. } = gt();
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn, f.fk LIMIT 100",
        list(&g, &[f[1], f[1]]),
    );
    assert_eq!(on, off, "duplicate f in the list");
    // f1 has 4 incoming edges (m3, m4×2, m5); duplicated → 8 rows.
    assert_eq!(
        on.len(),
        8,
        "each f1 match appears once per list occurrence"
    );
}

#[test]
fn unwind_topk_empty_and_nomatch() {
    let G { g, f, .. } = gt();
    // Empty list → no rows.
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
        list(&g, &[]),
    );
    assert_eq!(on, off);
    assert!(on.is_empty(), "empty list");
    // A list element with no matches (f3 has no edges) contributes nothing.
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
        list(&g, &[f[3]]),
    );
    assert_eq!(on, off);
    assert!(on.is_empty(), "f3 has no incoming C edges");
    // A NULL element binds f=null → no rows for it; the real f's rows remain.
    let mut p = BTreeMap::new();
    p.insert(
        "list".to_string(),
        Value::List(vec![node(&g, f[0]), Value::Null, node(&g, f[1])]),
    );
    let (on, off) = both(
        &g,
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn LIMIT 100",
        p,
    );
    assert_eq!(on, off, "a null element yields no rows, others unaffected");
}

#[test]
fn unwind_topk_tie_group_resolves_like_the_general_path() {
    // Under f0, m0/m1/m2 all have mc=50; the ORDER BY leaves them tied, so the
    // kept rows are decided PURELY by production order (list order × REVERSE
    // adjacency per f). This is the byte-identity-critical case and the canary.
    let G { g, f, .. } = gt();
    for k in 1..=3 {
        let src = format!(
            "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mn AS n ORDER BY m.mc LIMIT {k}"
        );
        let (on, off) = both(&g, &src, list(&g, &[f[0]]));
        assert_eq!(on, off, "tie group under f0, LIMIT {k}");
    }
}

#[test]
fn unwind_topk_declines_and_falls_back_identically() {
    // Each shape DECLINES (recogniser returns None) → the general path runs. ON
    // (columnar enabled but declined) must equal OFF regardless.
    let G { g, f, .. } = gt();
    let all = list(&g, &[f[0], f[1], f[2]]);
    let cases: &[&str] = &[
        // WHERE over f (not the end variable).
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) WHERE f.fk > 1 RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
        // Aggregation in the projection.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, count(m) AS c ORDER BY c DESC LIMIT 2",
        // DISTINCT.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN DISTINCT m.mc AS c ORDER BY m.mc LIMIT 5",
        // Variable-length hop.
        "UNWIND $list AS f MATCH (f)<-[:C*1..2]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
        // An ORDER BY key spanning BOTH variables.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN f.fk AS fk, m.mc AS c ORDER BY f.fk < m.mc LIMIT 5",
        // No LIMIT.
        "UNWIND $list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc",
        // Undirected hop.
        "UNWIND $list AS f MATCH (f)-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
        // A labelled start (f is already a node — this operator wants a bare start).
        "UNWIND $list AS f MATCH (f:Fr)<-[:C]-(m:Msg) RETURN m.mc AS c ORDER BY m.mc LIMIT 5",
    ];
    for src in cases {
        let (on, off) = both(&g, src, all.clone());
        assert_eq!(on, off, "decline must fall back identically: `{src}`");
    }
    // The collect-fed form (5 clauses) also declines and matches. Params empty
    // — the list is produced inline by `collect`.
    let src = "MATCH (x:Fr) WITH collect(x) AS list UNWIND list AS f MATCH (f)<-[:C]-(m:Msg) RETURN m.mc AS c, m.mn AS n ORDER BY m.mc, m.mn LIMIT 5";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "collect-fed form declines and matches the general path"
    );
}
