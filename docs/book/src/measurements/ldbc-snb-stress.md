# LDBC SNB stress

> **2026-08-28.** The mixed read/write suite — ten profiles, at one and eight
> clients.

## Method

Every run is **seeded**, so the operation sequence is reproducible. The wall
clock is read only to measure, never to decide.

**A fresh server and a fresh data directory per profile.** This matters more than
it sounds: an accumulating fixture makes each later profile scan a bigger graph,
so **one long-lived server measures the order the profiles ran in** as much as
the profiles themselves.

**The fixture creates an index on the lookup key before seeding.** Without it,
every "point lookup" is a full label scan and the shapes are misnamed — a suite
that reports a point-lookup number for a scan is measuring something, but not
what its column heading says.

## The profiles

| profile | shape |
|---|---|
| read-only | reads, no writes |
| read-heavy | mostly reads |
| balanced | an even mix |
| write-heavy | mostly writes |
| write-only | writes, no reads |
| contention | many clients on one key |
| rel-create | relationship creation |
| rel-hub | relationship writes sharing an endpoint |
| unique-create | creates under a uniqueness constraint |
| delete-churn | create-and-delete cycles |

The last five exist because they are where the storage design's costs and
benefits appear. Aggregate throughput hides them; the write-side profiles are
where an append-only store with derived adjacency should win, and where the
guard-row and OCC defects showed up.

## The suite self-verifies

Hot-locality profiles **reconcile acknowledged writes against the hot counter
and fail the run on loss.**

> A throughput number over lost updates never prints as a pass.

That is not a nicety. The comparison that motivated
[the scale and integrity plan](../history/scale-and-integrity-plan.md) had the
comparison engine ahead on a hot-key profile **because it was losing updates** —
so a suite that cannot detect loss will report a correctness failure as a
performance win.

`floor` — the tenth-percentile second as a fraction of the median — is the stall
detector. **Anything under 0.25 fails the run**, which is what caught the
derived-refresh write tax.

## On the hardware

The document is explicit, and the note is worth preserving as a model:

> A contended developer workstation. These numbers characterise **shapes and
> ratios**, not absolute performance, and **none of them should be published as
> a benchmark** — that needs a pinned single-tenant host.

Saying which numbers are not publishable, inside the document that produced
them, is the discipline. The alternative is a figure quoted later without the
caveat that came with it.

The publishable stress figures — the twenty levels on
[How Engram is measured](./index.md) — come from same-window paired runs on a
dedicated host.

## Next

- [How Engram is measured](./index.md) — the current standing.
- [The derived-refresh write tax](../history/derived-refresh-write-tax.md) —
  what the floor detector caught.
- [Benchmarking](../development/benchmarking.md) — running the suite.
