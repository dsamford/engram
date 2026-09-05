//! What does a read/write mix actually DO, per operation?
//!
//! Not an assertion about one suspected cache — a CENSUS. It runs an
//! SNB-shaped mix and prints every counter the engine recorded, normalised per
//! write, largest first.
//!
//! This exists because guessing has a bad record on this subsystem. Two
//! separate write-amplification defects have now been found here, and in both
//! cases the mechanism named by reading the code was the wrong one: the first
//! was blamed on label-membership invalidation (a counter trace showed
//! `membership snapshots built` never fired at all), and the second was assumed
//! to be scoped to a label when the index turned out to be keyed on the
//! property token alone. A census cannot be misled that way — whatever is
//! running N times per write appears at the top of the list with N in front
//! of it.
//!
//! The shape mirrors the LDBC SNB Interactive insert the stress harness runs:
//! a new message ATTACHED to an existing indexed person, not a free-floating
//! node. The attachment matters — it is what makes the write touch adjacency
//! as well as node storage.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn snb_shaped(persons: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for stmt in [
        "CREATE INDEX p_id FOR (n:Person) ON (n.id)",
        "CREATE INDEX m_id FOR (n:Message) ON (n.id)",
    ] {
        run_stmt(&g, &parse_any(stmt).expect("parse index"), BTreeMap::new())
            .expect("create index");
    }
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:Person {{id: i}})", persons - 1),
    );
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (p:Person {{id: i / 20}}) \
             CREATE (m:Message:Comment {{id: i}})-[:HAS_CREATOR]->(p)",
            persons * 20 - 1
        ),
    );
    g
}

/// Print every counter, per write, for a 5%-write mix — the profile whose
/// throughput the stress harness measured collapsing.
#[test]
fn census_of_a_read_heavy_mix() {
    let g = snb_shaped(2_000);
    // Warm every cache so first-touch costs are not attributed to the mix.
    run(&g, "MATCH (p:Person {id: 5}) RETURN p.id");
    run(&g, "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {id: 5}) RETURN m.id LIMIT 25");

    const WRITES: u64 = 20;
    const READS_PER_WRITE: u64 = 19; // 5% writes
    let ((), trace) = engram_observe::with_trace(|| {
        for w in 0..WRITES {
            for r in 0..READS_PER_WRITE {
                let k = (w * READS_PER_WRITE + r) % 2_000;
                run(&g, &format!("MATCH (p:Person {{id: {k}}}) RETURN p.id"));
            }
            run(
                &g,
                &format!(
                    "MATCH (p:Person {{id: {}}}) \
                     CREATE (m:Message:Comment {{id: {}}})-[:HAS_CREATOR]->(p)",
                    w % 2_000,
                    900_000 + w
                ),
            );
        }
    });

    let mut rows: Vec<(&String, u64)> = trace.counters().iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    eprintln!(
        "\ncounters for {WRITES} writes + {} reads (per write in brackets):",
        WRITES * READS_PER_WRITE
    );
    for (name, n) in rows.iter().take(30) {
        eprintln!("  {n:>9}  [{:>7.1}/w]  {name}", *n as f64 / WRITES as f64);
    }

    // The only assertion: the census actually observed something. What it
    // observed is for a human to read — turning any individual line into a
    // threshold here would freeze today's implementation into the test.
    assert!(
        !rows.is_empty(),
        "no counters recorded — the trace was not installed, so this census is blind"
    );
}

/// The census for the CONTENTION shape: repeated `SET` on one hot node, mixed
/// with reads that scan a label.
///
/// The stress harness measured this profile getting WORSE while every other
/// profile improved. Under it, an indexed point lookup stayed at 0.14 ms while
/// a label-scanning aggregate went from 0.25 ms to 120 ms — 480x — which is the
/// signature of a whole-collection cache being invalidated by a write that
/// changed nothing it holds. `SET n.hits` alters no label membership.
#[test]
fn census_of_hot_key_updates_mixed_with_label_scans() {
    let g = snb_shaped(2_000);
    run(&g, "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) RETURN c.name, count(p) AS n");

    const ROUNDS: u64 = 20;
    let ((), trace) = engram_observe::with_trace(|| {
        for _ in 0..ROUNDS {
            run(&g, "MATCH (p:Person {id: 0}) SET p.hits = coalesce(p.hits, 0) + 1");
            run(
                &g,
                "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) RETURN c.name, count(p) AS n",
            );
        }
    });

    let mut rows: Vec<(&String, u64)> = trace.counters().iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    eprintln!("\ncounters for {ROUNDS} hot SET + label-scan rounds (per round in brackets):");
    for (name, n) in rows.iter().take(30) {
        eprintln!("  {n:>9}  [{:>7.1}/r]  {name}", *n as f64 / ROUNDS as f64);
    }
    assert!(!rows.is_empty(), "no counters recorded — the census is blind");
}

