# The gates

```sh
cargo xtask all           # d3, c-deps, msrv, determinism, hygiene, docs
cargo xtask scrub <dir>   # no forbidden token survives in a staged tree
cargo xtask public-tree <dir>   # assemble a publishable tree by copy-allowlist
```

Each gate is a program that fails a build, not a lint that can be `#[allow]`ed
and not a line in a CI script.

## Why xtasks rather than CI script lines

Two reasons, both learned rather than assumed.

**Clippy config is bypassable** — an `#[allow]`, a changed lint level, a build
that never runs clippy. So determinism needs *both* a disallowed-types list
**and** a durable gate.

**A CI script line lives in a file the repo does not compile**, so it rots
without anything noticing — and a step that silently stopped running looks
exactly like a step that passes.

## Every gate reports what it scanned

```text
[PASS] c-deps  39 third-party package(s), 39 inspected for links + native-toolchain
               build scripts, 33 pure-Rust build script(s) accepted, 3 allow-listed
[PASS] d3      368 file(s), 12 Subsystem impl(s) checked, 2 test fixture(s) excluded,
               canary detected 3 of 3
```

Those two lines are a recorded run, not today's reading. The counts move with
the tree — `Cargo.lock` now resolves 44 third-party packages, and the `d3` walk
now sees over 600 files under `crates/` — so run the gate for current numbers.
What does not move is the **shape**: every count is reported, and the second
number on each line is the reach.

That second number exists because **a gate that walked zero files prints "no
findings" in exactly the same words as one that walked all of them.**

So a gate here **fails when its own reach is implausible**. That is not
hypothetical: this repository has shipped an audit that skipped the very site it
appeared to clear, and a guard whose pattern could not match anything.

It bites in practice. On its first CI run, `c-deps` reported *39 packages, 0
inspected* and failed itself — the registry cache is populated as a side effect
of building, so a cold runner had extracted nothing. The fix was upstream of the
gate: vendor the lock file so the gate sees every package on any host.

## The gates

### `d3` — every subsystem registers

Checks that every workspace member implementing `Subsystem` declares all four
categories: crash points, `sometimes!` events, counters, and gate/canary pairs.

Twelve register: `blob`, `bolt`, `crypto`, `cypher`, `exec-operators`, `graph`,
`key-codec`, `commit-log`, `objstore`, `executor`, `replica`, `store`.

`Gate::new(name, first_canary)` is the only constructor, so **a gate without a
canary is unrepresentable**.

### `c-deps` — the one-`cc`-invocation rule

> A native dependency may need one `cc` invocation. Not cmake, not bindgen, not
> an external shared library, not a process at build time.

It keys on **`links` and build-script presence**, never on a `-sys` or `cc`
*name* match — `ring` and `crc-fast` match neither pattern, so a name-based gate
would pass them both while claiming to have checked.

It reads extracted package sources, and takes `ENGRAM_CDEPS_SRC_DIRS` so a
caller can supply the full lock file deterministically rather than depending on
what a build happened to extract.

### `msrv` — every member inherits the declared minimum

Checks that every member manifest declares the same `rust-version` — 18 of them
today, and the gate names the number it read on every run, so a member that
stopped being walked is visible rather than silently unchecked.

This is the *declaration* half. The other half is CI building on a toolchain
pinned to that version, which is the only thing that makes the number a
constraint rather than a comment.

### `determinism` — two processes, one seed, one digest

```text
[PASS] determinism  seed 424242, two processes, digest 5483cf2ea8e8fc46
```

**Two processes, not two runs in one**, because a process reuses its allocator
and address-space layout, and a determinism bug hiding behind either is exactly
the kind that ships.

This catches what no lint can see: hash iteration order, float reduction order,
a dependency spawning its own thread pool.

One honest caveat, recorded in the project's own notes: this workload is
`SimRuntime`-only and never touches the store, graph or Bolt, and the gate
compares two fresh runs to *each other* rather than to a pinned constant. It is
not a substitute for the simulation sweep, which does trace real store
operations.

### `hygiene` — a property value cannot become a key component

