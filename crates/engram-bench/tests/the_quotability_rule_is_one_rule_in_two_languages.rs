//! The refusal rule now lives in TWO implementations, held to each other.
//!
//! # Why a second copy exists at all
//!
//! `docs/bench/ladybug-conc.py` is the LadybugDB arm. LadybugDB is EMBEDDED — a
//! Python library in a pod with no harness binary on it — so that executor has
//! to decide quotability itself, out of process, with nothing to ask. It
//! therefore transcribes the thresholds and the cause taxonomy from
//! [`engram_bench::report`], and a transcription is exactly the shape of thing
//! this project keeps deleting.
//!
//! # What a drift would do, and why "it fails closed" is not enough
//!
//! It does NOT fail closed. A rig drift at least surfaces as a refusal —
//! `compare` names both lanes and stops. A THRESHOLD drift surfaces as nothing
//! at all: every column still prints a number, every row still says
//! `quotable: true`, and the only difference is that one engine was held to a
//! rule the others were not. That is the failure mode this whole file exists
//! for, and it is silent by construction.
//!
//! The 2026-09-09 dry run is the precedent. Rule 0 — the plan-exhaustion
//! refusal — was added on the Rust side and nothing carried it across, so the
//! engram and PostgreSQL arms refused every level while LadybugDB, replaying
//! the SAME plan file, reported `quotable: true` and held a number in a cell
//! where the other two declined to quote one. Nobody had to make a mistake for
//! that to happen; one of two copies simply moved.
//!
//! So the Python is read as TEXT, at run time, because that is the artefact
//! that ships to the pod: a test that re-derived the values some other way
//! would pass while the file that runs disagrees.

use std::path::Path;

use engram_bench::report::{
    FLOOR_STALL, LevelResult, MIN_JUDGED_BUCKETS, NOT_QUOTABLE_CAUSES, NotQuotable,
    REFUSAL_SHARE_MAX, TREND_COLLAPSE, TREND_WARMUP_REFUSE, TREND_WARMUP_WARN,
};

