# Checked-in regression baselines

Result documents a candidate run is GATED against. A candidate records TWO
passes after its discarded one (see the protocol below), and both are gated
together:

```
harness report RECORDED.json RECORDED2.json --reproduce \
  --baseline measurements/baselines/<file>.json --max-regression 25 --min-regression-ms 5
```

Exit 0: no regression. Exit 4: at least one, each named with both numbers.
Exit 3: NOT COMPARABLE, no verdict — a different rig, fairness, catalogue text,
or (since 2026-09-23) a query key that BOUND DIFFERENT PARAMETERS. A refusal is
the gate working; it means the candidate did not ask the baseline's questions.
Exit 1: a document that does not parse, or a floor that is not a number.

## Why two passes and a floor

The harness times each query ONCE per pass, and at that resolution a relative
tolerance alone reports noise. rev54's Interactive pass, gated against rev49's
with only `--max-regression 25`, exited 4 on IC1 (6.2 → 9.0 ms) and IC3 (336.6 →
436.2 ms). Repeated runs of that same binary spanned 5.4–7.1 ms and 253–512 ms.
rev55b's first recorded pass read bi18 at 101.8 ms and IS7 at 16.1 ms, and its
second read 74.4 and 1.6.

- `--reproduce`: every document named is a repetition of ONE candidate, and a
  key is a regression only when every repetition regresses it. The figure shown
  is the best repetition's. Name repetitions of one binary only: two binaries'
  runs named together would hide a regression either of them has.
- `--min-regression-ms 5`: a millisecond key must also be slower by more than
  5 ms. It passes a slowdown smaller than itself whatever the ratio (a 1 ms
  query at 5 ms passes), and does not apply to throughput (ops/s) lanes.

Both costs are stated in `report::regressions_in_every`. The gate still catches
a real regression: rev43's recorded BI document, gated this way against rev49's
baseline, exits 4 naming bi12 (4,876.6 → 17,891.6 ms), bi4, bi13 and bi16a.

## What a candidate must do to be comparable

Each baseline records its rig, fairness block, catalogue digests and, per query,
the parameters it bound. A candidate must reproduce all of them:

