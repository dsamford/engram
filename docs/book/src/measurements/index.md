# How Engram is measured

Performance claims here come with the corpus, the host shape and the command
that produced them, or they do not appear. This page is the discipline that
makes the numbers on the next page worth reading, and then the current
standing.

## The rules

Every one of these exists because it was violated once and the resulting number
was wrong.

1. **One change per measurement.** A run that moves two things attributes
   neither.
2. **A lever per mechanism.** Nearly every optimisation has a `--no-*` flag or
   an environment toggle, so an A/B has a real control rather than a different
   binary. That is why most entries in the [CLI reference](../reference/cli.md)
   are levers rather than settings. The rule is enforced rather than trusted: a
   ratchet test scans every declared lever and requires each to be either set by
   the server or named on a frozen list of those that are not, so a new
   unreachable lever cannot be added and the debt can only shrink.
3. **A canary proven to bite, before the A/B.** A test that would pass either
   way measures nothing, so the instrument is broken deliberately first and
   required to fail. A canary that has never been seen to fail is not evidence
   that the thing it guards works.
4. **Predictions recorded before the run**, in the measuring script. A number
   explained afterwards explains anything.
5. **Same-window pairs.** A "before" measured on one machine and an "after" on
   another is not a comparison. Both arms run in one session, from fresh copies
   of the same snapshot, because write profiles mutate the store. A cross-window
   move is admissible as evidence **against** yourself and not for yourself.
6. **Matched execution widths across arms, asserted rather than assumed.** Each
   engine's parallelism is set explicitly and read back from its own startup
   line, because a leaked setting collapses both arms onto one side just as
   surely as a dropped one collapses them onto the other. The tell of a
   configuration difference is that the inflation is **non-uniform** —
   contention slows a battery roughly evenly, a configuration difference does
   not.
7. **Every run states its N and its start.** Passes inside one server share its
   state, so they are not independent samples of a binary; a suspected
   regression is checked across at least two fresh starts per binary from the
   same start state before it is called one.
8. **Throttle counters bracket every run.** A throttled run is reported as
   throttled.
9. **The harness self-verifies, and refuses.** Write profiles reconcile
   acknowledged writes against what the store holds and **fail** the run on
   loss — a throughput number over lost updates never prints as a pass. A level
   that acked nothing, had every write refused, or spent half its window inside
   one operation prints as **not quotable** beside its data rather than as a
   rate. Answer counts are asserted per query against the other engines, and a
   battery answering fewer queries than it names fails rather than being
   averaged in, where a missing query reads as a speed-up.
10. **A row count is not an answer.** Equal row counts cannot see a wrong value
    inside a `LIMIT`. Where a query's answer can be wrong in that way, it is
    checked value by value against an independent engine — the comparison page
    shows one such check, and the defect it found.
11. **Separation, not the ratio of medians.** A ratio of medians is not evidence
    unless the two arms' distributions come apart.

The recurring failure this guards against is the *silent instrument*: a
harness that measures nothing and reports success — a probe whose cost was
excluded from the reported time, a copy that wrote nothing, an audit that
skipped the site it appeared to clear, a server that started without its saved
structures and built them inside the measured pass. Each was converted into a
loud failure rather than fixed quietly.

## The current standing

**Engram build rev70 against Neo4j 5.26.31 Community and PostgreSQL 17.11, each
alone in a 40-CPU, 140 GiB container on the same 48-core server, on
LDBC-derived workloads at two data sizes each: Engram answered every query of
every family at both sizes — the only one of the three engines to do so — and
was faster than Neo4j on 103 of the 119 queries both answered and faster than
PostgreSQL on 90 of 126.** The tables, the method and the gaps are on
[Three engines at SF3 and SF10](./three-engines-sf3-sf10.md). In brief:

- **LSQB** (subgraph counting, SF3 and SF10): fastest on all nine queries at
  both sizes, typically 54× Neo4j and 8× PostgreSQL at SF3. At SF10 Neo4j could
  not answer q3 (transaction memory) or q9 (its 880 s limit).
- **SNB Business Intelligence** (28 queries): faster than Neo4j on 19 of 20 at
  SF3 and 16 of 17 at SF10, where Neo4j answers only 17; faster than
  PostgreSQL on 14 of 22 and 12 of 22. PostgreSQL leads the heavy joins LDBC's
  SQL is tuned for.
- **SNB Interactive** (21 queries): faster than Neo4j on 18 of 21 at both
  sizes, and than PostgreSQL on 15 and 17.
- **FinBench** (SF1 and SF10): the mixed result. Engram loses the
  transfer-path queries to both engines.
- **Serving under load**: the highest read-only and read-heavy throughput of
  the three at every client count measured (1 to 64) at both sizes — 7,723
  requests a second at SF3 with 64 clients, against 3,158 and 1,377.
- **Graphalytics** (Engram only): all six kernels on the ten S-size graphs, 57
  of 57 jobs validated against LDBC's reference output.

These are not official LDBC results; the comparison page says what that means.

### Query parallelism

Query parallelism is **off unless `ENGRAM_QUERY_PARALLELISM` is set**, and
there is no CLI flag for it. Every Engram figure on the comparison page ran
with it set to 40, the container's CPU quota; a default install runs each
statement on one thread. See [Environment variables](../reference/environment.md)
and [Concurrency and the worker model](../architecture/concurrency.md).

## What these numbers do not say

- **One rig, one start.** Each family was measured on one freshly started server
  per size, and the first recorded pass is shown. Some queries move with the
  start: BI 16 and BI 10 at SF10 each have two stable speeds that depend on it,
  on every Engram build measured.
- **SF10 is the largest size run.** SF30 and SF100 have not been.
- **Neo4j is the Community edition**, which runs each query on one thread. Its
  Enterprise parallel runtime was not measured.
- **Not the same build everywhere.** The query benchmarks ran on rev70, the
  stress tests on rev67 and Graphalytics on rev64; the builds between change
  nothing those workloads run, and the comparison page says so per family.
- **Not official LDBC results.** The workloads are derived from LDBC's
  benchmarks and were run with this project's harness, parameters and rules.
