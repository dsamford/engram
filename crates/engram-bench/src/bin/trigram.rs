#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! What does a trigram index save a text filter, and what does it cost to keep?
//!
//! `=~`, `CONTAINS` and `ENDS WITH` were full label scans: every value read and
//! re-examined per statement. A trigram index turns each into a seek over a
//! candidate set the predicate then re-verifies. The question this bin answers
//! is whether that trade is worth it at a realistic corpus size, and where it
//! stops being worth it — because the planner compares the candidate count
//! against the label's size and is supposed to decline when the scan is
//! cheaper.
//!
//! Two costs are measured, not one. An index that halves a read and doubles a
//! write has not obviously helped, and the write side of an inverted index is
//! the half people forget: one entry per distinct three-character window per
//! value, maintained on every write to the indexed property.
//!
//! | stage | what it times |
//! |---|---|
//! | `build` | first probe on a cold index — the O(label) scan and sort |
//! | `regex-on` / `regex-off` | `WHERE c.body =~ '.*needle.*'` |
//! | `contains-on` / `contains-off` | `WHERE c.body CONTAINS 'needle'` — no index path existed at all before |
//! | `endswith-on` / `endswith-off` | `WHERE c.body ENDS WITH '.rs'` — a range index cannot answer this at any price |
//! | `rare` / `common` | a needle in ~1% of rows against one in ~50% — where the seek should stop winning |
//! | `write-indexed` / `write-unindexed` | the same writes with the index warm and with the lever off |
//!
//! THE PREDICTION, recorded before the run (house rule: predict, then measure).
//! A seek reads `candidates` records where the scan reads `rows`, so the read
//! saving should track the selectivity and NOT the corpus size: a 1% needle
//! should be roughly two orders faster, a 50% needle roughly break even or
//! lose, and the planner should decline the 50% case rather than lose it. The
//! write cost should be a small constant multiple — the log entry already
//! exists for the range index, so the extra work is deriving trigrams from a
//! value already in hand, not a store read.
//!
//! WHAT IT MEASURED, and what the prediction got wrong. Median of three at
//! rows=20000, iters=20:
//!
//! ```text
//!   stage                     on         off      speedup   rows
//!   regex               0.241 ms    7.950 ms     33.9x       200
//!   contains            0.216 ms    0.841 ms      3.9x       200
//!   endswith            0.610 ms    0.595 ms      1.0x      3029
//!   common              1.123 ms    1.077 ms      1.0x     20000
//!   write-indexed       2.59 ms per 100 SETs   (2.3x unindexed)
//! ```
//!
//! The selectivity prediction held. What it MISSED was that the first two
//! numbers were 0.9x and 2.5x until this bin existed, for two reasons neither
//! a correctness test nor a reading of the code would have surfaced:
//!
//! 1. The probe MATERIALISED its candidates and only then let the planner
//!    reject them for being no smaller than the label - 2,005 store reads
//!    spent to conclude that the scan was better. The cap is now derived from
//!    the label size (`Graph::text_seek_cap`), so that refusal costs two
//!    binary searches. `endswith` and `common` moved from 0.4x to 1.0x.
//! 2. `.*zqx.*` derived NO trigram condition at all. The trailing `.*` unions
//!    an empty fragment into the suffix set, and an empty member makes the
//!    requirement collapse to match-all - so the index declined a pattern it
//!    could answer from one posting list. Safe, useless, and invisible: the
//!    differential tests passed throughout, because declining is correct.
//!    `regex` moved from 0.9x to 34x.
//!
//! Both were "correct but slow", which is precisely the class a differential
//! test cannot see and a benchmark can.
//!
//! THE THIRD, found later, and the reason the `interleaved` stage exists.
//! Neither stage above can see it: one reads a static corpus, the other writes
//! without reading, and the overlay fold happens on whichever thread next
//! finds the index stale. Alternating them, 5,000 write/read pairs:
//!
//!   fold threshold    median      p90       p99        max      wall
//!   4,096 (shipped)  0.900 ms  1.268 ms  1.681 ms  238.8 ms   8.15 s
//!   16,384           1.628 ms  2.283 ms  2.811 ms  256.4 ms  10.30 s
//!   65,536           5.157 ms  6.144 ms  7.688 ms   15.8 ms  23.16 s
//!   base/8           4.943 ms  5.707 ms  6.884 ms   11.3 ms  21.55 s
//!
//! A 265x tail on the shipped row, and 239 ms is what a cold build costs,
//! because that is what it is: `FOLD_AT` came from the range index, where one
//! write contributes one overlay entry, while a trigram write contributes one
//! per distinct trigram.
//!
//! AND THE FIX FOR IT WAS WRONG, which this bin is the only reason anyone
//! knows. Folding at a fraction of the base is the textbook amortisation, and
//! at `iters=5` — 500 pairs — it measured max 236 ms -> 3.7 ms and a third off
//! the wall time. Shipping-grade evidence, and backwards. At 5,000 pairs the
//! same change is a 5.5x median regression and 2.6x the wall time, because a
//! catch-up deep-copies the overlay on every stale read; and the maxima that
//! looked like a fix were runs in which no fold occurred at all.
//!
//! **A benchmark shorter than the period of the event it is measuring reports
//! that event's absence as its cure.** The 500-pair run was not weak evidence
//! for the fraction — it was evidence against it, read backwards. Hence
//! `iters=50` as the figure to quote from, and the per-quarter trend line,
//! which is what shows a cost that arrives with the overlay rather than with
//! the corpus.
//!
//! AND THE FIX THAT DID WORK, found by asking where the fold's time actually
//! went rather than how to run it less often. Two changes, neither touching
//! the threshold: the fold MERGES its two already-ordered inputs instead of
//! re-sorting their concatenation, and a document's store key is shared by
//! all of its entries behind an `Arc`, so rebuilding the base is a refcount
//! bump per entry rather than a heap allocation.
//!
//!                    median      p99        max      wall
//!   before          1.35-1.56  2.8-7.6  233-240 ms  10.3-12.6 s
//!   after           1.03-1.42  2.4-3.7   60-84  ms   7.4-9.5  s
//!
//! Better on EVERY axis. That is the tell, and it is worth internalising
//! alongside the failure above: a change that improves one metric and worsens
//! another is a trade, and needs a judgement about which matters. A change
//! that improves all of them means work was simply being wasted, and needs no
//! judgement at all. The threshold change was the first kind and I mistook it
//! for the second, on a run too short to show the cost side.
//!
//! ```text
//! trigram [rows=20000] [iters=20]
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn main() {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(20_000);
    let iters: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(20);

    println!("trigram [rows={rows}] [iters={iters}]");
    println!();

    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    load(&g, rows);

    // The index is declared but not built: the first probe pays for it.
    ddl(
        &g,
        "CREATE TRIGRAM INDEX chunk_body FOR (c:Chunk) ON (c.body)",
    );

    let t = Instant::now();
    let warm = rows_of(&g, RARE);
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("  build            {build_ms:8.2} ms   (first probe, cold index; {warm} rows)");
    println!();

    println!("  stage                     on         off      speedup   rows");
    println!("  ------------------------------------------------------------");
    for (name, src) in [
        ("regex", REGEX),
        ("contains", RARE),
        ("endswith", ENDSWITH),
        ("common", COMMON),
    ] {
        let (on, off, n) = ab(&g, src, iters);
        let speedup = if on > 0.0 { off / on } else { f64::NAN };
        println!("  {name:<16} {on:8.3} ms {off:8.3} ms   {speedup:6.1}x  {n:6}");
    }
    println!();

    // The write side. Same statements, index warm versus lever off.
    let w_on = write_cost(&g, true, iters);
    let w_off = write_cost(&g, false, iters);
    println!("  write-indexed    {w_on:8.3} ms per 100 SETs");
    println!("  write-unindexed  {w_off:8.3} ms per 100 SETs");
    if w_off > 0.0 {
        println!("  write overhead   {:8.2}x", w_on / w_off);
    }
    println!();

    // ── The interleaved stage ───────────────────────────────────────────
    // Reads and writes ALTERNATING, which is the only shape that can show
    // what a reader pays for the writes that preceded it. The two stages
    // above cannot: one reads a static corpus, the other writes without
    // reading, and the overlay fold happens on whichever thread next finds
    // the index stale.
    interleaved(&g, rows, iters);

    println!("  A speedup at or below 1.0 on `common` is the CORRECT outcome only if the");
    println!("  planner declined the seek. Check `interp.columnar seek probed a declared");
    println!("  trigram index` in the trace if it did not.");
}

