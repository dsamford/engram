#![allow(non_snake_case)]
//! Fix 109: a plain `LIMIT` listing over a label whose cap exceeds the
//! per-id seek cap still reads its property values.
//!
//! On the production mirror `MATCH (a:NewsArticle) RETURN a.articleId AS
//! id LIMIT 5000` answered 5,000 rows of `id: null` (and `id, title` both
//! null) while `LIMIT 2000` and `LIMIT 500` answered the ids, and no
//! article lacks the property — the corpus sample shows Neo4j's id beside
//! engram's null. The rows are pinned against the store's own values,
//! over a dense label and a label interleaved with fillers, on the paged
//! store the mirror runs.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

/// `n` articles, each followed by `fillers` other nodes; paged when asked.
fn corpus(n: i64, fillers: i64, paged: bool) -> (Graph, Option<std::path::PathBuf>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("{i:032x}")));
        m.insert("title".to_string(), Value::Str(format!("Article {i}")));
        m.insert(
            "content".to_string(),
            Value::Str("a paragraph of text ".repeat(10)),
        );
        g.create_node(&["NewsArticle".into()], &m).expect("article");
        for f in 0..fillers {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i * fillers + f));
            g.create_node(&["Filler".into()], &m).expect("filler");
        }
    }
    if !paged {
        return (g, None);
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!(
        "engram_capped_listing_{}_{}",
        std::process::id(),
        fillers
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 64 * 1024 * 1024)
        .expect("into_paged");
    (Graph::new(store, Realm(1), Namespace(1)), Some(dir))
}

fn check(g: &Graph, limit: usize) {
    for src in [
        format!("MATCH (a:NewsArticle) RETURN a.articleId AS id LIMIT {limit}"),
        format!("MATCH (a:NewsArticle) RETURN a.articleId AS id, a.title AS title LIMIT {limit}"),
    ] {
        for pass in 0..2 {
            let (got, c) = traced(g, &src);
            assert_eq!(got.len(), limit, "`{src}` pass {pass}: {c:?}");
            let nulls = got
                .iter()
                .filter(|r| r.iter().any(|v| matches!(v, Value::Null)))
                .count();
            assert_eq!(nulls, 0, "`{src}` pass {pass}: {nulls} null rows: {c:?}");
            let ids: std::collections::BTreeSet<String> = got
                .iter()
                .map(|r| match &r[0] {
                    Value::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
            assert_eq!(ids.len(), limit, "`{src}` pass {pass}: duplicate ids");
        }
    }
}

#[test]
fn a_a_capped_listing_past_the_seek_cap_reads_its_values_paged_and_sparse() {
    let (g, dir) = corpus(6_000, 6, true);
    check(&g, 500);
    check(&g, 2_000);
    check(&g, 5_000);
    if let Some(d) = dir {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn b_dense_and_in_memory_labels_read_their_values_too() {
    let (g, dir) = corpus(6_000, 0, true);
    check(&g, 5_000);
    if let Some(d) = dir {
        let _ = std::fs::remove_dir_all(d);
    }
    let (g, _) = corpus(6_000, 6, false);
    check(&g, 5_000);
}
