#![allow(non_snake_case)]
//! IC9's index-ordered top-k (Track B's IC9 lever) must be byte-identical to the
//! row-at-a-time interp it replaces. The query is IC9's exact shape: a KNOWS*1..2
//! neighbourhood collected DISTINCT, then each friend's messages before a date,
//! top-20 by (creationDate DESC, message.id ASC). `on` runs the columnar
//! pipeline (where the index-ordered operator fires in `run_multistage`); `off`
//! runs the interp. They must agree exactly — and the operator must actually
//! fire, else this proves nothing.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const IC9: &str = "MATCH (root:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
     WHERE NOT friend = root \
     WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < 5000 \
     RETURN friend.id AS personId, message.id AS commentOrPostId, \
            message.creationDate AS commentOrPostCreationDate \
     ORDER BY commentOrPostCreationDate DESC, message.id ASC LIMIT 20";

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // 15 people; the query roots at the one whose `id` property is 10.
    let mut person = Vec::new();
    for i in 0..15i64 {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(i));
        person.push(g.create_node(&["Person".into()], &p).expect("person"));
    }
    let empty = BTreeMap::new();
    // A KNOWS ring + chords so root=10's 1..2-hop neighbourhood is most people
    // (a DENSE friend set → the operator fires rather than bailing).
    for i in 0..15usize {
        g.create_rel(person[i], "KNOWS", person[(i + 1) % 15], &empty)
            .expect("knows");
        g.create_rel(person[i], "KNOWS", person[(i + 4) % 15], &empty)
            .expect("chord");
    }
    // ~80 messages: creationDate collides (ties → the message.id tiebreak must
    // decide), message.id is NOT aligned with creation order, spread across
    // authors so > 20 qualify under the date bound.
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(7000 - i)); // unique, anti-aligned
        m.insert(
            "creationDate".to_string(),
            Value::Int(1000 + (i * 37) % 4200),
        );
        let mid = g.create_node(&["Message".into()], &m).expect("message");
        let author = person[(i as usize * 11) % 15];
        g.create_rel(mid, "HAS_CREATOR", author, &empty)
            .expect("has_creator");
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

fn rows_with(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

/// IC9 on a TYPED corpus: `creationDate` is a DATETIME and the bound is a
/// `$maxDate` parameter, exactly as the harness binds it. Until 2026-09-24
/// the operator took integer bounds alone and the range index could not hold
/// a date at all, so on the typed SF3 store this shape gathered ~5M message
/// records to keep twenty (21 s, against PostgreSQL's 0.5).
const IC9_TYPED: &str = "MATCH (root:Person {id: 10})-[:KNOWS*1..2]-(friend:Person) \
     WHERE NOT friend = root \
     WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < $maxDate \
     RETURN friend.id AS personId, message.id AS commentOrPostId, \
            message.creationDate AS commentOrPostCreationDate \
     ORDER BY commentOrPostCreationDate DESC, message.id ASC LIMIT 20";

fn instant(secs: i64, offset_seconds: i32, zone: Option<&str>) -> Value {
    Value::DateTime {
        epoch_seconds: secs,
        nanos: 0,
        offset_seconds,
        zone: zone.map(str::to_string),
    }
}

/// The integer fixture with DATETIME creation dates. Instants collide (ties
/// fall to `message.id`), and the SAME instant is written with different
/// offsets and a zone id: the offset is presentation, so they must tie too.
/// `odd` adds messages whose `creationDate` is of ANOTHER class — a DATE, a
/// LOCAL DATETIME, an integer — for which `<` against a datetime is null.
fn typed(odd: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut person = Vec::new();
    for i in 0..15i64 {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(i));
        person.push(g.create_node(&["Person".into()], &p).expect("person"));
    }
    let empty = BTreeMap::new();
    for i in 0..15usize {
        g.create_rel(person[i], "KNOWS", person[(i + 1) % 15], &empty)
            .expect("knows");
        g.create_rel(person[i], "KNOWS", person[(i + 4) % 15], &empty)
            .expect("chord");
    }
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(7000 - i));
        let secs = 1_300_000_000 + ((i * 37) % 4200) * 3600;
        let date = match i % 3 {
            0 => instant(secs, 0, None),
            // the same instant, presented at +02:00 and in a named zone
            1 => instant(secs, 7200, None),
            _ => instant(secs, 3600, Some("Europe/Paris")),
        };
        m.insert("creationDate".to_string(), date);
        let mid = g.create_node(&["Message".into()], &m).expect("message");
        g.create_rel(mid, "HAS_CREATOR", person[(i as usize * 11) % 15], &empty)
            .expect("has_creator");
    }
    if odd {
        for (k, date) in [
            (0i64, Value::Date(15_000)),
            (1, Value::LocalDateTime { epoch_seconds: 1_300_000_000, nanos: 0 }),
            (2, Value::Int(1_300_000_000)),
            (3, Value::Date(20_000)),
        ] {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100 + k));
            m.insert("creationDate".to_string(), date);
            let mid = g.create_node(&["Message".into()], &m).expect("odd message");
            g.create_rel(mid, "HAS_CREATOR", person[(k as usize * 11) % 15], &empty)
                .expect("has_creator");
        }
    }
    g
}

