# Security

## Read this before exposing a server

**This release has no authentication and no TLS.**

`MSG_LOGON` accepts credentials and does not verify them, and the Bolt listener
speaks plaintext. Any client that can reach the port can run any statement.
There is no user model, no roles, and no authorization check to bypass — there
is nothing there yet.

Run it bound to loopback, or inside a network boundary you control. Do not put
it on a public interface, behind a permissive security group, or on a shared
network segment, and do not rely on the absence of a published address as a
control.

Authentication (OIDC), namespace-aware authorization and TLS are the next
milestone. Until they land, the deployment boundary is the only boundary.

## What is and is not hardened

Several things an unauthenticated client could previously do have been fixed,
and they are listed because "we hardened it" is not a claim anyone should
accept unenumerated:

| | |
|---|---|
| PackStream nesting | bounded (`MAX_DEPTH`); an unbounded recursion used to abort the process |
| Cypher expression nesting | bounded, with a declared minimum parser stack |
| Message size | bounded (`MAX_MESSAGE_BYTES`, 64 MiB) |
| Per-connection queued bytes | bounded, with real backpressure |
| Concurrent connections | bounded (`--max-connections`) |
| Idle and write timeouts | set, so a silent peer cannot hold a thread pair |
| Query row budget | set by the server (`--row-budget`), not left unbounded |
| A panic in one session | contained; it no longer kills the worker and silently strands every connection pinned to it |

Still absent, and load-bearing for an exposed deployment: authentication, TLS,
rate limiting, an audit log, and any record of who did what. Session lifecycle,
authentication attempts, DDL and authorization denials are **not** recorded
anywhere.

## Reporting a vulnerability

Open a **private security advisory** through this repository's GitHub Security
tab. Please do not open a public issue for an unfixed vulnerability.

Include what you need to reproduce it: the version or commit, the statement or
byte sequence, and what you observed. A crash is useful even without an
analysis.

### What to expect

This project currently has **one maintainer**, and the commitments below are
deliberately modest so that they are ones that can actually be met. An unmet
published SLA is worse than a small one.

- **Acknowledgement within 7 days.** If you have not heard back in that time,
  assume the report was lost and send it again.
- **An assessment within 30 days**, saying whether it is accepted, and if so a
  rough severity and intended timeline.
- **No bounty programme.** There is no money. Credit in the advisory and the
  changelog if you would like it, and none if you would prefer not.

There is no embargo policy and no coordinated-disclosure process yet, because
there is no release cadence to coordinate with. If a fix is going to take
longer than you are willing to wait, say so and we will agree a date rather
than let it drift.

## Scope

In scope: the engine, the Cypher implementation, the Bolt protocol handling, the
storage and WAL formats, and the server.

Out of scope for now, because they are documented absences rather than defects:

- Anything an unauthenticated client can do. There is no authentication, so
  "an unauthenticated client can read data" is the documented state, not a
  vulnerability. Reports of what an authenticated user could do to *another*
  tenant will become in scope when authentication exists.
- Plaintext traffic. There is no TLS.
- Denial of service through legitimate expensive queries. The row budget bounds
  one query's materialisation; it does not make the engine fair.

A memory-safety issue would be notable: the workspace denies `unsafe` and has
none, so any memory-safety fault is either in a dependency or in the compiler,
and either is worth knowing about.

## Supply chain

Dependencies are gated by `cargo deny check` against `deny.toml`: an allow-list
of licences, a deny-list of specific crates, and advisory checks. Every
dependency in the tree is permissively licensed; there is no copyleft.

`ring` is the only cryptographic dependency. `aws-lc-rs` and `aws-lc-sys` are
banned by name — see the reasoning in `deny.toml`, including what that costs
(they are FIPS-validated and `ring` is not).
