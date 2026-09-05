#![allow(non_snake_case)]
//! The wire — codec goldens, the real driver's handshake, whole sessions.

use std::collections::BTreeMap;

use engram_bolt::{BoltServer, Decoder, Pack, WireError, decode_value, encode_value};
use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn enc(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(v, &mut out).expect("encodes");
    out
}

fn round_trip(v: &Value) -> Value {
    let bytes = enc(v);
    let mut d = Decoder::new(&bytes);
    let p = d.decode().expect("decodes");
    assert!(d.done(), "whole input consumed");
    decode_value(p).expect("maps to a value")
}

// ─── PackStream goldens ─────────────────────────────────────────────────────

#[test]
fn integer_width_goldens() {
    // The width boundaries are where a codec silently corrupts — pinned as
    // exact bytes, not just round trips.
    let cases: &[(i64, &[u8])] = &[
        (0, &[0x00]),
        (127, &[0x7F]),
        (-1, &[0xFF]),
        (-16, &[0xF0]),
        (-17, &[0xC8, 0xEF]),
        (-128, &[0xC8, 0x80]),
        (128, &[0xC9, 0x00, 0x80]),
        (-129, &[0xC9, 0xFF, 0x7F]),
        (32_767, &[0xC9, 0x7F, 0xFF]),
        (32_768, &[0xCA, 0x00, 0x00, 0x80, 0x00]),
        (2_147_483_647, &[0xCA, 0x7F, 0xFF, 0xFF, 0xFF]),
        (
            2_147_483_648,
            &[0xCB, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00],
        ),
        (
            i64::MAX,
            &[0xCB, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ),
    ];
    for (v, bytes) in cases {
        assert_eq!(enc(&Value::Int(*v)), *bytes, "{v}");
        assert_eq!(round_trip(&Value::Int(*v)), Value::Int(*v));
    }
}

#[test]
fn scalar_and_container_goldens() {
    assert_eq!(enc(&Value::Null), vec![0xC0]);
    assert_eq!(enc(&Value::Bool(true)), vec![0xC3]);
    assert_eq!(enc(&Value::Bool(false)), vec![0xC2]);
    assert_eq!(
        enc(&Value::Float(1.5)),
        vec![0xC1, 0x3F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(enc(&Value::Str("ok".into())), vec![0x82, b'o', b'k']);
    // The 16-length boundary: tiny → sized-8.
    let s15 = "a".repeat(15);
    let s16 = "a".repeat(16);
    assert_eq!(enc(&Value::Str(s15.clone()))[0], 0x8F);
    assert_eq!(&enc(&Value::Str(s16.clone()))[..2], &[0xD0, 16]);
    assert_eq!(
        enc(&Value::List(vec![Value::Int(1), Value::Int(2)])),
        vec![0x92, 0x01, 0x02]
    );
    let mut m = BTreeMap::new();
    m.insert("a".to_string(), Value::Int(1));
    assert_eq!(enc(&Value::Map(m)), vec![0xA1, 0x81, b'a', 0x01]);
}

#[test]
fn every_value_shape_round_trips() {
    let mut props = BTreeMap::new();
    props.insert(
        "k".to_string(),
        Value::List(vec![Value::Str("x".into()), Value::Null]),
    );
    let values = [
        Value::Null,
        Value::Int(-9_007_199_254_740_993),
        Value::Float(f64::MIN_POSITIVE),
        Value::Str("héllo — ünïcode".into()),
        Value::List(vec![
            Value::Bool(true),
            Value::Float(0.1),
            Value::Str("s".into()),
        ]),
        Value::Map(props.clone()),
        Value::Node {
            id: 42,
            labels: vec!["A".into(), "B".into()],
            props: props.clone(),
        },
        Value::Rel {
            id: 7,
            src: 1,
            dst: 2,
            rel_type: "KNOWS".into(),
            props: BTreeMap::new(),
        },
    ];
    for v in &values {
        assert_eq!(&round_trip(v), v);
    }
}

#[test]
fn all_seven_temporal_structures_round_trip() {
    let values = [
        Value::Date(20_684),
        Value::Time {
            nanos: 45_015_000_000_000,
            offset_seconds: 7200,
        },
        Value::LocalTime(45_015_000_000_000),
        Value::DateTime {
            epoch_seconds: 1_787_133_600,
            nanos: 250_000_000,
            offset_seconds: 7200,
            zone: None,
        },
        Value::DateTime {
            epoch_seconds: 1_787_133_600,
            nanos: 0,
            offset_seconds: 0,
            zone: Some("Europe/Berlin".into()),
        },
        Value::LocalDateTime {
            epoch_seconds: 1_787_133_600,
            nanos: 1,
        },
        Value::Duration {
            months: 14,
            days: 3,
            seconds: 14_706,
            nanos: 500_000_000,
        },
    ];
    for v in &values {
        assert_eq!(&round_trip(v), v, "{v:?}");
    }
    // The tags are the DRIVER's, pinned.
    let bytes = enc(&Value::Date(1));
    assert_eq!(&bytes[..2], &[0xB1, 0x44]);
    let bytes = enc(&Value::Duration {
        months: 0,
        days: 0,
        seconds: 0,
        nanos: 0,
    });
    assert_eq!(&bytes[..2], &[0xB4, 0x45]);
}

#[test]
fn node_structure_carries_the_element_id() {
    // Bolt 5's reason to exist here: the STRING element id, 65 call sites.
    let n = Value::Node {
        id: 9,
        labels: vec![],
        props: BTreeMap::new(),
    };
    let bytes = enc(&n);
    let mut d = Decoder::new(&bytes);
    let Pack::Struct { tag, fields } = d.decode().expect("decodes") else {
        panic!()
    };
    assert_eq!(tag, 0x4E);
    assert_eq!(fields.len(), 4, "id, labels, props, element_id");
    assert_eq!(fields[3], Pack::Value(Value::Str("n:9".into())));
}

// ─── The handshake ──────────────────────────────────────────────────────────

/// The REAL driver's proposal bytes (neo4j-driver 5.28.3): manifest v1,
/// 5.8→5.0 range, 4.4→4.2 range, 3.0.
/// A pre-5.7 driver's handshake: four legacy proposals, no manifest.
const LEGACY_HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, // magic
    0x00, 0x08, 0x08, 0x05, // 5.8 back to 5.0 — a RANGE
    0x00, 0x02, 0x04, 0x04, // 4.4 back to 4.2
    0x00, 0x00, 0x00, 0x03, // 3.0
    0x00, 0x00, 0x00, 0x00,
];

/// What a 5.7+ / 6.x driver sends: the Manifest v1 request first, legacy
/// ranges behind it for servers that do not speak the manifest.
const DRIVER_HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, // magic
    0x00, 0x00, 0x01, 0xFF, // manifest v1
    0x00, 0x08, 0x08, 0x05, // 5.8 back to 5.0 — a RANGE
    0x00, 0x02, 0x04, 0x04, // 4.4 back to 4.2
    0x00, 0x00, 0x00, 0x03, // 3.0
];

/// The server's manifest reply, byte for byte: `00 00 01 FF`, a varint
/// count of 2, `6.0` (range 0) then `5.8` (range 8), and a zero capability
/// mask. Written out rather than derived so a change to the offer is a
/// change to this line.
const MANIFEST_REPLY: [u8; 14] = [
    0x00, 0x00, 0x01, 0xFF, // manifest v1 accepted
    0x02, //                   two versions follow
    0x00, 0x00, 0x00, 0x06, // 6.0
    0x00, 0x08, 0x08, 0x05, // 5.8 back to 5.0
    0x00, //                   no capabilities
];

#[test]
fn a_legacy_handshake_still_negotiates_5_8_from_the_range() {
    let mut s = BoltServer::new(graph());
    let reply = s.feed(&LEGACY_HANDSHAKE).expect("negotiates");
    assert_eq!(reply, vec![0, 0, 8, 5], "the server chooses from the RANGE");
    assert_eq!(s.version(), (5, 8));
}

#[test]
fn a_legacy_handshake_offering_6_0_gets_6_0() {
    let mut bytes = LEGACY_HANDSHAKE;
    bytes[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x06]);
    let mut s = BoltServer::new(graph());
    assert_eq!(s.feed(&bytes).expect("negotiates"), vec![0, 0, 0, 6]);
    assert_eq!(s.version(), (6, 0));
}

