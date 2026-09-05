#![allow(non_snake_case)]
//! Fix 64: the streaming matcher reuses a BOUND start that has no inline
//! map to test — the executor's rule since fix 51, never applied on the
//! streaming path, which re-materialised the row's node under the stage
//! projection on every row. The UserTrack listing (`MATCH (n:UserTrack
//! {userId: $userId}) OPTIONAL MATCH (n)-[:PERFORMED_BY]->(a:UserArtist)
//! RETURN properties(n) AS n, a.title AS artist ORDER BY n.createdAt DESC`)
//! decoded every track in full TWICE: 1,668 full decodes for 834 rows on
//! the mirror (2.2 ms against Neo4j's 0.6).
//!
//! Every answer is checked against the expectation computed from the
//! fixture (the row order is total: `createdAt` is unique per track).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str("u1".into()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const REUSED: &str = "interp.stream matcher reused the bound start";
const FULL: &str = "graph.nodes materialised in full";

/// One fixture track: its properties, whether it is `Media` too, and its
/// artist's title when it has one.
struct Track {
    props: BTreeMap<String, Value>,
    media: bool,
    artist: Option<String>,
}

fn track_props(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("id".into(), s(&format!("track-{i}")));
    m.insert("title".into(), s(&format!("Track {i}")));
    m.insert(
        "userId".into(),
        s(if i % 2 == 0 { "u1" } else { ["u2", "u3", "u4"][(i % 3) as usize] }),
    );
    // Unique per track, so `ORDER BY createdAt DESC` is a total order.
    m.insert(
        "createdAt".into(),
        s(&format!("2026-08-{:02}T00:{:02}:{:02}Z", 1 + (i / 60) % 28, (i / 60) % 60, i % 60)),
    );
    m.insert("kind".into(), s(if i % 3 == 0 { "live" } else { "studio" }));
    m
}

/// 600 UserTrack nodes (300 of them u1's, interleaved with three other
/// users'; every fourth also `Media`), 20 UserArtist nodes, a PERFORMED_BY
/// from every track except each seventh (the OPTIONAL null).
fn corpus() -> (Graph, Vec<Track>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX ut_user FOR (n:UserTrack) ON (n.userId)");
    let mut artists = Vec::new();
    for j in 0..20i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("artist-{j}")));
        m.insert("title".into(), s(&format!("Artist {j}")));
        artists.push(g.create_node(&["UserArtist".into()], &m).expect("artist"));
    }
    let mut tracks = Vec::new();
    for i in 0..600i64 {
        let props = track_props(i);
        let media = i % 4 == 0;
        let labels: Vec<String> = if media {
            vec!["UserTrack".into(), "Media".into()]
        } else {
            vec!["UserTrack".into()]
        };
        let id = g.create_node(&labels, &props).expect("track");
        let artist = if i % 7 == 0 {
            None
        } else {
            let j = (i % 20) as usize;
            g.create_rel(id, "PERFORMED_BY", artists[j], &BTreeMap::new()).expect("performed");
            Some(format!("Artist {j}"))
        };
        tracks.push(Track { props, media, artist });
    }
    (g, tracks)
}

/// u1's tracks as `(properties, artist-or-null)` rows, newest first, with
/// `keep` deciding whether the track's artist is reported.
fn expected(tracks: &[Track], keep: impl Fn(&Track) -> bool) -> Vec<Vec<Value>> {
    let mut mine: Vec<&Track> = tracks
        .iter()
        .filter(|t| t.props.get("userId") == Some(&s("u1")))
        .collect();
    let created = |t: &Track| match &t.props["createdAt"] {
        Value::Str(v) => v.clone(),
        other => panic!("createdAt is a string, not {other:?}"),
    };
    mine.sort_by_key(|t| std::cmp::Reverse(created(t)));
    mine.iter()
        .map(|t| {
            let artist = match (&t.artist, keep(t)) {
                (Some(a), true) => s(a),
                _ => Value::Null,
            };
            vec![Value::Map(t.props.clone()), artist]
        })
        .collect()
}

const ORIG: &str = "MATCH (n:UserTrack {userId: $userId}) \
    OPTIONAL MATCH (n)-[:PERFORMED_BY]->(a:UserArtist) \
    RETURN properties(n) AS n, a.title AS artist ORDER BY n.createdAt DESC";

#[test]
fn a_bound_start_without_a_map_is_reused_by_the_optional_hop() {
    let (g, tracks) = corpus();
    let want = expected(&tracks, |_| true);
    assert_eq!(want.len(), 300);
    // The index is built on the first probe; count the second run.
    let _ = rows(&g, ORIG);
    let (got, c) = traced(&g, ORIG);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REUSED), 300, "{c:?}");
    assert_eq!(count_of(&c, FULL), 300, "one full decode per track, not two: {c:?}");
}

/// A map on the later start keeps the re-read — its keys are tested on
/// the re-materialised node — and so does a path variable, whose trail
/// wants the whole node.
#[test]
fn b_a_map_or_a_path_variable_keeps_the_re_read() {
    let (g, tracks) = corpus();
    let mapped = ORIG.replace("OPTIONAL MATCH (n)-", "OPTIONAL MATCH (n {kind: 'live'})-");
    let want = expected(&tracks, |t| t.props.get("kind") == Some(&s("live")));
    let _ = rows(&g, &mapped);
    let (got, c) = traced(&g, &mapped);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REUSED), 0, "{c:?}");

    let pathed = ORIG.replace("OPTIONAL MATCH (n)-", "OPTIONAL MATCH p = (n)-");
    let want = expected(&tracks, |_| true);
    let (got, c) = traced(&g, &pathed);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REUSED), 0, "{c:?}");
}

/// A pattern label the bound node does not list falls back to the re-read,
/// which decides from the node's full label set: `(n:Media)` reports an
/// artist for the Media tracks only, `(n:UserArtist)` for none.
#[test]
fn c_a_label_the_bound_node_lacks_falls_back_to_the_re_read() {
    let (g, tracks) = corpus();
    let media = ORIG.replace("OPTIONAL MATCH (n)-", "OPTIONAL MATCH (n:Media)-");
    let want = expected(&tracks, |t| t.media);
    assert!(want.iter().any(|r| r[1] != Value::Null), "fixture: some Media track has an artist");
    assert!(want.iter().any(|r| r[1] == Value::Null), "fixture: some track is not Media");
    let _ = rows(&g, &media);
    let (got, c) = traced(&g, &media);
    assert_eq!(got, want);
    // The seed binds the node in full, labels included: the 150 Media
    // tracks pass the label test on the row's own value.
    assert_eq!(count_of(&c, REUSED), 150, "{c:?}");

    let foreign = ORIG.replace("OPTIONAL MATCH (n)-", "OPTIONAL MATCH (n:UserArtist)-");
    let want = expected(&tracks, |_| false);
    let (got, c) = traced(&g, &foreign);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, REUSED), 0, "{c:?}");
}
