#![allow(clippy::disallowed_methods)]
//! An algorithm's `write` mode must be reachable by a real client.
//!
//! It was not. For a release, `CALL engram.algo.<alg>.write(…)` over Bolt — a
//! fresh server, one client, one autocommit statement — was refused with "an
//! algorithm's `write` mode cannot run inside an open transaction".
//!
//! The mechanism is a loop the mode closes on itself. The server wraps every
//! statement that *can* write in a serialisable autocommit transaction
//! (`engram-bolt/src/server.rs`, the `parsed.may_write()` arm). `write` mode
//! can write, so it declared itself a writer, so the server wrapped it — and
//! it then refused to run inside the wrapper that exists to make its own write
//! durable.
//!
//! **Every functional test of the mode passed throughout**, because they call
//! `run_query` directly and that installs no transaction. The mode was
//! catalogued, documented, tested in all four modes, and unusable by anybody.
//! The distinction it actually needs is EXPLICIT transaction versus the
//! server's single-statement wrapper: the hazards the rule exists for — seeing
//! one's own uncommitted writes in the read snapshot, holding entity locks
//! across a fixpoint — are all about a transaction containing OTHER
//! statements.
//!
//! This file exists at the Bolt level and not beside the other algorithm tests
//! precisely because that is where the gap was: a test one layer lower cannot
//! see it.

use std::net::TcpListener;

use engram_bolt::client::Client;
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let _ = engram_server::run_server_with_workers(
            listener,
            || (Store::new(), Realm(1), Namespace(1)),
            2,
        );
    });
    addr
}

fn connect(addr: &str) -> Client {
    for _ in 0..50 {
        if let Ok(c) = Client::connect(addr) {
            return c;
        }
    }
    panic!("server never became reachable");
}

fn seed(c: &mut Client) {
    c.run("CREATE (a:N {k:1}), (b:N {k:2}), (d:N {k:3})")
        .expect("seed nodes");
    c.run("MATCH (a:N {k:1}), (b:N {k:2}) CREATE (a)-[:R]->(b)")
        .expect("seed edge");
    c.run("MATCH (b:N {k:2}), (d:N {k:3}) CREATE (b)-[:R]->(d)")
        .expect("seed edge");
}

#[test]
fn every_write_mode_lands_its_property_over_bolt() {
    let addr = start();
    let mut c = connect(&addr);
    seed(&mut c);

    // Every algorithm's write mode, because the defect was in the DISPATCH and
    // not in any one algorithm — one of them passing would have said nothing
    // about the others.
    for (alg, prop) in [
        ("pagerank", "p_pr"),
        ("wcc", "p_wcc"),
        ("scc", "p_scc"),
        ("degree", "p_deg"),
        ("betweenness", "p_btw"),
        ("closeness", "p_clo"),
        ("labelpropagation", "p_lpa"),
        ("louvain", "p_lou"),
        ("trianglecount", "p_tri"),
    ] {
        let stmt = format!(
            "CALL engram.algo.{alg}.write({{nodeLabels:['N'], relationshipTypes:['R'], \
             writeProperty:'{prop}'}}) YIELD nodesWritten RETURN nodesWritten"
        );
        let rows = c.query(&stmt).unwrap_or_else(|e| {
            panic!(
                "`{alg}.write` was refused over Bolt: {e}. The mode is catalogued and \
                 documented; if a client cannot call it, it does not exist"
            )
        });
        assert!(!rows.is_empty(), "{alg}.write returned no receipt");

        let landed = c
            .query(&format!(
                "MATCH (n:N) WHERE n.{prop} IS NOT NULL RETURN count(n)"
            ))
            .expect("count the written property");
        assert!(
            !landed.is_empty(),
            "{alg}.write reported a receipt but its property is not readable — the write \
             was reported and not performed",
        );
    }
}

#[test]
fn a_write_mode_receipt_carries_both_stamps() {
    // The MVCC honesty argument, over the wire: the scores describe snapshot
    // `asOf` and land at `committedAt`, and both are reported because they are
    // genuinely different moments. GDS returns one summary and never says the
    // scores describe a graph that no longer exists.
    let addr = start();
    let mut c = connect(&addr);
    seed(&mut c);
    let rows = c
        .query(
            "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
             writeProperty:'d'}) YIELD asOf, committedAt RETURN asOf, committedAt",
        )
        .expect("write mode must answer over Bolt");
    assert!(
        !rows.is_empty(),
        "the receipt must carry both stamps; a caller cannot tell how stale a written score \
         is from one of them",
    );
}

#[test]
fn a_read_only_mode_is_unaffected_by_the_autocommit_distinction() {
    // The control. `stream` cannot write, so the server never wraps it and it
    // was reachable throughout — which is exactly why the defect was invisible:
    // the mode everybody tests first is the one mode the wrapper never touched.
    let addr = start();
    let mut c = connect(&addr);
    seed(&mut c);
    let rows = c
        .query(
            "CALL engram.algo.degree.stream({nodeLabels:['N'], relationshipTypes:['R']}) \
             YIELD degree RETURN count(degree)",
        )
        .expect("stream mode");
    assert!(!rows.is_empty());
}

#[test]
fn mutate_and_its_result_surface_are_reachable_over_bolt() {
    // `mutate` is classified `ProcMode::Write` too — it publishes into the
    // result cache — so it takes the same autocommit wrapper and had the same
    // exposure. And a `mutate` whose result cannot be read back is a no-op
    // with a receipt, so the read-back is part of the same claim.
    let addr = start();
    let mut c = connect(&addr);
    seed(&mut c);
    c.query(
        "CALL engram.algo.wcc.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
         mutateKey:'k1'}) YIELD mutateKey RETURN mutateKey",
    )
    .expect("mutate must be reachable over Bolt");
    let back = c
        .query("CALL engram.algo.result.stream({mutateKey:'k1'}) YIELD value RETURN count(value)")
        .expect("the cached result must be readable back");
    assert!(!back.is_empty(), "mutate published nothing readable");
    c.query("CALL engram.algo.result.list() YIELD mutateKey, stale RETURN mutateKey")
        .expect("result.list must be reachable");
    c.query("CALL engram.algo.result.drop({mutateKey:'k1'}) YIELD dropped RETURN dropped")
        .expect("result.drop must be reachable");
}
