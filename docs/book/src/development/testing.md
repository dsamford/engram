# Testing

About **3,100 test functions across 500 integration test files**, plus the
vendored openCypher TCK and a deterministic simulation sweep. The most recent
full workspace run: **3,081 passed, 0 failed, 23 ignored.** The counts move
with every feature, so they are rounded and re-derived rather than maintained:
`ls crates/*/tests/*.rs | wc -l` and
`grep -rn '#\[test\]' --include=*.rs crates/ | wc -l` are what produce them.

Stock `cargo test`. No `criterion`, no `proptest`, no `benches/` directories —
performance is measured by the [benchmark harness](./benchmarking.md) instead,
where the discipline can be enforced.

## Running

```sh
cargo test --workspace                          # everything
cargo test -p engram-graph                      # one crate
cargo test -p engram-graph --test adjacency_cost_repair
cargo test -p engram-tck --test baseline -- --nocapture   # conformance
```

The full suite links roughly 500 binaries and moves real data. Raise the
optimisation level and keep the assertions:

```sh
CARGO_PROFILE_TEST_OPT_LEVEL=2 CARGO_PROFILE_TEST_DEBUG_ASSERTIONS=true \
  cargo test --workspace
```

## How it is organised

One file per **property or behaviour**, not one per module. `engram-graph` has
369 such files, and the names are sentences —
`chain_count_folds_to_degrees.rs`, `hop_count_memo_keys_on_two_clocks.rs`,
`merge_lost_race_binds_the_winner.rs`.

| crate | integration files |
|---|---|
| `engram-graph` | 369 |
| `engram-store` | 39 |
| `engram-server` | 29 |
| `engram-bench` | 24 |
| `engram-cypher` | 13 |
| `engram-bolt` | 7 |
| everything else | 1–3 each |

`engram-cypher` earned its own row when the regex and trigram suites landed,
and `engram-bench` when the benchmark harness did: its files pin the statement
catalogue's digests, the golden statement and op-sequence files, the
quotability rules, the PostgreSQL wire client and the checked-in regression
baselines. See [Benchmarking](./benchmarking.md).

Unit tests live in `#[cfg(test)] mod tests` inside the source, near what they
test.

## The rules that make the suite worth having

### A test must be able to fail

The recurring phrase in this codebase is **"proven to bite"**: before a test is
trusted, the thing it guards is deliberately broken and the test is required to
fail.

You see it in commit notes as *"a one-row-too-many cut fails 4 of 12"* — the
canary establishing that the twelve tests were actually checking the property
they claimed to.

A test that would pass either way measures nothing.

### Pair a guarantee with its negative

Load-bearing behaviour comes in twos: one test asserting the guarantee, one
demonstrating the loss with the mechanism turned off.

`hot_key_updates.rs` is the model —
`concurrent_autocommit_increments_of_one_node_all_land` beside
`without_serialisable_autocommit_concurrent_increments_are_lost`, and the same
pattern for the entity latch.

### An instrument assertion is not a correctness assertion

Concurrency tests assert both, and the distinction matters when one fails.

`review_fence_hammer` checks correctness — every settled table equals the store
row for row, the change log is unpoisoned — *and* separately that the race it
claims to have run actually ran: readers took the table path on at least half
their reads, and the fence actually clamped something.

When CI failed it with *"readers did not take the table path"*, every
correctness assertion had passed. It is skipped in CI with the reason recorded,
because a two-core runner starves the readers; it passes locally, and it is the
test to run before trusting a change to the write fence.

### Skip cleanly, or not at all

Tests needing an external corpus skip when their directory variable is unset,
rather than failing for the wrong reason or silently passing.

The benchmark crate's live-engine tests follow the same rule and add a way to
make the skip fatal. The PostgreSQL wire client is tested against a real server
only when `ENGRAM_PGWIRE_TEST_ADDR` names one; every behaviour it asserts is
also asserted against a scripted backend that always runs; and
`ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1` turns a missing address from a skip into a
failure, wherever the live run is meant to happen.

### Cross-platform means passing for the right reason

A test can pass on one platform *because a feature is missing there*, and that
is a defect.

`dirlock.rs` has both shapes. One test says so in its doc comment — "this passes
for the right reason everywhere, rather than passing on Windows because takeover
is unimplemented there" — and the test beside it planted a lock naming a dead
pid, which Linux's stale-takeover path correctly reclaimed. It passed only where
the feature did not exist. Now it names a live process.

## The openCypher TCK

```sh
cargo test --release -p engram-tck --test baseline -- --nocapture
```

**3,769 of 3,773 evaluated scenarios pass** — 99.9%.

Each scenario runs in its own thread with a 5-second timeout, against a fresh
graph. The fixture uses a fixed wall clock so temporal scenarios are
deterministic.

### The integrity rule

Three outcomes, and the third is the important one:

| outcome | meaning |
|---|---|
| **Pass** | fully evaluated, the answer matched |
| **Fail** | fully evaluated, the answer did not match — an *engine* verdict |
| **Skip** | the harness cannot evaluate it — a gap in the **harness**, never a pass |

