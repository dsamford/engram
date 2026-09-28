#![allow(clippy::disallowed_methods)]
//! The tenant-isolation differential (security milestone §10) — the gate that
//! would have caught §2.7, and the one the milestone treats as release-blocking.
//!
//! Isolation is proven here, not argued from the key layout. Two tenants are
//! built on ONE store, deliberately made to collide everywhere a per-graph
//! structure could be confused for another graph's:
//!
//! - the same node ids — both graphs create the same number of nodes in the
//!   same order, and ids are minted per namespace;
//! - the same label and relationship-type names;
//! - the same property-token NUMBERS for DIFFERENT property names — tenant A
//!   mints `name` first, tenant B mints `age` first, so A's `name` and B's `age`
//!   share a token (asserted, not assumed);
//! - both tenants' sidecars in one directory.
//!
//! A battery covering the engine's main paths — label scans, index seeks and
//! ranges, string predicates, traversals, variable-length and shortest paths,
//! aggregation, OPTIONAL MATCH, full-text search, graph algorithms, catalogue
//! procedures — runs in each tenant, and every answer must equal that
//! tenant's answer on a store holding it ALONE. It runs resident; after a paged
//! restart that adopts both tenants' sidecars; after a second maintenance tick
//! and restart; and after a write in one tenant. Twice over: two realms, and two
//! namespaces of one realm.
//!
//! Any new structure that is store-wide where it should be per-graph — the
//! class of §2.7 — changes an answer here.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A temp dir that removes itself; unique by pid + a process-local counter.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "engram-isolation-{}-{}-{}",
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

/// What distinguishes the two tenants' corpora. Same shape of build, different
/// values, a different first-minted property, and a different graph topology.
#[derive(Clone, Copy)]
struct Tenant {
    coord: (Realm, Namespace),
    /// Prefix of every string value, so answers are visibly the tenant's own.
    tag: &'static str,
    /// The property minted first — the lever that makes tokens collide.
    first: &'static str,
    /// Age multiplier: a different value distribution.
    mul: i64,
    /// KNOWS offset: a different topology.
    hop: i64,
    /// A word only this tenant's posts contain.
    word: &'static str,
}

const A: fn((Realm, Namespace)) -> Tenant = |coord| Tenant {
    coord,
    tag: "a",
    first: "name",
    mul: 1,
    hop: 1,
    word: "quasar",
};

const B: fn((Realm, Namespace)) -> Tenant = |coord| Tenant {
    coord,
    tag: "b",
    first: "age",
    mul: 7,
    hop: 3,
    word: "nebula",
};