#[test]
fn the_manifest_is_answered_with_the_whole_offer_and_the_client_picks() {
    let mut s = BoltServer::new(graph());
    let reply = s.feed(&DRIVER_HANDSHAKE).expect("manifest offered");
    assert_eq!(reply, MANIFEST_REPLY.to_vec(), "the offer, byte for byte");
    // Nothing is negotiated until the client answers.
    assert_eq!(s.version(), (0, 0));
    // The client picks 6.0 and selects no capabilities.
    let done = s.feed(&[0x00, 0x00, 0x00, 0x06, 0x00]).expect("pick accepted");
    assert!(done.is_empty(), "the server sends nothing for the pick itself");
    assert_eq!(s.version(), (6, 0));
}

#[test]
fn the_manifest_client_may_pick_any_offered_5_x_and_pipeline_hello() {
    let mut s = BoltServer::new(graph());
    s.feed(&DRIVER_HANDSHAKE).expect("manifest offered");
    // 5.3, inside the 5.8..5.0 range, then HELLO in the same write.
    let mut bytes = vec![0x00, 0x00, 0x03, 0x05, 0x00];
    bytes.extend(msg(0x01, vec![map_field(BTreeMap::new())]));
    let r = replies(&s.feed(&bytes).expect("pick + hello"));
    assert_eq!(s.version(), (5, 3));
    assert_eq!(r[0].0, 0x70, "HELLO succeeded behind the pick");
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else { panic!("no metadata") };
    assert_eq!(
        meta.get("protocol_version"),
        Some(&Value::Str("5.3".into())),
        "a manifest-negotiated session is told its version"
    );
}