**The pass rate is `Pass / (Pass + Fail)`**, with Skips reported separately, so
the number cannot be inflated by scenarios that were quietly not checked. A
harness scoring its own blind spots as passes would measure nothing.

A timeout is a Skip; a panic is a Fail.

### The ratchet

`MIN_PASS = 3768`, `MAX_FAIL = 4`, asserted in the test. A regression fails CI,
which is what makes "CI-ratcheted" true rather than aspirational. The pass floor
sits one below the current count, for the one scenario that rides the
five-second timeout; the failure ceiling equals the current count, so a fifth
failure fails CI.

Of the four failures, three are scenarios in which the TCK expects a bare
pattern used as a value — in a `RETURN` or `WITH` projection, or on the
right-hand side of a `SET` — to be refused as a syntax error, and Engram accepts
the query. The fourth is a time-zone-database expectation where this engine is
arguably the more correct of the two.

`ENGRAM_TCK_PRECISION_LOCKING=1` runs the suite with precision locking on, which
changes which statements commit. **Nothing runs it automatically.** The
conformance job is the one command above and there is no second arm, so a
regression that appears only under precision locking is not currently gated. It
is a hand-run arm until a second CI step exists.

## The book's claims are tested

Three examples under `crates/engram-graph/examples/` assert what the
documentation says, so a page that goes stale fails a build rather than a
reader:

| example | asserts |
|---|---|
| `first_graph` | every result the tutorial page prints, including that `DISTINCT` collapses the two routes and `count(r)` gives 0 for a node with no matches |
| `documented_gaps` | that the behaviours the pages document around former gaps still hold — `=~` evaluates, `UNION` inside `CALL {}` answers, a standalone `CALL` returns the procedure's declared columns, `YIELD … RETURN` works, and an explicit null is indistinguishable from an absent property |
| `row_budget_and_folds` | that 2,500 rows are refused under a 100-row budget and `count(*)` answers them anyway |

`documented_gaps` is the one worth understanding. A **gap** page rots in the
more embarrassing direction: a limitation quietly fixed leaves the
documentation telling people not to use something that works. So the example
asserted every documented refusal *as* a refusal, with each failure message
naming the pages to update.

Three of them have since closed — `=~`, `UNION` inside `CALL {}` and the
standalone `CALL` all answer now — and each assertion was inverted rather than
deleted: it pins the behaviour, so if any of them stops answering, the example
fails and names the pages — [Getting started](../intro/getting-started.md),
[Cypher support](../using/cypher-support.md),
[Cypher procedures](../reference/procedures.md),
[Regular expressions](../reference/regex.md) and
[Known limits](../known-limits.md) among them — that would then describe a
feature the engine no longer has. The example and the book are meant to move in
one change rather than four.

Each example carries a `#[test]` that calls its own body, because
`cargo test --examples` **compiles** an example without running its `main` —
which would prove the snippet type-checks and nothing about whether it still
answers.

## The simulation sweep

```sh
cargo test -p engram-sim --test sweep
ENGRAM_SWEEP_SEEDS=500 cargo test -p engram-sim --test sweep
```

48 seeds by default. Each derives its own configuration *from the seed* — op
mix, key spread, seal cadence, crash arming — so the sweep explores the
configuration space as well as the schedule space.

It enforces a **coverage floor**: any declared `sometimes!` event that never
fires across the sweep **fails the sweep**. See
[Deterministic simulation](./simulation.md).

## Determinism

```sh
cargo xtask determinism
```

Two processes, one seed, one identical trace digest. See
[The gates](./gates.md).

## What is not tested

Stated as absences:

- **No fuzzing** of the wire protocol or the parser.
- **No property-testing framework** — no `proptest` and no `quickcheck`
  anywhere in `Cargo.lock`. Generated-input testing is hand-rolled instead,
  and only where a mechanism's failure mode is a *silent wrong answer*:
  `pipeline_reorder_review.rs` compares 8,000 generated `count(*)` patterns and
  8,000 generated `OPTIONAL` statements against the interpreter as oracle, and
  `a_symmetry_broken_count_agrees_on_random_graphs.rs` asks one count three ways
  — symmetry on, symmetry off, fold off — over 180 generated graphs. Both
  drive their generator from a fixed seed rather than from `rand`, so a
  failure reproduces exactly; the symmetry suite prints the seed behind it.
- **No fuzz or property testing of the examples** — they assert fixed shapes,
  not generated ones.
- **No multi-node testing**, because there is no multi-node.
- **No performance assertion in the test suite.** Deliberate: timing in a unit
  test is flaky, and performance belongs to the measurement lane where the
  discipline can be enforced.

## Next

- [Deterministic simulation](./simulation.md) — the sweep in detail.
- [The gates](./gates.md) — what runs alongside.
- [Benchmarking](./benchmarking.md) — where performance is measured.
