//! `cq` — run Cypher statements against a live server and print the rows.
//!
//! The smallest possible Bolt client: `cq <addr> <statement> [statement...]`,
//! one statement per argument, rows printed one per line.
//!
//! Written because the stress harness can only run its own PROFILES, and
//! investigating a harness finding needs the ability to ask the server a
//! question the harness did not think of. The w6 sweep reported
//! "577054 edge(s) but only 586126 bind both endpoints", which is the wrong
//! shape for the defect it names, and settling that needs exactly this: two
//! counts, on a quiescent store, repeatable.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use engram_bolt::client::Client;

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| {
        eprintln!("usage: cq <addr> <statement> [statement...]");
        std::process::exit(2);
    });
    let statements: Vec<String> = args.collect();
    if statements.is_empty() {
        eprintln!("usage: cq <addr> <statement> [statement...]");
        std::process::exit(2);
    }
    let mut c = match Client::connect(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect {addr}: {e}");
            std::process::exit(1);
        }
    };
    let mut failed = false;
    for s in &statements {
        match c.query(s) {
            Ok(rows) => {
                println!("--- {s}");
                for r in rows {
                    println!("{r:?}");
                }
            }
            Err(e) => {
                println!("--- {s}");
                println!("ERROR {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
