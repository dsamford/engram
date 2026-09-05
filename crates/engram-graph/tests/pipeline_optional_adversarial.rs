#![allow(non_snake_case)]
//! Adversarial differential probes for the MULTI-OPTIONAL columnar admission
//! (`pipeline::recognise_optional` / `run_optional`, one `left_join_null_extend`
//! per OPTIONAL clause). Every probe is the same ON == OFF differential the
//! `pipeline_optional` suite uses; the "must decline" / "must fire" sets pin the
//! admission boundary, and the "probe" set asserts agreement only and REPORTS
//! whether the operator fired (so a silent widening of the class is visible).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The `pipeline_optional` fixture, verbatim: Forum f0..f3, Person pe0..pe2,
/// Post p0..p5 (len 10..60). CONTAINER_OF: f0 {p0,p1}, f1 {p2}, f2 {}, f3
/// {p3,p4,p5}. HAS_CREATOR: pe0 {p0,p1,p3}, pe1 {p2,p4}, pe2 {p5}. MEMBER_OF:
/// pe0->f0, pe1->f1, pe2->f2.
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
    for (post, forum) in [(0, 0), (1, 0), (2, 1), (3, 3), (4, 3), (5, 3)] {
        g.create_rel(p[post], "CONTAINER_OF", f[forum], &BTreeMap::new())
            .expect("CONTAINER_OF");
    }
    for (post, person) in [(0, 0), (1, 0), (2, 1), (3, 0), (4, 1), (5, 2)] {
        g.create_rel(p[post], "HAS_CREATOR", pe[person], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    for (person, forum) in [(0, 0), (1, 1), (2, 2)] {
        g.create_rel(pe[person], "MEMBER_OF", f[forum], &BTreeMap::new())
            .expect("MEMBER_OF");
    }
    g
}

/// Forum f0 with TWO posts and TWO members, f1 with one post and no member, f2
/// with no post and one member — the cross product per forum is observable.
fn crossg() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let f = [mk("Forum", "fid", 0), mk("Forum", "fid", 1), mk("Forum", "fid", 2)];
    let p = [mk("Post", "len", 10), mk("Post", "len", 20), mk("Post", "len", 30)];
    let m = [mk("Person", "pid", 0), mk("Person", "pid", 1), mk("Person", "pid", 2)];
    let e = BTreeMap::new();
    g.create_rel(p[0], "CONTAINER_OF", f[0], &e).unwrap();
    g.create_rel(p[1], "CONTAINER_OF", f[0], &e).unwrap();
    g.create_rel(p[2], "CONTAINER_OF", f[1], &e).unwrap();
    g.create_rel(m[0], "MEMBER_OF", f[0], &e).unwrap();
    g.create_rel(m[1], "MEMBER_OF", f[0], &e).unwrap();
    g.create_rel(m[2], "MEMBER_OF", f[2], &e).unwrap();
    g
}

/// A directed triangle a->b->c->a over `R`, `a` labelled Start — every 2-hop
/// walk from any node exists and re-uses rels another clause's walk used.
fn triangle() -> Graph {
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
    let e = BTreeMap::new();
    g.create_rel(a, "R", b, &e).unwrap();
    g.create_rel(b, "R", c, &e).unwrap();
    g.create_rel(c, "R", a, &e).unwrap();
    g
}

type Rows = Result<Vec<Vec<Value>>, String>;

fn try_rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn both(g: &Graph, src: &str) -> (Rows, Rows) {
    g.set_columnar_scans(true);
    let on = try_rows(g, src);
    g.set_columnar_scans(false);
    let off = try_rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn opt_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| try_rows(g, src));
    trace
        .counters()
        .get("interp.pipeline optional runs")
        .copied()
        == Some(1)
}

fn agrees_and_fires(g: &Graph, src: &str) {
    let (on, off) = both(g, src);
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(opt_fired(g, src), "OPTIONAL operator did not fire: `{src}`");
}

fn declines_but_agrees(g: &Graph, src: &str) {
    let (on, off) = both(g, src);
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(!opt_fired(g, src), "should have DECLINED: `{src}`");
}

