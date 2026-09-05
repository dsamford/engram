#![allow(non_snake_case)]
//! The FULL LDBC IC3 (the 5-stage stretch) — the hardest interactive-complex
//! query. It composes EVERY IC3 primitive built this session: a node-carry
//! prelude (`person`, `countryX`, `countryY`) with `LIMIT 1`, a seed-filtered
//! traversal start, a collect-list prelude (`cities`), node-identity membership
//! (`country IN [countryX, countryY]`, `NOT city IN cities`), a node-identity
//! CASE (`country = countryX`), a varlen-then-fixed split, and the general
//! N-stage pipeline (`sum(CASE …)` + HAVING + `xCount + yCount`). Byte-identical
//! to the interp, and fires the columnar N-stage pipeline (does not stream).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, props: &[(&str, Value)]| {
        let mut m = BTreeMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), v.clone());
        }
        g.create_node(&[label.into()], &m).expect("node")
    };
    let country0 = mk("Country", &[("name", Value::Str("Country0".into()))]);
    let country1 = mk("Country", &[("name", Value::Str("Country1".into()))]);
    let country2 = mk("Country", &[("name", Value::Str("Country2".into()))]);
    let city = |country: u64| {
        let c = mk("City", &[]);
        g.create_rel(c, "IS_PART_OF", country, &BTreeMap::new())
            .unwrap();
        c
    };
    let city_a = city(country0); // in cities (Country0)
    let _city_b = city(country1); // in cities (Country1) — no friend located here
    let city_c = city(country2); // NOT in cities (Country2)
    let person = |id: i64, first: &str, last: &str, loc: u64| {
        let p = mk(
            "Person",
            &[
                ("id", Value::Int(id)),
                ("firstName", Value::Str(first.into())),
                ("lastName", Value::Str(last.into())),
            ],
        );
        g.create_rel(p, "IS_LOCATED_IN", loc, &BTreeMap::new())
            .unwrap();
        p
    };
    let root = person(10, "Root", "Zero", city_c);
    let f1 = person(11, "Ana", "One", city_c); // located outside → kept
    let f2 = person(12, "Bob", "Two", city_c); // located outside → kept (but yCount 0)
    let f3 = person(13, "Cid", "Three", city_a); // located IN Country0 → excluded
    g.create_rel(root, "KNOWS", f1, &BTreeMap::new()).unwrap();
    g.create_rel(root, "KNOWS", f3, &BTreeMap::new()).unwrap();
    g.create_rel(f1, "KNOWS", f2, &BTreeMap::new()).unwrap(); // f2 is 2 hops
    // A message by `creator`, located in `country`, at `date`.
    let message = |creator: u64, country: u64, date: i64| {
        let msg = mk("Comment", &[("creationDate", Value::Int(date))]);
        g.create_rel(msg, "HAS_CREATOR", creator, &BTreeMap::new())
            .unwrap();
        g.create_rel(msg, "IS_LOCATED_IN", country, &BTreeMap::new())
            .unwrap();
    };
    // In-range date (1293840000000 <= d < 1325376000000).
    let d = 1_300_000_000_000i64;
    message(f1, country0, d); // f1 xCount
    message(f1, country0, d); // f1 xCount
    message(f1, country1, d); // f1 yCount → f1: x=2, y=1 → KEEP
    message(f2, country0, d); // f2 xCount only → y=0 → DROP
    // An out-of-range message must not count.
    message(f1, country1, 999); // date < lower bound → ignored
    g
}

const IC3: &str = "MATCH (countryX:Country {name: 'Country0'}), (countryY:Country {name: 'Country1'}), (person:Person {id: 10}) \
    WITH person, countryX, countryY LIMIT 1 \
    MATCH (city:City)-[:IS_PART_OF]->(country:Country) WHERE country IN [countryX, countryY] \
    WITH person, countryX, countryY, collect(city) AS cities \
    MATCH (person)-[:KNOWS*1..2]-(friend)-[:IS_LOCATED_IN]->(city) WHERE NOT person = friend AND NOT city IN cities \
    WITH DISTINCT friend, countryX, countryY \
    MATCH (friend)<-[:HAS_CREATOR]-(message), (message)-[:IS_LOCATED_IN]->(country) \
    WHERE 1325376000000 > message.creationDate >= 1293840000000 AND country IN [countryX, countryY] \
    WITH friend, CASE WHEN country = countryX THEN 1 ELSE 0 END AS mx, CASE WHEN country = countryY THEN 1 ELSE 0 END AS my \
    WITH friend, sum(mx) AS xCount, sum(my) AS yCount WHERE xCount > 0 AND yCount > 0 \
    RETURN friend.id AS friendId, friend.firstName AS friendFirstName, friend.lastName AS friendLastName, xCount, yCount, xCount + yCount AS xyCount \
    ORDER BY xyCount DESC, friendId ASC LIMIT 20";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn streamed(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn ic3_full_on_equals_off_and_fires() {
    let g = g();
    let (on, off) = both(&g, IC3);
    assert_eq!(on, off, "IC3 columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![i(11), s("Ana"), s("One"), i(2), i(1), i(3)]],
        "only f1 (Ana) has messages in BOTH countries; f2 lacks Country1, f3 is excluded by location"
    );
    assert!(
        !streamed(&g, IC3),
        "full IC3 must compose the rewrites + fire the N-stage pipeline, not stream"
    );
}
