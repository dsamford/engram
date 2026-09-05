//! The durable gates.
//!
//! ```text
//! cargo xtask d3           every subsystem registers all four things
//! cargo xtask c-deps       the one-`cc`-invocation purity rule
//! cargo xtask msrv         every member inherits the declared MSRV
//! cargo xtask determinism  the same seed twice, byte-identical traces
//! cargo xtask hygiene      a property value cannot become a key component
//! cargo xtask docs         the book has no orphans, no dead links, and its
//!                          CLI reference agrees with the CLI
//! cargo xtask all          all of the above
//! ```
//!
//! # Why these are xtasks rather than lints or CI script lines
//!
//! R14: clippy config "can be bypassed", so enforcement needs "a disallowed-types
//! list **and** a durable xtask gate". A CI script line is worse still — it
//! lives in a file the repo does not compile, so it rots without anything
//! noticing, and a step that silently stopped running looks exactly like a step
//! that passes.
//!
//! # Every gate here reports what it SCANNED
//!
//! A gate that walked zero files prints "no findings" in the same words as one
//! that walked all of them. Each gate below therefore fails when its own reach
//! is implausible, rather than reporting a clean result it did not earn. That
//! failure mode is not hypothetical here: this repo has shipped an audit that
//! skipped the very site it appeared to clear, and a guard whose regex could
//! not match anything.

