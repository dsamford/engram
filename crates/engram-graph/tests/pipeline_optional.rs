#![allow(non_snake_case)]
//! Differential tests for the composable columnar pipeline's OPTIONAL MATCH
//! operator (Phase 4b2 of `pipeline::plan_and_run_columnar`) — the LEFT JOIN with
//! null-fill. The contract is the same as the other `pipeline_*` suites: for
//! every OPTIONAL shape the pipeline accepts, running with
//! `set_columnar_scans(true)` (the outer chunk + per-outer-row left join +
//! shared tail) must equal `set_columnar_scans(false)` (the per-tuple
//! `exec_match` optional branch + aggregate/project) — the full ROW SET *and its
//! order*, byte-for-byte — and for every shape it declines, the general path
//! answers and the two still agree.
//!
//! THE load-bearing fact under test is the NULL-FILL: for each outer row whose
//! optional pattern produces ZERO matches, exactly ONE row is emitted with every
//! optional-introduced var bound to null. That is what makes `count(post)` over a
//! post-less forum 0 (a group that is present with count 0, NOT absent) while
//! `count(*)` counts the null row as 1. The CANARY (`opt_canary_null_fill_...`)
//! removes that branch and shows the 0-count group VANISHES ON while present OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Forum f0..f3 (fid 0..3); Person pe0..pe2 (pid 0..2); Post p0..p5 (len 10..60).
///
/// CONTAINER_OF (post -> forum): f0 has {p0,p1}, f1 has {p2}, f2 has {}, f3 has
/// {p3,p4,p5} — so an OPTIONAL `(forum)<-[:CONTAINER_OF]-(post)` gives forum f2 a
/// NULL-fill row (`count(post)` = 0, present), f1 one match, f0 two, f3 three.
///
/// HAS_CREATOR (post -> person): pe0 wrote {p0,p1,p3}, pe1 wrote {p2,p4}, pe2
/// wrote {p5}. MEMBER_OF (person -> forum): pe0->f0, pe1->f1, pe2->f2 — so the
/// connecting-path OPTIONAL `(person)<-[:HAS_CREATOR]-(post)-[:CONTAINER_OF]->
/// (forum)` matches (pe0,f0) twice, (pe1,f1) once, and (pe2,f2) NOT AT ALL (pe2's
/// only post p5 lives in f3), giving that pair a null-fill row.
fn optg() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_forum = |fid: i64| {
        let mut p = BTreeMap::new();
        p.insert("fid".to_string(), Value::Int(fid));
        g.create_node(&["Forum".into()], &p).expect("forum")
    };
    let f = [mk_forum(0), mk_forum(1), mk_forum(2), mk_forum(3)];
    let mk_person = |pid: i64| {
        let mut p = BTreeMap::new();
        p.insert("pid".to_string(), Value::Int(pid));
        g.create_node(&["Person".into()], &p).expect("person")
    };
    let pe = [mk_person(0), mk_person(1), mk_person(2)];
    let mk_post = |len: i64| {
        let mut p = BTreeMap::new();
        p.insert("len".to_string(), Value::Int(len));
        g.create_node(&["Post".into()], &p).expect("post")
    };
    let p = [
        mk_post(10),
        mk_post(20),
        mk_post(30),
        mk_post(40),
        mk_post(50),
        mk_post(60),
    ];
    // CONTAINER_OF: post -> forum (creation order drives reverse-adjacency).
    for (post, forum) in [(0, 0), (1, 0), (2, 1), (3, 3), (4, 3), (5, 3)] {
        g.create_rel(p[post], "CONTAINER_OF", f[forum], &BTreeMap::new())
            .expect("CONTAINER_OF");
    }
    // HAS_CREATOR: post -> person.
    for (post, person) in [(0, 0), (1, 0), (2, 1), (3, 0), (4, 1), (5, 2)] {
        g.create_rel(p[post], "HAS_CREATOR", pe[person], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    // MEMBER_OF: person -> forum.
    for (person, forum) in [(0, 0), (1, 1), (2, 2)] {
        g.create_rel(pe[person], "MEMBER_OF", f[forum], &BTreeMap::new())
            .expect("MEMBER_OF");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON, then the general path OFF; return both.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the OPTIONAL left-join operator fired for `src` with columnar ON.
fn opt_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline optional runs")
        .copied()
        == Some(1)
}

/// The pipeline must ANSWER (fire) and equal the general path, row-for-row and in
/// order.
fn agrees_and_fires(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(opt_fired(g, src), "OPTIONAL operator did not fire: `{src}`");
}

/// The pipeline must DECLINE (not fire) yet still agree with the general path.
fn declines_but_agrees(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(!opt_fired(g, src), "should have DECLINED: `{src}`");
}

/// The IC5 shape: OPTIONAL expand introducing a new var `post`, grouped by the
/// outer var `forum`, `count(post)` — a forum with no post keeps a null-post row
/// so its count is 0 (present, not absent). Matched AND unmatched outer rows.
#[test]
fn opt_count_grouped_by_outer_with_zero_group() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, count(post) AS c ORDER BY fid";
    agrees_and_fires(&g, src);
    // The explicit contract: f2 (no post) is present with count 0; every forum
    // appears (none is absent), counts are 2/1/0/3.
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(0)],
            vec![Value::Int(3), Value::Int(3)],
        ],
        "a post-less forum must be present with count 0"
    );
}