/// A richer SNB-shaped corpus for the balanced census: persons in cities,
/// a KNOWS ring, messages with creators and tags. Enough structure for every
/// read shape the stress harness issues to be meaningful.
fn snb_full(persons: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for stmt in [
        "CREATE INDEX p_id FOR (n:Person) ON (n.id)",
        "CREATE INDEX m_id FOR (n:Message) ON (n.id)",
    ] {
        run_stmt(&g, &parse_any(stmt).expect("parse index"), BTreeMap::new())
            .expect("create index");
    }
    run(&g, "UNWIND range(0, 99) AS i CREATE (:City {id: i, name: 'City' + toString(i)})");
    run(&g, "UNWIND range(0, 49) AS i CREATE (:Tag {id: i, name: 'Tag' + toString(i)})");
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (c:City {{id: i % 100}}) \
             CREATE (:Person {{id: i, name: 'P' + toString(i)}})-[:IS_LOCATED_IN]->(c)",
            persons - 1
        ),
    );
    // KNOWS: each person knows the next 8 (mod n), both directions stored once.
    for k in 1..=8i64 {
        run(
            &g,
            &format!(
                "UNWIND range(0, {}) AS i MATCH (a:Person {{id: i}}), \
                 (b:Person {{id: (i + {k}) % {persons}}}) CREATE (a)-[:KNOWS]->(b)",
                persons - 1
            ),
        );
    }
    run(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (p:Person {{id: i / 10}}), (t:Tag {{id: i % 50}}) \
             CREATE (m:Message:Comment {{id: i}})-[:HAS_CREATOR]->(p), (m)-[:HAS_TAG]->(t)",
            persons * 10 - 1
        ),
    );
    g
}

const BALANCED_SHAPES: &[(&str, &str)] = &[
    ("is1-profile", "MATCH (p:Person {id: 7}) RETURN p.name"),
    ("is3-friends", "MATCH (p:Person {id: 7})-[:KNOWS]-(f:Person) RETURN f.id LIMIT 25"),
    ("ic-foaf", "MATCH (p:Person {id: 7})-[:KNOWS]-()-[:KNOWS]-(f:Person) RETURN count(DISTINCT f) AS c"),
    ("is5-by-creator", "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {id: 7}) RETURN m.id LIMIT 25"),
    ("agg-by-city", "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) RETURN c.name, count(p) AS n ORDER BY n DESC LIMIT 10"),
    ("ic6-friend-tags", "MATCH (p:Person {id: 7})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) MATCH (m)-[:HAS_TAG]->(t:Tag) RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10"),
];

/// The BALANCED census: what does each read shape cost when every read is
/// preceded by a write, against what it costs alone — and which counters
/// account for the difference?
///
/// The stress harness measured the balanced profile at 344–739 ops/s against
/// read-only at 809–3,757 and write-only at 4,298–4,596: mixing reads and
/// writes costs several times more than either alone. The incumbent's
/// balanced number sits between its read-only and write-only, i.e. its reads
/// and writes do not interfere. Ours do, and this census says where.
///
/// Engine only — no server, no clients, one thread — so whatever slows down
/// here is engine interference (a cache invalidated, a structure repaired),
/// not lock scheduling across workers. That split matters: they need
/// different fixes.
#[test]
fn census_of_a_balanced_mix_per_shape() {
    let g = snb_full(2_000);
    for (_, q) in BALANCED_SHAPES {
        run(&g, q); // warm every cache the shape uses
    }
    let write = |i: u64| {
        format!(
            "MATCH (p:Person {{id: {}}}) \
             CREATE (m:Message:Comment {{id: {}, content: 'x'}})-[:HAS_CREATOR]->(p)",
            i % 2_000,
            700_000 + i
        )
    };
    const N: u64 = 30;
    eprintln!("\n{:<18} {:>10} {:>10} {:>7}", "shape", "alone ms", "mixed ms", "ratio");
    let mut wi = 0u64;
    for (name, q) in BALANCED_SHAPES {
        // Alone: N reads back to back.
        let t = std::time::Instant::now();
        for _ in 0..N {
            run(&g, q);
        }
        let alone = t.elapsed().as_secs_f64() * 1e3 / N as f64;
        // Mixed: a write, then the read, N times. Only the READ is timed.
        let mut mixed_total = 0.0;
        let ((), trace) = engram_observe::with_trace(|| {
            for _ in 0..N {
                run(&g, &write(wi));
                wi += 1;
                let t = std::time::Instant::now();
                run(&g, q);
                mixed_total += t.elapsed().as_secs_f64() * 1e3;
            }
        });
        let mixed = mixed_total / N as f64;
        eprintln!(
            "{:<18} {:>10.3} {:>10.3} {:>6.1}x",
            name,
            alone,
            mixed,
            mixed / alone.max(1e-9)
        );
        // The counters that fired per (write + read) pair, largest first,
        // filtered to the cache/rebuild family — the rest is the statements'
        // own work.
        let mut rows: Vec<(&String, u64)> = trace
            .counters()
            .iter()
            .filter(|(k, _)| {
                k.contains("built")
                    || k.contains("rebuild")
                    || k.contains("repaired")
                    || k.contains("caught up")
                    || k.contains("still current")
                    || k.contains("reused")
                    || k.contains("snapshots")
                    || k.contains("index.")
                    || k.contains("visitor scans")
            })
            .map(|(k, v)| (k, *v))
            .collect();
        rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
        for (k, v) in rows.iter().take(8) {
            eprintln!("    {:>8.1}/pair  {k}", *v as f64 / N as f64);
        }
    }
    assert!(wi > 0, "no writes were issued — the census compared nothing");
}

