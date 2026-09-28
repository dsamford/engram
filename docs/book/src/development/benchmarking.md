# Benchmarking

The crate `engram-bench` holds the benchmark harness and the tools around it:
thirty-two binaries under `src/bin`, plus the crate's own `engram-bench`
target, and a discipline that matters more than any of them. Nothing gates that
count, so read the directory rather than this sentence if it matters.

Two binaries produce every published figure: **`harness`**, which runs the LSQB,
SNB BI, SNB Interactive, FinBench and stress workloads against Engram and Neo4j
over Bolt, or PostgreSQL over its own wire protocol, and **`graphalytics`**,
which runs the LDBC Graphalytics kernels on Engram. The current results are on
[Three engines at SF3 and SF10](../measurements/three-engines-sf3-sf10.md), and
the rules they were taken under are on
[How Engram is measured](../measurements/index.md). This page is the tooling.

The harness exists for a measurement reason. Before it, each engine had its own
driver, and each driver restated "the same nine queries" in its own file. A
count only catches a restatement that changes the answer; one that changes the
*work* — a missing `LIMIT`, a join written the expensive way round — leaves the
count identical and the timing wrong. So the harness keeps every statement once,
per dialect, in a catalogue every engine is driven from, behind one engine seam
(`backend.rs`) with a Bolt backend and a PostgreSQL wire-protocol backend.

## The house rules

Every one exists because it was violated once and the number was wrong.

1. **One change per measurement.** A run that moves two things attributes
   neither.
2. **A lever per mechanism.** Nearly every optimisation has a `--no-*` flag or
   an env toggle, so an A/B has a real control rather than a different binary.
   This is why the [CLI reference](../reference/cli.md) runs to nearly seventy
   flags and why most are explicitly not settings. It is a ratchet, not an accomplished fact:
   four levers shipped documented and reachable from nothing but a unit test,
   so the fixes behind them could not be A/B'd on a running server at all and
   their effect had been attributed by comparing two *binaries* — which credits
   the whole delta between two builds to one fix.
   `every_lever_is_reachable_from_the_server` now freezes the remaining debt at
   a named list, and the list can only shrink.
3. **A canary proven to bite, before the A/B.** Break the instrument
   deliberately and require it to fail first.
4. **Predictions recorded before the run**, in the measuring script. A number
   explained afterwards explains anything.
5. **Same-window pairs.** A "before" on one machine and an "after" on another is
   not a comparison. Both arms in one session, from fresh copies of the same
   snapshot, because write profiles mutate the store.
6. **Declare N, publish the median**, and report percentiles rather than means.
   Sub-100 µs medians swing widely on small samples. N is not fixed across the
   programme — a flag A/B may be several batteries an arm, a scale battery may
   be two, a concurrency sweep may be one run per cell — so the number is
   stated beside the result, along with what it cannot resolve, rather than
   assumed.
7. **Throttle counters bracket every run.** A throttled run is reported as
   throttled.
8. **The harness self-verifies**, and refuses to quote what it could not
   measure. Hot-locality profiles reconcile acknowledged writes against the hot
   counter and **fail** on loss — a throughput number over lost updates never
   prints as a pass. A level that reads 0.00 ops/s, or whose window was consumed
   by a single operation, is reported `NOT QUOTABLE` rather than as a rate,
   which is why several sweeps carry no throughput number at all instead of a
   bad one. Counts are asserted equal per query across the arms and against
   canon, and a battery that answers fewer queries than it names fails the run
   — averaged in, a missing query reads as a speed-up.
9. **Judge on separation, not on the ratio of medians.** Two arms whose run
   ranges overlap have measured nothing, however clean the ratio looks. A
   change is banked when the arms are disjoint — the slowest run of the faster
   arm beats the fastest run of the slower one — and refused otherwise. The
   rule is fixed before the run, not after it, which is what lets it retire a
   "gain" a ratio threshold would have banked.
