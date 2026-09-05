#![allow(non_snake_case)]
//! The production list-column count on a PAGED store: `MATCH
//! (g:GeopoliticalEvent) WHERE g.startAt IS NOT NULL AND $a IN
//! coalesce(g.affectedCountries, []) RETURN count(g)` stayed at 15 ms against
//! Neo4j's 6.3 on v99, with the borrowed/aligned columns built for it: the
//! trace showed one column served from the cache and one re-read on EVERY
//! run, never the column-at-a-time count. The presence-only count of the
//! same label WAS vectorised. This pins the repeat read on a paged store
//! with a sparse, interleaved label — the mirror's layout.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("a".to_string(), Value::Str("USA".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
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

const VECTORISED: &str = "interp.columnar aggregate counted over cached columns";
const SKIPPED: &str = "interp.columnar column read skipped the span walk for a sparse label";
const SERVED: &str = "interp.columnar column read served from the property-column cache";

/// 3,000 `:Ev` interleaved with 6,000 `:Filler` (two fillers after every
/// event, so the label is sparse in the id space); `countries` is a list on
/// two of three events, `startAt` on all but every seventh. Paged behind a
/// small block cache, like the mirror.
fn paged_corpus() -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        if i % 3 != 0 {
            m.insert(
                "countries".to_string(),
                Value::List(if i % 6 == 1 {
                    vec![Value::Str("USA".into()), Value::Str("CAN".into())]
                } else {
                    vec![Value::Str("DEU".into())]
                }),
            );
        }
        if i % 7 != 0 {
            m.insert("startAt".to_string(), Value::Str(format!("2026-08-{:02}", 1 + i % 28)));
        }
        g.create_node(&["Ev".into()], &m).expect("ev");
        for k in 0..2 {
            let mut f = BTreeMap::new();
            f.insert("other".to_string(), Value::Str(format!("filler-{i}-{k}-{}", "x".repeat(40))));
            g.create_node(&["Filler".into()], &f).expect("filler");
        }
    }
    // One directory per call: the two tests share a pid, and a shared
    // directory raced the other test's `remove_dir_all` (gate39: `into_paged`
    // NotFound under a loaded machine).
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!(
        "engram_list_column_paged_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 64 * 1024).expect("into_paged");
    (Graph::new(store.clone(), Realm(1), Namespace(1)), dir)
}

const COALESCE_IN: &str =
    "MATCH (e:Ev) WHERE e.startAt IS NOT NULL AND $a IN coalesce(e.countries, []) RETURN count(e) AS n";

#[test]
fn the_repeat_read_of_a_list_column_count_is_vectorised_on_a_paged_store() {
    let (g, dir) = paged_corpus();
    let expect = (0..3000i64).filter(|i| i % 6 == 1 && i % 7 != 0).count() as i64;
    let (first, c1) = traced(&g, COALESCE_IN);
    assert_eq!(first, vec![vec![Value::Int(expect)]]);
    eprintln!("first: {c1:?}");
    let (second, c2) = traced(&g, COALESCE_IN);
    assert_eq!(second, first);
    eprintln!("second: {c2:?}");
    let (third, c3) = traced(&g, COALESCE_IN);
    assert_eq!(third, first);
    eprintln!("third: {c3:?}");
    assert!(
        count_of(&c3, VECTORISED) > 0,
        "the third read must be column-at-a-time: {c3:?}"
    );
    assert_eq!(count_of(&c3, SKIPPED), 0, "nothing re-read on the third run: {c3:?}");
    assert_eq!(count_of(&c3, SERVED), 0, "no walk at all on the third run: {c3:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// THE MIRROR'S CASE: the list property was never written — no token, no
/// column, nothing to keep — so every run of v99 walked the label and
/// evaluated the predicate per member (44k evaluations, 15 ms vs Neo4j's 6).
/// A property nothing ever wrote is Null everywhere: an aligned column of
/// Nulls, and the count is column-at-a-time from the second read (the first
/// keeps the presence column).
#[test]
fn a_never_written_property_is_an_all_null_column_for_the_vectorised_count() {
    let (g, dir) = paged_corpus();
    const ABSENT: &str =
        "MATCH (e:Ev) WHERE e.startAt IS NOT NULL AND $a IN coalesce(e.nothing, []) RETURN count(e) AS n";
    const ABSENT_PRESENCE: &str = "MATCH (e:Ev) WHERE e.nothing IS NOT NULL RETURN count(e) AS n";
    const ABSENT_OR: &str =
        "MATCH (e:Ev) WHERE e.startAt IS NOT NULL AND ($a IN coalesce(e.nothing, []) OR e.startAt = '2026-08-05') RETURN count(e) AS n";
    let (first, _) = traced(&g, ABSENT);
    assert_eq!(first, vec![vec![Value::Int(0)]]);
    let (second, c) = traced(&g, ABSENT);
    assert_eq!(second, first);
    assert!(count_of(&c, VECTORISED) > 0, "column-at-a-time over a Null column: {c:?}");
    assert!(count_of(&c, "graph.property column absent everywhere") > 0, "{c:?}");
    assert!(count_of(&c, "cypher.expressions evaluated") < 100, "no per-member walk: {c:?}");
    // Presence of a never-written property: Null everywhere, count 0, vectorised.
    let (r, c) = traced(&g, ABSENT_PRESENCE);
    assert_eq!(r, vec![vec![Value::Int(0)]]);
    assert!(count_of(&c, VECTORISED) > 0, "{c:?}");
    // Beside a real column in an OR: the real one is served, the absent one is Null.
    let expect = (0..3000i64).filter(|i| i % 7 != 0 && 1 + i % 28 == 5).count() as i64;
    let _ = traced(&g, ABSENT_OR);
    let (r, c) = traced(&g, ABSENT_OR);
    assert_eq!(r, vec![vec![Value::Int(expect)]]);
    assert!(count_of(&c, VECTORISED) > 0, "{c:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
