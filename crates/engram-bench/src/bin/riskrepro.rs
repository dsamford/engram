//! The risk-by-country wall, reproduced at production scale: 430 countries,
//! 27k events (a tenth in the recency window), 23k sanctions, ~1k OCCURS_IN
//! rels. Times the exact statement shape that measured ~50 minutes per
//! sample on the port pod before the clause-scan memo.
//!
//! A BENCH BINARY: wall-clock reads are the point, so the determinism
//! lints are waived here exactly as in every other engram-bench binary.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn main() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_wall_ms(1_755_600_000_000); // fixed now, as portbench sets it
    g.set_bulk_ingest(true).expect("bulk on");
    let load = std::time::Instant::now();
    let mut countries = Vec::new();
    for i in 0..430 {
        let mut props = BTreeMap::new();
        props.insert(
            "iso3".to_string(),
            engram_cypher::Value::Str(format!("C{i:03}")),
        );
        countries.push(g.create_node(&["Country".into()], &props).expect("country"));
    }
    // Events: ~10% inside the window; every 9th carries regionId; every 27th
    // gets an OCCURS_IN rel to a country.
    for i in 0..27_000i64 {
        let recent = i % 10 == 0;
        let t = if recent {
            1_755_000_000_000i64
        } else {
            1_600_000_000_000
        };
        let mut props = BTreeMap::new();
        props.insert("eventTime".to_string(), engram_cypher::Value::Int(t));
        if i % 9 == 0 {
            props.insert(
                "regionId".to_string(),
                engram_cypher::Value::Str(format!("C{:03}", i % 430)),
            );
        }
        props.insert(
            "description".to_string(),
            engram_cypher::Value::Str(format!(
                "event {i} with a description of plausible length for a geopolitical event record"
            )),
        );
        let id = g
            .create_node(&["GeopoliticalEvent".into()], &props)
            .expect("event");
        if i % 27 == 0 {
            let c = countries[(i as usize) % countries.len()];
            g.create_rel(id, "OCCURS_IN", c, &BTreeMap::new())
                .expect("rel");
        }
    }
    for i in 0..23_000i64 {
        let mut props = BTreeMap::new();
        props.insert(
            "targetCountryIso3".to_string(),
            engram_cypher::Value::Str(format!("C{:03}", i % 500)),
        );
        g.create_node(&["Sanction".into()], &props)
            .expect("sanction");
    }
    // Production ratio: the labels above are ~1.5% of the node population.
    // Filler nodes enlarge the label-set column every batch assembly walks.
    for i in 0..500_000i64 {
        let mut props = BTreeMap::new();
        props.insert("k".to_string(), engram_cypher::Value::Int(i));
        g.create_node(&["Filler".into()], &props).expect("filler");
    }
    g.set_bulk_ingest(false).expect("bulk exit");
    eprintln!("[repro] loaded in {:?}", load.elapsed());

    let params: BTreeMap<String, engram_cypher::Value> = BTreeMap::new();
    let stmt = "MATCH (country:Country) \
        OPTIONAL MATCH (e:GeopoliticalEvent) \
          WHERE coalesce(e.eventTime, e.startAt) >= 1750000000000 \
            AND (exists((e)-[:OCCURS_IN]->(country)) OR e.regionId = country.iso3) \
        OPTIONAL MATCH (s:Sanction) WHERE s.targetCountryIso3 = country.iso3 \
        RETURN country.iso3 AS iso3, count(DISTINCT e) AS geoEvents, count(DISTINCT s) AS sanctions \
        LIMIT 10";
    for (name, sub) in [
        (
            "events-half",
            "MATCH (country:Country)              OPTIONAL MATCH (e:GeopoliticalEvent)                WHERE coalesce(e.eventTime, e.startAt) >= 1750000000000                  AND (exists((e)-[:OCCURS_IN]->(country)) OR e.regionId = country.iso3)              RETURN country.iso3 AS iso3, count(DISTINCT e) AS geoEvents LIMIT 10",
        ),
        (
            "sanctions-half",
            "MATCH (country:Country)              OPTIONAL MATCH (s:Sanction) WHERE s.targetCountryIso3 = country.iso3              RETURN country.iso3 AS iso3, count(DISTINCT s) AS sanctions LIMIT 10",
        ),
        (
            "events-no-exists",
            "MATCH (country:Country)              OPTIONAL MATCH (e:GeopoliticalEvent)                WHERE coalesce(e.eventTime, e.startAt) >= 1750000000000                  AND e.regionId = country.iso3              RETURN country.iso3 AS iso3, count(DISTINCT e) AS geoEvents LIMIT 10",
        ),
    ] {
        let q = parse_statement(sub).expect("parse");
        let t = std::time::Instant::now();
        let r = run_query(&g, &q, params.clone()).expect("run");
        eprintln!("[repro] {name}: {:?}, {} rows", t.elapsed(), r.rows.len());
    }
    // The full statement (the candidate-batch admission lever it used to
    // toggle is retired — fix 61; the seed binds lean from label columns).
    {
        let q = parse_statement(stmt).expect("parse");
        let t = std::time::Instant::now();
        let r = run_query(&g, &q, params.clone()).expect("run");
        eprintln!(
            "[repro] risk-by-country: {:?}, {} rows",
            t.elapsed(),
            r.rows.len()
        );
    }
    // Production stores are COMPACTED (278 column blocks + segments on the
    // port); the memtable-only repro was hiding whatever per-probe cost the
    // k-way visitor adds. Compact here so the census walks the real shape.
    if std::env::var("REPRO_COMPACT").is_ok() {
        let t = std::time::Instant::now();
        g.shared_store().seal();
        g.shared_store().compact();
        eprintln!("[repro] compacted in {:?}", t.elapsed());
    }
    // R8 slice 1, the columnar aggregate scan vs the general path on the
    // same shape (an aggregating WITH declines the scan): an all-nodes
    // IS NOT NULL count and a keyed histogram over the filler column.
    for (name, src) in [
        (
            "isnotnull-count BATCH",
            "MATCH (n) WHERE n.k IS NOT NULL RETURN count(n)",
        ),
        (
            "isnotnull-count GENERAL",
            "MATCH (n) WHERE n.k IS NOT NULL WITH count(n) AS c RETURN c",
        ),
        (
            "keyed-histogram BATCH",
            "MATCH (n:Filler) WHERE n.k >= 0 RETURN n.k % 7 AS b, count(*) ORDER BY b LIMIT 3",
        ),
    ] {
        let q = parse_statement(src).expect("parse");
        let t = std::time::Instant::now();
        let r = run_query(&g, &q, params.clone()).expect("run");
        eprintln!(
            "[repro] {name}: {:?}, first {:?}",
            t.elapsed(),
            r.rows.first()
        );
    }
    // The degree-histogram census shape over every node (~550k here): the
    // statement that measured 670 s on the port pod at rev 47, whose cost
    // is carrying n through WITH n, d — the liveness target.
    let census = "MATCH (n) WITH n, count { (n)--() } AS d         WITH d ORDER BY d WITH collect(d) AS ds         RETURN ds[toInteger(size(ds) * 0.50)] AS p50, ds[size(ds) - 1] AS max";
    let q = parse_statement(census).expect("parse");
    let t = std::time::Instant::now();
    let (r, trace) = engram_observe::with_trace(|| run_query(&g, &q, params.clone()).expect("run"));
    eprintln!(
        "[repro] degree-histogram: {:?}, {:?}, projected mats {:?}",
        t.elapsed(),
        r.rows.first(),
        trace
            .counters()
            .get("graph.projected node materialisations")
    );
    let q = parse_statement(stmt).expect("parse");
    let t = std::time::Instant::now();
    let (r, trace) = engram_observe::with_trace(|| run_query(&g, &q, params).expect("run"));
    for (k, v) in trace.counters() {
        if k.starts_with("interp.") || k.starts_with("graph.") {
            eprintln!("[repro]   {k} = {v}");
        }
    }
    eprintln!(
        "[repro] risk-by-country: {:?}, {} rows, first: {:?}",
        t.elapsed(),
        r.rows.len(),
        r.rows.first()
    );
}