10. **Matched execution widths, asserted in both directions.** A leaked flag
    makes both arms the control as surely as a dropped one makes both the
    treatment, and only a two-direction check catches both. One benchmark script
    that did not set the query parallelism its comparator's script did ran
    Engram's fold on one core against a multi-worker PostgreSQL and produced
    clean, plausible losses that were the rig. The tell was non-uniformity:
    contention inflates a battery roughly evenly, and a configuration
    difference does not. The harness now checks the declared width against
    what each engine reports about itself (see
    [Rig and fairness](#rig-and-fairness)).

## The recurring enemy

**The silent instrument**: a harness that measures nothing and reports success.
It has happened here repeatedly, and each time was converted into a loud
failure:

- A benchmark set its A/B toggles on a **temporary loading graph** rather than
  the serving one. They were silently discarded, both arms ran the same engine,
  and the numbers came out indistinguishable — which reads exactly like "the
  fix does nothing".
- A battery reported seconds for a query that took minutes: the harness ran an
  **existence probe before every count** and reported only the count's
  time.
- A copy step wrote **nothing** because of a path-parsing quirk, and the guard
  that refuses to measure phase 2 without phase 1's artifact is what surfaced
  it.
- A thread dump showing every worker idle was a dump of the **wrapper process**,
  from a pattern that matched itself.
- A server started without the derived structures it keeps on disk between
  runs, and built them inside the measured passes.

If a measurement surprises you, suspect the instrument before the engine. More
than one wrong theory in this project has died to the instrument rather than
the engine.

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

The published comparison uses LDBC's own data: SNB Datagen output converted by
`datagen2jsonl`, FinBench data converted by `finbench2jsonl` (or generated on
FinBench's schema by `fbgen`), and Graphalytics graphs converted by `ga2jsonl`.
All of them write the same `nodes.jsonl` / `rels.jsonl` / `meta.json` corpus, and
every engine is loaded from it. See
[Loading data at scale](../using/bulk-loading.md).

## Loading

```sh
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687 --neo4j
```

**One corpus for every engine**, deliberately: two engines loaded by two
different paths differ by more than their engines — property types, label sets,
an index one side has. Every one of those shows up later as a performance
difference and gets attributed to the query engine. `jsonl2neo4j` exists for
the size at which Neo4j is bulk-imported rather than loaded over Bolt, and it
reads the corpus through the same parse `snbload` uses.

## Parameters

Every LDBC read query is parameterised, and **a parameter is part of the
question**: a value that matches nothing produces a fast, well-formed, empty
answer that reads as a timing. `snbparams` derives parameters from the corpus
a server is holding, prints how many rows each value matches, and refuses any
that matches nothing:

```sh
cargo run --release -p engram-bench --bin snbparams -- 127.0.0.1:7687 \
    --family snb-bi --pick p95 --out params.json
```

`--pick p95`, the default, takes the value at the 95th percentile of cost — a
representative one, the way LDBC curates; `--pick max` takes the most
expensive, for probing a ceiling. The result is valid for that corpus and
comparable across engines on it; it is **not** LDBC's curated parameter set, so
a number measured with it must not be compared with a published LDBC figure.
The file says so.

## The harness

```text
harness lsqb             <addr> …   the nine LSQB counting queries
harness snb-bi           <addr> …   LDBC SNB Business Intelligence
harness snb-interactive  <addr> …   LDBC SNB Interactive (IS1–IS7, IC1–IC14)
harness finbench         <addr> …   LDBC FinBench
harness stress           <addr> …   mixed read/write profiles at rising client counts
harness report           <result.json> …   compare runs, or gate one against a baseline
harness family | catalogue | plan   inspect statements, dump the catalogue, emit a replayable plan
```

Run `harness` with no arguments for the full usage, the known rigs and the
stress profiles.

### The statement catalogue

Every statement the harness sends lives in `crates/engram-bench/catalogue/`,
once per dialect, and is compiled into the binary, so a copied binary carries
the catalogue it was built against:

| file | family | holds |
|---|---|---|
| `statements.json` | `lsqb-stress` | the nine LSQB queries, their expected counts, and the stress workload's shapes, writes and integrity probes — **frozen** |
| `snb-bi.json` | `snb-bi` | SNB Business Intelligence |
| `snb-interactive.json` | `snb-interactive` | SNB Interactive |
| `finbench.json` | `finbench` | FinBench's twelve complex reads |
| `graphalytics.json` | `graphalytics` | the six Graphalytics kernels, as procedures |

The dialects are `cypher` (Engram and Neo4j share one text), `sql` (PostgreSQL),
`cypher_engram` — Engram's own text for the few queries where no single Cypher
serves both Bolt engines, such as the weighted shortest paths of bi15, bi19 and
bi20, which Neo4j reaches through a library its Community edition does not ship
— and `cypher_ladybug`, for an embedded engine driven out of process, from which
no figures are published. Every entry carries a status: `verified` (run, and
its answer checked against another engine's), `unverified` (transcribed, never
run) or `unsupported`, with the reason the dialect cannot express the query. A
run records the dialect it used, so a row produced from an engine-specific text
says so.

**Each family has its own digest** — FNV-1a over the file's bytes — and every
result document carries the digests of every family the binary holds. The
reporter refuses to compare two runs whose digest for the family they share
differs, because that is a comparison of two catalogues and not of two
engines. Families are separate files so that adding a battery never changes
the digest an existing result was recorded under.

`statements.json` is frozen: `tests/the_frozen_lsqb_digest_is_pinned.rs` pins
its digest, so any edit at all — a key, a reflow, a trailing newline — fails a
test. It has moved once, deliberately, when prose notes inside it were reworded
for publication and no statement changed. That edit is **declared** rather than
absorbed: `catalogue::EQUIVALENT_DIGESTS` lists the (before, after) pair, the
reporter treats the two digests as the same statements in either order, and the
pair is exact, so any later edit matches nothing. Two golden files under
`tests/golden/` pin the rendered LSQB Cypher and the stress workload's
operation sequence byte for byte.

`harness family <name> --list` lists a family's queries, and
`harness family <name> --query Q --dialect D --param k=v …` prints one
statement as it would be sent, without connecting to anything. An unsupported
entry exits 3, and a statement with an unbound `${name}` placeholder exits 4
rather than printing something an engine would half-parse.
`harness catalogue --dump <path> [--family NAME]` writes a family's exact bytes
out for an out-of-process executor.

### The read batteries

```sh
harness snb-bi 127.0.0.1:7687 --params params.json \
    --rig RIG --thread-cap 40 --cache-mb 32768 --corpus sf3 --json bi.json
```

`snb-bi`, `snb-interactive` and `finbench` share one lane. `--params` is
required: the lane binds parameters rather than splicing them into the text,
so the engine plans the catalogue's own bytes, and it will not invent a value.
Each value is coerced to the type the catalogue declares for it. `--engine pg`
(with `--pg-user` and `--pg-db`) drives PostgreSQL; `--dialect` overrides the
engine's default dialect; `--queries` and `--variant` select; the ceiling is
`--timeout-secs`, 300 by default.

Each query records its row count and its time, and a status. A query that
returns **zero rows is `empty` and fails the run** — it is never `ok`, because
an empty answer is usually a parameter that matched nothing. A query past its
ceiling is abandoned by the client, not by the server, which may go on
computing it; everything measured afterwards would share the machine with it.
So the lane **stops at the first timeout**, and every later query is recorded
as `abandoned-upstream`. `--continue-after-timeout` measures them anyway and
records each as `<status>-contaminated`.

`harness lsqb` runs the nine LSQB queries the same way, one statement at a time
on a fresh connection with a deadline (120 s by default), and judges each count
against an existence probe and the catalogue's expected count for the corpus.

### Rig and fairness

Every run must say where and how it was taken, and the harness checks what it
can:

- **`--rig`** is required and has no default. It names a rig the harness knows,
  or describes one inline as `name:node_type:cores:quota|none:mem_mb`. The
  corpus scale is part of the rig, so an SF3 figure and an SF10 figure are
  never put in one table. The declaration is checked against what the process
  can observe — the CPUs online, the cgroup quota, the memory limit — and a
  contradiction stops the run. A machine that reports nothing about itself is
  recorded as `unobservable`: stated, not passed.
- **`--thread-cap`** and **`--cache-mb`** are required and have no defaults,
  because they are claims about a server the harness did not start, and a
  defaulted claim is a plausible description of a server that is not running.
  They are checked against the engine wherever it will answer: PostgreSQL
  always (`shared_buffers`, and one leader plus its parallel workers per
  gather), Engram from the serving hint it sends in `HELLO` (its installed
  query parallelism and cache budget), Neo4j through `dbms.listConfig`. Where
  the engine answers nothing, the figure is recorded as declared and not
  observed — which is not a pass. A contradiction stops the run.

`--allow-rig-mismatch` and `--allow-fairness-mismatch` take the measurement
anyway and record the disagreement in the document; the run then fails, and the
reporter refuses the row. That is the point: a wrong stamp compares, and an
absent one does not.

### Result documents

Every lane writes one JSON document (`--json OUT`) with one schema. Beside the
measurements — `queries[]` for the read batteries, `levels[]` for stress — it
carries the engine and its version, the dialect, the dataset and corpus, the
`rig` and the result of checking it, the `fairness` block (thread cap, cache
budget, clients, seconds) and the result of checking that, the whole-file
`catalogue_digest` and the per-family `catalogue_families` digests, the plan's
SHA-256 when a plan was replayed, the `integrity` findings, the `failures`, and
`pass`. Every qualification travels inside the document rather than in the
script that assembles a table, because the script is where caveats go to be
forgotten.

### `harness report` — tables and the regression gate

```sh
harness report neo4j.json engram.json postgres.json
```

With two or more documents, `report` builds a comparison table — or refuses
to, and exits 3. It refuses when any document carries no rig, or a rig or
fairness stamp that was checked and contradicted; and when the documents differ
in rig (including scale), in the digest of the catalogue family they share
(unless the pair is declared equivalent), in the fairness block, or, for
stress, in writes mode. A query whose counts differ between engines is marked
`COUNTS DIFFER` in the table and given no ratio.

With `--baseline`, it is a regression gate:

```sh
harness report run1.json run2.json --reproduce \
    --baseline measurements/baselines/snb-bi-sf3-cypher.json \
    --max-regression 25 --min-regression-ms 5
```

| flag | meaning |
|---|---|
| `--baseline BASE.json` | the document the candidate is gated against; one candidate document is then enough |
| `--max-regression PCT` | the relative tolerance, 10% by default |
| `--min-regression-ms MS` | a millisecond key must also be slower by more than `MS`; it passes a smaller slowdown whatever its ratio, and does not apply to throughput (ops/s) keys |
| `--reproduce` | every document named is a repetition of one candidate, and a key regresses only when every repetition regresses it; the figure shown is the best repetition's. Name repetitions of one binary only |

Comparability is asked first, and the same key bound to **different
parameters** in the baseline and the candidate is a different question, so
either refuses a verdict. A key the baseline measured and the candidate did not
run is a regression — a shorter table is not a greener one — and so is a key
that was quotable and no longer is. A key that was already failing in the
baseline is not, and a key only the candidate has is not: adding coverage never
fails the gate. Throughput keys regress downwards and millisecond keys upwards.

| exit | meaning |
|---|---|
| 0 | no regression |
| 4 | at least one regression, each named with both numbers |
| 3 | not comparable, no verdict: a different rig, fairness, catalogue text or bound parameters |
| 1 | a document that does not parse, or a floor that is not a number |

A refusal is the gate working: it means the candidate did not ask the
baseline's questions.

### The checked-in baselines

`measurements/baselines/` holds the documents a candidate is gated against:
Engram's recorded SNB BI (in the `cypher` and `cypher_engram` dialects) and SNB
Interactive passes at SF3, the two parameter files they bound, and a README
stating the rig, the fairness flags, the server settings, the protocol and the
gate command. A candidate is comparable only if it reproduces all of them.

The protocol matters as much as the flags: a freshly started server per family;
one full pass of the same query list, discarded; then two recorded passes. The
baseline records the first; a candidate gates both, with `--reproduce` and a
5 ms floor, because the harness times each query once per pass and some figures
move with what ran before them on the server.

`tests/the_checked_in_baselines_still_gate.rs` keeps the baselines usable as
the report format evolves: each must parse, carry at least the number of
quotable rows it was recorded with, carry the parameter binding of every
quotable row, compare cleanly with itself, and report no regression against
itself under the README's gate. A baseline that stopped parsing would otherwise
fail a scheduled gate as exit 1, which a lane reading only exit 4 as "regressed"
would carry on treating as green.

### Stress: profiles and plans

```sh
harness stress 127.0.0.1:7687 all 1,8,32,64 60 --writes multi \
    --rig RIG --thread-cap 40 --cache-mb 32768 --json stress.json
```

`<profile|all> <clients-csv> <seconds>`: each profile runs one level per client
count, each level for the given seconds. `all` is the ten profiles below, in a
fixed order — the order is load-bearing, because write profiles mutate the
store and every later profile inherits what earlier ones wrote:

| profile | writes | measures |
|---|---:|---|
| `read-only` | 0% | the concurrency ceiling with no write interference |
| `read-heavy` | 5% | high read, low write |
| `balanced` | 50% | both paths contending |
| `write-heavy` | 95% | ingest under query load |
| `write-only` | 100% | raw insert throughput |
| `contention` | 50% | write–write conflict on one hot node |
| `rel-create` | 100% | relationship inserts between distinct endpoints |
| `rel-hub` | 100% | every relationship landing on one endpoint |
| `unique-create` | 100% | every client racing the same unique values: one winner each, no duplicates |
| `delete-churn` | 100% | create-then-delete per worker: `DETACH DELETE` and relationship clean-up under load |

Seven further profiles are diagnostic controls — node-only writes, writes on
property names no read seeks, label-free writes, writes on a relationship type
no read traverses, and graph algorithms alone, under delete churn and beside
concurrent writes. They run only by name, never under `all`, which would change
the profile count and the mutation history every recorded sweep is compared
against.

`--writes single|multi` is required: whether the engine was allowed more than
one concurrent write transaction changes what the same plan measures.
`--dataset` chooses the world: `synthetic` (the default) seeds its own, so it
runs anywhere including CI; `snb` attaches to a server already holding an LDBC
SNB corpus and probes its key space instead of seeding it; `snb-platform` reads
that corpus through a set of application access shapes — a two-key seek under a
declared composite index, a bare-`LIMIT` listing, an `IN`-list seek, a grouped
hop aggregate. An attached dataset requires `--corpus`, and `--id-base N` keeps
stress writes clear of ids the corpus already holds.

A run can replay a **plan** instead of generating operations live:
`harness plan --profile P --clients N --seconds S --out plan.jsonl` writes
seeded per-client operation streams and their SHA-256, so every engine replays
the same operations. `--seconds` is required, and a plan too short for
`rate × seconds` is refused before it is written — and again before it is
replayed, since an exhausted plan leaves every level unquotable. The sweep
stops at the first level whose plan ran out and names the `--ops` that would
have covered it. A plan emitted for fewer clients, or for a different key
space, than the run is refused rather than wrapped.

The older single-purpose `stress` binary stays in the tree to reproduce the
operation sequence earlier sweeps recorded, and
`tests/a_converged_plan_replays_the_stress_op_sequence.rs` holds the harness's
generator to the same statements, in order, byte for byte.

### Stress: integrity, and the "not quotable" rules

After every level, once every client has joined, the harness checks the store:
every acknowledged `contention` write present in the hot node's counter, no
unique value committed twice, no relationship with a missing endpoint, and
for `delete-churn` that acknowledged creates minus acknowledged deletes equals
what survives, per worker and in total, with no leftover relationships and no
duplicate ids. A churn level that did no verifiable work fails rather than
passing vacuously. Any finding, or any transport error, fails the run. So does a
level whose throughput **degraded** (its second half served less than half its
first) or **stalled** (its 10th-percentile second served less than a quarter of
its median).

A level can also be **not quotable**: its figure is printed beside the reason
and recorded in the document, it is listed among the run's failures, and it
must not be compared. Each cause is recorded as a finding about the engine or
as an operator error:

| cause | when | kind |
|---|---|---|
| `plan_exhausted` | a replayed plan ran out before the level's clock | operator error |
| `no_operations` | the level acknowledged nothing | finding |
| `all_writes_refused` | reads landed and every write was refused | finding |
| `stalled` | one operation took at least half the window | finding |
| `refusal_dominated` | more than half the write attempts were refused — except in `unique-create` and `contention`, where refusal is the measurement | finding |
| `no_concurrency` | more than one client, but never more than one operation in flight | finding |
| `too_short_to_judge` | fewer than four one-second buckets, so the degraded and stalled checks cannot fire | operator error |
| `warmup_ramp` | the second half ran more than 1.5 times the first: a warm-up, not a steady state | operator error |

Between 1.25 and 1.5 a ramp is a printed warning and the level is still quoted,
because some warm-up is expected on a sweep's first level. The harness refuses
a level shorter than four seconds before it starts; `--allow-short-levels`
permits one for a smoke probe and marks every such level `too_short_to_judge`.

### Graphalytics

```sh
graphalytics 127.0.0.1:7687 ./graphs/datagen-7_5-fb --scale S --reps 3 --json out.json
```

Runs the six LDBC Graphalytics kernels — BFS, WCC, PageRank, SSSP, LCC and
CDLP — against a graph already loaded at the address (converted by `ga2jsonl`
and loaded by `snbload`), and validates each against the reference output
published beside the graph. It measures the way the specification defines
measurement rather than with a stopwatch:

- Every kernel runs in conformance mode (`graphalytics: true`); the shipped
  `engram.algo.*` defaults differ deliberately and are not what the benchmark
  validates.
- Three repetitions per job, reported as the arithmetic mean of `Tp`, the
  algorithm alone; EVPS is (vertices + edges) / mean `Tp`, never a mean of
  per-run rates.
- Loading is timed apart and excluded from `Tp`, and so is a one-off warm-up
  that builds the kernel's projection. By default each repetition is timed as
  the procedure's `stats` mode, and the answer is validated from one `stream`
  run afterwards, so writing every vertex's value out is not counted as
  processing (`--tp stream` times the stream instead).
- Each scale has the specification's ceiling — 900 s for S, up to 10,800 s for
  2XL and above. A repetition that breaches it marks the whole job `TIM`, and
  its mean is recorded as a lower bound, never as a result.
- Validation follows the specification's comparison rules: exact for BFS and
  CDLP, a relative tolerance of 10⁻⁴ for PageRank, SSSP and LCC, and
  equivalence of the partition, not of the labels, for WCC.

`--kernels` runs a subset. The kernels' procedures are the `graphalytics`
family in the catalogue, and a test holds that family to the kernels the runner
actually runs.

## The single-purpose lanes

These came before the harness and remain in the tree. None of them produces a
published figure.

### `lsqb` — the Bolt-only LSQB lane

```sh
cargo run --release -p engram-bench --bin lsqb -- 127.0.0.1:7687 \
    --json out.json --timeout-secs 120 --expect counts.json
```

Nine queries, rendered from the same catalogue as `harness lsqb` — a test
fails if any statement is ever restated in this binary. `--expect`
checks the counts against a canon, because a fast wrong answer is not a result.

### `lsqbref` — the oracle

```sh
cargo run --release -p engram-bench --bin lsqbref -- ./corpus --expect counts.json
```

Computes the same nine counts **without going through the engine**. An
independent implementation is the only thing that catches a battery and an
engine agreeing on a wrong answer.

### `stress` — the original mixed-workload lane

```sh
cargo run --release -p engram-bench --bin stress -- 127.0.0.1:7687 all 1,8 30 \
    --seed 424242 --json out.json
```

The Bolt-only predecessor of `harness stress`, kept so the operation sequence
earlier sweeps recorded can be reproduced; the harness's generator is held to it
by a test. Its profiles are the same ten, and it self-verifies the same way.

### `snbconc` — concurrency over a query file

```sh
cargo run --release -p engram-bench --bin snbconc -- 127.0.0.1:7687 queries.txt 1,4,8 30 20
```

Runs a `;;`-separated file of statements at each client count for the given
seconds, and reports throughput and tail latency as the client count grows. The
last argument is the percentage of operations that **write**, so `20` is a
read-mostly mix and the default of `0` is read-only. It is the battery for a
query shape the fixed `stress` profiles do not cover — and it exists because a
single-client-sequential harness structurally hides a per-shard ceiling: every
query serialises on one engine thread behind the Bolt server.

## Attribution tools

When a battery says *what* moved, these say *why*.

| binary | question |
|---|---|
| `balattr` | which lever accounts for a balanced-profile change — `--lever name=on\|off` across the named mechanisms |
| `qcompare` | two servers, same statements, compared |
| `qcheck` | answers against expectations |
| `decoded`, `recdecode` | decoded values, and where record decoding spends time |
| `projscan` | projection and scan costs in isolation |
| `boltfloor` | the wire's own floor, with no engine work |
| `compat` | parse-rate over a statement corpus, bucketed by failure reason |
| `algowidth` | where splitting a graph-algorithm fixpoint across workers starts to pay — the evidence behind `Graph::algo_min_vertices` |
| `algocost` | what each graph algorithm costs per unit, and whether the all-pairs ceiling is placed right |
| `trigram` | what a trigram index saves a text filter, and what keeping it costs |
| `hoplist`, `mentlist`, `newsclass`, `riskrepro` | single read shapes, traced statement by statement |

`boltfloor` is the one people forget: it establishes what the protocol costs
before any engine work, so a "slow query" can be checked against the floor.

`compat` deliberately reports **failures bucketed by reason**, because a bare
percentage hides which features are missing — the only actionable content.

These tools answer an engineering question on the machine they are run on. A
figure one of them prints is not a published result; the published results are
the harness's, on [Three engines at SF3 and SF10](../measurements/three-engines-sf3-sf10.md).

## Utilities

| binary | |
|---|---|
| `cq` | the smallest possible Bolt client: `cq <addr> <statement>…`, rows one per line |
| `snbgen`, `fbgen` | generate a corpus: SNB-like, or on FinBench's schema |
| `datagen2jsonl`, `finbench2jsonl`, `ga2jsonl` | convert LDBC SNB, FinBench and Graphalytics data to the JSONL corpus |
| `snbload`, `jsonl2neo4j` | load a JSONL corpus over Bolt, or turn it into Neo4j bulk-import files |
| `snbparams` | derive query parameters from a loaded corpus |
| `engram-bench` (the crate's own target) | a sized sweep over every layer, writing `measurements/baseline.json` |

`cq` exists because a harness can only run its own profiles, and investigating a
finding needs the ability to ask a question the harness did not think of. Most
of the verified output in this book was produced with it.

The crate's own `engram-bench` target is the odd one out and is easy to miss,
because it is `src/main.rs` rather than a file under `bin/`. It sweeps sizes
across every layer and checks **growth shapes** rather than absolute times — a
point read whose cost grows with the corpus is a design failure that an absolute
number alone would hide — and it reports a shape breach as a warning rather than
a failure, on the grounds that wall time on a dev box is a report and not a
gate. It writes `measurements/baseline.json`, relative to the directory it is
run from, for comparing one local run with the next; it is not one of the
checked-in regression baselines.

## Interpreting a result

**Predict first.** Write it in the script.

**Check the counters, not just the time.** The
[counters](../reference/observability.md) say whether the mechanism you changed
actually ran — `adj_built` versus `adj_repaired`, `stale_served` versus
`stale_declined`, the `won@N` distribution.

**Confirm the arm ran.** A fired-counter canary proving the lever took effect is
the difference between an A/B and two identical runs.

**Report what did not move — and what moved backwards.** When a change helps
most queries and slows one, the slowed one is the informative part: a
mechanism whose new form loses to its old one on some shape is an optimisation
lead, and a report listing only the wins buries it. The same applies across
engines — the comparison page states where Engram trails at the same
prominence as where it leads.

**Gate before you quote.** A candidate build's documents gated against the
checked-in baselines with `harness report --baseline` say whether anything got
worse on the questions the baselines asked; a refusal (exit 3) says the
candidate did not ask the same questions, and no ratio should be read off it.

## Next

- [How Engram is measured](../measurements/index.md) — the rules.
- [Three engines at SF3 and SF10](../measurements/three-engines-sf3-sf10.md) —
  the current results.
- [Tuning guide](../reference/tuning.md) — turning a measurement into a change.
- [Loading data at scale](../using/bulk-loading.md) — the loaders in production
  use.
