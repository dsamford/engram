# Three engines at SF3 and SF10

The current standing: Engram against Neo4j and PostgreSQL on four LDBC-derived benchmark families at two data sizes each, the LDBC Graphalytics kernels on Engram, and a concurrent stress test on all three. Every figure on this page comes from one result document per engine and pass, written by the harness in `crates/engram-bench` and checked for identical answers before any time is compared.

> These are **not official LDBC benchmark results**. The workloads are derived from the LDBC Social Network Benchmark (SNB Interactive and BI), LDBC FinBench, LSQB and LDBC Graphalytics, using LDBC's published query texts and data generators, but they were run with this project's own harness, parameters and rules, and were not audited. They are not comparable with audited LDBC results.

## How the numbers were taken

| | |
|---|---|
| Machine | One dedicated 48-core cloud server (a CCX63 instance, 192 GB), used for nothing else. |
| Containers | Each engine alone in an identical container limited to **40 CPUs and 140 GiB**; engines never ran at the same time. Data on the same network-attached volume for all three. |
| Versions | Engram build rev70 for the query benchmarks (27–28 September 2026); the stress tests on build rev67 (27 September); Graphalytics on build rev64 (26 September). Neo4j **5.26.31 Community**; PostgreSQL **17.11**. rev68–rev70 change nothing the stress statements or the graph algorithms run. |
| Data | LDBC SNB datagen: **SF3** — 9.28 M nodes, 52.7 M relationships; **SF10** — 30.0 M nodes, 176.6 M relationships. The same generator output loaded into every engine. FinBench at SF1 and SF10 from its generator. Graphalytics: LDBC's published S-size graphs. |
| Caches | The same memory budget for every engine in each test: 32 GiB for SNB and stress, 8 GiB for LSQB and FinBench. Engram additionally caps its property-column cache at 8 GiB. |
| Parallelism | Engram and PostgreSQL may use up to 40 cores for one query. Neo4j Community runs each query on one thread — its parallel runtime is an Enterprise feature — which is a property of the product as shipped, not a setting chosen here. |
| Protocol | A freshly started server per family and size; one full pass discarded; then two recorded passes, the first of which is shown. SF10 BI: a warm-up pass and one recorded pass. Time limits: 30 minutes a query for LSQB, 15 for SNB (40 for BI at SF10), 10 for FinBench; Neo4j's own server-side limit is 880 s. |
| Parameters | One parameter file per family and size for every engine, each value derived from the data (95th percentile of cost) and checked to match real rows. PostgreSQL receives the original LDBC ids of the same people, tags and places. |
| Answers | Row counts (match counts for LSQB) equal across engines before any time is compared. A query that timed out, failed or could not run is shown as such, never as a time. BI 17 was additionally checked value by value (below). |

LSQB and SNB BI at SF10 were recorded a second time, early on 28 September: in the first run Engram's SF10 servers started without most of the derived structures it keeps on disk between runs (a defect fixed in build rev70b) and built them inside the measured passes. The second run started from the whole saved set, as build rev67's had.

## The standing

Across five benchmark families at two sizes each, Engram answered every query it was given, the only one of the three engines to do so, and was faster than Neo4j on 103 of the 119 queries both answered and faster than PostgreSQL on 90 of 126. Its widest leads are in pattern counting (LSQB) and in the SNB queries that walk a person's network. PostgreSQL stays ahead on a set of heavy analytical joins and on FinBench's transfer-path queries.

| family | size | answered (Engram / Neo4j / PostgreSQL) | faster than Neo4j | faster than PostgreSQL |
|---|---|---|---|---|
| LSQB | SF3 | 9 / 9 / 9 of 9 | 9 of 9 (typically 54.4×) | 9 of 9 (typically 8.2×) |
| LSQB | SF10 | 9 / 7 / 9 of 9 | 7 of 7 (typically 36.9×) | 9 of 9 (typically 7.5×) |
| SNB Business Intelligence | SF3 | 28 / 20 / 28 of 28 | 19 of 20 (typically 5.3×) | 14 of 22 (typically 1.5×) |
| SNB Business Intelligence | SF10 | 28 / 17 / 28 of 28 | 16 of 17 (typically 4.3×) | 12 of 22 (typically 1.0×) |
| SNB Interactive | SF3 | 21 / 21 / 21 of 21 | 18 of 21 (typically 4.9×) | 15 of 21 (typically 4.5×) |
| SNB Interactive | SF10 | 21 / 21 / 21 of 21 | 18 of 21 (typically 4.8×) | 17 of 21 (typically 5.6×) |
| FinBench | SF1 | 12 / 12 / 11 of 12 | 9 of 12 (typically 2.2×) | 8 of 11 (typically 2.5×) |
| FinBench | SF10 | 12 / 12 / 11 of 12 | 7 of 12 (typically 1.4×) | 6 of 11 (typically 1.1×) |
| **all** | | | **103 of 119** | **90 of 126** |

