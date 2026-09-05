//! The scrub gate — "this tree can be published".
//!
//! The open-source release is a SINGLE public launch built from one commit, so
//! the entire disclosure risk concentrates on that one commit. A hand-curated
//! delete list is not a control: it is verified by the same hand that wrote it,
//! and three separate such lists in this project's own planning were incomplete
//! when someone spent ten minutes on them. This gate is the control.
//!
//! # Why the RULES are a file and this is not
//!
//! This file ships in the published tree. The rules did too, once — and that
//! made the gate written to prevent disclosure into the most concentrated
//! disclosure in the tree: every private name in one place, each annotated with
//! what it is, better organised than anything it catches.
//!
//! So the gate ships and its rules do not. `scrub-rules.txt` is not in the
//! publish allow-list, which fails closed: it is unpublished because nothing
//! names it, rather than because something excludes it.
//!
//! A consequence worth having: this gate can now scan its own source, which it
//! previously had to skip.
//!
//! # A missing rules file FAILS
//!
//! Moving the rules out created a new way to be wrong. A gate with no rules
//! finds nothing, and finding nothing is the shape of a pass. So an absent,
//! unreadable, empty or rule-less file is a FINDING, and
//! `a_missing_rules_file_FAILS` proves that path rather than assuming it.
//!
//! # Why a byte scan, not `read_to_string`
//!
//! The tree contains ELF binaries, `.zst` archives and JSONL. `read_to_string`
//! returns `Err` on non-UTF-8 and the natural `let Ok(s) = ... else { continue }`
//! then SKIPS exactly the files most likely to carry an embedded private path —
//! a 28 MB unstripped binary links the paths it was built from. Skipping and
//! finding-nothing print identically. So every file is read as BYTES and matched
//! as bytes, and anything unreadable is a FINDING, never a skip.
//!
//! # Why a negative canary as well as a positive one
//!
//! An over-matching rule is not a safe failure here. `production` appears 192
//! times in `crates/`, and 79 of those are the benign `production order` / `seq`
//! / `row` execution vocabulary — a rule that matched the bare word would demand
//! ~113 edits to correct code. So every rule is checked BOTH ways on every run:
//! the positive canary proves it still catches what it is for, and the negative
//! canary proves it does not catch the near-misses beside it. A gate that has
//! quietly started matching everything is as broken as one matching nothing.

use std::fs;
use std::path::{Path, PathBuf};

/// Where the rules live, relative to the repository root. Deliberately outside
/// `xtask/`, so nothing about the published crate hints at its shape.
pub const RULES_FILE: &str = "scrub-rules.txt";

/// One forbidden token, and why it may not be published.
struct Rule {
    class: String,
    /// ASCII-lowercase; matching lowercases the haystack, so this must be too.
    needle: String,
    why: String,
}

/// The rules and both canaries, as loaded.
struct Rules {
    rules: Vec<Rule>,
    positive: String,
    negative: String,
}

/// Parse the rules file.
///
/// Every failure direction returns `Err`, because each one would otherwise
/// produce a gate that scans a tree against zero rules and reports it clean.
fn load_rules(path: &Path) -> Result<Rules, String> {
    let text = fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read the rules file {} ({e}) — without it this gate has NO rules, and a \
             gate with no rules reports every tree as clean",
            path.display()
        )
    })?;

    let (mut rules, mut positive, mut negative) = (Vec::new(), String::new(), String::new());
    let mut section = "";
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        match t {
            "[rules]" => section = "rules",
            "[positive]" => section = "positive",
            "[negative]" => section = "negative",
            _ => match section {
                "rules" => {
                    if t.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = t.split('|').map(str::trim).collect();
                    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
                        return Err(format!(
                            "malformed rule in {}: {t:?} — expected `class | needle | why`",
                            path.display()
                        ));
                    }
                    if parts[1] != parts[1].to_ascii_lowercase() {
                        return Err(format!(
                            "needle {:?} is not ASCII-lowercase; matching lowercases the \
                             haystack, so a mixed-case needle silently matches NOTHING",
                            parts[1]
                        ));
                    }
                    rules.push(Rule {
                        class: parts[0].to_string(),
                        needle: parts[1].to_string(),
                        why: parts[2].to_string(),
                    });
                }
                "positive" => {
                    positive.push_str(line);
                    positive.push('\n');
                }
                "negative" => {
                    negative.push_str(line);
                    negative.push('\n');
                }
                _ => {}
            },
        }
    }

    if rules.is_empty() {
        return Err(format!(
            "{} declares no rules — a gate with no rules finds nothing, and finding nothing is \
             the shape of a pass",
            path.display()
        ));
    }
    if positive.trim().is_empty() {
        return Err(format!(
            "{} has no [positive] canary — without it, a rule that matches nothing is \
             indistinguishable from a rule that is satisfied",
            path.display()
        ));
    }
    if negative.trim().is_empty() {
        return Err(format!(
            "{} has no [negative] canary — without it, a rule broadened until it matches \
             everything still passes its own self-test",
            path.display()
        ));
    }
    Ok(Rules {
        rules,
        positive,
        negative,
    })
}

