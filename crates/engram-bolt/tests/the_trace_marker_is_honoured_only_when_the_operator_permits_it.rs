//! Security milestone §2.13: a client cannot switch tracing on for itself.
//!
//! `/* engram:trace */` at the head of a statement dumps that statement's
//! counters to the server log. The marker is the CLIENT's choice, and tracing
//! is expensive — one LSQB shape measured 3 s untraced and 35 s traced — and
//! writes the statement's text into the operator's log. So it is honoured only
//! when the operator permits it (`BoltServer::set_trace_marker`, set from
//! `ENGRAM_TRACE_MARKER` by the server), and otherwise ignored as the comment
//! it is.
//!
//! Both directions are asserted, from the process-wide counters in
//! `engram_bolt::counters` — the channel an operator reads, and the one that
//! survives the traced path: a traced statement runs inside its own trace,
//! which replaces the caller's. The unpermitted direction adds the evidence of
//! where the work landed: its engine work is in the caller's trace, so it ran
//! untraced, and a counter saying "ignored" over a traced run would fail.
//! Both directions live in ONE test because those counters are process-wide
//! and tests in one binary run in parallel.

use std::collections::BTreeMap;

use engram_bolt::{BoltServer, Decoder, Pack, TRACE_MARKER};
use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SUCCESS: u8 = 0x70;
const IGNORED: &str = "bolt.trace marker ignored: tracing not permitted";
/// Recorded by the engine for the statement below. Present in the caller's
/// trace only when the statement ran UNtraced.
const ENGINE_WORK: &str = "store.gets";

const DRIVER_HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, // magic
    0x00, 0x00, 0x01, 0xFF, // manifest v1
    0x00, 0x08, 0x08, 0x05, // 5.8 back to 5.0
    0x00, 0x02, 0x04, 0x04, // 4.4 back to 4.2
    0x00, 0x00, 0x00, 0x03, // 3.0
];
const PICK_6_0: [u8; 5] = [0x00, 0x00, 0x00, 0x06, 0x00];

fn msg(tag: u8, fields: Vec<Pack>) -> Vec<u8> {
    let mut payload = Vec::new();
    engram_bolt::packstream::encode_struct(tag, &fields, &mut payload).expect("encodes");
    let mut out = (payload.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&payload);
    out.extend_from_slice(&[0, 0]);
    out
}

fn empty_map() -> Pack {
    Pack::Value(Value::Map(BTreeMap::new()))
}

fn first_reply(bytes: &[u8]) -> u8 {
    let size = u16::from_be_bytes(bytes[0..2].try_into().expect("2")) as usize;
    let mut d = Decoder::new(&bytes[2..2 + size]);
    let Pack::Struct { tag, .. } = d.decode().expect("reply decodes") else {
        panic!("reply was not a structure");
    };
    tag
}

fn ready(permitted: bool) -> BoltServer {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut s = BoltServer::new(g);
    s.set_trace_marker(permitted);
    s.feed(&DRIVER_HANDSHAKE).expect("handshake");
    s.feed(&PICK_6_0).expect("pick");
    assert_eq!(first_reply(&s.feed(&msg(0x01, vec![empty_map()])).expect("hello")), SUCCESS);
    assert_eq!(first_reply(&s.feed(&msg(0x6A, vec![empty_map()])).expect("logon")), SUCCESS);
    run_and_pull(&mut s, "UNWIND range(1, 20) AS x CREATE (:T {v: x})");
    s
}

fn run_and_pull(s: &mut BoltServer, q: &str) -> Vec<u8> {
    let mut bytes = msg(
        0x10,
        vec![Pack::Value(Value::Str(q.to_string())), empty_map(), empty_map()],
    );
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Int(-1));
    bytes.extend(msg(0x3F, vec![Pack::Value(Value::Map(pull))]));
    s.feed(&bytes).expect("run+pull")
}

fn run_marked(s: &mut BoltServer) -> BTreeMap<String, u64> {
    let q = format!("{TRACE_MARKER} MATCH (t:T) WHERE t.v > 10 RETURN t.v AS v");
    let (reply, trace) = engram_observe::with_trace(|| run_and_pull(s, &q));
    assert_eq!(first_reply(&reply), SUCCESS, "the statement itself succeeds either way");
    trace.counters().clone()
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

fn totals() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        engram_bolt::counters::TRACE_MARKER_HONOURED.load(Relaxed),
        engram_bolt::counters::TRACE_MARKER_IGNORED.load(Relaxed),
    )
}

#[test]
fn the_marker_traces_only_where_the_operator_permits_it() {
    // Unpermitted — the default: an ordinary comment.
    let mut s = ready(false);
    let (h0, i0) = totals();
    let c = run_marked(&mut s);
    let (h1, i1) = totals();
    assert_eq!((h1 - h0, i1 - i0), (0, 1), "ignored, not honoured");
    assert_eq!(get(&c, IGNORED), 1, "{c:?}");
    assert!(
        get(&c, ENGINE_WORK) > 0,
        "the statement's engine work landed in the caller's trace, so it ran \
         untraced: {c:?}"
    );

    // Permitted: traced.
    let mut s = ready(true);
    let (h0, i0) = totals();
    run_marked(&mut s);
    let (h1, i1) = totals();
    assert_eq!((h1 - h0, i1 - i0), (1, 0), "honoured, not ignored");
}
