#![allow(non_snake_case)]
//! A LOCAL REPRODUCTION OF THE SNB `balanced` SF10 STALL, in 0.08 s.
//!
//! At SF10 that profile dips to a throughput floor of 0.00 every ~6 s, and a
//! per-statement trace names the culprit: a 3,716 ms `is7-replies` against a
//! 0.516 ms instance of the SAME query does NO extra work — 15 store gets,
//! every one a cache HIT, one hop — but carries, once each:
//!
//!     1  index.overlay folds
//!     1  graph.range index caught up
//!     1  derived.members view caught up
//!     2  derived.snapshot published
//!
//! It is slow because it performs MAINTENANCE on the query thread.
//! `a_reader_does_not_fold_the_index_base` already pins that a reader must
//! not do this — "the fold is correct and must keep happening; what must not
//! happen is a READER doing it" — and its own header calls O(base) copying on
//! a query thread "the SF10 shape". That test passes. This one does not.
//!
//! THE TRIGGER IS ONE EXTRA INDEXED SEEK BEFORE THE WRITES — and it does not
//! have to write anything. Bisected:
//!
//! ```text
//! nothing (control)                          folds = 0
//! MATCH (m:Message {id:1}),(c:Comment{id:2}) RETURN 1    folds = 1  <- a READ
//! ...same MATCH + CREATE a relationship                  folds = 1
//! MATCH (m:Message {id:1}) CREATE (m)-[:SELF]->(m)       folds = 1
//! CREATE (:Xx)-[:R]->(:Yy)      (a rel, NO match)        folds = 0
//! CREATE (:Zz {q: 1})           (a node)                 folds = 0
//! ```
//!
//! A relationship with no MATCH does nothing; a MATCH with no write is
//! enough. So it is read-triggered index state, not a write.
//!
//! Also ruled out, each by flipping one thing: the hop in the read (a seed
//! with a hop on it does not fold), a sibling index over the same property (a
//! LONE index folds just the same), the dual `:Message:Comment` label, the
//! sibling being STALE (catching it up first leaves the fold), and the
//! `FOLD_AT` threshold itself (without the extra seek, 2,000 to 3,000 inserts
//! all fold ZERO times, so this is not a fixture sitting near a boundary).
//!
//! IGNORED, NOT DELETED: these fail today. They describe the defect rather
//! than the current behaviour, so they are the acceptance test for the fix —
//! remove the `#[ignore]` when a reader no longer folds.
//!
//!   cargo test -p engram-graph --test a_hop_seed_does_not_fold_the_index_base -- --ignored

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, QueryResult, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> QueryResult {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}
fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}
fn count_of(t: &engram_observe::Trace, name: &str) -> u64 {
    t.counters().get(name).copied().unwrap_or(0)
}

const FOLDS: &str = "index.overlay folds";
const SEEK: &str = "MATCH (m:Message {id: 1}) RETURN m.id";

/// `early_seek` is the variable under test — one extra indexed MATCH before
/// the writes. Everything else is the existing test's fixture: a declared
/// index and enough pending writes that a catch-up could cross `FOLD_AT`.
fn corpus(early_seek: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX m_id FOR (n:Message) ON (n.id)");
    run(&g, "CREATE (:Message {id: 1})");
    run(&g, "CREATE (:Message:Comment {id: 2})");
    if early_seek {
        // A READ. It writes nothing; `CREATE (:Xx)-[:R]->(:Yy)` in its place
        // leaves the later reader folding ZERO times.
        run(
            &g,
            "MATCH (m:Message {id: 1}), (c:Comment {id: 2}) RETURN 1",
        );
    }
    let _ = run(&g, SEEK); // build the index
    for i in 0..2_050 {
        run(&g, &format!("CREATE (:Message {{id: {}}})", 1_000 + i));
    }
    g
}

fn folds_for(rel: bool) -> (u64, engram_observe::Trace) {
    let g = corpus(rel);
    let (_, t) = engram_observe::with_trace(|| {
        let _ = run(&g, SEEK);
    });
    (count_of(&t, FOLDS), t)
}

