//! `qcompare <addr A> <addr B> [--dataset snb]` — do two Bolt servers give the
//! same answers?
//!
//! A performance comparison between two databases is worth nothing until they
//! agree on the results. "Faster" is not a property a query has on its own; it
//! is a property of a query that returns the right rows, and an engine that
//! returns fewer of them is faster for an uninteresting reason.
//!
//! This runs the SAME statements the stress harness measures against both
//! endpoints and compares what comes back. It compares ROW COUNTS rather than
//! row contents, because the Bolt client here reports counts — a deliberate
//! limit, stated plainly:
//!
//! - It **will** catch a missing edge, an index that misses rows, a traversal
//!   that stops early, a label that did not load, an aggregate over the wrong
//!   set — every structural divergence that would flatter one engine's timings.
//! - It **will not** catch two engines returning the same NUMBER of wrong rows,
//!   or the same rows with different property values.
//!
//! For the second class the openCypher TCK is the instrument, and it is run
//! separately. This one exists to make sure the two corpora and the two
//! traversals are the same shape before anyone quotes a ratio.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use engram_bolt::client::Client;

/// The statements to compare: every read shape the SNB stress mix issues, at
/// several parameter values, plus census queries over the whole corpus.
///
/// Parameterised by hand rather than generated, so the list is readable and a
/// failing entry names itself.
fn statements() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    // Census — these catch a corpus that did not fully load, which is the
    // failure that would silently make one engine look fast.
    for q in [
        "MATCH (n) RETURN count(n) AS c",
        "MATCH ()-[r]->() RETURN count(r) AS c",
        "MATCH (p:Person) RETURN count(p) AS c",
        "MATCH (m:Message) RETURN count(m) AS c",
        "MATCH (m:Comment) RETURN count(m) AS c",
        "MATCH (m:Post) RETURN count(m) AS c",
        "MATCH (f:Forum) RETURN count(f) AS c",
        "MATCH (t:Tag) RETURN count(t) AS c",
        "MATCH (:Person)-[:KNOWS]-(:Person) RETURN count(*) AS c",
        "MATCH (:Message)-[:HAS_CREATOR]->(:Person) RETURN count(*) AS c",
        "MATCH (:Message)-[:HAS_TAG]->(:Tag) RETURN count(*) AS c",
        "MATCH (:Comment)-[:REPLY_OF]->(:Message) RETURN count(*) AS c",
        "MATCH (:Person)-[:IS_LOCATED_IN]->(:City) RETURN count(*) AS c",
    ] {
        v.push(q.to_string());
    }
    // The measured read shapes, at a spread of parameters.
    for k in [0u64, 1, 7, 42, 137, 999, 1500] {
        v.push(format!(
            "MATCH (p:Person {{id: {k}}}) RETURN p.firstName, p.lastName, p.birthday"
        ));
        v.push(format!(
            "MATCH (p:Person {{id: {k}}})-[:KNOWS]-(f:Person) RETURN f.id, f.firstName LIMIT 25"
        ));
        v.push(format!(
            "MATCH (p:Person {{id: {k}}})-[:KNOWS]-()-[:KNOWS]-(f:Person) RETURN count(DISTINCT f) AS c"
        ));
        v.push(format!(
            "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {{id: {k}}}) RETURN m.id LIMIT 25"
        ));
        v.push(format!(
            "MATCH (p:Person {{id: {k}}})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT 25"
        ));
        v.push(format!(
            "MATCH (p:Person {{id: {k}}})-[:KNOWS*1..2]-(f:Person) RETURN count(DISTINCT f) AS c"
        ));
        v.push(format!(
            "MATCH (p:Person {{id: {k}}})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
             MATCH (m)-[:HAS_TAG]->(t:Tag) RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10"
        ));
    }
    v.push(
        "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) RETURN c.name, count(p) AS n \
         ORDER BY n DESC LIMIT 10"
            .to_string(),
    );
    v
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: qcompare <addr A> <addr B>");
        eprintln!("  runs the SNB read shapes against both and compares row counts.");
        std::process::exit(2);
    }
    let (a_addr, b_addr) = (args[1].clone(), args[2].clone());

    let mut a = match Client::connect(&a_addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[qcompare] cannot reach A ({a_addr}): {e}");
            std::process::exit(1);
        }
    };
    let mut b = match Client::connect(&b_addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[qcompare] cannot reach B ({b_addr}): {e}");
            std::process::exit(1);
        }
    };

    let stmts = statements();
    let mut agreed = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for s in &stmts {
        let ra = a.run(s);
        let rb = b.run(s);
        match (ra, rb) {
            (Ok(x), Ok(y)) if x == y => agreed += 1,
            (Ok(x), Ok(y)) => diffs.push(format!("A={x:>8} B={y:>8}  {s}")),
            // An error on ONE side is a divergence too, and the more serious
            // kind: one engine cannot answer a statement the other can.
            (Err(e), Ok(y)) => errors.push(format!("A failed ({e}), B={y}  {s}")),
            (Ok(x), Err(e)) => errors.push(format!("A={x}, B failed ({e})  {s}")),
            (Err(ea), Err(eb)) => errors.push(format!("both failed (A: {ea}; B: {eb})  {s}")),
        }
    }

    println!(
        "\nqcompare: {} statement(s); A={a_addr} B={b_addr}",
        stmts.len()
    );
    println!("  agreed:      {agreed}");
    println!("  disagreed:   {}", diffs.len());
    println!("  errored:     {}", errors.len());
    for d in diffs.iter().take(40) {
        println!("    DIFF  {d}");
    }
    for e in errors.iter().take(40) {
        println!("    ERR   {e}");
    }

    // A run where NOTHING was compared passes every equality it never made.
    if agreed == 0 && diffs.is_empty() && errors.is_empty() {
        println!("\nFAIL — no statement was compared at all");
        std::process::exit(1);
    }
    if diffs.is_empty() && errors.is_empty() {
        println!("\nPASS — both servers returned identical row counts for every statement");
    } else {
        println!("\nFAIL — the two servers do not agree; any timing comparison between them is void");
        std::process::exit(1);
    }
}
