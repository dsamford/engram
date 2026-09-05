# Licence and trademarks

## Engram

**MIT.** Everything public, no open core.

```text
Engram
Copyright (c) 2026 The Engram Authors
```

The full text is in `LICENSE` at the repository root, which is what actually
grants the licence — the `license = "MIT"` field in the manifest is a
declaration, not a grant.

## Dependencies

**39 third-party crates, every one permissively licensed. No copyleft anywhere
in the tree.**

That is enforced rather than asserted: `cargo deny check` runs against an
**allow-list** of licences in `deny.toml`, not a deny-list — because a deny-list
passes anything nobody thought to forbid.

Allowed: MIT, MIT-0, Apache-2.0, Apache-2.0 WITH LLVM-exception, ISC, BSD-2 and
BSD-3, Unicode-3.0, CC0-1.0, Zlib.

Most dependencies are MIT OR Apache-2.0 and are satisfied by the notice above.
The few carrying terms that travel with a **binary** are reproduced explicitly
in `NOTICE`, which is the file to ship alongside a compiled artifact.

### Banned by name

`aws-lc-rs` and `aws-lc-sys` are denied in `deny.toml`, which also records what
that costs: they are FIPS-validated and `ring`, which Engram uses instead, is
not. The reason is the one-`cc`-invocation rule — AWS-LC needs cmake — and the
trade is written down rather than left implicit.

## The vendored openCypher TCK

The Technology Compatibility Kit under `crates/engram-tck/` is **Apache-2.0,
copyright Neo4j Sweden AB**, redistributed under its own terms.

Conformance figures quoted anywhere in this book are produced by running it.
**That crate is not published to crates.io.**

## Trademarks

**Neo4j** and **Cypher** are trademarks of Neo4j, Inc. **openCypher** is a
project of Neo4j, Inc. **LDBC**, **SNB** and the LDBC benchmark names are marks
of the Linked Data Benchmark Council.

**This project is not affiliated with, endorsed by, sponsored by, or derived
from** Neo4j, Inc. or the Linked Data Benchmark Council.

Where those names appear they are used **nominatively** — to say truthfully what
this software is compatible with, what specification it implements, and what it
is measured against. There is no other way to state "speaks the Bolt protocol",
"implements openCypher" or "measured on LDBC SNB", and no such statement claims
a relationship with the mark's owner.

### The wire identity

The server reports itself as `engram/<version>` in the Bolt handshake and does
**not** present itself as Neo4j.

An earlier version answered `Neo4j/5.26.0 (engram)` — using another company's
registered mark as this product's own wire identity. That was wrong and is
fixed. The string remains configurable for a client that turns out to read it,
but the default is honest.

### On benchmark names

Where this project generates its own corpus with `snbgen`, that is **synthetic
data on an SNB-like schema** — *not* official LDBC Datagen output, and it is not
presentable as LDBC results.

[Measurements](./measurements/index.md) states which corpus produced which
number, and the distinction is kept in the prose rather than left to a footnote.

## What has not been cleared

Stated plainly, because the alternative is implying otherwise.

`TRADEMARKS.md` records that **this project has not cleared its own name**. No
trademark search has been performed for "Engram" in any jurisdiction or class,
and no registration is claimed. The name is in use, not asserted as a mark.

The repository field in the manifest was a placeholder until recently, and the
manifest still carries a note that the copyright holder in `LICENSE` was an open
decision — the kind of detail that is easy to leave unresolved and permanent on
crates.io once published.

## Security reporting

Through the repository's Security tab, privately. See
[Security posture](./using/security.md) for scope and response commitments.

## Next

- [Known limits](./known-limits.md) — what this software does not do.
- [Roadmap](./roadmap.md) — what is planned.
