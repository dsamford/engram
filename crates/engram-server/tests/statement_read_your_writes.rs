#![allow(non_snake_case)]
#![allow(clippy::disallowed_methods)]
//! A statement sees its own earlier clauses' writes — through every derived
//! structure, over Bolt, where every autocommit statement now runs as a
//! transaction whose writes are BUFFERED until it commits. The label
//! memberships, the adjacency tables, the property indexes and the counts
//! are all committed state; each overlays the transaction's buffered rows so
//! `MERGE` finds the node the previous `UNWIND` iteration created and a
//! `WITH … MATCH` finds what the `CREATE` before it made. And a statement is
//! ATOMIC: one that fails half-way leaves nothing behind.

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

/// The client reports ROW COUNTS; a scalar is read as a count of rows.
fn count(c: &mut Client, q: &str) -> u64 {
    c.run(q).expect(q)
}

#[test]
fn merge_finds_the_node_an_earlier_iteration_of_the_same_statement_created() {
    let addr = start();
    let mut c = connect(&addr);
    count(&mut c, "CREATE INDEX l_id FOR (n:L) ON (n.id)");
    count(&mut c, "UNWIND [1, 1, 1, 2, 2] AS x MERGE (n:L {id: x})");
    assert_eq!(count(&mut c, "MATCH (n:L) RETURN n"), 2, "two distinct ids, not five nodes");
    // Again, in a later statement: MERGE against committed rows still matches.
    count(&mut c, "UNWIND [1, 3] AS x MERGE (n:L {id: x})");
    assert_eq!(count(&mut c, "MATCH (n:L) RETURN n"), 3);
}

#[test]
fn a_later_clause_matches_what_an_earlier_clause_created() {
    let addr = start();
    let mut c = connect(&addr);
    assert_eq!(
        count(&mut c, "CREATE (a:P {id: 1}) WITH a MATCH (m:P) RETURN m"),
        1,
        "the label scan must see the node this statement created"
    );
    assert_eq!(
        count(
            &mut c,
            "CREATE (a:Q {id: 1})-[:R]->(b:Q {id: 2}) WITH a MATCH (a)-[:R]->(x) RETURN x"
        ),
        1,
        "the expansion must see the relationship this statement created"
    );
    assert_eq!(
        count(
            &mut c,
            "CREATE (:C) CREATE (:C) WITH 1 AS one MATCH (c:C) WITH count(c) AS k \
             UNWIND range(1, k) AS i RETURN i"
        ),
        2,
        "the count must include the nodes this statement created"
    );
    assert_eq!(
        count(
            &mut c,
            "MATCH (a:Q {id: 1}) CREATE (a)-[:R]->(:Q {id: 3}) WITH a \
             MATCH (a)-[:R]->(y) RETURN y"
        ),
        2,
        "a node with a committed edge plus a buffered one: both visible"
    );
}

#[test]
fn a_statement_that_fails_half_way_leaves_nothing_behind() {
    let addr = start();
    let mut c = connect(&addr);
    let r = c.run("CREATE (:E {id: 1}) WITH 1 AS x RETURN x / 0");
    assert!(r.is_err(), "division by zero fails the statement");
    // A FAILURE puts the Bolt session into its failed state until a RESET;
    // this minimal client does not send one, so the check uses a fresh
    // connection — the point is the graph, not the session state machine.
    let mut c = connect(&addr);
    assert_eq!(
        count(&mut c, "MATCH (e:E) RETURN e"),
        0,
        "a failed statement's earlier CREATE must not survive — the statement is atomic"
    );
    count(&mut c, "CREATE (:E {id: 2})");
    assert_eq!(count(&mut c, "MATCH (e:E) RETURN e"), 1);
}

#[test]
fn a_delete_earlier_in_the_statement_is_seen_by_a_later_scan() {
    let addr = start();
    let mut c = connect(&addr);
    count(&mut c, "CREATE (:D {id: 1}), (:D {id: 2})");
    assert_eq!(
        count(
            &mut c,
            "MATCH (d:D {id: 1}) DELETE d WITH 1 AS one MATCH (x:D) RETURN x"
        ),
        1,
        "the deleted node must be absent from the later scan"
    );
    assert_eq!(count(&mut c, "MATCH (d:D) RETURN d"), 1);
}

#[test]
fn two_connections_see_each_others_committed_statements_immediately() {
    let addr = start();
    let mut a = connect(&addr);
    let mut b = connect(&addr);
    for i in 0..200 {
        count(&mut a, &format!("CREATE (:V {{i: {i}}})"));
        assert_eq!(
            count(&mut b, &format!("MATCH (v:V {{i: {i}}}) RETURN v")),
            1,
            "acknowledged on A must be visible on B at once (iteration {i})"
        );
    }
}
