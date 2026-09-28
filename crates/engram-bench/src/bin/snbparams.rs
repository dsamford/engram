//! Derive substitution parameters FROM THE LOADED CORPUS, and print what each
//! one matches.
//!
//! # Why this binary exists
//!
//! Every LDBC read battery is parameterised, and **a parameter is part of the
//! question**. Two figures in this project's recorded history are not results
//! because of it:
//!
//! * `bi12` ran `languages ['en','de']` against a corpus carrying `uz`
//!   (3,413,921 messages), `tk` (2,951,499) and `ar` (692,750). It matched
//!   ZERO rows and was recorded as `1 row / 160 s`.
//! * `bi16` ran on dates its tag had no messages for, twice, and returned an
//!   empty result each time.
//!
//! Neither run failed. Both produced a well-formed answer, quickly, and both
//! were written into a comparison table. The parameters had been copied out of
//! LDBC's example `:params` block — which is curated for LDBC's own generated
//! corpus, not for ours.
//!
//! # What this produces, and what it does NOT
//!
//! It produces parameters that are **valid for the corpus in front of it**:
//! every value is read out of the data, and every value is printed with the
//! number of rows it matches. A value matching nothing is a REFUSAL, not a
//! warning.
//!
//! It does **not** produce LDBC's *curated* parameters. LDBC's `paramgen`
//! selects by percentile so that every substitution costs about the same —
//! `bi16a` takes `percentile_disc(0.995)` of a tag-frequency distribution,
//! `bi19a` the 55th percentile of city-pair friend counts. Reproducing that
//! needs the generator. So:
//!
//! > **A number measured with these parameters may be compared with another
//! > ENGINE on the same parameters. It may NOT be compared with a published
//! > LDBC figure.**
//!
//! That sentence is stamped into the emitted file, because the file will
//! outlive this comment and somebody will quote a number from it.
//!
//! # Unknown parameters are refused, never guessed
//!
//! Discovery is a table from parameter NAME to a statement that reads a value
//! out of the corpus. A name not in the table stops the run and says so. The
//! alternative — inventing a plausible value — is precisely how a parameter
//! that matches nothing gets into a results table.

use std::collections::BTreeMap;

use engram_bench::backend::{Backend, BoltBackend, Cell};
use engram_bench::catalogue;

/// How one parameter is discovered.
struct Discovery {
    /// A statement returning ONE row, ONE column: the value to bind.
    ///
    /// Ordered so the BEST candidate comes first; `--pick` decides how far
    /// down that order the chosen value actually sits.
    pick: &'static str,
    /// A statement returning ONE integer: how many rows that value matches.
    /// Runs with the picked value bound as `$v`.
    count: &'static str,
    /// What the count is counting, for the printed line.
    unit: &'static str,
    /// A statement mapping the picked value to LDBC's OWN id, bound as `$v`.
    ///
    /// Empty where the parameter is not an id, or where the corpus carries no
    /// second identifier. The PostgreSQL arm loaded raw Datagen CSV and uses
    /// LDBC ids where engram uses dense ones, so an id parameter that carried
    /// only one of the two would silently address a DIFFERENT ENTITY on one
    /// arm.
    ldbc: &'static str,
    /// A statement returning the NUMBER OF CANDIDATES this pick orders over.
    ///
    /// Only needed for `--pick p95`, which skips 5% of them. Empty means the
    /// candidate count is unknown or unbounded (a Person seek orders over
    /// millions), and such a discovery always takes the top-ranked value.
    card: &'static str,
}

