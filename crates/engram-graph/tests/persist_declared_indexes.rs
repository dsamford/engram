#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! DECLARED range indexes are written to sidecars, so a restart loads them
//! instead of rebuilding from a partition scan.
//!
//! `Graph::persist_indexes` existed and the server never called it, so every
//! restart rebuilt — measured at 43.2 s to warm official SF1. The sidecar is
//! safe by construction: `ensure_range_index` discards one whose vintage has
//! moved, so a stale file costs a rebuild and never a wrong answer.
//!
//! Only DECLARED indexes are persisted. Persisting whatever a query happened to
//! build would turn one ad-hoc statement's index into a permanent cost on every
//! maintenance tick; a declared index is the operator asking for it.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A temp dir that removes itself. No `tempfile` dev-dep, so the name is made
/// unique by pid + a process-local counter.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-sidecar-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        TmpDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn sidecars(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("idx-") && n.ends_with(".idx"))
        .collect();
    out.sort();
    out
}

/// The catalogue drives what is persisted — not whatever happened to be built.
#[test]
fn only_declared_properties_are_offered_for_persistence() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    assert!(
        g.declared_index_props().is_empty(),
        "nothing declared, nothing to persist"
    );

    ddl(&g, "CREATE INDEX a IF NOT EXISTS FOR (n:Churn) ON (n.id)");
    ddl(&g, "CREATE INDEX b IF NOT EXISTS FOR (n:Other) ON (n.nonce)");
    // A property that is written a lot but NOT declared must not appear.
    for i in 0..20i64 {
        run(&g, &format!("CREATE (:Churn {{id: {i}, undeclared: {i}}})"));
    }
    let props = g.declared_index_props();
    assert_eq!(
        props,
        vec!["id".to_string(), "nonce".to_string()],
        "only declared properties, deduplicated and in a deterministic order"
    );
}

/// The full round trip: persist, reopen the directory, and find the index
/// already there rather than rebuilt.
#[test]
fn a_persisted_index_is_loaded_on_reopen_instead_of_rebuilt() {
    let dir = TmpDir::new("roundtrip");
    let store = Store::new();
    {
        let g = Graph::new(store.clone(), Realm(1), Namespace(1));
        ddl(&g, "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)");
        for i in 0..400i64 {
            run(&g, &format!("CREATE (:Churn {{id: {i}, nonce: {}}})", i % 7));
        }
        // Page the store out to the directory, exactly as the server does.
        store
            .into_paged(dir.path(), 8 << 20)
            .expect("into_paged");
        let props = g.declared_index_props();
        let refs: Vec<&str> = props.iter().map(String::as_str).collect();
        let written = g.persist_indexes(dir.path(), &refs).expect("persist");
        assert_eq!(written, 1, "one declared property, one sidecar");
    }
    assert_eq!(
        sidecars(dir.path()).len(),
        1,
        "the sidecar file must actually be on disk: {:?}",
        sidecars(dir.path())
    );

    // Reopen the directory — the stand-in for a restart.
    let (reopened, _cache) = Store::open_paged_dir(dir.path(), 8 << 20).expect("reopen");
    let g2 = Graph::new(reopened, Realm(1), Namespace(1));
    let token = g2
        .prop_token_peek("id")
        .expect("the property token survives in the reopened store");
    assert!(
        g2.shared_store().persisted_index(token).is_some(),
        "the reopened store must carry the persisted index, so the first query \
         does not pay a partition scan"
    );
}

/// A sidecar is an OPTIMISATION: a corrupt one must cost a rebuild, never a
/// wrong answer and never a failed open.
#[test]
fn a_corrupt_sidecar_is_refused_rather_than_trusted() {
    let dir = TmpDir::new("corrupt");
    let store = Store::new();
    {
        let g = Graph::new(store.clone(), Realm(1), Namespace(1));
        ddl(&g, "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)");
        for i in 0..200i64 {
            run(&g, &format!("CREATE (:Churn {{id: {i}}})"));
        }
        store.into_paged(dir.path(), 8 << 20).expect("into_paged");
        let props = g.declared_index_props();
        let refs: Vec<&str> = props.iter().map(String::as_str).collect();
        g.persist_indexes(dir.path(), &refs).expect("persist");
    }
    // Flip a byte in the middle of the sidecar.
    let name = sidecars(dir.path()).first().cloned().expect("a sidecar");
    let path = dir.path().join(&name);
    let mut bytes = std::fs::read(&path).expect("read");
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write");

    // The open must SUCCEED — a bad optimisation is not a bad database.
    let (reopened, _cache) = Store::open_paged_dir(dir.path(), 8 << 20).expect("reopen");
    let g2 = Graph::new(reopened, Realm(1), Namespace(1));
    // And the answers must be right regardless of where the index came from.
    let q = parse_statement("MATCH (n:Churn {id: 42}) RETURN n.id").expect("parse");
    let got = run_query(&g2, &q, BTreeMap::new()).expect("query");
    assert_eq!(
        got.rows.len(),
        1,
        "a corrupt sidecar must cost a rebuild, not an answer"
    );
}
