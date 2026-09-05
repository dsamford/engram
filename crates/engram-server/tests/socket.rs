#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! Real sockets, real bytes: the adapter driven exactly as a driver would.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use engram_bolt::client::Client;
use engram_bolt::{Decoder, Pack};
use engram_cypher::Value;
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The real driver's handshake bytes (manifest probe + ranges).
const DRIVER_HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x00, 0x00,
];

fn start_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        let _ = engram_server::run_server(listener, || (Store::new(), Realm(1), Namespace(1)));
    });
    addr
}

fn msg(tag: u8, fields: Vec<Pack>) -> Vec<u8> {
    let mut payload = Vec::new();
    engram_bolt::packstream::encode_struct(tag, &fields, &mut payload).expect("encodes");
    let mut out = (payload.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&payload);
    out.extend_from_slice(&[0, 0]);
    out
}

fn map_field(m: BTreeMap<String, Value>) -> Pack {
    Pack::Value(Value::Map(m))
}

fn str_field(s: &str) -> Pack {
    Pack::Value(Value::Str(s.to_string()))
}

/// Read from the socket until `n` complete messages have arrived.
fn read_messages(stream: &mut TcpStream, n: usize) -> Vec<(u8, Vec<Pack>)> {
    let mut raw = Vec::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while out.len() < n {
        let got = stream.read(&mut buf).expect("read");
        assert!(
            got > 0,
            "the server closed early after {} message(s)",
            out.len()
        );
        raw.extend_from_slice(&buf[..got]);
        out = parse_messages(&raw);
    }
    out
}

fn parse_messages(bytes: &[u8]) -> Vec<(u8, Vec<Pack>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut payload = Vec::new();
    while at + 2 <= bytes.len() {
        let size = u16::from_be_bytes(bytes[at..at + 2].try_into().expect("2")) as usize;
        at += 2;
        if size == 0 {
            if !payload.is_empty() {
                if let Ok(Pack::Struct { tag, fields }) = Decoder::new(&payload).decode() {
                    out.push((tag, fields));
                }
                payload.clear();
            }
            continue;
        }
        if at + size > bytes.len() {
            break;
        }
        payload.extend_from_slice(&bytes[at..at + size]);
        at += size;
    }
    out
}

fn connect_ready(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(&DRIVER_HANDSHAKE).expect("handshake out");
    let mut reply = [0u8; 4];
    s.read_exact(&mut reply).expect("handshake in");
    assert_eq!(reply, [0, 0, 8, 5], "negotiated 5.8 over TCP");
    s.write_all(&msg(0x01, vec![map_field(BTreeMap::new())]))
        .expect("hello");
    let r = read_messages(&mut s, 1);
    assert_eq!(r[0].0, 0x70);
    s.write_all(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    let r = read_messages(&mut s, 1);
    assert_eq!(r[0].0, 0x70);
    s
}

fn connect_ready_ns(addr: std::net::SocketAddr, namespace: i64) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(&DRIVER_HANDSHAKE).expect("handshake out");
    let mut reply = [0u8; 4];
    s.read_exact(&mut reply).expect("handshake in");
    // HELLO carrying the namespace extra — the federation routing key.
    let mut extras = BTreeMap::new();
    extras.insert("namespace".to_string(), Value::Int(namespace));
    s.write_all(&msg(0x01, vec![map_field(extras)]))
        .expect("hello");
    assert_eq!(read_messages(&mut s, 1)[0].0, 0x70);
    s.write_all(&msg(0x6A, vec![map_field(BTreeMap::new())]))
        .expect("logon");
    assert_eq!(read_messages(&mut s, 1)[0].0, 0x70);
    s
}

fn run_pull(s: &mut TcpStream, q: &str, expect_msgs: usize) -> Vec<(u8, Vec<Pack>)> {
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
    s.write_all(&bytes).expect("run+pull");
    read_messages(s, expect_msgs)
}

#[test]
fn a_whole_session_over_a_REAL_socket() {
    let addr = start_server();
    let mut s = connect_ready(addr);
    let r = run_pull(&mut s, "CREATE (:P {name: 'Ada', xs: [1, 2]})", 2);
    assert!(r.iter().all(|(t, _)| *t == 0x70), "write succeeded: {r:?}");
    let r = run_pull(&mut s, "MATCH (p:P) RETURN p.name, p.xs", 3);
    assert_eq!(r[0].0, 0x70);
    assert_eq!(r[1].0, 0x71, "one record");
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!("record row")
    };
    assert_eq!(
        row,
        &vec![
            Value::Str("Ada".into()),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        ],
        "identical decoded values, through TCP"
    );
    assert_eq!(r[2].0, 0x70, "stream summary");
}

#[test]
fn two_connections_share_ONE_shard() {
    // The engine-thread architecture's observable: what connection A writes,
    // connection B reads — one store behind every session.
    let addr = start_server();
    let mut a = connect_ready(addr);
    let mut b = connect_ready(addr);
    let r = run_pull(&mut a, "CREATE (:Shared {v: 42})", 2);
    assert!(r.iter().all(|(t, _)| *t == 0x70));
    let r = run_pull(&mut b, "MATCH (n:Shared) RETURN n.v", 3);
    assert_eq!(r[1].0, 0x71);
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    assert_eq!(row, &vec![Value::Int(42)], "B sees A's write");
}

#[test]
fn an_http_probe_dies_alone_and_the_server_lives_on() {
    let addr = start_server();
    // The misdirected client: an HTTP request on the Bolt port.
    let mut h = TcpStream::connect(addr).expect("connect");
    h.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("http");
    let mut buf = [0u8; 64];
    let n = h.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "the connection is closed, not answered");
    // And a REAL client right after works — the refusal was per-connection.
    let mut s = connect_ready(addr);
    let r = run_pull(&mut s, "RETURN 7 AS x", 3);
    assert_eq!(r[1].0, 0x71);
}

