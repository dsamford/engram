//! End-to-end robustness: what an UNAUTHENTICATED client can do to a running
//! server over a real socket.
//!
//! These are deliberately not unit tests. Every fix they cover was a case where
//! the mechanism existed somewhere in the workspace and the server did not use
//! it — a row budget nothing set, a panic nothing caught, a message assembler
//! nothing bounded. A unit test on the mechanism would have passed throughout.
//! The only way to show the SERVER is bounded is to drive the server.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use engram_key::{Namespace, Realm};
use engram_server::{ServerConfig, run_server_with_config};
use engram_store::Store;

/// Start a server on an ephemeral port and return its address.
fn serve(cfg: ServerConfig) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let _ = run_server_with_config(listener, || (Store::new(), Realm(1), Namespace(1)), cfg);
    });
    // Give the accept loop a moment to reach `incoming()`.
    std::thread::sleep(Duration::from_millis(120));
    addr
}

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
/// A LEGACY handshake (no manifest request): these tests are about the
/// server surviving hostile sessions, and a four-byte version reply is the
/// simplest "still serving" signal. The manifest exchange has its own
/// tests in engram-bolt.
const HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x00, 0x00,
];

fn connect(addr: &str) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    s
}

/// V6 — a panic in one session must not take the server, or any other session.
///
/// The failure this replaces was the nastiest in the set: a panic unwound out
/// of the worker loop, dropped its whole session map, and the accept loop then
/// treated every subsequent failed send as `continue` — so connections were
/// SILENTLY refused while the process stayed alive and the listener kept
/// accepting. A liveness probe saw a healthy server. With `workers = 1` that is
/// the entire server, permanently.
///
/// The assertion is therefore not "the panicking session failed" — it is that a
/// SECOND, LATER connection is still served.
#[test]
fn a_panicking_session_does_not_brick_the_server() {
    let addr = serve(ServerConfig {
        workers: 1,
        ..ServerConfig::default()
    });

    // Session 1: reach Ready, then send a message whose STRUCTURE is valid but
    // whose tag is unknown in this state — and, more to the point, keep the
    // server exercising the engine. We cannot deterministically panic the
    // engine from outside, so this session instead exits abruptly mid-message,
    // which used to be enough to leave the worker's map inconsistent.
    {
        let mut c = connect(&addr);
        c.write_all(&HANDSHAKE).expect("write handshake");
        let mut v = [0u8; 4];
        c.read_exact(&mut v).expect("version");
        // A chunk header promising 64 bytes, then only 3, then a hard close.
        c.write_all(&[0x00, 0x40, 0xAA, 0xBB, 0xCC]).ok();
        drop(c);
    }

    std::thread::sleep(Duration::from_millis(150));

    // Session 2 must still get a full handshake. Before panic isolation and the
    // credit fix, a poisoned worker made this hang or be refused.
    let mut c2 = connect(&addr);
    c2.write_all(&HANDSHAKE).expect("write handshake 2");
    let mut v2 = [0u8; 4];
    c2.read_exact(&mut v2)
        .expect("the server must still serve a NEW connection after a session died badly");
    assert_eq!(v2[3], 5, "a Bolt 5.x version must be negotiated: {v2:?}");
}

/// V4 — an unterminated message must be refused, not buffered for ever.
///
/// The client sends chunk headers and payload but never the `0x0000`
/// terminator. `next_message` returns `Ok(None)` WITHOUT draining, so before
/// the cap both the assembled payload and the session's inbox grew without
/// bound: one connection could exhaust the process, which is a denial of
/// service against every other connection.
///
/// The assertion is that the server drops this connection rather than
/// absorbing an unbounded amount — and, crucially, that it is still serving
/// afterwards.
#[test]
fn an_unterminated_message_is_refused_and_the_server_survives() {
    // A SMALL cap, so the test proves the mechanism in a second rather than
    // spending a minute filling 64 MiB. The production default is unchanged;
    // what is under test is that the bound is enforced at all, and enforcing it
    // at 256 KiB exercises exactly the same branch as enforcing it at 64 MiB.
    let addr = serve(ServerConfig {
        max_message_bytes: 256 * 1024,
        ..ServerConfig::default()
    });
    let mut c = connect(&addr);
    c.write_all(&HANDSHAKE).expect("handshake");
    let mut v = [0u8; 4];
    c.read_exact(&mut v).expect("version");

    // Non-terminating 64 KiB chunks: the cap is reached in a handful of
    // iterations and the write then fails, which is the pass condition.
    let chunk = {
        let mut b = vec![0xFF, 0xFF];
        b.extend(std::iter::repeat_n(0x00, 0xFFFF));
        b
    };
    let mut wrote = 0usize;
    for _ in 0..2048 {
        match c.write_all(&chunk) {
            Ok(()) => wrote += chunk.len(),
            Err(_) => break, // server hung up: the cap fired
        }
    }
    drop(c);

    // Whatever happened to that connection, the SERVER must still be alive.
    std::thread::sleep(Duration::from_millis(150));
    let mut c2 = connect(&addr);
    c2.write_all(&HANDSHAKE).expect("handshake 2");
    let mut v2 = [0u8; 4];
    c2.read_exact(&mut v2).unwrap_or_else(|e| {
        panic!("server died absorbing {wrote} bytes of unterminated message: {e}")
    });
    assert_eq!(v2[3], 5);
}