/// A discovery for ONE query, overriding the name table.
///
/// # Why this exists, and why the name table is not enough
///
/// The name table's premise is that LDBC reuses a parameter name to mean the
/// same thing — `personId` is a Person wherever it appears. **That premise is
/// FALSE for FinBench**, measured on SF1 2026-09-21: `id` is an
/// **Account** in tcr1, a **Person** in tcr2, tcr5 and tcr12, and a **Loan**
/// in tcr8. Binding an account id into tcr2's `(person:Person {id: $id})`
/// matches nothing, and five of the twelve queries returned zero rows in under
/// a millisecond each — fast, well-formed, and empty.
///
/// So where a family overloads a name, the QUERY decides.
fn discovery_by_query(query: &str, name: &str) -> Option<Discovery> {
    Some(match (query, name) {
        // `id` is the Person who OWNS the accounts the query walks from.
        ("tcr2" | "tcr5" | "tcr12", "id") => Discovery {
            pick: "MATCH (p:Person)-[:own]->(:Account)-[:transfer]-() \
                   WITH p, count(*) AS d \
                   RETURN p.id AS v ORDER BY d DESC, p.id ASC LIMIT 1",
            count: "MATCH (p:Person {id: $v})-[:own]->(a:Account)-[:transfer]-() \
                    RETURN count(*) AS n",
            unit: "transfer(s) on an owned account",
            ldbc: "",
            // A CARDINALITY, so `--pick p95` can apply. Without one this always
            // took the single busiest owner — the worst-case parameter — and
            // tcr5 took 501 s at SF10 enumerating 1..3-hop paths from it.
            card: "MATCH (p:Person)-[:own]->(:Account)-[:transfer]-() RETURN count(DISTINCT p) AS n",
        },
        // `id` is a Loan, and it must have a deposit to walk from.
        ("tcr8", "id") => Discovery {
            pick: "MATCH (l:Loan)-[:deposit]->(:Account) WITH l, count(*) AS d \
                   RETURN l.id AS v ORDER BY d DESC, l.id ASC LIMIT 1",
            count: "MATCH (l:Loan {id: $v})-[:deposit]->(a) RETURN count(a) AS n",
            unit: "deposit(s)",
            ldbc: "",
            card: "",
        },
        // tcr6 walks WITHDRAW edges into the account —
        // `(:Account {id: $id})<-[:withdraw]-(mid)` — and keeps a `mid` with MORE
        // THAN THREE incoming transfers. The generic account discovery ranks by
        // TRANSFERS, a different relationship type entirely, so the account it
        // picked had nothing withdrawn into it and tcr6 returned zero rows.
        ("tcr6", "id") => Discovery {
            pick: "MATCH (a:Account)<-[:withdraw]-(mid:Account)<-[:transfer]-(:Account) \
                   WITH a, mid, count(*) AS t WHERE t > 3 \
                   WITH a, count(DISTINCT mid) AS d \
                   RETURN a.id AS v ORDER BY d DESC, a.id ASC LIMIT 1",
            count: "MATCH (a:Account {id: $v})<-[:withdraw]-(mid:Account) RETURN count(DISTINCT mid) AS n",
            unit: "account(s) withdrawing into it",
            ldbc: "",
            card: "",
        },
        // tcr1 walks `(account {id: $id})-[:transfer*1..3]->(other)` and keeps
        // only an `other` that a BLOCKED medium signs in to. The generic
        // account discovery picks the busiest account, which says nothing
        // about blocked media: at SF10 it matched 1,142 transfers and the query
        // returned ZERO rows after 314 s. Picking from the far end guarantees
        // at least a length-1 path, whose single timestamp trivially passes
        // the query's strictly-increasing check.
        ("tcr1", "id") => Discovery {
            pick: "MATCH (m:Medium {isBlocked: true})-[:signIn]->(x:Account)<-[:transfer]-(a:Account) \
                   WITH a, count(DISTINCT x) AS d \
                   RETURN a.id AS v ORDER BY d DESC, a.id ASC LIMIT 1",
            count: "MATCH (a:Account {id: $v})-[:transfer]->(x:Account)<-[:signIn]-(:Medium {isBlocked: true}) \
                    RETURN count(DISTINCT x) AS n",
            unit: "account(s) one transfer away that a BLOCKED medium signs in to",
            ldbc: "",
            card: "",
        },
        // tcr4 asks about a TRANSFER TRIANGLE: `src -> dst`, then `dst -> other
        // -> src`. The first version picked a connected pair — the busiest
        // account and its busiest out-neighbour — which satisfies the first
        // MATCH and says nothing about the cycle, so tcr4 returned ZERO rows
        // at SF10 in 1.7 s. A parameter that matches the query's first clause
        // is not a parameter for the query.
        //
        // Both ids come from the SAME triangle: identical statements with an
        // identical total order, returning `s` for id1 and `d` for id2. The
        // source side is limited to the busiest accounts, because enumerating
        // every 3-cycle over tens of millions of transfers is the expensive
        // thing a DISCOVERY must not become.
        ("tcr4", "id1") => Discovery {
            pick: "MATCH (s:Account)-[:transfer]->() WITH s, count(*) AS deg \
                   ORDER BY deg DESC, s.id ASC LIMIT 50 \
                   MATCH (s)-[:transfer]->(d:Account)-[:transfer]->(o:Account)-[:transfer]->(s) \
                   WITH s, d, count(DISTINCT o) AS n \
                   RETURN s.id AS v ORDER BY n DESC, s.id ASC, d.id ASC LIMIT 1",
            count: "MATCH (s:Account {id: $v})-[:transfer]->(:Account)-[:transfer]->(o:Account)-[:transfer]->(s) \
                    RETURN count(DISTINCT o) AS n",
            unit: "account(s) closing a transfer triangle through id1",
            ldbc: "",
            card: "",
        },
        ("tcr4", "id2") => Discovery {
            pick: "MATCH (s:Account)-[:transfer]->() WITH s, count(*) AS deg \
                   ORDER BY deg DESC, s.id ASC LIMIT 50 \
                   MATCH (s)-[:transfer]->(d:Account)-[:transfer]->(o:Account)-[:transfer]->(s) \
                   WITH s, d, count(DISTINCT o) AS n \
                   RETURN d.id AS v ORDER BY n DESC, s.id ASC, d.id ASC LIMIT 1",
            count: "MATCH (d:Account {id: $v})-[:transfer]->(o:Account)-[:transfer]->(s:Account)-[:transfer]->(d) \
                    RETURN count(DISTINCT o) AS n",
            unit: "account(s) on a transfer triangle through id2",
            ldbc: "",
            card: "",
        },
        // bi20 asks for the cheapest STUDY_AT route from a company's workers to
        // `person2`, and the generic picks are independent: the company with
        // the most employees, and the person with the most KNOWS edges. At SF10
        // that paired Air_Niamey's 283 workers with a person whose university
        // had eight students and no other link — a BFS over PATH_Q20 from them
        // reached nothing past depth 1 — so bi20 answered EMPTY, correctly, for
        // a parameter that could never have a route. One ordering picks both
        // from the same row: a company among the 20 largest, and a co-student
        // of one of its workers, so a route always exists. `n ASC` prefers a
        // person2 reachable through a SINGLE worker — a route that exists, not
        // the most trivially connected person in the corpus.
        //
        // The worker and person2 must KNOW each other as well as share a
        // university: that is one edge of LDBC's PathQ20 (umbra/dml/precomp/
        // bi-20.sql joins Person_knows_person with the study pairs). This pick
        // followed STUDY_AT alone until 2026-09-23, matching an engram
        // precomputation that made the same omission -- so the parameter and the
        // wrong answer agreed with each other, and only PostgreSQL disagreed.
        ("bi20", "company") => Discovery {
            pick: "MATCH (c:Company)<-[:WORK_AT]-(e:Person) WITH c, count(e) AS size \
                   ORDER BY size DESC, c.name ASC LIMIT 20 \
                   MATCH (c)<-[:WORK_AT]-(w:Person)-[:KNOWS]-(p2:Person), \
                         (w)-[:STUDY_AT]->(:University)<-[:STUDY_AT]-(p2) \
                   WHERE p2 <> w WITH c, p2, count(DISTINCT w) AS n \
                   RETURN c.name AS v ORDER BY n ASC, c.name ASC, p2.id ASC LIMIT 1",
            count: "MATCH (c:Company {name: $v})<-[:WORK_AT]-(p:Person) RETURN count(p) AS n",
            unit: "employee(s)",
            ldbc: "",
            card: "",
        },
        ("bi20", "person2Id") => Discovery {
            pick: "MATCH (c:Company)<-[:WORK_AT]-(e:Person) WITH c, count(e) AS size \
                   ORDER BY size DESC, c.name ASC LIMIT 20 \
                   MATCH (c)<-[:WORK_AT]-(w:Person)-[:KNOWS]-(p2:Person), \
                         (w)-[:STUDY_AT]->(:University)<-[:STUDY_AT]-(p2) \
                   WHERE p2 <> w WITH c, p2, count(DISTINCT w) AS n \
                   RETURN p2.id AS v ORDER BY n ASC, c.name ASC, p2.id ASC LIMIT 1",
            count: "MATCH (p2:Person {id: $v})-[:KNOWS]-(w:Person)-[:WORK_AT]->(:Company), \
                          (p2)-[:STUDY_AT]->(:University)<-[:STUDY_AT]-(w) \
                    WHERE w <> p2 RETURN count(DISTINCT w) AS n",
            unit: "co-student(s) who KNOW person2 and work at a company",
            ldbc: "MATCH (p:Person {id: $v}) RETURN p.sourceId AS v",
            card: "",
        },
        _ => return None,
    })
}