/// Agreement only; report the routing so a class change is visible in the log.
/// Returns the disagreement (if any) so a probe set can report EVERY divergence
/// rather than stopping at the first.
fn agrees_report(g: &Graph, src: &str) -> Option<String> {
    let (on, off) = both(g, src);
    let fired = opt_fired(g, src);
    eprintln!("[probe] fired={fired} `{src}`\n        on ={on:?}\n        off={off:?}");
    (on != off).then(|| format!("fired={fired} `{src}`\n   on ={on:?}\n   off={off:?}"))
}

fn assert_all_agree(divergences: Vec<String>) {
    assert!(
        divergences.is_empty(),
        "{} divergence(s):\n{}",
        divergences.len(),
        divergences.join("\n")
    );
}

// ─── (1a) a later leg re-rooted at / closing onto / filtering an earlier leg ──

#[test]
fn adv_declines_later_leg_touching_earlier_nullable() {
    let g = optg();
    for src in [
        // Chained root in the SECOND PATH of a later clause (path 1 outer-rooted).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(m:Person), (post)-[:HAS_CREATOR]->(pe:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(m) AS cm, count(pe) AS cpe ORDER BY fid",
        // Same var name re-bound by the second leg (a close onto the nullable).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(post) \
         RETURN forum.fid AS fid, count(post) AS cp ORDER BY fid",
        // Two-var id predicate in the later WHERE against the earlier nullable.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE member <> post \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // Earlier leg's REL var read by the later WHERE.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[r:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE r IS NOT NULL \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // Earlier nullable read inside a scalar fn in the later WHERE.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE coalesce(post.len, 0) < 100 \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // Third leg rooted at the SECOND leg's var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (member)<-[:HAS_CREATOR]-(mp:Post) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm, count(mp) AS cmp ORDER BY fid",
        // Third leg closing onto the FIRST leg's var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(x:Person)<-[:HAS_CREATOR]-(post) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm, count(x) AS cx ORDER BY fid",
        // A later leg restating a NEW label on an outer var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum:Person)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
    ] {
        declines_but_agrees(&g, src);
    }
}

// ─── (1b) rel-uniqueness is PER CLAUSE — both legs may walk the same rel ─────

#[test]
fn adv_rel_reuse_across_multi_hop_legs_is_allowed() {
    let g = triangle();
    for src in [
        // Two 2-hop legs from the same outer var, both walking r3 then r1.
        "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
         OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
         OPTIONAL MATCH (m)-[:R]->(f)-[:R]->(h) \
         RETURN a.nid AS a, count(*) AS cs, count(e) AS ce, count(h) AS ch",
        // Second leg a 2-hop CONNECTING path closing onto the outer `b` (uses
        // r3 then r1 — r1 is the OUTER's first rel and leg 1's second).
        "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
         OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
         OPTIONAL MATCH (m)-[:R]->(x)-[:R]->(b) \
         RETURN a.nid AS a, count(*) AS cs, count(e) AS ce, count(x) AS cx",
        // Three legs, each a 3-hop cycle back onto the outer start (all three
        // rels used by every leg).
        "MATCH (a:Start) \
         OPTIONAL MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) \
         OPTIONAL MATCH (a)-[:R]->(d)-[:R]->(e)-[:R]->(a) \
         OPTIONAL MATCH (a)-[:R]->(f)-[:R]->(h)-[:R]->(a) \
         RETURN a.nid AS a, count(*) AS cs, count(c) AS cc, count(e) AS ce, count(h) AS ch",
        // Core tail over the same.
        "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
         OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
         OPTIONAL MATCH (m)-[:R]->(f)-[:R]->(h) \
         RETURN a.nid AS a, e.nid AS e, h.nid AS h",
    ] {
        agrees_and_fires(&g, src);
    }
    let (on, _) = both(
        &g,
        "MATCH (a:Start)-[:R]->(b)-[:R]->(m) \
         OPTIONAL MATCH (m)-[:R]->(d)-[:R]->(e) \
         OPTIONAL MATCH (m)-[:R]->(f)-[:R]->(h) \
         RETURN a.nid AS a, count(*) AS cs, count(e) AS ce, count(h) AS ch",
    );
    assert_eq!(
        on.unwrap(),
        vec![vec![Value::Int(0), Value::Int(1), Value::Int(1), Value::Int(1)]],
        "both legs match despite sharing every rel"
    );
    // Single-hop twins over the same rels: n^2 per forum.
    let g = optg();
    agrees_and_fires(
        &g,
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(p1:Post) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(p2:Post) \
         RETURN forum.fid AS fid, count(*) AS cs, count(p1) AS c1, count(p2) AS c2 ORDER BY fid",
    );
}