fn run(g: &Graph, src: &str) -> String {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let r = run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
    format!("{:?}", r.rows)
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

/// The build, as a list of steps, so two tenants can be built INTERLEAVED on one
/// store — commit timestamps and id allocation alternate between them, the way a
/// shared server's would.
fn build_steps(t: Tenant) -> Vec<Box<dyn Fn(&Graph)>> {
    let Tenant {
        tag,
        first,
        mul,
        hop,
        word,
        ..
    } = t;
    vec![
        Box::new(move |g| {
            let v = if first == "name" {
                format!("{{name: '{tag}-seed'}}")
            } else {
                "{age: 99}".to_string()
            };
            run(g, &format!("CREATE (:Person {v})"));
        }),
        Box::new(|g| ddl(g, "CREATE INDEX person_name IF NOT EXISTS FOR (n:Person) ON (n.name)")),
        Box::new(|g| ddl(g, "CREATE INDEX person_age IF NOT EXISTS FOR (n:Person) ON (n.age)")),
        Box::new(move |g| {
            run(
                g,
                &format!(
                    "UNWIND range(0, 119) AS i \
                     CREATE (:Person {{name: '{tag}-' + toString(i), age: (i * {mul}) % 50}})"
                ),
            );
        }),
        Box::new(move |g| {
            run(
                g,
                &format!(
                    "UNWIND range(0, 4) AS i CREATE (:City {{name: '{tag}-city-' + toString(i)}})"
                ),
            );
        }),
        Box::new(move |g| {
            run(
                g,
                &format!(
                    "MATCH (p:Person) WHERE p.age IS NOT NULL AND p.name IS NOT NULL \
                     MATCH (c:City {{name: '{tag}-city-' + toString(p.age % 5)}}) \
                     CREATE (p)-[:LIVES_IN]->(c)"
                ),
            );
        }),
        Box::new(move |g| {
            run(
                g,
                &format!(
                    "MATCH (p:Person), (q:Person) \
                     WHERE p.age IS NOT NULL AND q.age IS NOT NULL AND p.name IS NOT NULL \
                       AND q.name IS NOT NULL AND (p.age + {hop}) % 50 = q.age AND p <> q \
                     CREATE (p)-[:KNOWS]->(q)"
                ),
            );
        }),
        Box::new(move |g| {
            run(
                g,
                &format!(
                    "MATCH (p:Person) WHERE p.age % 4 = 0 AND p.name IS NOT NULL \
                     CREATE (p)-[:WROTE]->(:Post {{body: '{word} engine note ' + toString(p.age)}})"
                ),
            );
        }),
        Box::new(|g| ddl(g, "CREATE FULLTEXT INDEX posts FOR (d:Post) ON EACH [d.body]")),
    ]
}

/// The same query text runs in both tenants; the answers differ by tenant, and
/// must not differ from each tenant's solo answer.
const BATTERY: &[&str] = &[
    "MATCH (p:Person) RETURN count(p) AS n",
    "MATCH (p:Person) WHERE p.age = 7 RETURN p.name AS name ORDER BY name",
    "MATCH (p:Person {age: 21}) RETURN p.name AS name ORDER BY name",
    "MATCH (p:Person) WHERE p.age >= 10 AND p.age < 20 RETURN p.name AS name ORDER BY name",
    "MATCH (p:Person) WHERE p.name = 'a-7' OR p.name = 'b-7' RETURN p.age AS age",
    "MATCH (p:Person) WHERE p.name STARTS WITH 'a-1' OR p.name STARTS WITH 'b-1' RETURN count(p) AS n",
    "MATCH (p:Person) WHERE p.name CONTAINS '-2' RETURN p.name AS name ORDER BY name",
    "MATCH (p:Person) WHERE p.name ENDS WITH '9' RETURN count(p) AS n",
    "MATCH (p:Person) WHERE p.age IS NULL RETURN count(p) AS n",
    "MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN p.name AS a, q.name AS b ORDER BY a, b LIMIT 60",
    "MATCH (p:Person)-[:KNOWS]->()-[:KNOWS]->(r) RETURN count(r) AS n",
    "MATCH (p:Person {age: 3})-[:KNOWS*1..3]->(q) RETURN count(DISTINCT q) AS n",
    "MATCH (a:Person {age: 1}), (b:Person {age: 5}) \
     MATCH p = shortestPath((a)-[:KNOWS*]->(b)) \
     RETURN a.name AS a, b.name AS b, length(p) AS len ORDER BY a, b LIMIT 20",
    "MATCH (p:Person)-[:LIVES_IN]->(c:City) \
     RETURN c.name AS city, count(p) AS n, sum(p.age) AS s ORDER BY city",
    "MATCH (p:Person) WHERE p.name IS NOT NULL OPTIONAL MATCH (p)-[:WROTE]->(x:Post) \
     RETURN p.name AS name, count(x) AS posts ORDER BY name LIMIT 40",
    "MATCH (p:Person) RETURN DISTINCT p.age AS age ORDER BY age DESC SKIP 2 LIMIT 10",
    "MATCH (p:Person) WHERE id(p) < 6 RETURN id(p) AS id, p.name AS name ORDER BY id",
    "MATCH (p:Person) WHERE p.age = 5 RETURN properties(p) AS props ORDER BY p.name",
    "MATCH (p:Person)<-[:KNOWS]-(q) WITH p, count(q) AS indeg \
     RETURN indeg, count(p) AS n ORDER BY indeg",
    "CALL db.index.fulltext.queryNodes('posts', 'engine') YIELD node \
     RETURN count(node) AS n",
    "CALL db.index.fulltext.queryNodes('posts', 'quasar nebula') YIELD node \
     RETURN node.body AS body ORDER BY body LIMIT 10",
    "CALL engram.algo.wcc.stream({nodeLabels: ['Person'], relationshipTypes: ['KNOWS']}) \
     YIELD nodeId, componentId RETURN count(DISTINCT componentId) AS components",
    "CALL engram.algo.pagerank.stream({nodeLabels: ['Person'], relationshipTypes: ['KNOWS']}) \
     YIELD nodeId, score RETURN nodeId, score ORDER BY nodeId LIMIT 25",
    "CALL db.labels() YIELD label RETURN label ORDER BY label",
    "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey ORDER BY propertyKey",
];

fn battery(g: &Graph) -> Vec<String> {
    BATTERY.iter().map(|q| run(g, q)).collect()
}

fn assert_same(phase: &str, tenant: &str, got: &[String], alone: &[String]) {
    for ((q, got), alone) in BATTERY.iter().zip(got).zip(alone) {
        assert_eq!(
            got, alone,
            "{phase}: tenant {tenant} answered differently on a shared store than alone\n\
             query: {q}"
        );
    }
}

fn solo(t: Tenant) -> Graph {
    let g = Graph::new(Store::new(), t.coord.0, t.coord.1);
    for step in build_steps(t) {
        step(&g);
    }
    g
}

fn open(store: &Store, t: Tenant) -> Graph {
    let g = Graph::new(store.clone(), t.coord.0, t.coord.1);
    g.set_persist_derived(true);
    g
}

/// The quiescent maintenance tick, for both tenants into ONE directory: every
/// declared range index, and the derived bases.
fn maintenance_tick(dir: &std::path::Path, graphs: &[&Graph]) {
    for g in graphs {
        let props = g.declared_index_props();
        let refs: Vec<&str> = props.iter().map(String::as_str).collect();
        g.persist_indexes(dir, &refs).expect("persist indexes");
        g.persist_derived_at_stop(dir, 0);
    }
}

fn sidecars(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".idx") || n.ends_with(".dsc"))
        .collect();
    v.sort();
    v
}