/// The discovery table, by parameter name.
///
/// Keyed on the NAME rather than the query because within the SNB families
/// LDBC reuses names consistently — `personId`, `tagName`, `startDate` mean
/// the same thing wherever they appear — and a per-query table would be twenty
/// copies of the same six statements, drifting apart one edit at a time.
///
/// **That does not hold for FinBench**, where `id` is an Account in tcr1, a
/// Person in tcr2/tcr5/tcr12 and a Loan in tcr8. [`discovery_by_query`] holds
/// those overrides and is consulted first.
///
/// Every `pick` orders deterministically, so two runs against the same corpus
/// emit the same file. An `ORDER BY` that ties would make the parameter file a
/// source of run-to-run variance, which is the one thing a parameter file must
/// not be.
fn discovery(name: &str) -> Option<Discovery> {
    // The most-connected / most-frequent value rather than an arbitrary one:
    // LDBC curates for a substantial workload, and a Person with no friends
    // makes IC1-IC14 trivially empty without matching zero rows.
    Some(match name {
        "personId" | "person1Id" | "person2Id" | "personIdA" | "personIdB" => Discovery {
            pick: "MATCH (p:Person)-[:KNOWS]-() WITH p, count(*) AS d \
                   RETURN p.id AS v ORDER BY d DESC, p.id ASC LIMIT 1",
            count: "MATCH (p:Person {id: $v})-[:KNOWS]-(f) RETURN count(f) AS n",
            unit: "friend(s)",
            ldbc: "MATCH (p:Person {id: $v}) RETURN p.sourceId AS v",
            card: "",
        },
        "tagName" | "tag" | "tagA" | "tagB" => Discovery {
            pick: "MATCH (t:Tag)<-[:HAS_TAG]-(m) WITH t, count(m) AS c \
                   RETURN t.name AS v ORDER BY c DESC, t.name ASC LIMIT 1",
            count: "MATCH (t:Tag {name: $v})<-[:HAS_TAG]-(m) RETURN count(m) AS n",
            unit: "tagged message(s)",
            ldbc: "",
            card: "MATCH (t:Tag) RETURN count(t) AS n",
        },
        "tagClass" | "tagClassName" => Discovery {
            pick: "MATCH (tc:TagClass)<-[:HAS_TYPE]-(t:Tag) WITH tc, count(t) AS c \
                   RETURN tc.name AS v ORDER BY c DESC, tc.name ASC LIMIT 1",
            count: "MATCH (tc:TagClass {name: $v})<-[:HAS_TYPE]-(t) RETURN count(t) AS n",
            unit: "tag(s)",
            ldbc: "",
            card: "MATCH (tc:TagClass) RETURN count(tc) AS n",
        },
        "country" | "countryName" | "country1" | "country2" | "country1Name" | "country2Name"
        | "countryXName" | "countryYName" => Discovery {
            pick: "MATCH (c:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(p:Person) \
                   WITH c, count(p) AS n \
                   RETURN c.name AS v ORDER BY n DESC, c.name ASC LIMIT 1",
            count: "MATCH (c:Country {name: $v})<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(p:Person) \
                    RETURN count(p) AS n",
            unit: "resident(s)",
            ldbc: "",
            card: "",
        },
        "city1Id" | "city2Id" => Discovery {
            pick: "MATCH (c:City)<-[:IS_LOCATED_IN]-(p:Person) WITH c, count(p) AS n \
                   RETURN c.id AS v ORDER BY n DESC, c.id ASC LIMIT 1",
            count: "MATCH (c:City {id: $v})<-[:IS_LOCATED_IN]-(p:Person) RETURN count(p) AS n",
            unit: "resident(s)",
            ldbc: "MATCH (c:City {id: $v}) RETURN c.sourceId AS v",
            card: "MATCH (c:City) RETURN count(c) AS n",
        },
        "company" | "companyName" => Discovery {
            pick: "MATCH (c:Company)<-[:WORK_AT]-(p:Person) WITH c, count(p) AS n \
                   RETURN c.name AS v ORDER BY n DESC, c.name ASC LIMIT 1",
            count: "MATCH (c:Company {name: $v})<-[:WORK_AT]-(p:Person) RETURN count(p) AS n",
            unit: "employee(s)",
            ldbc: "",
            card: "MATCH (c:Company) RETURN count(c) AS n",
        },
        "forumId" => Discovery {
            pick: "MATCH (f:Forum)-[:CONTAINER_OF]->(p:Post) WITH f, count(p) AS n \
                   RETURN f.id AS v ORDER BY n DESC, f.id ASC LIMIT 1",
            count: "MATCH (f:Forum {id: $v})-[:CONTAINER_OF]->(p) RETURN count(p) AS n",
            unit: "post(s)",
            ldbc: "MATCH (f:Forum {id: $v}) RETURN f.sourceId AS v",
            card: "",
        },
        "messageId" | "commentId" | "postId" => Discovery {
            pick: "MATCH (m:Message)<-[:REPLY_OF]-(r) WITH m, count(r) AS n \
                   RETURN m.id AS v ORDER BY n DESC, m.id ASC LIMIT 1",
            count: "MATCH (m:Message {id: $v})<-[:REPLY_OF]-(r) RETURN count(r) AS n",
            unit: "repl(ies)",
            ldbc: "MATCH (m:Message {id: $v}) RETURN m.sourceId AS v",
            card: "",
        },
        // ── Temporals ───────────────────────────────────────────────────
        //
        // # A cutoff needs data on BOTH sides, and the check must say so
        //
        // The first cut of this table picked `min(creationDate)` for every
        // lower-bound-looking name and verified it with `>=`. That parameter
        // "matched 9,010,236 messages" and made bi1 return NOTHING, because
        // bi1 filters `message.creationDate < $datetime` — strictly BELOW the
        // corpus minimum. The check had verified a different predicate from
        // the one the query uses, so it agreed with itself and proved nothing.
        //
        // Two consequences, both applied here:
        //
        //  * **Pick an interior value, not an extreme.** A midpoint between
        //    min and max has data on both sides whichever way the query's
        //    comparison points. It is computed arithmetically from two
        //    aggregates rather than by ordering nine million rows.
        //  * **Count the SMALLER side.** The count below returns
        //    `min(below, at-or-above)`, so a value with nothing on either side
        //    scores zero and is refused. A one-directional count cannot
        //    distinguish a good cutoff from a degenerate one.
        //
        // The window bounds are placed a quarter in from each end, so
        // `startDate < endDate` and both sit inside the corpus.
        "date" | "datetime" | "creationDate" => Discovery {
            pick: "MATCH (m:Message) WITH min(m.creationDate) AS lo, max(m.creationDate) AS hi \
                   RETURN toString(datetime({epochMillis: \
                   (lo.epochMillis + hi.epochMillis) / 2})) AS v",
            count: "MATCH (m:Message) \
                    WITH sum(CASE WHEN m.creationDate < $v THEN 1 ELSE 0 END) AS below,                          sum(CASE WHEN m.creationDate >= $v THEN 1 ELSE 0 END) AS above \
                    RETURN CASE WHEN below < above THEN below ELSE above END AS n",
            unit: "message(s) on the thinner side of the cutoff",
            ldbc: "",
            card: "",
        },
        "startDate" | "minDate" => Discovery {
            pick: "MATCH (m:Message) WITH min(m.creationDate) AS lo, max(m.creationDate) AS hi \
                   RETURN toString(datetime({epochMillis: \
                   (3 * lo.epochMillis + hi.epochMillis) / 4})) AS v",
            count: "MATCH (m:Message) \
                    WITH sum(CASE WHEN m.creationDate < $v THEN 1 ELSE 0 END) AS below,                          sum(CASE WHEN m.creationDate >= $v THEN 1 ELSE 0 END) AS above \
                    RETURN CASE WHEN below < above THEN below ELSE above END AS n",
            unit: "message(s) on the thinner side of it",
            ldbc: "",
            card: "",
        },
        "endDate" | "maxDate" => Discovery {
            pick: "MATCH (m:Message) WITH min(m.creationDate) AS lo, max(m.creationDate) AS hi \
                   RETURN toString(datetime({epochMillis: \
                   (lo.epochMillis + 3 * hi.epochMillis) / 4})) AS v",
            count: "MATCH (m:Message) \
                    WITH sum(CASE WHEN m.creationDate <= $v THEN 1 ELSE 0 END) AS below,                          sum(CASE WHEN m.creationDate > $v THEN 1 ELSE 0 END) AS above \
                    RETURN CASE WHEN below < above THEN below ELSE above END AS n",
            unit: "message(s) on the thinner side of it",
            ldbc: "",
            card: "",
        },
        "dateA" | "dateB" => Discovery {
            // A day the corpus actually carries traffic on, which is the whole
            // point for bi16: each of its two dates must carry messages or the
            // query returns nothing and looks fast.
            //
            // `toString` because a temporal read back as a value arrives here
            // through `Cell`, which carries only Int/Text/Null -- so a Date
            // would land as its DEBUG form, `Date(15259)`, and neither bind
            // nor coerce. ISO text round trips through the declared type.
            pick: "MATCH (m:Message) WITH date(m.creationDate) AS d, count(*) AS c \
                   RETURN toString(d) AS v ORDER BY c DESC, d ASC LIMIT 1",
            // `date($v)` because the parameter is a DATETIME (LDBC's own
            // example supplies `datetime('2012-09-16')`), and bi16 itself
            // compares `date(message1.creationDate) = date(paramDateX)`. The
            // count must apply the same conversion the query does or it
            // measures a different predicate -- which is the defect this
            // whole section exists around.
            count: "MATCH (m:Message) WHERE date(m.creationDate) = date($v) RETURN count(m) AS n",
            unit: "message(s) on that day",
            ldbc: "",
            card: "",
        },
        // TWO discoveries, because the declared types differ and so must the
        // predicate. `languages` is a STRING[] and coerces to a LIST, and a
        // list never equals a scalar -- `p.language = $v` counted ZERO for a
        // language this corpus demonstrably carries. That is the same
        // false-empty this tool exists to prevent, produced by the tool
        // itself, and it is why the count is bound and run rather than
        // assumed.
        "languages" => Discovery {
            pick: "MATCH (p:Post) WHERE p.language IS NOT NULL \
                   WITH p.language AS l, count(*) AS c \
                   RETURN l AS v ORDER BY c DESC, l ASC LIMIT 1",
            count: "MATCH (p:Post) WHERE p.language IN $v RETURN count(p) AS n",
            unit: "post(s) in it",
            ldbc: "",
            card: "MATCH (p:Post) WHERE p.language IS NOT NULL RETURN count(DISTINCT p.language) AS n",
        },
        "language" => Discovery {
            pick: "MATCH (p:Post) WHERE p.language IS NOT NULL \
                   WITH p.language AS l, count(*) AS c \
                   RETURN l AS v ORDER BY c DESC, l ASC LIMIT 1",
            count: "MATCH (p:Post) WHERE p.language = $v RETURN count(p) AS n",
            unit: "post(s) in it",
            ldbc: "",
            card: "MATCH (p:Post) WHERE p.language IS NOT NULL RETURN count(DISTINCT p.language) AS n",
        },
        "firstName" | "name" => Discovery {
            pick: "MATCH (p:Person) WITH p.firstName AS f, count(*) AS c \
                   RETURN f AS v ORDER BY c DESC, f ASC LIMIT 1",
            count: "MATCH (p:Person) WHERE p.firstName = $v RETURN count(p) AS n",
            unit: "person(s)",
            ldbc: "",
            card: "MATCH (p:Person) RETURN count(DISTINCT p.firstName) AS n",
        },
        // ── FinBench ────────────────────────────────────────────────────
        //
        // A different schema entirely: Account, Person, Company, Loan, Medium,
        // joined by `transfer`, `withdraw`, `signIn`, `own`, `guarantee` and
        // the two apply-loan edges. These names cannot collide with SNB's
        // because SNB has no `id` parameter and no Account label, so one table
        // serves both corpora and the wrong one simply finds nothing --
        // which is a refusal, not a wrong answer.
        //
        // `fbgen` mints account ids from 2^62, so the value read here is the
        // corpus's own id and must never be a small integer invented by hand.
        "id" | "id1" | "id2" | "accountId" => Discovery {
            pick: "MATCH (a:Account)-[:transfer]-() WITH a, count(*) AS d \
                   RETURN a.id AS v ORDER BY d DESC, a.id ASC LIMIT 1",
            count: "MATCH (a:Account {id: $v})-[:transfer]-(o) RETURN count(o) AS n",
            unit: "transfer(s)",
            ldbc: "",
            card: "",
        },
        // tcr10 compares two investors, so `pid1`/`pid2` must be DIFFERENT
        // people -- `pair_rank` gives the second the next-ranked value.
        "pid" | "pid1" | "pid2" | "personIdFb" => Discovery {
            pick: "MATCH (p:Person)-[:invest]->() WITH p, count(*) AS d \
                   RETURN p.id AS v ORDER BY d DESC, p.id ASC LIMIT 1",
            count: "MATCH (p:Person {id: $v})-[:invest]->(c) RETURN count(c) AS n",
            unit: "investment(s)",
            ldbc: "",
            card: "",
        },
        "cid" | "companyId" => Discovery {
            pick: "MATCH (c:Company)-[:own]->(:Account) WITH c, count(*) AS d \
                   RETURN c.id AS v ORDER BY d DESC, c.id ASC LIMIT 1",
            count: "MATCH (c:Company {id: $v})-[:own]->(a) RETURN count(a) AS n",
            unit: "owned account(s)",
            ldbc: "",
            card: "",
        },
        "loanId" => Discovery {
            pick: "MATCH (l:Loan) RETURN l.id AS v ORDER BY l.id ASC LIMIT 1",
            count: "MATCH (l:Loan {id: $v}) RETURN count(l) AS n",
            unit: "loan(s)",
            ldbc: "",
            card: "",
        },
        // FinBench's window bounds are epoch milliseconds on the edge itself.
        "startTime" | "time" => Discovery {
            pick: "MATCH ()-[t:transfer]->() RETURN toString(min(t.timestamp)) AS v",
            count: "MATCH ()-[t:transfer]->() WHERE t.timestamp >= $v RETURN count(t) AS n",
            unit: "transfer(s) at or after it",
            ldbc: "",
            card: "",
        },
        "endTime" => Discovery {
            pick: "MATCH ()-[t:transfer]->() RETURN toString(max(t.timestamp)) AS v",
            count: "MATCH ()-[t:transfer]->() WHERE t.timestamp <= $v RETURN count(t) AS n",
            unit: "transfer(s) at or before it",
            ldbc: "",
            card: "",
        },
        // ── Literals: bounds and thresholds, not corpus lookups ─────────
        //
        // These have no discovery statement because they are knobs, not data.
        // Their defaults are LDBC's own example values, and the count is the
        // corpus size the knob will be applied to, so a reader still sees the
        // scale it is cutting against.
        _ => return None,
    })
}

