//! BI7's 2-hop count-rollup proven byte-identical to the ordinary expand + reduce
//! AND to the interp — on `count(a) GROUP BY c.<prop>` over `(a)-[:R1]->(b)-[:R2]->
//! (c)`, including a COUNT TIE between two far groups (broken by the group key, so
//! the rollup's arrival order can't leak) and a non-source node on the middle (must
//! not be counted).

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
    g.set_bi7_rollup(true);
    let roll = rows(g, src);
    g.set_bi7_rollup(false);
    let general = rows(g, src);
    g.set_bi7_rollup(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (roll, general, interp)
}
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_bi7_rollup(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline bi7 rollup")
        .copied()
        .unwrap_or(0)
        > 0
}

#[test]
fn bi7_rollup_matches_general_and_interp() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let africa = node(&g, "Continent", &[("name", s("Africa"))]);
    let europe = node(&g, "Continent", &[("name", s("Europe"))]);
    let c1 = node(&g, "Country", &[]);
    let c2 = node(&g, "Country", &[]);
    let c3 = node(&g, "Country", &[]);
    rel(&g, c1, "IS_PART_OF", africa);
    rel(&g, c2, "IS_PART_OF", africa);
    rel(&g, c3, "IS_PART_OF", europe);
    let msg = |g: &Graph, country: u64| {
        let m = node(g, "Message", &[]);
        rel(g, m, "IS_LOCATED_IN", country);
    };
    msg(&g, c1);
    msg(&g, c1); // c1: 2
    msg(&g, c2); // c2: 1  → Africa = 3 (across two countries)
    msg(&g, c3);
    msg(&g, c3);
    msg(&g, c3); // c3: 3  → Europe = 3  (TIE with Africa)
    // A company on a country must NOT be counted (source is (m:Message)).
    let comp = node(&g, "Company", &[]);
    rel(&g, comp, "IS_LOCATED_IN", c1);

    let src = "MATCH (m:Message)-[:IS_LOCATED_IN]->(co:Country)-[:IS_PART_OF]->(cont:Continent) \
        RETURN cont.name AS continent, count(m) AS cnt ORDER BY cnt DESC, continent ASC LIMIT 20";
    let (roll, general, interp) = three(&g, src);
    assert_eq!(roll, general, "bi7 rollup vs general disagree");
    assert_eq!(roll, interp, "bi7 rollup vs interp disagree");
    // Both continents have 3 (the rollup sums two Africa countries); tie broken by
    // name ASC → Africa before Europe. The Company is not counted.
    assert_eq!(
        roll,
        vec![vec![s("Africa"), i(3)], vec![s("Europe"), i(3)]],
        "count rolled up per continent, tie broken by name, non-Message excluded"
    );
    assert!(
        fired(&g, src),
        "the 2-hop count-by-far-target must take the rollup"
    );
}

/// BI8's shape: the count is over the MIDDLE var (`count(m)`), the source and far
/// vars share the same label (`Person` likes, `Person` creates), and the ORDER BY
/// ties on the group key. `count(m)` == the row count == the rollup's sum, so it is
/// byte-identical too.
#[test]
fn bi8_count_over_middle_matches_general_and_interp() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Two creators (grouped by c.id); likers are Persons too (source label == far).
    let ca = node(&g, "Person", &[("id", i(1))]);
    let cb = node(&g, "Person", &[("id", i(2))]);
    let ma1 = node(&g, "Message", &[]);
    let ma2 = node(&g, "Message", &[]);
    let mb1 = node(&g, "Message", &[]);
    rel(&g, ma1, "HAS_CREATOR", ca);
    rel(&g, ma2, "HAS_CREATOR", ca);
    rel(&g, mb1, "HAS_CREATOR", cb);
    let liker = |g: &Graph| node(g, "Person", &[]);
    let (p1, p2, p3) = (liker(&g), liker(&g), liker(&g));
    // ca: ma1 liked by p1,p2 (2) + ma2 liked by p3 (1) = 3 across two messages.
    rel(&g, p1, "LIKES", ma1);
    rel(&g, p2, "LIKES", ma1);
    rel(&g, p3, "LIKES", ma2);
    // cb: mb1 liked by p1,p2,p3 = 3 on one message → TIE with ca.
    rel(&g, p1, "LIKES", mb1);
    rel(&g, p2, "LIKES", mb1);
    rel(&g, p3, "LIKES", mb1);

    let src = "MATCH (p:Person)-[:LIKES]->(m:Message)-[:HAS_CREATOR]->(c:Person) \
        RETURN c.id AS creator, count(m) AS likes ORDER BY likes DESC, creator ASC LIMIT 20";
    let (roll, general, interp) = three(&g, src);
    assert_eq!(roll, general, "bi8 rollup vs general disagree");
    assert_eq!(roll, interp, "bi8 rollup vs interp disagree");
    // count(m) counts the (p,m) rows per creator: ca=3, cb=3; tie broken by id ASC.
    assert_eq!(
        roll,
        vec![vec![i(1), i(3)], vec![i(2), i(3)]],
        "count(middle) rolled up per creator equals the row count, tie broken by id"
    );
    assert!(
        fired(&g, src),
        "count-over-middle must also take the rollup"
    );
}
