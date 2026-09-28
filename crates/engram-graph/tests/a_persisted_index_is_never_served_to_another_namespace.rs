#![allow(clippy::disallowed_methods)]
//! Security milestone §2.7: a persisted range index is served only to the
//! `(realm, namespace)` that wrote it.
//!
//! Index-at-seal sidecars were named `idx-<token>.idx`, and loaded into one
//! store-wide map keyed by the token alone. But a property token is minted per
//! graph — the dictionary lives under each graph's own KV prefix — so two
//! tenants sharing a store routinely give unrelated properties the SAME token
//! number. The server persists every graph's declared indexes into one
//! directory, and after a restart the reader asked `persisted_index(token)`
//! with its own token: tenant B was served tenant A's index, built from tenant
//! A's values, whenever the two tokens coincided. The vintage check could not
//! refuse it, because the clock it compares against belongs to the shared
//! store, not to the graph.
//!
//! Two directions, because a one-direction gate proves nothing: the foreign
//! graph must NOT be served the sidecar, and the graph that wrote it MUST still
//! be — otherwise "never served" could be satisfied by a loader that loads
//! nothing at all. A third case pins the migration: a sidecar written under the
//! old token-only name is not adopted by anyone.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

const WHOLE: &str = "graph.range index served from disk";
const BUILT: &str = "graph.range index builds";
const LEGACY: &str = "store.legacy index sidecar not adopted";

/// A temp dir that removes itself; unique by pid + a process-local counter.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-foreign-idx-{}-{}-{}",
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn node(g: &Graph, prop: &str, value: &str) -> u64 {
    let mut m = BTreeMap::new();
    m.insert(prop.to_string(), Value::Str(value.to_string()));
    g.create_node(&["Item".into()], &m).expect("create")
}

/// Tenant A holds `secret` values; tenant B holds `status` values. Built the
/// same way in both graphs, so the one property each mints gets the same token
/// number in each — the collision this test depends on, asserted rather than
/// assumed. Returns B's ids whose status is `open`.
fn two_tenants(store: &Store, a: (Realm, Namespace), b: (Realm, Namespace)) -> Vec<u64> {
    let ga = Graph::new(store.clone(), a.0, a.1);
    let gb = Graph::new(store.clone(), b.0, b.1);
    for i in 0..200 {
        node(&ga, "secret", &format!("s-{i}"));
    }
    let mut open = Vec::new();
    for i in 0..50 {
        let status = if i % 5 == 0 { "open" } else { "closed" };
        let id = node(&gb, "status", status);
        if status == "open" {
            open.push(id);
        }
    }
    assert_eq!(
        ga.prop_token_peek("secret"),
        gb.prop_token_peek("status"),
        "the premise: two graphs on one store gave unrelated properties the same token"
    );
    open
}

/// Page the store out, persist ONLY tenant A's index, and reopen the directory
/// — the server's quiescent maintenance tick followed by a restart.
fn persist_a_and_reopen(store: Store, dir: &TmpDir, a: (Realm, Namespace)) -> Store {
    store.seal();
    let _ = store.into_paged(dir.path(), 1 << 20).expect("into_paged");
    let ga = Graph::new(store.clone(), a.0, a.1);
    assert_eq!(
        ga.persist_indexes(dir.path(), &["secret"]).expect("persist"),
        1,
        "tenant A's sidecar was written"
    );
    drop(ga);
    drop(store);
    let (reopened, _cache) = Store::open_paged_dir(dir.path(), 1 << 20).expect("reopen");
    reopened
}

fn probe(g: &Graph, prop: &str, value: &str) -> (Vec<u64>, BTreeMap<String, u64>) {
    let v = Value::Str(value.to_string());
    let (ids, tr) = engram_observe::with_trace(|| {
        g.index_probe_eq(prop, &v, None)
            .expect("probe")
            .expect("servable")
    });
    (ids, tr.counters().clone())
}

fn the_foreign_graph_is_refused_and_the_owner_is_served(
    tag: &str,
    a: (Realm, Namespace),
    b: (Realm, Namespace),
) {
    let dir = TmpDir::new(tag);
    let store = Store::new();
    let open = two_tenants(&store, a, b);
    let reopened = persist_a_and_reopen(store, &dir, a);

    // Tenant B never wrote a sidecar, so it must build its own index — and
    // answer from its own rows.
    let gb = Graph::new(reopened.clone(), b.0, b.1);
    let (ids, c) = probe(&gb, "status", "open");
    assert_eq!(
        count_of(&c, WHOLE),
        0,
        "{tag}: tenant B was served a persisted index it did not write: {c:?}"
    );
    assert_eq!(count_of(&c, BUILT), 1, "{tag}: B builds its own: {c:?}");
    assert_eq!(ids, open, "{tag}: B answers from B's rows");

    // The positive control: tenant A still gets its own sidecar.
    let ga = Graph::new(reopened, a.0, a.1);
    let (ids, c) = probe(&ga, "secret", "s-7");
    assert_eq!(
        count_of(&c, WHOLE),
        1,
        "{tag}: the owner must still be served its sidecar, or this test \
         would pass against a loader that loads nothing: {c:?}"
    );
    assert_eq!(ids.len(), 1, "{tag}: one node carries s-7");
}

#[test]
fn a_persisted_index_is_not_served_to_another_realm() {
    the_foreign_graph_is_refused_and_the_owner_is_served(
        "realm",
        (Realm(1), Namespace(1)),
        (Realm(2), Namespace(1)),
    );
}

#[test]
fn a_persisted_index_is_not_served_to_another_namespace_of_the_same_realm() {
    the_foreign_graph_is_refused_and_the_owner_is_served(
        "namespace",
        (Realm(1), Namespace(1)),
        (Realm(1), Namespace(2)),
    );
}

/// The migration: a sidecar under the old token-only name carries no
/// coordinate, so it cannot be attributed to a graph and is adopted by none —
/// not even the one that happens to have written it. It costs one rebuild.
#[test]
fn a_sidecar_under_the_legacy_token_only_name_is_adopted_by_no_one() {
    let a = (Realm(1), Namespace(1));
    let dir = TmpDir::new("legacy");
    let store = Store::new();
    let ga = Graph::new(store.clone(), a.0, a.1);
    for i in 0..200 {
        node(&ga, "secret", &format!("s-{i}"));
    }
    let token = ga.prop_token_peek("secret").expect("minted");
    drop(ga);
    let reopened = persist_a_and_reopen(store, &dir, a);
    drop(reopened);

    // Rename the sidecar to the name every build before this fix wrote.
    let written: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("idx-") && n.ends_with(".idx"))
        })
        .collect();
    assert_eq!(written.len(), 1, "one sidecar: {written:?}");
    let legacy = dir.path().join(format!("idx-{token}.idx"));
    if written[0] != legacy {
        std::fs::rename(&written[0], &legacy).expect("rename to the legacy name");
    }

    let (reopen, tr) =
        engram_observe::with_trace(|| Store::open_paged_dir(dir.path(), 1 << 20).expect("reopen"));
    assert_eq!(
        count_of(tr.counters(), LEGACY),
        1,
        "the legacy sidecar is seen and refused, not silently skipped: {:?}",
        tr.counters()
    );
    let ga = Graph::new(reopen.0, a.0, a.1);
    let (ids, c) = probe(&ga, "secret", "s-7");
    assert_eq!(count_of(&c, WHOLE), 0, "not adopted: {c:?}");
    assert_eq!(count_of(&c, BUILT), 1, "rebuilt instead: {c:?}");
    assert_eq!(ids.len(), 1, "and the answer is right regardless");
}
