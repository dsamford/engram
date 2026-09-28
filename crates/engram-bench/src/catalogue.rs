//! The statement catalogue — one file, every dialect, read by Rust and Python
//! alike.
//!
//! # The failure this removes
//!
//! The LSQB battery was written three times: once in Cypher in
//! `src/bin/lsqb.rs`, once in SQL in a PostgreSQL runner script, once in Kuzu's Cypher
//! in an embedded Kuzu runner. Three implementations of "the same nine
//! queries" is three chances for one of them to ask a slightly different
//! question, and a slightly different question produces a faster number that
//! looks like an engine result. The counts agreeing is what caught that so far
//! — and a count only catches a divergence that changes the answer. A
//! divergence that changes only the WORK (a missing `LIMIT`, a join written
//! the expensive way round) leaves the count identical and the timing wrong,
//! which is worse, because nothing fails.
//!
//! So the text lives here, once, per dialect, and every driver renders from
//! this file — `lsqb.rs` included, since its cutover.
//! `tests/a_lsqb_statement_lives_once_and_matches_the_baseline.rs` refuses
//! if any statement is ever restated in `src/bin/lsqb.rs` again, and refuses
//! if the Cypher here stops being the bytes this project's recorded LSQB
//! numbers were measured with.
//!
//! # Why the file is EMBEDDED and also dumpable
//!
//! [`SOURCE`] is `include_str!`, so a binary copied to a pod carries the
//! catalogue it was compiled against and cannot be pointed at a stale one.
//! `harness catalogue --dump` writes those exact bytes out for the Python
//! executor, so the out-of-process peer gets the catalogue THIS BINARY holds
//! rather than whatever is on the pod's disk. [`digest`] travels in every
//! result document; the reporter refuses to build a comparison row out of two
//! runs whose digests differ, because that comparison is between two
//! catalogues and not between two engines.
//!
//! # What a status means
//!
//! Every per-dialect entry carries one. `verified` has been executed and its
//! answer checked against another engine's. `unverified` is transcribed and
//! has never run — a first run, not a comparison. `unsupported` carries the
//! reason the dialect cannot express the shape, declared rather than dropped:
//! the same rule `lsqb`'s `unmappable` follows, for the same reason, which is
//! that a query silently missing from a battery is a battery that scores an
//! engine on eight of nine and says nine.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_cypher::json::from_json;

/// The catalogue's bytes, compiled in.
pub const SOURCE: &str = include_str!("../catalogue/statements.json");

/// FNV-1a (64-bit) over the catalogue bytes.
///
/// A hash rather than a version number because a version number is a promise
/// somebody has to remember to keep. Two runs whose digests differ were driven
/// by different statement text, whatever their version fields say.
#[must_use]
pub fn digest() -> u64 {
    fnv1a(SOURCE.as_bytes())
}

/// FNV-1a, 64-bit. Small, dependency-free, and used only to say "the same
/// bytes" — never for anything that needs collision resistance.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Digest pairs that name the SAME statements: `(before, after)` for an edit
/// that changed only prose inside a catalogue file, never a statement.
///
/// The whole-file digest moves on any byte, and the reporter refuses a row built
/// across two digests. That is right for a statement edit and wrong for a
/// reworded note, which would otherwise retire every number recorded against
/// the old bytes. So such an edit is DECLARED here, pair by pair: the pair is
/// exact (a later edit moves the file off `after` and matches nothing), and
/// `tests/the_frozen_lsqb_digest_is_pinned.rs` pins `after` to the file.
///
/// One entry: `statements.json` on 2026-09-28 (v0.2.0), five prose notes that
/// named benchmark pods and their script paths reworded for publication
/// (`what_this_is`, `provenance/lsqb.sql`, `provenance/lsqb.cypher_ladybug`,
/// and two notes under `stress/datasets/snb`). Every statement string and the
/// document's shape were checked unchanged when the pair was declared.
pub const EQUIVALENT_DIGESTS: &[(u64, u64)] = &[(0x6d90_c151_3a6a_cd0e, 0x2cf6_26ba_1f17_8f53)];

/// Whether two digests, as a result document prints them (16 hex digits),
/// name the same statements: equal, or a pair in [`EQUIVALENT_DIGESTS`] in
/// either order.
#[must_use]
pub fn same_statements(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Ok(x), Ok(y)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else {
        return false;
    };
    EQUIVALENT_DIGESTS
        .iter()
        .any(|&(p, q)| (x, y) == (p, q) || (x, y) == (q, p))
}

/// What KIND of statements a family holds.
///
/// Not decoration: the two shapes have different INVARIANTS, and a test that
/// asserts one against the other either fails honestly or gets made to pass by
/// fabricating entries. Graphalytics kernels are PROCEDURES -- there is no SQL
/// text for BFS and never will be -- so demanding three dialect entries of
/// them would mean inventing two, which is the dishonest way to a green suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One entry per dialect (`cypher`, `cypher_ladybug`, `sql`) per query.
    Dialects,
    /// One procedure per kernel, judged by its own declared match mode.
    Procedures,
}

/// One catalogue FILE, named, with its own digest.
///
/// # Why families exist at all
///
/// [`digest`] is FNV-1a over the whole of `statements.json`, and the reporter
/// refuses to compare two runs whose digests differ. That instinct is right —
/// two runs driven by different statement text are two catalogues and not two
/// engines — but at whole-file granularity it has a second, unintended
/// consequence: ADDING a query to the file changes the digest, so every number
/// already recorded stops being comparable with anything taken afterwards. The
/// catalogue could then never grow without orphaning the project's entire
/// measurement history.
///
/// So a new battery goes in a NEW file with its OWN digest, and the refusal is
/// asked of the family two runs actually share. `statements.json` — the LSQB
/// and stress catalogue every recorded number was measured against — is frozen
/// by `tests/the_frozen_lsqb_digest_is_pinned.rs` and must not gain entries.
pub struct Family {
    /// The family's name, as it appears in a result document and in a refusal.
    /// A refusal that says only "catalogue digest differs" sends the reader to
    /// the wrong file, which is why the name is carried rather than derived.
    pub name: &'static str,
    /// That file's bytes, compiled in for the same reason [`SOURCE`] is: a
    /// binary copied to a pod carries the catalogue it was built against and
    /// cannot be pointed at a stale one.
    pub source: &'static str,
    /// Where the query objects live INSIDE that file, as a path of object
    /// keys.
    ///
    /// Carried rather than assumed because the four files do not agree: the
    /// frozen file nests its battery under `lsqb.queries`, `snb-bi` under
    /// `snb_bi.queries`, and `snb-interactive` and `finbench` put `queries` at
    /// the top level. Each was written by a different hand against a different
    /// upstream, and normalising them would mean editing a file whose digest
    /// is the thing being protected. So the reader is told the path instead of
    /// guessing it, and a guess that happened to work on three of four files
    /// is not available to be written.
    pub queries_path: &'static [&'static str],
    /// Which invariants apply to this family -- see [`Shape`].
    pub shape: Shape,
}