#[test]
fn a_legacy_session_is_not_told_a_protocol_version() {
    let mut s = BoltServer::new(graph());
    s.feed(&LEGACY_HANDSHAKE).expect("legacy");
    let r = replies(&s.feed(&msg(0x01, vec![map_field(BTreeMap::new())])).expect("hello"));
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else { panic!("no metadata") };
    assert!(
        !meta.contains_key("protocol_version"),
        "the spec reserves protocol_version for the manifest exchange"
    );
    assert!(meta.contains_key("server") && meta.contains_key("connection_id"));
}

#[test]
fn a_pick_outside_the_offer_is_refused() {
    let mut s = BoltServer::new(graph());
    s.feed(&DRIVER_HANDSHAKE).expect("manifest offered");
    // 4.4 was never offered.
    assert!(matches!(
        s.feed(&[0x00, 0x00, 0x04, 0x04, 0x00]),
        Err(WireError::NoCommonVersion)
    ));
}

#[test]
fn selecting_a_capability_that_was_not_offered_is_a_protocol_error() {
    let mut s = BoltServer::new(graph());
    s.feed(&DRIVER_HANDSHAKE).expect("manifest offered");
    assert!(matches!(
        s.feed(&[0x00, 0x00, 0x00, 0x06, 0x01]),
        Err(WireError::Protocol(_))
    ));
}

#[test]
fn the_pick_waits_for_a_whole_varint() {
    let mut s = BoltServer::new(graph());
    s.feed(&DRIVER_HANDSHAKE).expect("manifest offered");
    // A continuation bit with nothing after it: not yet decidable.
    assert!(s.feed(&[0x00, 0x00, 0x00, 0x06, 0x80]).expect("waits").is_empty());
    assert_eq!(s.version(), (0, 0));
    // The varint completes to 0 (`80 00`), which selects nothing.
    s.feed(&[0x00]).expect("completes");
    assert_eq!(s.version(), (6, 0));
}

#[test]
fn no_common_version_refuses() {
    let mut bytes = LEGACY_HANDSHAKE;
    bytes[4..20].copy_from_slice(&[
        0x00, 0x00, 0x02, 0xFF, // a manifest version we do not speak (v2)
        0x00, 0x02, 0x04, 0x04, // …and 4.x
        0x00, 0x00, 0x00, 0x03, //
        0x00, 0x00, 0x00, 0x00,
    ]);
    let mut s = BoltServer::new(graph());
    assert!(matches!(s.feed(&bytes), Err(WireError::NoCommonVersion)));
}