/// `count(optvar)` is NOT `count(*)` when the var is nullable: the null-fill row
/// counts as 1 for `count(*)` but 0 for `count(post)`. f2 shows cs=1, cp=0.
#[test]
fn opt_count_star_counts_the_null_row_count_var_does_not() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp ORDER BY fid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert!(
        on.contains(&vec![Value::Int(2), Value::Int(1), Value::Int(0)]),
        "f2: count(*)=1 (the null row) but count(post)=0: {on:?}"
    );
}

/// A CONNECTING-PATH OPTIONAL (expand `post` then SEMIJOIN close onto the bound
/// `forum`) with null-fill: the (pe2,f2) pair has no post authored-and-contained,
/// so it keeps a null-post row (count 0). Matched pairs keep their matches.
#[test]
fn opt_connecting_path_semijoin_null_fill() {
    let g = optg();
    let src = "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
               OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(post:Post)-[:CONTAINER_OF]->(forum) \
               RETURN person.pid AS pid, count(post) AS c ORDER BY pid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(0)],
        ],
        "the pe2/f2 pair with no connecting post is present with count 0"
    );
}

/// `collect(optvar.prop)` over the optional skips nulls: a post-less forum
/// collects the empty list, a forum collects its posts' lengths in production
/// order (ON == OFF pins that order).
#[test]
fn opt_collect_skips_nulls() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, collect(post.len) AS lens ORDER BY fid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    // f2 collects [] (its only row is the null-fill row, whose post.len is null).
    assert!(
        on.contains(&vec![Value::Int(2), Value::List(vec![])]),
        "a post-less forum collects the empty list: {on:?}"
    );
}

/// `collect(optvar)` over the optional (the whole node) also skips the null-fill
/// row — a post-less forum collects [].
#[test]
fn opt_collect_node_skips_nulls() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, collect(post) AS posts ORDER BY fid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert!(
        on.contains(&vec![Value::Int(2), Value::List(vec![])]),
        "a post-less forum collects no posts: {on:?}"
    );
}

/// PROJECT of a nullable var (non-aggregating core tail): `RETURN forum.fid,
/// post.len` yields one row per (forum, post) match, and for a post-less forum a
/// single row with `post.len` = null. Multiple matches per outer row are all
/// kept (no null row); production order is ON == OFF.
#[test]
fn opt_project_nullable_var_is_null_for_unmatched() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, post.len AS plen";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    // f2 (no post) → exactly one row with a null plen.
    assert!(
        on.contains(&vec![Value::Int(2), Value::Null]),
        "an unmatched forum projects post.len = null: {on:?}"
    );
    // f0 (two posts) → two non-null rows; f2's null row is the only null one.
    let null_rows = on.iter().filter(|r| r[1] == Value::Null).count();
    assert_eq!(null_rows, 1, "exactly one forum (f2) has no post: {on:?}");
    let f0_rows = on.iter().filter(|r| r[0] == Value::Int(0)).count();
    assert_eq!(
        f0_rows, 2,
        "f0 keeps both its post rows (no null row): {on:?}"
    );
}

/// RETURN the nullable NODE itself: an unmatched outer row projects it as null.
#[test]
fn opt_project_nullable_node_is_null() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, post AS p";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert!(
        on.contains(&vec![Value::Int(2), Value::Null]),
        "an unmatched forum projects the null node: {on:?}"
    );
}

/// ORDER PRESERVED over an aggregate tail: `ORDER BY c DESC, fid ASC` sorts the
/// group rows (including the 0-count null-fill group) with the general path's
/// stable order. Both the counts and the tie-break are ON == OFF.
#[test]
fn opt_order_by_over_aggregate_preserved() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post) AS c ORDER BY c DESC, fid ASC",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post) AS c ORDER BY c DESC, fid ASC LIMIT 2",
    ] {
        agrees_and_fires(&g, src);
    }
}

/// Form A (WITH → RETURN) over the optional: group in the WITH, project the
/// aliases in the RETURN, with a post-WITH HAVING-style WHERE over the count.
#[test]
fn opt_form_a_with_return() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         WITH forum, count(post) AS c RETURN forum.fid AS fid, c ORDER BY fid",
        // A HAVING-style filter over the aggregate keeps only forums with posts —
        // the 0-count group is filtered AFTER the aggregate, not lost before it.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         WITH forum, count(post) AS c WHERE c >= 1 RETURN forum.fid AS fid, c ORDER BY fid",
    ] {
        agrees_and_fires(&g, src);
    }
}

/// An optional WHERE applied WITHIN the left join (a single-var property
/// predicate): a match failing it does not count, so a forum whose posts all fail
/// the predicate null-fills to count 0.
#[test]
fn opt_inner_where_property_predicate() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               WHERE post.len >= 40 RETURN forum.fid AS fid, count(post) AS c ORDER BY fid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    // f0 (posts 10,20) and f1 (post 30) fail the >=40 filter ⇒ count 0; f3
    // (40,50,60) keeps all three; f2 has no post at all ⇒ 0. All present.
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(0)],
            vec![Value::Int(1), Value::Int(0)],
            vec![Value::Int(2), Value::Int(0)],
            vec![Value::Int(3), Value::Int(3)],
        ],
        "posts failing the inner WHERE do not count; groups stay present"
    );
}