impl Family {
    /// FNV-1a over this family's bytes alone.
    #[must_use]
    pub fn digest(&self) -> u64 {
        fnv1a(self.source.as_bytes())
    }

    /// Parse this family's document.
    ///
    /// # Errors
    /// If the file is not a JSON object.
    pub fn load(&self) -> Result<Catalogue, CatalogueError> {
        Catalogue::parse(self.source)
    }
}

/// The frozen LSQB + stress catalogue. Same bytes as [`SOURCE`], so
/// `LSQB_STRESS.digest() == digest()` by construction and the two can never
/// drift apart.
pub const LSQB_STRESS: Family = Family {
    name: "lsqb-stress",
    source: SOURCE,
    queries_path: &["lsqb", "queries"],
    shape: Shape::Dialects,
};

/// LDBC SNB Interactive — IS1-IS7 and IC1-IC14.
pub const SNB_INTERACTIVE: Family = Family {
    name: "snb-interactive",
    source: include_str!("../catalogue/snb-interactive.json"),
    queries_path: &["queries"],
    shape: Shape::Dialects,
};

/// LDBC SNB Business Intelligence.
pub const SNB_BI: Family = Family {
    name: "snb-bi",
    source: include_str!("../catalogue/snb-bi.json"),
    queries_path: &["snb_bi", "queries"],
    shape: Shape::Dialects,
};

/// LDBC FinBench.
pub const FINBENCH: Family = Family {
    name: "finbench",
    source: include_str!("../catalogue/finbench.json"),
    queries_path: &["queries"],
    shape: Shape::Dialects,
};

/// LDBC Graphalytics — BFS, WCC, PR, SSSP, LCC, CDLP.
///
/// Its queries are PROCEDURES rather than Cypher text, so `queries_path`
/// points at the kernel table. The family exists so a Graphalytics run stamps
/// a digest a later reader can check its claims against, exactly as the other
/// four do — the report schema has carried a placeholder for it since before
/// the lane was written.
pub const GRAPHALYTICS: Family = Family {
    name: "graphalytics",
    source: include_str!("../catalogue/graphalytics.json"),
    queries_path: &["kernels"],
    shape: Shape::Procedures,
};

/// Every family this binary was compiled with.
///
/// A run stamps all of them, not just the one it drove: a document that lists
/// what the binary held is a document a later reader can check a claim
/// against, and the cost is four hex numbers.
pub const FAMILIES: &[Family] = &[LSQB_STRESS, SNB_INTERACTIVE, SNB_BI, FINBENCH, GRAPHALYTICS];

/// Look a family up by name, or `None`.
///
/// `None` rather than a fallback to [`LSQB_STRESS`]: a caller asking for a
/// family this binary does not carry must find that out, not be handed
/// somebody else's statements.
#[must_use]
pub fn family(name: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| f.name == name)
}

/// Which family a workload's statements come from.
///
/// `lsqb` and `stress` both live in the frozen file — they are two sections of
/// one document, and one digest covers both. A workload this function does not
/// know answers `None`, and the caller (see `report::compare`) then falls back
/// to the whole-file digest rather than guessing: an unknown workload is a gap
/// in this map, and a guess here would be a comparison nobody authorised.
#[must_use]
pub fn family_for_workload(workload: &str) -> Option<&'static Family> {
    match workload {
        "lsqb" | "stress" => Some(&LSQB_STRESS),
        "snb-interactive" => Some(&SNB_INTERACTIVE),
        "snb-bi" => Some(&SNB_BI),
        "finbench" => Some(&FINBENCH),
        _ => None,
    }
}

/// Which statement language a backend speaks.
///
/// Not the same thing as an engine: engram and Neo4j share [`Dialect::Cypher`]
/// and differ in everything else, which is exactly the point — one dialect,
/// one text, so a Bolt comparison cannot become a comparison of two Cyphers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    /// openCypher over Bolt — engram and Neo4j.
    Cypher,
    /// LadybugDB / Kuzu's Cypher, embedded and driven from Python.
    CypherLadybug,
    /// PostgreSQL's SQL over the v3 wire protocol.
    Sql,
    /// engram's own Cypher, for the queries where NO SINGLE TEXT serves both
    /// Bolt engines.
    ///
    /// bi15, bi19 and bi20 need a weighted shortest path, and the two engines
    /// reach it by different procedures: Neo4j through GDS, engram through
    /// `engram.algo.kshortestpaths`. The shared [`Dialect::Cypher`] entry for
    /// those three is `unsupported` and says so, with each engine's reference
    /// text parked beside it (`cypher_neo4j`, `cypher_engram`).
    ///
    /// **This does not weaken the one-text rule.** That rule exists so a Bolt
    /// comparison cannot quietly become a comparison of two Cyphers; here the
    /// texts are openly different, declared different, and a run driven in this
    /// dialect stamps `cypher_engram` in its document. A reader can see that a
    /// row was not produced from the shared text.
    CypherEngram,
}

impl Dialect {
    /// The key this dialect uses inside the catalogue.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Dialect::Cypher => "cypher",
            Dialect::CypherLadybug => "cypher_ladybug",
            Dialect::Sql => "sql",
            Dialect::CypherEngram => "cypher_engram",
        }
    }

    /// Parse a dialect name, or `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Dialect> {
        match s {
            "cypher" => Some(Dialect::Cypher),
            "cypher_ladybug" | "ladybug" | "kuzu" => Some(Dialect::CypherLadybug),
            "sql" | "postgres" => Some(Dialect::Sql),
            "cypher_engram" | "engram" => Some(Dialect::CypherEngram),
            _ => None,
        }
    }
}

/// How much a given statement text is worth trusting.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    /// Executed against the engine it names, answer cross-checked.
    Verified,
    /// Transcribed, never executed.
    Unverified,
    /// The dialect cannot express the shape; the string says why.
    Unsupported(String),
}

impl Status {
    /// Whether a run may issue this text at all.
    #[must_use]
    pub fn runnable(&self) -> bool {
        !matches!(self, Status::Unsupported(_))
    }

    /// The word that appears in a report.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Verified => "verified",
            Status::Unverified => "unverified",
            Status::Unsupported(_) => "unsupported",
        }
    }
}