mod docs;
mod public_tree;
mod scrub;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let root = repo_root();

    let results: Vec<GateResult> = match task.as_str() {
        "d3" => vec![gate_d3(&root)],
        "c-deps" => vec![gate_c_deps(&root)],
        "msrv" => vec![gate_msrv(&root)],
        "determinism" => vec![gate_determinism(&root)],
        "hygiene" => vec![gate_hygiene(&root)],
        // The book's own gate: orphan pages, dead intra-book links, and the
        // CLI reference agreeing with the CLI in BOTH directions.
        "docs" => vec![gate_docs(&root)],
        // The scrub gate takes an optional PATH: the public tree is assembled
        // into a fresh directory by copy-allowlist, and the thing that must be
        // clean is that staged tree, not the working one.
        "scrub" => {
            let target = std::env::args().nth(2).map(PathBuf::from).unwrap_or(root);
            vec![gate_scrub(&target)]
        }
        // Assemble the publishable tree by copy-allowlist into a FRESH
        // directory, then scrub the result. See `public_tree.rs` for why the
        // allow-list is the control and the scrub is the proof.
        "public-tree" => {
            let Some(dest) = std::env::args().nth(2).map(PathBuf::from) else {
                eprintln!("usage: cargo xtask public-tree <dest dir>");
                return ExitCode::from(2);
            };
            let r = public_tree::run(&root, &dest);
            vec![GateResult {
                name: "public-tree",
                passed: r.passed,
                scanned: r.scanned,
                findings: r.findings,
            }]
        }
        // `all` deliberately does NOT include scrub. Until the tree is scrubbed
        // it fails by design, and a gate that is always red gets ignored — which
        // is how the one control protecting the release stops being read. It is
        // wired into the release lane, not the developer loop.
        "all" => vec![
            gate_d3(&root),
            gate_c_deps(&root),
            gate_msrv(&root),
            gate_determinism(&root),
            gate_hygiene(&root),
            gate_docs(&root),
        ],
        other => {
            eprintln!(
                "unknown task `{other}`; try: d3 | c-deps | msrv | determinism | hygiene | \
                 scrub [path] | all"
            );
            return ExitCode::from(2);
        }
    };

    let mut failed = false;
    for r in &results {
        println!("{}", r.render());
        if !r.passed {
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ─── Result plumbing ────────────────────────────────────────────────────────

struct GateResult {
    name: &'static str,
    passed: bool,
    /// What the gate actually looked at. Printed on success as well as failure,
    /// because "0 findings over 0 files" is the reading this exists to prevent.
    scanned: String,
    findings: Vec<String>,
}

impl GateResult {
    fn render(&self) -> String {
        let mut s = String::new();
        let mark = if self.passed { "PASS" } else { "FAIL" };
        let _ = writeln!(s, "[{mark}] {:<12} {}", self.name, self.scanned);
        for f in &self.findings {
            let _ = writeln!(s, "       - {f}");
        }
        s.trim_end().to_string()
    }
}

/// The documentation gate. See `docs.rs` for why the flag check runs in both
/// directions and why that is the one that finds real drift.
fn gate_docs(root: &Path) -> GateResult {
    let r = docs::run(root);
    GateResult {
        name: "docs",
        passed: r.passed,
        scanned: r.scanned,
        findings: r.findings,
    }
}

/// The publishability gate. See `scrub.rs` for why it byte-scans and why it
/// carries a negative canary as well as a positive one.
fn gate_scrub(target: &Path) -> GateResult {
    let r = scrub::run(target);
    GateResult {
        name: "scrub",
        passed: r.passed,
        scanned: r.scanned,
        findings: r.findings,
    }
}

fn repo_root() -> PathBuf {
    // The xtask crate sits one level under the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "target" || name == ".git" {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn member_manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["crates", "."] {
        let base = root.join(dir);
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            let m = e.path().join("Cargo.toml");
            if m.is_file() {
                out.push(m);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// ─── D3 — every subsystem registers all four things ─────────────────────────

/// Scan for `impl Subsystem for X` and require its `register` body to declare
/// in all four categories.
///
/// The type system already makes a gate-without-a-canary unrepresentable, and
/// `engram_observe::register` panics at construction on an empty category. This
/// gate covers what neither can: a subsystem whose `register` is never *called*
/// still compiles and still runs. Source-level is the right level for that
/// question — the failure is code that exists and is never reached, which no
/// runtime check can observe precisely because it is not reached.
fn gate_d3(root: &Path) -> GateResult {
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);

    // Fixtures under `tests/` are EXCLUDED. A deliberately-incomplete
    // `impl Subsystem` is how the coverage rules are tested, so counting one as
    // a defect would make the gate unable to coexist with its own test suite —
    // and the usual response to that is to delete the fixture, which is the
    // wrong half to lose. The exclusion is reported below rather than silent:
    // an unexplained gap between "impls found" and "impls checked" is how a
    // scope narrowing becomes invisible.
    let is_fixture = |p: &Path| {
        let s = slash_path(p);
        s.contains("/tests/") || s.contains("/benches/")
    };

    let mut impls = 0usize;
    let mut fixtures = 0usize;
    let mut findings = Vec::new();

    for f in &files {
        let Ok(text) = fs::read_to_string(f) else {
            continue;
        };
        let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
        let found = scan_d3(&text, &rel);
        let count = text.matches("impl Subsystem for ").count();
        if is_fixture(f) {
            fixtures += count;
            continue;
        }
        impls += count;
        findings.extend(found);
    }

    // ── The gate's own canary ────────────────────────────────────────────
    //
    // D3 requires every gate to ship a deliberate violation that MUST make it
    // fail. This gate is not exempt from its own rule. The fixture is embedded
    // rather than read from disk so the check cannot be defeated by a file
    // move, and it runs on EVERY invocation rather than in a separate lane
    // somebody can forget to wire up.
    //
    // Without it, a change to the needle strings — a rename from `.gate(` to
    // `.with_gate(`, say — would make this gate stop finding anything and
    // report a clean workspace in exactly the same words as a correct one.
    const CANARY_SRC: &str = "impl Subsystem for Canary {\n    fn register() -> Registration {\n        Registration::new().crash_point(\"x\")\n    }\n}\n";
    let canary_findings = scan_d3(CANARY_SRC, "<canary>");
    if canary_findings.len() != 3 {
        findings.push(format!(
            "SELF-TEST FAILED: the canary declares only a crash point, so this gate must \
             report 3 missing categories; it reported {}. The gate cannot detect a \
             violation, so its verdict on the workspace means nothing",
            canary_findings.len(),
        ));
    }

    // The integrity floor. A directory that moved produces "0 findings" from 0
    // impls, which is the shape of a pass.
    if impls == 0 {
        findings.push(
            "SCANNED NOTHING: no `impl Subsystem for` found in crates/*/src. Either no \
             subsystem exists yet, or this gate has stopped matching — and those two print \
             identically unless one of them is made an error."
                .into(),
        );
    }

    GateResult {
        name: "d3",
        passed: findings.is_empty(),
        scanned: format!(
            "{} file(s), {impls} Subsystem impl(s) checked, {fixtures} test fixture(s) \
             excluded, canary detected {} of 3",
            files.len(),
            canary_findings.len(),
        ),
        findings,
    }
}

fn slash_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// The D3 rule, over one file's text. Pure, so the gate can run it against its
/// own canary as well as against the workspace.
fn scan_d3(text: &str, rel: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for (idx, _) in text.match_indices("impl Subsystem for ") {
        let after = &text[idx..];
        let name: String = after["impl Subsystem for ".len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // The impl block, up to a line that closes an item at column 0.
        let body_end = after[1..].find("\n}").map_or(after.len(), |i| i + 3);
        let body = &after[..body_end];

        let required = [
            (".crash_point(", "crash points"),
            (".sometimes(", "sometimes! events"),
            (".counter(", "operation counters"),
            (".gate(", "gates"),
        ];
        for (needle, label) in required {
            if !body.contains(needle) {
                findings.push(format!(
                    "{rel}: `impl Subsystem for {name}` declares no {label} ({needle}…)"
                ));
            }
        }
    }
    findings
}

// ─── C-dependency purity ────────────────────────────────────────────────────

/// The one-`cc`-invocation rule, keyed on `links` / build-script presence.
///
/// NOT on a `-sys` or `cc` NAME match. The plan records why: `ring` and
/// `crc-fast` match neither pattern, so a name-based gate clears both while
/// appearing to have checked them — a gate that reports on a property it never
/// examined. The first version of this gate was exactly that, and its positive
/// control caught it: a planted `aws-lc-rs` was NOT detected, because cargo
/// rewrites `Cargo.lock` when the gate runs and the name list was all the gate
/// had.
///
/// What it keys on instead, per package:
///
///  - `links = "..."` in its manifest — a binding to a native library;
///  - a `build.rs` that drives a NATIVE TOOLCHAIN (a `cc` / `cmake` /
///    `bindgen` / `pkg-config` build-dependency, or those APIs in its source).
///
/// The second condition was narrower than "has a build script" only after the
/// first run: keying on build-script PRESENCE flagged `bytes`, `quote`, `syn`,
/// `tokio` and four more, all of which ship a pure-Rust `build.rs` that emits
/// cfg flags and touches no C at all. The rule is *one `cc` invocation, no
/// external library, no build-time process* — a Rust build script violates none
/// of it. Eight findings that are all correct-by-the-letter and wrong-by-the-
/// rule is how a gate gets muted, and a muted gate is worse than none.
///
/// Pure-Rust build scripts are COUNTED in the summary rather than ignored, so
/// the narrowing is visible to whoever reads the pass.
///
/// Everything is read from the EXTRACTED SOURCE in the cargo registry cache,
/// which is the same source cargo compiles. A package whose source cannot be
/// found is reported as UNRESOLVED rather than passed: a gate that could not
/// look is not a gate that found nothing.
fn gate_c_deps(root: &Path) -> GateResult {
    // Recorded exceptions, each with the reason it is acceptable. A bare name
    // list decays into "things someone once added"; the reason is what lets a
    // future reader re-decide instead of re-deriving.
    let allowed: BTreeMap<&str, &str> = BTreeMap::from([
        (
            "ring",
            "pre-generated assembly, one `cc`, no cmake/bindgen — the accepted level",
        ),
        ("blake3", "one `cc` for SIMD, no external library"),
        (
            "libmimalloc-sys",
            "the mimalloc C allocator — a FULL external C library, deliberately \
             BEYOND the one-`cc` rule. The owner-level exception (see \
             engram-server/Cargo.toml): a THREAD-CACHING allocator lifts the \
             concurrent multi-hop ceiling that a single-arena allocator's lock \
             imposes (foaf 32→18 collapse → 73→324, sys-time 51%→1%), and there \
             is NO battle-tested pure-Rust equivalent (ferroc is nightly-only and \
             abandoned). Linked only into the musl SERVER binaries (engram-server, \
             portserve); the CI musl build supplies its C compiler via `zig cc`.",
        ),
    ]);

    let Ok(lock) = fs::read_to_string(root.join("Cargo.lock")) else {
        return GateResult {
            name: "c-deps",
            passed: true,
            scanned: "Cargo.lock absent — nothing resolved yet".into(),
            findings: vec![],
        };
    };

    // (name, version) for every non-workspace package.
    let mut third_party: Vec<(String, String)> = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| -> Option<String> {
            block
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{key} = ")))
                .map(|l| {
                    l.trim()
                        .trim_start_matches(&format!("{key} = "))
                        .trim_matches('"')
                        .to_string()
                })
        };
        let (Some(name), Some(version)) = (field("name"), field("version")) else {
            continue;
        };
        // Workspace members have no `source`; they are ours to police elsewhere.
        if block
            .lines()
            .any(|l| l.trim_start().starts_with("source = "))
        {
            third_party.push((name, version));
        }
    }

    let sources = registry_src_dirs();
    let mut findings = Vec::new();
    let mut unresolved = Vec::new();
    let mut inspected = 0usize;
    let mut pure_rust_build_scripts = 0usize;

    for (name, version) in &third_party {
        let Some(dir) = sources
            .iter()
            .map(|d| d.join(format!("{name}-{version}")))
            .find(|d| d.is_dir())
        else {
            unresolved.push(format!("{name}-{version}"));
            continue;
        };
        inspected += 1;

        let manifest = fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
        let build_rs_path = dir.join("build.rs");
        let has_build_rs = build_rs_path.is_file()
            || manifest
                .lines()
                .any(|l| l.trim_start().starts_with("build = "));
        let build_src = fs::read_to_string(&build_rs_path).ok();

        match classify(&manifest, has_build_rs, build_src.as_deref()) {
            Purity::Clean => continue,
            Purity::PureRustBuildScript => {
                pure_rust_build_scripts += 1;
                continue;
            }
            Purity::Native(why) => {
                if allowed.contains_key(name.as_str()) {
                    continue;
                }
                findings.push(format!(
                    "`{name} {version}` has {why} — the one-`cc` rule allows one `cc` \
                     invocation and no external library, cmake, bindgen or build-time \
                     process. Add it to the allow-list WITH A REASON if it genuinely qualifies"
                ));
            }
        }
    }

    // ── The gate's own canary ───────────────────────────────────────────
    //
    // The first version of this gate keyed on a NAME LIST, and its positive
    // control could not detect a planted violation at all — cargo rewrites
    // Cargo.lock when the gate runs, so the plant never survived to be read.
    // A gate whose failure mode is unreachable by its own test is a gate
    // nobody has watched fail.
    //
    // Embedded fixtures instead: they need no network, no violating
    // dependency in the real graph, and cannot be rewritten out from under
    // the check. Every run proves the classifier still separates the three
    // cases before its verdict on the workspace is believed.
    for (label, expected, manifest, build_rs, src) in [
        (
            "links binding",
            "native",
            "[package]\nname = \"x\"\nlinks = \"z\"\n",
            false,
            None,
        ),
        (
            "cc build-dependency",
            "native",
            "[package]\nname = \"x\"\n[build-dependencies]\ncc = \"1\"\n",
            true,
            None,
        ),
        (
            "cc::Build in the source",
            "native",
            "[package]\nname = \"x\"\n",
            true,
            Some("fn main() { cc::Build::new().file(\"a.c\").compile(\"a\"); }"),
        ),
        (
            "pure-Rust build script",
            "pure",
            "[package]\nname = \"x\"\n",
            true,
            Some("fn main() { println!(\"cargo::rustc-check-cfg=cfg(loom)\"); }"),
        ),
        (
            "no build script at all",
            "clean",
            "[package]\nname = \"x\"\n",
            false,
            None,
        ),
    ] {
        let got = match classify(manifest, build_rs, src) {
            Purity::Native(_) => "native",
            Purity::PureRustBuildScript => "pure",
            Purity::Clean => "clean",
        };
        if got != expected {
            findings.push(format!(
                "SELF-TEST FAILED on `{label}`: expected {expected}, got {got}. The \
                 classifier no longer separates a native dependency from a pure-Rust \
                 build script, so its verdict on the workspace means nothing"
            ));
        }
    }

    // The integrity floor. Unresolved packages are the gate admitting it could
    // not look, which is the one thing it must never render as a pass.
    if !unresolved.is_empty() {
        findings.push(format!(
            "UNRESOLVED: {} package(s) whose extracted source was not found, so their \
             `links`/build-script status is UNKNOWN, not clean: {}",
            unresolved.len(),
            unresolved.join(", "),
        ));
    }

    GateResult {
        name: "c-deps",
        passed: findings.is_empty(),
        scanned: format!(
            "{} third-party package(s), {inspected} inspected for links + native-toolchain \
             build scripts, {pure_rust_build_scripts} pure-Rust build script(s) accepted, \
             {} allow-listed",
            third_party.len(),
            allowed.len(),
        ),
        findings,
    }
}

/// What one package's build surface amounts to.
#[derive(Debug, PartialEq, Eq)]
enum Purity {
    /// No build script, no `links`.
    Clean,
    /// A build script that touches no native toolchain. Accepted, and counted.
    PureRustBuildScript,
    /// A native library binding or a native-toolchain build script.
    Native(String),
}

/// The one-`cc` rule, over one package's manifest and build script.
///
/// Pure, so the gate can run it against embedded fixtures as well as against
/// the registry — the same shape as `scan_d3`. A gate that cannot be pointed at
/// a known violation is a gate nobody has watched fail.
fn classify(manifest: &str, has_build_rs: bool, build_src: Option<&str>) -> Purity {
    let links = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("links = "))
        .map(|l| {
            l.trim()
                .trim_start_matches("links = ")
                .trim_matches('"')
                .to_string()
        });

    // Two independent signals, because either alone can be absent: a crate may
    // declare `cc` and drive it through a helper, or vendor the invocation
    // inline with no build-dependency at all.
    const TOOLCHAIN: [&str; 4] = ["cc", "cmake", "bindgen", "pkg-config"];
    let declares = manifest
        .split("[build-dependencies]")
        .nth(1)
        .map(|section| {
            section
                .lines()
                .take_while(|l| !l.trim_start().starts_with('['))
                .any(|l| {
                    let key = l.split(['=', '.']).next().unwrap_or("").trim();
                    TOOLCHAIN.contains(&key)
                })
        })
        .unwrap_or(false);
    let uses = build_src
        .map(|src| {
            src.contains("cc::Build") || src.contains("cmake::") || src.contains("bindgen::")
        })
        .unwrap_or(false);
    let native_toolchain = has_build_rs && (declares || uses);

    match (links, native_toolchain, has_build_rs) {
        (Some(l), true, _) => Purity::Native(format!(
            "links = \"{l}\" AND a native-toolchain build script"
        )),
        (Some(l), false, _) => Purity::Native(format!("links = \"{l}\"")),
        (None, true, _) => Purity::Native("a build script that drives a native toolchain".into()),
        (None, false, true) => Purity::PureRustBuildScript,
        (None, false, false) => Purity::Clean,
    }
}

/// Every directory holding extracted package sources for this gate to read.
///
/// # Why this takes an override
///
/// The registry cache is populated as a SIDE EFFECT of building: cargo extracts
/// a crate when something compiles against it. That makes the gate's reach a
/// property of the host rather than of the lock file — on Linux, every
/// `windows-*` and `wasi` package in `Cargo.lock` is never built, so it is
/// never extracted, and the gate correctly reports it UNRESOLVED rather than
/// clean. Twenty-one of thirty-nine packages, on a runner that had just built
/// the whole workspace.
///
/// `ENGRAM_CDEPS_SRC_DIRS` (a path-separated list) is how a caller supplies
/// every package deterministically — `cargo vendor --versioned-dirs` produces
/// exactly this layout for the FULL lock file, every target, no build required.
/// The override is additive: it never hides what the local cache holds.
fn registry_src_dirs() -> Vec<PathBuf> {
    let mut extra: Vec<PathBuf> = std::env::var_os("ENGRAM_CDEPS_SRC_DIRS")
        .map(|v| std::env::split_paths(&v).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    extra.extend(registry_cache_dirs());
    extra
}

/// Every `registry/src/<index>/` directory cargo may have extracted into.
fn registry_cache_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let Some(home) = home else { return Vec::new() };
    let src = home.join("registry").join("src");
    let Ok(entries) = fs::read_dir(&src) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

// ─── MSRV ───────────────────────────────────────────────────────────────────

/// Every member must inherit the workspace MSRV, and it must be declared.
///
/// Risk C11: `rust-version` is advisory in most build paths, so a member that
/// quietly declares its own — or none — breaks for whoever pinned the minimum,
/// and breaks late. Inheriting keeps one value in one place; this gate is what
/// makes "inherits" true rather than customary.
fn gate_msrv(root: &Path) -> GateResult {
    let ws = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    let declared = ws
        .lines()
        .find(|l| l.trim_start().starts_with("rust-version"))
        .and_then(|l| l.split('=').nth(1))
        .map(|v| v.trim().trim_matches('"').to_string());

    let mut findings = Vec::new();
    let Some(declared) = declared else {
        return GateResult {
            name: "msrv",
            passed: false,
            scanned: "workspace Cargo.toml".into(),
            findings: vec!["the workspace declares no `rust-version`".into()],
        };
    };

    let manifests = member_manifests(root);
    for m in &manifests {
        if m == &root.join("Cargo.toml") {
            continue;
        }
        let Ok(text) = fs::read_to_string(m) else {
            continue;
        };
        let rel = m.strip_prefix(root).unwrap_or(m).display().to_string();
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("rust-version"));
        match line {
            None => findings.push(format!("{rel}: declares no `rust-version`")),
            Some(l) if !l.contains("workspace") => findings.push(format!(
                "{rel}: declares its own `rust-version` instead of inheriting \
                 (`rust-version.workspace = true`) — two values drift, and the drift \
                 surfaces only on the toolchain nobody develops on"
            )),
            Some(_) => {}
        }
    }

    if manifests.len() < 2 {
        findings.push(format!(
            "SCANNED {} manifest(s) — too few to be a workspace check; the walk is broken",
            manifests.len()
        ));
    }

    GateResult {
        name: "msrv",
        passed: findings.is_empty(),
        scanned: format!(
            "declared {declared}, {} member manifest(s)",
            manifests.len()
        ),
        findings,
    }
}

// ─── Determinism ────────────────────────────────────────────────────────────

/// Run the determinism test binary twice and require identical output.
///
/// The check itself lives in `crates/engram-runtime/tests/determinism.rs`,
/// which prints the trace digest for a fixed seed. This gate runs it in two
/// separate PROCESSES: a same-process repeat would not catch anything seeded
/// per process, which is exactly how `RandomState` breaks reproducibility.
fn gate_determinism(root: &Path) -> GateResult {
    let run = |ord: &str| -> Option<String> {
        let out = std::process::Command::new(env!("CARGO"))
            .args([
                "test",
                "-p",
                "engram-runtime",
                "--test",
                "determinism",
                "--",
                "--nocapture",
            ])
            .env("ENGRAM_SEED", "424242")
            .env("ENGRAM_RUN", ord)
            .current_dir(root)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        text.lines()
            .find(|l| l.starts_with("DIGEST "))
            .map(|l| l.trim_start_matches("DIGEST ").trim().to_string())
    };

    let a = run("a");
    let b = run("b");

    match (a, b) {
        (Some(a), Some(b)) if a == b => GateResult {
            name: "determinism",
            passed: true,
            scanned: format!("seed 424242, two processes, digest {a}"),
            findings: vec![],
        },
        (Some(a), Some(b)) => GateResult {
            name: "determinism",
            passed: false,
            scanned: "seed 424242, two processes".into(),
            findings: vec![format!(
                "DIVERGED: {a} != {b}. One seed produced two runs — look for HashMap \
                 iteration, a direct clock read, float reduction order, or a dependency \
                 that spawns its own threads"
            )],
        },
        _ => GateResult {
            name: "determinism",
            passed: false,
            scanned: "seed 424242".into(),
            findings: vec![
                "the determinism test printed no DIGEST line. A gate that could not read \
                 its own measurement must FAIL: reporting a pass here would be certifying \
                 determinism it never observed"
                    .into(),
            ],
        },
    }
}

// ─── Keyspace hygiene ───────────────────────────────────────────────────────

/// Prove that a user property value CANNOT become a key component.
///
/// > A user property value must never appear in a sort-ordered key position.
///
/// An LSM sorts by key, so a plaintext value in a key IS order-preserving
/// encryption, sorting attack included, whether or not anyone called it that.
/// `engram_key::Structural` is sealed to make that unrepresentable — but a
/// sealed trait is a claim about the compiler, and this repo has shipped guards
/// that could not fire. So the gate COMPILES a violation and requires the build
/// to FAIL, then compiles a legitimate use and requires it to SUCCEED.
///
/// Both directions, deliberately. A gate that only checks the violation fails
/// passes trivially if the fixture stops compiling for an unrelated reason —
/// a typo, a missing dependency, a renamed crate — and reports the hygiene rule
/// as enforced on the strength of a build error about something else.
fn gate_hygiene(root: &Path) -> GateResult {
    let dir = root.join("target").join("hygiene-gate");
    let src = dir.join("src");
    let mut findings = Vec::new();

    if fs::create_dir_all(&src).is_err() {
        return GateResult {
            name: "hygiene",
            passed: false,
            scanned: "could not create the fixture directory".into(),
            findings: vec!["the gate could not run — which is not the same as passing".into()],
        };
    }

    let manifest = format!(
        "[package]\nname = \"hygiene-gate\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nengram-key = {{ path = {:?} }}\n\n[workspace]\n",
        root.join("crates").join("engram-key"),
    );
    let _ = fs::write(dir.join("Cargo.toml"), manifest);

    // NEGATIVE: a property value trying to become a key component.
    let violation = "use engram_key::Structural;
/// Stands in for a user-supplied property value.
pub struct PropertyValue(pub String);
impl Structural for PropertyValue {
    fn encode_into(&self, out: &mut Vec<u8>) { out.extend_from_slice(self.0.as_bytes()); }
}
fn main() {}
";
    // POSITIVE: the legitimate use, which must keep working.
    let legitimate = "use engram_key::{Realm, Structural};
fn main() {
    let mut out = Vec::new();
    Realm(1).encode_into(&mut out);
    assert_eq!(out, vec![0, 0, 0, 1]);
}
";

    let build = |source: &str| -> (bool, String) {
        let _ = fs::write(src.join("main.rs"), source);
        match std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet"])
            .current_dir(&dir)
            .output()
        {
            Ok(o) => (
                o.status.success(),
                String::from_utf8_lossy(&o.stderr).to_string(),
            ),
            Err(e) => (false, e.to_string()),
        }
    };

    let (violation_built, verr) = build(violation);
    if violation_built {
        findings.push(
            "A FOREIGN TYPE IMPLEMENTED `Structural`. The seal is off, so a user property value \
             can be placed in a sort-ordered key position — which is order-preserving encryption \
             with a sorting attack, however it got there."
                .into(),
        );
    } else if !verr.contains("Sealed") && !verr.contains("private") && !verr.contains("E0277") {
        // It failed, but possibly for an unrelated reason. Reporting that as a
        // pass is how a gate comes to certify a rule it never tested.
        findings.push(format!(
            "the violation failed to build, but NOT visibly because of the seal — the error \
             mentions neither `Sealed`, `private` nor E0277, so this gate cannot claim the \
             hygiene rule is what stopped it. First line: {}",
            verr.lines()
                .find(|l| l.contains("error"))
                .unwrap_or("(no error line)"),
        ));
    }

    let (legit_built, lerr) = build(legitimate);
    if !legit_built {
        findings.push(format!(
            "the LEGITIMATE fixture does not build, so the negative result above proves nothing: {}",
            lerr.lines().find(|l| l.contains("error")).unwrap_or("(no error line)"),
        ));
    }

    let _ = fs::remove_dir_all(&dir);

    GateResult {
        name: "hygiene",
        passed: findings.is_empty(),
        scanned: format!(
            "violation build={} (must fail), legitimate build={} (must pass)",
            if violation_built { "OK" } else { "refused" },
            if legit_built { "OK" } else { "FAILED" },
        ),
        findings,
    }
}