// ─── (1c) cross product order / multiplicity across rounds ───────────────────

#[test]
fn adv_cross_product_order_and_collect_multiplicity() {
    let g = crossg();
    for src in [
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, collect(post.len) AS lens, collect(member.pid) AS pids ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, post.len AS plen, member.pid AS mpid ORDER BY fid DESC LIMIT 3",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN count(*) AS cs, count(post) AS cp, count(member) AS cm",
    ] {
        agrees_and_fires(&g, src);
    }
    let (on, _) = both(
        &g,
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid",
    );
    assert_eq!(
        on.unwrap(),
        vec![
            vec![Value::Int(0), Value::Int(4), Value::Int(4), Value::Int(4)],
            vec![Value::Int(1), Value::Int(1), Value::Int(1), Value::Int(0)],
            vec![Value::Int(2), Value::Int(1), Value::Int(0), Value::Int(1)],
        ],
        "f0 = 2 posts x 2 members"
    );
}

// ─── (1d) three legs with the MIDDLE one empty ───────────────────────────────

#[test]
fn adv_three_legs_middle_empty() {
    let g = optg();
    for src in [
        // Never-minted type in the middle.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:NOPE]-(x) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(x) AS cx, count(member) AS cm ORDER BY fid",
        // Unsatisfiable WHERE in the middle.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(x:Person) WHERE x.pid > 100 \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(x) AS cx, count(member) AS cm ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:NOPE]-(x) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, post.len AS plen, x AS x, member.pid AS mpid",
        // Middle empty, LAST leg a connecting path onto an outer var.
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:NOPE]-(x) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(ap:Post)-[:CONTAINER_OF]->(forum) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp, count(x) AS cx, count(ap) AS ca ORDER BY pid",
    ] {
        agrees_and_fires(&g, src);
    }
}

// ─── (1e) pure-semijoin later leg (introduces NO var), rel vars, anon nodes ──

#[test]
fn adv_semijoin_only_leg_rel_vars_anon_nodes() {
    let g = optg();
    for src in [
        // Later leg binds NOTHING new (both ends outer): 1 row per closing edge,
        // or the row unchanged on a miss.
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)-[:MEMBER_OF]->(forum) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp ORDER BY pid",
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)<-[:HAS_CREATOR]-(forum) \
         RETURN person.pid AS pid, count(*) AS cs, count(post) AS cp ORDER BY pid",
        // Rel var introduced by the second leg.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[r:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(r) AS cr, collect(r) AS rs, count(post) AS cp ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[r:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, r AS r, post.len AS plen",
        // Rel var on a semijoin-closing later leg.
        "MATCH (person:Person)-[:MEMBER_OF]->(forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (person)-[r:MEMBER_OF]->(forum) \
         RETURN person.pid AS pid, count(r) AS cr, count(post) AS cp ORDER BY pid",
        // Anonymous mid node in the second leg.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-()-[:HAS_CREATOR]->(creator:Person) \
         RETURN forum.fid AS fid, count(*) AS cs, count(member) AS cm, count(creator) AS cc ORDER BY fid",
        // Anonymous mid node in BOTH legs (synthesised names must not collide).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-()-[:HAS_CREATOR]->(c1:Person) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-()-[:HAS_CREATOR]->(c2:Person) \
         RETURN forum.fid AS fid, count(*) AS cs, count(c1) AS a, count(c2) AS b ORDER BY fid",
        // Later clause with TWO paths, path 2 rooted at path 1's OWN var.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(m:Person), (m)<-[:HAS_CREATOR]-(mp:Post) \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(m) AS cm, count(mp) AS cmp ORDER BY fid",
        // Later leg WHERE over an OUTER var only / a two-var id pred vs outer.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE forum.fid < 2 \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) WHERE member <> forum \
         RETURN forum.fid AS fid, count(*) AS cs, count(post) AS cp, count(member) AS cm ORDER BY fid",
        // Outer WHERE + two legs + Form A HAVING on the second leg's count.
        "MATCH (forum:Forum) WHERE forum.fid <> 1 \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         WITH forum, count(post) AS cp, count(member) AS cm WHERE cm = 0 \
         RETURN forum.fid AS fid, cp, cm ORDER BY fid",
        // count(DISTINCT ...) / collect(DISTINCT ...) over nullable legs.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(DISTINCT post) AS dp, count(DISTINCT member.pid) AS dm, collect(DISTINCT post.len) AS dl ORDER BY fid",
    ] {
        agrees_and_fires(&g, src);
    }
}

