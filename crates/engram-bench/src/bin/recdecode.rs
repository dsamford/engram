#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! Where does a FULL-RECORD listing spend its time?
//!
//! On the production mirror `MATCH (r:ManagedRepo) RETURN properties(r)`
//! (143 records, ~32 properties each) answers in 7.6 ms against Neo4j's
//! 4.2, and the traced decomposition (v109) put it in the per-record CPU
//! path, not in the store: `count(properties(r))` — decode, no encode —
//! 4.1 ms; a bare `RETURN r` 5.9; three projected properties 1.3. That is
//! ~24 µs per full decode and ~13 µs per node encode.
//!
//! This bin times each stage in isolation on a paged store holding a
//! ManagedRepo-shaped label (32 mixed properties, fillers interleaved so
//! the label is sparse in the id space), so a lever is chosen from a
//! number rather than from a reading of the code:
//!
//! | stage | what it measures |
//! |---|---|
//! | `get`        | the store read alone (`store_get_peek` through the block cache) |
//! | `node`       | `Graph::node` — read + `Record::decode` + 32 token names + 32 values into a `BTreeMap` |
//! | `projected3` | `Graph::node_projected` for three properties |
//! | `clone`      | `properties(r)` — one clone of the decoded map |
//! | `encode`     | packstream encode of the full `Value::Node` |
//! | `query-*`    | `run_query` end to end (no Bolt): `RETURN id(r)`, `RETURN r`, `RETURN properties(r)`, `RETURN r.repoId` |
//!
//! ```text
//! recdecode [records=143] [iters=200]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn record(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let s = |k: &str, v: String| (k.to_string(), Value::Str(v));
    for (k, v) in [
        s("repoId", format!("lore-e2e-{i:08x}{}", "a".repeat(20))),
        s("fullName", format!("org-{}/repo-{i}", i % 9)),
        s("orgId", format!("org-{}", i % 9)),
        s("provider", if i % 4 == 0 { "github".into() } else { "lore".into() }),
        s("syncMode", if i % 3 == 0 { "push".into() } else { "onboarding".into() }),
        s("autonomyLevel", if i % 6 == 1 { "unmanaged".into() } else { "assisted".into() }),
        s("defaultBranch", "main".into()),
        s("cloneUrl", format!("scm://scm.example.net:41337/{i:032x}")),
        s("htmlUrl", format!("https://github.example.com/org-{}/repo-{i}", i % 9)),
        s("loreProjectId", format!("{i:08x}-b338-4020-ba04-31e41079{i:04x}")),
        s("loreRepoId", format!("{i:032x}")),
        s("__mid", format!("4:92c0b34c-{i:04x}-4a5b-8c6d-{i:012x}:{i}")),
        s("createdAt", "2026-08-20T14:03:11.412Z".into()),
        s("updatedAt", "2026-09-04T22:17:45.001Z".into()),
        s("lastSyncedAt", "2026-09-04T22:17:45.001Z".into()),
        s("lastMirrorSha", format!("{i:040x}")),
        s("mirrorStatus", "current".into()),
        s("kind", "service".into()),
        s("visibility", "private".into()),
        s("language", "TypeScript".into()),
        s("description", format!("Repository {i}: {}", "lorem ipsum dolor sit amet ".repeat(4))),
        s("installationId", format!("{}", 1_000_000 + i)),
        s("webhookSecretRef", format!("secret/webhook-{i}")),
        s("ownerUserId", format!("{i:08x}-933d-472b-9d3a-5d2408e8a06b")),
        s("settings", format!("{{\"ci\":true,\"lanes\":[\"native\",\"github\"],\"deadlineMin\":45,\"repo\":{i}}}")),
    ] {
        m.insert(k, v);
    }
    m.insert("pathAllowlist".to_string(), Value::List(Vec::new()));
    m.insert("archived".to_string(), Value::Bool(i % 17 == 0));
    m.insert("mirrorEnabled".to_string(), Value::Bool(i % 5 != 0));
    m.insert("protected".to_string(), Value::Bool(true));
    m.insert("fork".to_string(), Value::Bool(false));
    m.insert("stars".to_string(), Value::Int(i % 40));
    m.insert("openIssues".to_string(), Value::Int(i % 7));
    m
}

