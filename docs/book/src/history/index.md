# About these documents

This part is the **engineering record**: the designs, evaluations and
remediation plans that produced the engine described in the rest of this book.

> **These are historical documents.** Each is dated, several were overtaken by
> the work they proposed, and some record conclusions that later measurement
> reversed. They are kept because **the reasoning is the durable part** — the
> attribution that found a defect, the theory that died to an instrument, the
> technique that was evaluated and rejected.
>
> For what the engine does **today**, read
> [Architecture](../architecture/overview.md) and
> [Reference](../reference/cli.md). Where those disagree with a page here, they
> are right and this is what was believed at the time.

## Why keep them at all

Because a design document that is only kept while it is current teaches nothing.
The most useful pages here are the ones that were **wrong**:

- The execution-engine evaluation named worst-case-optimal joins as state of the
  art and then **rejected** them for this workload — WCOJ wins on cyclic
  patterns and loses on the acyclic shape that dominates here. Knowing a
  technique was evaluated and declined is worth more than not mentioning it.
- The concurrency direction concluded that morsel-parallel execution was not
  needed at the corpus scale then in use. It was built later, and the
  measurement that overturned the conclusion is on the page.
- The remediation plan ordered work by measured impact, and the item that
  actually moved the most — first-call cardinality estimation — appears in
  *none* of the profiles that motivated the plan, because its cost hid in
  planning.

## How they were ported

These pages are **rewritten**, not copied. The originals sit in the
repository's `docs/` directory and reference private infrastructure, a private
product, and internal source paths throughout.

The rewriting preserves the engineering argument and drops:

- names of private systems, hosts and repositories;
- corpus descriptions that would disclose a private data shape;
- dated status tracking — DONE/PARTIAL markers, rev numbers, per-day progress.

Where a document's argument depends on private material, the engine-side finding
is kept and the caller-side detail is dropped. Where that would leave nothing
useful, the page says so rather than being silently thinned.

## Reading order

If you are working through this part rather than looking something up:

**The engine's shape**
1. [Engine redesign](./engine-redesign.md) — the survey and the target
   architecture
2. [Execution engine evaluation](./execution-engine-evaluation.md) — where the
   time actually went
3. [Query planning](./query-planning.md) — the first planner work
4. [Tail remediation plan](./remediation-plan.md) — the ordered response

**Concurrency and the write path**
5. [Concurrency direction](./concurrency-direction.md) — what serialises writes
6. [Write concurrency ceiling](./write-concurrency-ceiling.md)
7. [Write path, phase 0](./write-path-phase0.md) — what landed, and its canaries
8. [RC1 — guard-row exemption](./rc1-guard-exemption.md)
9. [RC2 — the sealed prefix](./rc2-sealed-prefix.md)
10. [Scale and integrity plan](./scale-and-integrity-plan.md)

**Derived structures**
11. [Derived structures](./derived-structures.md) — the one rule
12. [The derived-refresh write tax](./derived-refresh-write-tax.md) — its price

**Measurement and conformance**
13. [The SNB proving ground](./snb-benchmark.md)
14. [LSQB completeness](./lsqb-completeness.md)
15. [Relationship-create parity](./rel-create-parity.md)
16. [openCypher conformance strategy](./conformance-strategy.md)

**The inventory**
17. [Leave nothing on the floor](./leave-nothing-on-the-floor.md) — everything
    known to still be open

## The house rules these documents follow

Every measurement in this part was taken under the rules on
[How Engram is measured](../measurements/index.md):

> One change per measurement; a lever per mechanism; a canary **proven to bite**
> before the A/B; predictions recorded in the measuring script *before* it runs;
> same-window pairs.

When reading a claim here, those rules are what makes it checkable — and where a
document notes that a rule was broken, that note is usually the most important
sentence on the page.

## Next

- [Architecture overview](../architecture/overview.md) — the engine as it is.
- [Measurements](../measurements/index.md) — the numbers as they stand.
- [Roadmap](../roadmap.md) — what these documents left open.
