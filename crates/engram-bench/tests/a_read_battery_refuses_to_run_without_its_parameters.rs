//! The three LDBC read batteries must not run on invented parameters.
//!
//! # The failure this guards
//!
//! A parameter is part of the question. Two figures in this project's recorded
//! history are not results because of it:
//!
//! * `bi12` ran `languages ['en','de']` against a corpus carrying `uz`, `tk`
//!   and `ar`. It matched zero rows, returned in a fraction of the real time,
//!   and was written into a comparison table as `1 row / 160 s`.
//! * `bi16` ran on dates its tag had no messages for — twice.
//!
//! Neither failed. Both produced a well-formed answer that looked like a fast
//! query. So the lane refuses to start without a parameter file, and the
//! refusal has to NAME the problem rather than fall through to a generic
//! usage dump — otherwise the next person supplies nothing and reads the
//! resulting twenty empty rows as a battery that ran.
//!
//! These assertions need no server: every one of them is about what the
//! binary does BEFORE it opens a socket.

use std::process::Command;

fn harness(args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(args)
        .output()
        .expect("the harness binary runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.code().unwrap_or(-1))
}

/// An address that nothing is listening on. Every test here must refuse before
/// it would matter, so a connection attempt is itself a failure of the test's
/// premise.
const DEAD: &str = "127.0.0.1:9";

const FAMILIES: [&str; 3] = ["snb-bi", "snb-interactive", "finbench"];

#[test]
fn every_read_battery_refuses_without_a_parameter_file() {
    for fam in FAMILIES {
        let (text, code) = harness(&[
            fam,
            DEAD,
            "--rig",
            "bench-ccx63",
            "--thread-cap",
            "6",
            "--cache-mb",
            "8192",
        ]);
        assert_eq!(code, 2, "{fam} should refuse with exit 2:\n{text}");
        assert!(
            text.contains("--params"),
            "{fam}'s refusal must name the missing flag:\n{text}"
        );
    }
}

#[test]
fn the_refusal_explains_why_rather_than_only_what() {
    // A usage line teaches nothing. The refusal carries the bi12 case so the
    // reader understands that supplying SOMETHING is not the fix — supplying
    // parameters derived from THIS corpus is.
    let (text, _) = harness(&[
        "snb-bi",
        DEAD,
        "--rig",
        "bench-ccx63",
        "--thread-cap",
        "6",
        "--cache-mb",
        "8192",
    ]);
    assert!(text.contains("bi12"), "the refusal cites the case:\n{text}");
    assert!(
        text.contains("ZERO rows") || text.contains("zero rows"),
        "the refusal says what went wrong:\n{text}"
    );
    assert!(
        text.contains("snbparams"),
        "the refusal names the tool that fixes it:\n{text}"
    );
}

#[test]
fn a_parameter_file_that_is_not_an_object_is_refused_by_name() {
    let dir = std::env::temp_dir().join(format!("engram-params-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.json");
    std::fs::write(&path, "[1, 2, 3]").unwrap();
    let (text, code) = harness(&[
        "snb-bi",
        DEAD,
        "--params",
        path.to_str().unwrap(),
        "--rig",
        "bench-ccx63",
        "--thread-cap",
        "6",
        "--cache-mb",
        "8192",
    ]);
    assert_eq!(code, 2, "a malformed file stops the run:\n{text}");
    assert!(
        text.contains("not a JSON object"),
        "the refusal says what is wrong with the file:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_parameter_value_of_an_unusable_shape_is_refused_and_names_the_parameter() {
    // A nested object has no text form, so it cannot be coerced against the
    // catalogue's declared type. Refusing beats stringifying it into something
    // that binds and matches nothing.
    let dir = std::env::temp_dir().join(format!("engram-params2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nested.json");
    std::fs::write(&path, r#"{"bi1": {"datetime": {"nested": 1}}}"#).unwrap();
    let (text, code) = harness(&[
        "snb-bi",
        DEAD,
        "--params",
        path.to_str().unwrap(),
        "--rig",
        "bench-ccx63",
        "--thread-cap",
        "6",
        "--cache-mb",
        "8192",
    ]);
    assert_eq!(code, 2, "{text}");
    assert!(
        text.contains("datetime"),
        "the refusal names the offending parameter:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_usage_documents_that_a_ceiling_bounds_the_client_not_the_server() {
    // The rule this pins was MEASURED, not reasoned about: on SF3 bi7
    // answered in 184 ms in one run and hit the 300 s ceiling in the next,
    // and bi8a/bi8b went from 54 s each to the ceiling, purely because bi6
    // had overrun just before them and the server was still computing it.
    //
    // A reader who does not know that reads four timeouts as four slow
    // queries, so the usage text has to say it and the override has to exist.
    let (text, _) = harness(&["--help"]);
    assert!(
        text.contains("--continue-after-timeout"),
        "the override must be documented:
{text}"
    );
    assert!(
        text.to_lowercase().contains("bounds the client"),
        "the usage must state the rule, not just the flag:
{text}"
    );
}

#[test]
fn the_usage_text_no_longer_claims_these_families_cannot_run() {
    // The note said "`lsqb` and `stress` are the only workloads with an
    // EXECUTION lane ... a lane that appeared to run them would be reporting
    // on a battery no one has checked". That was true and is not any more, and
    // a stale note is how the next reader concludes the lane is missing.
    let (text, _) = harness(&["--help"]);
    assert!(
        !text.contains("are the only workloads with an EXECUTION lane"),
        "the stale families note survives:\n{text}"
    );
    for fam in FAMILIES {
        assert!(
            text.contains(fam),
            "usage should list the {fam} lane:\n{text}"
        );
    }
}