/// One dialect's answer for one catalogue entry.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The statement text, with `${name}` placeholders still in it. Empty when
    /// the status is [`Status::Unsupported`].
    pub text: String,
    /// How far this text has been proven.
    pub status: Status,
}

/// The parsed catalogue.
pub struct Catalogue {
    root: BTreeMap<String, Value>,
}

/// What went wrong reading the catalogue. Every variant names the path it was
/// looking at, because "missing key" without the key is a bug report nobody
/// can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueError(String);

impl std::fmt::Display for CatalogueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "catalogue: {}", self.0)
    }
}

impl std::error::Error for CatalogueError {}

fn err<T>(what: impl Into<String>) -> Result<T, CatalogueError> {
    Err(CatalogueError(what.into()))
}

/// The type of a FinBench parameter, inferred from its NAME.
///
/// FinBench's catalogue declares `params` as bare strings, where SNB BI and
/// SNB Interactive both declare `{name, type}`. Something has to decide how
/// `startTime` is coerced, and inferring from the name is the least-bad option
/// available without editing a catalogue whose digest is pinned.
///
/// It is still an inference. The LDBC FinBench specification types these
/// explicitly, and the right fix is to carry the types in the catalogue file;
/// until then an unrecognised name is typed `STRING`, which is the only choice
/// that cannot silently turn a value into a number it was not.
#[must_use]
pub fn finbench_param_type(name: &str) -> &'static str {
    match name {
        // Every FinBench entity id is a 64-bit integer; `fbgen` mints account
        // ids from 2^62.
        "id" | "id1" | "id2" | "pid" | "pid1" | "pid2" | "aid" | "aid1" | "aid2" | "cid"
        | "cid1" | "cid2" | "loanId" | "accountId" | "personId" | "companyId" => "ID",
        // The window bounds are epoch milliseconds in FinBench's own CSVs.
        "startTime" | "endTime" | "time" | "timestamp" => "DATETIME",
        "threshold" | "amountThreshold" | "ratioThreshold" => "FLOAT",
        "truncationLimit" | "limit" | "k" => "INT",
        "truncationOrder" => "STRING",
        _ => "STRING",
    }
}

/// The key one curated variant is recorded under.
///
/// LDBC labels a variant with the query's own number plus a suffix — `bi10`'s
/// two variants are `10a` and `10b` — so concatenating the query name and the
/// label yields `bi1010a`. The number belongs to the query, not to the label,
/// so the query's trailing digits are dropped and the label supplies them.
///
/// Shared by the runner and by `snbparams` on purpose: they are the writer and
/// the reader of the same parameter file, and a key built two ways is a
/// parameter file that silently binds nothing.
#[must_use]
pub fn variant_key(query: &str, label: &str) -> String {
    let prefix: String = query
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string();
    // A label that does not start with the query's number is a bare suffix
    // (or something unexpected); keep the whole query name in that case so the
    // key stays unique rather than becoming just the prefix.
    if label.starts_with(|c: char| c.is_ascii_digit()) {
        format!("{prefix}{label}")
    } else {
        format!("{query}{label}")
    }
}

impl Catalogue {
    /// Parse the compiled-in catalogue.
    ///
    /// # Errors
    /// If the embedded document is not a JSON object. That is a build-time
    /// defect rather than a runtime condition, but it is reported rather than
    /// panicked so a caller can say which binary is broken.
    pub fn load() -> Result<Catalogue, CatalogueError> {
        Self::parse(SOURCE)
    }

    /// Parse an explicit catalogue document — the path a test uses to check
    /// that a dumped copy is the copy that was compiled in.
    ///
    /// # Errors
    /// If `src` is not a JSON object.
    pub fn parse(src: &str) -> Result<Catalogue, CatalogueError> {
        match from_json(src) {
            Ok(Value::Map(root)) => Ok(Catalogue { root }),
            Ok(other) => err(format!("document is not an object: {other:?}")),
            Err(e) => err(format!("document is not JSON: {e}")),
        }
    }

    fn map_at(&self, path: &[&str]) -> Result<&BTreeMap<String, Value>, CatalogueError> {
        let mut cur = &self.root;
        for (i, seg) in path.iter().enumerate() {
            match cur.get(*seg) {
                Some(Value::Map(m)) => cur = m,
                Some(other) => {
                    return err(format!(
                        "{} is not an object ({other:?})",
                        path[..=i].join(".")
                    ));
                }
                None => return err(format!("no such entry: {}", path[..=i].join("."))),
            }
        }
        Ok(cur)
    }

    /// Read one `{status, text|body|reason}` entry.
    fn entry_from(
        m: &BTreeMap<String, Value>,
        dialect: Dialect,
        where_: &str,
    ) -> Result<Entry, CatalogueError> {
        let Some(Value::Map(d)) = m.get(dialect.key()) else {
            // A dialect that simply is not present is NOT the same as one
            // declared unsupported, and the difference matters: the first is a
            // gap nobody noticed, the second is a decision somebody recorded.
            // Only the second may pass quietly.
            return err(format!(
                "{where_} has no `{}` entry — a missing dialect is a gap, not a \
                 declared exclusion; add it with a status",
                dialect.key()
            ));
        };
        let status = match d.get("status") {
            Some(Value::Str(s)) if s == "verified" => Status::Verified,
            Some(Value::Str(s)) if s == "unverified" => Status::Unverified,
            Some(Value::Str(s)) if s == "unsupported" => {
                let Some(Value::Str(reason)) = d.get("reason") else {
                    return err(format!(
                        "{where_}.{}: unsupported with no reason",
                        dialect.key()
                    ));
                };
                Status::Unsupported(reason.clone())
            }
            other => {
                return err(format!(
                    "{where_}.{}: status must be verified/unverified/unsupported, got {other:?}",
                    dialect.key()
                ));
            }
        };
        let text = match (&status, d.get("text"), d.get("body")) {
            (Status::Unsupported(_), _, _) => String::new(),
            (_, Some(Value::Str(t)), _) => t.clone(),
            (_, None, Some(Value::Str(b))) => b.clone(),
            _ => {
                return err(format!(
                    "{where_}.{}: a runnable entry needs `text` or `body`",
                    dialect.key()
                ));
            }
        };
        Ok(Entry { text, status })
    }

    /// The query names under an arbitrary catalogue path, in the document's
    /// own key order.
    ///
    /// The generic form of [`Catalogue::lsqb_names`], for the families whose
    /// batteries do not live at `lsqb.queries`. It is a separate function
    /// rather than a rewrite of that one because `lsqb_names` is on the path
    /// every recorded number was taken through, and the SF10 campaign in
    /// flight is not a good moment to re-route it.
    ///
    /// # Errors
    /// If the path does not name an object.
    pub fn query_names(&self, path: &[&str]) -> Result<Vec<String>, CatalogueError> {
        Ok(self.map_at(path)?.keys().cloned().collect())
    }

