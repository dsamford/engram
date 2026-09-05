//! The documentation gate — "the book still describes this engine".
//!
//! # Why a gate rather than a review
//!
//! Documentation rots in a way code does not, because nothing dereferences it.
//! A renamed flag breaks a caller; a renamed flag described in prose breaks
//! nobody, and the prose stays wrong until a reader is confused enough to
//! report it. That is the failure mode this gate exists for.
//!
//! It checks four things, and each is a promise the book makes that the tree
//! can contradict:
//!
//! | check | the promise |
//! |---|---|
//! | orphans | every page is reachable from `SUMMARY.md` |
//! | links | every intra-book link resolves to a page that exists |
//! | reference | the CLI reference names every flag the CLI parses |
//! | usage | `--help` names exactly the flags the CLI parses |
//!
//! # The flag check is bidirectional on purpose
//!
//! A one-way check — "every documented flag exists" — passes a reference that
//! documents three of forty flags. Only the other direction finds the state
//! this repository was actually in: eight flags parsed by `main.rs` and absent
//! from its own `USAGE` string, so `--help` was wrong about the very program
//! printing it.
//!
//! # What this gate deliberately does NOT check
//!
//! That the prose is true. No gate can. It checks the mechanical claims — the
//! ones with a right answer in the tree — so a human reading the book spends
//! their attention on the sentences rather than on the tables.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub struct Report {
    pub passed: bool,
    pub scanned: String,
    pub findings: Vec<String>,
}

/// Where the book lives, relative to the repository root.
const BOOK_SRC: &str = "docs/book/src";
/// The CLI whose flags the reference must agree with.
const CLI_SOURCE: &str = "crates/engram-server/src/main.rs";
/// The page that must name every flag.
const CLI_REFERENCE: &str = "docs/book/src/reference/cli.md";

pub fn run(root: &Path) -> Report {
    let src = root.join(BOOK_SRC);
    if !src.is_dir() {
        return Report {
            passed: false,
            scanned: format!("{BOOK_SRC} does not exist"),
            findings: vec![format!(
                "the book source directory `{BOOK_SRC}` is absent — this gate cannot \
                 report a clean book it never found"
            )],
        };
    }

    let mut findings = Vec::new();

    // ── Every markdown file under the book, on disk ────────────────────────
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    collect_markdown(&src, &src, &mut on_disk);

    // ── Every page SUMMARY.md links to ─────────────────────────────────────
    let summary = match fs::read_to_string(src.join("SUMMARY.md")) {
        Ok(s) => s,
        Err(e) => {
            return Report {
                passed: false,
                scanned: format!("{BOOK_SRC} — {} markdown file(s)", on_disk.len()),
                findings: vec![format!("cannot read SUMMARY.md: {e}")],
            };
        }
    };
    let listed: BTreeSet<String> = links_in(&summary)
        .into_iter()
        .filter(|l| l.ends_with(".md"))
        .map(|l| normalise(&l))
        .collect();

    // ── Orphans: on disk, unreachable from SUMMARY ─────────────────────────
    for f in on_disk.difference(&listed) {
        if f == "SUMMARY.md" {
            continue;
        }
        findings.push(format!(
            "{f} — a page no chapter links to; unreachable in the built book. Add it to \
             SUMMARY.md or delete it"
        ));
    }

    // ── Listed but absent ──────────────────────────────────────────────────
    for f in listed.difference(&on_disk) {
        findings.push(format!("SUMMARY.md links to {f}, which does not exist"));
    }

    // ── Intra-book links from every page ───────────────────────────────────
    let mut links_checked = 0usize;
    for page in &on_disk {
        let Ok(text) = fs::read_to_string(src.join(page)) else {
            findings.push(format!("{page} — unreadable"));
            continue;
        };
        let dir = Path::new(page).parent().map(Path::to_path_buf).unwrap_or_default();
        for link in links_in(&text) {
            // External URLs, in-page anchors and the generated rustdoc under
            // ../api are out of scope: the first two are not ours to resolve,
            // the third does not exist until `cargo doc` has run.
            if link.starts_with("http://")
                || link.starts_with("https://")
                || link.starts_with('#')
                || link.starts_with("mailto:")
                || link.contains("/api/")
            {
                continue;
            }
            let target = link.split('#').next().unwrap_or("");
            if target.is_empty() || !target.ends_with(".md") {
                continue;
            }
            links_checked += 1;
            let resolved = normalise(&dir.join(target).to_string_lossy());
            if !on_disk.contains(&resolved) {
                findings.push(format!("{page} links to `{link}`, which does not exist"));
            }
        }
    }

    // ── The CLI reference agrees with the CLI, both directions ─────────────
    let cli_src = fs::read_to_string(root.join(CLI_SOURCE)).unwrap_or_default();
    let parsed = flags_parsed(&cli_src);
    let in_usage = flags_in_usage(&cli_src);
    let reference = fs::read_to_string(root.join(CLI_REFERENCE)).unwrap_or_default();

    if reference.trim().is_empty() {
        findings.push(format!(
            "{CLI_REFERENCE} is absent or empty — the flag check must not pass by finding \
             nothing to disagree with"
        ));
    } else {
        for f in &parsed {
            if !reference.contains(f.as_str()) {
                findings.push(format!(
                    "`{f}` is parsed by {CLI_SOURCE} but absent from the CLI reference"
                ));
            }
        }
    }

    for f in parsed.difference(&in_usage) {
        findings.push(format!(
            "`{f}` is parsed by {CLI_SOURCE} but absent from its own USAGE string — \
             `--help` is wrong about the program printing it"
        ));
    }
    for f in in_usage.difference(&parsed) {
        findings.push(format!(
            "`{f}` appears in USAGE but is never parsed — `--help` promises a flag that \
             does nothing"
        ));
    }

    // A gate that found no pages must not report a clean book.
    if on_disk.len() < 2 {
        findings.push(format!(
            "only {} markdown file(s) found under {BOOK_SRC} — the gate's reach is \
             implausible, so this is a failure rather than a clean run",
            on_disk.len()
        ));
    }

    Report {
        passed: findings.is_empty(),
        scanned: format!(
            "{} page(s), {} SUMMARY entr(ies), {links_checked} intra-book link(s), {} CLI \
             flag(s) parsed / {} in USAGE",
            on_disk.len(),
            listed.len(),
            parsed.len(),
            in_usage.len(),
        ),
        findings,
    }
}

