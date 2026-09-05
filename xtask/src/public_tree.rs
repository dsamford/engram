//! `cargo xtask public-tree <dest>` — build the publishable tree by COPYING an
//! allow-listed set into a fresh directory, then proving the result is clean.
//!
//! # Why a copy-allowlist and not a scrub-in-place
//!
//! An allowlist fails CLOSED and a `.gitignore` fails OPEN. Excluding
//! `measurements/corpus.jsonl` by name protects against that one file; listing
//! what may be copied protects against every file nobody thought about — which
//! is the set that matters, because the private data in this tree arrived
//! incidentally (a benchmark artifact, a census, a report) rather than by
//! anyone deciding to put it there.
//!
//! The tree holds, today, a 1.2 MB corpus of every Cypher statement in a
//! private product with file paths and line numbers, a document publishing that
//! product's authorization model including a privilege-escalation bypass, and
//! internal deployment notes. None of that is subtle to remove once named. The
//! risk is the file nobody names.
//!
//! (That sentence was itself rewritten because an earlier wording tripped this
//! gate's own scrub — a rule fired on a phrase describing what the rule
//! forbids. The fix was the wording, not an exemption: `NEVER` stays empty, and
//! a gate that starts carving out its own source is one step from carving out
//! whatever else is inconvenient.)
//!
//! # What this is not
//!
//! It is not a release. The output still needs a LICENSE with a real copyright
//! holder, the rename, a README that describes the project rather than its
//! history, and the vendored TCK re-fetched from a pinned upstream commit. This
//! gate answers exactly one question — *is there a subset of this repository
//! that can be published at all?* — and answers it by construction rather than
//! by assertion.

use std::fs;
use std::path::{Path, PathBuf};