    /// One query at an arbitrary catalogue path, in one dialect.
    ///
    /// Unlike [`Catalogue::lsqb`] this performs NO SQL wrapping: the LSQB
    /// family stores a `FROM …` body that `sql_count`/`sql_probe` complete,
    /// and the three LDBC families store whole statements. Wrapping a whole
    /// statement in `SELECT count(*) FROM (…)` would run, and would answer a
    /// different question than the battery asks — which is the failure the
    /// catalogue exists to prevent, so it is refused by not being offered.
    ///
    /// # Errors
    /// If the query or its dialect entry is absent or malformed.
    pub fn query(
        &self,
        path: &[&str],
        name: &str,
        dialect: Dialect,
    ) -> Result<Entry, CatalogueError> {
        let mut p: Vec<&str> = path.to_vec();
        p.push(name);
        let m = self.map_at(&p)?;
        Self::entry_from(m, dialect, &format!("{}.{name}", path.join(".")))
    }

    /// The declared substitution-parameter variants for one query.
    ///
    /// Returns `(variant label, declared parameters)` per variant, in the
    /// catalogue's own order. LDBC defines several queries in two or three
    /// variants that differ ONLY in how their parameters were curated — bi16a
    /// takes the 99.5th percentile where bi16b takes the 45th — so the variant
    /// is part of the query's identity and a lane that collapsed them would be
    /// reporting one number for two questions.
    ///
    /// A query with no `variants` key takes no parameters, which is a real
    /// answer (LSQB's nine are all unparameterised) and not an error.
    ///
    /// # Errors
    /// If `variants` is present but is not an array of objects, or a declared
    /// parameter is missing its `name` or `type`. A malformed declaration is
    /// refused rather than skipped: a parameter silently dropped here is a
    /// parameter the runner never binds.
    pub fn variants(
        &self,
        path: &[&str],
        query: &str,
    ) -> Result<Vec<(String, Vec<crate::params::ParamSpec>)>, CatalogueError> {
        let mut p: Vec<&str> = path.to_vec();
        p.push(query);
        let m = self.map_at(&p)?;

        // ── Shape 1: snb-bi -- `variants[].parameters[{name,type}]` ──────
        if let Some(vs) = m.get("variants") {
            let Value::List(vs) = vs else {
                return err(format!("{query}.variants is not an array"));
            };
            let mut out = Vec::new();
            for (i, v) in vs.iter().enumerate() {
                let Value::Map(vm) = v else {
                    return err(format!("{query}.variants[{i}] is not an object"));
                };
                let label = match vm.get("variant") {
                    Some(Value::Str(s)) => s.clone(),
                    Some(Value::Int(n)) => n.to_string(),
                    _ => i.to_string(),
                };
                out.push((label, Self::specs_from(vm.get("parameters"), query, i)?));
            }
            return Ok(out);
        }

        // ── Shape 2: snb-interactive -- `parameters[{name,type}]` ────────
        //
        // One unlabelled variant. Interactive's queries are not curated into
        // an `a`/`b` pair the way several BI queries are.
        if let Some(ps) = m.get("parameters") {
            return Ok(vec![(String::new(), Self::specs_from(Some(ps), query, 0)?)]);
        }

        // ── Shape 3: finbench -- `params: ["id", "startTime"]` ───────────
        //
        // Bare NAMES with no types. The type is inferred from the name by
        // `finbench_param_type`, and that is a gap in the catalogue rather
        // than a feature of this reader: the other two families declare their
        // types, and an inferred type is a guess about how a value must be
        // coerced. Recorded here so the next reader fixes the catalogue
        // instead of trusting the inference.
        if let Some(Value::List(ps)) = m.get("params") {
            let mut specs = Vec::new();
            for (j, pv) in ps.iter().enumerate() {
                let Value::Str(name) = pv else {
                    return err(format!("{query}.params[{j}] is not a string"));
                };
                specs.push(crate::params::ParamSpec {
                    name: name.clone(),
                    ty: finbench_param_type(name).to_string(),
                });
            }
            return Ok(vec![(String::new(), specs)]);
        }

        // No declaration at all: LSQB's nine take no parameters, which is a
        // real answer and not an error.
        Ok(Vec::new())
    }

    /// Read a `[{name, type}]` array into specs.
    fn specs_from(
        v: Option<&Value>,
        query: &str,
        i: usize,
    ) -> Result<Vec<crate::params::ParamSpec>, CatalogueError> {
        let mut specs = Vec::new();
        let Some(Value::List(ps)) = v else {
            return Ok(specs);
        };
        for (j, pv) in ps.iter().enumerate() {
            let Value::Map(pm) = pv else {
                return err(format!("{query}[{i}].parameters[{j}] is not an object"));
            };
            let (Some(Value::Str(name)), Some(Value::Str(ty))) = (pm.get("name"), pm.get("type"))
            else {
                return err(format!(
                    "{query}[{i}].parameters[{j}] is missing `name` or `type`"
                ));
            };
            specs.push(crate::params::ParamSpec {
                name: name.clone(),
                ty: ty.clone(),
            });
        }
        Ok(specs)
    }

    /// The nine LSQB query names, in table order.
    ///
    /// # Errors
    /// If the `lsqb.queries` object is missing.
    pub fn lsqb_names(&self) -> Result<Vec<String>, CatalogueError> {
        Ok(self.map_at(&["lsqb", "queries"])?.keys().cloned().collect())
    }

    /// One LSQB query in one dialect. For [`Dialect::Sql`] the text is the
    /// `FROM …` body; wrap it with [`Catalogue::sql_count`] or
    /// [`Catalogue::sql_probe`].
    ///
    /// # Errors
    /// If the query or its dialect entry is absent or malformed.
    pub fn lsqb(&self, query: &str, dialect: Dialect) -> Result<Entry, CatalogueError> {
        let m = self.map_at(&["lsqb", "queries", query])?;
        Self::entry_from(m, dialect, &format!("lsqb.queries.{query}"))
    }

    /// The count statement for a SQL body.
    ///
    /// # Errors
    /// If `lsqb.sql_shape.count` is absent.
    pub fn sql_count(&self, body: &str) -> Result<String, CatalogueError> {
        let m = self.map_at(&["lsqb", "sql_shape"])?;
        match m.get("count") {
            Some(Value::Str(t)) => Ok(render(t, &[("body", body)])),
            _ => err("lsqb.sql_shape.count is missing"),
        }
    }

