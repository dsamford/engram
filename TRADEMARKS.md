# Trademarks

## Marks belonging to others

**Neo4j** and **Cypher** are trademarks of Neo4j, Inc. **openCypher** is a
project of Neo4j, Inc. **LDBC**, **SNB** and the LDBC benchmark names are marks
of the Linked Data Benchmark Council.

This project is **not affiliated with, endorsed by, sponsored by, or derived
from** Neo4j, Inc. or the Linked Data Benchmark Council.

Where those names appear here they are used **nominatively** — to say
truthfully what this software is compatible with, what specification it
implements, and what it is measured against. There is no other way to state
"speaks the Bolt protocol", "implements openCypher", or "measured on LDBC SNB",
and no such statement claims a relationship with the mark's owner.

### Specifically

- **The Bolt wire protocol.** This server implements Bolt so that existing
  drivers can connect. It reports its own identity in the handshake —
  `engram/<version>` — and does **not** present itself as Neo4j. An earlier
  version answered `Neo4j/5.26.0 (engram)`, using another company's registered
  mark as this product's own wire identity. That was wrong and is fixed. The
  string remains configurable for compatibility with a client that turns out to
  read it, but the default is honest.
- **openCypher.** The Technology Compatibility Kit vendored under
  `crates/engram-tck/` is Apache-2.0, copyright Neo4j Sweden AB, redistributed
  under its own terms. Conformance figures quoted by this project are produced
  by running it. That crate is not published to crates.io.
- **LDBC.** Benchmark results here use a corpus generated to the SNB schema by
  this project's own generator. They are **not** official LDBC audited results
  and must never be presented as such: an audited result requires the official
  Datagen, the official workload driver, and an LDBC-appointed auditor. Ours
  has none of those.

## This project's own name

"Engram" is used here as the name of this software. No trademark registration
has been sought or granted, and **no clearance search has been performed** for
it or for the planned name `engraphdb` in the database-software class. That is
an open item, recorded here rather than assumed away — the marks belonging to
others were checked, and this project's own was not.

Nothing in this file grants any licence to any trademark. The MIT licence in
`LICENSE` covers the code and does not convey trademark rights.