"Typically" is the geometric mean of the other engine's time over Engram's, across the queries both answered; above 1× Engram is faster. PostgreSQL's comparable count excludes the queries that read tables LDBC's scripts precompute before timing (³ below).

### Strengths and open gaps

- **Engram leads.** Pattern counting: fastest on all nine LSQB queries at both sizes, typically 37 to 54 times Neo4j and about 8 times PostgreSQL. The SNB queries against Neo4j: faster on 19 of 20 BI and 18 of 21 Interactive queries at SF3. Reading under load: 7,723 requests a second at SF3 with 64 clients, 2.4 times Neo4j and 5.6 times PostgreSQL, with the shortest slow tail. Writes that collide on one record: 7,553 updates a second against about 650 on both others. And it is the one engine that answered every query of every family at both sizes.
- **Neo4j leads.** A handful of short lookups where its per-query work is leaner (bi5, IS3, IC7 on the millisecond-date data, IC8 at SF10), FinBench's transfer-path queries (tcr1, tcr2, tcr5 at SF10), and the half-read, half-write "balanced" stress mix at 32 clients and above.
- **PostgreSQL leads.** Plain inserts: about 12,000 writes a second at 64 clients against Engram's 7,600. Heavy BI joins where LDBC's SQL is tuned (bi8, bi11, bi12, bi15, bi17, bi9), the two Interactive aggregations over a person's two-step network (IC5, IC6), and FinBench's transfer paths.
- **Still open for Engram.** The per-row cost behind bi5, IS3 and the FinBench transfer paths; memory under create-then-delete churn at SF10, the one workload that crashed; the slow first writes after an SF10 start; and teaching the fast pipeline operators Cypher's MATCH-wide relationship rule, whose general-path fallback costs bi14 about 15% today.

## LSQB

Engram is the fastest of the three on all nine counting queries at both sizes. At SF3 it is typically 54 times faster than Neo4j and 8 times faster than PostgreSQL; the widest gaps are the friends-of-friends shapes, q6 (278 times Neo4j) and q9 (167 times PostgreSQL). At SF10 Neo4j could not answer two of them, q3 (it ran out of transaction memory) and q9 (past its 880-second limit); on the other seven Engram is typically 37 times faster than Neo4j, and it is typically 7.5 times faster than PostgreSQL on all nine.

| query | Engram SF3 | Neo4j SF3 | PostgreSQL SF3 | Engram SF10 | Neo4j SF10 | PostgreSQL SF10 |
|---|---:|---:|---:|---:|---:|---:|
| q1 | 2,303 | 30,943 | 5,476 | 8,191 | 143,555 | 19,143 |
| q2 | 240 | 6,891 | 1,785 | 888 | 18,652 | 5,890 |
| q3 | 462 | 117,392 | 7,699 | 2,385 | out of memory | 45,534 |
| q4 | 1,126 | 33,519 | 2,954 | 4,179 | 132,948 | 10,273 |
| q5 | 1,179 | 31,927 | 2,345 | 5,143 | 121,739 | 7,036 |
| q6 | 530 | 147,131 | 32,476 | 2,085 | 745,110 | 106,374 |
| q7 | 1,628 | 38,022 | 5,709 | 6,198 | 137,350 | 19,932 |
| q8 | 1,334 | 58,975 | 4,047 | 5,070 | 213,416 | 13,075 |
| q9 | 1,194 | 221,237 | 199,319 | 5,082 | timed out | 930,992 |

Milliseconds, the first recorded pass.

## SNB Business Intelligence