const RARE: &str = "MATCH (c:Chunk) WHERE c.body CONTAINS 'zqx' RETURN count(c)";
const COMMON: &str = "MATCH (c:Chunk) WHERE c.body CONTAINS 'the' RETURN count(c)";
const REGEX: &str = "MATCH (c:Chunk) WHERE c.body =~ '.*zqx.*' RETURN count(c)";
const ENDSWITH: &str = "MATCH (c:Chunk) WHERE c.body ENDS WITH '.rs' RETURN count(c)";

/// A corpus shaped like source text: most rows share common words, a few carry
/// a rare marker, and a slice ends in a suffix no sort order can seek.
fn load(g: &Graph, rows: usize) {
    for i in 0..rows {
        let body = if i % 100 == 0 {
            format!("the quick zqx marker line {i} in file{i}.rs")
        } else if i % 7 == 0 {
            format!("the ordinary line {i} in file{i}.rs")
        } else {
            format!("the ordinary line {i} with no marker at all")
        };
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i as i64));
        m.insert("body".to_string(), Value::Str(body));
        g.create_node(&["Chunk".into()], &m).expect("chunk");
    }
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn rows_of(g: &Graph, src: &str) -> i64 {
    let q = parse_statement(src).expect("parse");
    let r = run_query(g, &q, BTreeMap::new()).expect("run");
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    }
}

