//! A WHERE conjunct is tested at the hop that binds what it reads, not after
//! the whole path has been walked.
//!
//! SNB BI bi16 at SF10 walks `(tag)<-[:HAS_TAG]-(message2)-[:HAS_CREATOR]->
//! (person2)-[:KNOWS]-(person1)` with `WHERE date(message2.creationDate) =
//! date($d)`: 15,680 tagged messages, 309 on the date, and every one of the
//! 15,680 was walked on to its creator and a KNOWS probe before the date was
//! looked at — per row, 300 rows a side. See `early_hop_filters`.
//!
//! THE LOAD-BEARING TEST IS DIFFERENTIAL: the lever on and off must return
//! identical rows (or the identical error) for every query here, including
//! the shapes that must NOT be pushed — OR, rand(), a conjunct that errors,
//! an OPTIONAL MATCH whose every path is refused.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const REFUSED: &str = "interp.hop end refused by its WHERE before the next hop";

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

type Outcome = Result<Vec<Vec<Value>>, String>;

fn run(g: &Graph, q: &str) -> (Outcome, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (r, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .map(|r| r.rows)
            .map_err(|e| e.to_string())
    });
    (r, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn both_ways(g: &Graph, q: &str) -> (Outcome, Outcome) {
    g.set_rel_predicate_pushdown(true);
    let on = run(g, q).0;
    g.set_rel_predicate_pushdown(false);
    let off = run(g, q).0;
    g.set_rel_predicate_pushdown(true);
    (on, off)
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// 120 persons who each KNOW the next 8; 40 messages each on day `m % 7`, a
/// quarter of them missing `day` entirely (so a predicate over it is NULL);
/// every 13th message carries the tag.
fn social() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 119) AS i CREATE (:Person {id: i})");
    ddl(&g, "CREATE (:Tag {name: 'hot'}), (:Tag {name: 'cold'})");
    ddl(
        &g,
        "UNWIND range(0, 119) AS i UNWIND range(1, 8) AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 120}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 4799) AS m MATCH (p:Person {id: m % 120}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Message {id: m, zero: 0, \
           creationDate: datetime('2011-10-0' + toString(1 + m % 7) + 'T12:00:00Z')})",
    );
    ddl(&g, "MATCH (m:Message) WHERE m.id % 4 <> 0 SET m.day = m.id % 7");
    ddl(
        &g,
        "MATCH (m:Message), (t:Tag) WHERE (m.id % 13 = 0) = (t.name = 'hot') \
         CREATE (m)-[:HAS_TAG]->(t)",
    );
    let _ = g.warm();
    g
}

const LEG: &str = "(tag)<-[:HAS_TAG]-(message2:Message)-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(person1)";
const HEAD: &str = "MATCH (tag:Tag {name: 'hot'}), (person1:Person) WHERE person1.id % 10 = 0 ";

/// The pushable shapes prune, and still agree with the lever off.
#[test]
fn a_decidable_conjunct_prunes_at_its_hop_and_changes_no_answer() {
    let g = social();
    for pred in [
        "message2.day = 3",
        "date(message2.creationDate) = date(datetime('2011-10-04T00:00:00Z'))",
        "message2.day = 3 AND person2.id % 2 = 0",
        // an OR is ONE conjunct; once everything it reads is bound (here at
        // person2's hop) it is decided, and pruning there is exact
        "message2.day = 3 OR person2.id = 1",
    ] {
        for kind in ["MATCH", "OPTIONAL MATCH"] {
            let q = format!(
                "{HEAD}{kind} {LEG} WHERE {pred} \
                 RETURN person1.id AS p, count(message2) AS n ORDER BY p"
            );
            let (on, off) = both_ways(&g, &q);
            assert_eq!(on, off, "lever changed the answer to `{q}`");
            let rows = on.as_ref().expect("runs");
            assert!(
                rows.iter().any(|r| matches!(r.get(1), Some(Value::Int(n)) if *n > 0)),
                "`{q}` matched nothing, so it compares two empties"
            );
            let (_, c) = run(&g, &q);
            assert!(get(&c, REFUSED) > 0, "`{q}` never pruned at a hop");
        }
    }
}