    /// The existence probe for a SQL body.
    ///
    /// # Errors
    /// If `lsqb.sql_shape.probe` is absent.
    pub fn sql_probe(&self, body: &str) -> Result<String, CatalogueError> {
        let m = self.map_at(&["lsqb", "sql_shape"])?;
        match m.get("probe") {
            Some(Value::Str(t)) => Ok(render(t, &[("body", body)])),
            _ => err("lsqb.sql_shape.probe is missing"),
        }
    }

    /// The expected count for a query on a named corpus, if one is recorded.
    ///
    /// An absent entry carries NO expectation. It is never read as an
    /// expectation of zero — that is the `unverified_zero` trap, one level up.
    ///
    /// # Errors
    /// If the query is absent, or a recorded expectation is not an integer.
    pub fn lsqb_expected(&self, query: &str, corpus: &str) -> Result<Option<i64>, CatalogueError> {
        let m = self.map_at(&["lsqb", "queries", query])?;
        match m.get("expected") {
            None => Ok(None),
            Some(Value::Map(e)) => match e.get(corpus) {
                None => Ok(None),
                Some(Value::Int(n)) => Ok(Some(*n)),
                Some(other) => err(format!(
                    "lsqb.queries.{query}.expected.{corpus} is not an integer ({other:?})"
                )),
            },
            Some(other) => err(format!(
                "lsqb.queries.{query}.expected is not an object ({other:?})"
            )),
        }
    }

    /// The declared join skeleton for a query: the relationship-type multiset
    /// and the counts of optional legs, anti-joins and inequality predicates.
    ///
    /// This is what makes the SQL/Cypher pair CHECKABLE rather than merely
    /// stated — see `tests/a_catalogue_dialect_pair_asks_the_same_question.rs`.
    ///
    /// # Errors
    /// If the skeleton is absent or malformed.
    pub fn lsqb_skeleton(&self, query: &str) -> Result<Skeleton, CatalogueError> {
        let m = self.map_at(&["lsqb", "queries", query, "skeleton"])?;
        let types = match m.get("types") {
            Some(Value::List(l)) => l
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(CatalogueError(format!("skeleton type {other:?}"))),
                })
                .collect::<Result<Vec<String>, CatalogueError>>()?,
            _ => return err(format!("lsqb.queries.{query}.skeleton.types is missing")),
        };
        let num = |k: &str| -> Result<usize, CatalogueError> {
            match m.get(k) {
                Some(Value::Int(n)) if *n >= 0 => Ok(*n as usize),
                other => err(format!(
                    "lsqb.queries.{query}.skeleton.{k} must be a non-negative integer ({other:?})"
                )),
            }
        };
        Ok(Skeleton {
            types,
            optional: num("optional")?,
            anti: num("anti")?,
            inequalities: num("inequalities")?,
        })
    }

    /// The SQL table name to relationship type map, for the derived supertype
    /// and symmetric-`KNOWS` tables the relational schema materialises.
    ///
    /// # Errors
    /// If `lsqb.table_alias` is absent.
    pub fn table_alias(&self) -> Result<BTreeMap<String, String>, CatalogueError> {
        let m = self.map_at(&["lsqb", "table_alias"])?;
        Ok(m.iter()
            .filter(|(k, _)| k.as_str() != "note")
            .filter_map(|(k, v)| match v {
                Value::Str(s) => Some((k.clone(), s.clone())),
                _ => None,
            })
            .collect())
    }

    /// The LSQB census statements for a dialect: the all-node count and the
    /// `:Person` count.
    ///
    /// An empty corpus answers 0 to every query and would pass vacuously, so a
    /// run against one has to fail before measuring anything. Two statements
    /// rather than one because a corpus with nodes but no persons is a corpus
    /// loaded from the wrong export, and the whole battery would then answer 0
    /// legitimately.
    ///
    /// # Errors
    /// If the census block, or this dialect's entry in it, is absent.
    pub fn lsqb_census(&self, dialect: Dialect) -> Result<(String, String), CatalogueError> {
        let m = self.map_at(&["lsqb", "census", dialect.key()])?;
        let get = |k: &str| match m.get(k) {
            Some(Value::Str(s)) => Ok(s.clone()),
            other => err(format!("lsqb.census.{}.{k} = {other:?}", dialect.key())),
        };
        Ok((get("nodes")?, get("persons")?))
    }

    /// One stress read shape in one dialect.
    ///
    /// # Errors
    /// If the shape or its dialect entry is absent or malformed.
    pub fn read_shape(&self, shape: &str, dialect: Dialect) -> Result<Entry, CatalogueError> {
        let m = self.map_at(&["stress", "read_shapes", shape])?;
        Self::entry_from(m, dialect, &format!("stress.read_shapes.{shape}"))
    }

    /// Every read-shape name the catalogue declares.
    ///
    /// # Errors
    /// If `stress.read_shapes` is absent.
    pub fn read_shape_names(&self) -> Result<Vec<String>, CatalogueError> {
        Ok(self
            .map_at(&["stress", "read_shapes"])?
            .keys()
            .filter(|k| !k.starts_with('_'))
            .cloned()
            .collect())
    }

    /// One stress write op, for a corpus family, in one dialect.
    ///
    /// The op name is dataset-free — a plan names the corpus once, in its
    /// header — so the dispatch happens here: the `<family>` group if the op
    /// renders differently per corpus, `any` if it does not. A missing group
    /// is an error rather than a fallback, because "this op has no text for
    /// this corpus" is a gap and quietly rendering the other corpus's text
    /// would be a workload nobody chose.
    ///
    /// # Errors
    /// If the op, its family group, or its dialect entry is absent or
    /// malformed.
    pub fn write_op(
        &self,
        op: &str,
        family: &str,
        dialect: Dialect,
    ) -> Result<Entry, CatalogueError> {
        let group = self.map_at(&["stress", "write_ops", op])?;
        let (key, m) = match (group.get(family), group.get("any")) {
            (Some(Value::Map(m)), _) => (family, m),
            (None, Some(Value::Map(m))) => ("any", m),
            _ => {
                return err(format!(
                    "stress.write_ops.{op} has neither a `{family}` group nor an `any` one"
                ));
            }
        };
        Self::entry_from(m, dialect, &format!("stress.write_ops.{op}.{key}"))
    }

    /// Every write-op name the catalogue declares.
    ///
    /// # Errors
    /// If `stress.write_ops` is absent.
    pub fn write_op_names(&self) -> Result<Vec<String>, CatalogueError> {
        Ok(self
            .map_at(&["stress", "write_ops"])?
            .keys()
            .filter(|k| !k.starts_with('_'))
            .cloned()
            .collect())
    }

    /// One integrity probe in one dialect.
    ///
    /// # Errors
    /// If the probe or its dialect entry is absent or malformed.
    pub fn integrity_probe(&self, probe: &str, dialect: Dialect) -> Result<Entry, CatalogueError> {
        let m = self.map_at(&["stress", "integrity_probes", probe])?;
        Self::entry_from(m, dialect, &format!("stress.integrity_probes.{probe}"))
    }

    /// A dataset-level string list — `cypher_seed`, `sql_seed` — or an empty
    /// vector when the dataset declares none.
    ///
    /// # Errors
    /// If a member is not a string.
    pub fn dataset_list(&self, dataset: &str, key: &str) -> Result<Vec<String>, CatalogueError> {
        let Ok(m) = self.map_at(&["stress", "datasets", dataset]) else {
            return Ok(Vec::new());
        };
        match m.get(key) {
            None => Ok(Vec::new()),
            Some(Value::List(l)) => l
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(CatalogueError(format!(
                        "stress.datasets.{dataset}.{key} member {other:?} is not a string"
                    ))),
                })
                .collect(),
            Some(other) => err(format!(
                "stress.datasets.{dataset}.{key} is not a list ({other:?})"
            )),
        }
    }

    /// A dataset-level scalar — `cypher_attach`, `cypher_census`.
    ///
    /// # Errors
    /// If it is present but not a string.
    pub fn dataset_str(&self, dataset: &str, key: &str) -> Result<Option<String>, CatalogueError> {
        let Ok(m) = self.map_at(&["stress", "datasets", dataset]) else {
            return Ok(None);
        };
        match m.get(key) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(other) => err(format!(
                "stress.datasets.{dataset}.{key} is not a string ({other:?})"
            )),
        }
    }

    /// A fixture list — `indexes`, `probes` or `setup` — for a dataset in a
    /// dialect. An absent list is empty, which is a real answer here: the
    /// synthetic dataset deliberately has no index probe, because its index is
    /// built incrementally by the seeding writes and a probe would time
    /// nothing while looking like a measurement.
    ///
    /// # Errors
    /// If the fixture group is absent, or a member is not a string.
    pub fn fixture(
        &self,
        group: &str,
        dialect: Dialect,
        list: &str,
    ) -> Result<Vec<String>, CatalogueError> {
        let m = match self.map_at(&["stress", "fixtures", group, dialect.key()]) {
            Ok(m) => m,
            // A dialect with no fixtures for a group needs none.
            Err(_) => return Ok(Vec::new()),
        };
        match m.get(list) {
            None => Ok(Vec::new()),
            Some(Value::List(l)) => l
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(CatalogueError(format!(
                        "stress.fixtures.{group}.{}.{list} member {other:?} is not a string",
                        dialect.key()
                    ))),
                })
                .collect(),
            Some(other) => err(format!(
                "stress.fixtures.{group}.{}.{list} is not a list ({other:?})",
                dialect.key()
            )),
        }
    }
}

