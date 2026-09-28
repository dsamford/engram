//! The date-ordered per-creator message index behind IC2's k-way merge and
//! IC3's date window ranks TYPED dates — and answers as the ordinary path does.
//!
//! It held dates as `i64` (an integer or a DATE's days) and skipped every other
//! value, and both bounds took only those: on the typed SNB corpus, where
//! `creationDate` is a DATETIME, IC2 and IC3 declined their fast paths on EVERY
//! run — IC9's typed-date story again. It was also cached by commit epoch
//! alone, so an index built over one label answered a statement asking for
//! another.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Rows, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// The query with its fast path, then with the columnar paths off (the
/// ordinary interp) — the answers must agree; returns the fast run's counters.
fn agrees(g: &Graph, q: &str) -> (Rows, BTreeMap<String, u64>) {
    let (fast, c) = run(g, q);
    g.set_columnar_scans(false);
    let (interp, _) = run(g, q);
    g.set_columnar_scans(true);
    assert_eq!(fast, interp, "\n  {q}");
    (fast, c)
}

const IC2_MERGE: &str = "interp.pipeline ic2 ordered merge";
const IC2_DECLINE: &str = "interp.pipeline ic2 ordered merge declined a bound it cannot rank";
const IC3_WINDOW: &str = "interp.pipeline ic3 datewindow";

/// Person 0 knows friends 1-3; each friend writes twelve messages an hour apart
/// (friend 3's collide in time with friend 2's, so ties break by id), a
/// stranger (9) writes more, and friend 1 also has one message dated by a bare
/// INTEGER — which a DATETIME bound compares null with.
fn friends() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND [0, 1, 2, 3, 9] AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "MATCH (p:Person {id: 0}), (f:Person) WHERE f.id IN [1, 2, 3] CREATE (p)-[:KNOWS]->(f)",
    );
    ddl(
        &g,
        "MATCH (f:Person) WHERE f.id > 0 UNWIND range(0, 11) AS k \
         CREATE (f)<-[:HAS_CREATOR]-(:Message {id: f.id * 100 + k, \
             creationDate: datetime.fromepochmillis(1300000000000 + (k + (CASE WHEN f.id = 3 THEN 0 ELSE f.id END)) * 3600000)})",
    );
    ddl(
        &g,
        "MATCH (f:Person {id: 1}) CREATE (f)<-[:HAS_CREATOR]-(:Message {id: 199, creationDate: 1300000000000})",
    );
    let _ = g.warm();
    g
}

fn ic2(bound: &str) -> String {
    format!(
        "MATCH (:Person {{id: 0}})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate <= {bound} \
         RETURN friend.id AS personId, message.id AS postOrCommentId, \
                message.creationDate AS postOrCommentCreationDate \
         ORDER BY postOrCommentCreationDate DESC, toInteger(postOrCommentId) ASC LIMIT 20"
    )
}

#[test]
fn ic2_merges_friends_datetime_messages() {
    let g = friends();
    for bound in [
        "datetime.fromepochmillis(1300000000000 + 8 * 3600000)",
        "datetime.fromepochmillis(1300000000000 + 30 * 3600000)",
        "datetime('2011-03-13T07:06:40Z')",
    ] {
        let q = ic2(bound);
        let (rows, c) = agrees(&g, &q);
        assert!(!rows.is_empty(), "vacuous: {q}");
        assert!(counter(&c, IC2_MERGE) > 0, "the merge declined a DATETIME bound: {q}\n{c:?}");
    }
    // The integer-dated message answers only an integer bound — and there too.
    let (rows, c) = agrees(&g, &ic2("1300000000000"));
    assert_eq!(rows.len(), 1, "one integer-dated message: {rows:?}");
    assert!(counter(&c, IC2_MERGE) > 0, "{c:?}");
}

#[test]
fn an_integer_bound_declines_where_a_date_is_a_float() {
    let g = friends();
    ddl(
        &g,
        "MATCH (f:Person {id: 2}) CREATE (f)<-[:HAS_CREATOR]-(:Message {id: 299, creationDate: 1300000000000.5})",
    );
    // 1300000000001 > 1300000000000.5 numerically: the float message qualifies.
    let (rows, c) = agrees(&g, &ic2("1300000000001"));
    assert_eq!(rows.len(), 2, "the float and the integer message: {rows:?}");
    assert!(counter(&c, IC2_DECLINE) > 0, "an integer bound over a float must decline: {c:?}");
}

