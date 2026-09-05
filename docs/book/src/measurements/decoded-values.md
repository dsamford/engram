# Decoded-value parity

> **2026-08-19.** The run that answered a question a parse-rate cannot: not
> *"does it parse"*, but **"does a real statement over real data decode to the
> same values?"**

## Why parse rate was not enough

An earlier sweep established that 98.5% of an application's statements **parsed**.

That is the front half of a claim, and on its own it is close to worthless. A
parser that accepts a statement has demonstrated that it accepts the statement.
It has not demonstrated that executing it returns the same rows, with the same
types, decoded to the same values through a driver.

The project's own framing was blunt: a parser alone can only claim "parses",
which is the claim it explicitly rejects as near-zero value.

So this run is the back half.

## Method

**A closed world.** Rather than sampling, a bounded subgraph was exported
*whole*: 49 anchor labels — every label under a size threshold referenced by a
qualifying statement — plus every relationship they touch and every one-hop
peer. About 165,756 nodes and 190,464 relationships.

Two labels were excluded by a **degree pre-check** for being too large to close
over, and the exclusion is recorded rather than silently applied.

**Capture at the driver level.** Values were captured through the driver
directly, not through the application. The application's own layer would add its
own coercions, and then the comparison would be of two application layers rather
than of two engines.

## What "identical" means here

The comparison is on **decoded values**, which is stricter than it sounds and
catches a specific class of defect:

- an integer that arrives as a float,
- a temporal that decodes to a different instant,
- a null that arrives as an absent key,
- a list whose order differs,
- a node whose property map omits a null column.

Every one of those would pass a row-count comparison. Several would pass a
string comparison of the result. They are exactly the differences that break an
application after a migration, and none of them is visible from a pass rate.

That last one has history: the note that an incumbent's property function
**omits null columns**, and that the collapse cost this project repeatedly, is
why the on-disk format keeps an explicit NULL tag distinct from an absent
property. See [Key encoding](../architecture/key-encoding.md).

## Why a closed world

Exporting a bounded subgraph *whole* rather than sampling means a statement's
answer over the subgraph is **complete** — not an approximation of the answer
over the full corpus.

A sampled comparison can only tell you the two engines agree about the sample.
A closed world tells you they agree, full stop, over that world.

## What it established

The parse-rate claim became a value claim, on real statements over real data,
captured where an application would see it.

The successor to this discipline is the openCypher TCK — a third-party
definition of correctness rather than a self-selected corpus — and the
`lsqbref` oracle, which computes benchmark answers without going through the
engine at all.

Both follow the same rule: **the thing being measured must not also be the thing
supplying the expected answer.**

## Next

- [Testing](../development/testing.md) — the TCK and its integrity rule.
- [Cypher support](../using/cypher-support.md) — the value model.
- [How Engram is measured](./index.md) — the discipline.