/// A MULTI-HOP outer chain feeding the optional (the outer is not just a bare
/// scan): `(a)-[:MEMBER_OF]->(forum)` then the connecting-path optional.
#[test]
fn opt_multihop_outer_chain() {
    let g = optg();
    let src = "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN person.pid AS pid, count(post) AS c ORDER BY pid";
    agrees_and_fires(&g, src);
}

/// A nullable var as a GROUPING key — bare, or a direct property, from either
/// leg — is ACCEPTED since fix 30 (v105): the null-fill rows group under a Null
/// key exactly as the per-tuple path groups an unmatched optional var.
#[test]
fn nullable_group_keys_fire() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN post, count(*) AS c",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN post.len AS plen, count(*) AS c ORDER BY plen",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN post, count(member) AS cm",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN member, count(post) AS cp",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         WITH forum, member.pid AS mpid, collect(DISTINCT post.len) AS lens \
         RETURN forum.fid AS fid, mpid, lens ORDER BY fid, mpid",
    ] {
        agrees_and_fires(&g, src);
    }
}

/// DECLINE set — each must fall back to the general path (not fire) yet agree.
#[test]
fn opt_declines_outside_the_class() {
    let g = optg();
    let declines: &[&str] = &[
        // (A nullable var as a bare GROUPING key is ACCEPTED since fix 30
        // (v105) — see `nullable_group_keys_fire`; a null-MAPPING key over it
        // still declines.)
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN coalesce(post.len, -1) AS plen, count(*) AS c ORDER BY plen",
        // A nullable var in ORDER BY (a core tail with LIMIT).
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY post.len LIMIT 20",
        // A nullable var in a NON-count/collect aggregate (sum).
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, sum(post.len) AS s ORDER BY fid",
        // avg over a nullable var.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, avg(post.len) AS a ORDER BY fid",
        // A SECOND OPTIONAL MATCH CHAINED off the first's nullable var (`post`
        // may be NULL for a post-less forum, and the operator never expands from
        // the null sentinel). A second OPTIONAL rooted at an OUTER var is in the
        // class — see the multi-OPTIONAL section below.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (post)-[:HAS_CREATOR]->(person:Person) \
         RETURN forum.fid AS fid, count(post) AS c ORDER BY fid",
    ];
    for src in declines {
        declines_but_agrees(&g, src);
    }
}

// ─── MULTI-OPTIONAL — one left join per OPTIONAL clause ──────────────────────
//
// `[Match, Match(opt)+, tail]`: round k's combined chunk is round k+1's outer
// chunk. Each clause null-fills INDEPENDENTLY of the others — a first-leg match
// survives a second-leg miss with only the second leg's vars null — which is
// what makes `count(post)` and `count(member)` differ per forum while `count(*)`
// counts the cross product of the two legs' rows (or 1 for a doubly-missed
// forum). Every case is the ON == OFF differential (row set AND order) plus the
// operator counter.
//
// In `optg()`: leg 1 `(forum)<-[:CONTAINER_OF]-(post)` is EMPTY for f2 only;
// leg 2 `(forum)<-[:MEMBER_OF]-(member)` is EMPTY for f3 only (pe0→f0, pe1→f1,
// pe2→f2). So f2 is a first-leg miss, f3 a second-leg miss, f0/f1 match both.

/// The whole two-leg matrix over an outer var: `count(*)`, `count(leg1)`,
/// `count(leg2)`, `collect(legvar)`, grouped by an outer prop with ORDER BY,
/// per-leg WHERE, and the core (non-aggregating) projection.
#[test]
fn multi_opt_two_legs_rooted_at_outer_matrix() {
    let g = optg();
    let two = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) ";
    for tail in [
        "RETURN count(*) AS c",
        "RETURN count(post) AS c",
        "RETURN count(member) AS c",
        "RETURN count(*) AS cs, count(post) AS cp, count(member) AS cm",
        "RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid",
        "RETURN forum.fid AS fid, collect(post.len) AS lens, collect(member.pid) AS pids ORDER BY fid",
        "RETURN forum.fid AS fid, collect(post) AS posts, collect(member) AS members ORDER BY fid",
        "RETURN forum.fid AS fid, count(post) AS cp ORDER BY cp DESC, fid ASC",
        "RETURN forum.fid AS fid, count(member) AS cm ORDER BY cm DESC, fid ASC LIMIT 2",
        // Core tail: the per-forum cross product of the two legs' rows, a null
        // in exactly the missed leg's column.
        "RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid",
        "RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid ORDER BY fid LIMIT 10",
        // Form A: aggregate in the WITH, project in the RETURN, HAVING-style
        // filter over one leg's count.
        "WITH forum, count(post) AS cp, count(member) AS cm RETURN forum.fid AS fid, cp, cm ORDER BY fid",
        "WITH forum, count(post) AS cp, count(member) AS cm WHERE cm >= 1 RETURN forum.fid AS fid, cp, cm ORDER BY fid",
    ] {
        agrees_and_fires(&g, &format!("{two}{tail}"));
    }
    // The explicit contract: per forum `count(*)` is leg1 × leg2 rows (1 for a
    // doubly-missed forum), `count(post)` / `count(member)` count only their
    // own leg's real rows. f2: no post, one member → cs 1, cp 0, cm 1. f3:
    // three posts, no member → cs 3, cp 3, cm 0.
    let (on, _) = both(
        &g,
        &format!(
            "{two}RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid"
        ),
        BTreeMap::new(),
    );
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(2), Value::Int(2), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1), Value::Int(0), Value::Int(1)],
            vec![Value::Int(3), Value::Int(3), Value::Int(3), Value::Int(0)],
        ],
        "each leg null-fills independently: {on:?}"
    );
}

