# Architecture decision records

An ADR records a decision that is **expensive or impossible to reverse**, with
the reasoning that produced it and the consequences accepted along with it.

Most decisions here do not need one. A flag, an operator, a threshold — those
are measurable and reversible, and the [design history](../history/index.md)
carries their reasoning. An ADR is for the ones where being wrong is permanent.

## The index

| | decision | status |
|---|---|---|
| [ADR-001](./adr-001-on-disk-formats.md) | The on-disk formats: key encoding and value tags | **accepted, frozen** |

## When to write one

The test is not importance; it is **reversibility**.

Write an ADR when the decision:

- **cannot be undone once data exists** — a key layout, a value encoding, a hash
  rule;
- **cannot be retrofitted** — a hash chain added later cannot attest to history
  predating it; a key derived from a keyspace prefix cannot be added after data
  exists under a different derivation;
- **constrains everything above it**, so that a later choice inherits it whether
  or not anyone notices.

Do **not** write one for a decision a measurement can overturn. This project has
overturned several, and the record of that belongs in
[design history](../history/index.md) rather than in a document titled
"decision".

## What one contains

Following ADR-001's shape:

**Context** — what forced the decision, and what it costs to get wrong. ADR-001
opens by naming the key encoding as first in the risk register, *"unfixable once
data exists"*, and notes that five separate requirements are consequences of its
shape rather than features built on top.

**The decisions**, numbered, each with the alternative rejected and why.

**Consequences** — including the ones that hurt. ADR-001 records that **new
fixed-width value tags are only assignable before any reader ships**; after
that, every new type is length-prefixed. That is a permanent constraint accepted
in writing rather than discovered later.

**What enforces it** — golden byte-level tests, a sealed trait, a gate. A frozen
decision with nothing checking it is not frozen.

## The pattern worth copying

ADR-001 does something most ADRs do not: it records the decisions **not to
decide yet**.

> Spatial support ships no implementation in early milestones. What it gets is
> **reservations**, because the decisions that would preclude it are the ones
> frozen first.

Reserving `0x70–0x9F` for spatial tags and `0xA0–0xCF` for vector tags costs
nothing now and preserves an option that could not be recovered later. The same
reasoning put variable-length escaping in the codec although nothing in v1 uses
it — because **inventing escaping after data exists is the unfixable class**.

That is the ADR question in its most useful form: *what would we be unable to do
later if we do not decide this now?*

## Amending one

ADR-001 has been amended once. The commit-log chain rule was changed from one
hash over the whole entry to a two-hash form, **before any released format
existed**, because hashing the payload inside the log latch was the largest
serialised cost left after the tail was sharded.

The amendment is recorded **in the ADR**, dated, with the reason and the
consequence — logs written under the old rule do not verify under the new one
and were regenerated. The golden test's literal was updated exactly once and
carries the previous value.

After 0.1 the rule is frozen with the rest of the document.

That is the bar: an amendment is a dated entry explaining why the original was
wrong, not a silent edit.

## Next

- [ADR-001](./adr-001-on-disk-formats.md)
- [Key encoding](../architecture/key-encoding.md) — the decision in use.
- [Design history](../history/index.md) — the reasoning that is not frozen.
