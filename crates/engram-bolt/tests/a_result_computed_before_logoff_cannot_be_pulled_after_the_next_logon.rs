//! Security milestone §2.8: LOGOFF ends everything the previous principal
//! left on the connection.
//!
//! Bolt 5.1 lets a driver re-authenticate a connection in place — LOGOFF, then
//! LOGON as someone else — which is how a connection pool serves several users
//! without reconnecting. LOGOFF used to change only the session's state, so an
//! open result stream and an explicit transaction both survived it: the next
//! principal could PULL rows computed for the previous one, and COMMIT writes
//! the previous one buffered. Harmless while credentials are not verified;
//! a cross-principal read and write the moment they are. LOGOFF now does what
//! RESET does: every stream dropped, the transaction rolled back.

use std::collections::BTreeMap;

use engram_bolt::{BoltServer, Decoder, Pack};
use engram_cypher::Value;
use engram_graph::Graph;
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const HELLO: u8 = 0x01;
const LOGON: u8 = 0x6A;
const LOGOFF: u8 = 0x6B;
const RUN: u8 = 0x10;
const PULL: u8 = 0x3F;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;

/// The 6.x driver's handshake: manifest v1, offering 5.8..5.0 as a range.
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

fn send(s: &mut BoltServer, tag: u8, fields: Vec<Pack>) -> Vec<(u8, Vec<Pack>)> {
    replies(&s.feed(&msg(tag, fields)).expect("feed"))
}

fn logged_on(g: &std::sync::Arc<Graph>) -> BoltServer {
    let mut s = BoltServer::shared(std::sync::Arc::clone(g));
    s.feed(&DRIVER_HANDSHAKE).expect("handshake");
    s.feed(&PICK_6_0).expect("pick");
    assert_eq!(send(&mut s, HELLO, vec![empty_map()])[0].0, SUCCESS);
    assert_eq!(send(&mut s, LOGON, vec![empty_map()])[0].0, SUCCESS);
    s
}

fn run_only(s: &mut BoltServer, q: &str) -> Vec<(u8, Vec<Pack>)> {
    send(
        s,
        RUN,
        vec![Pack::Value(Value::Str(q.to_string())), empty_map(), empty_map()],
    )
}

fn pull_all(s: &mut BoltServer) -> Vec<(u8, Vec<Pack>)> {
    let mut m = BTreeMap::new();
    m.insert("n".to_string(), Value::Int(-1));
    send(s, PULL, vec![Pack::Value(Value::Map(m))])
}

fn count(s: &mut BoltServer, label: &str) -> i64 {
    run_only(s, &format!("MATCH (x:{label}) RETURN count(x) AS c"));
    let r = pull_all(s);
    let rec = r.iter().find(|(t, _)| *t == RECORD).expect("a record");
    let Pack::Value(Value::List(row)) = &rec.1[0] else {
        panic!("a record is a list");
    };
    match &row[0] {
        Value::Int(n) => *n,
        other => panic!("expected a count, got {other:?}"),
    }
}

fn relogon(s: &mut BoltServer) {
    assert_eq!(send(s, LOGOFF, vec![])[0].0, SUCCESS, "LOGOFF");
    assert_eq!(send(s, LOGON, vec![empty_map()])[0].0, SUCCESS, "LOGON again");
}

#[test]
fn an_unpulled_result_is_gone_after_logoff() {
    let g = std::sync::Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    let mut s = logged_on(&g);
    run_only(&mut s, "UNWIND [1, 2, 3] AS x CREATE (:Private {v: x})");
    pull_all(&mut s);

    // The first principal runs a query and leaves its result unpulled.
    let r = run_only(&mut s, "MATCH (p:Private) RETURN p.v AS v ORDER BY v");
    assert_eq!(r[0].0, SUCCESS, "RUN accepted, result held for PULL");

    relogon(&mut s);

    // The next principal pulls: none of those rows may arrive.
    let r = pull_all(&mut s);
    assert_eq!(
        r.iter().filter(|(t, _)| *t == RECORD).count(),
        0,
        "a row computed for the previous principal was delivered to the next: {r:?}"
    );
}

#[test]
fn an_uncommitted_transaction_is_rolled_back_by_logoff() {
    let g = std::sync::Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    let mut s = logged_on(&g);
    assert_eq!(send(&mut s, BEGIN, vec![empty_map()])[0].0, SUCCESS, "BEGIN");
    run_only(&mut s, "CREATE (:Buffered {n: 1})");
    pull_all(&mut s);

    relogon(&mut s);

    // The next principal's COMMIT must not publish the previous one's write.
    send(&mut s, COMMIT, vec![empty_map()]);
    let mut other = logged_on(&g);
    assert_eq!(
        count(&mut other, "Buffered"),
        0,
        "the previous principal's buffered write was committed by the next"
    );
    // And the re-authenticated session is not inside a transaction: its own
    // autocommit write is visible to everyone at once.
    run_only(&mut s, "CREATE (:Own {n: 1})");
    pull_all(&mut s);
    assert_eq!(count(&mut other, "Own"), 1, "autocommit after re-LOGON");
}