/// Per-leg WHERE inside each left join, dialling first-leg-empty, second-leg-
/// empty and BOTH-empty forums in one query: leg 1 keeps only `len >= 40`
/// posts (f0, f1 lose theirs; f2 has none), leg 2 drops member pe2 (f2 loses
/// its member; f3 has none). f2 is therefore doubly missed (one all-null row),
/// f0/f1 first-leg missed, f3 second-leg missed.
#[test]
fn multi_opt_per_leg_where_first_second_and_both_empty() {
    let g = optg();
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) WHERE post.len >= 40 \
               OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE member.pid <> 2 \
               RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(1), Value::Int(0), Value::Int(1)],
            vec![Value::Int(1), Value::Int(1), Value::Int(0), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1), Value::Int(0), Value::Int(0)],
            vec![Value::Int(3), Value::Int(3), Value::Int(3), Value::Int(0)],
        ],
        "a doubly-missed forum keeps ONE all-null row (cs 1, cp 0, cm 0): {on:?}"
    );
    // The core tail over the same legs: f2's single row is null in BOTH
    // optional columns.
    let core = "MATCH (forum:Forum) \
                OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) WHERE post.len >= 40 \
                OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE member.pid <> 2 \
                RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid";
    agrees_and_fires(&g, core);
    let (on, _) = both(&g, core, BTreeMap::new());
    assert!(
        on.contains(&vec![Value::Int(2), Value::Null, Value::Null]),
        "f2 misses both legs → one row null in both optional columns: {on:?}"
    );
}

/// EVERY forum misses BOTH legs (each leg's WHERE is unsatisfiable): one
/// all-null row per outer row, `count(*)` = the outer row count, both
/// per-leg counts 0.
#[test]
fn multi_opt_every_outer_row_misses_both_legs() {
    let g = optg();
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) WHERE post.len >= 100 \
               OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE member.pid >= 5 \
               RETURN count(*) AS cs, count(post) AS cp, count(member) AS cm";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on,
        vec![vec![Value::Int(4), Value::Int(0), Value::Int(0)]],
        "four all-null rows: {on:?}"
    );
}

/// THREE OPTIONAL legs rooted at the outer var, the third with its own WHERE —
/// the per-clause loop is not a two-leg special case.
#[test]
fn multi_opt_three_legs() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(big:Post) WHERE big.len >= 50 \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm, count(big) AS cb ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(big:Post) WHERE big.len >= 50 \
         RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid, big.len AS blen",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(big:Post) WHERE big.len >= 50 \
         RETURN count(*) AS cs",
    ] {
        agrees_and_fires(&g, src);
    }
    // f3: 3 posts × 1 (no member → null) × 2 big posts (50, 60) = 6 rows.
    let (on, _) = both(
        &g,
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(big:Post) WHERE big.len >= 50 \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm, count(big) AS cb ORDER BY fid",
        BTreeMap::new(),
    );
    assert!(
        on.contains(&vec![
            Value::Int(3),
            Value::Int(6),
            Value::Int(6),
            Value::Int(0),
            Value::Int(6)
        ]),
        "f3 = 3 posts × null member × 2 big posts: {on:?}"
    );
}

/// Legs rooted at DIFFERENT outer vars of a multi-hop outer chain, and a
/// second leg that is a CONNECTING PATH closing (semijoin) onto an outer var —
/// the later round's first hop resets isomorphism and its multi-hop leg tracks
/// it, exactly as a single OPTIONAL's does.
#[test]
fn multi_opt_legs_rooted_at_different_outer_vars_and_connecting_second_leg() {
    let g = optg();
    for src in [
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(authored:Post) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp, count(authored) AS ca ORDER BY pid",
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(ap:Post)-[:CONTAINER_OF]->(forum) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp, count(ap) AS ca ORDER BY pid",
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(ap:Post)-[:CONTAINER_OF]->(forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN person.pid AS pid, post.len AS plen, ap.len AS alen",
    ] {
        agrees_and_fires(&g, src);
    }
    // (pe2, f2): no post in f2 and no connecting post → one row, cs 1, cp 0,
    // ca 0. (pe0, f0): 2 posts × 2 connecting posts = 4 rows.
    let (on, _) = both(
        &g,
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(ap:Post)-[:CONTAINER_OF]->(forum) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp, count(ap) AS ca ORDER BY pid",
        BTreeMap::new(),
    );
    assert_eq!(
        on,
        vec![
            vec![Value::Int(0), Value::Int(4), Value::Int(4), Value::Int(4)],
            vec![Value::Int(1), Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1), Value::Int(0), Value::Int(0)],
        ],
        "independent null-fill per leg over a multi-hop outer: {on:?}"
    );
}