Engram and PostgreSQL answer all 28 BI queries at both sizes; Neo4j answers 20 at SF3 and 17 at SF10. Against Neo4j, Engram is faster on 19 of the 20 queries both answered at SF3 and 16 of 17 at SF10, typically 4 to 5 times faster. The widest gaps are bi8 (4.9 s against Neo4j's 404 s at SF3; at SF10 Neo4j did not finish within 880 s) and bi14 (30 to 33 times). The one Neo4j wins is bi5, a short query: 58 ms against 33 at SF3, 246 against 157 at SF10. Neo4j Community cannot run the eight variants of bi10, bi15, bi19 and bi20, which need a procedure library it does not ship. bi17, the information-propagation query, is answered now: 14.6 s at SF3 (Neo4j 28.5 s, PostgreSQL 3.1 s) and 103 s at SF10, where Neo4j runs out of memory and PostgreSQL takes 16 s. Until build rev69 it ran past the 15-minute limit, and its counts were wrong (see How we tested). PostgreSQL is the harder opponent here: Engram is faster on 14 of 22 comparable queries at SF3 and 12 of 22 at SF10, and PostgreSQL leads clearly on bi8 (0.34 s against 4.9 s at SF3), bi12, bi11, bi17, bi15 and bi9, and at SF10 on bi16. Six PostgreSQL answers (bi4, bi6, bi19, bi20) read a table built before timing and are not compared.

| query | Engram SF3 | Neo4j SF3 | PostgreSQL SF3 | Engram SF10 | Neo4j SF10 | PostgreSQL SF10 |
|---|---:|---:|---:|---:|---:|---:|
| bi1 | 604 | 2,533 | 1,512 | 2,411 | 9,108 | 8,423 |
| bi2a | 538 | 6,355 | 8,976 | 1,618 | 24,096 | 35,418 |
| bi2b | 442 | 6,595 | 9,210 | 1,622 | 23,830 | 35,474 |
| bi3 | 518 | 1,664 | 2,551 | 2,195 | 5,006 | 10,032 |
| bi4 | 6,255 | 9,688 | 65.8 ³ | 21,766 | 43,698 | 222 ³ |
| bi5 | 57.7 | 32.6 | 984 | 246 | 156 | 5,378 |
| bi6 | 2,513 | 14,519 | 97.5 ³ | 14,424 | 95,881 | 317 ³ |
| bi7 | 19.4 | 39.4 | 95.2 | 50.2 | 141 | 299 |
| bi8a | 4,917 | 404,180 | 343 | 28,478 | timed out | 996 |
| bi8b | 4,911 | 383,151 | 337 | 28,889 | timed out | 1,006 |
| bi9 | 9,733 | 16,701 | 4,937 | 34,866 | 60,254 | 7,252 |
| bi10a | 2,555 ⁴ | not runnable ¹ | 21,160 | 42,152 ⁴ | not runnable ¹ | 92,843 |
| bi10b | 2,397 ⁴ | not runnable ¹ | 20,531 | 54,528 ⁴ | not runnable ¹ | 91,416 |
| bi11 | 3,655 | 5,878 | 715 | 19,908 | 36,459 | 4,444 |
| bi12 | 4,405 | 21,542 | 419 | 21,511 | 85,149 | 944 |
| bi13 | 530 | 1,147 | 3,747 | 2,386 | 4,438 | 14,638 |
| bi14a | 5,673 | 184,178 | 11,562 | 19,396 | 584,321 | 45,046 |
| bi14b | 5,835 | 183,872 | 11,491 | 19,951 | 586,281 | 45,156 |
| bi15a | 14,620 ⁴ | not runnable ¹ | 4,916 | 45,415 ⁴ | not runnable ¹ | 15,308 |
| bi15b | 14,105 ⁴ | not runnable ¹ | 4,922 | 48,290 ⁴ | not runnable ¹ | 15,223 |
| bi16a | 72.0 | 407 | 182 | 1,255 | 7,528 | 421 |
| bi16b | 165 | 398 | 178 | 1,000 | 7,663 | 416 |
| bi17 | 14,631 | 28,531 | 3,145 | 103,368 | out of memory | 16,099 |
| bi18 | 65.3 | 136 | 375 | 4.8 | 5.6 | 12.2 |
| bi19a | 14,500 ⁴ | not runnable ¹ | 69.8 ³ | 157,447 ⁴ | not runnable ¹ | 68.0 ³ |
| bi19b | 12,508 ⁴ | not runnable ¹ | 68.0 ³ | 132,958 ⁴ | not runnable ¹ | 66.5 ³ |
| bi20a | 103 ⁴ | not runnable ¹ | 22.8 ³ | 559 ⁴ | not runnable ¹ | 179 ³ |
| bi20b | 66.3 ⁴ | not runnable ¹ | 22.8 ³ | 431 ⁴ | not runnable ¹ | 176 ³ |

Milliseconds, the first recorded pass.

## SNB Interactive

Engram answered 18 of the 21 Interactive queries faster than Neo4j at both sizes, often by a wide margin on the lookups that walk a person's network (IC9 663 times faster and IC2 132 times at SF3). It is slower on IS3, a person's friends with the date each friendship began (13 ms against 11 at SF3), on IC7, which runs on the millisecond-date copy of the data (92 ms against 22 at SF3), on IS7 at SF3 by a fraction of a millisecond, and on IC8 at SF10. Against PostgreSQL running LDBC's own SQL, Engram is faster on 15 of 21 at SF3 and 17 of 21 at SF10. PostgreSQL's large leads are IC5 and IC6, the two queries that aggregate over a person's whole two-step network (IC5: 6.7 s against 2.0 s at SF3); it is also slightly ahead on IS3 at both sizes, on IC7, IS7 and IC10 at SF3, and on IC8 at SF10.

| query | Engram SF3 | Neo4j SF3 | PostgreSQL SF3 | Engram SF10 | Neo4j SF10 | PostgreSQL SF10 |
|---|---:|---:|---:|---:|---:|---:|
| IC1 | 6.2 | 21.5 | 52.7 | 12.2 | 60.5 | 780 |
| IC2 | 4.3 | 566 | 492 | 5.4 | 1,117 | 1,128 |
| IC3 | 314 | 8,628 | 1,268 | 715 | 27,930 | 3,397 |
| IC4 | 155 | 432 | 3,257 | 241 | 839 | 4,058 |
| IC5 | 6,666 | 6,944 | 2,008 | 22,360 | 23,308 | 5,065 |
| IC6 | 643 | 4,461 | 101 | 1,678 | 14,886 | 333 |
| IC7 | 92.0 ² | 22.0 ² | 42.5 | 52.8 ² | 13.3 ² | 87.1 |
| IC8 | 7.1 | 16.0 | 33.8 | 34.0 | 8.6 | 23.3 |
| IC9 | 12.2 | 8,073 | 485 | 25.9 | 28,465 | 1,565 |
| IC10 | 1,046 ² | 1,528 ² | 1,149 | 2,452 ² | 4,578 ² | 4,327 |
| IC11 | 15.2 | 112 | 47.2 | 30.3 | 200 | 105 |
| IC12 | 267 | 4,170 | 1,181 | 348 | 2,753 | 5,587 |
| IC13 | 1.2 | 3.5 | 95.3 | 1.4 | 3.6 | 168 |
| IC14 | 862 | 1,392 | 2,911 | 912 | 1,469 | 3,933 |
| IS1 | 0.40 | 2.2 | 1.3 | 0.36 | 2.7 | 1.3 |
| IS2 | 1.0 | 8.9 | 30.4 | 0.94 | 7.4 | 14.2 |
| IS3 | 13.3 | 11.0 | 9.5 | 19.3 | 14.2 | 16.7 |
| IS4 | 0.23 | 1.6 | 1.3 | 0.32 | 1.6 | 1.5 |
| IS5 | 0.24 | 1.3 | 2.7 | 0.21 | 1.0 | 1.9 |
| IS6 | 0.35 | 1.2 | 2.8 | 0.30 | 1.0 | 2.9 |
| IS7 | 2.8 | 2.6 | 2.6 | 1.9 | 2.8 | 3.1 |

Milliseconds, the first recorded pass.

## FinBench

FinBench is the mixed result. At SF1 Engram is faster than Neo4j on 9 of the 12 queries and faster than PostgreSQL on 8 of the 11 both answered; at SF10, on 7 of 12 and 6 of 11. Its clear losses are the transfer-path queries: tcr1 and tcr5 against both engines at both sizes, and tcr2 everywhere except against Neo4j at SF1. At SF10 tcr1 takes 526 ms against Neo4j's 74 ms and PostgreSQL's 47 ms. The heaviest query, tcr8, goes the other way at SF10: 2.6 s on Engram against 3.7 s on Neo4j and 12.5 s on PostgreSQL. Most of the rest take a few milliseconds on every engine; at SF10 Neo4j and PostgreSQL edge ahead on some of them by less than a millisecond. PostgreSQL could not answer tcr3 at either size: at SF1 it ran out of temporary disk space, and at SF10 it passed the 15-minute limit.

| query | Engram SF1 | Neo4j SF1 | PostgreSQL SF1 | Engram SF10 | Neo4j SF10 | PostgreSQL SF10 |
|---|---:|---:|---:|---:|---:|---:|
| tcr1 | 60.6 | 18.7 | 37.5 | 526 | 73.7 | 47.0 |
| tcr2 | 8.2 | 9.1 | 4.4 | 47.0 | 10.1 | 8.9 |
| tcr3 | 0.34 | 4.3 | out of temp disk | 0.56 | 17.6 | timed out |
| tcr4 | 1.1 | 3.3 | 5.9 | 0.72 | 2.4 | 2.5 |
| tcr5 | 13.2 | 5.9 | 5.8 | 29.9 | 5.4 | 4.1 |
| tcr6 | 3.0 | 4.3 | 6.0 | 4.8 | 4.1 | 5.3 |
| tcr7 | 1.6 | 3.1 | 4.3 | 2.7 | 2.5 | 2.7 |
| tcr8 | 62.7 | 41.5 | 1,200 | 2,557 | 3,656 | 12,472 |
| tcr9 | 1.7 | 4.8 | 5.3 | 3.0 | 4.5 | 3.4 |
| tcr10 | 0.96 | 5.7 | 2.1 | 1.2 | 4.7 | 2.3 |
| tcr11 | 0.33 | 3.8 | 2.5 | 0.52 | 2.2 | 2.1 |
| tcr12 | 0.45 | 2.8 | 3.0 | 0.47 | 1.8 | 2.6 |

Milliseconds, the first recorded pass.

¹ Needs the APOC or Graph Data Science library, which Neo4j Community does not ship. ² LDBC's Cypher for IC7 and IC10 does arithmetic on timestamps, which both graph engines refuse on the typed corpus; their figures come from a copy of the same corpus with dates stored as epoch-millisecond integers, the layout that text was written for. PostgreSQL runs LDBC's SQL on its own schema. ³ PostgreSQL reads a table LDBC's scripts build before the timer starts; shown, not compared. ⁴ Engram's own text for the query, calling its built-in procedures where the reference text needs APOC or GDS.

## BI 17, checked by value

A row count cannot see a wrong value inside a `LIMIT`. BI 17 returned ten rows on every engine while Engram's counts were wrong: it scoped Cypher's relationship-uniqueness rule to one path instead of to the whole `MATCH`, so it let one person play two roles that Cypher keeps apart. After the fix, build rev70's complete answer, without the `LIMIT`, matched PostgreSQL's person by person at both sizes: **1,177 people at SF3 and 4,778 at SF10, every count equal**. The persons are matched on the original LDBC id, which Engram keeps as a property.

## What scale does

From SF3 to SF10, 3.3 times the data, Engram's total time over the queries every engine answered grew 4.0 times on BI, against Neo4j's 3.5 and PostgreSQL's 3.8, and 2.9 times on Interactive, against 2.9 and 2.3. Serving throughput is where scale costs Engram most: with 32 clients it kept about half its SF3 read rate, where Neo4j kept about two thirds, though Engram still answered more reads a second at SF10 than either (3,428 against 1,892 and 794).

Query by query, the typical Engram query took about 4 times longer at SF10 on BI (median 4.0) and only 1.4 times longer on Interactive, whose lookups touch a person's neighbourhood rather than the whole graph. FinBench's data grows ten times from SF1 to SF10; Engram's total grew 21 times, almost all of it one query (tcr8, the heaviest), while its typical query grew 1.7 times and Neo4j's and PostgreSQL's barely grew (about 0.9), because most FinBench lookups are small at either size. The graph-algorithm chart shows the other side of scale: throughput follows a graph's density more than its size.

## Serving under load

With 64 clients sending the read mix, Engram answered 7,723 requests a second at SF3: 2.4 times Neo4j (3,158) and 5.6 times PostgreSQL (1,377). Its slowest 1% of answers stayed under 91 ms, where Neo4j's reached 367 ms and PostgreSQL's 1.2 s. At SF10, with 3.3 times the data, Engram served 3,547 a second against Neo4j's 1,916 and PostgreSQL's 759.

The short lookups (a profile, a person's friends, a message's replies) take a fraction of a millisecond on Engram and PostgreSQL even with 32 clients; Neo4j spends 2 to 3 ms on each. The heavy shapes decide the throughput: the tag count over friends' posts (ic6) takes 42 ms on Engram at SF3, against 118 ms on Neo4j and 362 ms on PostgreSQL. The city aggregation is the one shape Engram answers more slowly than both: 17.5 ms against about 11 at SF3, and 66 ms against 24 and 35 at SF10.

## Stress: ten mixed workloads at 1, 8, 32 and 64 clients

Engram's write path was rebuilt for this round (build rev67), and it changed the picture: at SF3 and 64 clients it now writes 7,585 new comments a second where the previous build managed 1,005, ahead of Neo4j's 3,427 though still behind PostgreSQL's 12,075. When every client fights over one record, Engram completes 7,553 updates a second against about 650 on both others. At 32 clients the one mix where it trails Neo4j at both sizes is the half-read, half-write "balanced" mix; at 64 clients Neo4j also edges ahead on write-heavy at SF10.

Every integrity check that ran passed on all three engines: every acknowledged write was found, no unique value was stored twice, and every deleted node took its relationships with it. The harness refuses to quote six of Engram's levels, and each has a reason. Racing inserts of the same unique value (unique-create) serialise on Engram: only one operation was ever in flight at 64 clients at SF3 and at 8 and 64 clients at SF10. Two levels began stalled and recovered partway through: create-then-delete churn at 64 clients at SF3, where storage maintenance had fallen behind the write rate (the level's second half ran 23 times its first), and contention at 64 clients at SF10, where that workload had seconds with almost nothing completed at every client count. At SF10 the first writes after Engram starts take about a second each for roughly 40 seconds, which is why its 1-client write-only figure reads 1 a second and its 8-client level is not quoted; the cause is not yet found. And at SF10 create-then-delete churn crashed Engram after its 8-client level had completed and reconciled: the relationship check the harness runs between levels scans every relationship in the store while the churn keeps the typed index stale, and the memory it used was not given back. From write-only on, Engram's SF10 resets also ran on a freshly started server (the SF10 test method says why); nothing inside a measured level differs.