#[test]
fn an_index_over_one_label_does_not_answer_another() {
    let g = friends();
    ddl(&g, "MATCH (m:Message) WHERE m.id % 2 = 0 SET m:Post");
    // The index over :Post first, in the same epoch as the :Message statement.
    let posts = "MATCH (:Person {id: 0})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Post) \
         WHERE message.creationDate <= datetime.fromepochmillis(1300000000000 + 30 * 3600000) \
         RETURN message.id AS id ORDER BY message.creationDate DESC, id ASC LIMIT 20";
    let (p, _) = agrees(&g, posts);
    assert!(p.iter().all(|r| matches!(r[0], Value::Int(i) if i % 2 == 0)), "{p:?}");
    let (m, c) = agrees(&g, &ic2("datetime.fromepochmillis(1300000000000 + 30 * 3600000)"));
    assert!(
        m.iter().any(|r| matches!(r[1], Value::Int(i) if i % 2 == 1)),
        "the :Message answer lost its odd ids to the :Post index: {m:?}"
    );
    assert!(counter(&c, IC2_MERGE) > 0, "{c:?}");
}

/// IC3's shape (as `ic3_datewindow.rs`), its window and every date DATETIME;
/// friend 1 also has an INTEGER-dated message in CY inside the integer window,
/// which the DATETIME bounds must not count.
#[test]
fn ic3_windows_datetime_messages() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE (:Country {name: 'CX'}), (:Country {name: 'CY'}), (:Country {name: 'CZ'})");
    ddl(
        &g,
        "MATCH (c:Country) CREATE (:City {name: c.name + 'city'})-[:IS_PART_OF]->(c)",
    );
    ddl(&g, "CREATE (:Person {id: 10})");
    ddl(
        &g,
        "MATCH (p:Person {id: 10}), (z:City {name: 'CZcity'}) UNWIND [1, 2, 3, 4, 5] AS i \
         CREATE (p)-[:KNOWS]->(:Person {id: i})-[:IS_LOCATED_IN]->(z)",
    );
    ddl(
        &g,
        "MATCH (p:Person {id: 10}), (x:City {name: 'CXcity'}) \
         CREATE (p)-[:KNOWS]->(:Person {id: 6})-[:IS_LOCATED_IN]->(x)",
    );
    // (friend, country, day): a/d pass, b/e fail the HAVING, c falls outside,
    // f lives inside CX.
    for (f, country, day) in [
        (1, "CX", 150),
        (1, "CY", 200),
        (1, "CZ", 150),
        (2, "CX", 150),
        (2, "CX", 250),
        (3, "CX", 50),
        (3, "CY", 350),
        (4, "CX", 100),
        (4, "CY", 299),
        (5, "CX", 300),
        (5, "CY", 200),
        (6, "CX", 150),
        (6, "CY", 200),
    ] {
        ddl(
            &g,
            &format!(
                "MATCH (f:Person {{id: {f}}}), (c:Country {{name: '{country}'}}) \
                 CREATE (f)<-[:HAS_CREATOR]-(:Message {{id: {f} * 1000 + {day}, \
                     creationDate: datetime.fromepochmillis({day} * 86400000)}})-[:IS_LOCATED_IN]->(c)"
            ),
        );
    }
    // Friend 2's integer-dated CY message: in the integer window, but the
    // DATETIME window compares null with it — friend 2 must still fail.
    ddl(
        &g,
        "MATCH (f:Person {id: 2}), (c:Country {name: 'CY'}) \
         CREATE (f)<-[:HAS_CREATOR]-(:Message {id: 2999, creationDate: 200})-[:IS_LOCATED_IN]->(c)",
    );
    let _ = g.warm();
    let q = "MATCH (countryX:Country {name: 'CX'}), (countryY:Country {name: 'CY'}), (person:Person {id: 10}) \
        WITH person, countryX, countryY LIMIT 1 \
        MATCH (city:City)-[:IS_PART_OF]->(country:Country) WHERE country IN [countryX, countryY] \
        WITH person, countryX, countryY, collect(city) AS cities \
        MATCH (person)-[:KNOWS*1..2]-(friend)-[:IS_LOCATED_IN]->(city) WHERE NOT person = friend AND NOT city IN cities \
        WITH DISTINCT friend, countryX, countryY \
        MATCH (friend)<-[:HAS_CREATOR]-(message), (message)-[:IS_LOCATED_IN]->(country) \
        WHERE datetime.fromepochmillis(300 * 86400000) > message.creationDate >= datetime.fromepochmillis(100 * 86400000) \
          AND country IN [countryX, countryY] \
        WITH friend, CASE WHEN country = countryX THEN 1 ELSE 0 END AS mx, CASE WHEN country = countryY THEN 1 ELSE 0 END AS my \
        WITH friend, sum(mx) AS xCount, sum(my) AS yCount \
        WHERE xCount > 0 AND yCount > 0 \
        RETURN friend.id AS fid, xCount, yCount ORDER BY xCount + yCount DESC, fid ASC LIMIT 20";
    let (rows, c) = agrees(&g, q);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Int(4), Value::Int(1), Value::Int(1)],
        ],
        "only both-country friends outside CX/CY, inside the window"
    );
    assert!(counter(&c, IC3_WINDOW) > 0, "the window declined DATETIME bounds: {c:?}");
}

