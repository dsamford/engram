# openCypher conformance strategy

> **Historical — written 2026-08-27**, when conformance was **unmeasured**. The
> programme it proposed produced the 3,772-of-3,773 figure quoted throughout
> this book.

## The thesis

Engram already implemented a broad, semantically correct Cypher core — the full
clause set, three-valued logic, `OPTIONAL` null-fill, implicit grouping,
`DISTINCT`, variable-length traversal, `shortestPath`, `MERGE`, the temporal type
family and the DDL surface.

So "full openCypher" was **a bounded, additive programme, not a rewrite.**

Three moves:

1. **Make conformance measurable.** Adopt the TCK as the *definition* of full
   support and the north-star metric.
2. **Close a well-enumerated gap set in ROI order** — dominated by the function
   library, which is cheap, localised and high scenario-count, followed by a
   handful of clause, procedure and semantic items.
3. **Answer two policy questions up front** — the zero-dependency boundary
   (regex, spatial, IANA timezones) and the openCypher-9-versus-GQL scope line.

## The structural asset

The thing that made the programme safe, and the reason it could be additive:

> The executor **refuses unsupported constructs by name and never emits wrong
> rows.**

Every gap is therefore **enumerable** and **fail-closed**. Engram was never
silently non-conformant — only explicitly incomplete.

That is the ideal starting point for incremental conformance, and it is worth
noticing as a design property rather than an accident. An engine that
approximates an unsupported construct produces a gap set nobody can enumerate,
because the failures are wrong answers rather than refusals.

You can still see this: `=~` parses and refuses at evaluation, `UNION` inside
`CALL {}` refuses by name. See [Cypher support](../using/cypher-support.md).

## "We cannot manage what we do not measure"

The first move was the important one, and it is the one most likely to be
skipped.

Before the TCK was adopted, conformance was a *belief* — a broad clause set, a
corpus that passed, an absence of complaints. Adopting a third-party suite
turned it into a number that can fall.

And the number is **ratcheted in CI** (`MIN_PASS`, `MAX_FAIL`), so it cannot
fall quietly.

## The measurement rule that makes the number trustworthy

The harness scores a scenario **only when it can actually judge it**:

| outcome | meaning |
|---|---|
| **Pass** | fully evaluated, the answer matched |
| **Fail** | fully evaluated, the answer did not match — an *engine* verdict |
| **Skip** | the harness cannot evaluate it — a gap in the **harness**, never a pass |

**Pass rate is `Pass / (Pass + Fail)`**, with Skips reported separately.

A harness scoring its own blind spots as passes would measure nothing — which is
the failure mode this codebase guards against everywhere, and the reason the
conformance figure is quotable at all.

## The two policy questions

Both were genuinely open, and both are still visible in the engine:

**The zero-dependency boundary.** Regex, spatial types and IANA timezones each
require either a dependency or an implementation. The timezone answer was to
take `tz-rs` and `tzdb`; the regex answer is *still* to refuse — `=~` parses and
refuses at evaluation, and there is no regex crate in the workspace. See
[Roadmap](../roadmap.md).

**openCypher 9 versus GQL.** Where the target moves, conformance to what?

Answering these *before* the work meant the gap set was closed against a
decision rather than a preference.

## Where it landed

**3,772 of 3,773 evaluated scenarios**, 99.97%, CI-ratcheted.

The single failure is a time-zone-database expectation where this engine is
arguably the more correct of the two — which is the kind of residual a
conformance programme should end with.

## Next

- [Cypher support](../using/cypher-support.md) — what is supported and refused.
- [Testing](../development/testing.md) — the harness and its ratchet.