**SF3**, operations a second, Engram / Neo4j / PostgreSQL (`*` = not quotable, `×` = no result):

| profile | 1 client | 8 clients | 32 clients | 64 clients |
|---|---:|---:|---:|---:|
| read-only | 499 / 113 / 203 | 3,164 / 976 / 1,133 | 7,270 / 2,930 / 1,412 | 7,723 / 3,158 / 1,377 |
| read-heavy | 430 / 128 / 206 | 2,213 / 1,042 / 1,162 | 4,243 / 3,049 / 1,491 | 4,779 / 3,343 / 1,444 |
| balanced | 354 / 126 / 278 | 915 / 846 / 1,533 | 2,462 / 2,700 / 2,835 | 2,730 / 4,175 / 2,682 |
| write-heavy | 343 / 180 / 391 | 1,486 / 616 / 1,704 | 4,932 / 2,007 / 6,521 | 5,579 / 3,681 / 12,113 |
| write-only | 386 / 169 / 425 | 1,714 / 685 / 1,757 | 5,569 / 2,086 / 6,248 | 7,585 / 3,427 / 12,075 |
| contention | 431 / 156 / 276 | 1,630 / 607 / 837 | 5,170 / 667 / 736 | 7,553 / 640 / 692 |
| rel-create | 436 / 211 / 432 | 1,862 / 880 / 1,558 | 6,744 / 2,268 / 6,579 | 12,013 / 4,008 / 13,191 |
| rel-hub | 389 / 195 / 474 | 1,581 / 687 / 1,846 | 5,848 / 2,175 / 7,029 | 11,246 / 4,267 / 13,136 |
| unique-create | 436 / 294 / 429 | 377 / 283 / 413 | 285 / 247 / 402 | 211* / 247 / 353 |
| delete-churn | 374 / 207 / 451 | 1,838 / 700 / 1,847 | 6,447 / 2,350 / 5,804 | 1,307* / 4,422 / 13,641 |