/// WHAT THE TWO ARMS ACTUALLY COST — and why neither is simply "the bug".
///
/// It is tempting to assert `folds == 0` here. DO NOT: the arm that folds is
/// the CHEAPER one locally.
///
/// ```text
/// control (no early seek)   folds=0  builds=1  caught_up=0  store.gets=2054
/// with early seek           folds=1  builds=0  caught_up=1  store.gets=1
/// ```
///
/// The control avoids the fold by REBUILDING, which is also O(base) and here
/// costs 2,054 store gets against the folding arm's 1. So the fold counter is
/// telling us WHICH path ran, not which is expensive, and a test that drove
/// `folds` to zero would drive the engine onto the rebuild path.
///
/// The SF10 cost lives somewhere this fixture cannot show: there the base is
/// millions of rows, so `folded()` clones it in MEMORY — work that never
/// appears as a store get and that a 2,050-row base makes free. This file
/// therefore reproduces the code PATH and not the COST, which is why it
/// asserts only what it can see.
#[test]
fn the_early_seek_decides_which_maintenance_path_a_reader_takes() {
    let (folds_no, t_no) = folds_for(false);
    let (folds_yes, t_yes) = folds_for(true);
    assert_eq!(folds_no, 0, "the control folded: {:?}", t_no.counters());
    assert_eq!(
        folds_yes,
        1,
        "one extra indexed seek should put the reader on the catch-up+fold          path: {:?}",
        t_yes.counters()
    );
    // The point of the file: EITHER WAY the reader does O(base) work on its
    // own thread — a fold in one arm, a rebuild in the other.
    let builds_no = count_of(&t_no, "graph.range index builds");
    assert_eq!(
        builds_no,
        1,
        "the control must be REBUILDING, or the claim that both arms pay          O(base) is wrong: {:?}",
        t_no.counters()
    );
}

/// IS THE REL WRITE A MECHANISM, OR JUST A FEW MORE LOG ENTRIES?
///
/// ~2,050 inserts is where `added + removed` crosses `FOLD_AT` (4,096), so
/// the control sits just BELOW the threshold by construction. If the
/// rel-creating write merely adds entries and tips it over, then raising the
/// insert count alone reproduces the fold WITHOUT the relationship — and the
/// "trigger" is a threshold artifact, not a mechanism.
#[test]
#[ignore = "diagnostic: prints, asserts nothing"]
fn does_the_control_fold_if_we_simply_write_more() {
    for n in [2_000usize, 2_050, 2_100, 2_200, 2_500, 3_000] {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        ddl(&g, "CREATE INDEX m_id FOR (n:Message) ON (n.id)");
        run(&g, "CREATE (:Message {id: 1})");
        run(&g, "CREATE (:Message:Comment {id: 2})");
        let _ = run(&g, SEEK);
        for i in 0..n {
            run(&g, &format!("CREATE (:Message {{id: {}}})", 1_000 + i));
        }
        let (_, t) = engram_observe::with_trace(|| {
            let _ = run(&g, SEEK);
        });
        println!("no-rel, {n:5} inserts -> folds = {}", count_of(&t, FOLDS));
    }
}

/// IS IT THE STALE SIBLING? The rel write seeks `(c:Comment {id: 2})` on an
/// UNDECLARED label, which builds an implicit `:Comment`-scoped index over
/// `id`. That sibling then holds an old snapshot, and fix 116's guard keeps
/// the shared property log alive for it — so the `:Message.id` reader finds a
/// large coverable delta and folds, where without the sibling the log prunes
/// and the catch-up declines instead.
///
/// If that is the chain, CATCHING THE SIBLING UP before the Message seek
/// should let the log prune and the fold disappear.
#[test]
#[ignore = "diagnostic: prints, asserts nothing"]
fn does_catching_the_sibling_up_remove_the_fold() {
    for (name, touch_sibling_after) in [("sibling left stale", false), ("sibling caught up", true)]
    {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        ddl(&g, "CREATE INDEX m_id FOR (n:Message) ON (n.id)");
        run(&g, "CREATE (:Message {id: 1})");
        run(&g, "CREATE (:Message:Comment {id: 2})");
        run(
            &g,
            "MATCH (m:Message {id: 1}), (c:Comment {id: 2}) CREATE (c)-[:REPLY_OF]->(m)",
        );
        let _ = run(&g, SEEK);
        for i in 0..2_050 {
            run(&g, &format!("CREATE (:Message {{id: {}}})", 1_000 + i));
        }
        if touch_sibling_after {
            // Drive the sibling's own catch-up before the Message seek.
            let _ = run(&g, "MATCH (c:Comment {id: 2}) RETURN c.id");
        }
        let (_, t) = engram_observe::with_trace(|| {
            let _ = run(&g, SEEK);
        });
        println!(
            "{name:20} -> folds = {}, log-kept-for-sibling = {}",
            count_of(&t, FOLDS),
            count_of(&t, "graph.property log kept for an older sibling index")
        );
    }
}