// ─── (1f) null-mapping aggregate arguments: the null-fill row is NOT evaluated ─
//
// `site_push_value` short-circuits a `NULL_ID` row to `Value::Null` WITHOUT
// evaluating the site's expression. For an expression that maps null to a
// NON-null (`IS NULL`, `coalesce`, `CASE`, `AND false`, `OR true`), the
// per-tuple path counts/collects the null-fill row while the columnar path
// would skip it. `nullable_agg_ok` therefore admits a count/collect argument
// over a nullable var ONLY as the bare var or a direct `var.prop`; every other
// expression over a nullable var DECLINES the whole recogniser. These pin the
// decline (the 11 shapes that diverged before the admission was tightened, plus
// the null-preserving `IN` / `toInteger` forms that fall under the same rule).

#[test]
fn adv_null_mapping_aggregate_args() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len IS NULL) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len IS NOT NULL) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len > 10 AND false) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len > 10 OR true) AS c ORDER BY fid",
        // Null-preserving (`null IN […]` / `toInteger(null)` are null), but the
        // rule is by SHAPE, not by a per-function null analysis — they decline.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len IN [10, 20]) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(toInteger(post.len)) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post IS NULL) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(coalesce(post.len, 0)) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, collect(coalesce(post.len, -1)) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, collect(post.len IS NULL) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(CASE WHEN post IS NULL THEN 1 ELSE 0 END) AS c ORDER BY fid",
        // A later leg's nullable var, and the Form A (WITH→RETURN) tail.
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(member.pid IS NULL) AS c, count(post) AS cp ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         WITH forum, collect(coalesce(member.pid, -1)) AS pids RETURN forum.fid AS fid, pids ORDER BY fid",
        // DISTINCT does not widen the admitted shape; a nested property read is
        // not the direct form either; a null-mapping expr over a nullable REL var.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(DISTINCT post.len IS NULL) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len.x) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[r:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(r IS NULL) AS c ORDER BY fid",
    ] {
        declines_but_agrees(&g, src);
    }
    // The core tail evaluates per row through `eval_expr` over a `node_of` null,
    // so the same null-mapping expressions PROJECTED agree — and fire.
    agrees_and_fires(
        &g,
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len IS NULL AS isnull, coalesce(post.len, -1) AS c",
    );
}

/// The admitted argument class over a nullable var — the bare var and the
/// direct `var.prop`, with or without DISTINCT, node or rel — still FIRES.
#[test]
fn adv_direct_prop_aggregate_args_still_fire() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, collect(post.len) AS c ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(DISTINCT post.len) AS c, collect(DISTINCT post) AS ps ORDER BY fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[r:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(r) AS cr, count(r.weight) AS cw ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         WITH forum, count(post.len) AS cp, collect(member.pid) AS pids \
         RETURN forum.fid AS fid, cp, pids ORDER BY fid",
    ] {
        agrees_and_fires(&g, src);
    }
    let (on, _) = both(
        &g,
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(post.len) AS c ORDER BY fid",
    );
    assert_eq!(
        on.unwrap(),
        vec![
            vec![Value::Int(0), Value::Int(2)],
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(0)],
            vec![Value::Int(3), Value::Int(3)],
        ],
        "the null-fill row is excluded from count(post.len)"
    );
}