/// One allow-list entry.
///
/// Every variant except [`Allow::Optional`] MUST match at least one file, and
/// the gate fails when one does not. That rule exists because of this gate's
/// own first bug: it allow-listed `crates/engram-tck/tck`, the TCK actually
/// lives in `crates/engram-tck/features`, and a missing directory was quietly
/// skipped. The gate reported a clean tree, the tree built, and its openCypher
/// conformance suite — the project's headline claim — had no feature files at
/// all. A rule that matches nothing looks exactly like a rule that is
/// satisfied.
enum Allow {
    /// Exactly this path, relative to the root. Must exist.
    File(&'static str),
    /// Exactly this path, and it is fine if it is absent — for files a project
    /// may legitimately not have yet.
    Optional(&'static str),
    /// Everything under this directory whose extension is in `exts` (empty =
    /// any extension), recursively. Must match at least one file.
    Tree {
        dir: &'static str,
        exts: &'static [&'static str],
    },
}

/// What may be copied. Everything else is left behind, including anything added
/// after this list was written — which is the point.
const ALLOW: &[Allow] = &[
    // ── Build and workspace configuration ──────────────────────────────────
    Allow::File("Cargo.toml"),
    Allow::File("Cargo.lock"),
    Allow::File("rust-toolchain.toml"),
    Allow::File("clippy.toml"),
    Allow::File("deny.toml"),
    Allow::File(".gitignore"),
    // The `cargo xtask` alias. Without it the published tree cannot run a
    // single one of its own gates — `cargo xtask all` is "no such command" —
    // which the first public snapshot demonstrated, on the run that was
    // supposed to prove the tree was sound.
    Allow::File(".cargo/config.toml"),
    // Line-ending normalisation. It ships because several tests pin BYTES, and
    // a consumer who checks out this tree with CRLF and then runs the golden
    // hash tests gets a failure whose cause is their git configuration.
    Allow::File(".gitattributes"),
    // ── Legal. Absent today; the gate reports them missing rather than
    //    silently producing a tree with no licence, which would look fine. ──
    Allow::File("LICENSE"),
    Allow::File("NOTICE"),
    Allow::File("README.md"),
    // Not written yet. Optional so their absence does not fail the gate for the
    // wrong reason — LICENSE/NOTICE/README absence is reported explicitly below.
    Allow::Optional("TRADEMARKS.md"),
    Allow::Optional("SECURITY.md"),
    Allow::Optional("CONTRIBUTING.md"),
    Allow::Optional("CHANGELOG.md"),
    // ── Source. Manifests, code, tests, benches — nothing else. ────────────
    Allow::Tree {
        dir: "crates",
        exts: &["rs", "toml"],
    },
    Allow::Tree {
        dir: "xtask",
        exts: &["rs", "toml"],
    },
    // The vendored openCypher TCK: feature files, plus its own licence text.
    Allow::Tree {
        dir: "crates/engram-tck/features",
        exts: &["feature", "md", "txt"],
    },
    // ── The documentation site ─────────────────────────────────────────────
    //
    // `docs/book`, NOT `docs`. The tree under `docs/` is the engineering
    // record — dated plans and measurement write-ups that name the private
    // product, its subsystems and its source paths. Those are the working
    // documents; what ships is the rewritten form under `docs/book/src`, and
    // the distinction has to live in the RULE rather than in a habit, because
    // a rule that says `docs` publishes the next file anyone drops there.
    //
    // The extension list is closed for the same reason: a `.diff` or a
    // `.jsonl` under `docs/book` would be copied by a bare directory rule, and
    // the two largest disclosures in this tree are exactly those shapes.
    Allow::Tree {
        dir: "docs/book",
        exts: &["md", "toml", "css", "js", "svg", "hbs"],
    },
    // ── CI ─────────────────────────────────────────────────────────────────
    //
    // Without this the published tree has no way to build, test or deploy
    // itself. The gate would still pass — it checks that every rule matches
    // something, not that everything worth shipping has a rule — so this is
    // asserted in `ci.yml` against the assembled tree as well.
    Allow::Tree {
        dir: ".github",
        exts: &["yml"],
    },
];

/// Paths that are NEVER copied even if an allow rule would otherwise take them.
///
/// A deny inside an allow-list is normally a smell — if the allow-list is right
/// the deny is unreachable. These exist because the `crates` tree rule takes
/// every `.rs` and `.toml` beneath it, and a fixture that deliberately embeds a
/// forbidden token would then be copied by a rule that is otherwise correct.
/// They are listed, few, and each says why.
const NEVER: &[&str] = &[
    // Nothing today. Kept as the place such a case goes, so the next person
    // adds a line here rather than weakening a tree rule.
];

fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for p in names {
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Build output and VCS metadata are never source.
            if name == "target" || name == ".git" || name == "node_modules" {
                continue;
            }
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The files the allow-list selects, plus any rule that matched NOTHING.
///
/// The second return value is the one that matters. See [`Allow`].
fn selected(root: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut dead: Vec<String> = Vec::new();
    for a in ALLOW {
        match a {
            Allow::File(f) => {
                let p = root.join(f);
                if p.is_file() {
                    out.push(p);
                } else {
                    dead.push(format!("required file `{f}` does not exist"));
                }
            }
            Allow::Optional(f) => {
                let p = root.join(f);
                if p.is_file() {
                    out.push(p);
                }
            }
            Allow::Tree { dir, exts } => {
                let d = root.join(dir);
                if !d.is_dir() {
                    dead.push(format!("allow-listed directory `{dir}` does not exist"));
                    continue;
                }
                let mut found = Vec::new();
                walk(&d, &mut found);
                let before = out.len();
                for p in found {
                    if exts.is_empty() || exts.contains(&ext_of(&p).as_str()) {
                        out.push(p);
                    }
                }
                if out.len() == before {
                    dead.push(format!(
                        "allow-listed directory `{dir}` matched no file with extension(s) {exts:?}"
                    ));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    let never: Vec<String> = NEVER.iter().map(|s| (*s).to_string()).collect();
    out.retain(|p| !never.contains(&rel(root, p)));
    (out, dead)
}

/// The outcome, in the shape `main.rs` reports.
pub struct TreeReport {
    pub passed: bool,
    /// What was copied and what was left — printed on success too, because
    /// "0 findings" over an empty tree is the reading this gate exists to
    /// prevent.
    pub scanned: String,
    pub findings: Vec<String>,
}

/// Whether the working tree has uncommitted changes.
///
/// # Why this gate refuses a dirty tree
///
/// `run` copies a DIRECTORY, not a commit. Pointed at a working tree it
/// publishes whatever happens to be open in an editor — and the first public
/// snapshot of this project did exactly that, sweeping in 1,115 lines of
/// in-progress work across sixteen files that nobody had reviewed for
/// publication.
///
/// Nothing in the output distinguishes it: an allow-list copy of a dirty tree
/// and of a clean one produce the same PASS line. So the refusal is here, at
/// the only point that can tell the difference.
///
/// Cut from a clean checkout instead:
///
/// ```text
/// git worktree add --detach /tmp/clean HEAD
/// cd /tmp/clean && cargo xtask public-tree /tmp/public
/// ```
///
/// A tree that is not a git checkout at all is NOT refused — the gate must
/// still assemble from an exported archive — but it says which case it took,
/// because "clean" and "could not tell" must not print identically.
fn dirty_files(root: &Path) -> Result<Vec<String>, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err("not a git checkout".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

pub fn run(root: &Path, dest: &Path) -> TreeReport {
    // Refuse a dirty tree before copying a single file. See `dirty_files`.
    let provenance = match dirty_files(root) {
        Ok(d) if !d.is_empty() => {
            let mut findings = vec![
                format!(
                    "REFUSING a dirty working tree: {} uncommitted change(s).",
                    d.len()
                ),
                "This gate copies a DIRECTORY, not a commit, so it would publish work in progress, and the output would carry no sign of it.".to_string(),
                "Cut from a clean checkout: `git worktree add --detach /tmp/clean HEAD && cd /tmp/clean && cargo xtask public-tree <dest>`".to_string(),
            ];
            findings.extend(d.into_iter().take(10).map(|f| format!("  dirty: {f}")));
            return TreeReport {
                passed: false,
                scanned: format!("refused before copying, at {}", root.display()),
                findings,
            };
        }
        Ok(_) => "clean git checkout",
        Err(_) => "not a git checkout (provenance unverified)",
    };
    let _ = provenance;
    // Refuse a destination that already holds anything. Copying over an
    // existing tree would leave whatever was there before mixed into the
    // output, and the whole value of this gate is that the result contains
    // ONLY what the allow-list chose.
    if dest.exists() {
        let empty = fs::read_dir(dest).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !empty {
            return TreeReport {
                passed: false,
                scanned: format!("destination {}", dest.display()),
                findings: vec![format!(
                    "destination {} exists and is not empty — refusing. The output must \
                     contain only allow-listed files, and copying into an existing tree \
                     cannot promise that.",
                    dest.display()
                )],
            };
        }
    }

    let (picked, dead) = selected(root);
    if !dead.is_empty() {
        return TreeReport {
            passed: false,
            scanned: format!("{} allow rule(s), {} file(s) selected", ALLOW.len(), picked.len()),
            findings: dead
                .into_iter()
                .map(|d| format!("{d} — an allow rule that selects nothing is a rule that is wrong, not one that is satisfied"))
                .collect(),
        };
    }
    if picked.is_empty() {
        return TreeReport {
            passed: false,
            scanned: format!("root {}", root.display()),
            findings: vec![
                "the allow-list selected NO files — the root is probably wrong. A gate \
                 that copies nothing and reports success is worse than no gate."
                    .into(),
            ],
        };
    }

    // How much was left behind? Reported, because "copied 400 files" means
    // nothing without knowing whether that was 400 of 400 or 400 of 4,000.
    let mut all = Vec::new();
    walk(root, &mut all);
    let skipped = all.len().saturating_sub(picked.len());

    let mut bytes = 0u64;
    for src in &picked {
        let r = rel(root, src);
        let out = dest.join(&r);
        if let Some(parent) = out.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return TreeReport {
                    passed: false,
                    scanned: format!("{} file(s) selected", picked.len()),
                    findings: vec![format!("could not create {}: {e}", parent.display())],
                };
            }
        }
        match fs::copy(src, &out) {
            Ok(n) => bytes += n,
            Err(e) => {
                return TreeReport {
                    passed: false,
                    scanned: format!("{} file(s) selected", picked.len()),
                    findings: vec![format!("could not copy {r}: {e}")],
                };
            }
        }
    }

    // PROVE the result. Running scrub over the OUTPUT rather than trusting the
    // allow-list is the whole point: an allow rule can be wrong, and this is
    // the only step that would notice.
    let report = crate::scrub::run(dest);
    let missing_legal: Vec<&str> = ["LICENSE", "NOTICE", "README.md"]
        .into_iter()
        .filter(|f| !dest.join(f).is_file())
        .collect();

    let scanned = format!(
        "{} file(s) copied ({} KiB) into {}, {} left behind; the OUTPUT then scrubbed: {}",
        picked.len(),
        bytes / 1024,
        dest.display(),
        skipped,
        report.scanned
    );

    let mut findings = report.findings;
    if !missing_legal.is_empty() {
        // Reported as a finding, and it FAILS the gate. A tree with no LICENSE
        // is not publishable, and a gate that called it "clean" because no
        // forbidden token appeared would be answering a narrower question than
        // the one its name asks.
        findings.push(format!(
            "MISSING: {} — the tree scrubs clean but cannot be published without them. \
             LICENSE needs a copyright HOLDER, which is still an open owner decision.",
            missing_legal.join(", ")
        ));
    }

    TreeReport {
        passed: report.passed && missing_legal.is_empty(),
        scanned,
        findings,
    }
}