/// The interleaved MERGE across rounds: a multi-match first leg followed by
/// forums missing it, then a second leg that misses a different forum — the
/// null-fills of each round land in place, never at the end (ON == OFF is the
/// order gate).
#[test]
fn multi_opt_merge_order_across_rounds() {
    let g = forums_with_posts(&[2, 0, 0, 1]);
    // A second leg over a type that was never minted: every forum misses it,
    // so round 2 null-fills EVERY round-1 row (matches and null-fills alike).
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
               RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(on.len(), 5, "2 + null + null + 1 rows: {on:?}");
    assert!(
        on.iter().all(|r| r[2] == Value::Null),
        "the second leg misses every row: {on:?}"
    );
    let null_idxs: Vec<usize> = on
        .iter()
        .enumerate()
        .filter(|(_, r)| r[1] == Value::Null)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(null_idxs, vec![2, 3], "round-1 null-fills stay in place: {on:?}");
}

/// The multi-OPTIONAL decline set — each falls back to the general path (the
/// operator does not fire) yet agrees with it.
#[test]
fn multi_opt_declines() {
    let g = optg();
    let declines: &[&str] = &[
        // A later OPTIONAL ROOTED at an earlier OPTIONAL's nullable var (chained).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (post)-[:HAS_CREATOR]->(person:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(person) AS cpe ORDER BY fid",
        // A later OPTIONAL CLOSING (semijoin) onto an earlier OPTIONAL's nullable var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post)-[:HAS_CREATOR]->(member) \
         RETURN forum.fid AS fid, count(member) AS cm, count(post) AS cp ORDER BY fid",
        // A later OPTIONAL's WHERE reading an earlier OPTIONAL's nullable var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE post.len > 10 \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // A VARIABLE-LENGTH hop in the FIRST leg.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF*1..2]-(post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // A VARIABLE-LENGTH hop in the SECOND leg.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF*1..2]-(member) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // (A nullable GROUPING key — bare, or a direct property — is an
        // ACCEPTED shape since fix 30 (v105): see `nullable_group_keys_fire`.)
        // A second-leg nullable var in a non-count/collect aggregate.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, sum(member.pid) AS s ORDER BY fid",
        // A second-leg nullable var as an ORDER BY key in the core tail.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, member.pid AS mpid ORDER BY member.pid LIMIT 5",
    ];
    for src in declines {
        declines_but_agrees(&g, src);
    }
}

/// A WHOLE-NODE `x IN <list>` membership inside the optional now FIRES: the
/// node-identity primitive loads the needle var's id-only node column
/// (`NODE_IDENTITY_KEY`), so `eval_column` vectorises the membership by id.
/// `post IN [10, 20]` (a node vs ints) is uniformly false — byte-identical to the
/// interp, count 0 for every forum — and the OPTIONAL operator fires.
#[test]
fn opt_where_whole_node_in_over_optional_fires() {
    let g = optg();
    agrees_and_fires(
        &g,
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         WHERE post IN [10, 20] RETURN forum.fid AS fid, count(post) AS c ORDER BY fid",
    );
}

/// A PROPERTY-membership `post.len IN [<const list>]` WHERE inside the optional:
/// the needle is `eval_column`-able, so the shared single-var WHERE recognizer
/// (the one place the read chain and the OPTIONAL pattern agree on tractable
/// forms) now accepts it — the OPTIONAL operator fires and equals the general
/// path. (A whole-node membership still declines; see the case above.)
#[test]
fn opt_where_in_property_over_optional_fires() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               WHERE post.len IN [10, 20] RETURN forum.fid AS fid, count(post) AS c ORDER BY fid";
    agrees_and_fires(&g, src);
}

/// A non-OPTIONAL two-MATCH query (both required) is NOT this operator's shape and
/// must decline (the OPTIONAL flag is what the recognizer keys on).
#[test]
fn opt_declines_non_optional_second_match() {
    let g = optg();
    let src = "MATCH (forum:Forum) MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, count(post) AS c ORDER BY fid";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(
        !opt_fired(&g, src),
        "a required second MATCH is not the OPTIONAL shape"
    );
}

/// CANARY — the null-fill branch is load-bearing. This test's f2 group is present
/// with count 0 ONLY because a post-less outer row emits a null-fill row. With
/// the null-fill branch removed the f2 group VANISHES ON while present OFF, so
/// this assertion FAILS. (Manually verified: comment out the `if live.is_empty()`
/// null-fill push in `run_optional` and this test's `assert_eq` breaks — the ON
/// result loses `[2, 0]`.) Restoring the branch makes it pass again.
#[test]
fn opt_canary_null_fill_keeps_the_zero_group() {
    let g = optg();
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, count(post) AS c ORDER BY fid";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "null-fill differential");
    assert!(
        on.contains(&vec![Value::Int(2), Value::Int(0)]),
        "the 0-count group is present ONLY via null-fill: {on:?}"
    );
    assert!(opt_fired(&g, src), "the OPTIONAL operator must fire");
}

