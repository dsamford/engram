#![allow(non_snake_case)]
//! Differential tests for the core top-k / project path accepting an `ORDER BY`
//! key that is a BARE reference to a projection ALIAS (`RETURN b.bx AS ax …
//! ORDER BY ax`) rather than a pattern variable/property.
//!
//! The recognizer (`core_over_chain` + the shared `classify_key`) used to DECLINE
//! any `ORDER BY <alias>` because a bare `Var(alias)` resolves to no bound pattern
//! var — the whole shape fell to `run_streaming`. It now RESOLVES the alias to the
//! expression the RETURN projects (`ax` -> `b.bx`) and classifies/evaluates THAT,
//! so the core top-k fires and sorts by the same values `run_streaming`'s
//! post-projection ORDER BY scope produces.
//!
//! THE CONTRACT: for every accepted shape, `set_columnar_scans(true)` (the
//! pipeline) equals `set_columnar_scans(false)` (`run_streaming`) — the full ROW
//! SET *and its order*, byte-for-byte — and the core top-k FIRES (the
//! 'interp.pipeline hop runs' counter, NOT a fall to the streamed chain). A bare
//! PATTERN var in ORDER BY, and an alias whose target is an AGGREGATE, keep
//! DECLINING (ON==OFF via the general path, no fire).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// One scan start `a0:A`, hopping `-[:R]->` to seven `b:B` ends carrying `bx`
/// (int; a three-row tie at 50, one NULL) and `by` (str; one NULL). The tie +
/// nulls make an alias ORDER BY's DESC/ASC null placement and second-key tiebreak
/// observable, and every row is reached from the single start so the shape is a
/// plain single-hop top-k.
fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ap = BTreeMap::new();
    ap.insert("ak".to_string(), Value::Int(1));
    let a0 = g.create_node(&["A".into()], &ap).expect("a");
    let mk_b = |bx: Option<i64>, by: Option<&str>| {
        let mut p = BTreeMap::new();
        if let Some(v) = bx {
            p.insert("bx".to_string(), Value::Int(v));
        }
        if let Some(s) = by {
            p.insert("by".to_string(), Value::Str(s.to_string()));
        }
        g.create_node(&["B".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some(50), Some("p")), // b0 — tie group at 50
        mk_b(Some(50), Some("q")), // b1 — tie group at 50
        mk_b(Some(50), Some("r")), // b2 — tie group at 50
        mk_b(Some(10), Some("a")), // b3
        mk_b(Some(20), Some("b")), // b4
        mk_b(None, Some("z")),     // b5 — bx NULL
        mk_b(Some(30), None),      // b6 — by NULL
    ];
    for d in b {
        g.create_rel(a0, "R", d, &BTreeMap::new()).expect("R");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse '{src}': {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run '{src}': {e}"))
        .rows
}

/// Run `src` with the pipeline ON and the general path OFF (order preserved — the
/// order is under test).
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

/// Whether the single-stage core pipeline fired for `src` with columnar ON.
fn core_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get("interp.pipeline hop runs").copied() == Some(1)
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

fn s(x: &str) -> Value {
    Value::Str(x.to_string())
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// The canonical case: `RETURN b.bx AS ax, b.by AS ay ORDER BY ax DESC, ay ASC
/// LIMIT k`. ON==OFF row-for-row AND in order; the core top-k FIRES. DESC puts the
/// NULL bx FIRST, then the 50-tie broken by `ay` ASC — precisely the values
/// `run_streaming`'s alias-in-scope ORDER BY produces.
#[test]
fn alias_order_by_desc_asc_fires_and_matches() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, b.by AS ay ORDER BY ax DESC, ay ASC LIMIT 4";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "alias ORDER BY ON must equal OFF row-for-row and in order"
    );
    assert_eq!(
        on,
        vec![
            vec![Value::Null, s("z")], // bx NULL sorts FIRST under DESC
            vec![i(50), s("p")],
            vec![i(50), s("q")],
            vec![i(50), s("r")],
        ],
        "DESC bx (null first) then ay ASC over the 50-tie"
    );
    assert!(
        core_fired(&g, src),
        "the alias ORDER BY must FIRE the core top-k, not fall to run_streaming"
    );
}

/// The SAME result whether the ORDER BY is spelled with the ALIAS (`ax`, `ay`) or
/// the underlying pattern PROPERTIES (`b.bx`, `b.by`) — the alias resolves to
/// exactly the projected expression, and both fire the core top-k.
#[test]
fn alias_order_by_equals_pattern_prop_order_by() {
    let g = g();
    let alias = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, b.by AS ay ORDER BY ax ASC, ay ASC LIMIT 5";
    let prop = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, b.by AS ay ORDER BY b.bx ASC, b.by ASC LIMIT 5";
    let (alias_on, alias_off) = both(&g, alias, BTreeMap::new());
    let (prop_on, prop_off) = both(&g, prop, BTreeMap::new());
    assert_eq!(alias_on, alias_off, "alias form ON==OFF");
    assert_eq!(prop_on, prop_off, "prop form ON==OFF");
    assert_eq!(
        alias_on, prop_on,
        "ORDER BY alias must equal ORDER BY the aliased property"
    );
    assert!(core_fired(&g, alias), "the alias form must FIRE");
    assert!(core_fired(&g, prop), "the prop form must FIRE");
}

/// DESC + NULLS with an alias: `ORDER BY ax DESC` places the NULL bx FIRST (the
/// reversed comparison), ASC places it LAST. ON==OFF and the value is visible.
#[test]
fn alias_desc_and_asc_null_placement() {
    let g = g();
    // DESC: NULL first.
    let desc = "MATCH (a:A)-[:R]->(b:B) RETURN b.bx AS ax ORDER BY ax DESC LIMIT 100";
    let (on, off) = both(&g, desc, BTreeMap::new());
    assert_eq!(on, off, "alias DESC null-first ON==OFF");
    assert_eq!(
        on.first(),
        Some(&vec![Value::Null]),
        "null sorts first DESC"
    );
    assert!(core_fired(&g, desc), "alias DESC must FIRE");
    // ASC: NULL last.
    let asc = "MATCH (a:A)-[:R]->(b:B) RETURN b.bx AS ax ORDER BY ax ASC LIMIT 100";
    let (on, off) = both(&g, asc, BTreeMap::new());
    assert_eq!(on, off, "alias ASC null-last ON==OFF");
    assert_eq!(on.last(), Some(&vec![Value::Null]), "null sorts last ASC");
    assert!(core_fired(&g, asc), "alias ASC must FIRE");
}

/// A TIE at the LIMIT boundary broken by a SECOND alias key. `ORDER BY ax ASC, ay
/// ASC LIMIT 5` cuts the 50-tie between `ay='q'` (kept) and `ay='r'` (dropped) —
/// the second alias key decides the boundary, byte-identically to `run_streaming`.
#[test]
fn alias_second_key_breaks_tie_at_limit_boundary() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, b.by AS ay ORDER BY ax ASC, ay ASC LIMIT 5";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "tie-boundary alias ON==OFF in order");
    assert_eq!(
        on,
        vec![
            vec![i(10), s("a")],
            vec![i(20), s("b")],
            vec![i(30), Value::Null], // by NULL, but ax=30 is unique here
            vec![i(50), s("p")],
            vec![i(50), s("q")], // 'q' kept, 'r' dropped by the ay-ASC tiebreak
        ],
        "the ay-ASC second alias key keeps 'q' and drops 'r' at the LIMIT-5 boundary"
    );
    assert!(core_fired(&g, src), "the tie-boundary alias case must FIRE");
}

