//! The one rig table that cannot be deduplicated, held to the one that can.
//!
//! # Why a second copy exists at all
//!
//! `docs/bench/ladybug-conc.py` is the LadybugDB arm. LadybugDB is EMBEDDED —
//! a Python library in a pod that has no harness binary on it — so the executor
//! has to resolve `--rig` itself, out of process, with nothing to ask. It
//! therefore transcribes [`engram_bench::report::KNOWN_RIGS`], and a
//! transcription is exactly the shape of thing this project keeps deleting.
//!
//! # What a drift would do, and why "it fails closed" is not enough
//!
//! A rig block is compared as TEXT. If the executor's `main-pod-6cpu` said 32
//! cores and the reporter's said 16, `compare` would refuse the table naming
//! both lanes — so the immediate failure is safe. What is NOT safe is the
//! reading: the refusal says "these runs were taken on DIFFERENT RIGS", and
//! they were not. Somebody would go and re-run an arm that was already correct,
//! or worse, "fix" it by editing the rig into agreement, which is the
//! reconstruction the whole mechanism exists to refuse.
//!
//! So the table is checked here rather than trusted. The check reads the Python
//! source as text, because that is the artefact that ships to the pod: a test
//! that re-derived the values some other way would pass while the file that
//! runs disagrees.

use std::path::Path;

use engram_bench::report::{KNOWN_RIGS, Rig};

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

/// Pull `KNOWN_RIGS = { … }` out of the executor's source.
///
/// Deliberately a small hand parser over the literal shape the file uses,
/// rather than anything general: the point is to read the bytes that ship, and
/// a parser that accepted more shapes than the file writes would let a
/// reformatting slip past by matching something else.
fn python_rigs(src: &str) -> Vec<(String, String, u32, Option<u32>, u32)> {
    let start = src
        .find("KNOWN_RIGS = {")
        .expect("the executor must declare KNOWN_RIGS");
    let body = &src[start..];
    let end = body
        .find("\n}\n")
        .expect("KNOWN_RIGS must close at column 0");
    let body = &body[..end];

    let field = |block: &str, key: &str| -> String {
        let at = block
            .find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("a rig block has no `{key}`: {block}"));
        let rest = &block[at + key.len() + 3..];
        let stop = rest.find(',').unwrap_or(rest.len());
        rest[..stop].trim().trim_matches('"').to_string()
    };

    let mut out = Vec::new();
    let mut rest = body;
    // Each entry is `    "name": {` … `    },`.
    while let Some(at) = rest.find("\n    \"") {
        let after = &rest[at + 6..];
        let close = after.find("\": {").expect("a rig entry names itself");
        let name = after[..close].to_string();
        let block_start = &after[close + 4..];
        let block_end = block_start.find("\n    }").expect("a rig entry must close");
        let block = &block_start[..block_end];
        let quota = field(block, "cpu_quota_cores");
        out.push((
            name,
            field(block, "node_type"),
            field(block, "node_cores").parse().expect("node_cores"),
            if quota == "None" {
                None
            } else {
                Some(quota.parse().expect("cpu_quota_cores"))
            },
            // `40 * 1024`, as the file writes it.
            field(block, "mem_limit_mb")
                .split('*')
                .map(|p| p.trim().parse::<u32>().expect("mem_limit_mb"))
                .product(),
        ));
        rest = &block_start[block_end..];
    }
    out
}

#[test]
fn the_python_executor_and_the_reporter_agree_on_every_lane() {
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    let mut py = python_rigs(&src);
    py.sort();

    let mut rs: Vec<(String, String, u32, Option<u32>, u32)> = KNOWN_RIGS
        .iter()
        .map(|r| {
            (
                r.name.to_string(),
                r.node_type.to_string(),
                r.node_cores,
                r.cpu_quota_cores,
                r.mem_limit_mb,
            )
        })
        .collect();
    rs.sort();

    assert_eq!(
        py, rs,
        "docs/bench/ladybug-conc.py's KNOWN_RIGS has drifted from report.rs's. A rig \
         block is compared as TEXT, so the drift would surface as `these runs were taken \
         on DIFFERENT RIGS` for two runs that were taken on the same one — and the \
         obvious remedy for that message is to edit a rig into agreement, which is the \
         reconstruction the whole mechanism exists to refuse."
    );
}

#[test]
fn the_comparison_would_actually_notice_a_drift() {
    // THE GUARD OBSERVED FAILING. A check that has never been seen to fail is
    // not known to be a check — and this one is a text comparison over a hand
    // parser, which is exactly the kind that can quietly match nothing and
    // pass. So the same parser is fed a source whose only difference is one
    // core count, and the comparison must reject it.
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    let real = python_rigs(&src);
    assert_eq!(
        real.len(),
        KNOWN_RIGS.len(),
        "the parser must find every lane"
    );

    let drifted = python_rigs(&src.replace("\"node_cores\": 16,", "\"node_cores\": 32,"));
    assert_ne!(
        real, drifted,
        "a one-field drift must change what the parser returns, or this test is \
         comparing something that is not the rig table"
    );
    assert!(
        drifted.iter().any(|r| r.2 == 32),
        "the mutation must land in the parsed table, not merely in the text"
    );
}

#[test]
fn the_executors_rig_names_still_resolve_in_the_reporter() {
    // The other direction: a name the executor offers an operator must be one
    // `Rig::from_spec` accepts, or `--rig main-pod-6cpu` would be a valid flag
    // on the pod and an unknown rig everywhere else.
    let Some(src) = executor_source_or_skip() else {
        return;
    };
    for (name, node_type, cores, quota, mem) in python_rigs(&src) {
        let rig = Rig::from_spec(&name, "sf1").unwrap_or_else(|e| {
            panic!(
                "the executor offers `{name}`, which the reporter \
                                        refuses: {e}"
            )
        });
        assert_eq!(rig.node_type, node_type);
        assert_eq!(rig.node_cores, cores);
        assert_eq!(rig.cpu_quota_cores, quota);
        assert_eq!(rig.mem_limit_mb, mem);
    }
}