/// Census / unrelated shapes still route elsewhere: a plain single-MATCH
/// aggregate must NOT be claimed by the OPTIONAL operator (no optional runs).
#[test]
fn opt_unrelated_shapes_do_not_fire_optional() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) RETURN count(*) AS c",
        "MATCH (forum:Forum)<-[:CONTAINER_OF]-(post:Post) RETURN forum.fid AS fid, count(post) AS c ORDER BY fid",
    ] {
        assert!(!opt_fired(&g, src), "OPTIONAL must not claim: `{src}`");
    }
}

// ─── VECTORIZED-MERGE STRESS (the interleaved null-fill) ─────────────────────
//
// The vectorized OPTIONAL runs the optional pattern over the WHOLE outer chunk
// in one pass, then MERGES: walk the outer rows in order, emitting each outer
// row's contiguous block of surviving rows, or ONE null-fill row when its block
// is empty. These tests stress the merge directly — a null-fill that must land
// BETWEEN two match-blocks, an all-null-filled set, an all-matched set — with
// the differential `on == off` (full row set AND order) as the primary gate.

/// N forums (scan order = creation order, `fid` 0..N), forum i carrying
/// `counts[i]` posts via CONTAINER_OF. Lets a test dial the exact adjacency of
/// match-blocks and zero-blocks the merge must interleave.
fn forums_with_posts(counts: &[usize]) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Create every forum FIRST so the label scan (ascending id = creation order)
    // visits them in `counts` order.
    let forums: Vec<_> = (0..counts.len())
        .map(|i| {
            let mut p = BTreeMap::new();
            p.insert("fid".to_string(), Value::Int(i as i64));
            g.create_node(&["Forum".into()], &p).expect("forum")
        })
        .collect();
    let mut next_len = 0i64;
    for (i, &c) in counts.iter().enumerate() {
        for _ in 0..c {
            let mut p = BTreeMap::new();
            p.insert("len".to_string(), Value::Int(next_len));
            let post = g.create_node(&["Post".into()], &p).expect("post");
            g.create_rel(post, "CONTAINER_OF", forums[i], &BTreeMap::new())
                .expect("CONTAINER_OF");
            next_len += 1;
        }
    }
    g
}

/// MERGE STRESS + CANARY TARGET: an outer row with MULTIPLE matches immediately
/// followed by outer rows with ZERO — the null-fills must land BETWEEN the
/// multi-match block and the next match, never after it. This is the order test
/// the null-fill-position CANARY breaks: emit the null-fills at the END instead
/// of interleaved and `on == off` (via `agrees_and_fires`), the `null_idxs`
/// assertion, AND the "last row is a real match" assertion all FAIL.
#[test]
fn opt_merge_multi_match_block_then_zero_blocks() {
    let g = forums_with_posts(&[2, 0, 0, 1]);
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, post.len AS plen";
    agrees_and_fires(&g, src); // full-order on == off is the real gate
    let (on, _) = both(&g, src, BTreeMap::new());
    // fid 0 → 2 matches, fids 1 and 2 → 0, fid 3 → 1 match. 2 + null + null + 1.
    assert_eq!(
        on.len(),
        5,
        "two matches, two null-fills, one match: {on:?}"
    );
    let null_idxs: Vec<usize> = on
        .iter()
        .enumerate()
        .filter(|(_, r)| r[1] == Value::Null)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        null_idxs,
        vec![2, 3],
        "the null-fills interleave immediately after the 2-match block: {on:?}"
    );
    assert_ne!(
        on.last().expect("non-empty")[1],
        Value::Null,
        "fid 3's real match is LAST — a null-fill dumped at the end would break this: {on:?}"
    );
}

/// An ALL-UNMATCHED outer set: every forum has zero posts, so every outer row
/// null-fills to exactly one row. The merge emits a null-fill per outer row and
/// nothing else — `on == off` and every row is null.
#[test]
fn opt_merge_all_unmatched_every_row_null_filled() {
    let g = forums_with_posts(&[0, 0, 0]);
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, post.len AS plen";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(on.len(), 3, "three forums, each ONE null-fill row: {on:?}");
    assert!(
        on.iter().all(|r| r[1] == Value::Null),
        "every outer row is null-filled: {on:?}"
    );
}

/// An ALL-MATCHED outer set: every forum has at least one post, so NO outer row
/// null-fills. The merge emits only match-blocks — `on == off`, no null row.
#[test]
fn opt_merge_all_matched_no_null_fill() {
    let g = forums_with_posts(&[1, 2, 1]);
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS fid, post.len AS plen";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(on.len(), 4, "1 + 2 + 1 matches, NO null-fill row: {on:?}");
    assert!(
        on.iter().all(|r| r[1] != Value::Null),
        "no outer row null-fills when every forum has a post: {on:?}"
    );
}

