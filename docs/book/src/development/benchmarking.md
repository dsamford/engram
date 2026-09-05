# Benchmarking

Nineteen binaries in `engram-bench`, and a discipline that matters more than any
of them.

[How Engram is measured](../measurements/index.md) is the rules and the current
standing. This page is the tooling.

## The house rules

Every one exists because it was violated once and the number was wrong.

1. **One change per measurement.** A run that moves two things attributes
   neither.
2. **A lever per mechanism.** Nearly every optimisation has a `--no-*` flag or
   an env toggle, so an A/B has a real control rather than a different binary.
   This is why the [CLI reference](../reference/cli.md) has forty flags and why
   most are explicitly not settings.
3. **A canary proven to bite, before the A/B.** Break the instrument
   deliberately and require it to fail first.
4. **Predictions recorded before the run**, in the measuring script. A number
   explained afterwards explains anything.
5. **Same-window pairs.** A "before" on one machine and an "after" on another is
   not a comparison. Both arms in one session, from fresh copies of the same
   snapshot, because write profiles mutate the store.
6. **N = 3, publish the median**, and report percentiles rather than means.
   Sub-100 µs medians swing ±30% on small samples.
7. **Throttle counters bracket every run.** A throttled run is reported as
   throttled.
8. **The harness self-verifies.** Hot-locality profiles reconcile acknowledged
   writes against the hot counter and **fail** on loss — a throughput number
   over lost updates never prints as a pass.

## The recurring enemy

**The silent instrument**: a harness that measures nothing and reports success.
It has happened here repeatedly, and each time was converted into a loud
failure:

- A benchmark set its A/B toggles on a **temporary loading graph** rather than
  the serving one. They were silently discarded, both arms ran the same engine,
  and the numbers came out within 2% of each other — which reads exactly like
  "the fix does nothing".
- A battery reported `q3 4.5 s` while the query took minutes: the harness ran an
  **existence probe before every count** and reported only the count's
  milliseconds.
- A copy step wrote **nothing** because of a path-parsing quirk, and the guard
  that refuses to measure phase 2 without phase 1's artifact is what surfaced
  it.
- A thread dump showing every worker idle was a dump of the **wrapper process**,
  from a pattern that matched itself.

If a measurement surprises you, suspect the instrument before the engine. Three
wrong theories died to instruments in the campaign that produced the current
numbers.

## Making a corpus

```sh
cargo run --release -p engram-bench --bin snbgen -- ./corpus 10000 42
```

`snbgen <out_dir> <persons> [seed]` — deterministic, dependency-free. The same
`(persons, seed)` reproduces the graph **byte for byte**: a SplitMix64 stream
drives every choice, with no wall clock and no system randomness.

Roughly 50 nodes and 224 relationships per person.

> Output is *synthetic data on an SNB-like schema*. **Not** official LDBC
> Datagen output, and not presentable as LDBC results. Use `datagen2jsonl` to
> convert real Datagen output.

## Loading

```sh
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687 --neo4j
```

**One loader for both engines**, deliberately: two engines loaded by two
different paths differ by more than their engines — property types, label sets,
an index one side has. Every one of those shows up later as a performance
difference and gets attributed to the query engine.

## The batteries

### `lsqb` — analytical subgraph queries

```sh
cargo run --release -p engram-bench --bin lsqb -- 127.0.0.1:7687 \
    --json out.json --timeout-secs 120 --expect counts.json
```

Nine queries. `--expect` checks the counts against a canon, because a fast wrong
answer is not a result.

### `lsqbref` — the oracle

```sh
cargo run --release -p engram-bench --bin lsqbref -- ./corpus --expect counts.json
```

Computes the same nine counts **without going through the engine**. An
independent implementation is the only thing that catches a battery and an
engine agreeing on a wrong answer.

### `stress` — mixed read/write profiles

```sh
cargo run --release -p engram-bench --bin stress -- 127.0.0.1:7687 all 1,8 30 \
    --seed 424242 --json out.json
```

Ten profiles — read-only, read-heavy, balanced, write-heavy, write-only,
contention, rel-create, rel-hub, unique-create, delete-churn — at one and eight
clients.

**It self-verifies.** Hot-locality profiles reconcile acknowledged writes
against the hot counter and fail the run on loss.

### `snbconc` — concurrency over a query file

```sh
cargo run --release -p engram-bench --bin snbconc -- 127.0.0.1:7687 queries.txt 1,4,8 30 20
```

## Attribution tools

When a battery says *what* moved, these say *why*.

| binary | question |
|---|---|
| `balattr` | which lever accounts for a balanced-profile change — `--lever name=on\|off` across fourteen named mechanisms |
| `portbench` | the port battery, with an `ENGRAM_PORT_*` matrix of mechanism arms |
| `qcompare` | two servers, same statements, compared |
| `qcheck` | answers against expectations |
| `decoded`, `recdecode` | decoded values, and where record decoding spends time |
| `projscan` | projection and scan costs in isolation |
| `boltfloor` | the wire's own floor, with no engine work |
| `riskrepro` | reproduce a specific risk scenario |
| `compat` | parse-rate over a statement corpus, bucketed by failure reason |

`boltfloor` is the one people forget: it establishes what the protocol costs
before any engine work, so a "slow query" can be checked against the floor.

`compat` deliberately reports **failures bucketed by reason**, because a bare
percentage hides which features are missing — the only actionable content.

## Utilities

| binary | |
|---|---|
| `cq` | the smallest possible Bolt client: `cq <addr> <statement>…`, rows one per line |
| `portserve` | serve an exported corpus |
| `snbload`, `snbgen`, `datagen2jsonl` | corpus tools |

`cq` exists because a harness can only run its own profiles, and investigating a
finding needs the ability to ask a question the harness did not think of. Most
of the verified output in this book was produced with it.

## Interpreting a result

**Predict first.** Write it in the script.

**Check the counters, not just the time.** The
[counters](../reference/observability.md) say whether the mechanism you changed
actually ran — `adj_built` versus `adj_repaired`, `stale_served` versus
`stale_declined`, the `won@N` distribution.

**Confirm the arm ran.** A fired-counter canary proving the lever took effect is
the difference between an A/B and two identical runs.

**Report what did not move.** The morsel-parallel result named q1, q3 and q7 as
**flat**, and that was the informative part: the win landed where the count fold
drives, not where the prediction put it.

## Next

- [How Engram is measured](../measurements/index.md) — the rules and the
  standing.
- [Tuning guide](../reference/tuning.md) — turning a measurement into a change.
- [Loading data at scale](../using/bulk-loading.md) — the loaders in production
  use.