/// One hit.
struct Hit {
    path: String,
    class: String,
    needle: String,
    why: String,
    count: usize,
}

/// Case-insensitive byte substring search. `haystack` is already lowercased.
fn contains_at(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut n = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            n += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    n
}

/// Apply every rule to one blob. Pure, so it runs against the canaries and the
/// tree through the same code path — a self-test that exercised a different
/// function would prove nothing about the one that scanned the workspace.
fn scan_bytes(rules: &[Rule], bytes: &[u8], path: &str) -> Vec<Hit> {
    let lower: Vec<u8> = bytes.to_ascii_lowercase();
    let mut out = Vec::new();
    for r in rules {
        let count = contains_at(&lower, r.needle.as_bytes());
        if count > 0 {
            out.push(Hit {
                path: path.to_string(),
                class: r.class.clone(),
                needle: r.needle.clone(),
                why: r.why.clone(),
                count,
            });
        }
    }
    out
}

/// Every file under `root`, excluding build output and VCS metadata.
///
/// This gate's own source is NO LONGER excluded: the needles live in the rules
/// file now, so this file no longer matches itself.
fn files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if p.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The rules file contains every needle by construction, so it matches itself.
/// Excluded by path, narrowly — and never published.
fn is_rules_file(p: &Path) -> bool {
    p.display()
        .to_string()
        .replace('\\', "/")
        .ends_with(RULES_FILE)
}

/// The gate's verdict, mirroring `GateResult` in main.rs.
pub struct ScrubReport {
    pub passed: bool,
    pub scanned: String,
    pub findings: Vec<String>,
}

pub fn run(root: &Path) -> ScrubReport {
    run_with_rules(root, &repo_rules_path(root))
}

/// The rules live at the repository root. When scrubbing a STAGED tree — which
/// deliberately does not contain them — fall back to this repository's own.
fn repo_rules_path(root: &Path) -> PathBuf {
    let here = root.join(RULES_FILE);
    if here.is_file() {
        return here;
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|r| r.join(RULES_FILE))
        .unwrap_or(here)
}

