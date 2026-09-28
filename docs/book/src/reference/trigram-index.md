# Trigram index

```cypher
CREATE TRIGRAM INDEX file_content FOR (f:File) ON (f.content)
```

A trigram index makes four predicates seekable that were previously full scans:

| predicate | before | after |
|---|---|---|
| `f.content =~ 'fn parse_.*'` | scan | seek |
| `f.content CONTAINS 'RowDirectory'` | scan | seek |
| `f.content ENDS WITH '.rs'` | scan | seek |
| `f.content STARTS WITH 'pub '` | range index, if declared | seek, when no range index is |

`CONTAINS` and `ENDS WITH` are the ones that matter most, because a range index
cannot answer either at any price: neither is a contiguous span of any sort
order.

## How it works, and why the answer is a superset

Every indexed value contributes the set of three-character windows it contains.
A query is turned into a boolean condition over those windows — `foo.*bar`
requires the trigrams of `foo` **and** those of `bar` — and whatever satisfies
the condition is a **candidate**. The real predicate is then run against every
candidate.

That asymmetry is the whole design, and it is worth stating plainly:

- A condition **wider** than necessary costs time. Extra candidates are read,
  the predicate rejects them, the answer is right.
- A condition **narrower** than necessary loses rows, silently, with nothing
  anywhere looking wrong.

So every rule in the analysis returns "I cannot constrain this, scan instead"
the moment it is unsure, and the property is tested by generating thousands of
pattern-and-string pairs and requiring that *every genuine match satisfies the
derived condition* — rather than by checking that the conditions look plausible.
The analysis runs over the same parsed pattern the matcher itself uses, so the
index and the matcher cannot disagree about what a pattern means.

## What it indexes

The raw character sequence, **including punctuation and whitespace**, folded to
lowercase. There is no tokenisation, no camelCase splitting and no stemming.

That is the difference from the [full-text index](./procedures.md), and it is
deliberate: it is what makes `->foo`, `foo(bar` and `::baz` searchable at all.
A word-oriented analyzer throws exactly those characters away.

Values are padded with sentinels — two before, two after — so that anchoring
becomes an ordinary trigram requirement. That is what lets `ENDS WITH 'y'` be
indexed on a single character.

## When it declines

The index answers `None` — "scan instead" — rather than a partial answer, when:

- **The condition constrains nothing.** `f.content =~ '.*'` requires no trigram,
  and neither does a `CONTAINS` needle under three characters (no trigram is
  implied by two characters). An alternation with an unconstrained branch —
  `(foo|.*)` — constrains nothing either, because that branch matches anything.
- **The label scan is smaller.** The candidate count is compared against the
  label's size exactly as a property seek's is; the scan wins when it is
  cheaper, and no special case protects the index.
- **The pattern carries a variable.** `f.content CONTAINS other.name` is
  per-row, and a seek is chosen once for the clause.
- **The property is not declared.** An index nobody asked for is not built on
  the strength of one query.
- **Any indexed row holds a non-string.** This one differs from the range
  index, which reports an honest *floor* over the rows it could order and lets
  the caller decide. Here the answer is a candidate set, and a candidate set
  that is a floor is simply a wrong answer — the row it skipped might have
  matched. So one non-string value disables the index, and the scan answers.

## Cost

One entry per distinct three-character window per value, so a trigram index is
substantially larger per row than a range index over the same property. It is
maintained incrementally from the same change log the range index uses — for a
string property, the logged value *is* what the index needs — so a write costs
no additional read.

Sixteen indexes are held in memory at once, against the range cache's 256, for
the size reason above.

### The overlay and its fold

Writes accumulate in an overlay that is folded into a new base once it grows
past 4,096 entries. The fold rebuilds the base, and it runs on the thread of
whichever **reader** next finds the index stale — so its cost is a read
latency, not a background one.

That threshold invites an obvious criticism, and the criticism is correct: it
was copied from the range index, where one write contributes one overlay
entry, so 4,096 means 4,096 writes. Here one rewritten body contributes one
entry per distinct trigram, so the threshold arrives after a few dozen writes
of an ordinary body, and each fold shows up as a read whose latency is far
above the median.

**The obvious repair does not work.** Folding less often — at a larger fixed
threshold, or at a fraction of the base, so that O(base) work buys O(base)
insertions — is the textbook amortisation, and it fails here for two reasons.
A catch-up begins by cloning the index, so the overlay is copied in full on
every stale read; enlarging it charges the frequent operation to spare the
rare one, and every read gets slower. And a larger threshold does not make the
fold cheaper, only rarer: the spike is postponed, not amortised, and lands
larger when it comes.

**What fixed it was making the fold cheap rather than rare**, which is what
the amortisation argument was reaching for by the wrong route. The fold merges
its two already-ordered inputs instead of re-sorting their concatenation, and a
document's store key is shared by all of its entries behind an `Arc` — so
rebuilding the base is a refcount bump per entry rather than a heap allocation
per entry, and the index holds one key per document instead of one per trigram.
That removes work rather than moving it, which is why it improved the median,
the tail and throughput together where the threshold changes only traded one
for another. The fold that remains is the O(base) rebuild itself; taking it off
the reader's thread altogether would need a maintenance pass, and is the next
move if it ever matters.

Trigram indexing is **on by default**, so a declared `CREATE TRIGRAM INDEX` is
consulted on a stock server. `--no-trigram-indexes` turns the whole mechanism
off: every predicate above falls back to the scan that answered it before.
That is the switch the differential tests use to require both paths to return
identical rows.

## What it does not do

- **No relationship indexes.** Nodes only.
- **No multi-property index.** One label, one property. A fulltext index spans
  labels times properties and pays for it in its write hook; this one does not
  need to.
- **No persistence.** The index is neither loaded from a sidecar nor built at
  open. It is built lazily, behind a single-flight guard, on the thread of the
  first query that needs it — the same discipline the overlay fold above
  follows. Building is O(label), so the first text predicate after a restart
  pays for it and the restart itself does not; the structure is a cache of data
  the store already holds.
- **No case-sensitive mode.** Values and patterns are both folded to lowercase
  — one scalar to one scalar, no locale. That is *lowercasing*, not the case
  folding `=~`'s own `(?i)` does, and the rule is written out twice, once on
  each side of the crate boundary, because `engram-store` cannot reach up to
  `engram-cypher`. The two copies must agree, and nothing asserts that they do:
  the source names a Basic-Multilingual-Plane agreement test in `engram-graph`
  and no such test exists. What protects the answer meanwhile is the superset
  discipline above — every rule that is unsure widens to a scan.

## Next

- [Regular expressions](./regex.md) — the operator this index seeks for.
- [Schema, indexes and constraints](../using/schema.md) — declaring one.
- [Known limits](../known-limits.md) — where this sits among the rest.
