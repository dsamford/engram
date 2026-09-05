# The derived-refresh write tax

> **Historical — measured 2026-08-30**, on identical store copies with only the
> server binary or its flags differing.
>
> This is the page to read before changing `--refresh-after-writes` or
> `--no-derived-refresh`. See also
> [Tuning guide](../reference/tuning.md#my-writes-stall-periodically).

## What happened

The derived-structure maintenance refresh was built to fix a real defect: a
write-only burst left the adjacency change logs unpruned — they are pruned only
by a reader's publish — so the **first** read afterwards paid a full rebuild.

A **25-second stall** that made a contention profile do eleven operations in
twenty seconds.

**The fix works.** That profile went from **1 → 373 ops/s.**

It also cost **2–3× of write throughput**, and the tax is not subtle.

## The measurement

| arm | write-only 1c / 8c | floor 1c / 8c | rel-create 1c / 8c |
|---|---|---|---|
| before the refresh existed | 1,050 / 1,304 | 0.91 / 0.84 | 1,444 / 1,840 |
| **default** (refresh on, 8,192 stamps) | 533 / 455 | **0.08 / 0.21** | 783 / 992 |
| `--no-derived-refresh` | **1,055 / 1,294** | 0.82 / 0.87 | 1,383 / 1,912 |
| tick-only (`--refresh-after-writes 0`, 60 s tick) | 580 / 1,029 | 0.08 / **0.03** | 1,475 / 1,674 |

`floor` is the 10th-percentile second as a fraction of the median — the
harness's stall detector. **Anything under 0.25 fails the run.**

## Three conclusions the table supports

**1. The refresh is the whole regression.** Turning it off restores the earlier
numbers *exactly* — 1,055 against 1,050 at one client, 1,294 against 1,304 at
eight, rel-create likewise. Nothing else in the intervening work costs write
throughput.

That precision is the point of same-window A/B pairs. "Roughly the same" would
not support the claim; three matching pairs do.

**2. Cadence is not the fix.** A 60-second tick with the write-count trigger
disabled still floors at **0.03–0.08**. One pass is itself expensive enough to
stall a 20-second measurement window.

**The cost is the pass, not its frequency.** That rules out the obvious
mitigation before anyone spends a week on it.

**3. It is present at one client**, so it is not lock contention between
writers. The maintenance thread is stopping the *single* writer.

## Why a pass is expensive

`refresh_stale_derived` walks every cached adjacency slot behind its epoch and
every membership slot behind its label epoch, repairing or rebuilding **through
the same paths and the same latches a reader would take**.

At the scale measured that is a 17.26M-row span per untyped table over ~100
sealed segments — and the paged path never compacted, so the segment count only
grew.

The refresh's budget was "at most one rebuild per pass". **Repairs were
unbounded**, and a repair over a large changed set is not cheap.

## What was done

**Publish both arms.** Neither is strictly better:

- **Refresh on** trades write throughput for the absence of a reader stall. Right
  when reads follow writes.
- **Refresh off** restores write throughput and moves the cost to the next
  reader. Right for a write-only load where nothing reads until later.

`--refresh-pass-rows` was added to bound a pass — rows one pass may re-read
before deferring the rest — so the cost is a budget rather than a function of
the changed set.

## Why this page exists

Because the honest form of "we fixed the stall" is **"we fixed the stall and it
cost 2–3× of write throughput, and here is the flag."**

A performance page that reports only the win teaches a reader to expect the win
in their workload. This one lets them work out which arm they are.

## Next

- [Derived structures](../architecture/derived-structures.md) — the mechanism
  being refreshed.
- [Tuning guide](../reference/tuning.md) — choosing an arm.
- [How Engram is measured](../measurements/index.md) — the same-window rule this
  relies on.