/// OPTIONAL MATCH: when EVERY path is refused early, each row must still come
/// back once, with the optional variables null — the same as with no pruning.
#[test]
fn an_optional_match_refused_everywhere_still_emits_its_null_row() {
    let g = social();
    let q = format!(
        "{HEAD}OPTIONAL MATCH {LEG} WHERE message2.day = 99 \
         RETURN person1.id AS p, message2 IS NULL AS none ORDER BY p"
    );
    let (on, off) = both_ways(&g, &q);
    assert_eq!(on, off);
    let rows = on.expect("runs");
    assert_eq!(rows.len(), 12, "one null row per person1");
    assert!(rows.iter().all(|r| r.get(1) == Some(&Value::Bool(true))));
}

/// Shapes that must NOT be pushed still agree, and do not prune. The last node
/// here, `friend`, is bound by the path itself, so a predicate that needs it is
/// decidable only on the finished row.
#[test]
fn what_must_not_be_pushed_is_not() {
    let g = social();
    let leg = "(tag)<-[:HAS_TAG]-(message2:Message)-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(friend:Person)";
    for pred in [
        // an OR that needs the LAST node: no part of it decides anything early
        "message2.day = 3 OR friend.id = 1",
        // reads only the last node
        "friend.id % 2 = 0",
        // rand() would be evaluated twice: this conjunct must not move, though
        // the deterministic one beside it may
        "rand() >= 0.0 AND friend.id >= 0",
    ] {
        let q = format!(
            "MATCH (tag:Tag {{name: 'hot'}}) MATCH {leg} WHERE {pred} \
             RETURN friend.id AS f, count(message2) AS n ORDER BY f"
        );
        let (on, off) = both_ways(&g, &q);
        assert_eq!(on, off, "lever changed the answer to `{q}`");
        assert!(!on.as_ref().expect("runs").is_empty(), "`{q}` matched nothing");
        let (_, c) = run(&g, &q);
        assert_eq!(get(&c, REFUSED), 0, "`{q}` was pruned early; it must not be");
    }
}

/// A conjunct that ERRORS keeps the walk going, so the WHERE on the finished
/// row raises exactly what it raised before.
#[test]
fn an_erroring_conjunct_leaves_the_error_to_the_where() {
    let g = social();
    let q = format!(
        "{HEAD}MATCH {LEG} WHERE 1 / message2.zero > 0 \
         RETURN count(*) AS n"
    );
    let (on, off) = both_ways(&g, &q);
    assert_eq!(on, off, "on: {on:?}\noff: {off:?}");
}

const MEMO: &str = "interp.hop end served from the statement's memo of its WHERE survivors";

/// The memo applies to the FIRST hop, so the leg must be walked from the tag.
/// With the path estimate at its default (off) the first-hop rule turns this
/// leg round to `person1` (16 KNOWS edges against ~370 tagged messages), the
/// message test lands on the SECOND hop, and nothing here would be eligible —
/// which is how the first version of these tests passed without exercising
/// the memo at all. bi16 runs at SF10 with the estimate on.
fn social_from_the_tag() -> Graph {
    let g = social();
    g.set_path_estimate(true);
    g
}

/// The first hop's survivors are computed ONCE per statement per start, and
/// every later row with that start takes them as the hop's end set. bi16 asked
/// the same tag's messages the same date question for every one of ~600 rows.
#[test]
fn a_rows_repeated_first_hop_question_is_answered_once_per_statement() {
    let g = social_from_the_tag();
    for kind in ["MATCH", "OPTIONAL MATCH"] {
        let q = format!(
            "{HEAD}{kind} {LEG} WHERE message2.day = 3 \
             RETURN person1.id AS p, count(message2) AS n ORDER BY p"
        );
        let (on, off) = both_ways(&g, &q);
        assert_eq!(on, off, "the memo changed the answer to `{q}`");
        let (_, c) = run(&g, &q);
        // 12 person1 rows share ONE tag: the first computes, the rest reuse
        assert!(get(&c, MEMO) >= 10, "`{q}` reused the memo {} times", get(&c, MEMO));
    }
}