/// RELATIONSHIP-ISOMORPHISM RESET at the optional's FIRST hop. The outer is a
/// 2-hop chain, so its row carries `used_rels = [r1, r2]`; the optional is its
/// OWN 2-hop path that legally re-uses r1 (isomorphism is per optional path, not
/// across the outer boundary — the OPTIONAL is a separate clause). The oracle
/// keeps that match; the vectorized pass must too, which is exactly what forcing
/// `reset` on the first optional hop guarantees (without it the optional would
/// inherit the outer's `[r1, r2]` and wrongly drop the r1-reusing walk). A
/// directed triangle a→b→c→a makes the outer walk a→b→c and the optional walk
/// c→a→b reuse r1 (a→b).
#[test]
fn opt_reset_iso_at_first_hop_allows_outer_rel_reuse() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |labels: &[&str], v: i64| {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), Value::Int(v));
        let ls: Vec<String> = labels.iter().map(|s| (*s).to_string()).collect();
        g.create_node(&ls, &p).expect("node")
    };
    let a = mk(&["N", "Start"], 0);
    let b = mk(&["N"], 1);
    let c = mk(&["N"], 2);
    let empty = BTreeMap::new();
    g.create_rel(a, "R", b, &empty).expect("r1"); // a -> b
    g.create_rel(b, "R", c, &empty).expect("r2"); // b -> c
    g.create_rel(c, "R", a, &empty).expect("r3"); // c -> a
    // Outer a→b→c binds (a, b, m=c) with used_rels [r1, r2]; the optional c→a→b
    // (via r3 then r1) re-uses r1 — kept iff the first optional hop resets iso.
    let src = "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
               OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
               RETURN a.nid AS a, count(e) AS c";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert_eq!(
        on,
        vec![vec![Value::Int(0), Value::Int(1)]],
        "the optional match that re-uses the outer's r1 is KEPT (count 1, not 0): {on:?}"
    );
}

// ─── OPERATOR D — the OPTIONAL FOLD ──────────────────────────────────────────
//
// Under an all-`count(*)` tail an OPTIONAL leg whose vars nothing reads need not
// be produced at all: its matches are COUNTED per outer row and the row's weight
// becomes `max(1, matches)` — the `max` being the left join itself, since an
// outer row with no match still emits the null-fill row that `count(*)` counts.
// The differential twin is the fold lever off, which expands and merges the leg
// exactly as before.

/// One row set per engine path.
type Rows = Vec<Vec<Value>>;

/// fold ON / fold OFF / columnar OFF.
fn fold_triple(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let on = rows(g, src, BTreeMap::new());
    engram_graph::pipeline::set_count_fold(false);
    let fold_off = rows(g, src, BTreeMap::new());
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(false);
    let general = rows(g, src, BTreeMap::new());
    g.set_columnar_scans(true);
    (on, fold_off, general)
}

/// How many OPTIONAL legs folded for `src` (one counter per folded clause).
fn legs_folded(g: &Graph, src: &str) -> u64 {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline optional fold")
        .copied()
        .unwrap_or(0)
}

/// Every path agrees and `legs` clauses folded.
fn opt_folds(g: &Graph, src: &str, legs: u64) -> Vec<Vec<Value>> {
    let (on, fold_off, general) = fold_triple(g, src);
    assert_eq!(on, general, "optional fold ON vs general: `{src}`");
    assert_eq!(fold_off, general, "optional fold OFF vs general: `{src}`");
    assert_eq!(legs_folded(g, src), legs, "folded legs: `{src}`");
    assert!(opt_fired(g, src), "the OPTIONAL operator must answer: `{src}`");
    on
}

/// A leg with ZERO matches must still weigh ONE: forum f2 contains no post, so
/// its null-fill row counts. `count(*)` over the fold is the general path's.
#[test]
fn opt_fold_counts_the_null_fill_row_as_one() {
    let g = optg();
    // f0 → 2 posts, f1 → 1, f2 → 0 (one null row), f3 → 3: 2 + 1 + 1 + 3 = 7.
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN count(*) AS n";
    assert_eq!(opt_folds(&g, src, 1), vec![vec![Value::Int(7)]]);
    // The same leg two hops deep (post → its creator), and a CONNECTING leg
    // that closes onto a SECOND outer var (pe2/f2 matches nothing → 1).
    let deep = "MATCH (forum:Forum) \
                OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post)-[:HAS_CREATOR]->(w:Person) \
                RETURN count(*) AS n";
    assert_eq!(opt_folds(&g, deep, 1), vec![vec![Value::Int(7)]]);
    let connecting = "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
                      OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(post:Post)\
                      -[:CONTAINER_OF]->(forum) RETURN count(*) AS n";
    // (pe0,f0) → 2, (pe1,f1) → 1, (pe2,f2) → 0 ⇒ 1: total 4.
    assert_eq!(opt_folds(&g, connecting, 1), vec![vec![Value::Int(4)]]);
}

/// TWO legs, each folded independently: the weights multiply, which is the
/// cartesian product the two merges would have produced.
#[test]
fn opt_fold_multiplies_two_legs() {
    let g = optg();
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
               RETURN count(*) AS n";
    // posts × members with max(1,·): f0 2×1, f1 1×1, f2 max(1,0)×1, f3 3×max(1,0).
    assert_eq!(opt_folds(&g, src, 2), vec![vec![Value::Int(7)]]);
    // One leg foldable, the other not (`count(member)` reads its var): the
    // per-leg decision is independent, so exactly ONE folds.
    let mixed = "MATCH (forum:Forum) \
                 OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
                 OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
                 RETURN count(member) AS n";
    let (on, fold_off, general) = fold_triple(&g, mixed);
    assert_eq!(on, general);
    assert_eq!(fold_off, general);
    assert_eq!(
        legs_folded(&g, mixed),
        0,
        "a non-star site keeps EVERY leg materialised"
    );
}

