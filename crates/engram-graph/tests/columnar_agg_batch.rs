//! The columnar aggregate's member-scan BATCHING proven byte-identical to the
//! whole-walk fold AND to the row-at-a-time interp. A small member-batch size forces
//! a genuine MULTI-batch split whose groups SPAN batches (every language recurs in a
//! later batch), so the cross-batch fold accumulation — the whole point — is what's
//! exercised. This is the bound that keeps a large scan aggregate's materialised
//! column (BI3's ~1.5M-row / ~369 MB language column) to one batch.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn node(g: &Graph, label: &str, props: &[(&str, Value)]) -> u64 {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    g.create_node(&[label.into()], &m).expect("node")
}
fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap();
    run_query(g, &q, BTreeMap::new()).unwrap().rows
}
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn three(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_columnar_agg_batch(true);
    g.set_columnar_agg_batch_size(3); // force a multi-batch split
    let batched = rows(g, src);
    g.set_columnar_agg_batch(false);
    let whole = rows(g, src);
    g.set_columnar_agg_batch(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (batched, whole, interp)
}
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_columnar_agg_batch(true);
    g.set_columnar_agg_batch_size(3);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.columnar aggregate batched")
        .copied()
        .unwrap_or(0)
        > 0
}

const SRC: &str =
    "MATCH (m:Message) RETURN m.language AS lang, count(m) AS c ORDER BY c DESC, lang ASC";
const SRC_WHERE: &str = "MATCH (m:Message) WHERE m.language IS NOT NULL \
    RETURN m.language AS lang, count(m) AS c ORDER BY c DESC, lang ASC";

fn ten_messages() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // 10 messages across 3 languages; scanned in id order, batch size 3 → batches
    // [m1..3][m4..6][m7..9][m10], every language recurring across batches.
    for lang in ["en", "en", "fr", "de", "en", "fr", "de", "en", "fr", "en"] {
        node(&g, "Message", &[("language", s(lang))]);
    }
    g
}

#[test]
fn columnar_agg_batch_matches_whole_and_interp() {
    let g = ten_messages();
    let (batched, whole, interp) = three(&g, SRC);
    assert_eq!(batched, whole, "batched vs whole-walk disagree");
    assert_eq!(batched, interp, "batched vs interp disagree");
    assert_eq!(
        batched,
        vec![
            vec![s("en"), i(5)],
            vec![s("fr"), i(3)],
            vec![s("de"), i(2)]
        ],
        "counts folded across batches, ordered by count then language"
    );
    assert!(
        fired(&g, SRC),
        "a large Nodes scan must take the batched path"
    );
}

#[test]
fn columnar_agg_batch_with_where_matches() {
    // The BI3 shape exactly: a presence WHERE plus a value RETURN, both on the
    // batched column — the split must still be byte-identical.
    let g = ten_messages();
    // add two messages with no language (excluded by the WHERE, counted otherwise)
    node(&g, "Message", &[]);
    node(&g, "Message", &[]);
    let (batched, whole, interp) = three(&g, SRC_WHERE);
    assert_eq!(batched, whole, "batched vs whole disagree (WHERE)");
    assert_eq!(batched, interp, "batched vs interp disagree (WHERE)");
    assert_eq!(
        batched,
        vec![
            vec![s("en"), i(5)],
            vec![s("fr"), i(3)],
            vec![s("de"), i(2)]
        ],
        "the null-language messages are excluded by IS NOT NULL"
    );
    assert!(fired(&g, SRC_WHERE), "the WHERE'd scan must also batch");
}
