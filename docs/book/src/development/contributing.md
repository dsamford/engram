# Contributing

## Before you push

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets
cargo xtask all
```

`cargo xtask all` runs `d3`, `c-deps`, `msrv`, `determinism`, `hygiene` and
`docs`. See [The gates](./gates.md).

## The one idea to absorb

> **A rule nothing checks is a preference.**

Everything else on this page follows from it. If you add a rule, add the thing
that fails when it is broken — and then break it once, deliberately, to prove
that thing works.

## Conventions

### Comments explain *why*, at the site

This codebase's comments are unusually long and that is deliberate. They record
the reasoning, the alternative that was rejected, and — often — the incident
that motivated the code.

Compare:

```rust
// Take the lock before reading.
```

with what is actually there:

```rust
// Under snapshot isolation the read returns the transaction's snapshot
// instead, EVERY contender's guard passes, and ownership/tenancy/governance
// writes lose updates silently — no error, no log line, just the last writer
// winning a race nobody knew ran.
```

The second survives a reader who is considering changing it.

**A comment that goes stale is a bug.** Two were found while writing this
documentation: a test module doc describing a guarantee as unbuilt long after it
shipped, and a constant's doc saying "default 4" where the constructor set 8.

### Emphasis marks the load-bearing sentence

Capitals and bold mark the claim a reader must not miss — *"the tail is strictly
newer than every segment"*, *"a Skip is a gap in the harness, never a pass"*.
Used sparingly, they work.

### Name the failure, not the mechanism

`hop_count_memo_keys_on_two_clocks.rs`,
`without_serialisable_autocommit_concurrent_increments_are_lost`,
`a_second_acquire_is_REFUSED_and_names_the_holder`.

A test named after what breaks tells the next reader what they broke.

### Refuse rather than guess

Throughout: a bounded mechanism that gives up cleanly with a correct fallback
behind it. A columnar scan **declines** past its budget. A span read **declines**
rather than truncating. A recogniser **declines** an unfamiliar shape.

Refusing is cheap and the general path is always correct. Guessing is not.

### State absences

Feature lists imply the rest exists. [Known limits](../known-limits.md) is
written as absences for that reason, and `--help` says the server has no
authentication.

## Adding a mechanism

The expected shape:

1. **Add a lever.** A `--no-*` flag or an env toggle, so the mechanism has a
   control. This is why there are forty flags and why most are not settings.
2. **Write the test, then break it.** Establish that it fails without the
   mechanism — "proven to bite".
3. **Pair the guarantee with its negative** where it is load-bearing: one test
   asserting the behaviour, one demonstrating the loss with the lever off.
4. **Add a counter** if the mechanism has a frequency worth knowing.
5. **Register anything new in D3** — crash points, `sometimes!` events,
   counters. `cargo xtask d3` checks all four categories.
6. **Predict, then measure**, with both arms in the same window. See
   [Benchmarking](./benchmarking.md).

## Adding a gate

Two canaries, always:

- a **positive** one proving it still catches what it is for;
- a **negative** one proving it has not started matching everything.

An over-matching gate is not a safe failure. And report **what was scanned**,
failing when your own reach is implausible — a gate that walked zero files
prints "no findings" in exactly the same words as one that walked all of them.

## Touching a frozen format

The key encoding, the value tags, the WAL header and the commit-log hash rule
are **frozen**. They are pinned by golden byte-level tests, so a change is a
visible diff plus a failing test rather than an accident.

If you believe one must change, it is an ADR, not a commit. See
[ADR-001](./adr-001-on-disk-formats.md).

## Documentation

The book is `docs/book`, and `cargo xtask docs` checks it: no orphan pages, no
dead links, and the CLI reference and `--help` both agreeing with what
`main.rs` parses — **in both directions**.

Two things worth knowing if you write a page:

**Run what you document.** Writing this book against a live server caught two
errors that reading the source had not: `CALL dbms.components()` returns no
rows without `YIELD … RETURN`, and explicit-null versus absent properties are
indistinguishable at the query level.

**Check the code, not the design notes.** Several `docs/` documents are dated
plans, and the tree has moved past some of them. Three claims in early drafts of
this book came from design documents and were wrong: that morsel parallelism did
not exist, that frontier-BFS expansion was unimplemented, and that the engine
loses on analytical queries.

## Style

- `rustfmt` defaults. **Note:** the tree is not currently rustfmt-clean — 177
  files differ — so `cargo fmt --all --check` is not in CI. Format what you
  touch; the whole-tree reformat belongs in its own commit.
- No `unsafe`. The workspace denies it.
- `BTreeMap`/`BTreeSet`, not `HashMap`/`HashSet` — denied types, because
  iteration order must be deterministic.
- Time from the injected clock, never `Instant::now`.
- No `thread::spawn` outside `engram-server`.

## Adding a dependency

A high bar. 39 today, all permissively licensed, no copyleft.

- `cargo deny check` must pass — allow-listed licences, advisories, banned
  crates.
- **One `cc` invocation at most.** No cmake, no bindgen, no external shared
  library, no build-time process. `cargo xtask c-deps` enforces it.
- No `reqwest` or `tonic` in an engine crate. Network adapters belong in adapter
  crates, outside the simulation envelope.

## Security

Report privately through the repository's Security tab, not a public issue. See
[Security posture](../using/security.md) for scope and response times.

## Next

- [The gates](./gates.md) — what must pass.
- [Testing](./testing.md) — how the suite is organised.
- [Architecture decision records](./adr.md) — how a frozen decision is recorded.