/// A leg whose close targets a SIBLING branch's var — not the level's own var
/// and not an ancestor — is still `NULL_ID` when the level runs, so the leg
/// falls back to the ordinary left join and still agrees.
#[test]
fn opt_fold_declines_a_close_onto_a_sibling_leg_var() {
    let g = optg();
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post)-[:HAS_CREATOR]->(w:Person), \
               (forum)<-[:CONTAINER_OF]-(q:Post), (q)-[:HAS_CREATOR]->(w) \
               RETURN count(*) AS n";
    let (on, fold_off, general) = fold_triple(&g, src);
    assert_eq!(on, general, "columnar vs general disagree");
    assert_eq!(fold_off, general, "fold OFF vs general disagree");
    assert_eq!(legs_folded(&g, src), 0, "the sibling close declines the leg");
}

/// The folded leg re-seeds relationship isomorphism exactly as the materialised
/// one does: the optional walk may re-use a relationship the OUTER walk took.
#[test]
fn opt_fold_re_seeds_rel_iso_per_clause() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |labels: &[&str], nid: i64| {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), Value::Int(nid));
        let ls: Vec<String> = labels.iter().map(|s| (*s).to_string()).collect();
        g.create_node(&ls, &p).expect("node")
    };
    let a = mk(&["N", "Start"], 0);
    let b = mk(&["N"], 1);
    let c = mk(&["N"], 2);
    let empty = BTreeMap::new();
    g.create_rel(a, "R", b, &empty).expect("r1");
    g.create_rel(b, "R", c, &empty).expect("r2");
    g.create_rel(c, "R", a, &empty).expect("r3");
    // The outer a→b→c used r1 and r2; the optional c→a→b re-uses r1, which a
    // per-clause re-seed KEEPS — under the fold as under the merge.
    let src = "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
               OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
               RETURN count(*) AS n";
    assert_eq!(opt_folds(&g, src, 1), vec![vec![Value::Int(1)]]);
}

/// A KEYED count over a folded leg keeps FIRST-SEEN group order and every
/// group: the fold emits exactly ONE row per outer row, in outer order, so a
/// tie resolves where the merge would have put it — and a zero-match outer row
/// is still present (weight 1), unlike the inner-join fold, which DROPS a row
/// whose count is zero.
#[test]
fn opt_fold_keyed_count_keeps_first_seen_order_and_zero_groups() {
    let g = optg();
    // f3 → 3 posts, f0 → 2, f1 → 1, f2 → 0 ⇒ weight 1. The f1/f2 tie is cut by
    // the scan's first-seen order (f0, f1, f2, f3).
    let src = "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
               RETURN forum.fid AS k, count(*) AS n ORDER BY n DESC";
    let full = opt_folds(&g, src, 1);
    assert_eq!(
        full,
        vec![
            vec![Value::Int(3), Value::Int(3)],
            vec![Value::Int(0), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1)],
        ],
        "the zero-match forum keeps its group at count 1"
    );
    for lim in 1..=4 {
        let src = format!(
            "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
             RETURN forum.fid AS k, count(*) AS n ORDER BY n DESC LIMIT {lim}"
        );
        assert_eq!(
            opt_folds(&g, &src, 1),
            full[..lim].to_vec(),
            "the LIMIT {lim} prefix is first-seen decided"
        );
    }
}

/// ONE OPTIONAL clause whose leg is TWO comma paths: each is a fold ROOT, and
/// the leg's match count is their PRODUCT — the cartesian product per outer row
/// that the materialised left join produces. A zero factor makes the whole leg
/// unmatched, so the row still weighs one.
///
/// This is the multi-root arm of `fold_optional_leg` (the single-clause tests
/// above each fold ONE root, and `opt_fold_multiplies_two_legs` multiplies
/// across CLAUSES, not within one). It also pins that the second path's root
/// re-seeds relationship isomorphism: `collect_hops` marks a path boundary
/// `reset`, which is the base `hop_sum` takes for a root.
#[test]
fn opt_fold_multiplies_two_paths_of_one_leg() {
    let g = optg();
    let src = "MATCH (forum:Forum) \
               OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post), \
               (forum)<-[:MEMBER_OF]-(member:Person) \
               RETURN count(*) AS n";
    // f0: 2 posts × 1 member = 2; f1: 1 × 1 = 1; f2: 0 posts ⇒ max(1,0) = 1;
    // f3: 3 posts × 0 members ⇒ max(1,0) = 1. Total 5 — and NOT the 7 the two
    // separate clauses give, so the product is measured and not assumed.
    assert_eq!(opt_folds(&g, src, 1), vec![vec![Value::Int(5)]]);
    // Keyed, so the per-outer-row weights are visible one at a time.
    let keyed = "MATCH (forum:Forum) \
                 OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post), \
                 (forum)<-[:MEMBER_OF]-(member:Person) \
                 RETURN forum.fid AS k, count(*) AS n ORDER BY k";
    assert_eq!(
        opt_folds(&g, keyed, 1),
        vec![
            vec![Value::Int(0), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1)],
            vec![Value::Int(3), Value::Int(1)],
        ],
        "each outer row's weight is its two roots' product, floored at one"
    );
}