/// A survivor that fails a LATER hop for one row can match for the next, so
/// the memo must hold what passed the FIRST hop's test — not what finished.
/// Here each row's KNOWS neighbourhood is different, so the finished rows
/// differ per person1 while the first hop's survivors do not.
#[test]
fn the_memo_holds_first_hop_survivors_not_finished_rows() {
    let g = social_from_the_tag();
    let q = format!(
        "{HEAD}MATCH {LEG} WHERE message2.day = 3 \
         RETURN person1.id AS p, collect(DISTINCT person2.id) AS via ORDER BY p"
    );
    let (on, off) = both_ways(&g, &q);
    assert_eq!(on, off);
    let rows = on.expect("runs");
    let distinct: std::collections::BTreeSet<String> =
        rows.iter().map(|r| format!("{:?}", r.get(1))).collect();
    assert!(distinct.len() > 1, "every row finished the same way; this proves nothing");
}

/// Where the first hop's test reads the ROW, survivors differ per row and the
/// memo must not be used; nor with an inline map on the hop's node.
#[test]
fn a_row_dependent_first_hop_is_never_memoised() {
    let g = social_from_the_tag();
    for (pred, leg) in [
        ("message2.day = person1.id % 7", LEG),
        (
            "message2.day = 3",
            "(tag)<-[:HAS_TAG]-(message2:Message {zero: 0})-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(person1)",
        ),
    ] {
        let q = format!(
            "{HEAD}MATCH {leg} WHERE {pred} \
             RETURN person1.id AS p, count(message2) AS n ORDER BY p"
        );
        let (on, off) = both_ways(&g, &q);
        assert_eq!(on, off, "lever changed the answer to `{q}`");
        let (_, c) = run(&g, &q);
        assert_eq!(get(&c, MEMO), 0, "`{q}` used the memo; its survivors depend on the row");
    }
}

/// A LIMIT stops the walk part-way; a memo stored from that walk would be a
/// SUBSET, and later rows would lose matches. The stop is `Saturated`, and the
/// memo is stored only after a complete expansion.
#[test]
fn a_walk_cut_short_by_a_limit_leaves_no_partial_memo() {
    let g = social_from_the_tag();
    for limit in [1, 3, 7, 50] {
        let q = format!(
            "{HEAD}MATCH {LEG} WHERE message2.day = 3 \
             WITH person1, message2 LIMIT {limit} \
             RETURN count(*) AS n"
        );
        let (on, off) = both_ways(&g, &q);
        assert_eq!(on, off, "LIMIT {limit}: memo changed the answer");
    }
    // and afterwards, in fresh statements, the full answer is unchanged
    let q = format!(
        "{HEAD}MATCH {LEG} WHERE message2.day = 3 \
         RETURN person1.id AS p, count(message2) AS n ORDER BY p"
    );
    let (on, off) = both_ways(&g, &q);
    assert_eq!(on, off);
}