/// The balanced census's headline finding, pinned: after a message insert
/// (a node write plus a `HAS_CREATOR` edge), the city aggregate must serve
/// EVERY hop from its `IS_LOCATED_IN` table. Before the derived-structure
/// protocol the probe gate reset on the commit clock and the first 1,024
/// hops opened a visitor scan each — 1,024 scans + 976 reuses per aggregate,
/// the single largest cost in the balanced head-to-head.
#[test]
fn a_message_insert_leaves_the_city_aggregates_adjacency_table_serving_every_hop() {
    let g = snb_full(2_000);
    let agg = BALANCED_SHAPES
        .iter()
        .find(|(n, _)| *n == "agg-by-city")
        .map(|(_, q)| *q)
        .expect("shape");
    run(&g, agg);
    run(&g, agg);
    let ((), alone) = engram_observe::with_trace(|| run(&g, agg));
    let reused_alone = alone
        .counters()
        .get("graph.adjacency tables reused")
        .copied()
        .unwrap_or(0);
    assert!(reused_alone >= 2_000, "warm: every person's hop from the table: {:?}", alone.counters());

    let ((), mixed) = engram_observe::with_trace(|| {
        run(
            &g,
            "MATCH (p:Person {id: 7}) \
             CREATE (m:Message:Comment {id: 700001, content: 'x'})-[:HAS_CREATOR]->(p)",
        );
        run(&g, agg);
    });
    let c = mixed.counters();
    assert_eq!(c.get("graph.adjacency tables built"), None, "{c:?}");
    assert_eq!(c.get("graph.adjacency tables repaired"), None, "{c:?}");
    assert!(
        c.get("graph.adjacency tables reused").copied().unwrap_or(0) >= reused_alone,
        "after the insert the aggregate must reuse the table on every hop: {c:?}"
    );
    let scans: u64 = c
        .iter()
        .filter(|(k, _)| k.contains("visitor scans"))
        .map(|(_, v)| *v)
        .sum();
    let scans_alone: u64 = alone
        .counters()
        .iter()
        .filter(|(k, _)| k.contains("visitor scans"))
        .map(|(_, v)| *v)
        .sum();
    assert!(
        scans <= scans_alone + 8,
        "the read after a write opened {scans} visitor scans against {scans_alone} alone: {c:?}"
    );
}

/// The same census for a WRITE-ONLY run, to separate what a write costs from
/// what a read following a write costs.
///
/// A counter that scales with writes appears in both; one that only appears in
/// the mixed census above is a read paying for a write, which is the shape of
/// an invalidation rather than of the write's own work.
#[test]
fn census_of_writes_alone() {
    let g = snb_shaped(2_000);
    run(&g, "MATCH (p:Person {id: 5}) RETURN p.id");

    const WRITES: u64 = 20;
    let ((), trace) = engram_observe::with_trace(|| {
        for w in 0..WRITES {
            run(
                &g,
                &format!(
                    "MATCH (p:Person {{id: {}}}) \
                     CREATE (m:Message:Comment {{id: {}}})-[:HAS_CREATOR]->(p)",
                    w % 2_000,
                    800_000 + w
                ),
            );
        }
    });

    let mut rows: Vec<(&String, u64)> = trace.counters().iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    eprintln!("\ncounters for {WRITES} writes, no reads (per write in brackets):");
    for (name, n) in rows.iter().take(30) {
        eprintln!("  {n:>9}  [{:>7.1}/w]  {name}", *n as f64 / WRITES as f64);
    }
    assert!(!rows.is_empty(), "no counters recorded — the census is blind");
}