/// Every `*.md` beneath `dir`, as forward-slashed paths relative to `base`.
fn collect_markdown(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_markdown(base, &p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            if let Ok(rel) = p.strip_prefix(base) {
                out.insert(normalise(&rel.to_string_lossy()));
            }
        }
    }
}

/// Markdown link targets: the parenthesised half of every `[text](target)`.
///
/// Deliberately not a markdown parser. It over-collects — a link inside a
/// fenced code block counts — and that is the safe direction: a false positive
/// is a findable line, a false negative is a dead link that ships.
fn links_in(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] == b']' && b[i + 1] == b'(' {
            let start = i + 2;
            let mut j = start;
            let mut depth = 1usize;
            while j < b.len() {
                match b[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b'\n' => break,
                    _ => {}
                }
                j += 1;
            }
            if j < b.len() && b[j] == b')' {
                // `[text](target "title")` — the title is not part of the path.
                if let Some(target) = text[start..j].split_whitespace().next() {
                    if !target.is_empty() {
                        out.push(target.to_string());
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `a/./b` and `a/../b` reduced, backslashes forward-slashed.
fn normalise(p: &str) -> String {
    let p = p.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The `USAGE` literal, and everything else, as two separate haystacks — so
/// the two halves of the flag comparison never read the same bytes.
fn split_usage(src: &str) -> (String, String) {
    let Some(start) = src.find("const USAGE") else {
        return (String::new(), src.to_string());
    };
    let Some(end) = src[start..].find("\";") else {
        return (String::new(), src.to_string());
    };
    let usage = src[start..start + end].to_string();
    let rest = format!("{}{}", &src[..start], &src[start + end..]);
    (usage, rest)
}

fn is_flag_body(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
}

/// Long flags the CLI actually parses, read from the string literals OUTSIDE
/// the usage text, so `--help` and the parser can be compared to each other.
fn flags_parsed(src: &str) -> BTreeSet<String> {
    let (_, body) = split_usage(src);
    let b = body.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i + 3 < b.len() {
        if b[i] == b'"' && b[i + 1] == b'-' && b[i + 2] == b'-' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' {
                j += 1;
            }
            let flag = &body[start..j];
            if flag.len() > 2 && is_flag_body(&flag[2..]) {
                out.insert(flag.to_string());
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out.remove("--help");
    out.remove("--version");
    out
}

/// Long flags DECLARED in the `USAGE` string — at the head of a line, because a
/// flag named inside a prose paragraph is a cross-reference, not a declaration.
fn flags_in_usage(src: &str) -> BTreeSet<String> {
    let (usage, _) = split_usage(src);
    let mut out = BTreeSet::new();
    for line in usage.lines() {
        for token in line.split_whitespace().take(3) {
            let token = token.trim_end_matches(',');
            if let Some(rest) = token.strip_prefix("--") {
                if is_flag_body(rest) {
                    out.insert(token.to_string());
                }
            }
        }
    }
    out.remove("--help");
    out.remove("--version");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_and_ignores_bare_parentheses() {
        let got = links_in("see [a](./a.md) and [b](../b/c.md \"t\") but not (plain).");
        assert_eq!(got, vec!["./a.md", "../b/c.md"]);
    }

    #[test]
    fn normalise_resolves_dot_dot() {
        assert_eq!(normalise("intro/../using/a.md"), "using/a.md");
        assert_eq!(normalise(".\\x\\y.md"), "x/y.md");
    }

    /// The canary for the check that matters: the two flag sets must be read
    /// from DIFFERENT bytes, or the comparison is a tautology that passes
    /// whatever the tree does.
    #[test]
    fn the_two_flag_sets_are_read_from_different_halves() {
        let src = "\
            const USAGE: &str = \"\n\
                --only-in-usage  does nothing\n\
            \";\n\
            if args.iter().any(|a| a == \"--real-flag\") { }\n";
        let parsed = flags_parsed(src);
        let usage = flags_in_usage(src);
        assert!(parsed.contains("--real-flag"), "{parsed:?}");
        assert!(!parsed.contains("--only-in-usage"), "{parsed:?}");
        assert!(usage.contains("--only-in-usage"), "{usage:?}");
        assert!(!usage.contains("--real-flag"), "{usage:?}");
    }
}