/// bi16's real shape: the date is not a parameter but a ROW variable, carried
/// from an outer UNWIND into a CALL subquery. The memo keys on its VALUE, so
/// rows that share a date share the survivors, and the other date never sees
/// them. At SF10 the first memo version declined this shape outright (it
/// allowed only the hop's own node) and bi16 did not move: 457 / 391 s.
#[test]
fn a_row_variable_in_the_first_hop_test_keys_the_memo_by_its_value() {
    let g = social_from_the_tag();
    let q = "UNWIND [3, 5] AS d \
             CALL { WITH d \
               MATCH (tag:Tag {name: 'hot'}), (person1:Person) WHERE person1.id % 10 = 0 \
               MATCH (tag)<-[:HAS_TAG]-(message2:Message)-[:HAS_CREATOR]->(person2:Person)-[:KNOWS]-(person1) \
               WHERE message2.day = d \
               RETURN person1, count(message2) AS n } \
             RETURN d, person1.id AS p, n ORDER BY d, p";
    let (on, off) = both_ways(&g, q);
    assert_eq!(on, off, "the memo changed bi16's shape");
    let rows = on.expect("runs");
    let per_date = |d: i64| {
        rows.iter()
            .filter(|r| r.first() == Some(&Value::Int(d)))
            .map(|r| r.get(2).cloned())
            .collect::<Vec<_>>()
    };
    assert_ne!(per_date(3), per_date(5), "the two dates must answer differently, or a shared memo would pass");
    let (_, c) = run(&g, q);
    assert!(get(&c, MEMO) >= 20, "reused {} times across 2 dates x 12 rows", get(&c, MEMO));
}

/// The whole-path estimate cannot see the memo: a person with few friends
/// LOOKS cheaper to walk from than a busy tag, though after the first row the
/// tag end costs only its memoised survivors. On bi16 at SF10 that kept 558 of
/// 787 row-decisions on the `person1` end, and they were where the time went.
/// Here person 0 is given a hundred friends, so the estimate walks ITS row from
/// the tag and records the memo; the low-degree rows after it must then turn to
/// the tag too — with the same answers as the lever off.
#[test]
fn a_leg_turns_to_the_end_its_statement_memo_already_answers() {
    let g = social_from_the_tag();
    ddl(
        &g,
        "MATCH (a:Person {id: 0}), (b:Person) WHERE b.id >= 10 AND b.id < 110 \
         CREATE (a)-[:KNOWS]->(b)",
    );
    let _ = g.warm();
    let q = "MATCH (tag:Tag {name: 'hot'}), (person1:Person) WHERE person1.id % 10 = 0 \
             MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-(message2:Message)-[:HAS_TAG]->(tag) \
             WHERE message2.day = 3 \
             RETURN person1.id AS p, count(message2) AS n ORDER BY p";
    let (on, off) = both_ways(&g, q);
    assert_eq!(on, off, "turning to the memoised end changed the answer");
    let (_, c) = run(&g, q);
    let turned = get(&c, "interp.pattern turned to the end its statement memo already answers");
    assert!(turned > 0, "no row turned to the memoised end");
    assert!(get(&c, MEMO) >= turned, "a turned row did not use the memo");
}

/// With the estimate lever at its DEFAULT (off), no row ever walked from the
/// tag, so no memo was ever seeded: bi16 at SF10 walked every row from
/// `person1` and hit the 900 s ceiling. When exactly one end can be memoised
/// and it is not plainly worse (within 4x on the whole-path estimate), the
/// first row takes it, and every later row finds it answered.
#[test]
fn the_memoisable_end_is_seeded_with_the_estimate_lever_off() {
    let g = social(); // the path estimate stays OFF
    let q = "MATCH (tag:Tag {name: 'hot'}), (person1:Person) WHERE person1.id % 10 = 0 \
             MATCH (person1)-[:KNOWS]-(person2:Person)<-[:HAS_CREATOR]-(message2:Message)-[:HAS_TAG]->(tag) \
             WHERE message2.day = 3 \
             RETURN person1.id AS p, count(message2) AS n ORDER BY p";
    let (on, off) = both_ways(&g, q);
    assert_eq!(on, off, "seeding the memo changed the answer");
    let (_, c) = run(&g, q);
    let seeded = get(&c, "interp.pattern turned to the end its first-hop memo can answer")
        + get(&c, "interp.pattern kept the end its first-hop memo can answer");
    assert!(seeded >= 1, "no row seeded the memo");
    assert!(get(&c, MEMO) >= 10, "the seeded memo was reused {} times", get(&c, MEMO));
}
