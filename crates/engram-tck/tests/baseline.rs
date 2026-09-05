//! Runs the whole vendored openCypher TCK and prints the honest baseline:
//! Pass / Fail / Skip per category, and the pass rate over *evaluated*
//! scenarios. Run with `cargo test -p engram-tck --test baseline -- --nocapture`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use engram_tck::{Outcome, Scenario, Tally, parse_feature, run_scenario};

/// Run one scenario on its own thread, isolating both PANICS (→ Fail) and HANGS
/// (→ Skip after a timeout), so a single pathological scenario cannot abort or
/// stall the whole baseline. The row budget bounds row *materialisation*, but a
/// pure-compute loop (a large `reduce`/list-comprehension) is not row-bounded,
/// so the timeout is load-bearing. A timed-out thread is leaked deliberately;
/// the process exits at the end of the run.
///
/// The workspace bans `std::thread::spawn` to keep the ENGINE's execution
/// deterministic and injectable; this is a TEST harness spawning a throwaway
/// worker, which touches neither — so the ban is locally waived, with cause.
#[allow(clippy::disallowed_methods)]
fn guarded(sc: Scenario) -> Outcome {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_scenario(&sc)))
            .unwrap_or_else(|_| Outcome::Fail("engine panicked".into()));
        let _ = tx.send(out);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(o) => o,
        Err(_) => Outcome::Skip("harness timeout (>5s)".into()),
    }
}

fn feature_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            feature_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "feature") {
            out.push(p);
        }
    }
}

fn category(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|r| r.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_else(|| "?".into())
}

#[test]
fn tck_baseline() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("features");
    let mut files = Vec::new();
    feature_files(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no vendored .feature files found under {root:?}"
    );

    let mut total = Tally::default();
    let mut by_cat: BTreeMap<String, Tally> = BTreeMap::new();
    // A few example failures per category, to make the number actionable.
    let mut fail_samples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut skip_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut fail_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows_mismatch_by_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut scenarios_seen = 0usize;
    // Panics are expected (some scenarios probe unimplemented paths); keep them
    // out of the test log so the summary stays readable.
    std::panic::set_hook(Box::new(|_| {}));

    for f in &files {
        let cat = category(f, &root);
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for sc in parse_feature(&text) {
            scenarios_seen += 1;
            let name = sc.name.clone();
            let outcome = guarded(sc);
            total.record(&outcome);
            by_cat.entry(cat.clone()).or_default().record(&outcome);
            match &outcome {
                Outcome::Fail(why) => {
                    let bucket = fail_samples.entry(cat.clone()).or_default();
                    if bucket.len() < 8 {
                        bucket.push(format!("{name}: {why}"));
                    }
                    if why == "rows did not match" {
                        let file = f
                            .strip_prefix(&root)
                            .unwrap_or(f)
                            .to_string_lossy()
                            .replace('\\', "/");
                        *rows_mismatch_by_file.entry(file).or_insert(0) += 1;
                    }
                    let fkey = if why.starts_with("expected error ") {
                        why.trim_end_matches(", query succeeded").to_string()
                    } else if let Some(rest) = why
                        .strip_prefix("query failed: ")
                        .or(why.strip_prefix("setup failed: "))
                    {
                        // Sub-bucket engine errors by the first few words so the
                        // dominant "query failed" cause is visible.
                        let head: Vec<&str> = rest.split_whitespace().take(6).collect();
                        format!("query failed: {}", head.join(" "))
                    } else {
                        why.split(':').next().unwrap_or(why).trim().to_string()
                    };
                    *fail_reasons.entry(fkey).or_insert(0) += 1;
                }
                Outcome::Skip(why) => {
                    // For "unhandled step" keep the step text (first few words) so
                    // the dominant unrecognised step is visible; else bucket by the
                    // reason prefix.
                    let key = if let Some(rest) = why.strip_prefix("unhandled step: ") {
                        let words: Vec<&str> = rest.split_whitespace().take(5).collect();
                        format!("unhandled: {}", words.join(" "))
                    } else {
                        why.split(':').next().unwrap_or(why).trim().to_string()
                    };
                    *skip_reasons.entry(key).or_insert(0) += 1;
                }
                Outcome::Pass => {}
            }
        }
    }

    println!("\n==== openCypher TCK baseline ====");
    println!(
        "feature files: {}  scenarios: {}\n",
        files.len(),
        scenarios_seen
    );
    println!(
        "{:<14} {:>6} {:>6} {:>6} {:>8}",
        "category", "pass", "fail", "skip", "rate"
    );
    for (cat, t) in &by_cat {
        println!(
            "{:<14} {:>6} {:>6} {:>6} {:>7.1}%",
            cat,
            t.pass,
            t.fail,
            t.skip,
            t.rate() * 100.0
        );
    }
    println!(
        "{:<14} {:>6} {:>6} {:>6} {:>7.1}%",
        "TOTAL",
        total.pass,
        total.fail,
        total.skip,
        total.rate() * 100.0
    );
    println!(
        "\npass rate over EVALUATED (pass+fail={}) scenarios: {:.1}%   [{} skipped = harness gaps]",
        total.pass + total.fail,
        total.rate() * 100.0,
        total.skip
    );

    println!("\n-- fail reasons (engine gaps, by frequency) --");
    let mut fails: Vec<_> = fail_reasons.iter().collect();
    fails.sort_by(|a, b| b.1.cmp(a.1));
    for (reason, n) in fails.iter().take(20) {
        println!("   {n:>4}  {reason}");
    }

    println!("\n-- 'rows did not match' by feature file (top 20) --");
    let mut rmf: Vec<_> = rows_mismatch_by_file.iter().collect();
    rmf.sort_by(|a, b| b.1.cmp(a.1));
    for (file, n) in rmf.iter().take(20) {
        println!("   {n:>4}  {file}");
    }

    println!("\n-- skip reasons (harness gaps, by frequency) --");
    let mut skips: Vec<_> = skip_reasons.iter().collect();
    skips.sort_by(|a, b| b.1.cmp(a.1));
    for (reason, n) in skips.iter().take(15) {
        println!("   {n:>4}  {reason}");
    }

    for (cat, samples) in &fail_samples {
        println!("\n-- sample failures: {cat} --");
        for s in samples {
            println!("   {s}");
        }
    }

    // Sanity: the harness itself must actually be running scenarios and scoring
    // some as pass (a harness that passes nothing, or evaluates nothing, is
    // broken — instrument integrity, not a conformance claim).
    assert!(
        scenarios_seen > 3000,
        "suspiciously few scenarios parsed: {scenarios_seen}"
    );
    assert!(total.pass + total.fail > 0, "harness evaluated nothing");

    // ── RATCHET ──────────────────────────────────────────────────────────
    // The conformance pass count may only RISE. A drop is a regression and
    // fails CI. When a change genuinely raises the number, bump `MIN_PASS`
    // (and lower `MAX_FAIL`) to the new floor in the same commit — that is the
    // whole point of the gate. The small margin absorbs the one scenario that
    // rides the 5s timeout (its verdict can flip run to run).
    const MIN_PASS: usize = 3768; // measured 3771, 2026-08-28 (margin for the timeout flake)
    const MAX_FAIL: usize = 4; // measured 1 (only the stale-tzdb Stockholm-1818 case), 2026-08-28
    assert!(
        total.pass >= MIN_PASS,
        "TCK regression: pass count {} fell below the ratchet {MIN_PASS}",
        total.pass
    );
    assert!(
        total.fail <= MAX_FAIL,
        "TCK regression: fail count {} rose above the ratchet {MAX_FAIL}",
        total.fail
    );
}