```text
[PASS] hygiene  violation build=refused (must fail), legitimate build=OK (must pass)
```

Compiles a **violating** program and requires the build to fail *for the seal's
reason*, then a **legitimate** one and requires it to pass.

**Both directions.** One alone would certify the rule on the strength of an
unrelated build error.

### `docs` — the book still describes this engine

Four checks:

| check | the promise |
|---|---|
| orphans | every page is reachable from `SUMMARY.md` |
| links | every intra-book link resolves |
| reference | the CLI reference names every flag the CLI parses |
| usage | `--help` names exactly the flags the CLI parses |

**The `usage` check is bidirectional on purpose.** "Every documented flag
exists" passes a reference documenting three of forty. Only the other direction
found the actual state: **eight flags parsed by `main.rs` and absent from its
own `USAGE`**, so `--help` was wrong about the program printing it.

The `reference` check has **one** direction — parsed, then looked for in the
reference. Stated as an absence: a row in the CLI reference for a flag the
binary does not parse is *not* caught here. That direction is why the shipped
reference could carry four flags belonging to a workstream whose code was not in
that tree without this gate objecting — they were neither parsed nor in `USAGE`,
so no comparison it makes had anything to disagree about.

Its own canary is that the two flag sets are read from **different halves** of
the file. A version reading both from one source would be a tautology passing
whatever the tree did.

**It runs on a tree that carries the CLI reference and nothing else**, because
that is what ships. A missing `SUMMARY.md` skips the orphan and link checks and
runs the flag check anyway — and says so on the line that reports what it
scanned (`no SUMMARY.md — chapter checks SKIPPED, flags only`), since a gate
that quietly narrows its scope and still prints PASS is the failure this gate
exists to catch. Refusing to run there is how the gate came to be absent from
every shipped revision while the shipped CLI reference documented four flags the
shipped binary does not have.

Each half then carries its own vacuity floor, because the two halves can now be
reached independently: a book that is present with fewer than two pages is a
finding, and so is fewer than ten flags on either side of the comparison. An
agreement between two empty sets is not an agreement.

### `scrub` — this tree can be published

A **byte** scan, not `read_to_string`. The tree contains binaries and archives,
and `read_to_string` returns `Err` on non-UTF-8 — the natural
`let Ok(s) = … else { continue }` then **skips exactly the files most likely to
carry an embedded private path**. A 28 MB unstripped binary links the paths it
was built from. So everything is read as bytes, and anything unreadable is a
**finding**, never a skip.

**The rules are a separate file, and it is not published.** `scrub-rules.txt`
lives outside `xtask/` and outside the publish allow-list, because the gate
itself ships. The rules did ship once — and that made the gate written to
prevent disclosure into the most concentrated disclosure in the tree: every
private name in one place, each annotated with what it is, better organised than
anything it catches. Moving them out created a new way to be wrong, so an
absent, unreadable, empty or rule-less file is a **finding**, proved by
`a_missing_rules_file_FAILS` rather than assumed. A side benefit: the gate can
now scan its own source, which it previously had to skip.

Every rule carries **two canaries**: a positive one proving it still catches
what it is for, and a **negative** one proving it has not started matching
everything. An over-matching rule is not a safe failure — `production` appears
hundreds of times in `crates/`, much of it the benign execution vocabulary
(*production order*), so a rule matching the bare word would demand edits to
correct code.

The positive canary is a **fixed literal, deliberately not generated from the
rules**. A canary built by joining the needles is worthless against the failure
that actually happens: someone edits a needle, the generated canary changes with
it, the self-test still passes, and the rule now matches nothing while reporting
a clean run.

### `public-tree` — assemble what ships

Copies an **allow-list** into a fresh directory, then scrubs the result.

An allow-list fails **closed** and a `.gitignore` fails **open**: anything added
after the list was written is left behind by default, which is the point. What
it carries is the workspace and toolchain configuration, the legal files, the
Rust sources and manifests under `crates/` and `xtask/`, the vendored TCK
features, the book, the CI workflows — and, because the benchmark crate does
not build or test without them, the harness's statement catalogue
(`crates/engram-bench/catalogue`), its golden files
(`crates/engram-bench/tests/golden`) and the checked-in regression baselines
(`measurements/baselines`). The rest of `measurements/` stays behind.