/// ONE index serves every label set it holds all the messages of. IC2 walks
/// `(message:Message)` and IC3 an unlabelled `(message)`: keyed by labels, each
/// evicted the other's from the one cache slot, and both paid a whole-graph
/// build on every run of a battery (13.4 s and 14.7 s at SF3).
#[test]
fn one_index_serves_a_labelled_walk_and_an_unlabelled_one() {
    const BUILT: &str = "interp.pipeline date index built";
    const FILTERED: &str = "interp.pipeline date index answered other labels, filtered to them";
    let labelled = ic2("datetime.fromepochmillis(1300000000000 + 30 * 3600000)");
    let bare = labelled.replace("(message:Message)", "(message)");
    let note = "MATCH (p:Person {id: 1}) CREATE (p)<-[:HAS_CREATOR]-(:Note {id: 5, \
                creationDate: datetime.fromepochmillis(1300000000000 + 29 * 3600000)})";
    let has_note = |rows: &Rows| rows.iter().any(|r| r[1] == Value::Int(5));

    // Labelled first: every creator edge starts at a Message, so the index
    // answers the unlabelled walk as it is.
    let g = friends();
    let (_, c) = agrees(&g, &labelled);
    assert_eq!(counter(&c, BUILT), 1, "{c:?}");
    let (rows, c) = agrees(&g, &bare);
    assert!(!rows.is_empty(), "vacuous");
    assert!(counter(&c, IC2_MERGE) > 0, "{c:?}");
    assert_eq!(counter(&c, BUILT), 0, "the unlabelled walk rebuilt the index: {c:?}");

    // A Note with a creator: an index over Messages no longer holds every
    // message the unlabelled walk reaches, so it builds its own — and finds
    // the Note.
    let g = friends();
    ddl(&g, note);
    let (_, c) = agrees(&g, &labelled);
    assert_eq!(counter(&c, BUILT), 1, "{c:?}");
    let (rows, c) = agrees(&g, &bare);
    assert_eq!(counter(&c, BUILT), 1, "an index missing the Note answered: {c:?}");
    assert!(has_note(&rows), "{rows:?}");

    // Unlabelled first: its index holds every source and answers `:Message`
    // filtered to Messages — without the Note.
    let g = friends();
    ddl(&g, note);
    let (rows, c) = agrees(&g, &bare);
    assert_eq!(counter(&c, BUILT), 1, "{c:?}");
    assert!(has_note(&rows), "{rows:?}");
    let (rows, c) = agrees(&g, &labelled);
    assert_eq!(counter(&c, BUILT), 0, "the labelled walk rebuilt the index: {c:?}");
    assert!(counter(&c, FILTERED) > 0, "{c:?}");
    assert!(!has_note(&rows), "the Note reached a `:Message` answer: {rows:?}");
}

/// SNB Interactive IS2 pages a creator's newest messages from the date index
/// IC2 built — fed only the entries that can reach the page, through the
/// stage's own projector — and answers as the full walk does: ties across the
/// page's boundary included, and a creator with two date classes declined.
#[test]
fn a_creators_newest_messages_page_from_the_date_index() {
    const PAGED: &str = "interp.stage paged a creator's newest messages from the date index";
    let g = friends();
    ddl(&g, "CREATE INDEX person_id FOR (p:Person) ON (p.id)");
    // three more of friend 2's messages dated as its message 207, so a page of
    // five ends inside a tie
    ddl(
        &g,
        "MATCH (f:Person {id: 2}) UNWIND [252, 250, 251] AS i \
         CREATE (f)<-[:HAS_CREATOR]-(:Message {id: i, \
             creationDate: datetime.fromepochmillis(1300000000000 + 9 * 3600000)})",
    );
    let is2 = |p: i64, k: i64| {
        format!(
            "MATCH (:Person {{id: {p}}})<-[:HAS_CREATOR]-(message) \
             WITH message, message.id AS messageId, message.creationDate AS messageCreationDate \
             ORDER BY messageCreationDate DESC, messageId ASC LIMIT {k} \
             RETURN messageId, messageCreationDate"
        )
    };
    // the full walk, before any index exists
    let pages = [(2, 5), (2, 6), (2, 20), (3, 4), (1, 5)];
    let want: Vec<Rows> = pages.iter().map(|&(p, k)| run(&g, &is2(p, k)).0).collect();
    assert!(want.iter().all(|w| !w.is_empty()), "vacuous: {want:?}");
    // IC2 builds the index
    let (_, c) = agrees(&g, &ic2("datetime.fromepochmillis(1300000000000 + 30 * 3600000)"));
    assert!(counter(&c, IC2_MERGE) > 0, "{c:?}");
    for (&(p, k), want) in pages.iter().zip(&want) {
        let (got, c) = run(&g, &is2(p, k));
        assert_eq!(&got, want, "person {p}, page of {k}");
        if p == 1 {
            // an INTEGER-dated message beside the DATETIMEs: two date classes
            assert_eq!(counter(&c, PAGED), 0, "person 1 paged from the index: {c:?}");
        } else {
            assert!(counter(&c, PAGED) > 0, "person {p}, page of {k}, walked: {c:?}");
        }
    }
}
