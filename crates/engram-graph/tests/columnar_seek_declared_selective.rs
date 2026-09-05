#![allow(non_snake_case)]
//! The columnar count/projection seek probes the DECLARED, LABEL-SCOPED index
//! on the MOST SELECTIVE equality — not the partition-wide index on whichever
//! equality the WHERE (or the pattern map) happened to name first.
//!
//! The production shape (2026-09-04): `MATCH (n:UserDataNode {nodeType: 'email',
//! userId: $u}) RETURN count(n)` took the first conjunct, `nodeType`, probed the
//! partition-wide index for it (18k of 38k ids, over the cap) and scanned the
//! label — while the operator had declared `FOR (n:UserDataNode) ON (n.userId)`
//! exactly so that shape would seek. Neo4j answered from its index in 4 ms.
//!
//! The contract: with a declared index on the selective key, the seek FIRES via
//! a scoped probe (counted) whichever conjunct comes first; the rows are
//! byte-identical to the general path; and with no declared index nothing
//! changes (the control).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the columnar operators ON, then the general path (the oracle).
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

/// A named counter's value after running `src` once with the operators ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

const SCOPED: &str = "interp.columnar seek chose a declared scoped index";

/// 700 `:Doc` nodes — above the seek floor (512). `kind` is UNSELECTIVE
/// ('email' on 600) and undeclared; `owner` is SELECTIVE (2 per value) and
/// declared when `declare` is set. Nodes are created in one pass so the label
/// is dense in id space — this test is about the planner, not the walk.
fn corpus(declare: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    if declare {
        ddl(&g, "CREATE INDEX doc_owner IF NOT EXISTS FOR (n:Doc) ON (n.owner)");
    }
    for i in 0..700i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("owner".to_string(), Value::Str(format!("u{}", i % 350)));
        m.insert("n".to_string(), Value::Int(i));
        g.create_node(&["Doc".into()], &m).expect("node");
    }
    g
}

const COUNT_FIRST_UNSELECTIVE: &str =
    "MATCH (d:Doc {kind: 'email', owner: 'u8'}) RETURN count(d) AS n";
const COUNT_FIRST_SELECTIVE: &str =
    "MATCH (d:Doc {owner: 'u8', kind: 'email'}) RETURN count(d) AS n";
const COUNT_WHERE: &str =
    "MATCH (d:Doc) WHERE d.kind = 'email' AND d.owner = 'u8' RETURN count(d) AS n";
const PROJECT_WHERE: &str =
    "MATCH (d:Doc) WHERE d.kind = 'email' AND d.owner = 'u8' RETURN d.n AS n ORDER BY n";

/// With the selective key declared, every arrangement of the two equalities
/// seeks the declared scoped index and agrees with the general path.
#[test]
fn a_declared_selective_key_is_sought_whichever_conjunct_comes_first() {
    let g = corpus(true);
    for src in [
        COUNT_FIRST_UNSELECTIVE,
        COUNT_FIRST_SELECTIVE,
        COUNT_WHERE,
        PROJECT_WHERE,
    ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree on `{src}`");
        assert!(
            counter(&g, src, SCOPED) > 0,
            "`{src}` must seek the declared scoped index on `owner`"
        );
    }
    // Fixture sanity: u8 owns docs 8 and 358, both 'email' (neither is a
    // multiple of 7 — u7's docs 7 and 357 are both 'note', which is why the
    // owner is u8).
    assert_eq!(rows(&g, COUNT_WHERE), vec![vec![Value::Int(2)]]);
    assert_eq!(
        rows(&g, PROJECT_WHERE),
        vec![vec![Value::Int(8)], vec![Value::Int(358)]]
    );
}

/// CONTROL: with nothing declared the scoped probe never fires, and the
/// answers are the same — the change is which index is asked, not the rows.
#[test]
fn without_a_declared_index_the_scoped_probe_never_fires_and_rows_agree() {
    let g = corpus(false);
    for src in [COUNT_FIRST_UNSELECTIVE, COUNT_WHERE, PROJECT_WHERE] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree on `{src}`");
        assert_eq!(
            counter(&g, src, SCOPED),
            0,
            "no declared index — nothing to probe scoped on `{src}`"
        );
    }
    assert_eq!(rows(&g, COUNT_WHERE), vec![vec![Value::Int(2)]]);
}

/// A declared index on the UNSELECTIVE key alone must not make the seek fire
/// where the scan wins: 600 of 700 is a scan, and the probe result says so.
#[test]
fn a_declared_but_unselective_key_still_scans() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    ddl(&g, "CREATE INDEX doc_kind IF NOT EXISTS FOR (n:Doc) ON (n.kind)");
    for i in 0..700i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("owner".to_string(), Value::Str(format!("u{}", i % 350)));
        g.create_node(&["Doc".into()], &m).expect("node");
    }
    let (on, off) = both(&g, COUNT_FIRST_UNSELECTIVE);
    assert_eq!(on, off);
    assert_eq!(on, vec![vec![Value::Int(2)]]);
    assert_eq!(
        counter(&g, COUNT_FIRST_UNSELECTIVE, SCOPED),
        0,
        "a probe of 600 in a label of 700 loses to the scan and must not be chosen"
    );
}