**SF10**, operations a second, Engram / Neo4j / PostgreSQL (`*` = not quotable, `×` = no result):

| profile | 1 client | 8 clients | 32 clients | 64 clients |
|---|---:|---:|---:|---:|
| read-only | 309 / 80 / 109 | 1,821 / 624 / 608 | 3,428 / 1,892 / 794 | 3,547 / 1,916 / 759 |
| read-heavy | 224 / 87 / 112 | 1,229 / 625 / 628 | 2,405 / 1,928 / 816 | 2,522 / 1,990 / 803 |
| balanced | 185 / 117 / 159 | 708 / 742 / 1,014 | 1,301 / 2,236 / 1,522 | 1,256 / 3,300 / 1,430 |
| write-heavy | 248 / 174 / 343 | 1,107 / 644 / 1,522 | 2,465 / 2,249 / 6,154 | 3,389 / 3,635 / 10,200 |
| write-only | 1 / 191 / 409 | 906* / 616 / 1,690 | 6,144 / 2,368 / 6,706 | 7,133 / 4,189 / 12,175 |
| contention | 241 / 132 / 170 | 690 / 608 / 786 | 2,551 / 604 / 755 | 3,484* / 674 / 655 |
| rel-create | 393 / 193 / 355 | 1,736 / 789 / 1,515 | 4,749 / 2,238 / 5,874 | 8,631 / 4,160 / 13,070 |
| rel-hub | 403 / 189 / 377 | 1,824 / 556 / 1,688 | 5,589 / 2,246 / 6,531 | 9,502 / 4,025 / 13,093 |
| unique-create | 438 / 297 / 374 | 396* / 308 / 408 | 301 / 286 / 386 | 226* / 260 / 340 |
| delete-churn | × / 217 / 366 | × / 816 / 1,767 | × / 2,269 / 6,871 | × / 4,108 / 13,318 |

