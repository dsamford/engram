#![allow(
    non_snake_case,
    dead_code,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
//! LOCAL-ONLY timing probe (not checked in): where does a RUN with a
//! 3,000-string list parameter spend its time inside the bolt server —
//! decode, run, encode — for a statement that tests the list and one that
//! only sizes it? On the mirror the NOT-IN story pick's round trip grew
//! from 13 to 85 ms with the list's length while its engine wall stayed at
//! 1.9 ms and `RETURN size($ids)` cost 3 ms.
//!
//! ```text
//! cargo test -p engram-bolt --test x_list_param_timing -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use engram_bolt::{BoltServer, Decoder, Pack};
use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

const DRIVER_HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04,
    0x00, 0x00, 0x00, 0x03,
];
const PICK_6_0: [u8; 5] = [0x00, 0x00, 0x00, 0x06, 0x00];

fn msg(tag: u8, fields: Vec<Pack>) -> Vec<u8> {
    let mut payload = Vec::new();
    engram_bolt::packstream::encode_struct(tag, &fields, &mut payload).expect("encodes");
    let mut out = Vec::new();
    // Chunked at 64 KiB like the driver does.
    for chunk in payload.chunks(0xFFFF) {
        out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

fn str_field(s: &str) -> Pack {
    Pack::Value(Value::Str(s.to_string()))
}

fn map_field(m: BTreeMap<String, Value>) -> Pack {
    Pack::Value(Value::Map(m))
}

fn replies(bytes: &[u8]) -> Vec<(u8, Vec<Pack>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut payload = Vec::new();
    while at + 2 <= bytes.len() {
        let size = u16::from_be_bytes(bytes[at..at + 2].try_into().expect("2")) as usize;
        at += 2;
        if size == 0 {
            if !payload.is_empty() {
                let mut d = Decoder::new(&payload);
                let Pack::Struct { tag, fields } = d.decode().expect("reply decodes") else {
                    panic!("reply was not a structure");
                };
                out.push((tag, fields));
                payload.clear();
            }
            continue;
        }
        payload.extend_from_slice(&bytes[at..at + size]);
        at += size;
    }
    out
}

fn graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..2000i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "storyId".to_string(),
            Value::Str(format!("{i:08x}-b156-45c3-bcb7-13fa88498f21")),
        );
        m.insert(
            "topic".to_string(),
            Value::Str(if i % 8 == 0 {
                "crime".into()
            } else {
                "other".into()
            }),
        );
        m.insert(
            "status".to_string(),
            Value::Str(if i % 5 == 0 {
                "stale".into()
            } else {
                "active".into()
            }),
        );
        m.insert(
            "lastUpdatedAt".to_string(),
            Value::Str(format!("2026-08-{:02}T00:00:00Z", 1 + i % 28)),
        );
        m.insert("title".to_string(), Value::Str(format!("Story {i}")));
        m.insert(
            "summary".to_string(),
            Value::Str("a summary of the story ".repeat(8)),
        );
        g.create_node(&["NewsStory".into()], &m).expect("story");
    }
    g
}

fn ready_server() -> BoltServer {
    let mut s = BoltServer::new(graph());
    s.feed(&DRIVER_HANDSHAKE).expect("handshake");
    s.feed(&PICK_6_0).expect("pick");
    let r = s
        .feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    assert_eq!(replies(&r)[0].0, 0x70);
    let r = s
        .feed(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    assert_eq!(replies(&r)[0].0, 0x70);
    s
}

fn run_with(s: &mut BoltServer, q: &str, params: BTreeMap<String, Value>) -> (usize, f64) {
    let mut bytes = msg(
        0x10,
        vec![str_field(q), map_field(params), map_field(BTreeMap::new())],
    );
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    bytes.extend(msg(0x3F, vec![map_field(pull)]));
    let t0 = Instant::now();
    let out = s.feed(&bytes).expect("run+pull");
    let el = t0.elapsed().as_secs_f64() * 1e3;
    let rs = replies(&out);
    (rs.iter().filter(|(t, _)| *t == 0x71).count(), el)
}

#[test]
fn time_a_list_param_of_three_thousand_ids() {
    let mut s = ready_server();
    let ids = |n: usize| {
        Value::List(
            (0..n)
                .map(|i| Value::Str(format!("00000000-0000-4000-8000-{i:012}")))
                .collect::<Vec<_>>().into(),
        )
    };
    let pick = "MATCH (s:NewsStory) WHERE s.topic = $topic AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff AND NOT s.storyId IN $existingIds RETURN s.storyId AS storyId, s.title AS title, s.summary AS summary LIMIT 5";
    let base = "MATCH (s:NewsStory) WHERE s.topic = $topic AND s.status <> 'stale' AND s.lastUpdatedAt > $cutoff RETURN s.storyId AS storyId, s.title AS title, s.summary AS summary LIMIT 5";
    for n in [10usize, 200, 1000, 3000] {
        let mut p = BTreeMap::new();
        p.insert("topic".to_string(), Value::Str("crime".into()));
        p.insert("cutoff".to_string(), Value::Str("2026-08-10".into()));
        p.insert("existingIds".to_string(), ids(n));
        // warm
        let _ = run_with(&mut s, pick, p.clone());
        let mut best = f64::MAX;
        let mut rows = 0;
        for _ in 0..5 {
            let (r, el) = run_with(&mut s, pick, p.clone());
            rows = r;
            best = best.min(el);
        }
        let (_, size_el) = run_with(&mut s, "RETURN size($existingIds) AS n", p.clone());
        let (_, base_el) = run_with(&mut s, base, p.clone());
        eprintln!(
            "ids {n:>5}: pick {best:7.3} ms ({rows} rows)   size() {size_el:7.3} ms   base (no NOT IN) {base_el:7.3} ms"
        );
    }
}