/// A MIX: one key an alias, the other the pattern property. Both resolve and the
/// core top-k fires; ON==OFF.
#[test]
fn alias_mixed_with_pattern_prop_key() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, b.by AS ay ORDER BY ax DESC, b.by ASC LIMIT 3";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "mixed alias/prop ORDER BY ON==OFF");
    assert!(core_fired(&g, src), "the mixed key case must FIRE");
}

// ─── DECLINES ─────────────────────────────────────────────────────────────────

/// ORDER BY a BARE PATTERN NODE VAR (`ORDER BY b`) — not an alias, the whole
/// entity. The node-identity primitive now lets `classify_key` vectorise it (`b`
/// reads its id-only node column via `NODE_IDENTITY_KEY`); node values are
/// order-Equal under `lt3` (no Node arm → Unknown), so the sort is a NO-OP that
/// keeps production order — byte-identical to the interp, and the core top-k
/// FIRES rather than declining.
#[test]
fn order_by_bare_pattern_node_var_is_a_noop_sort_and_fires() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) RETURN a.ak AS ak, b.bx AS x ORDER BY b LIMIT 5";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "bare pattern node var ORDER BY must agree with the interp"
    );
    assert!(
        core_fired(&g, src),
        "ORDER BY a bare node var now vectorises as a no-op sort and fires the core top-k"
    );
}

/// ORDER BY an alias whose TARGET is an AGGREGATE (`count(*) AS c … ORDER BY c`).
/// Resolving the alias reaches an aggregate expression the core path must not
/// own — it DECLINES (as does the aggregate recognizer's own alias ORDER BY), so
/// the general path answers; ON==OFF, no core fire.
#[test]
fn decline_order_by_alias_of_aggregate() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) \
         RETURN b.bx AS ax, count(*) AS c ORDER BY c DESC LIMIT 5";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "alias-of-aggregate ORDER BY must agree via fallback"
    );
    assert!(
        !core_fired(&g, src),
        "ORDER BY an alias whose target aggregates must DECLINE the core top-k"
    );
}

/// An alias whose target is a COMPUTED expression the column path cannot
/// vectorise (`b.bx + 1 AS ax … ORDER BY ax`) — resolving reaches `b.bx + 1`,
/// which `classify_key` declines, so the core path falls back; ON==OFF, no fire.
#[test]
fn decline_order_by_alias_of_computed_expr() {
    let g = g();
    let src = "MATCH (a:A)-[:R]->(b:B) RETURN b.bx + 1 AS ax ORDER BY ax DESC LIMIT 5";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on, off,
        "alias-of-computed-expr ORDER BY must agree via fallback"
    );
    assert!(
        !core_fired(&g, src),
        "ORDER BY an alias of a non-vectorisable computed expr must DECLINE"
    );
}