/// Knobs: a parameter that is a bound rather than a value read from the data.
/// The value is LDBC's own example, recorded so a reader can see it was chosen
/// rather than discovered.
fn literal(name: &str) -> Option<&'static str> {
    Some(match name {
        "maxKnowsLimit" => "4",
        "lengthThreshold" => "20",
        "delta" => "4",
        "minPathDistance" => "3",
        // IC10's window is a calendar month, 1-12; IC11's is a hire year. Both
        // are LDBC's own example values -- knobs, not data.
        "month" => "5",
        "workFromYear" => "2011",
        "maxPathDistance" => "4",
        "truncationLimit" => "100",
        "truncationOrder" => "TIMESTAMP_DESCENDING",
        "k" | "limit" => "10",
        "durationDays" => "30",
        // FinBench's amount thresholds. LDBC's own defaults; knobs, not data.
        "threshold" | "amountThreshold" | "threshold1" | "threshold2" => "0",
        _ => return None,
    })
}

fn cell_text(c: &Cell) -> Option<String> {
    match c {
        Cell::Int(n) => Some(n.to_string()),
        Cell::Text(s) => Some(s.clone()),
        Cell::Null => None,
    }
}

/// Swap a discovery's day-bucketing statements for ones that work on an
/// epoch-millisecond corpus.
///
/// `date(m.creationDate)` is a type error when `creationDate` is an integer --
/// "date(): takes a string or map, got integer" -- and a discovery that dies
/// there yields no parameter at all. Integer division by 86,400,000 buckets by
/// day just as well, and the value it returns is the day's first millisecond,
/// which is exactly what binds against an integer column.
/// Which of a PAIR this parameter is: the first (0) or the second (1).
///
/// LDBC pairs parameters by suffix -- `tagA`/`tagB`, `country1`/`country2`,
/// `person1Id`/`person2Id`, `city1Id`/`city2Id` -- and the pair is meant to be
/// two DIFFERENT things. bi19 is "the interaction path between two cities" and
/// bi15 a weighted path between two people; bind both ends to the same node
/// and the query degenerates into a path from something to itself. bi14 is
/// explicitly curated on `country1Id <> country2Id`.
///
/// Discovery picks the top-ranked value, so the second of a pair takes the
/// NEXT one. Without this the two ends are identical and the query answers a
/// question nobody asked -- quickly, and without failing.
/// `pick` advanced `rank` places down its ordering.
///
/// Only the OUTERMOST `LIMIT 1` is rewritten. A pick can carry an inner
/// `LIMIT 1` that chooses an ANCHOR — the original tcr4 `id2` pick chose its
/// source account that way — and `str::replace` rewrote every occurrence,
/// skipping the anchor as well as the answer, so the two stopped describing the
/// same row.
fn skip_to_rank(pick: &str, rank: usize) -> String {
    if rank == 0 {
        return pick.to_string();
    }
    match pick.rfind("LIMIT 1") {
        Some(at) => format!("{}SKIP {rank} {}", &pick[..at], &pick[at..]),
        None => pick.to_string(),
    }
}

