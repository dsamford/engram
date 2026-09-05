# Testing

About **1,700 test functions across 276 integration test files**, plus the
vendored openCypher TCK and a deterministic simulation sweep.

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

The full suite links 276 binaries and moves real data. Raise the optimisation
level and keep the assertions:

```sh
CARGO_PROFILE_TEST_OPT_LEVEL=2 CARGO_PROFILE_TEST_DEBUG_ASSERTIONS=true \
  cargo test --workspace
```

## How it is organised

One file per **property or behaviour**, not one per module. `engram-graph` has
196 such files, and the names are sentences —
`chain_count_folds_to_degrees.rs`, `hop_count_memo_keys_on_two_clocks.rs`,
`merge_lost_race_binds_the_winner.rs`.

| crate | integration files |
|---|---|
| `engram-graph` | 196 |
| `engram-store` | 28 |
| `engram-server` | 19 |
| everything else | 1–5 each |

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

**3,772 of 3,773 evaluated scenarios pass** — 99.97%.

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
which is what makes "CI-ratcheted" true rather than aspirational.

The single failure is a time-zone-database expectation where this engine is
arguably the more correct of the two.

A second arm runs with `ENGRAM_TCK_PRECISION_LOCKING=1`, since that flag changes
which statements commit.

## The book's claims are tested

Three examples under `crates/engram-graph/examples/` assert what the
documentation says, so a page that goes stale fails a build rather than a
reader:

| example | asserts |
|---|---|
| `first_graph` | every result the tutorial page prints, including that `DISTINCT` collapses the two routes and `count(r)` gives 0 for a node with no matches |
| `documented_gaps` | every documented refusal still refuses — `=~`, `UNION` in `CALL {}`, the standalone `CALL` that yields nothing, and null-versus-absent |
| `row_budget_and_folds` | that 2,500 rows are refused under a 100-row budget and `count(*)` answers them anyway |

`documented_gaps` is the one worth understanding. A **gap** page rots in the
more embarrassing direction: a limitation quietly fixed leaves the
documentation telling people not to use something that works. So the refusals
are asserted as refusals, and each failure message names the pages to update.

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
- **No property-based testing** — no `proptest`, no `quickcheck`.
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
