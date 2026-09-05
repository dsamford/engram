# How Engram is measured

Performance claims here come with the corpus, the host shape and the command
that produced them, or they do not appear. This page is the discipline that
makes the numbers on the other pages worth reading, and then the current
standing.

## The rules

Every one of these exists because it was violated once and the resulting number
was wrong.

1. **One change per measurement.** A run that moves two things attributes
   neither.
2. **A lever per mechanism.** Nearly every optimisation has a `--no-*` flag or
   an environment toggle, so an A/B has a real control rather than a different
   binary. This is why the [CLI reference](../reference/cli.md) has forty flags
   and why most are explicitly not settings.
3. **A canary proven to bite, before the A/B.** A test that would pass either
   way measures nothing, so the instrument is broken deliberately first and
   required to fail.
4. **Predictions recorded before the run**, in the measuring script. A number
   explained afterwards explains anything.
5. **Same-window pairs.** A "before" measured on one machine and an "after" on
   another is not a comparison. Both arms run in one session, from fresh copies
   of the same snapshot, because write profiles mutate the store.
6. **N = 3, publish the median**, and report percentiles rather than means.
   Sub-100 µs medians swing ±30% on small samples; judge by the ms-scale
   profiles.
7. **Throttle counters bracket every run.** A throttled run is reported as
   throttled.
8. **The harness self-verifies.** Hot-locality profiles reconcile acknowledged
   writes against the hot counter and **fail** the run on loss — a throughput
   number over lost updates never prints as a pass.

The recurring failure this guards against is the *silent instrument*: a
harness that measures nothing and reports success. That has happened here
repeatedly — a probe whose cost was excluded from the reported time, a copy
that wrote nothing, an audit that skipped the site it appeared to clear. Each
was converted into a loud failure rather than fixed quietly.

## The current standing

**Against Neo4j 5.26-community on LDBC SNB SF1, in same-window paired runs on
one host, N = 3 medians, with identical answer counts on every query: Engram
leads on all nineteen workloads.**

### LSQB — analytical subgraph queries

| query | engram (ms) | Neo4j (ms) | ahead |
|---|---:|---:|---|
| q1 | 266 | 9,284 | **34.9×** |
| q2 | 886 | 1,716 | **1.94×** |
| q3 | 5,614 | 14,171 | **2.5×** |
| q4 | 584 | 14,948 | **25.6×** |
| q5 | 2,176 | 10,588 | **4.9×** |
| q6 | 658 | 35,646 | **54.2×** |
| q7 | 3,328 | 16,477 | **5.0×** |
| q8 | 2,647 | 37,825 | **14.3×** |
| q9 | 5,086 | 53,282 | **10.5×** |

Answer counts are identical to Neo4j's on every query, and independently to an
oracle implementation (`lsqbref`) that computes the same nine counts without
going through the engine.

### Stress — ten mixed profiles, at one and eight clients

| profile | @1 | @8 |
|---|---|---|
| read-only | 1.46× | 1.35× |
| read-heavy | 1.35× | 1.45× |
| balanced | 1.82× | 1.58× |
| write-heavy | 3.94× | 1.58× |
| write-only | 5.64× | 1.41× |
| contention | 1.39× | 1.46× |
| rel-create | 8.21× | 6.71× |
| rel-hub | 9.00× | 7.03× |
| unique-create | 10.76× | 5.54× |
| delete-churn | 8.25× | 4.88× |

Both suites passed with zero errors on both sides.

### With morsel parallelism

Measured separately, at width 6, N = 3 medians, interleaved arms, counts
identical across all 54 measurements: q2 5.6×, q9 5.8×, q6 5.0×, q5 4.0×,
q8 3.8×, q4 1.8×, with q1/q3/q7 flat. The single-threaded verdict above stands
on its own and is not combined with these.

Note where the win landed — on the queries the count fold *drives*, not where
the prediction put it. The prediction was recorded before the run, which is how
anyone can tell.

## What these numbers do not say

- **SF1 only.** SF10 and SF100 have not been run. These margins are not
  demonstrated to be scale-free, and the project's own plan says not to quote
  them as if they were.
- **Neo4j 5.26-community**, not Enterprise.
- **The SNB Interactive suite has not been re-baselined.** Its last numbers
  predate the memos, the estimator and clause fusion, and several were losses.
  Treat IC as unmeasured rather than won or lost.
- **Kuzu and LadybugDB have not been measured head to head.** A smoke pass
  exists and is explicitly not a measurement, so no number from it is quoted
  anywhere in this book.
- **A comparison is against one configuration**, not against a tuned one.

## Why the q2 story is the useful one

q2 is the narrowest margin at 1.94×, and it began at **14.6× behind**. The path
from one to the other was three separate defects — a join order driven from the
wrong side, a repeated estimation, and an unpriced first estimation — and
**none of them was in the executor**.

Three wrong theories died to instruments before the right one was found. That
is the argument for the discipline at the top of this page: the fix was not
where reasoning about the architecture said it would be, and only measurement
distinguished them.

## Reproducing a number

Every claim traces to a script and a corpus:

```sh
# Generate a corpus deterministically — same (persons, seed), same bytes
cargo run --release -p engram-bench --bin snbgen -- ./corpus 10000 42

# Load it
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687

# The analytical battery
cargo run --release -p engram-bench --bin lsqb -- 127.0.0.1:7687 --json out.json

# The oracle, which computes the same counts without the engine
cargo run --release -p engram-bench --bin lsqbref -- ./corpus --expect counts.json

# The mixed-workload sweep
cargo run --release -p engram-bench --bin stress -- 127.0.0.1:7687 all 1,8 30
```

`snbgen` is deterministic: the same person count and seed produce a
byte-identical corpus, with no wall clock and no system randomness. That is
what makes a comparison repeatable rather than merely repeated.

> **On the corpus.** `snbgen` produces *synthetic data on an SNB-like schema*.
> It is **not** official LDBC Datagen output and must not be presented as LDBC
> results. Where a page says "SF1" it means the official corpus; where it says
> `snbgen`, it does not.

## Next

- [Benchmarking](../development/benchmarking.md) — the nineteen harness
  binaries and how to drive them.
- [Tuning guide](../reference/tuning.md) — turning a measurement into a change.
- [How Engram is different](../intro/how-its-different.md) — what the
  architecture predicts, against what was measured.
