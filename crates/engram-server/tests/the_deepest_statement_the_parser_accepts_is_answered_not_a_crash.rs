#![allow(clippy::disallowed_methods)]
//! Bounded nesting is a guarantee only on a stack large enough to REACH the
//! bound (`engram_cypher::MIN_PARSER_STACK_BYTES`). The Cypher hostile suite
//! proves the bound on a thread it sizes itself; this proves the SERVER gives
//! the threads that parse and evaluate statements that stack.
//!
//! It did not: the engine workers and the morsel workers were spawned at the
//! platform default, so the parser's declared minimum was a promise nothing on
//! the serving path kept. A stack overflow is not a panic — it aborts the
//! process, taking every session with it — and the input that causes it needs
//! no credential. Found while sizing the fuzz targets' threads (security plan
//! §10, step 0).
//!
//! For each nesting shape, the test finds the deepest statement the parser
//! accepts, on a correctly sized stack of its own, then sends exactly that to a
//! real server. Anything the parser accepts, the server must answer, and must
//! still be serving afterwards.

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn start_server() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        let _ = engram_server::run_server(listener, || (Store::new(), Realm(1), Namespace(1)));
    });
    addr
}

/// The deepest `depth` at which `shape(depth)` still parses, probed on a
/// thread with the parser's declared stack.
fn deepest_accepted(shape: fn(usize) -> String) -> (usize, String) {
    std::thread::Builder::new()
        .name("parse-probe".to_string())
        .stack_size(engram_cypher::MIN_PARSER_STACK_BYTES)
        .spawn(move || {
            let mut best = None;
            for depth in 1..=200 {
                let src = shape(depth);
                if engram_cypher::parse_any(&src).is_ok() {
                    best = Some((depth, src));
                } else {
                    break;
                }
            }
            best.expect("the shallowest form parses")
        })
        .expect("spawn")
        .join()
        .expect("probe")
}

// The shapes whose VALUE is as deep as the expression return a scalar made
// from it: a 60-deep list is a legal answer, but PackStream's own decode bound
// (64, counting the RECORD around it) is the client's to enforce, and this test
// is about the server surviving the statement, not the client reading it.
fn lists(d: usize) -> String {
    format!("RETURN size({}1{}) AS v", "[".repeat(d), "]".repeat(d))
}
fn parens(d: usize) -> String {
    format!("RETURN {}1{} AS v", "(1 + ".repeat(d), ")".repeat(d))
}
fn calls(d: usize) -> String {
    format!("RETURN {}-1{} AS v", "abs(".repeat(d), ")".repeat(d))
}
fn negations(d: usize) -> String {
    format!("RETURN {}true AS v", "NOT ".repeat(d))
}
fn maps(d: usize) -> String {
    format!("RETURN size(keys({}1{})) AS v", "{k: ".repeat(d), "}".repeat(d))
}
fn cases(d: usize) -> String {
    format!(
        "RETURN {}1{} AS v",
        "CASE WHEN true THEN ".repeat(d),
        " ELSE 0 END".repeat(d)
    )
}

#[test]
fn the_deepest_accepted_statement_of_every_shape_is_answered() {
    let addr = start_server();
    let mut c = Client::connect(addr).expect("connect");
    for (name, shape) in [
        ("lists", lists as fn(usize) -> String),
        ("parenthesised arithmetic", parens),
        ("function calls", calls),
        ("negations", negations),
        ("maps", maps),
        ("CASE", cases),
    ] {
        let (depth, src) = deepest_accepted(shape);
        let rows = c.query(&src).unwrap_or_else(|e| {
            panic!("{name} at depth {depth}, the deepest the parser accepts: the server failed: {e}")
        });
        assert_eq!(rows.len(), 1, "{name} at depth {depth}: one row");
    }
    // Still serving: the process did not abort under any of the above.
    let mut again = Client::connect(addr).expect("the server is still accepting");
    assert_eq!(again.run("RETURN 1 AS one").expect("still answering"), 1);
}