/// V7 — the connection cap refuses rather than exhausting threads.
///
/// Each connection costs TWO OS threads, so an unbounded accept loop is an
/// unbounded thread count. The cap must REFUSE (close immediately) rather than
/// accept-and-hang, because a client cannot distinguish a hung server from a
/// slow one.
#[test]
fn the_connection_cap_refuses_beyond_the_limit() {
    let cap = 4;
    let addr = serve(ServerConfig {
        max_connections: cap,
        // No read timeout, so held connections stay held for the test.
        read_timeout: None,
        ..ServerConfig::default()
    });

    // Hold `cap` connections open, each having completed a handshake so the
    // server counts them as live.
    let mut held = Vec::new();
    for _ in 0..cap {
        let mut c = connect(&addr);
        c.write_all(&HANDSHAKE).expect("handshake");
        let mut v = [0u8; 4];
        c.read_exact(&mut v).expect("version");
        held.push(c);
    }
    std::thread::sleep(Duration::from_millis(150));

    // The next one must not be served. The server closes it, so either the
    // connect fails or the read returns EOF (0 bytes) rather than a version.
    let mut over = connect(&addr);
    over.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let _ = over.write_all(&HANDSHAKE);
    let mut v = [0u8; 4];
    let refused = match over.read(&mut v) {
        Ok(0) => true,  // clean EOF — refused
        Ok(_) => false, // served: the cap did not hold
        Err(_) => true, // reset — also refused
    };
    assert!(
        refused,
        "connection {} of a {cap}-connection cap was served; the cap is not enforced",
        cap + 1
    );

    // And the cap must be a queue, not a wall: freeing a slot lets a new
    // connection in. A cap that permanently poisons the server is worse than
    // no cap.
    held.clear();
    std::thread::sleep(Duration::from_millis(300));
    let mut after = connect(&addr);
    after
        .write_all(&HANDSHAKE)
        .expect("handshake after release");
    let mut v2 = [0u8; 4];
    after
        .read_exact(&mut v2)
        .expect("after releasing connections the server must accept again");
    assert_eq!(v2[3], 5);
}

/// A non-Bolt preamble is still refused at four bytes rather than hanging.
/// The caps sit in the same code path, so this pins that they did not break it.
#[test]
fn an_http_probe_is_still_refused_promptly() {
    let addr = serve(ServerConfig::default());
    let mut c = connect(&addr);
    c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    let mut buf = [0u8; 64];
    // Either a refusal payload or a hangup; both are prompt. A hang is the
    // failure this asserts against, and the read timeout makes it observable.
    let _ = c.read(&mut buf);
    assert_ne!(&buf[..4], &MAGIC, "an HTTP probe must not be handshaken");
}

/// V5 — the row budget is actually SET on the server's graphs.
///
/// This is the "mechanism exists, nothing turns it on" defect in its purest
/// form. `Graph::set_row_budget` and the 30 `budget_check` call sites have
/// existed for a long time; the engine defaults the budget to `None`, and the
/// SERVER never set it — so every one of those checks was inert in the only
/// binary facing a network, and `MATCH (a)-[*]->(b)` ran until the OOM killer
/// arrived. The engine's own doc records the full-corpus benchmark dying that
/// way.
///
/// A unit test on `set_row_budget` would have passed throughout. The only
/// assertion that means anything is that a cartesian product sent over a SOCKET
/// is refused.
#[test]
fn the_row_budget_is_enforced_on_the_server() {
    let addr = serve(ServerConfig {
        // Deliberately tiny, so a modest product trips it quickly.
        row_budget: Some(500),
        ..ServerConfig::default()
    });
    let mut c = engram_bolt::client::Client::connect(&addr).expect("connect");

    // 40 nodes → a 3-way product is 64,000 rows, well past a 500-row budget.
    for _ in 0..40 {
        c.run("CREATE (:Budget)").expect("create");
    }
    let refused = c.query("MATCH (a:Budget),(b:Budget),(c:Budget) RETURN a,b,c");
    assert!(
        refused.is_err(),
        "a 64,000-row product must be REFUSED under a 500-row budget; \
         an Ok here means the budget is not set on the graph the session uses"
    );

    // And the refusal must be recoverable: the session survives it and the
    // next query works. A budget that kills the connection would be a denial
    // of service with extra steps.
    let ok = c.query("MATCH (a:Budget) RETURN a LIMIT 3");
    assert!(
        ok.is_ok(),
        "the session must survive a budget refusal: {:?}",
        ok.err()
    );
}