/// The rank of a pair's member: 1 for the second (`B`, `2`, `Y`), else 0.
///
/// IC3 names its pair `countryXName` / `countryYName`. `Y` was not counted as a
/// second until 2026-09-23, so both ends took rank 0 and IC3 ran with X = Y =
/// China: every message counted as both an X and a Y message, `xCount = yCount`
/// on every row, and the query answered — agreeing with PostgreSQL, which was
/// given the same file — a question LDBC never asks.
fn pair_rank(name: &str) -> usize {
    let n = name.trim_end_matches("Id").trim_end_matches("Name");
    if n.ends_with('B') || n.ends_with('2') || n.ends_with('Y') {
        1
    } else {
        0
    }
}

fn adapt(d: Discovery, epoch_ms: bool) -> Discovery {
    if !epoch_ms {
        return d;
    }
    // On such a corpus `creationDate` is a plain integer, so `date(...)` is a
    // type error -- "date(): takes a string or map, got integer" -- and
    // `.epochMillis` is a property read on a number. A discovery that dies
    // there yields no parameter at all, so the arithmetic is done directly on
    // the integers instead. The VALUES are then epoch milliseconds, which is
    // exactly what binds against an integer column.
    if d.pick.contains("date(m.creationDate)") {
        // Integer division by 86,400,000 buckets by day, and the value
        // returned is the day's first millisecond.
        return Discovery {
            pick: "MATCH (m:Message) WITH (m.creationDate / 86400000) * 86400000 AS d, \
                   count(*) AS c RETURN d AS v ORDER BY c DESC, d ASC LIMIT 1",
            count: "MATCH (m:Message) WHERE m.creationDate >= $v \
                    AND m.creationDate < $v + 86400000 RETURN count(m) AS n",
            unit: d.unit,
            ldbc: "",
            card: d.card,
        };
    }
    if d.pick.contains("epochMillis") {
        // The same interior-point rule as the typed arm, in integer
        // arithmetic: the weights in the numerator decide whether this is a
        // midpoint, a quarter or a three-quarter point, and they are read back
        // out of the typed statement so the two arms cannot drift.
        let (a, b) = if d.pick.contains("3 * lo.epochMillis") {
            (3, 1)
        } else if d.pick.contains("3 * hi.epochMillis") {
            (1, 3)
        } else {
            (1, 1)
        };
        let pick = format!(
            "MATCH (m:Message) WITH min(m.creationDate) AS lo, max(m.creationDate) AS hi \
             RETURN ({a} * lo + {b} * hi) / {} AS v",
            a + b
        );
        // The same two-sided count, so a degenerate cutoff is still refused.
        let count = if d.count.contains("m.creationDate <= $v") {
            "MATCH (m:Message) \
             WITH sum(CASE WHEN m.creationDate <= $v THEN 1 ELSE 0 END) AS below,                   sum(CASE WHEN m.creationDate > $v THEN 1 ELSE 0 END) AS above \
             RETURN CASE WHEN below < above THEN below ELSE above END AS n"
        } else {
            "MATCH (m:Message) \
             WITH sum(CASE WHEN m.creationDate < $v THEN 1 ELSE 0 END) AS below,                   sum(CASE WHEN m.creationDate >= $v THEN 1 ELSE 0 END) AS above \
             RETURN CASE WHEN below < above THEN below ELSE above END AS n"
        };
        return Discovery {
            pick: Box::leak(pick.into_boxed_str()),
            count,
            unit: d.unit,
            ldbc: "",
            card: d.card,
        };
    }
    d
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: snbparams <bolt-addr> [--family snb-bi|snb-interactive|finbench]
                 [--out params.json] [--pick p95|max]

Derives substitution parameters from the CORPUS the address serves, prints what
each one matches, and refuses any that matches nothing.

The result is valid for THIS corpus and comparable across engines on it. It is
NOT LDBC's curated parameter set, so a number measured with it must not be
compared with a published LDBC figure."
        );
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let flag = |k: &str| -> Option<String> {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let fam_name = flag("--family").unwrap_or_else(|| "snb-bi".into());
    // `p95` by default: a representative parameter, the way LDBC curates.
    // `max` is the worst case and stays available for probing a ceiling.
    let pick_mode = flag("--pick").unwrap_or_else(|| "p95".into());
    let pick_p95 = match pick_mode.as_str() {
        "p95" => true,
        "max" => false,
        other => {
            eprintln!("[snbparams] --pick takes `p95` or `max`, got `{other}`");
            std::process::exit(2);
        }
    };
    let Some(family) = catalogue::family(&fam_name) else {
        eprintln!("[snbparams] no such family `{fam_name}`");
        std::process::exit(2);
    };
    let cat = match family.load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[snbparams] catalogue {fam_name}: {e}");
            std::process::exit(1);
        }
    };
    let mut be = match BoltBackend::connect(&addr) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[snbparams] cannot reach {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[snbparams] {} {} against {addr}",
        be.engine(),
        be.version()
    );

    // One scalar read, as text.
    //
    // RECONNECTS on a transport error. `BoltBackend` drops its client on any
    // non-refusal failure, so without this one bad statement turns every
    // later parameter into "connection is closed" -- a cascade that reads as
    // forty broken parameters when the truth is one. Discovery statements are
    // exploratory by nature (a corpus may not carry the label a pick names),
    // so the failure is expected and must stay local to the parameter that
    // caused it.
    let scalar = |be: &mut BoltBackend,
                  stmt: &str,
                  bind: Option<(&str, engram_cypher::Value)>|
     -> Result<Option<String>, String> {
        let run = |be: &mut BoltBackend| match &bind {
            None => be.query(stmt),
            Some((k, v)) => {
                let mut p = engram_bench::backend::Params::new();
                p.insert((*k).to_string(), v.clone());
                be.query_with(stmt, &p)
            }
        };
        let rows = match run(be) {
            Ok(r) => r,
            Err(e) => {
                let first = e.to_string();
                // One reconnect and one retry. A second failure is the
                // statement's, not the socket's.
                be.reconnect()
                    .map_err(|r| format!("{first}; reconnect: {r}"))?;
                run(be).map_err(|e2| e2.to_string())?
            }
        };
        Ok(rows.first().and_then(|r| r.first()).and_then(cell_text))
    };

    // ── How does THIS corpus store its temporals? ────────────────────────
    //
    // Not a detail. The catalogue declares bi1's `datetime` as a DATETIME
    // because LDBC's Cypher says `datetime(...)`; whether that BINDS depends
    // on the loaded corpus, and the ordinary SNB load carries `creationDate`
    // as an epoch-millisecond integer. Bind a temporal against an integer and
    // the comparison matches nothing -- silently, fast, and indistinguishable
    // from a working query.
    //
    // So it is measured, printed, and stamped into the file.
    //
    // Probed against whichever temporal this FAMILY actually carries. FinBench
    // has no `:Message` at all, so an SNB-only probe finds nothing there and
    // silently falls back to "typed" — which then refuses every one of its
    // window bounds for not being a YYYY-MM-DD date. The corpus has to be
    // asked about a property it owns.
    let probe_stmt = if fam_name == "finbench" {
        "MATCH ()-[t:transfer]->() WHERE t.timestamp IS NOT NULL RETURN t.timestamp AS v LIMIT 1"
    } else {
        "MATCH (m:Message) WHERE m.creationDate IS NOT NULL RETURN m.creationDate AS v LIMIT 1"
    };
    let temporal_encoding = match scalar(&mut be, probe_stmt, None) {
        Ok(Some(v)) if v.chars().all(|c| c.is_ascii_digit()) => {
            eprintln!(
                "[snbparams] corpus temporal encoding: EPOCH MILLIS (creationDate reads as {v}).
                             DATE/DATETIME parameters will bind as integers. LDBC's own Cypher
                             calls datetime() on its parameters, so a query text that does so
                             will NOT match this corpus -- see docs/bench/snb-datetime-corpus-build.sh
                             and ldbc-coverage-plan.md 0.3.0."
            );
            engram_bench::params::TemporalEncoding::EpochMillis
        }
        Ok(Some(v)) => {
            eprintln!("[snbparams] corpus temporal encoding: TYPED (creationDate reads as {v})");
            engram_bench::params::TemporalEncoding::Typed
        }
        Ok(None) => {
            eprintln!("[snbparams] no Message carries a creationDate; assuming typed temporals");
            engram_bench::params::TemporalEncoding::Typed
        }
        Err(e) => {
            eprintln!("[snbparams] cannot probe the temporal encoding: {e}");
            engram_bench::params::TemporalEncoding::Typed
        }
    };
    let epoch_ms = temporal_encoding == engram_bench::params::TemporalEncoding::EpochMillis;

    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut refusals: Vec<String> = Vec::new();
    // Discovered values are cached: `tagName` is the same tag wherever it
    // appears, and re-deriving it per query would both cost N scans and risk
    // two queries binding two different tags under one name.
    let mut cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    // Candidate counts, so one `count(t)` per discovery rather than per query.
    let mut cardinality_cache: BTreeMap<String, usize> = BTreeMap::new();
    // All messages, once: what an interior cutoff's thinner side is a share of.
    let mut message_total: Option<i64> = None;

    let names = cat.query_names(family.queries_path).unwrap_or_default();
    for q in &names {
        let variants = match cat.variants(family.queries_path, q) {
            Ok(v) => v,
            Err(e) => {
                refusals.push(format!("{q}: {e}"));
                continue;
            }
        };
        for (label, specs) in &variants {
            if specs.is_empty() {
                continue;
            }
            let key = if variants.len() > 1 {
                catalogue::variant_key(q, label)
            } else {
                q.clone()
            };
            let mut vals = BTreeMap::new();
            for spec in specs {
                if let Some(lit) = literal(&spec.name) {
                    eprintln!(
                        "  {key:<10} {:<18} = {lit:<24} (a bound, not discovered)",
                        spec.name
                    );
                    vals.insert(spec.name.clone(), lit.to_string());
                    continue;
                }
                // A QUERY-SPECIFIC discovery can pick a CORRELATED pair — tcr4's
                // `id1`/`id2` are two ends of one triangle — and its pick
                // already encodes that relationship. The name table's picks
                // are independent, which is the only case `pair_rank` is for.
                let query_specific = discovery_by_query(q, &spec.name).is_some();
                let Some(d) = discovery_by_query(q, &spec.name)
                    .or_else(|| discovery(&spec.name))
                    .map(|d| adapt(d, epoch_ms))
                else {
                    // REFUSED, not guessed. A plausible invented value is how
                    // a zero-matching parameter reaches a results table.
                    refusals.push(format!(
                        "{key}.{} ({}): no discovery is defined for this parameter name. \
                         Add one to snbparams' table rather than inventing a value.",
                        spec.name, spec.ty
                    ));
                    continue;
                };
                // ── How far down the ordering to take the value ─────────
                //
                // `pick` orders best-first, so rank 0 is the corpus MAXIMUM.
                // That guarantees a substantial, non-empty parameter, which is
                // why the first cut of this tool used it -- and it is a
                // WORST-CASE parameter, not a representative one.
                //
                // MEASURED, on SF3 2026-09-21. bi6 takes one tag. With the
                // corpus maximum (`Augustine_of_Hippo`, 84,063 tagged
                // messages) it was KILLED at 600 s; with LDBC's own example
                // (`Arnold_Schwarzenegger`, 14,459) it answered in 98.0 s.
                // The maximum is an outlier even among tags -- the
                // second-heaviest carries 44,182.
                //
                // LDBC curates by PERCENTILE for exactly this reason:
                // `percentile_disc(0.995)`, never `max`, so that every
                // substitution costs about the same. `--pick p95` approximates
                // that by skipping the top 5% of candidates; `--pick max`
                // keeps the old behaviour for worst-case probing.
                //
                // A discovery with no `card` statement orders over an unbounded
                // set (a Person by degree, over millions) and always takes the
                // top: skipping 5% of millions would land somewhere arbitrary
                // rather than somewhere representative.
                let pct_skip = if pick_p95 && !d.card.is_empty() {
                    match cardinality_cache.get(d.card) {
                        Some(n) => *n,
                        None => {
                            let n = scalar(&mut be, d.card, None)
                                .ok()
                                .flatten()
                                .and_then(|t| t.parse::<usize>().ok())
                                .map_or(0, |n| n / 20);
                            cardinality_cache.insert(d.card.to_string(), n);
                            n
                        }
                    }
                } else {
                    0
                };
                // The second of a pair takes the NEXT-ranked value, so the
                // two ends of bi14/bi15/bi16/bi19/bi20 are actually different.
                //
                // NOT for a query-specific discovery. `pair_rank("id2")` is 1,
                // and applying it to tcr4 took id2 from a DIFFERENT row than
                // id1 — the second-ranked account's second-ranked neighbour
                // instead of id1's own — so the "connected pair" was not
                // connected and tcr4 returned zero rows at SF10.
                let rank = pct_skip
                    + if query_specific {
                        0
                    } else {
                        pair_rank(&spec.name)
                    };
                let pick = skip_to_rank(d.pick, rank);
                // Cached on the STATEMENT, not the parameter name: `tag`,
                // `tagA` and `bi17`'s `tag` are one discovery and must agree,
                // while `tagB` is a different statement and must not.
                let picked = match cache.get(&pick) {
                    Some(v) => v.clone(),
                    None => {
                        let v = match scalar(&mut be, &pick, None) {
                            Ok(v) => v,
                            Err(e) => {
                                refusals.push(format!("{key}.{}: pick failed: {e}", spec.name));
                                cache.insert(pick.clone(), None);
                                continue;
                            }
                        };
                        cache.insert(pick.clone(), v.clone());
                        v
                    }
                };
                let Some(value) = picked else {
                    refusals.push(format!(
                        "{key}.{}: the corpus yielded no value for this parameter — \
                         the data it is read from is absent or empty",
                        spec.name
                    ));
                    continue;
                };
                // ── The check this binary exists for ──────────────────────
                //
                // The picked TEXT is coerced through the same path the runner
                // will use before the count is taken, so what is counted is
                // exactly what will later bind. A check that counted a
                // differently-typed value would agree with itself and prove
                // nothing -- which is how an ISO string silently compared
                // against a DateTime column and matched zero.
                let typed = match engram_bench::params::coerce_with(
                    &value,
                    &spec.ty,
                    temporal_encoding,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        refusals.push(format!(
                            "{key}.{}: the corpus yielded `{value}`, which does not coerce to \
                             its declared type {}: {e}",
                            spec.name, spec.ty
                        ));
                        continue;
                    }
                };
                let matched = match scalar(&mut be, d.count, Some(("v", typed.clone()))) {
                    Ok(Some(n)) => n.parse::<i64>().unwrap_or(0),
                    Ok(None) => 0,
                    Err(e) => {
                        refusals.push(format!("{key}.{}: count failed: {e}", spec.name));
                        continue;
                    }
                };
                if matched <= 0 {
                    refusals.push(format!(
                        "{key}.{} = {value} matches ZERO {} in this corpus. A parameter that \
                         matches nothing produces an empty result indistinguishable from a \
                         working query.",
                        spec.name, d.unit
                    ));
                    continue;
                }
                // An INTERIOR cutoff must SPLIT the population, not merely have
                // something on each side. It is arithmetic on min/max, and
                // outliers move it out of the bulk: on the epoch-millisecond
                // SF3 store (2026-09-23) a max creationDate near the year 3094
                // put every "interior" date past every real message; the
                // thinner side was ~49k outliers of 10.2M, this check passed,
                // and IC3/IC4/IC5 filtered on windows no message falls in.
                if d.unit.contains("thinner side") {
                    let total = *message_total.get_or_insert_with(|| {
                        scalar(&mut be, "MATCH (m:Message) RETURN count(m) AS n", None)
                            .ok()
                            .flatten()
                            .and_then(|n| n.parse::<i64>().ok())
                            .unwrap_or(0)
                    });
                    if total > 0 && matched.saturating_mul(100) < total {
                        refusals.push(format!(
                            "{key}.{} = {value} leaves only {matched} of {total} {} -- under \
                             1%, so it is not an interior cutoff: the corpus's min or max is an \
                             outlier and the arithmetic point left the bulk of the data.",
                            spec.name, d.unit
                        ));
                        continue;
                    }
                }
                eprintln!(
                    "  {key:<10} {:<18} = {value:<24} matches {matched} {}",
                    spec.name, d.unit
                );
                // ── The SECOND identifier, where the parameter is an id ────
                //
                // engram's `id` is dense; the PostgreSQL arm carries LDBC's
                // own. Emitting only one means the same file addresses a
                // DIFFERENT ENTITY on one arm -- silently, because the id it
                // was given exists there too. So an id parameter carries both,
                // and the runner binds whichever its dialect's id space uses.
                if !d.ldbc.is_empty() {
                    match scalar(&mut be, d.ldbc, Some(("v", typed.clone()))) {
                        Ok(Some(l)) => {
                            eprintln!(
                                "  {:<10} {:<18} = {l:<24} (LDBC id, for the SQL arm)",
                                "", format!("{}@ldbc", spec.name)
                            );
                            vals.insert(format!("{}@ldbc", spec.name), l);
                        }
                        Ok(None) => refusals.push(format!(
                            "{key}.{}: the corpus carries no LDBC id (sourceId) for this \
                             entity, so a cross-engine parameter file cannot be built from it",
                            spec.name
                        )),
                        Err(e) => refusals.push(format!(
                            "{key}.{}: LDBC-id lookup failed: {e}",
                            spec.name
                        )),
                    }
                }
                vals.insert(spec.name.clone(), value);
            }
            if !vals.is_empty() {
                out.insert(key, vals);
            }
        }
    }

    if !refusals.is_empty() {
        eprintln!("\n[snbparams] {} parameter(s) REFUSED:", refusals.len());
        for r in &refusals {
            eprintln!("  - {r}");
        }
    }

    let mut json = String::from("{\n");
    // How this corpus stores its temporals, carried so the runner binds the
    // form the data is actually in. Without it a DATE parameter binds as a
    // temporal against an integer column and matches nothing — the bi16
    // failure by a second route.
    json.push_str("  \"_temporal_encoding\": \"");
    json.push_str(temporal_encoding.as_str());
    json.push_str("\",\n");
    json.push_str(
        "  \"_note\": \"Derived from the corpus by snbparams. Every value was checked to \
         match a non-zero number of rows. These are NOT LDBC's curated parameters: LDBC \
         selects by percentile so each substitution costs about the same, which needs its \
         paramgen. A number measured with these may be compared with another ENGINE on the \
         same parameters, and may NOT be compared with a published LDBC figure.\"",
    );
    for (q, vals) in &out {
        json.push_str(",\n  \"");
        json.push_str(q);
        json.push_str("\": {");
        let mut first = true;
        for (k, v) in vals {
            if !first {
                json.push(',');
            }
            first = false;
            json.push_str("\n    \"");
            json.push_str(k);
            json.push_str("\": \"");
            json.push_str(&v.replace('\\', "\\\\").replace('"', "\\\""));
            json.push('"');
        }
        json.push_str("\n  }");
    }
    json.push_str("\n}\n");

    match flag("--out") {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &json) {
                eprintln!("[snbparams] cannot write {path}: {e}");
                std::process::exit(1);
            }
            eprintln!("\n[snbparams] {} quer(ies) written to {path}", out.len());
        }
        None => print!("{json}"),
    }
    // A refusal is a failure. Emitting a file with holes in it and exiting 0
    // would let a battery run on a subset and report a pass.
    if !refusals.is_empty() {
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod correlated_pick_tests {
    use super::{discovery_by_query, pair_rank, skip_to_rank};

    #[test]
    fn every_pair_the_catalogues_name_has_a_first_and_a_second() {
        for (first, second) in [
            ("tagA", "tagB"),
            ("country1", "country2"),
            ("country1Name", "country2Name"),
            ("countryXName", "countryYName"),
            ("person1Id", "person2Id"),
            ("personIdA", "personIdB"),
            ("city1Id", "city2Id"),
            ("id1", "id2"),
        ] {
            assert_eq!(pair_rank(first), 0, "{first} is a pair's first");
            assert_eq!(pair_rank(second), 1, "{second} is a pair's second");
        }
        // And a name that merely ends in a letter or a digit is no pair.
        for single in ["personId", "countryName", "tagClassName", "workFromYear", "maxDate", "month"] {
            assert_eq!(pair_rank(single), 0, "{single} is not a pair's second");
        }
    }

    #[test]
    fn only_the_OUTERMOST_limit_is_advanced() {
        let pick = "MATCH (a) WITH a ORDER BY a.d DESC LIMIT 1 MATCH (a)-->(b) RETURN b.id AS v ORDER BY b.id LIMIT 1";
        let got = skip_to_rank(pick, 2);
        assert_eq!(got.matches("SKIP").count(), 1, "exactly one SKIP: {got}");
        assert!(
            got.ends_with("SKIP 2 LIMIT 1"),
            "the ANSWER is advanced, not the anchor: {got}"
        );
        assert!(
            got.contains("ORDER BY a.d DESC LIMIT 1 MATCH"),
            "the inner anchor LIMIT must be untouched: {got}"
        );
    }

    #[test]
    fn rank_zero_leaves_the_pick_alone() {
        assert_eq!(
            skip_to_rank("RETURN 1 AS v LIMIT 1", 0),
            "RETURN 1 AS v LIMIT 1"
        );
    }

    #[test]
    fn tcr4s_two_ids_come_from_the_SAME_triangle_row() {
        // The picks must be identical except for WHICH end they return, so
        // with the same rank they describe the same (s, d) row.
        let a = discovery_by_query("tcr4", "id1").expect("id1").pick;
        let b = discovery_by_query("tcr4", "id2").expect("id2").pick;
        assert_eq!(
            a.replace("RETURN s.id AS v", "RETURN ? AS v"),
            b.replace("RETURN d.id AS v", "RETURN ? AS v"),
            "id1 and id2 must share one ordering and differ only in the column"
        );
        assert!(
            a.contains("-[:transfer]->(s)"),
            "the pick must close the triangle: {a}"
        );
    }

    #[test]
    fn bi20s_company_and_person2_come_from_the_SAME_row() {
        // Independent picks paired a company with a person no STUDY_AT route
        // reaches, and bi20 answered empty at SF10. The two picks must share
        // one ordering and differ only in the column they return.
        let c = discovery_by_query("bi20", "company").expect("company").pick;
        let p = discovery_by_query("bi20", "person2Id").expect("person2Id").pick;
        assert_eq!(
            c.replace("RETURN c.name AS v", "RETURN ? AS v"),
            p.replace("RETURN p2.id AS v", "RETURN ? AS v"),
            "company and person2Id must share one ordering and differ only in the column"
        );
        assert!(
            c.contains("[:WORK_AT]-(w:Person)-[:KNOWS]-(p2:Person)")
                && c.contains("(w)-[:STUDY_AT]->(:University)<-[:STUDY_AT]-(p2)"),
            "person2 must KNOW one of the company's workers AND share a university \
             with them -- one edge of LDBC's PathQ20: {c}"
        );
    }

    #[test]
    fn tcr6_is_discovered_by_WITHDRAW_not_by_transfer() {
        let d = discovery_by_query("tcr6", "id").expect("tcr6 has its own discovery");
        assert!(d.pick.contains("withdraw"), "{}", d.pick);
    }
}