| | |
|---|---|
| rig | `bench-sf3-40:ccx63:48:40:143360` (the engram benchmark container on the dedicated bench node) |
| fairness | `--thread-cap 40 --cache-mb 32768`, server `--paged-cache-mb 32768 --prop-column-budget-mb 8192`, `ENGRAM_QUERY_PARALLELISM=40` |
| why 8 GiB of columns | one cached `:Message` column at SF3 is ~650 MB (40 B per entry + 32 B per member aligned), over the whole 512 MB default, so at the default no Message column is ever kept and bi1/bi9 re-walk the store every statement (bi1 17.6 s -> 3.1 s, bi9 20.8 s -> 11.0 s on the same binary). Engram's caches total 40 GiB; the Neo4j arm ran a 32g page cache + 31g heap + 8g off-heap, PostgreSQL `shared_buffers=32GB` + the OS page cache. The engine default is unchanged. |
| harness | `harness-v9` records and gates (the catalogue whose bi15/bi19/bi20 entries are `verified`; `report --reproduce` and `--min-regression-ms` are v9's) |
| parameters | `bi-p95-v3.json` (BI) and `ic-v4.json` (Interactive) in this directory — derived by `snbparams --pick p95`, with `@ldbc` companions |
| store | `cypher` (BI and Interactive): the published SF3 store; `cypher_engram`: a precomputation copy whose `PATH_Q19` / `PATH_Q20` were rebuilt to LDBC's definitions. An older copy held a WRONG `PATH_Q19`; it was deleted on 2026-09-25 |
| storage | since 2026-09-25 the stores live on a network-attached 1,150 GB block volume, not the node's root disk. The SF3 stores fit in memory, so recorded (warm) passes read the same from either |
| protocol | a FRESH server per family; one full pass of the same query list, DISCARDED; then two recorded passes, each one harness invocation over the list. The first is what a baseline records; a candidate gates both |

The protocol matters as much as the flags. With no warm-up, a query's figure
depends on the server's history: bi1 is ~45 s as the first statement on a
just-warmed server and ~19 s warm, and bi13 is ~0.7 s after the battery's other
queries and ~2.1 s alone — on every binary. A candidate that
skips the warm-up pass will "regress".

## bi17 is excluded by name

SNB BI bi17 is the heaviest query in the family, and it is run apart from the
battery, as the published comparison runs it. These baselines were recorded
before build rev69 fixed it (a wrong answer and two plan defects); until then it
held the 900 s ceiling in a whole-family invocation, and the harness abandoned
every query after it — correctly, because the server was still computing it. On
rev70 it answers in 14.6 s at SF3. The query lists below leave it out.

```
cypher (BI)          bi1,bi2,bi3,bi4,bi5,bi6,bi7,bi8,bi9,bi10,bi11,bi12,bi13,bi14,bi15,bi16,bi18,bi19,bi20
cypher_engram (BI)   bi10,bi15,bi19,bi20
cypher (Interactive) the whole family
```

(bi10, bi15, bi19 and bi20 are in the `cypher` list and record as not quotable
there — they need the engram dialect. IC7 and IC10 are not quotable on the typed
corpus: their reference texts do epoch arithmetic on a DATETIME, which Neo4j
refuses alike. A baseline row that was not quotable never fails the gate.)

## Files

| file | binary | recorded |
|---|---|---|
| `snb-bi-sf3-cypher.json` | engram rev60b | 2026-09-26 |
| `snb-bi-sf3-cypher_engram.json` | engram rev60b | 2026-09-26 |
| `snb-interactive-sf3-cypher.json` | engram rev60b | 2026-09-26 |
| `bi-p95-v3.json` | BI parameters | 2026-09-23 |
| `ic-v4.json` | Interactive parameters (IC3's countryX/countryY now two countries) | 2026-09-24 |

These are the first recorded passes of chain118c, the first
recording from the bench volume. They replace rev59's documents (chain116,
recorded on the node's root disk), which are archived on the bench volume. rev60b's recorded documents, both passes, gated with
the command above against rev59's, exit 0 in every family (gate68), as rev59's
had against rev58e's (archived on the bench volume) and rev58e's
against rev55b's (archived on the bench volume).
rev55b's had replaced rev49's (archived on the bench volume), which
had replaced the rev22 documents of 2026-09-23, whose `cypher_engram` document
was recorded on `sf3bi19-store` (a precomputation that did not match LDBC's
`PATH_Q19`) and whose parameter file was `bi-p95-v2.json`.

## Figures that move with the server's history

These recorded figures depend on what ran before them in the pass, or vary
between server instances, on every binary. The two-pass `--reproduce` gate and
its floor are how it allows for them:

- IC9 recorded 12.5–15 ms in the rev43, rev48, rev49 and rev55b passes and
  360–436 ms in three others (rev43, rev46, rev47). Its warm speed rides on a
  date index built in the discarded pass; what else the pass leaves cached
  decides which figure it gets.
- IC3 spans 253–512 ms on one binary (rev55b recorded 397 and 370).
- bi1 is ~0.7 s after rev55b's full battery (3.5 s at rev49); its columns
  compete with the other queries' for the 8 GiB budget.
- bi18 read 101.8 ms and 74.4 ms in rev55b's two recorded passes.
- IS3 read 10.9–16.1 ms across server instances and clients (the engineering log; 12.9–13.7 at rev58e and 13.5–14.2 at rev59, whose relationships bind
  lean), and IS7 16.1 ms once and 1–2 ms in twenty runs.
- IC8 read 28–30 ms in one of the two passes at rev57b, rev58d, rev58e and
  rev59 at SF10, and 5–9 ms in the other.
- IC14 read 800–937 ms at rev58e and 933–1,002 at rev59 at SF3, where nothing
  in rev59 reaches it; at SF10 the two binaries read 799–802 and 784–844.
- bi13 read 517–808 ms across three A/B runs of rev59 against rev60 / rev60b
  on fresh servers (ab137–ab139, 2026-09-25/26). The newer binary read
  below rev59 in two of them and above it in the third, so a one-run bi13
  step is the server, not the binary.
- bi1 read 608–663 ms on fresh rev59 and rev60b servers from the volume
  (ab139), and 744–772 in chain118c's recorded passes on the same binary.
