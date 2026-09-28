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
        eprintln!("usage: cq <addr> [--param k=v]... <statement> [statement...]");
        std::process::exit(2);
    });
    // `--param k=v` before or between statements. A value that parses as an
    // integer is sent as one: FinBench's `truncationLimit` is compared as a
    // number, and a string "100" would silently fail to match rather than
    // refuse, which is the failure this tool exists to avoid.
    //
    // `--param k:TYPE=v` binds `v` as the harness binds a catalogue parameter
    // of that type (`engram_bench::params::coerce`): `date:DATETIME=2011-05-
    // 09T06:21:15.141Z` arrives a DATETIME, as the benchmark sends it. A
    // decomposition probe that wrote the date as a `datetime('…')` literal
    // instead measured SNB BI bi4 at 27 s against the harness's 13 s.
    //
    // `--repeat N` runs each statement N times on the one connection and
    // prints each run's client-side milliseconds; the rows are printed for the
    // first run only. A decomposition of a 40 ms statement cannot be timed
    // from a shell: the bench pod's clock is `/proc/uptime`, 10 ms a tick.
    let mut statements: Vec<String> = Vec::new();
    let mut params: std::collections::BTreeMap<String, engram_cypher::Value> = Default::default();
    let mut repeat: usize = 1;
    let mut rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--repeat" {
            match rest.get(i + 1).map(|n| n.parse::<usize>()) {
                Some(Ok(n)) if n >= 1 => repeat = n,
                _ => {
                    eprintln!("cq: --repeat takes a count of at least 1");
                    std::process::exit(2);
                }
            }
            i += 2;
        } else if rest[i] == "--param" {
            let Some(kv) = rest.get(i + 1) else {
                eprintln!("cq: --param needs key=value");
                std::process::exit(2);
            };
            let Some((k, v)) = kv.split_once('=') else {
                eprintln!("cq: --param takes key=value, got `{kv}`");
                std::process::exit(2);
            };
            let (k, val) = match k.split_once(':') {
                Some((name, ty)) => match engram_bench::params::coerce(v, ty) {
                    Ok(val) => (name, val),
                    Err(e) => {
                        eprintln!("cq: --param {kv}: {e}");
                        std::process::exit(2);
                    }
                },
                None => (
                    k,
                    match v.parse::<i64>() {
                        Ok(n) => engram_cypher::Value::Int(n),
                        Err(_) => engram_cypher::Value::Str(v.to_string()),
                    },
                ),
            };
            params.insert(k.to_string(), val);
            i += 2;
        } else {
            statements.push(std::mem::take(&mut rest[i]));
            i += 1;
        }
    }
    if statements.is_empty() {
        eprintln!("usage: cq <addr> [--param k=v]... [--repeat N] <statement> [statement...]");
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
        println!("--- {s}");
        for run in 0..repeat {
            let started = std::time::Instant::now();
            let got = c.query_with(s, params.clone());
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match got {
                Ok(rows) => {
                    if repeat > 1 {
                        println!("[cq] run {} {ms:.2} ms {} row(s)", run + 1, rows.len());
                    }
                    if run == 0 {
                        for r in rows {
                            println!("{r:?}");
                        }
                    }
                }
                Err(e) => {
                    println!("ERROR {e}");
                    failed = true;
                    break;
                }
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
