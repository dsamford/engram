//! IC3's date-windowed HAS_CREATOR seek proven byte-identical to the ordinary
//! batched expansion AND to the row-at-a-time interp — on the FULL IC3 clause
//! structure (multi-anchor stage 0, a `collect(city)` exclusion set, a KNOWS*1..2
//! var-length hop, then the date+country message stage grouped by friend with a
//! both-countries HAVING). The last stage seeks each friend's in-window messages
//! from the date-ordered `creator_msgs` index. The fixture pins the window
//! boundaries (inclusive lower, exclusive upper), the HAVING (a friend with
//! messages in only one country is dropped), the city-exclusion (a friend living
//! INSIDE one of the two countries is dropped), and a non-{cx,cy} country (never
//! counted).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn node(g: &Graph, label: &str, props: &[(&str, Value)]) -> u64 {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    g.create_node(&[label.into()], &m).expect("node")
}
fn rel(g: &Graph, s: u64, t: &str, d: u64) {
    g.create_rel(s, t, d, &BTreeMap::new()).expect("rel");
}
fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap();
    run_query(g, &q, BTreeMap::new()).unwrap().rows
}
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn three(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_ic3_datewindow(true);
    let seek = rows(g, src);
    g.set_ic3_datewindow(false);
    let general = rows(g, src);
    g.set_ic3_datewindow(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (seek, general, interp)
}
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_ic3_datewindow(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline ic3 datewindow")
        .copied()
        .unwrap_or(0)
        > 0
}

// The real IC3 shape, with a small window [100, 300) and two named countries.
const SRC: &str = "MATCH (countryX:Country {name: 'CX'}), (countryY:Country {name: 'CY'}), (person:Person {id: 10}) \
    WITH person, countryX, countryY LIMIT 1 \
    MATCH (city:City)-[:IS_PART_OF]->(country:Country) WHERE country IN [countryX, countryY] \
    WITH person, countryX, countryY, collect(city) AS cities \
    MATCH (person)-[:KNOWS*1..2]-(friend)-[:IS_LOCATED_IN]->(city) WHERE NOT person = friend AND NOT city IN cities \
    WITH DISTINCT friend, countryX, countryY \
    MATCH (friend)<-[:HAS_CREATOR]-(message), (message)-[:IS_LOCATED_IN]->(country) \
    WHERE 300 > message.creationDate >= 100 AND country IN [countryX, countryY] \
    WITH friend, CASE WHEN country = countryX THEN 1 ELSE 0 END AS mx, CASE WHEN country = countryY THEN 1 ELSE 0 END AS my \
    WITH friend, sum(mx) AS xCount, sum(my) AS yCount \
    WHERE xCount > 0 AND yCount > 0 \
    RETURN friend.id AS fid, xCount, yCount ORDER BY xCount + yCount DESC, fid ASC LIMIT 20";

#[test]
fn ic3_datewindow_matches_general_and_interp() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let cx = node(&g, "Country", &[("name", s("CX"))]);
    let cy = node(&g, "Country", &[("name", s("CY"))]);
    let cz = node(&g, "Country", &[("name", s("CZ"))]); // decoy — never in [cx,cy]
    let xcity = node(&g, "City", &[]);
    rel(&g, xcity, "IS_PART_OF", cx);
    let ycity = node(&g, "City", &[]);
    rel(&g, ycity, "IS_PART_OF", cy);
    let zcity = node(&g, "City", &[]);
    rel(&g, zcity, "IS_PART_OF", cz);
    let person = node(&g, "Person", &[("id", i(10))]);

    let mut next_mid = 0i64;
    let mut msg = |g: &Graph, creator: u64, country: u64, date: i64| {
        next_mid += 1;
        let m = node(
            g,
            "Message",
            &[("id", i(next_mid)), ("creationDate", i(date))],
        );
        rel(g, m, "HAS_CREATOR", creator);
        rel(g, m, "IS_LOCATED_IN", country);
    };
    let mkfriend = |g: &Graph, id: i64, city: u64| {
        let f = node(g, "Person", &[("id", i(id))]);
        rel(g, person, "KNOWS", f);
        rel(g, f, "IS_LOCATED_IN", city);
        f
    };

    // Friends live OUTSIDE cx/cy (in zcity) unless noted; person KNOWS each (1 hop).
    let a = mkfriend(&g, 1, zcity); // CX@150 + CY@200 → x=1,y=1 → PASSES
    msg(&g, a, cx, 150);
    msg(&g, a, cy, 200);
    msg(&g, a, cz, 150); // decoy country — must not count
    let b = mkfriend(&g, 2, zcity); // two in CX, none in CY → y=0 → HAVING drop
    msg(&g, b, cx, 150);
    msg(&g, b, cx, 250);
    let c = mkfriend(&g, 3, zcity); // one before (@50), one after (@350) → out of window
    msg(&g, c, cx, 50);
    msg(&g, c, cy, 350);
    let d = mkfriend(&g, 4, zcity); // CX@100 (INCLUSIVE lower) + CY@299 → PASSES
    msg(&g, d, cx, 100);
    msg(&g, d, cy, 299);
    let e = mkfriend(&g, 5, zcity); // CX@300 (EXCLUSIVE upper → dropped) + CY@200 → x=0 → drop
    msg(&g, e, cx, 300);
    msg(&g, e, cy, 200);
    let f = mkfriend(&g, 6, xcity); // lives INSIDE cx → excluded by the city filter
    msg(&g, f, cx, 150);
    msg(&g, f, cy, 200);
    let _ = (b, c, e, f);

    let (seek, general, interp) = three(&g, SRC);
    assert_eq!(seek, general, "ic3 datewindow vs general disagree");
    assert_eq!(seek, interp, "ic3 datewindow vs interp disagree");
    // A and D each have x=1,y=1 (sum 2); tie broken by fid ASC. B/C/E dropped by
    // HAVING/window, F dropped by the city-exclusion, the CZ message never counted.
    assert_eq!(
        seek,
        vec![vec![i(1), i(1), i(1)], vec![i(4), i(1), i(1)]],
        "only both-country friends living outside cx/cy, inside [100,300), survive"
    );
    assert!(
        fired(&g, SRC),
        "the date-windowed HAS_CREATOR stage must take the seek"
    );
}