It **refuses a dirty working tree.** The gate copies a directory, not a commit,
so pointed at a working tree it would publish whatever happened to be open in an
editor, and an allow-list copy of a dirty tree prints the same PASS line as one
of a clean tree. Cut the publishable tree from a clean checkout of a commit
instead.

Every rule must match at least one file, and the gate fails when one does not —
because of its own first bug. It allow-listed `crates/engram-tck/tck`; the TCK
lives in `crates/engram-tck/features`; the missing directory was quietly
skipped. The gate reported a clean tree, the tree built, and its openCypher
conformance suite — the project's headline claim — **had no feature files at
all.**

> A rule that matches nothing looks exactly like a rule that is satisfied.

## Why `scrub` is not in `all`

Deliberately. Until a tree is scrubbed it fails by design, and **a gate that is
always red gets ignored** — which is how the one control protecting a release
stops being read. It belongs in the release lane, not the developer loop.

The same reasoning keeps `cargo fmt --check` out of CI: the tree is not
rustfmt-clean, so adding the check without the reformat would make the job
permanently red. The fix is one mechanical command in its own commit, with the
check added to CI in the same change, so that the tree and the gate become true
together. See [Contributing](./contributing.md).

## In CI

| job | what |
|---|---|
| build and test | `cargo test --workspace`, less `review_fence_hammer`'s heavy arm (which needs more cores than the runner has), then `cargo test --workspace --examples` |
| benchmark harness smoke | the harness and its lanes build; every catalogue parses and every family's digest is stable; parameter binding holds; a read battery still refuses to run without its parameters; the regression gate fires in the right direction; then one short stress level end to end against a fresh server, with the regression gate required to pass against itself and to fail against an injected faster baseline |
| xtask gates | `cargo xtask all` |
| publishable tree | `public-tree` then `scrub`, plus assertions on what shipped; skipped, and saying so, in a published snapshot, which does not carry the scrub rules |
| clippy | `--workspace --all-targets` |
| MSRV | a build on the declared minimum |
| supply chain | `cargo deny check` |
| openCypher conformance | the ratcheted TCK |
| documentation gate | `cargo xtask docs` |
| the book builds | `mdbook build docs/book` |
| the API reference builds | `cargo doc --workspace --no-deps`, with `-D warnings` |

The workflow also runs once a day on a schedule, so the benchmark harness is
exercised even when nothing is pushed: an instrument nobody runs drifts without
saying so. The smoke job is not a performance measurement — two clients for a
few seconds on a shared hosted runner — and its timings are never recorded or
quoted. What it proves is that the lanes still run, still stamp rig and
fairness, still write a document the reporter accepts, and that the regression
gate is still wired. See [Benchmarking](./benchmarking.md).

The `--examples` step is the one that keeps the book's runnable claims honest:
`cargo test --examples` compiles an example without running its `main`, so the
examples carry a `#[test]` that calls their own body and this step is what runs
it. See [Testing](./testing.md).

The last three are a second workflow, `docs.yml`. `cargo doc` with
`RUSTDOCFLAGS: -D warnings` is what makes the `missing_docs` and
`broken_intra_doc_links` denials real rather than declared. That workflow also
carries a Pages deploy that is written and **inert**, gated on a repository
variable: Pages published from a private repository is public on every plan
below Enterprise Cloud, so turning it on would publish the documentation of a
codebase `scrub` and `public-tree` exist to keep unpublished — the same
disclosure, arriving through a side door.

The publishable-tree job also asserts the assembled tree **carries** the book
and the workflows and **leaves behind** the private material — stated as a class
rather than a list of names, because the first version of that check named a
private directory and `scrub` failed the workflow file for spelling it.

## Next

- [Testing](./testing.md) — the suite the gates sit alongside.
- [Deterministic simulation](./simulation.md) — the coverage floor.
- [Contributing](./contributing.md) — what to run before a change.
