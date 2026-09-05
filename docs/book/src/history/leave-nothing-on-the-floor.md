# Leave nothing on the floor

> **Historical — written 2026-09-01**, the evening the same-window verdict
> landed. The lead was real; this is the inventory of everything **known** to
> still be open.
>
> It is the most recent document in the engineering record, and the direct
> ancestor of [Roadmap](../roadmap.md).

## What kind of document this is

Not a plan for the next feature. An **inventory of unfinished business**, written
at the moment it would have been easiest not to write:

- every measured signal not yet acted on;
- the two performance reserves scoped during the campaign and **deliberately not
  spent**;
- the release work still owed.

Dependency-ordered, with **the prediction each item must beat recorded before its
run**.

Writing this on the evening a campaign succeeds is the discipline. The
alternative — declaring victory and letting the open items disperse into memory —
is how a project loses the list of things it already knows are wrong.

## The house rules, restated at the top

The document restates them rather than assuming them, for every item without
exception:

> One change per measurement; a lever per mechanism; a canary **proven to bite**
> before the A/B; predictions recorded in the measuring script before it runs;
> same-window pairs.

## What was on it

The items themselves have largely been absorbed into
[Roadmap](../roadmap.md), which is where to look for current status. The
categories are worth keeping:

**Measured signals not yet acted on.** Attribution that named a mechanism where
no fix was yet built — a materialised set inside a labelled hop count, a memo
keyed on a clock more global than it needed.

**Reserves deliberately not spent.** Two performance opportunities scoped during
the campaign and left unspent, because spending a reserve to improve a number
that is already ahead costs the ability to spend it when it is needed. One was
the morsel-parallel count fold, later measured at 5.6× on q2.

**The comparator being dead.** The named comparison engine for the redesign had
been archived, and the document records both that fact and the work to measure
its community continuation — with the archived version kept runnable so the old
numbers reproduce.

**Release work.** The pre-publication items: the tree passing scrub, the legal
files, the name.

**Scale.** The plan called for SF1 → SF100 as the proving ground, and only SF1
had run — with an explicit warning **against quoting SF1 margins as if
scale-free.** That caveat is carried into
[Measurements](../measurements/index.md) verbatim in spirit.

## The line worth taking from it

> **A prediction recorded before the run.**

Every item carries the number it must beat, written into the measuring script
*before* it executes.

That is what turns "the estimator overshot its prediction" into a fact rather
than a story. It is also what made the campaign's surprises legible: the win
landing on the count fold's drivers rather than where predicted, and the
estimator moving queries it was not aimed at, are only observable if the aim was
written down first.

## Next

- [Roadmap](../roadmap.md) — where these items stand now.
- [Write path, phase 0](./write-path-phase0.md) — the verdict this followed.
- [How Engram is measured](../measurements/index.md) — the rules, and the
  caveats this document set.