/// The A/B: the same statement with the index on and with the lever off. The
/// row counts are compared, because a faster answer that is a different answer
/// is not a result.
fn ab(g: &Graph, src: &str, iters: usize) -> (f64, f64, i64) {
    g.set_trigram_indexes(true);
    let n_on = rows_of(g, src); // warm
    let t = Instant::now();
    for _ in 0..iters {
        rows_of(g, src);
    }
    let on = t.elapsed().as_secs_f64() * 1e3 / iters as f64;

    g.set_trigram_indexes(false);
    let n_off = rows_of(g, src);
    let t = Instant::now();
    for _ in 0..iters {
        rows_of(g, src);
    }
    let off = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
    g.set_trigram_indexes(true);

    assert_eq!(
        n_on, n_off,
        "the index and the scan disagreed on `{src}` — the measurement is meaningless"
    );
    (on, off, n_on)
}

/// A hundred property writes, timed, with the index maintained or not.
/// Read latency under a write stream, and how often a reader pays a FOLD.
///
/// The question, which the static stages cannot answer: a reader that finds
/// the index stale catches it up on its own thread, and past `FOLD_AT`
/// accumulated overlay pairs that catch-up rebuilds the base — a clone, a
/// sort and a dedup of every entry, on the query thread.
///
/// The arithmetic that makes this worth measuring rather than assuming: one
/// rewritten body of ~100 characters contributes ~100 overlay PAIRS, not one,
/// so `FOLD_AT` of 4,096 is roughly forty writes. If the fold is as expensive
/// as its shape suggests, a mixed workload should show a read-latency tail far
/// above the median, arriving about that often.
fn interleaved(g: &Graph, rows: usize, iters: usize) {
    g.set_trigram_indexes(true);
    rows_of(g, RARE);

    let mut reads: Vec<f64> = Vec::new();
    let writes = iters * 100;
    let t_all = Instant::now();
    for i in 0..writes {
        let src = format!(
            "MATCH (c:Chunk {{id: {}}}) SET c.body = 'interleaved body {} pass {}'",
            i % rows.min(2_000),
            i,
            i / 100
        );
        let q = parse_statement(&src).expect("parse");
        run_query(g, &q, BTreeMap::new()).expect("write");

        let t = Instant::now();
        let _ = rows_of(g, RARE);
        reads.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let wall = t_all.elapsed().as_secs_f64() * 1e3;
    let in_order = reads.clone();

    // THE QUARTILE TREND, which is the question a single median cannot
    // answer: a catch-up begins with `self.clone()`, so the overlay is copied
    // on every stale read. A proportional fold threshold lets the overlay
    // grow, and if that copy is expensive the READ MEDIAN MUST RISE ACROSS
    // THE RUN as the overlay fills toward the threshold. A flat trend says
    // the amortisation was free; a rising one says the fold cost was moved
    // onto the readers rather than removed.
    let q = reads.len() / 4;
    if q > 0 {
        let med = |sl: &[f64]| {
            let mut v = sl.to_vec();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let quarters: Vec<f64> = (0..4).map(|i| med(&in_order[i * q..(i + 1) * q])).collect();
        println!(
            "    read median by quarter of the run: {:.3} → {:.3} → {:.3} → {:.3} ms  (drift {:.2}x)",
            quarters[0],
            quarters[1],
            quarters[2],
            quarters[3],
            if quarters[0] > 0.0 {
                quarters[3] / quarters[0]
            } else {
                f64::NAN
            },
        );
    }

    reads.sort_by(f64::total_cmp);
    let at = |q: f64| reads[((reads.len() as f64 - 1.0) * q) as usize];
    println!("  interleaved read latency over {writes} write/read pairs");
    println!(
        "    median {:7.3} ms   p90 {:7.3} ms   p99 {:7.3} ms   max {:7.3} ms   wall {:8.1} ms",
        at(0.5),
        at(0.9),
        at(0.99),
        reads[reads.len() - 1],
        wall,
    );
    println!(
        "    tail ratio max/median {:6.1}x   — a reader paying a base rebuild shows up HERE",
        if at(0.5) > 0.0 {
            reads[reads.len() - 1] / at(0.5)
        } else {
            f64::NAN
        }
    );
    println!();
}

fn write_cost(g: &Graph, indexed: bool, iters: usize) -> f64 {
    g.set_trigram_indexes(indexed);
    // Warm the index so the cost measured is MAINTENANCE, not a build.
    rows_of(g, RARE);
    let t = Instant::now();
    for round in 0..iters {
        for i in 0..100 {
            let src = format!(
                "MATCH (c:Chunk {{id: {}}}) SET c.body = 'rewritten body {} round {}'",
                i, i, round
            );
            let q = parse_statement(&src).expect("parse");
            run_query(g, &q, BTreeMap::new()).expect("write");
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
    g.set_trigram_indexes(true);
    ms
}