fn two_tenants_answer_as_they_do_alone(tag: &str, a: Tenant, b: Tenant) {
    let (alone_a, alone_b) = (solo(a), solo(b));
    let (want_a, want_b) = (battery(&alone_a), battery(&alone_b));
    assert_ne!(want_a, want_b, "{tag}: the tenants' corpora must differ, or nothing is tested");

    // ── Resident: both tenants built interleaved on one store. ──────────────
    let store = Store::new();
    let (ga, gb) = (open(&store, a), open(&store, b));
    for (sa, sb) in build_steps(a).into_iter().zip(build_steps(b)) {
        sa(&ga);
        sb(&gb);
    }
    assert_eq!(
        ga.prop_token_peek("name"),
        gb.prop_token_peek("age"),
        "{tag}: the premise — A's `name` and B's `age` share a token number"
    );
    assert_ne!(
        ga.prop_token_peek("name"),
        gb.prop_token_peek("name"),
        "{tag}: and the same NAME has different numbers in the two tenants"
    );
    let ids = "MATCH (p:Person) RETURN min(id(p)) AS lo, max(id(p)) AS hi";
    assert_eq!(run(&ga, ids), run(&gb, ids), "{tag}: the premise — node ids collide");
    assert_same(&format!("{tag}/resident"), "A", &battery(&ga), &want_a);
    assert_same(&format!("{tag}/resident"), "B", &battery(&gb), &want_b);

    // ── Paged, then a maintenance tick writing BOTH tenants' sidecars into one
    //    directory, then a restart that adopts them. ─────────────────────────
    let dir = TmpDir::new(tag);
    store.seal();
    let _ = store.into_paged(dir.path(), 1 << 20).expect("into_paged");
    maintenance_tick(dir.path(), &[&ga, &gb]);
    let on_disk = sidecars(dir.path());
    assert!(
        on_disk.iter().filter(|n| n.ends_with(".idx")).count() >= 4,
        "{tag}: both tenants' declared indexes are on disk side by side: {on_disk:?}"
    );
    assert_eq!(
        on_disk.iter().filter(|n| n.ends_with(".dsc")).count(),
        2,
        "{tag}: both tenants' derived bases are on disk side by side: {on_disk:?}"
    );
    drop((ga, gb));
    drop(store);

    let (store, _cache) = Store::open_paged_dir(dir.path(), 1 << 20).expect("reopen");
    let (ga, gb) = (open(&store, a), open(&store, b));
    ga.adopt_derived_sidecar(dir.path());
    gb.adopt_derived_sidecar(dir.path());
    assert_same(&format!("{tag}/restarted"), "A", &battery(&ga), &want_a);
    assert_same(&format!("{tag}/restarted"), "B", &battery(&gb), &want_b);

    // ── A second tick and restart: sidecars written by a reopened store. ────
    maintenance_tick(dir.path(), &[&ga, &gb]);
    drop((ga, gb));
    drop(store);
    let (store, _cache) = Store::open_paged_dir(dir.path(), 1 << 20).expect("reopen again");
    let (ga, gb) = (open(&store, a), open(&store, b));
    ga.adopt_derived_sidecar(dir.path());
    gb.adopt_derived_sidecar(dir.path());
    assert_same(&format!("{tag}/restarted twice"), "A", &battery(&ga), &want_a);
    assert_same(&format!("{tag}/restarted twice"), "B", &battery(&gb), &want_b);

    // ── A write in B moves B's answers, exactly as it moves B-alone's, and
    //    moves none of A's. ──────────────────────────────────────────────────
    let late = "CREATE (:Person {name: 'b-late', age: 7})";
    run(&gb, late);
    run(&alone_b, late);
    let want_b_after = battery(&alone_b);
    assert_ne!(want_b_after, want_b, "{tag}: the write is visible to B-alone");
    assert_same(&format!("{tag}/after B wrote"), "B", &battery(&gb), &want_b_after);
    assert_same(&format!("{tag}/after B wrote"), "A", &battery(&ga), &want_a);
}

#[test]
fn two_realms_on_one_store_answer_exactly_as_each_does_alone() {
    two_tenants_answer_as_they_do_alone(
        "realms",
        A((Realm(1), Namespace(1))),
        B((Realm(2), Namespace(1))),
    );
}

#[test]
fn two_namespaces_of_one_realm_answer_exactly_as_each_does_alone() {
    two_tenants_answer_as_they_do_alone(
        "namespaces",
        A((Realm(1), Namespace(1))),
        B((Realm(1), Namespace(2))),
    );
}