#[test]
fn ic9_on_typed_dates_is_served_by_the_index_and_answers_as_the_interp_does() {
    for odd in [false, true] {
        let g = typed(odd);
        // a bound inside the corpus's range, so the walk starts mid-index
        let mut params = BTreeMap::new();
        params.insert(
            "maxDate".to_string(),
            instant(1_300_000_000 + 3000 * 3600, 0, None),
        );
        g.set_columnar_scans(false);
        let off = rows_with(&g, IC9_TYPED, params.clone());
        g.set_columnar_scans(true);
        let (on, trace) = engram_observe::with_trace(|| rows_with(&g, IC9_TYPED, params.clone()));
        assert_eq!(on, off, "odd = {odd}: index-ordered top-k diverged from the interp");
        assert_eq!(off.len(), 20, "odd = {odd}: the fixture must fill the top-k");
        assert!(
            trace
                .counters()
                .get("interp.pipeline index-ordered topk served stage 2")
                .copied()
                .unwrap_or(0)
                > 0,
            "odd = {odd}: the operator did not serve a DATETIME bound: {:?}",
            trace.counters()
        );
    }
}

#[test]
fn a_bound_the_index_cannot_order_declines_to_the_general_path() {
    // A STRING bound against datetimes: `<` is null for every row, so the
    // answer is empty either way — and the operator must not be the one to
    // decide that.
    let g = typed(false);
    let mut params = BTreeMap::new();
    params.insert("maxDate".to_string(), Value::Str("2011".into()));
    g.set_columnar_scans(false);
    let off = rows_with(&g, IC9_TYPED, params.clone());
    g.set_columnar_scans(true);
    let (on, trace) = engram_observe::with_trace(|| rows_with(&g, IC9_TYPED, params.clone()));
    assert_eq!(on, off);
    assert!(off.is_empty(), "a string bound admits no datetime: {off:?}");
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline index-ordered topk served stage 2")
            .copied()
            .unwrap_or(0),
        0,
        "a string bound must not be served by the index walk"
    );
}

#[test]
fn ic9_index_ordered_topk_is_byte_identical_to_interp() {
    let g = g();

    g.set_columnar_scans(false);
    let off = rows(&g, IC9);
    g.set_columnar_scans(true);
    let (on, trace) = engram_observe::with_trace(|| rows(&g, IC9));

    assert_eq!(on, off, "index-ordered top-k diverged from the interp");
    assert!(
        !off.is_empty(),
        "the fixture produced no rows — test is vacuous"
    );
    assert!(
        trace
            .counters()
            .get("interp.pipeline index-ordered topk served stage 2")
            .copied()
            .unwrap_or(0)
            > 0,
        "the index-ordered operator did not fire — the differential proves nothing"
    );
}