fn corpus(records: i64) -> (Graph, Vec<u64>, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ids = Vec::new();
    for i in 0..records {
        ids.push(
            g.create_node(&["Repo".into(), "ManagedRepo".into()], &record(i))
                .expect("repo"),
        );
        for k in 0..40i64 {
            let mut f = BTreeMap::new();
            f.insert("other".to_string(), Value::Str(format!("filler-{i}-{k}-{}", "x".repeat(60))));
            f.insert("n".to_string(), Value::Int(k));
            g.create_node(&["Filler".into()], &f).expect("filler");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_recdecode_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 256 * 1024 * 1024).expect("into_paged");
    (Graph::new(store.clone(), Realm(1), Namespace(1)), ids, dir)
}

fn time<T>(label: &str, per: usize, iters: usize, mut f: impl FnMut() -> T) -> T {
    // Warm once (the first paged read is cold), then time `iters` rounds.
    let mut out = f();
    let t0 = Instant::now();
    for _ in 0..iters {
        out = f();
    }
    let el = t0.elapsed();
    let per_round = el.as_secs_f64() * 1e3 / iters as f64;
    println!(
        "{label:<14} {per_round:8.3} ms per round   {:8.2} µs per record",
        per_round * 1e3 / per as f64
    );
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let records: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(143);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let (g, ids, dir) = corpus(records);
    let n = ids.len();
    println!("records {n}, {} properties each, iters {iters}, paged store {}", record(0).len(), dir.display());

    // The store read plus the smallest decode there is — labels only, no
    // property named or copied (`node_projected` with nothing wanted).
    let none: BTreeSet<String> = BTreeSet::new();
    time("get+labels", n, iters, || {
        let mut out = Vec::with_capacity(n);
        for &id in &ids {
            out.push(g.node_projected(id, &none).expect("node").expect("present"));
        }
        out
    });
    // One traced round: how many blocks / segments a point read touches.
    let (_, trace) = engram_observe::with_trace(|| {
        for &id in &ids {
            let _ = g.node_projected(id, &none);
        }
    });
    for (k, v) in trace.counters() {
        if k.starts_with("paged.") || k.starts_with("store.") || k.contains("segment") {
            println!("   traced round: {v:>8}  {k}");
        }
    }
    let nodes = time("node", n, iters, || {
        let mut out = Vec::with_capacity(n);
        for &id in &ids {
            out.push(g.node(id).expect("node").expect("present"));
        }
        out
    });
    let want: BTreeSet<String> = ["repoId", "fullName", "provider"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    time("projected3", n, iters, || {
        let mut out = Vec::with_capacity(n);
        for &id in &ids {
            out.push(g.node_projected(id, &want).expect("node").expect("present"));
        }
        out
    });
    time("clone", n, iters, || {
        let mut out = Vec::with_capacity(n);
        for v in &nodes {
            if let Value::Node { props, .. } = v {
                out.push(Value::Map(props.clone()));
            }
        }
        out
    });
    time("encode", n, iters, || {
        let mut buf = Vec::with_capacity(64 * 1024);
        for v in &nodes {
            engram_bolt::packstream::encode_value(v, &mut buf).expect("encode");
        }
        buf.len()
    });
    for (label, src) in [
        ("query-id", "MATCH (r:ManagedRepo) RETURN id(r) AS id"),
        ("query-bare", "MATCH (r:ManagedRepo) RETURN r"),
        ("query-props", "MATCH (r:ManagedRepo) RETURN properties(r) AS r"),
        ("query-count", "MATCH (r:ManagedRepo) RETURN count(properties(r)) AS n"),
        ("query-3props", "MATCH (r:ManagedRepo) RETURN r.repoId AS id, r.fullName AS name, r.provider AS p"),
        ("query-2label", "MATCH (r:Repo:ManagedRepo) RETURN properties(r) AS r"),
    ] {
        let q = parse_statement(src).expect("parse");
        time(label, n, iters, || {
            run_query(&g, &q, BTreeMap::new()).expect("run").rows.len()
        });
    }
    let _ = std::fs::remove_dir_all(&dir);
}
