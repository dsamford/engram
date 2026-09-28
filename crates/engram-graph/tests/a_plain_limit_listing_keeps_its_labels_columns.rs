#![allow(non_snake_case)]
//! Fix 112: a plain `LIMIT` listing over a label whose columns are not yet
//! kept walks the label WHOLE once — in id order, stopping at the limit —
//! so its columns are kept and every later listing reads the cache.
//!
//! The production `MATCH (a:NewsArticle) RETURN a.articleId AS id, a.title
//! AS title LIMIT 5000` gathered 5,000 fat records on every execution (54
//! ms on the mirror, Neo4j 18.5): fix 52's cut walk reads only the first
//! `cap` members, and the walk keeps whole-label columns alone.
//!
//! The rows are pinned to the first `cap` members in id order — the cut
//! walk's own — cold and warm; a label past the cache budget keeps the cut.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const WIDENED: &str =
    "interp.columnar projection walked its label whole to keep the columns for its limit";
const CUT: &str = "interp.columnar projection walk cut at the plain limit";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const GETS: &str = "store.gets";

const N: i64 = 6_000;

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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn article_id(i: i64) -> String {
    format!("{i:032x}")
}

/// `N` articles interleaved with six fillers each, on the paged store.
/// `tag` names the paged directory per test: the two tests run in parallel
/// in one process and each removes its own directory at the end, so a
/// directory named by the process id alone is deleted under its sibling.
fn corpus(tag: &str) -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..N {
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(article_id(i)));
        m.insert("title".to_string(), Value::Str(format!("Article {i}")));
        m.insert(
            "content".to_string(),
            Value::Str("a paragraph of text ".repeat(10)),
        );
        g.create_node(&["NewsArticle".into()], &m).expect("article");
        for f in 0..6 {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i * 6 + f));
            g.create_node(&["Filler".into()], &m).expect("filler");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_limit_keeps_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 64 * 1024 * 1024)
        .expect("into_paged");
    (Graph::new(store, Realm(1), Namespace(1)), dir)
}

fn expected(limit: i64) -> Vec<Vec<Value>> {
    (0..limit)
        .map(|i| {
            vec![
                Value::Str(article_id(i)),
                Value::Str(format!("Article {i}")),
            ]
        })
        .collect()
}

const STMT: &str = "MATCH (a:NewsArticle) RETURN a.articleId AS id, a.title AS title LIMIT 5000";

/// Cold, the walk is widened to the whole label and keeps its columns;
/// warm, the listing reads the cache and gathers no record — both answer
/// the first 5,000 members in id order.
#[test]
fn a_the_first_listing_keeps_the_columns_and_the_next_reads_them() {
    let (g, dir) = corpus("a");
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(5_000));
    assert_eq!(count_of(&c, WIDENED), 1, "cold: {c:?}");
    assert_eq!(count_of(&c, CUT), 0, "cold: {c:?}");
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(5_000));
    assert_eq!(count_of(&c, WIDENED), 0, "warm: {c:?}");
    assert!(count_of(&c, SERVED) >= 2, "warm: {c:?}");
    assert_eq!(count_of(&c, GETS), 0, "warm: {c:?}");
    // A smaller limit reads the same cache.
    let (got, c) = traced(
        &g,
        "MATCH (a:NewsArticle) RETURN a.articleId AS id, a.title AS title LIMIT 500",
    );
    assert_eq!(got, expected(500));
    assert_eq!(count_of(&c, GETS), 0, "{c:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// CONTROL: a label the cache budget cannot hold keeps fix 52's cut walk,
/// and answers the same rows.
#[test]
fn b_a_label_past_the_budget_keeps_the_cut_walk() {
    let (g, dir) = corpus("b");
    g.set_prop_column_budget(1024);
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(5_000));
    assert_eq!(count_of(&c, WIDENED), 0, "{c:?}");
    assert_eq!(count_of(&c, CUT), 1, "{c:?}");
    let _ = std::fs::remove_dir_all(dir);
}