/// An ORDER BY aggregate that is NOT a projected item (`ORDER BY count(post.len
/// IS NULL)` with no matching item) — `nullable_agg_ok` strips the aggregate
/// before its free-var check, so a null-mapping argument there would be invisible
/// to it. Measured: BOTH paths reject the shape upstream of any recogniser
/// (`ORDER BY references `post`, not in scope after the projection`), so the
/// stripped check is unreachable by it. Agreement only (the same error on both
/// sides); reports the routing so a scope-rule change becomes visible here.
#[test]
fn adv_order_by_aggregate_not_an_item_over_nullable() {
    let g = optg();
    let mut div = Vec::new();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(*) AS c ORDER BY count(post.len IS NULL) DESC, fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(*) AS c ORDER BY count(post) DESC, fid",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, count(*) AS c ORDER BY collect(coalesce(post.len, -1)), fid",
    ] {
        div.extend(agrees_report(&g, src));
    }
    assert_all_agree(div);
}

/// An ORDER BY key that reads a nullable var declines in BOTH spellings — the
/// direct `ORDER BY post.len` and the aliased `post.len AS plen ORDER BY plen`
/// (`nullable_core_ok` classifies the alias-RESOLVED key, as the recogniser and
/// the top-k do). Pins that the alias form cannot slip past the decline.
#[test]
fn adv_alias_order_over_nullable_declines() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY post.len DESC LIMIT 20",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY plen DESC LIMIT 20",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY fid, plen LIMIT 20",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, member.pid AS mpid ORDER BY mpid LIMIT 5",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, member.pid AS mpid, post.len AS plen ORDER BY mpid DESC, plen ASC LIMIT 5",
        // A nullable REL var's property through an alias.
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[r:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, r.weight AS w ORDER BY w LIMIT 5",
    ] {
        declines_but_agrees(&g, src);
    }
    // The control: the same alias spelling over a NON-nullable (outer) var fires.
    agrees_and_fires(
        &g,
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS f, post.len AS plen ORDER BY f DESC LIMIT 20",
    );
}

// ─── (1g) DISTINCT projections and alias ORDER BY over nullable columns ───────

#[test]
fn adv_distinct_and_alias_order_over_nullable() {
    let g = optg();
    let mut div = Vec::new();
    for src in [
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN DISTINCT forum.fid AS fid, post.len AS plen, member.pid AS mpid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         WITH DISTINCT forum, member RETURN forum.fid AS fid, member.pid AS mpid",
        // ORDER BY an ALIAS of a nullable column (the raw key is `Var(alias)`).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, member.pid AS mpid ORDER BY mpid LIMIT 5",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY plen DESC LIMIT 20",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post.len AS plen ORDER BY coalesce(post.len, -1) LIMIT 20",
        "MATCH (forum:Forum) OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         RETURN forum.fid AS fid, post AS p ORDER BY p LIMIT 20",
        // Alias ORDER BY over a nullable REL var / a nullable var from a
        // SEMIJOIN-closing leg (the rel column is the only nullable one).
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[r:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, r AS rr ORDER BY rr LIMIT 5",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, member.pid AS mpid, post.len AS plen ORDER BY mpid DESC, plen ASC LIMIT 5",
    ] {
        div.extend(agrees_report(&g, src));
    }
    assert_all_agree(div);
}

// ─── (3) var-length legs and chained-nullable roots decline ──────────────────

#[test]
fn adv_varlen_and_chained_roots_decline() {
    let g = optg();
    for src in [
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF*1..2]-(post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF*1..2]-(member) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         OPTIONAL MATCH (member)<-[:HAS_CREATOR*1..2]-(x) \
         RETURN forum.fid AS fid, count(post) AS cp, count(member) AS cm, count(x) AS cx ORDER BY fid",
        "MATCH (forum:Forum)<-[:MEMBER_OF*1..2]-(m) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (forum)<-[:MEMBER_OF]-(member:Person) \
         RETURN count(post) AS cp, count(member) AS cm",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (post)-[:HAS_CREATOR]->(person:Person) \
         RETURN forum.fid AS fid, count(post) AS cp, count(person) AS cpe ORDER BY fid",
        "MATCH (forum:Forum) \
         OPTIONAL MATCH (forum)<-[:CONTAINER_OF]-(post:Post) \
         OPTIONAL MATCH (post)-[:HAS_CREATOR]->(person:Person) \
         RETURN forum.fid AS fid, post.len AS plen, person.pid AS pid",
    ] {
        declines_but_agrees(&g, src);
    }
}