/// A query's declared join shape, independent of dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skeleton {
    /// The relationship types the pattern traverses, one entry per traversal,
    /// sorted. A multiset, not a set: q3 walks `KNOWS` three times and a set
    /// would call that the same shape as walking it once.
    pub types: Vec<String>,
    /// How many optional legs (`OPTIONAL MATCH` / `LEFT JOIN`).
    pub optional: usize,
    /// How many anti-joins (`NOT (…)` / `NOT EXISTS`).
    pub anti: usize,
    /// How many inequality predicates (`a <> b`).
    pub inequalities: usize,
}

/// Substitute `${name}` placeholders.
///
/// Deliberately not a template language: a benchmark's statement text is data,
/// and a data file that can branch is a data file that can be wrong in a way
/// nobody reads. An unknown placeholder is LEFT IN PLACE rather than replaced
/// with an empty string — a statement that still contains `${` fails at the
/// engine, loudly, instead of silently becoming a different query.
#[must_use]
pub fn render(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(at) = rest.find("${") {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        if let Some(close) = tail[2..].find('}') {
            let name = &tail[2..2 + close];
            if let Some((_, v)) = params.iter().find(|(k, _)| *k == name) {
                out.push_str(v);
                rest = &tail[2 + close + 1..];
                continue;
            }
        }
        out.push_str("${");
        rest = &tail[2..];
    }
    out.push_str(rest);
    out
}

/// Whether a rendered statement still carries an unsubstituted placeholder.
///
/// Called before every send. A `${key}` that reached the wire is a binding the
/// plan did not carry, and the engine's syntax error is a much worse way to
/// find that out than this is.
#[must_use]
pub fn has_unbound(rendered: &str) -> bool {
    rendered.contains("${")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalogue_parses_and_holds_nine_lsqb_queries() {
        let c = Catalogue::load().expect("the compiled-in catalogue must parse");
        let names = c.lsqb_names().expect("lsqb.queries");
        assert_eq!(names.len(), 9, "LSQB defines nine queries, got {names:?}");
        for n in &names {
            for d in [Dialect::Cypher, Dialect::CypherLadybug, Dialect::Sql] {
                let e = c
                    .lsqb(n, d)
                    .unwrap_or_else(|err| panic!("{n}/{}: {err}", d.key()));
                assert!(
                    e.status.runnable() && !e.text.is_empty(),
                    "{n}/{}: every LSQB dialect must be runnable",
                    d.key()
                );
            }
        }
    }

    #[test]
    fn render_leaves_an_unknown_placeholder_in_place() {
        assert_eq!(render("a ${x} b", &[("x", "1")]), "a 1 b");
        // The whole point: an unbound name must NOT vanish.
        assert_eq!(render("a ${x} b", &[]), "a ${x} b");
        assert!(has_unbound(&render("a ${x} b", &[])));
        assert!(!has_unbound(&render("a ${x} b", &[("x", "1")])));
        // A lone `$` or `{` is literal.
        assert_eq!(render("$ { } ${", &[("x", "1")]), "$ { } ${");
        // Repeats all substitute.
        assert_eq!(render("${x}${x}", &[("x", "z")]), "zz");
    }

    #[test]
    fn a_missing_dialect_is_an_error_and_a_declared_one_is_not() {
        let c = Catalogue::parse(
            r#"{"stress":{"read_shapes":{
                 "s1":{"cypher":{"status":"verified","text":"A"}},
                 "s2":{"cypher":{"status":"unsupported","reason":"because"}}}}}"#,
        )
        .expect("parse");
        assert_eq!(c.read_shape("s1", Dialect::Cypher).unwrap().text, "A");
        // Absent — a gap nobody noticed.
        assert!(c.read_shape("s1", Dialect::Sql).is_err());
        // Present and declared — a decision somebody recorded.
        let e = c.read_shape("s2", Dialect::Cypher).unwrap();
        assert_eq!(e.status, Status::Unsupported("because".to_string()));
        assert!(!e.status.runnable());
    }

    #[test]
    fn an_unsupported_entry_without_a_reason_refuses() {
        let c = Catalogue::parse(
            r#"{"stress":{"read_shapes":{"s":{"cypher":{"status":"unsupported"}}}}}"#,
        )
        .expect("parse");
        assert!(c.read_shape("s", Dialect::Cypher).is_err());
    }

    #[test]
    fn every_family_parses_and_the_frozen_one_is_the_whole_file() {
        // The frozen family must BE `SOURCE`, not a copy of it: two sets of
        // bytes that are equal today are two sets of bytes that can diverge.
        assert_eq!(LSQB_STRESS.digest(), digest());
        for f in FAMILIES {
            f.load()
                .unwrap_or_else(|e| panic!("family {} does not parse: {e}", f.name));
            assert_eq!(family(f.name).map(|g| g.name), Some(f.name));
        }
        assert!(family("no-such-family").is_none());
        assert_eq!(
            family_for_workload("lsqb").map(|f| f.name),
            Some("lsqb-stress")
        );
        assert_eq!(
            family_for_workload("stress").map(|f| f.name),
            Some("lsqb-stress")
        );
        // An unknown workload answers nothing rather than the first family.
        assert!(family_for_workload("something-new").is_none());
    }

    #[test]
    fn the_digest_moves_when_the_bytes_move() {
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_eq!(fnv1a(SOURCE.as_bytes()), digest());
    }
}