fn run_with_rules(root: &Path, rules_path: &Path) -> ScrubReport {
    let mut findings = Vec::new();

    // ── The rules, before anything else ──────────────────────────────────
    let loaded = match load_rules(rules_path) {
        Ok(r) => r,
        Err(e) => {
            return ScrubReport {
                passed: false,
                scanned: format!("NO RULES LOADED from {}", rules_path.display()),
                findings: vec![e],
            };
        }
    };
    let rules = &loaded.rules;

    // ── Self-test FIRST ──────────────────────────────────────────────────
    //
    // Before the tree's verdict means anything, the matcher has to be shown to
    // work in both directions. Running this first also means a broken gate
    // reports "I am broken" rather than "the tree is clean".
    let pos_hits = scan_bytes(rules, loaded.positive.as_bytes(), "<positive-canary>");
    if pos_hits.len() != rules.len() {
        // Name the rules that did NOT fire. "16 of 17" sends the reader looking;
        // naming the silent rule sends them to the line that broke.
        let fired: Vec<&str> = pos_hits.iter().map(|h| h.needle.as_str()).collect();
        let silent: Vec<&str> = rules
            .iter()
            .map(|r| r.needle.as_str())
            .filter(|n| !fired.contains(n))
            .collect();
        findings.push(format!(
            "SELF-TEST FAILED (positive): {} of {} rules fired against the hand-written canary. \
             Silent rule(s): {:?}. Either the needle was edited without updating the canary, or \
             the matcher is broken — and both report a clean tree in the same words as a correct \
             run",
            pos_hits.len(),
            rules.len(),
            silent,
        ));
    }
    let neg_hits = scan_bytes(rules, loaded.negative.as_bytes(), "<negative-canary>");
    for h in &neg_hits {
        findings.push(format!(
            "SELF-TEST FAILED (negative): rule `{}` ({}) matched benign text that must NOT trip \
             it. An over-matching rule would demand edits to correct code, which is how a gate \
             gets switched off",
            h.needle, h.class,
        ));
    }

    // ── The tree ─────────────────────────────────────────────────────────
    let mut paths = Vec::new();
    files(root, &mut paths);
    paths.sort();
    let total_files = paths.len();
    let mut scanned_files = 0usize;
    let mut scanned_bytes = 0u64;
    let mut unreadable = 0usize;
    let mut hits: Vec<Hit> = Vec::new();

    for p in &paths {
        if is_rules_file(p) {
            continue;
        }
        // BYTES, not a string: a binary or a compressed archive must be scanned,
        // not skipped. `read` only fails on I/O, never on encoding.
        match fs::read(p) {
            Ok(bytes) => {
                scanned_files += 1;
                scanned_bytes += bytes.len() as u64;
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(p)
                    .display()
                    .to_string()
                    .replace('\\', "/");
                hits.extend(scan_bytes(rules, &bytes, &rel));
            }
            Err(e) => {
                // Not a skip. A file this gate could not read is a file it
                // cannot vouch for, and "could not read" must never be
                // indistinguishable from "read it and it was clean".
                unreadable += 1;
                findings.push(format!(
                    "UNREADABLE: {} ({e}) — the gate cannot vouch for a file it did not read",
                    p.display()
                ));
            }
        }
    }

    // ── The integrity floor ──────────────────────────────────────────────
    if scanned_files == 0 {
        findings.push(
            "SCANNED NOTHING: zero files were read. A moved directory, a wrong root, or a \
             broken walk all produce '0 findings', which is the exact shape of a pass"
                .into(),
        );
    }

    // ── Report the tree's hits, grouped so the output is actionable ──────
    hits.sort_by(|a, b| (&a.class, &a.path, &a.needle).cmp(&(&b.class, &b.path, &b.needle)));
    for h in &hits {
        findings.push(format!(
            "[{}] {} — `{}` x{} ({})",
            h.class, h.path, h.needle, h.count, h.why,
        ));
    }

    let files_with_hits = {
        let mut v: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };

    ScrubReport {
        passed: findings.is_empty(),
        scanned: format!(
            "{scanned_files}/{total_files} file(s) read as bytes ({} MiB), {} rule(s) in {} \
             class(es), canaries {}/{} positive + {} false-positive, {files_with_hits} file(s) \
             with hits, {unreadable} unreadable",
            scanned_bytes / (1024 * 1024),
            rules.len(),
            {
                let mut c: Vec<&str> = rules.iter().map(|r| r.class.as_str()).collect();
                c.sort_unstable();
                c.dedup();
                c.len()
            },
            pos_hits.len(),
            rules.len(),
            neg_hits.len(),
        ),
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("engram-scrub-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    /// The canary for moving the rules out of this file.
    ///
    /// A gate whose rules are absent scans every tree against nothing and finds
    /// nothing — which prints identically to a clean tree. This asserts the
    /// missing-file direction FAILS, and that the message says why rather than
    /// reporting a clean count.
    #[test]
    #[allow(non_snake_case)]
    fn a_missing_rules_file_FAILS() {
        let d = tmp("missing");
        fs::write(d.join("a.txt"), b"harmless").expect("write");
        let r = run_with_rules(&d, &d.join("does-not-exist.txt"));
        assert!(
            !r.passed,
            "a gate with no rules must FAIL; finding nothing is the shape of a pass"
        );
        assert!(
            r.findings.iter().any(|f| f.contains("NO rules")),
            "the failure must say the gate had no rules, not report a clean count: {:?}",
            r.findings
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// An empty or rule-less file is the same hazard as a missing one.
    #[test]
    #[allow(non_snake_case)]
    fn a_rules_file_with_no_rules_FAILS() {
        let d = tmp("norules");
        let rf = d.join("rules.txt");
        fs::write(&rf, "# only a comment\n[positive]\nx\n[negative]\ny\n").expect("write");
        fs::write(d.join("a.txt"), b"harmless").expect("write");
        let r = run_with_rules(&d, &rf);
        assert!(!r.passed, "a rules file declaring no rules must fail");
        let _ = fs::remove_dir_all(&d);
    }

    /// A needle that is not lowercase silently matches nothing, because the
    /// matcher lowercases the haystack. Refuse it at load rather than at
    /// "0 findings".
    #[test]
    #[allow(non_snake_case)]
    fn a_mixed_case_needle_is_REFUSED() {
        let d = tmp("case");
        let rf = d.join("rules.txt");
        fs::write(
            &rf,
            "[rules]\nc | MixedCase | why\n[positive]\nMixedCase\n[negative]\nz\n",
        )
        .expect("write");
        let r = run_with_rules(&d, &rf);
        assert!(!r.passed, "a mixed-case needle must be refused at load");
        let _ = fs::remove_dir_all(&d);
    }

    /// The real rules file must load, and both canaries must hold. The same
    /// check `run` performs, pinned as a test so a bad edit fails `cargo test`
    /// and not only the release lane.
    #[test]
    fn the_repository_rules_load_and_both_canaries_hold() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        let loaded = load_rules(&root.join(RULES_FILE)).expect("the rules file must load");
        assert!(loaded.rules.len() >= 10, "suspiciously few rules");

        let pos = scan_bytes(&loaded.rules, loaded.positive.as_bytes(), "<pos>");
        assert_eq!(
            pos.len(),
            loaded.rules.len(),
            "every rule must fire against the hand-written positive canary"
        );
        let neg = scan_bytes(&loaded.rules, loaded.negative.as_bytes(), "<neg>");
        assert!(
            neg.is_empty(),
            "no rule may match the negative canary's near-misses: {:?}",
            neg.iter().map(|h| &h.needle).collect::<Vec<_>>()
        );
    }
}