Integrity checks run between levels on every engine; every one that ran passed. The profiles, their shapes and the rules that make a level quotable are in [Benchmarking](../development/benchmarking.md).

## Graphalytics (Engram)

The six LDBC Graphalytics kernels on the ten S-size graphs, in the specification's conformance mode (`graphalytics: true`), each validated against LDBC's reference output with the specification's comparison rules. **57 of 57 jobs validated; none timed out.** Tp is the arithmetic mean of three repetitions of the algorithm alone, excluding loading and the one-off warm-up that builds the projection; EVPS is (vertices + edges) / mean Tp. No other engine was run on these graphs.

| graph | vertices | edges | BFS (ms) | WCC (ms) | PR (ms) | CDLP (ms) | LCC (ms) | SSSP (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| datagen-7_5-fb | 633,432 | 34,185,747 | 109 | 484 | 155 | 487 | 907 | 583 |
| datagen-7_6-fb | 754,147 | 42,162,988 | 147 | 605 | 194 | 571 | 1,142 | 837 |
| datagen-7_7-zf | 13,180,508 | 32,791,267 | 932 | 1,689 | 1,402 | 1,150 | 643 | 8,392 |
| datagen-7_8-zf | 16,521,886 | 41,025,255 | 1,189 | 2,184 | 1,768 | 1,466 | 739 | 9,790 |
| datagen-7_9-fb | 1,387,587 | 85,670,523 | 547 | 1,226 | 394 | 1,169 | 2,574 | 2,292 |
| graph500-22 | 2,396,657 | 64,155,735 | 407 | 1,118 | 593 | 786 | 5,416 | — |
| dota-league | 61,170 | 50,870,313 | 65.5 | 679 | 78.9 | 307 | 18,335 | 209 |
| wiki-Talk | 2,394,385 | 5,021,410 | 100 | 215 | 231 | 243 | 286 | — |
| kgs | 832,247 | 17,891,698 | 88.7 | 295 | 138 | 215 | 904 | 596 |
| cit-Patents | 3,774,768 | 16,518,947 | 43.2 | 433 | 436 | 413 | 235 | — |

A dash means the kernel does not apply to that graph (SSSP needs edge weights).

## What these numbers do not say

- They are one rig, one run per pass, and the first recorded pass. Some queries move with the server's start: BI 16 and BI 10 at SF10 each have two stable speeds that depend on the start, on every Engram build measured.
- Neo4j is the Community edition. Its Enterprise parallel runtime was not measured.
- PostgreSQL ran LDBC's own SQL, with the indexes and precomputed tables LDBC's scripts create.
- The open gaps are listed on [Known limits](../known-limits.md).

