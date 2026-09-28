//! A Bolt server sends a zero-length chunk as a keep-alive NOOP while a long
//! request runs (Neo4j: every `server.bolt.connection_keep_alive`, one minute
//! by default). The chunk carries no message; a client that reads it as an
//! empty message decodes nothing and fails. The LSQB comparison's slowest
//! Neo4j count sat at 56–58 s for three runs and passed; the first window in
//! which it crossed a minute failed all three runs with
//! `decode: Truncated { at: 0 }`, and the arm was not quotable.
//!
//! The peer here is a fake server on a local socket that speaks exactly the
//! subset the client needs and injects NOOPs wherever a real server may: in
//! front of every reply, including between a RECORD and its summary. The
//! control is the same exchange with no NOOPs, so a failure of the first test
//! alone is the chunk reader, and a failure of both is the fake.
//!
//! Canary: reverting the reader's `payload.is_empty()` branch fails the first
//! test with `decode: Truncated { at: 0 }` and leaves the control green.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use engram_bolt::client::Client;
use engram_bolt::packstream::{Pack, encode_struct};
use engram_cypher::Value;

const MSG_HELLO: u8 = 0x01;
const MSG_RUN: u8 = 0x10;
const MSG_PULL: u8 = 0x3F;
const MSG_LOGON: u8 = 0x6A;
const MSG_SUCCESS: u8 = 0x70;
const MSG_RECORD: u8 = 0x71;

/// Read one chunked message off the client and return its struct tag.
fn read_tag(s: &mut TcpStream) -> u8 {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        s.read_exact(&mut len).expect("chunk header");
        let n = u16::from_be_bytes(len) as usize;
        if n == 0 {
            if payload.is_empty() {
                continue;
            }
            break;
        }
        let start = payload.len();
        payload.resize(start + n, 0);
        s.read_exact(&mut payload[start..]).expect("chunk body");
    }
    // A struct header is a marker byte (0xB0 | field count), then the tag.
    payload[1]
}

fn send(s: &mut TcpStream, tag: u8, fields: &[Pack]) {
    let mut payload = Vec::new();
    encode_struct(tag, fields, &mut payload).expect("encode");
    let mut framed = Vec::new();
    framed.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    framed.extend_from_slice(&payload);
    framed.extend_from_slice(&[0, 0]);
    s.write_all(&framed).expect("write");
}

/// `n` keep-alive chunks: zero-length, outside any message.
fn noop(s: &mut TcpStream, n: usize) {
    for _ in 0..n {
        s.write_all(&[0, 0]).expect("noop");
    }
}

fn map(kv: &[(&str, Value)]) -> Pack {
    Pack::Value(Value::Map(
        kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
    ))
}

/// A fake Bolt 5 peer on one accepted connection: handshake, HELLO, LOGON,
/// then one RUN + PULL, with `noops` keep-alive chunks in front of every
/// reply.
fn serve(l: &TcpListener, noops: usize) {
    let (mut s, _) = l.accept().expect("accept");
    let mut hs = [0u8; 20];
    s.read_exact(&mut hs).expect("handshake");
    assert_eq!(&hs[..4], &[0x60, 0x60, 0xB0, 0x17], "bolt magic");
    s.write_all(&[0, 0, 8, 5]).expect("version");
    assert_eq!(read_tag(&mut s), MSG_HELLO);
    noop(&mut s, noops);
    send(
        &mut s,
        MSG_SUCCESS,
        &[map(&[("server", Value::Str("fake/1".to_string()))])],
    );
    assert_eq!(read_tag(&mut s), MSG_LOGON);
    noop(&mut s, noops);
    send(&mut s, MSG_SUCCESS, &[map(&[])]);
    // The client pipelines PULL behind RUN, so both arrive before any reply.
    assert_eq!(read_tag(&mut s), MSG_RUN);
    assert_eq!(read_tag(&mut s), MSG_PULL);
    noop(&mut s, noops);
    send(
        &mut s,
        MSG_SUCCESS,
        &[map(&[(
            "fields",
            Value::List((vec![Value::Str("n".to_string())]).into()),
        )])],
    );
    noop(&mut s, noops);
    send(
        &mut s,
        MSG_RECORD,
        &[Pack::Value(Value::List((vec![Value::Int(42)]).into()))],
    );
    noop(&mut s, noops);
    send(&mut s, MSG_SUCCESS, &[map(&[])]);
}

/// Run `body` against a fake peer served on a scoped thread (the workspace
/// forbids a free `std::thread::spawn`; a scope joins the peer before the
/// test returns, so a peer that panics fails the test rather than leaking).
fn with_fake_server(noops: usize, body: impl FnOnce(&str)) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr").to_string();
    thread::scope(|scope| {
        scope.spawn(|| serve(&l, noops));
        body(&addr);
    });
}

#[test]
fn a_keep_alive_noop_in_front_of_every_reply_is_skipped() {
    with_fake_server(3, |addr| {
        let mut c = Client::connect(addr).expect("connect through keep-alives");
        assert_eq!(c.server_agent(), "fake/1");
        let rows = c
            .query("RETURN 42 AS n")
            .expect("a query answered through keep-alives");
        assert_eq!(rows, vec![Value::List((vec![Value::Int(42)]).into())]);
    });
}

#[test]
fn b_the_same_exchange_without_keep_alives_answers() {
    with_fake_server(0, |addr| {
        let mut c = Client::connect(addr).expect("connect");
        assert_eq!(c.server_agent(), "fake/1");
        let rows = c.query("RETURN 42 AS n").expect("query");
        assert_eq!(rows, vec![Value::List((vec![Value::Int(42)]).into())]);
    });
}