#[test]
fn datetime_now_works_over_the_wire_via_the_injected_clock() {
    // The adapter injects the wall clock; the engine never reads one. The
    // observable: datetime() over TCP answers a plausible NOW, not a refusal
    // and not the epoch.
    let addr = start_server();
    let mut s = connect_ready(addr);
    let r = run_pull(&mut s, "RETURN datetime().year AS y", 3);
    assert_eq!(r[1].0, 0x71, "datetime() answered: {r:?}");
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    let Value::Int(year) = row[0] else {
        panic!("year: {row:?}")
    };
    assert!(
        (2026..2100).contains(&year),
        "a real year, not 1970: {year}"
    );
}

#[test]
fn two_connections_share_ONE_graph() {
    // What A creates, B queries — over a real socket. (This certifies the
    // definition's VISIBILITY; that the index is built once for the server
    // rather than once per connection is the bolt crate's build-count
    // test, which a socket cannot observe.)
    let addr = start_server();
    let mut a = connect_ready(addr);
    let mut b = connect_ready(addr);
    let r = run_pull(
        &mut a,
        "CREATE (:Vec {v: 1, e: [1.0, 0.0]}), (:Vec {v: 2, e: [0.0, 1.0]})",
        2,
    );
    assert!(r.iter().all(|(t, _)| *t == 0x70));
    let r = run_pull(
        &mut a,
        "CREATE VECTOR INDEX shared_vi FOR (n:Vec) ON (n.e)",
        2,
    );
    assert!(r.iter().all(|(t, _)| *t == 0x70), "{r:?}");
    let r = run_pull(
        &mut b,
        "CALL db.index.vector.queryNodes('shared_vi', 1, [0.9, 0.1]) YIELD node, score RETURN node.v",
        3,
    );
    assert_eq!(r[1].0, 0x71, "B queries A's index: {r:?}");
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    assert_eq!(row, &vec![Value::Int(1)], "B sees A's index");
}

#[test]
fn sessions_on_different_namespaces_are_ISOLATED() {
    // Federation routing: a HELLO `namespace` binds the session to that
    // coordinate's graph. What namespace 7 writes, namespace 8 does not see;
    // what namespace 7 writes, ANOTHER namespace-7 session DOES see (they
    // share the coordinate's graph and its caches). Per-namespace id spaces
    // make this isolation structural, not a filter.
    let addr = start_server();
    let mut a = connect_ready_ns(addr, 7);
    let mut b = connect_ready_ns(addr, 8);
    let mut a2 = connect_ready_ns(addr, 7);

    let r = run_pull(&mut a, "CREATE (:Tenant {v: 7})", 2);
    assert!(r.iter().all(|(t, _)| *t == 0x70));

    // Namespace 8 does not see namespace 7's node.
    let r = run_pull(&mut b, "MATCH (n:Tenant) RETURN count(n) AS c", 3);
    assert_eq!(r[1].0, 0x71);
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    assert_eq!(row, &vec![Value::Int(0)], "namespace 8 is isolated from 7");

    // A second namespace-7 session DOES see it.
    let r = run_pull(&mut a2, "MATCH (n:Tenant) RETURN n.v", 3);
    assert_eq!(r[1].0, 0x71);
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    assert_eq!(
        row,
        &vec![Value::Int(7)],
        "the same namespace shares its graph"
    );

    // The default (no-namespace) session is its own coordinate, isolated too.
    let mut d = connect_ready(addr);
    let r = run_pull(&mut d, "MATCH (n:Tenant) RETURN count(n) AS c", 3);
    let Pack::Value(Value::List(row)) = &r[1].1[0] else {
        panic!()
    };
    assert_eq!(
        row,
        &vec![Value::Int(0)],
        "the default coordinate is isolated"
    );
}

#[test]
fn the_reusable_bolt_client_round_trips_over_a_real_socket() {
    // The `engram_bolt::client::Client` the concurrency harness (snbconc) is
    // built from, driven against the real server over a real socket: it must
    // handshake, LOGON, RUN + PULL, and decode RECORDs identically to the
    // hand-rolled byte path the rest of this file uses. This is the canary for
    // the harness — a framing or handshake regression fails HERE, not silently
    // as a benchmark that measures nothing.
    let addr = start_server();
    let mut c = Client::connect(addr).expect("client reaches Ready");

    // Multiple RECORDs, decoded to their row Values — no data needed.
    let rows = c.query("UNWIND [1, 2, 3] AS x RETURN x").expect("unwind");
    assert_eq!(
        rows,
        vec![
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(2)]),
            Value::List(vec![Value::Int(3)]),
        ],
        "three RECORDs, each a one-column row, decoded through the client"
    );

    // The counting path the throughput harness actually uses agrees.
    assert_eq!(c.run("UNWIND [1, 2, 3] AS x RETURN x").expect("count"), 3);

    // A write then a read on the SAME connection proves the round-trip carries
    // real data, and that the connection is reusable across statements.
    assert_eq!(
        c.run("CREATE (:Widget {sku: 'A1', qty: 7})")
            .expect("create"),
        0
    );
    let got = c
        .query("MATCH (w:Widget) RETURN w.sku, w.qty")
        .expect("match");
    assert_eq!(
        got,
        vec![Value::List(vec![Value::Str("A1".into()), Value::Int(7)])],
        "the written row reads back through the client, byte-identical"
    );
}