/// BISECT THE REL WRITE. It both MATCHES two indexed/labelled nodes and
/// CREATES a relationship. Which half makes a later reader fold?
#[test]
#[ignore = "diagnostic: prints, asserts nothing"]
fn which_half_of_the_rel_write_causes_the_fold() {
    let cases: &[(&str, Option<&str>)] = &[
        ("nothing (control)", None),
        (
            "match only, no create",
            Some("MATCH (m:Message {id: 1}), (c:Comment {id: 2}) RETURN 1"),
        ),
        (
            "create rel, matched",
            Some("MATCH (m:Message {id: 1}), (c:Comment {id: 2}) CREATE (c)-[:REPLY_OF]->(m)"),
        ),
        (
            "create rel, one match",
            Some("MATCH (m:Message {id: 1}) CREATE (m)-[:SELF]->(m)"),
        ),
        ("create rel, no match", Some("CREATE (:Xx)-[:R]->(:Yy)")),
        ("create node only", Some("CREATE (:Zz {q: 1})")),
    ];
    for (name, stmt) in cases {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        ddl(&g, "CREATE INDEX m_id FOR (n:Message) ON (n.id)");
        run(&g, "CREATE (:Message {id: 1})");
        run(&g, "CREATE (:Message:Comment {id: 2})");
        if let Some(q) = stmt {
            run(&g, q);
        }
        let _ = run(&g, SEEK);
        for i in 0..2_050 {
            run(&g, &format!("CREATE (:Message {{id: {}}})", 1_000 + i));
        }
        let (_, t) = engram_observe::with_trace(|| {
            let _ = run(&g, SEEK);
        });
        println!("{name:24} -> folds = {}", count_of(&t, FOLDS));
    }
}

/// IS "NO FOLD" ACTUALLY CHEAP? A fold is O(base); so is a REBUILD. If the
/// control avoids the fold only by rebuilding instead, both arms pay O(base)
/// and the fold counter is measuring bookkeeping, not cost.
#[test]
#[ignore = "diagnostic: prints, asserts nothing"]
fn does_the_control_rebuild_instead_of_folding() {
    for (name, early) in [
        ("control (no early seek)", false),
        ("with early seek", true),
    ] {
        let g = corpus(early);
        let (_, t) = engram_observe::with_trace(|| {
            let _ = run(&g, SEEK);
        });
        println!(
            "{name:26} folds={} builds={} caught_up={} store.gets={}",
            count_of(&t, FOLDS),
            count_of(&t, "graph.range index builds"),
            count_of(&t, "graph.range index caught up"),
            count_of(&t, "store.gets"),
        );
    }
}

/// THE LEVER, and the claim it rests on: a bigger threshold gives FEWER
/// folds, not dearer ones.
///
/// `folded()` walks and clones the whole base, so its cost is O(base) however
/// much overlay it collapses. That is the whole argument for
/// `--range-fold-at`: at SF10 a reader's fold is 3.7 s inside a query whose
/// p95 is 1.25 ms, and a reader is the only thing that folds a range index.
#[test]
fn raising_the_fold_threshold_removes_the_reader_fold() {
    let g = corpus(true);
    let (_, before) = engram_observe::with_trace(|| {
        let _ = run(&g, SEEK);
    });
    assert_eq!(
        count_of(&before, FOLDS),
        1,
        "the fixture must fold at the default threshold or this proves nothing"
    );

    // Same corpus, threshold raised past what this overlay reaches.
    let g = corpus(true);
    g.set_range_fold_at(1_000_000);
    let (_, after) = engram_observe::with_trace(|| {
        let _ = run(&g, SEEK);
    });
    assert_eq!(
        count_of(&after, FOLDS),
        0,
        "raising the threshold must move the reader off the fold: {:?}",
        after.counters()
    );
    // AND IT MUST STILL CATCH UP — a threshold that skipped the catch-up
    // would be answering from a stale index, which is a wrong answer and not
    // a faster one.
    assert_eq!(
        count_of(&after, "graph.range index caught up"),
        1,
        "the catch-up must still run: {:?}",
        after.counters()
    );
}

/// 0 restores the built-in, so an operator cannot accidentally disable
/// folding by passing a falsy value.
#[test]
fn zero_restores_the_built_in_threshold() {
    let g = corpus(true);
    g.set_range_fold_at(1_000_000);
    g.set_range_fold_at(0);
    assert_eq!(g.range_fold_at(), engram_store::RangeIndex::FOLD_AT);
    let (_, t) = engram_observe::with_trace(|| {
        let _ = run(&g, SEEK);
    });
    assert_eq!(
        count_of(&t, FOLDS),
        1,
        "0 must behave exactly like the default: {:?}",
        t.counters()
    );
}

/// THE INVARIANT THE LEVER MUST NOT BREAK: the answer is identical either
/// way. A fold changes an index's layout and nothing it says.
#[test]
fn the_answer_is_the_same_at_either_threshold() {
    for fold_at in [0usize, 1_000_000] {
        let g = corpus(true);
        g.set_range_fold_at(fold_at);
        let hit = run(&g, "MATCH (m:Message {id: 1500}) RETURN m.id");
        assert_eq!(
            hit.rows.len(),
            1,
            "fold_at={fold_at}: lost a row that exists"
        );
        let miss = run(&g, "MATCH (m:Message {id: 999999}) RETURN m.id");
        assert_eq!(
            miss.rows.len(),
            0,
            "fold_at={fold_at}: found a row that does not"
        );
        let all = run(&g, "MATCH (m:Message) RETURN count(m)");
        assert_eq!(
            all.rows.len(),
            1,
            "fold_at={fold_at}: count returned no row"
        );
    }
}
