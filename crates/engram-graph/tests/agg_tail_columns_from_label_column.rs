#![allow(non_snake_case)]
//! Fix 33 (v108): the aggregate tails — the reduce's key and argument
//! columns and the group-key gather — read a labelled node var's properties
//! through the label's cached column (`load_var_columns_labelled`), so the
//! second statement of a listing reads no record for them. Before this the
//! Proposal listing on the OPTIONAL pipeline gathered `a.id`, `r.id` and the
//! carried `p`'s ORDER BY properties by a record read per distinct id on
//! every statement (~250 reads on the mirror), while the same labels' columns
//! sat in the property-column cache for the core path's filters.
//!
//! On a PAGED store with an interleaved layout — the mirror's, where a
//! column read declines the span walk and gathers — the second statement
//! gathers nothing. Every answer is checked against the general path's.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("s".to_string(), Value::Str("pending".to_string()));
    p.insert(
        "modes".to_string(),
        Value::List(vec![Value::Str("push".into()), Value::Str("onboarding".into())]),
    );
    p.insert("limit".to_string(), Value::Int(25));
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

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const GATHER: &str = "graph.column point-gather";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const FROM_LABEL: &str = "interp.pipeline bound-var columns read from the label column";
const OPTIONAL_RUNS: &str = "interp.pipeline optional runs";
const HOP_RUNS: &str = "interp.pipeline hop runs";

/// Proposals with artifacts and repos, KM projects tracking repos, every
/// node followed by a `:Filler` so each label is sparse in the id space.
fn paged_corpus() -> (Graph, std::path::PathBuf) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let filler = |g: &Graph, i: i64| {
        let mut f = BTreeMap::new();
        f.insert("other".to_string(), Value::Str(format!("filler-{i}-{}", "x".repeat(40))));
        g.create_node(&["Filler".into()], &f).expect("filler");
    };
    for i in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "status".to_string(),
            Value::Str(if i % 3 == 0 { "pending".into() } else { "done".into() }),
        );
        m.insert("priority".to_string(), Value::Int(i % 7));
        m.insert("proposedAt".to_string(), Value::Int(1_000_000 - i));
        m.insert("title".to_string(), Value::Str(format!("proposal {i}")));
        let p = g.create_node(&["Proposal".into()], &m).expect("proposal");
        filler(&g, i);
        for k in 0..((i / 3) % 3) {
            let mut am = BTreeMap::new();
            am.insert("id".to_string(), Value::Str(format!("art-{i}-{k}")));
            let a = g.create_node(&["Artifact".into()], &am).expect("artifact");
            g.create_rel(p, "HAS_ARTIFACT", a, &BTreeMap::new()).expect("has");
            filler(&g, 10_000 + i * 3 + k);
        }
        if i % 2 == 0 {
            let mut rm = BTreeMap::new();
            rm.insert("id".to_string(), Value::Str(format!("repo-{}", i % 11)));
            let r = g.create_node(&["Repo".into()], &rm).expect("repo");
            g.create_rel(p, "FOR_REPO", r, &BTreeMap::new()).expect("for");
            filler(&g, 20_000 + i);
        }
    }
    let mut repos = Vec::new();
    for i in 0..140i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "provider".to_string(),
            Value::Str(if i % 4 == 0 { "github".into() } else { "lore".into() }),
        );
        if i % 3 == 0 {
            m.insert("syncMode".to_string(), Value::Str("push".into()));
        }
        m.insert("orgId".to_string(), Value::Str(format!("org-{}", i % 7)));
        m.insert("externalId".to_string(), Value::Str(format!("ext-{i:04}")));
        m.insert("repoId".to_string(), Value::Str(format!("repo-{:04}", (i * 37) % 140)));
        repos.push(
            g.create_node(&["MRepo".into(), "ManagedRepo".into()], &m)
                .expect("mrepo"),
        );
        filler(&g, 30_000 + i);
    }
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:03}")));
        let p = g.create_node(&["KMProject".into()], &m).expect("project");
        filler(&g, 40_000 + i);
        for k in 0..(1 + i % 3) {
            let r = repos[((i * 11 + k * 29) % 140) as usize];
            let mut rm = BTreeMap::new();
            if (i + k) % 3 == 0 {
                rm.insert("primary".to_string(), Value::Bool(true));
            }
            g.create_rel(p, "TRACKS_REPO", r, &rm).expect("tracks");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!(
        "engram_agg_tail_label_column_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 64 * 1024).expect("into_paged");
    (Graph::new(store.clone(), Realm(1), Namespace(1)), dir)
}

const LISTING: &str = "MATCH (p:Proposal) WHERE p.status = $s \
    OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
    OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
    WITH p, collect(DISTINCT a.id) AS ids, r.id AS rid \
    RETURN p, ids, rid ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25";

const KM: &str = "MATCH (p:KMProject)-[t:TRACKS_REPO]->(lore:MRepo:ManagedRepo) \
    WHERE lore.provider = 'lore' AND coalesce(t.primary, false) = true \
    AND coalesce(lore.syncMode, 'onboarding') IN $modes \
    WITH DISTINCT lore \
    RETURN lore.orgId AS orgId, lore.externalId AS repositoryId, lore.repoId AS managedRepoId \
    ORDER BY lore.repoId LIMIT toInteger($limit)";

#[test]
fn the_second_listing_reads_its_tail_columns_from_the_label_column() {
    let (g, dir) = paged_corpus();
    let want = general(&g, LISTING);
    assert_eq!(want.len(), 25);
    let (got, first) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert!(count_of(&first, OPTIONAL_RUNS) > 0, "{first:?}");
    assert!(count_of(&first, FROM_LABEL) > 0, "{first:?}");
    let (again, second) = traced(&g, LISTING);
    assert_eq!(again, want);
    assert!(count_of(&second, SERVED) >= 2, "{second:?}");
    assert_eq!(
        count_of(&second, GATHER),
        0,
        "the tail's columns come from the cache on the second statement: {second:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_second_km_listing_reads_its_tail_columns_from_the_label_column() {
    let (g, dir) = paged_corpus();
    let want = general(&g, KM);
    assert!(want.len() >= 5, "fixture: {} rows", want.len());
    let (got, first) = traced(&g, KM);
    assert_eq!(got, want);
    assert!(count_of(&first, HOP_RUNS) > 0, "{first:?}");
    let (again, second) = traced(&g, KM);
    assert_eq!(again, want);
    assert!(count_of(&second, SERVED) >= 1, "{second:?}");
    // The relationship property `t.primary` has no label column — its
    // gather stays; the node columns are all served.
    assert!(count_of(&second, FROM_LABEL) > 0, "{second:?}");
    let _ = std::fs::remove_dir_all(dir);
}