/// The LadybugDB executor's source, or `None` in a PUBLISHED SNAPSHOT.
///
/// `docs/bench/` is the engineering record and is not published, so a snapshot
/// cut by `cargo xtask public-tree` does not carry this script. A snapshot is
/// recognised the way the scrub gate recognises one: no `scrub-rules.txt` at
/// the workspace root. In the SOURCE tree a missing script still fails, so the
/// agreement this file checks cannot quietly stop being checked there.
fn executor_source_or_skip() -> Option<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("docs/bench/ladybug-conc.py");
    match std::fs::read_to_string(&path) {
        Ok(src) => Some(src),
        Err(_) if !root.join("scrub-rules.txt").is_file() => {
            println!(
                "skipping: no docs/bench/ladybug-conc.py -- this is a published snapshot, \
                 which does not carry the engineering record"
            );
            None
        }
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

/// Pull a module-level `NAME = <number>` out of the executor's source.
///
/// Deliberately a small hand parser over the literal shape the file uses — an
/// assignment at column 0 — rather than anything general: the point is to read
/// the bytes that ship, and a parser that accepted more shapes than the file
/// writes would let a move past by matching something else. A name that is not
/// found at column 0 panics rather than defaulting, because the whole failure
/// here is a check that silently matches nothing.
fn python_number(src: &str, name: &str) -> f64 {
    let needle = format!("\n{name} = ");
    let at = src
        .find(&needle)
        .unwrap_or_else(|| panic!("the executor must declare `{name}` at column 0"));
    let rest = &src[at + needle.len()..];
    let stop = rest.find('\n').unwrap_or(rest.len());
    let text = rest[..stop].split('#').next().unwrap_or("").trim();
    text.parse()
        .unwrap_or_else(|e| panic!("`{name} = {text}` is not a number: {e}"))
}

/// Pull `NOT_QUOTABLE_CAUSES = { … }` out of the executor's source, as
/// `(code, class)` pairs in the order written.
fn python_causes(src: &str) -> Vec<(String, String)> {
    let start = src
        .find("NOT_QUOTABLE_CAUSES = {")
        .expect("the executor must declare NOT_QUOTABLE_CAUSES");
    let body = &src[start..];
    let end = body
        .find("\n}\n")
        .expect("NOT_QUOTABLE_CAUSES must close at column 0");
    let mut out = Vec::new();
    for line in body[..end].lines().skip(1) {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        let mut parts = line.split(':');
        let code = parts
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        let class = parts
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches(',')
            .trim()
            .trim_matches('"')
            .to_string();
        out.push((code, class));
    }
    out
}

#[test]
fn the_python_executor_and_the_reporter_agree_on_every_threshold() {
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    let pairs: [(&str, f64); 6] = [
        ("MIN_JUDGED_BUCKETS", MIN_JUDGED_BUCKETS as f64),
        ("TREND_COLLAPSE", TREND_COLLAPSE),
        ("TREND_WARMUP_WARN", TREND_WARMUP_WARN),
        ("TREND_WARMUP_REFUSE", TREND_WARMUP_REFUSE),
        ("FLOOR_STALL", FLOOR_STALL),
        ("REFUSAL_SHARE_MAX", REFUSAL_SHARE_MAX),
    ];
    for (name, rs) in pairs {
        let py = python_number(&src, name);
        assert!(
            (py - rs).abs() < f64::EPSILON,
            "docs/bench/ladybug-conc.py's `{name}` is {py} and report.rs's is {rs}. A \
             threshold drift does NOT fail closed: every column still prints a number and \
             every row still says `quotable: true`, and the only difference is that one \
             engine was held to a rule the others were not. Change both in one edit."
        );
    }
    // The bands have to be ORDERED, or the warning band is empty and the
    // warm-up refusal has no run-up: a warn edge above the refuse edge would
    // make `warm_up_warning` unreachable and the check would look present while
    // being dead.
    const _: () = assert!(
        TREND_COLLAPSE < 1.0 && 1.0 < TREND_WARMUP_WARN && TREND_WARMUP_WARN < TREND_WARMUP_REFUSE,
        "the bands must read collapse < steady < warn < refuse"
    );
}

#[test]
fn the_python_executor_and_the_reporter_agree_on_the_cause_taxonomy() {
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    let py = python_causes(&src);
    assert!(
        py.len() >= NOT_QUOTABLE_CAUSES.len(),
        "the parser found {} cause(s) in the Python and the reporter declares {} — a \
         parser that finds nothing passes every comparison",
        py.len(),
        NOT_QUOTABLE_CAUSES.len()
    );
    let mut rs: Vec<(String, String)> = NOT_QUOTABLE_CAUSES
        .iter()
        .map(|(c, k)| ((*c).to_string(), (*k).to_string()))
        .collect();
    let mut py_sorted = py.clone();
    rs.sort();
    py_sorted.sort();
    assert_eq!(
        py_sorted, rs,
        "the cause taxonomy has drifted. `not_quotable_class` is what a sweep is triaged \
         by — `operator_error` means nothing was measured and `finding` means something \
         was caught — so a code that carries one kind in one language and the other kind \
         in the other makes a sweep of nothing read exactly like a sweep of something."
    );
}

#[test]
fn every_rust_refusal_declares_its_kind_in_the_shared_table() {
    // The table is the shared vocabulary, so a variant added to the enum and
    // NOT added to the table would be invisible to the Python and to any
    // consumer filtering by class. One of each variant, by construction, so
    // adding a variant without a table entry fails here rather than in a sweep.
    let all = [
        NotQuotable::PlanExhausted {
            clients: 1,
            had: None,
            sufficient: None,
            drained_after_s: None,
            seconds: 20.0,
        },
        NotQuotable::NoOperations {
            refusals: 1,
            errors: 0,
        },
        NotQuotable::AllWritesRefused { refusals: 1 },
        NotQuotable::Stalled {
            max_us: 1,
            seconds: 1.0,
        },
        NotQuotable::RefusalDominated {
            refusals: 9,
            attempts: 10,
            kinds: String::new(),
        },
        NotQuotable::NoConcurrency { clients: 8 },
        NotQuotable::TooShortToJudge {
            buckets: 3,
            seconds: 3.0,
        },
        NotQuotable::WarmUpRamp {
            trend: 1.65,
            buckets: 20,
        },
    ];
    for c in &all {
        let declared = NOT_QUOTABLE_CAUSES
            .iter()
            .find(|(code, _)| *code == c.code())
            .unwrap_or_else(|| {
                panic!(
                    "`{}` is a refusal the reporter can produce and NOT_QUOTABLE_CAUSES does \
                     not declare it — so the Python cannot classify it and neither can a \
                     sweep",
                    c.code()
                )
            });
        assert_eq!(
            declared.1,
            c.class(),
            "`{}` is classified `{}` by the enum and `{}` by the shared table",
            c.code(),
            c.class(),
            declared.1
        );
    }
}

#[test]
fn the_comparison_would_actually_notice_a_drift() {
    // THE GUARD OBSERVED FAILING. A check that has never been seen to fail is
    // not known to be a check — and these are hand parsers over text, which is
    // exactly the kind that can quietly match nothing and pass. So the same
    // parsers are fed a source whose only difference is one threshold and one
    // class word, and both must come back changed.
    let Some(src) = executor_source_or_skip() else {
        return;
    };

    let drifted = src.replace("\nTREND_WARMUP_REFUSE = 1.5", "\nTREND_WARMUP_REFUSE = 2.0");
    assert_ne!(drifted, src, "the mutation must land in the text");
    assert!(
        (python_number(&drifted, "TREND_WARMUP_REFUSE") - 2.0).abs() < f64::EPSILON,
        "a one-number drift must change what the parser returns, or this test is comparing \
         something that is not the threshold table"
    );
    assert!(
        (python_number(&src, "TREND_WARMUP_REFUSE") - TREND_WARMUP_REFUSE).abs() < f64::EPSILON,
        "and the real file must still agree"
    );

    let reclassified = src.replace(
        "\"too_short_to_judge\": \"operator_error\",",
        "\"too_short_to_judge\": \"finding\",",
    );
    assert_ne!(reclassified, src, "the mutation must land in the text");
    let py = python_causes(&reclassified);
    assert!(
        py.contains(&("too_short_to_judge".to_string(), "finding".to_string())),
        "a one-word reclassification must change what the parser returns"
    );
    assert!(
        !python_causes(&src).contains(&("too_short_to_judge".to_string(), "finding".to_string())),
        "and the real file must still say operator_error"
    );
}

#[test]
fn the_python_still_calls_the_rules_it_declares() {
    // The thresholds could agree perfectly and be unread. Both new rules and
    // the two incumbent shape rules have to appear in the executor's decision
    // code, not merely in its constants — a constant nothing branches on is a
    // rule that was transcribed and then not applied, which is a drift the
    // number comparison above cannot see.
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    for needle in [
        "if len(res[\"per_sec\"]) < MIN_JUDGED_BUCKETS:",
        "if res[\"trend\"] > TREND_WARMUP_REFUSE:",
        "if share > REFUSAL_SHARE_MAX:",
        "if len(per_sec) < MIN_JUDGED_BUCKETS:",
        "\"not_quotable_cause\": cause,",
        "\"not_quotable_class\": cause_class(cause) if cause else None,",
    ] {
        assert!(
            src.contains(needle),
            "the executor declares the rule but its decision code has no `{needle}` — a \
             threshold nobody branches on is a rule that was transcribed and not applied"
        );
    }
}

#[test]
fn a_short_level_and_a_ramping_level_are_both_refused_on_the_rust_side() {
    // The two Rust rules the Python mirrors, exercised here so this file fails
    // if either side alone is edited away. Their own detailed tests live beside
    // the rules in `report.rs`; these are the parity anchors.
    let short = short_level(vec![100, 100, 100]);
    assert_eq!(
        short.not_quotable(1_000).map(|c| c.code().to_string()),
        Some("too_short_to_judge".to_string())
    );
    let ramp = short_level(vec![100, 100, 100, 100, 165, 165, 165, 165]);
    assert_eq!(
        ramp.not_quotable(1_000).map(|c| c.code().to_string()),
        Some("warmup_ramp".to_string())
    );
}

fn short_level(per_sec: Vec<u64>) -> LevelResult {
    LevelResult {
        profile: "balanced".into(),
        clients: 1,
        secs: per_sec.len() as f64,
        r_ops: per_sec.iter().sum::<u64>() as usize,
        w_ops: 0,
        r: vec![1_000; 10],
        w: Vec::new(),
        errors: 0,
        refusals: 0,
        per_sec,
        started_unix_ms: 0,
        per_shape: Default::default(),
        plan_exhausted: Vec::new(),
        plan_exhausted_us: Vec::new(),
        plan_ops_per_client: None,
        refusal_kinds: Default::default(),
        max_inflight: 2,
        writes_mode: "multi".into(),
    }
}
