# Scale and integrity plan

> **Historical — 2026-08-30.** Three workstreams, and a documented instance of
> measurement overruling the design.

## The three problems

| | problem | where it stood |
|---|---|---|
| **W1** | **Phantom-class constraint races.** Two concurrent creates of one `UNIQUE`/`NODE KEY` value both commit; a `DETACH DELETE` racing a relationship create can leave a dangling edge | **Durable integrity violations**, invisible to OCC because the enforcement reads were unrecorded scans |
| **W2** | **Hot-key contention.** Eight clients incrementing one node: 2,620 ops/s *correct*, against a comparison engine's 3,615 | Losers burned full re-executions; the published comparison was not like-for-like |
| **W3** | **Morsel-parallel execution.** One statement ran on one thread; one-worker mixed profiles ceilinged around 700–1,000 ops/s | An opt-in parallel expand already existed, byte-identical and A/B-proven — the work was a generalisation, not a greenfield |

**Order: W1 → W2 → W3. Correctness before throughput before scale.**

## Why W1 is first even though W2 and W3 are the performance items

W1 is a data-corruption class, and the sentence describing it is the important
one:

> invisible to OCC because the **enforcement reads are unrecorded scans**

Constraint enforcement read the data to check for a duplicate, and that read was
not in the transaction's read set. OCC validates what a transaction *recorded*
reading — so a check that reads outside the read set is a check two concurrent
transactions can both pass.

Two creates of the same unique value both commit. Durably. With no error.

Within W1, guard rows land first: the smallest change, and it closes the
dangling-edge case. That became [RC1](./rc1-guard-exemption.md).

The uniqueness fix became the **marker row** — a key encoding the constrained
tuple, so two concurrent creates write the same key and the keyspace decides.
Enforcement moves from a read that can race to a write that cannot.

## The honest note about W2's baseline

W2's framing is worth reading twice:

> 2,620 ops/s (correct) against the incumbent's 3,615 — **its number documents
> lost updates**

The comparison engine was faster at that workload **because it was losing
updates**. Reporting the gap without that clause would be reporting a
performance deficit; reporting it with the clause is reporting a correctness
difference that presents as one.

The [testing page](../development/testing.md) has the same idea as a rule: *a
throughput number over lost updates never prints as a pass.*

## Where measurement overruled the design

The plan was executed, and the addendum records two places where the result
contradicted the design:

**The morsel seam was inert at that corpus scale.** W3's generalisation did not
move the numbers where the plan predicted, because the corpus was not large
enough for the split to pay.

**The one-worker win came from clause fusion** — an *operator-coverage* fix the
attribution surfaced — not from any of the three workstreams as designed.

Both are recorded in the plan itself rather than quietly dropped. A plan that
only records the parts that worked is a plan nobody can learn the shape of
prediction error from.

Morsel parallelism was later measured to matter, at a larger scale and on the
count fold rather than on expand — q2 5.6×, q9 5.8×, q6 5.0×. The design was
right about the mechanism and wrong about where it would pay.

## The correction the critique forced

The plan carries a section of corrections its own adversarial review produced
before it was written down, and the first is the sharpest:

> **The determinism digest does not gate this work.** Every "digest unchanged"
> acceptance clause in the raw designs was **vacuous.**

The gate runs a simulation-only workload that never touches the store, graph or
Bolt, and compares two fresh runs to each other rather than to a pinned
constant. Citing it as evidence for a store change is citing a gate for
something it does not exercise.

The real instruments named instead: the simulation sweep, A/B differential tests
with fired-counter canaries, and seeded one-worker-versus-six equivalence runs.

## Next

- [RC1 — guard-row exemption](./rc1-guard-exemption.md) — W1's first landing.
- [Transactions and isolation](../using/transactions.md) — constraints and OCC
  today.
- [Concurrency](../architecture/concurrency.md) — where morsel parallelism ended
  up.