#[test]
fn http_on_the_bolt_port_fails_LEGIBLY() {
    let mut s = BoltServer::new(graph());
    let err = s.feed(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap_err();
    match err {
        WireError::NotBolt { detail } => {
            assert!(
                detail.contains("HTTP"),
                "the misconfiguration is NAMED: {detail}"
            )
        }
        other => panic!("expected NotBolt, got {other:?}"),
    }
}

// ─── Sessions ───────────────────────────────────────────────────────────────

fn msg(tag: u8, fields: Vec<Pack>) -> Vec<u8> {
    let mut payload = Vec::new();
    engram_bolt::packstream::encode_struct(tag, &fields, &mut payload).expect("encodes");
    let mut out = (payload.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&payload);
    out.extend_from_slice(&[0, 0]);
    out
}

fn str_field(s: &str) -> Pack {
    Pack::Value(Value::Str(s.to_string()))
}

fn map_field(m: BTreeMap<String, Value>) -> Pack {
    Pack::Value(Value::Map(m))
}

/// Parse a reply byte stream into (tag, first-field) messages.
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

/// The 6.x driver's handshake in full: the manifest request, then the pick
/// of 6.0 with no capabilities. Every session helper negotiates this way, so
/// the session suite runs on the NEWEST version; the legacy path has its
/// own tests above.
const PICK_6_0: [u8; 5] = [0x00, 0x00, 0x00, 0x06, 0x00];

fn negotiate_6_0(s: &mut BoltServer) {
    assert_eq!(s.feed(&DRIVER_HANDSHAKE).expect("handshake"), MANIFEST_REPLY.to_vec());
    s.feed(&PICK_6_0).expect("pick");
    assert_eq!(s.version(), (6, 0));
}

fn ready_server() -> BoltServer {
    let mut s = BoltServer::new(graph());
    negotiate_6_0(&mut s);
    let r = s
        .feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    assert_eq!(replies(&r)[0].0, 0x70, "HELLO succeeds");
    let r = s
        .feed(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    assert_eq!(replies(&r)[0].0, 0x70, "LOGON succeeds");
    s
}

fn run_stmt(s: &mut BoltServer, q: &str) -> Vec<(u8, Vec<Pack>)> {
    let mut bytes = msg(
        0x10,
        vec![
            str_field(q),
            map_field(BTreeMap::new()),
            map_field(BTreeMap::new()),
        ],
    );
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    bytes.extend(msg(0x3F, vec![map_field(pull)]));
    replies(&s.feed(&bytes).expect("run+pull"))
}

#[test]
fn a_whole_session_streams_IDENTICAL_values_to_the_interpreter() {
    let mut s = ready_server();
    let r = run_stmt(&mut s, "CREATE (:P {name: 'Ada', xs: [1, 2]})");
    assert!(r.iter().all(|(t, _)| *t == 0x70), "write succeeds");

    let query = "MATCH (p:P) RETURN p.name, p.xs, p";
    let r = run_stmt(&mut s, query);
    assert_eq!(r[0].0, 0x70, "RUN success");
    let records: Vec<&Vec<Pack>> = r
        .iter()
        .filter(|(t, _)| *t == 0x71)
        .map(|(_, f)| f)
        .collect();
    assert_eq!(records.len(), 1);
    let Pack::Value(Value::List(_)) = &records[0][0] else {
        // RECORD's field is a LIST; graph entities inside arrive as
        // structures, so decode structurally.
        let wire_row = match &records[0][0] {
            Pack::Value(v) => vec![v.clone()],
            Pack::Bytes(_) | Pack::Struct { .. } => panic!("record field should be a list"),
        };
        panic!("unexpected record shape: {wire_row:?}");
    };

    // Decode the wire row fully (structures included) and compare with the
    // interpreter's own answer — THE kill-criterion check, in miniature.
    let raw = &records[0][0];
    let wire_row: Vec<Value> = match raw {
        Pack::Value(Value::List(vs)) => vs.clone(),
        _ => unreachable!("asserted above"),
    };
    let g2 = graph();
    run_query(
        &g2,
        &parse_statement("CREATE (:P {name: 'Ada', xs: [1, 2]})").expect("p"),
        BTreeMap::new(),
    )
    .expect("seed");
    let direct = run_query(&g2, &parse_statement(query).expect("p"), BTreeMap::new()).expect("run");
    assert_eq!(
        wire_row, direct.rows[0],
        "wire values decode IDENTICAL to interpreter values"
    );
}

#[test]
fn record_rows_carry_structures_for_entities() {
    let mut s = ready_server();
    run_stmt(&mut s, "CREATE (:E {v: 1})-[:R {w: 2}]->(:E {v: 2})");
    let r = run_stmt(&mut s, "MATCH (a:E)-[k:R]->(b:E) RETURN a, k, b");
    let rec = r.iter().find(|(t, _)| *t == 0x71).expect("a record");
    let Pack::Value(Value::List(row)) = &rec.1[0] else {
        panic!("record field is a list")
    };
    assert!(matches!(row[0], Value::Node { .. }));
    assert!(matches!(&row[1], Value::Rel { rel_type, .. } if rel_type == "R"));
    assert!(matches!(row[2], Value::Node { .. }));
}

#[test]
fn the_failure_protocol_IGNOREs_until_reset() {
    let mut s = ready_server();
    let r = run_stmt(&mut s, "MATCH (n RETURN n");
    assert_eq!(r[0].0, 0x7F, "syntax error FAILS");
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else {
        panic!()
    };
    assert!(matches!(meta.get("code"), Some(Value::Str(c)) if c.contains("SyntaxError")));
    // While failed: everything is IGNORED…
    let r = run_stmt(&mut s, "RETURN 1");
    assert!(
        r.iter().all(|(t, _)| *t == 0x7E),
        "IGNORED until RESET, got {r:?}"
    );
    // …until RESET.
    let r = replies(&s.feed(&msg(0x0F, vec![])).expect("reset"));
    assert_eq!(r[0].0, 0x70);
    let r = run_stmt(&mut s, "RETURN 1");
    assert!(r.iter().any(|(t, _)| *t == 0x71), "recovered");
}

#[test]
fn rollback_now_genuinely_undoes_instead_of_refusing() {
    // This once returned a NAMED refusal ("ROLLBACK is not supported:
    // statements autocommit") — honest about a limitation. With session-owned
    // transactions the limitation is gone: ROLLBACK SUCCEEDS and the buffered
    // writes it discards never published.
    let mut s = ready_server();
    let r = replies(
        &s.feed(&msg(0x11, vec![map_field(BTreeMap::new())]))
            .expect("begin"),
    );
    assert_eq!(r[0].0, 0x70, "BEGIN accepted");
    run_stmt(&mut s, "CREATE (:Gone {n: 1})");
    let r = replies(
        &s.feed(&msg(0x13, vec![map_field(BTreeMap::new())]))
            .expect("rollback"),
    );
    assert_eq!(
        r[0].0, 0x70,
        "ROLLBACK now SUCCEEDS, no longer a named refusal"
    );
    assert_eq!(
        wire_count(&mut s, "MATCH (g:Gone) RETURN count(g) AS c"),
        0,
        "and the write it undid is genuinely gone"
    );
}

/// The per-statement trace marker (`TRACE_MARKER`, a leading block comment)
/// is invisible to the parser: a marked statement plans, answers and errors
/// exactly as the unmarked one. The dump it triggers goes to stderr, which is
/// the operator's to read; this asserts the wire contract it must not touch.
#[test]
fn the_trace_marker_changes_nothing_on_the_wire() {
    let mut s = ready_server();
    run_stmt(&mut s, "UNWIND [1, 2, 3] AS x CREATE (:T {v: x})");
    let plain = run_stmt(&mut s, "MATCH (t:T) WHERE t.v > 1 RETURN t.v AS v ORDER BY v");
    let marked = run_stmt(
        &mut s,
        &format!(
            "{} MATCH (t:T) WHERE t.v > 1 RETURN t.v AS v ORDER BY v",
            engram_bolt::TRACE_MARKER
        ),
    );
    assert_eq!(marked.len(), plain.len(), "same messages: SUCCESS, 2 RECORDs, SUCCESS");
    assert_eq!(marked.iter().filter(|(tag, _)| *tag == 0x71).count(), 2);
    for ((tp, fp), (tm, fm)) in plain.iter().zip(marked.iter()) {
        assert_eq!(tp, tm);
        if *tp == 0x71 {
            assert_eq!(format!("{fp:?}"), format!("{fm:?}"), "records identical");
        }
    }
    // A marker in the MIDDLE of a statement is an ordinary comment: no trace,
    // no error, same answer.
    let mid = run_stmt(&mut s, "MATCH (t:T) /* engram:trace */ WHERE t.v > 1 RETURN t.v AS v ORDER BY v");
    assert_eq!(
        mid.iter().filter(|(tag, _)| *tag == 0x71).count(),
        2,
        "a mid-statement comment is skipped by the lexer: {mid:?}"
    );
    // A syntax error behind the marker is still the statement's own. Last,
    // because a FAILURE puts the session in the failed state (every message
    // until RESET is IGNORED) — the protocol, not the marker.
    let err = run_stmt(&mut s, &format!("{} MATCH (t:T RETURN t", engram_bolt::TRACE_MARKER));
    assert_eq!(err[0].0, 0x7F, "FAILURE, not a silent success: {:?}", err[0]);
}

#[test]
fn pull_batches_and_reports_has_more() {
    let mut s = ready_server();
    run_stmt(&mut s, "UNWIND [1, 2, 3] AS x CREATE (:B {v: x})");
    let bytes = msg(
        0x10,
        vec![
            str_field("MATCH (b:B) RETURN b.v ORDER BY b.v"),
            map_field(BTreeMap::new()),
            map_field(BTreeMap::new()),
        ],
    );
    let r = replies(&s.feed(&bytes).expect("run"));
    assert_eq!(r[0].0, 0x70);
    let mut pull2 = BTreeMap::new();
    pull2.insert("n".to_string(), Value::Int(2));
    let r = replies(
        &s.feed(&msg(0x3F, vec![map_field(pull2.clone())]))
            .expect("pull"),
    );
    assert_eq!(r.iter().filter(|(t, _)| *t == 0x71).count(), 2);
    let (_, last) = r.last().expect("summary");
    let Pack::Value(Value::Map(meta)) = &last[0] else {
        panic!()
    };
    assert_eq!(
        meta.get("has_more"),
        Some(&Value::Bool(true)),
        "partial stream says so"
    );
    let r = replies(&s.feed(&msg(0x3F, vec![map_field(pull2)])).expect("pull"));
    assert_eq!(
        r.iter().filter(|(t, _)| *t == 0x71).count(),
        1,
        "the remainder"
    );
    let (_, last) = r.last().expect("summary");
    let Pack::Value(Value::Map(meta)) = &last[0] else {
        panic!()
    };
    assert!(
        meta.contains_key("bookmark"),
        "a finished stream returns the bookmark"
    );
}

#[test]
fn noop_chunks_are_tolerated_between_messages() {
    let mut s = ready_server();
    let mut bytes = vec![0, 0, 0, 0]; // two NOOP keep-alives
    bytes.extend(msg(
        0x10,
        vec![
            str_field("RETURN 7"),
            map_field(BTreeMap::new()),
            map_field(BTreeMap::new()),
        ],
    ));
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    bytes.extend(msg(0x3F, vec![map_field(pull)]));
    let r = replies(&s.feed(&bytes).expect("noop tolerated"));
    assert!(r.iter().any(|(t, f)| {
        *t == 0x71 && matches!(&f[0], Pack::Value(Value::List(vs)) if vs == &vec![Value::Int(7)])
    }));
}

#[test]
fn the_route_stub_names_itself_all_three_roles() {
    let mut s = ready_server();
    let r = replies(
        &s.feed(&msg(
            0x66,
            vec![
                map_field(BTreeMap::new()),
                Pack::Value(Value::List(vec![])),
                map_field(BTreeMap::new()),
            ],
        ))
        .expect("route"),
    );
    assert_eq!(r[0].0, 0x70);
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else {
        panic!()
    };
    let Some(Value::Map(rt)) = meta.get("rt") else {
        panic!("rt missing")
    };
    let Some(Value::List(servers)) = rt.get("servers") else {
        panic!("servers missing")
    };
    assert_eq!(
        servers.len(),
        3,
        "ROUTER, READER and WRITER — all this one node"
    );
}

#[test]
fn run_before_logon_is_a_protocol_violation_on_5_1_plus() {
    let mut s = BoltServer::new(graph());
    negotiate_6_0(&mut s);
    s.feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    let err = s
        .feed(&msg(
            0x10,
            vec![
                str_field("RETURN 1"),
                map_field(BTreeMap::new()),
                map_field(BTreeMap::new()),
            ],
        ))
        .unwrap_err();
    assert!(
        matches!(err, WireError::Protocol(_)),
        "the LOGON flow is enforced"
    );
}

fn ready_shared(graph: std::sync::Arc<Graph>) -> BoltServer {
    let mut s = BoltServer::shared(graph);
    negotiate_6_0(&mut s);
    let r = s
        .feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    assert_eq!(replies(&r)[0].0, 0x70, "HELLO succeeds");
    let r = s
        .feed(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    assert_eq!(replies(&r)[0].0, 0x70, "LOGON succeeds");
    s
}

#[test]
fn sessions_over_one_graph_share_its_indexes_without_rebuilding() {
    // Graph-level state is the SERVER's: a vector index created on session
    // A is built ONCE and answers session B. Index definitions persist in
    // the store either way — a Graph per session would still answer B, by
    // rebuilding the HNSW for itself (115 s on the production export) —
    // so the claim is the build COUNT, not the answer.
    let g = std::sync::Arc::new(graph());
    // The HNSW is built lazily, at the first query that takes the ANN arm;
    // under the exact-arm ceiling nothing is built at all, so the ceiling
    // is lowered to make the build the observable.
    g.set_vector_exact_max(0);
    let mut a = ready_shared(std::sync::Arc::clone(&g));
    let mut b = ready_shared(std::sync::Arc::clone(&g));
    let (_, t) = engram_observe::with_trace(|| {
        let r = run_stmt(
            &mut a,
            "CREATE (:Vec {v: 1, e: [1.0, 0.0]}), (:Vec {v: 2, e: [0.0, 1.0]})",
        );
        assert!(r.iter().all(|(t, _)| *t == 0x70), "{r:?}");
        let r = run_stmt(&mut a, "CREATE VECTOR INDEX shared_vi FOR (n:Vec) ON (n.e)");
        assert!(r.iter().all(|(t, _)| *t == 0x70), "{r:?}");
        let r = run_stmt(
            &mut a,
            "CALL db.index.vector.queryNodes('shared_vi', 1, [0.9, 0.1]) YIELD node, score RETURN node.v",
        );
        assert_eq!(r[1].0, 0x71, "A builds and answers: {r:?}");
        let r = run_stmt(
            &mut b,
            "CALL db.index.vector.queryNodes('shared_vi', 1, [0.9, 0.1]) YIELD node, score RETURN node.v",
        );
        assert_eq!(r[1].0, 0x71, "B answers from A's index: {r:?}");
        let Pack::Value(Value::List(row)) = &r[1].1[0] else {
            panic!()
        };
        assert_eq!(row, &vec![Value::Int(1)]);
    });
    assert_eq!(
        t.counters().get("graph.vector ann index builds"),
        Some(&1),
        "one build for the server, not one per session: {:?}",
        t.counters()
    );
}

// ─── Explicit transactions (BEGIN / COMMIT / ROLLBACK) ──────────────────────

/// Feed one no-field message (BEGIN 0x11 / COMMIT 0x12 / ROLLBACK 0x13).
fn ctl(s: &mut BoltServer, tag: u8) -> Vec<(u8, Vec<Pack>)> {
    replies(
        &s.feed(&msg(tag, vec![map_field(BTreeMap::new())]))
            .expect("feed"),
    )
}

/// The integer a single-record `RETURN count(..)` yields over the wire.
fn wire_count(s: &mut BoltServer, q: &str) -> i64 {
    let r = run_stmt(s, q);
    let rec = r.iter().find(|(t, _)| *t == 0x71).expect("a record");
    let Pack::Value(Value::List(row)) = &rec.1[0] else {
        panic!("record field is a list");
    };
    match &row[0] {
        Value::Int(n) => *n,
        other => panic!("expected an int count, got {other:?}"),
    }
}

#[test]
fn an_explicit_transaction_is_atomic_over_the_wire() {
    let mut s = ready_server();

    // ROLLBACK discards the whole transaction — nothing persists.
    assert_eq!(ctl(&mut s, 0x11)[0].0, 0x70, "BEGIN");
    assert!(
        run_stmt(&mut s, "CREATE (:Foo {n: 1})")
            .iter()
            .all(|(t, _)| *t == 0x70),
        "a create inside the txn succeeds"
    );
    run_stmt(&mut s, "CREATE (:Foo {n: 2})");
    let rb = ctl(&mut s, 0x13);
    assert_eq!(
        rb[0].0, 0x70,
        "ROLLBACK is now a real SUCCESS, not the old named refusal"
    );
    assert_eq!(
        wire_count(&mut s, "MATCH (f:Foo) RETURN count(f) AS c"),
        0,
        "a rolled-back transaction leaves nothing behind"
    );

    // COMMIT publishes the whole write-set atomically.
    assert_eq!(ctl(&mut s, 0x11)[0].0, 0x70, "BEGIN");
    run_stmt(&mut s, "CREATE (:Bar {n: 1})");
    run_stmt(&mut s, "CREATE (:Bar {n: 2})");
    // A scan INSIDE the transaction sees this transaction's own buffered
    // creates — read-your-writes: the label membership, the counts and the
    // adjacency tables overlay the write buffer over the committed snapshot.
    // (Before serialisable autocommit this read 0; that was the documented
    // limitation the overlay removed. That OTHER sessions still read 0 until
    // COMMIT is asserted in engram-graph's `txn_statement_differential`.)
    assert_eq!(
        wire_count(&mut s, "MATCH (b:Bar) RETURN count(b) AS c"),
        2,
        "buffered writes are visible to the transaction's own scan before commit"
    );
    assert_eq!(ctl(&mut s, 0x12)[0].0, 0x70, "COMMIT");
    assert_eq!(
        wire_count(&mut s, "MATCH (b:Bar) RETURN count(b) AS c"),
        2,
        "after COMMIT the whole write-set is visible at once"
    );
}

#[test]
fn a_second_session_never_sees_another_sessions_uncommitted_writes() {
    // The isolation the session-owned transaction guarantees: an explicit
    // transaction's buffered writes are invisible to a DIFFERENT session on the
    // same shared graph until it commits.
    let g = std::sync::Arc::new(graph());
    let mut a = BoltServer::shared(std::sync::Arc::clone(&g));
    negotiate_6_0(&mut a);
    a.feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    a.feed(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    let mut b = BoltServer::shared(std::sync::Arc::clone(&g));
    negotiate_6_0(&mut b);
    b.feed(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    b.feed(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");

    assert_eq!(ctl(&mut a, 0x11)[0].0, 0x70, "A: BEGIN");
    run_stmt(&mut a, "CREATE (:Iso {n: 1})");
    // B sees nothing yet.
    assert_eq!(
        wire_count(&mut b, "MATCH (i:Iso) RETURN count(i) AS c"),
        0,
        "B must not see A's uncommitted write"
    );
    assert_eq!(ctl(&mut a, 0x12)[0].0, 0x70, "A: COMMIT");
    // Now B sees it.
    assert_eq!(
        wire_count(&mut b, "MATCH (i:Iso) RETURN count(i) AS c"),
        1,
        "after A commits, B sees the write"
    );
}

// ─── 5.7+ FAILURE metadata, and the 6.0 Vector structure ────────────────────

fn ready_at(handshake: &[u8; 20], pick: Option<&[u8]>) -> BoltServer {
    let mut s = BoltServer::new(graph());
    s.feed(handshake).expect("handshake");
    if let Some(p) = pick {
        s.feed(p).expect("pick");
    }
    let r = s.feed(&msg(0x01, vec![map_field(BTreeMap::new())])).expect("hello");
    assert_eq!(replies(&r)[0].0, 0x70);
    if s.version() >= (5, 1) {
        let r = s.feed(&msg(0x6A, vec![map_field(BTreeMap::new())])).expect("logon");
        assert_eq!(replies(&r)[0].0, 0x70);
    }
    s
}

fn failure_meta(s: &mut BoltServer, q: &str) -> BTreeMap<String, Value> {
    let r = run_stmt(s, q);
    assert_eq!(r[0].0, 0x7F, "expected a FAILURE, got tag {:#x}", r[0].0);
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else { panic!("FAILURE without metadata") };
    meta.clone()
}

#[test]
fn a_6_0_failure_carries_the_5_7_fields_and_a_5_0_failure_does_not() {
    let mut six = ready_at(&DRIVER_HANDSHAKE, Some(&[0x00, 0x00, 0x00, 0x06, 0x00]));
    let m = failure_meta(&mut six, "MATCH (n RETURN n");
    assert_eq!(m.get("neo4j_code"), Some(&Value::Str("Neo.ClientError.Statement.SyntaxError".into())));
    assert_eq!(m.get("code"), m.get("neo4j_code"), "`code` stays beside `neo4j_code`");
    assert_eq!(m.get("gql_status"), Some(&Value::Str("42001".into())), "a syntax error is GQL 42001");
    assert!(matches!(m.get("description"), Some(Value::Str(d)) if d.contains("invalid syntax")));
    let Some(Value::Map(rec)) = m.get("diagnostic_record") else { panic!("no diagnostic_record") };
    assert_eq!(rec.get("OPERATION"), Some(&Value::Str(String::new())));
    assert_eq!(rec.get("OPERATION_CODE"), Some(&Value::Str("0".into())));
    assert_eq!(rec.get("CURRENT_SCHEMA"), Some(&Value::Str("/".into())));
    assert_eq!(rec.get("_classification"), Some(&Value::Str("CLIENT_ERROR".into())));

    // A 5.0 client (legacy range 5.0..5.0) sees the pre-5.7 shape.
    let mut hs = LEGACY_HANDSHAKE;
    hs[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x05]);
    let mut five = ready_at(&hs, None);
    assert_eq!(five.version(), (5, 0));
    let m = failure_meta(&mut five, "MATCH (n RETURN n");
    assert_eq!(m.get("code"), Some(&Value::Str("Neo.ClientError.Statement.SyntaxError".into())));
    assert!(!m.contains_key("neo4j_code") && !m.contains_key("gql_status"), "5.0 never saw these keys");
}

#[test]
fn a_non_syntax_failure_is_the_general_gql_status_with_its_classification() {
    let mut six = ready_at(&DRIVER_HANDSHAKE, Some(&[0x00, 0x00, 0x00, 0x06, 0x00]));
    let m = failure_meta(&mut six, "CALL db.noSuchProcedure()");
    assert_eq!(m.get("gql_status"), Some(&Value::Str("50N42".into())));
    let Some(Value::Map(rec)) = m.get("diagnostic_record") else { panic!("no diagnostic_record") };
    assert_eq!(rec.get("_classification"), Some(&Value::Str("CLIENT_ERROR".into())));
}

#[test]
fn begin_and_run_report_the_home_db_from_5_8() {
    let mut six = ready_at(&DRIVER_HANDSHAKE, Some(&[0x00, 0x00, 0x00, 0x06, 0x00]));
    let r = run_stmt(&mut six, "RETURN 1");
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else { panic!("no RUN metadata") };
    assert_eq!(meta.get("db"), Some(&Value::Str("neo4j".into())));
    let mut five = ready_at(&LEGACY_HANDSHAKE, None);
    assert_eq!(five.version(), (5, 8));
    let r = run_stmt(&mut five, "RETURN 1");
    let Pack::Value(Value::Map(meta)) = &r[0].1[0] else { panic!("no RUN metadata") };
    assert_eq!(meta.get("db"), Some(&Value::Str("neo4j".into())), "5.8 is where `db` began");
}

/// Encode a Vector structure by hand: `B2 56`, the type marker as an
/// integer, the packed data as a byte array.
fn vector_bytes(marker: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0xB2, 0x56];
    engram_bolt::packstream::encode_struct(
        0x56,
        &[Pack::Value(Value::Int(i64::from(marker))), Pack::Bytes(data.to_vec())],
        &mut out,
    )
    .expect("encodes");
    // `encode_struct` wrote its own header; drop the hand-written one.
    out.drain(..2);
    out
}

#[test]
fn a_vector_decodes_to_a_list_of_numbers_at_the_markers_width() {
    // float32 [1.5, -2.0]
    let mut data = Vec::new();
    data.extend_from_slice(&1.5f32.to_be_bytes());
    data.extend_from_slice(&(-2.0f32).to_be_bytes());
    let bytes = vector_bytes(0xC6, &data);
    let v = decode_value(Decoder::new(&bytes).decode().expect("decodes")).expect("a value");
    assert_eq!(v, Value::List(vec![Value::Float(1.5), Value::Float(-2.0)]));
    // int16 [300, -1]
    let mut data = Vec::new();
    data.extend_from_slice(&300i16.to_be_bytes());
    data.extend_from_slice(&(-1i16).to_be_bytes());
    let bytes = vector_bytes(0xC9, &data);
    let v = decode_value(Decoder::new(&bytes).decode().expect("decodes")).expect("a value");
    assert_eq!(v, Value::List(vec![Value::Int(300), Value::Int(-1)]));
    // A ragged payload is refused by name, not truncated.
    let bytes = vector_bytes(0xCA, &[0, 0, 1]);
    assert!(decode_value(Decoder::new(&bytes).decode().expect("decodes")).is_err());
}

#[test]
fn a_byte_array_parameter_arrives_as_a_list_of_byte_values() {
    let bytes = [0xCC, 0x03, 0x01, 0xFF, 0x10];
    let v = decode_value(Decoder::new(&bytes).decode().expect("decodes")).expect("a value");
    assert_eq!(v, Value::List(vec![Value::Int(1), Value::Int(255), Value::Int(16)]));
}

#[test]
fn a_vector_travels_as_a_run_parameter() {
    let mut six = ready_at(&DRIVER_HANDSHAKE, Some(&[0x00, 0x00, 0x00, 0x06, 0x00]));
    let mut data = Vec::new();
    for x in [0.25f64, 0.5, 1.0] {
        data.extend_from_slice(&x.to_be_bytes());
    }
    let mut params = Vec::new();
    // params map {v: Vector(...)} built by hand: A1 "v" then the structure.
    params.extend_from_slice(&[0xA1, 0x81, b'v']);
    params.extend(vector_bytes(0xC1, &data));
    let mut payload = vec![0xB3, 0x10];
    engram_bolt::packstream::encode_value(&Value::Str("RETURN $v AS v, size($v) AS n".into()), &mut payload)
        .expect("encodes");
    payload.extend(params);
    engram_bolt::packstream::encode_value(&Value::Map(BTreeMap::new()), &mut payload).expect("encodes");
    let mut bytes = (payload.len() as u16).to_be_bytes().to_vec();
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&[0, 0]);
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    bytes.extend(msg(0x3F, vec![map_field(pull)]));
    let r = replies(&six.feed(&bytes).expect("run+pull"));
    assert_eq!(r[0].0, 0x70, "RUN succeeded: {:?}", r[0].1);
    let Pack::Value(Value::List(row)) = &r[1].1[0] else { panic!("no record") };
    assert_eq!(row[0], Value::List(vec![Value::Float(0.25), Value::Float(0.5), Value::Float(1.0)]));
    assert_eq!(row[1], Value::Int(3));
}