/// The declared-parameter reader, against the real shipped catalogues.
#[cfg(test)]
mod variant_tests {
    use super::{FINBENCH, LSQB_STRESS, SNB_BI, SNB_INTERACTIVE};

    #[test]
    fn bi16_declares_five_parameters_in_two_variants() {
        let cat = SNB_BI.load().unwrap();
        let vs = cat.variants(&["snb_bi", "queries"], "bi16").unwrap();
        assert_eq!(vs.len(), 2, "bi16 is defined in two curated variants");
        assert_eq!(vs[0].0, "16a");
        let names: Vec<&str> = vs[0].1.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["tagA", "dateA", "tagB", "dateB", "maxKnowsLimit"]
        );
        // The types are what drives coercion — a DATE that read as a STRING is
        // the bi16 failure.
        //
        // `dateA` is a DATETIME, and that is a CORRECTION: the catalogue
        // declared it (and nineteen others across ten queries) as a DATE,
        // where LDBC's own `:params` block supplies `datetime('2012-09-16')`
        // and the query compares directly against `message.creationDate`,
        // which is a DATETIME. Bound as a DATE against a typed-temporal
        // corpus every one of them matched ZERO rows — an empty result
        // indistinguishable from a working query, measured on SF3 on
        // 2026-09-21.
        assert_eq!(
            vs[0].1[1].ty, "DATETIME",
            "dateA follows LDBC's own example"
        );
        assert_eq!(vs[0].1[4].ty, "INT");
    }

    #[test]
    fn bi12_declares_the_string_array_that_matched_nothing() {
        let cat = SNB_BI.load().unwrap();
        let vs = cat.variants(&["snb_bi", "queries"], "bi12").unwrap();
        let langs = vs[0]
            .1
            .iter()
            .find(|s| s.name == "languages")
            .expect("bi12 declares `languages`");
        assert_eq!(langs.ty, "STRING[]");
    }

    #[test]
    fn every_snb_bi_query_declares_readable_variants() {
        // A parse failure anywhere here means a query the lane could not bind,
        // and would have to skip — which is how a battery quietly shrinks.
        let cat = SNB_BI.load().unwrap();
        for name in cat.query_names(&["snb_bi", "queries"]).unwrap() {
            let vs = cat
                .variants(&["snb_bi", "queries"], &name)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!vs.is_empty(), "{name} declares no variant");
        }
    }

    #[test]
    fn every_finbench_query_declares_readable_variants() {
        let cat = FINBENCH.load().unwrap();
        for name in cat.query_names(&["queries"]).unwrap() {
            cat.variants(&["queries"], &name)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn no_parameter_is_declared_date_where_ldbc_supplies_a_datetime() {
        // The defect this pins, stated once: a parameter declared DATE binds
        // as a date, `message.creationDate` is a DATETIME, and the comparison
        // matches nothing — fast, well-formed, and indistinguishable from a
        // working query. Twenty of them were wrong at once, so a one-off fix
        // without a guard would drift back the next time the file is edited.
        //
        // The evidence is the catalogue's own text: LDBC ships an example
        // `:params` block in every query, and where it writes `datetime(...)`
        // the declared type must not be DATE.
        let cat = SNB_BI.load().unwrap();
        let src = SNB_BI.source;
        let mut bad = Vec::new();
        for name in cat.query_names(&["snb_bi", "queries"]).unwrap() {
            for (label, specs) in cat.variants(&["snb_bi", "queries"], &name).unwrap() {
                for s in specs.iter().filter(|s| s.ty == "DATE") {
                    // Look for `<param>: datetime(` anywhere in the family's
                    // source — the example blocks are the only place it
                    // appears beside a parameter name.
                    let needle = format!("{}: datetime(", s.name);
                    if src.contains(&needle) {
                        bad.push(format!("{name}{label}.{}", s.name));
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "declared DATE where LDBC's example supplies a datetime(): {bad:?}"
        );
    }

    #[test]
    fn snb_interactive_declares_its_parameters_without_a_variants_wrapper() {
        // Shape 2. The three catalogues were written by different hands
        // against different upstreams, and a reader that only knew bi's shape
        // silently reported ZERO parameters for all 21 Interactive queries --
        // which reads as "takes none" rather than "I could not tell".
        let cat = SNB_INTERACTIVE.load().unwrap();
        let vs = cat.variants(&["queries"], "IC1").unwrap();
        assert_eq!(vs.len(), 1, "Interactive queries are not curated in pairs");
        let names: Vec<&str> = vs[0].1.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["personId", "firstName"]);
        assert_eq!(vs[0].1[0].ty, "ID");
    }

    #[test]
    fn every_snb_interactive_query_declares_at_least_one_parameter() {
        // All 21 are parameterised. A family reporting none anywhere is the
        // reader failing, not the catalogue.
        let cat = SNB_INTERACTIVE.load().unwrap();
        for name in cat.query_names(&["queries"]).unwrap() {
            let vs = cat.variants(&["queries"], &name).unwrap();
            let n: usize = vs.iter().map(|(_, s)| s.len()).sum();
            assert!(n > 0, "{name} reported no parameters");
        }
    }

    #[test]
    fn finbench_parameters_are_named_but_untyped_and_the_type_is_inferred() {
        // Shape 3, and a CATALOGUE GAP: finbench declares bare names where the
        // other two declare {name, type}. The inference is documented and
        // tested so it is visible rather than accidental.
        let cat = FINBENCH.load().unwrap();
        let vs = cat.variants(&["queries"], "tcr1").unwrap();
        let names: Vec<&str> = vs[0].1.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["id", "startTime", "endTime"]);
        assert_eq!(vs[0].1[0].ty, "ID");
        assert_eq!(vs[0].1[1].ty, "DATETIME");
    }

    #[test]
    fn the_engram_dialect_carries_bi19_and_bi20_where_the_shared_one_cannot() {
        // bi15/19/20 need a weighted shortest path, and the two Bolt engines
        // reach it by different procedures — GDS on Neo4j,
        // `engram.algo.kshortestpaths` on engram. The shared `cypher` entry is
        // therefore `unsupported`, which is correct and must STAY correct:
        // that is the rule stopping a Bolt comparison from quietly becoming a
        // comparison of two Cyphers.
        let cat = SNB_BI.load().unwrap();
        for q in ["bi15", "bi19", "bi20"] {
            let shared = cat
                .query(&["snb_bi", "queries"], q, super::Dialect::Cypher)
                .unwrap();
            assert!(
                matches!(shared.status, super::Status::Unsupported(_)),
                "{q}'s SHARED cypher entry must stay unsupported"
            );
            let engram = cat
                .query(&["snb_bi", "queries"], q, super::Dialect::CypherEngram)
                .unwrap_or_else(|e| panic!("{q} has no cypher_engram text: {e}"));
            assert!(
                engram.text.contains("engram.algo.kshortestpaths"),
                "{q}'s engram text must use the weighted path procedure"
            );
        }
        // bi19 and bi20 read a weight their (untimed) precomputation
        // materialised, as LDBC's own arms read a precomputed table.
        for q in ["bi19", "bi20"] {
            let engram = cat
                .query(&["snb_bi", "queries"], q, super::Dialect::CypherEngram)
                .unwrap();
            assert!(
                engram.text.contains("relationshipWeightProperty"),
                "{q}'s engram text must read the materialised weight"
            );
        }
        // bi15 builds its projection INSIDE the timed statement, as LDBC's own
        // does — and, as LDBC's own does, IN MEMORY: `engram.algo.project` from
        // the weighting join's rows. It used to DELETE and CREATE a
        // relationship per KNOWS pair, which at SF10 is a join inside a writing
        // statement (the shape that reached 109 GB for one slice of bi20's
        // precomputation). Its text must therefore WRITE NOTHING.
        let bi15 = cat
            .query(&["snb_bi", "queries"], "bi15", super::Dialect::CypherEngram)
            .unwrap();
        assert!(
            bi15.text.contains("engram.algo.project"),
            "bi15 must build its projection from rows"
        );
        assert!(
            bi15.text.contains("projection: projection"),
            "bi15's path call must read the projection it built"
        );
        for write in ["DELETE", "CREATE", "SET ", "MERGE"] {
            assert!(
                !bi15.text.contains(write),
                "bi15's engram text must write nothing, and it contains `{write}`"
            );
        }
        assert!(
            bi15.text.contains("-1.0"),
            "bi15 returns -1.0 when no path exists; an EMPTY result is a different answer"
        );
        // The weights are edge-centric: every KNOWS pair at 1.0 AND each
        // interacting pair again at 1 / (w + 1), two parallel edges of which the
        // path takes the cheaper. That is the pair-centric weight ONLY for a
        // single shortest path (engram-graph's
        // `bi15s_edge_centric_weights_answer_as_the_pair_centric_ones`); a k > 1
        // search would count the parallel edges as distinct routes.
        if bi15.text.contains("knows + interacting") {
            assert!(
                bi15.text.contains("k: 1"),
                "bi15's parallel-edge weights hold for k = 1 only"
            );
        }

        // bi10's reference needs `apoc.path.subgraphNodes`; its engram text
        // answers the same depth band with one BFS. The equivalence is proven
        // in engram-graph's `a_bfs_depth_band_is_the_subgraph_nodes_difference`
        // for exactly this spelling — UNDIRECTED, and banded on `depth`.
        let bi10 = cat
            .query(&["snb_bi", "queries"], "bi10", super::Dialect::CypherEngram)
            .unwrap_or_else(|e| panic!("bi10 has no cypher_engram text: {e}"));
        for needle in [
            "engram.algo.bfs.stream",
            "orientation: 'UNDIRECTED'",
            "depth >= $minPathDistance AND depth <= $maxPathDistance",
        ] {
            assert!(bi10.text.contains(needle), "bi10's engram text lost `{needle}`");
        }
        assert!(
            !bi10.text.contains("apoc."),
            "bi10's engram text must not need APOC"
        );

        // And the dialect round-trips through its name.
        assert_eq!(
            super::Dialect::parse("cypher_engram"),
            Some(super::Dialect::CypherEngram)
        );
        assert_eq!(super::Dialect::CypherEngram.key(), "cypher_engram");
    }

    #[test]
    fn every_finbench_id_shaped_name_is_typed_id_not_string() {
        // MEASURED, on the FinBench SF1 corpus 2026-09-21: `tcr10.pid1` and
        // `pid2` fell through to STRING, so `{id: $v}` compared an integer
        // property to a quoted string and matched ZERO rows — a fast, empty,
        // well-formed answer. Every id-shaped name in the family is pinned.
        for n in [
            "id",
            "id1",
            "id2",
            "pid",
            "pid1",
            "pid2",
            "aid",
            "cid",
            "loanId",
            "accountId",
            "personId",
            "companyId",
        ] {
            assert_eq!(
                super::finbench_param_type(n),
                "ID",
                "{n} must bind as an id"
            );
        }
    }

    #[test]
    fn an_unrecognised_finbench_name_is_typed_string_not_guessed_numeric() {
        // STRING is the only inference that cannot turn a value into a number
        // it never was.
        assert_eq!(super::finbench_param_type("somethingNew"), "STRING");
        assert_eq!(super::finbench_param_type("id"), "ID");
    }

    #[test]
    fn an_unparameterised_family_reports_no_parameters_rather_than_failing() {
        // LSQB's nine take none. That is an answer, not an error.
        let cat = LSQB_STRESS.load().unwrap();
        let vs = cat.variants(&["lsqb", "queries"], "q1").unwrap();
        assert!(vs.iter().all(|(_, specs)| specs.is_empty()));
    }
}
