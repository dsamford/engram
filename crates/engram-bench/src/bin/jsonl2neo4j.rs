//! `jsonl2neo4j <corpus-dir> <out-dir>` — turn the SNB corpus every other
//! engine loads into `neo4j-admin database import` CSV, so a Neo4j arm can be
//! bulk-loaded in ~an hour instead of ~21 h of Bolt `CREATE`s.
//!
//! # The input is the JSONL corpus, NOT the raw CsvComposite files
//!
//! This is the load-bearing decision in the whole binary, so it comes first.
//!
//! `neo4j-admin database import` cannot read SNB's CsvComposite output: those
//! files are `|`-delimited under SNB headers (`Person.id`, `Comment.id`) with
//! no `:ID`/`:START_ID`/`:END_ID` form. The obvious converter therefore reads
//! `/datasets/raw/social_network-sfN-CsvComposite-…` and writes Neo4j CSV.
//! **That converter would be wrong, and quietly so.**
//!
//! `datagen2jsonl` does not transcribe those CSVs. It splits `place` into
//! City/Country/Continent and `organisation` into University/Company on a
//! `type` column, folds several source files into one edge type (HAS_CREATOR
//! from two files, HAS_TAG from three, IS_LOCATED_IN from four), and — the
//! part that matters — **remaps every sparse Datagen id to a dense id per
//! label family, with Post and Comment sharing ONE dense Message space**. Its
//! output is what engram, PostgreSQL and LadybugDB all load. A second
//! implementation of that remap is exactly where a corpus stops being the same
//! corpus: the counts would still match (they are counts of a *shape*) while
//! the graphs differed, and nothing downstream would say so.
//!
//! So this binary reads `nodes.jsonl` / `rels.jsonl` / `meta.json` — the
//! artifact the other engines load — through `engram_bench::read_jsonl` and
//! `engram_bench::untag_prop`, which is the SAME parse `snbload` performs.
//! The remap is not reimplemented, it is inherited byte for byte. There is no
//! id arithmetic in this file at all: a node's Neo4j `:ID` **is** its corpus
//! id string (`p:412`, `m:9001`), which is also the `gid` property `snbload`
//! writes onto every node of both engines. Two engines therefore hold the same
//! node under the same addressable name, which is the property that makes a
//! cross-engine count comparison mean anything.
//!
//! Keying the import on the corpus id string also removes `snbload`'s one
//! genuinely subtle failure mode: it must decide which LABEL each prefix keys
//! on (`m:` is shared by `:Message:Post` and `:Message:Comment`, so it keys on
//! `Message`; `cont:`/`country:`/`city:` are each dense from 0, so `:Place` is
//! ambiguous and they key on their specific label). Nothing here resolves a
//! (label, id) pair, so nothing here can resolve one wrongly.
//!
//! The guard chain closes: `sf3-corpus-reconcile.sh` asserts the raw CSVs
//! reproduce `meta.json` label-for-label and type-for-type; this binary
//! asserts the Neo4j CSVs reproduce the same `meta.json`. Both ends are pinned
//! to one census that neither of them wrote.
//!
//! # Memory: bitsets, not maps
//!
//! `datagen2jsonl` must hold eight `BTreeMap<i64, i64>` keyed by the sparse
//! Datagen id — ~0.9–1.2 GB at SF10 (`docs/bench/sf10-plan.md` §4.8), against a
//! fetch pod capped at 2 GiB. This binary inherits none of that. It needs only
//! to answer "was this corpus id defined by `nodes.jsonl`?", and corpus ids are
//! dense integers under a prefix, so the answer is **one bit per node**: ~4 MB
//! at SF10, not ~1.2 GB. A corpus id that does not spell an integer falls back
//! to a set of strings, which is empty for both generators. The recommendation
//! to raise `bench-fetch` to 8 Gi is therefore not needed *for this step*; it
//! still applies to `datagen2jsonl` itself, which runs first and does hold the
//! maps.
//!
//! # Relationship properties are NOT emitted by default
//!
//! `snbload`'s relationship pass is `CREATE (a)-[:{t}]->(b)` — it never sends
//! relationship properties, on either engine. Both the Neo4j SF1 arm and every
//! engram arm were loaded that way (`load-sf3.sh` runs `snbload` too). Emitting
//! `creationDate` / `joinDate` / `classYear` / `workFrom` here would give Neo4j
//! a graph the Bolt-loaded engines do not have: a larger store, more page-cache
//! pressure, and a difference that would be read as an engine difference.
//! **CORRECTED 2026-09-14: relationship properties are now emitted by DEFAULT.**
//! The paragraph above describes how this stood when `snbload`'s relationship
//! pass really was `CREATE (a)-[:{t}]->(b)` and nothing else. It is not how it
//! stands now: snbload sends `SET r = ...` whenever the corpus carries edge
//! properties and has NO flag to suppress it, so an import that omits them
//! gives Neo4j a SMALLER GRAPH than the Bolt-loaded engines have — the exact
//! divergence the old default existed to prevent, now produced by keeping it.
//!
//! It is not academic. SNB BI's bi11 filters on `KNOWS.creationDate` (it is
//! the ONLY BI query that reads an edge property), and SNB Interactive's
//! IC1, IC5, IC7, IC11 and IS3 read `studyAt.classYear`, `workAt.workFrom`,
//! `membership.joinDate`, `like.creationDate` and `KNOWS.creationDate`. At
//! SF3 all 565,247 KNOWS edges carry theirs in the engram store. Without
//! these columns those queries cannot be answered on Neo4j at all, and the
//! difference would be read as an engine result.
//!
//! `--no-rel-props` opts OUT for a corpus that wants the old shape; the manifest records
//! which was used.
//!
//! # Failing loudly is the entire point
//!
//! Every check below is a refusal, never a skip. The node pass is a PRE-FLIGHT
//! that writes nothing: it resolves every column's type, checks every id, label
//! and property, and refuses before a byte of CSV exists. Output files are
//! written as `*.csv.part` and renamed only once the whole conversion has
//! reconciled against `meta.json`, so an aborted run leaves no importable CSV
//! set behind. `manifest.json` — which carries the import command and the
//! counts the imported database must answer — is written LAST, so its presence
//! is itself the proof that the conversion reconciled.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use engram_cypher::{Value, json};

/// Dense corpus ids are `0..N` per family. An id past this is not dense and is
/// not worth sizing a bitset for, so it takes the string fallback instead —
/// the same bound `snbload` uses, for the same reason.
const MAX_DENSE_ID: u64 = u32::MAX as u64;

/// Flush the CSV buffer at roughly this size. SF10 output is GB-scale and must
/// never be held in memory whole.
const FLUSH_AT: usize = 1 << 22;

// ── Refusal ─────────────────────────────────────────────────────────────────

/// A refusal. Everything the corpus can trip goes through here, and — for the
/// node pass — before any output file exists.
fn refuse(why: &str) -> ! {
    eprintln!("[jsonl2neo4j] REFUSING: {why}");
    eprintln!(
        "  no manifest.json was written, so no CSV set here is importable; a corpus that \
         cannot be converted exactly is not converted at all"
    );
    std::process::exit(1);
}

// ── CSV ─────────────────────────────────────────────────────────────────────

/// RFC 4180: quote a field iff it contains the delimiter or a quote; escape an
/// inner quote by doubling it. `neo4j-admin` reads exactly this
/// (`--legacy-style-quoting` defaults to false, so `\"` is NOT an escape and
/// doubling is the only one). Line breaks are refused upstream rather than
/// quoted, because `--multiline-fields` defaults to off.
fn csv_field(s: &str, out: &mut String) {
    if s.contains(',') || s.contains('"') {
        out.push('"');
        for c in s.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// A buffered CSV sink that counts the data rows it wrote.
struct Sink {
    /// Where it is writing.
    path: PathBuf,
    /// The file.
    f: std::io::BufWriter<std::fs::File>,
    /// Rows not yet flushed.
    buf: String,
    /// Data rows written (the header is not one).
    rows: u64,
}

impl Sink {
    /// Create the file and write its header row.
    fn create(path: PathBuf, header: &str) -> Self {
        let f = std::fs::File::create(&path)
            .unwrap_or_else(|e| refuse(&format!("create {}: {e}", path.display())));
        let mut s = Sink {
            path,
            f: std::io::BufWriter::with_capacity(1 << 20, f),
            buf: String::with_capacity(FLUSH_AT + 4096),
            rows: 0,
        };
        s.buf.push_str(header);
        s.buf.push('\n');
        s
    }
    /// Append one row; returns the bytes it occupies, newline included.
    fn row(&mut self, build: impl FnOnce(&mut String)) -> usize {
        let start = self.buf.len();
        build(&mut self.buf);
        let n = self.buf.len() - start + 1;
        self.buf.push('\n');
        self.rows += 1;
        if self.buf.len() >= FLUSH_AT {
            self.flush_buf();
        }
        n
    }
    /// Push the buffer to the file.
    fn flush_buf(&mut self) {
        self.f
            .write_all(self.buf.as_bytes())
            .unwrap_or_else(|e| refuse(&format!("write {}: {e}", self.path.display())));
        self.buf.clear();
    }
    /// Flush, fsync, and hand back (path, rows written).
    fn finish(mut self) -> (PathBuf, u64) {
        self.flush_buf();
        self.f
            .flush()
            .unwrap_or_else(|e| refuse(&format!("flush {}: {e}", self.path.display())));
        self.f
            .get_ref()
            .sync_all()
            .unwrap_or_else(|e| refuse(&format!("fsync {}: {e}", self.path.display())));
        (self.path, self.rows)
    }
}

/// Count the data lines of a written CSV by reading it back. The only check
/// that can catch a short write — a full disk, a truncated buffer — which a
/// counter incremented in memory cannot.
fn readback_rows(path: &Path) -> u64 {
    let f = std::fs::File::open(path)
        .unwrap_or_else(|e| refuse(&format!("readback open {}: {e}", path.display())));
    let mut n: u64 = 0;
    for line in std::io::BufReader::with_capacity(1 << 20, f).lines() {
        let line = line.unwrap_or_else(|e| refuse(&format!("readback {}: {e}", path.display())));
        if !line.is_empty() {
            n += 1;
        }
    }
    n.saturating_sub(1) // the header
}

// ── Corpus id presence: one bit per node ────────────────────────────────────

/// A dense id set, as a bitset.
#[derive(Default)]
struct IdSet {
    /// The bits.
    words: Vec<u64>,
    /// How many are set.
    len: u64,
    /// Smallest id seen.
    min: u64,
    /// Largest id seen.
    max: u64,
}

impl IdSet {
    /// Insert; false if it was already present.
    fn insert(&mut self, id: u64) -> bool {
        let w = (id / 64) as usize;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let bit = 1u64 << (id % 64);
        if self.words[w] & bit != 0 {
            return false;
        }
        self.words[w] |= bit;
        if self.len == 0 {
            self.min = id;
            self.max = id;
        } else {
            self.min = self.min.min(id);
            self.max = self.max.max(id);
        }
        self.len += 1;
        true
    }
    /// Whether this id was inserted.
    fn contains(&self, id: u64) -> bool {
        let w = (id / 64) as usize;
        w < self.words.len() && self.words[w] & (1u64 << (id % 64)) != 0
    }
    /// Whether the set is exactly `0..len` — the invariant both corpus
    /// generators guarantee and `stress.rs` derives its key space from.
    /// Reported, not enforced: a gap does not break a string-keyed import, but
    /// it does mean the corpus is not the shape those derivations assume.
    fn dense(&self) -> bool {
        self.len > 0 && self.min == 0 && self.max + 1 == self.len
    }
}

/// `p:412` → `("p", 412)`. A corpus id that does not spell a non-negative
/// dense integer is not structured and takes the string fallback.
fn split_gid(gid: &str) -> Option<(&str, u64)> {
    let (prefix, digits) = gid.rsplit_once(':')?;
    if prefix.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    (n <= MAX_DENSE_ID).then_some((prefix, n))
}

/// Which corpus ids `nodes.jsonl` defined. Structured ids cost one bit each;
/// anything else costs a string, and both generators produce none.
#[derive(Default)]
struct Presence {
    /// Prefix → the dense ids seen under it.
    prefixes: BTreeMap<String, IdSet>,
    /// Corpus ids that do not spell a dense integer.
    other: BTreeSet<String>,
}

impl Presence {
    /// Record a corpus id; false if it was already defined.
    fn insert(&mut self, gid: &str) -> bool {
        match split_gid(gid) {
            Some((p, n)) => self.prefixes.entry(p.to_string()).or_default().insert(n),
            None => self.other.insert(gid.to_string()),
        }
    }
    /// Whether `nodes.jsonl` defined this corpus id.
    fn contains(&self, gid: &str) -> bool {
        match split_gid(gid) {
            Some((p, n)) => self.prefixes.get(p).is_some_and(|s| s.contains(n)),
            None => self.other.contains(gid),
        }
    }
    /// Roughly what the presence structure costs.
    fn bytes(&self) -> usize {
        self.prefixes
            .values()
            .map(|s| s.words.len() * 8)
            .sum::<usize>()
            + self.other.iter().map(|s| s.len() + 32).sum::<usize>()
    }
}

// ── Column typing ───────────────────────────────────────────────────────────

/// A Neo4j import column type. `datagen2jsonl` emits only `long` and `string`;
/// the other two exist so a different corpus is converted rather than refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ColTy {
    /// `:long`
    Long,
    /// `:double`
    Double,
    /// `:boolean`
    Boolean,
    /// `:string`
    Str,
    /// `:date`
    Date,
    /// `:datetime`
    DateTime,
}

/// `days` since the epoch → `yyyy-MM-dd` (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl ColTy {
    /// The header suffix.
    fn decl(self) -> &'static str {
        match self {
            ColTy::Long => "long",
            ColTy::Double => "double",
            ColTy::Boolean => "boolean",
            ColTy::Str => "string",
            ColTy::Date => "date",
            ColTy::DateTime => "datetime",
        }
    }
}

/// The column type a value implies, or `None` for a value that should be
/// written as an EMPTY field. Neo4j sets no property for an empty unquoted
/// field, which is what `snbload` does with a Cypher `null` and what
/// `datagen2jsonl` does by omitting the key — the three agree.
fn col_ty(v: &Value) -> Option<Result<ColTy, String>> {
    match v {
        Value::Null => None,
        Value::Int(_) => Some(Ok(ColTy::Long)),
        Value::Float(f) if f.is_finite() => Some(Ok(ColTy::Double)),
        Value::Float(f) => Some(Err(format!(
            "{f} — a non-finite float has no CSV spelling neo4j-admin will parse back"
        ))),
        Value::Bool(_) => Some(Ok(ColTy::Boolean)),
        Value::Str(_) => Some(Ok(ColTy::Str)),
        // SNB temporals. `neo4j-admin import` reads `:date` and `:datetime`
        // columns as ISO-8601, which is the same instant AND the same type
        // `snbload` now inlines as `date(...)` / `datetime(...)`. Before this,
        // `datagen2jsonl` flattened both to epoch millis and BOTH engines got
        // an integer -- consistent, and consistently unable to answer the SNB
        // BI queries, which compare `creationDate` against a `datetime()`
        // literal. Typing it on one engine only would turn that into a real
        // divergence, so the two move together.
        Value::Date(_) => Some(Ok(ColTy::Date)),
        Value::DateTime { .. } => Some(Ok(ColTy::DateTime)),
        other => Some(Err(format!(
            "{other:?} — a Neo4j import column has ONE type, and this is not a value \
             `snbload` can inline either, so the two engines would diverge here"
        ))),
    }
}

/// Render a value into a CSV field of the declared column type. A value whose
/// type disagrees with the column is unreachable — the pre-flight pass refuses
/// it — but is spelled out rather than silently coerced.
fn render(v: &Value, ty: ColTy, out: &mut String) {
    match (v, ty) {
        (Value::Null, _) => {}
        (Value::Int(n), ColTy::Long) => {
            let _ = write!(out, "{n}");
        }
        (Value::Float(f), ColTy::Double) => {
            let _ = write!(out, "{f}");
        }
        // only in a column `--widen-mixed-numbers` declared `:double`
        (Value::Int(n), ColTy::Double) => {
            let _ = write!(out, "{n}");
        }
        (Value::Bool(b), ColTy::Boolean) => out.push_str(if *b { "true" } else { "false" }),
        (Value::Str(s), ColTy::Str) => csv_field(s, out),
        (Value::Date(days), ColTy::Date) => {
            let (y, m, d) = civil_from_days(*days);
            let _ = write!(out, "{y:04}-{m:02}-{d:02}");
        }
        (
            Value::DateTime {
                epoch_seconds,
                nanos,
                ..
            },
            ColTy::DateTime,
        ) => {
            // `epoch_seconds` is already UTC (`untag_temporal` subtracts the
            // offset), so this renders with a `Z` rather than re-applying one.
            let (days, rem) = (
                epoch_seconds.div_euclid(86_400),
                epoch_seconds.rem_euclid(86_400),
            );
            let (y, mo, d) = civil_from_days(days);
            let (h, mi, sec) = (rem / 3600, rem / 60 % 60, rem % 60);
            let milli = nanos / 1_000_000;
            let _ = write!(
                out,
                "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}.{milli:03}Z"
            );
        }
        (other, ty) => refuse(&format!(
            "value {other:?} in a ':{}' column — the pre-flight pass and the write pass \
             disagree, which is a bug in this binary, not in the corpus",
            ty.decl()
        )),
    }
}

// ── Groups ──────────────────────────────────────────────────────────────────

/// One node output file: every node carrying exactly this label set.
struct NodeGroup {
    /// The corpus label list, in corpus order (`["Message", "Post"]`).
    labels: Vec<String>,
    /// The key `meta.json`'s node census uses: the LAST (most specific) label,
    /// which is what `datagen2jsonl` counts under.
    census_key: String,
    /// Property key → column type, resolved over every row in the pre-flight.
    cols: BTreeMap<String, ColTy>,
    /// Rows the pre-flight pass counted.
    rows_in: u64,
    /// Non-empty property cells, excluding `gid` and `:LABEL`.
    cells: u64,
}

/// One relationship output file.
#[derive(Default)]
struct RelGroup {
    /// Property key → column type (empty unless `--rel-props`).
    cols: BTreeMap<String, ColTy>,
    /// Rows read for this type.
    rows_in: u64,
    /// Non-empty property cells.
    cells: u64,
}

/// A property key must be a bare identifier: it becomes a Neo4j property name,
/// and `snbload` refuses anything else rather than escape it into something
/// that parses differently on the two engines.
fn bare_identifier(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A string that cannot survive the default CSV reader. `--multiline-fields`
/// defaults to off, so a value containing a line break would end the record
/// early and shift every following column — silently, because the column COUNT
/// of the truncated row can still be legal.
fn has_line_break(s: &str) -> bool {
    s.contains('\n') || s.contains('\r')
}

/// Pull `l` as a label list, refusing anything that is not a list of bare
/// identifier strings.
fn labels_of(m: &BTreeMap<String, Value>, gid: &str) -> Vec<String> {
    let items = engram_bench::get_list(m, "l");
    if items.is_empty() {
        refuse(&format!(
            "node {gid:?} carries no label — Neo4j would import it label-less and every \
             :Label pattern would miss it"
        ));
    }
    items
        .iter()
        .map(|l| match l {
            Value::Str(s) if bare_identifier(s) => s.clone(),
            other => refuse(&format!(
                "node {gid:?} has a label {other:?} that is not a bare identifier"
            )),
        })
        .collect()
}

/// Untag a record's `p` map into real values, the way `snbload` and
/// `load_export` both do.
fn props_of(m: &BTreeMap<String, Value>, unloadable: &mut usize) -> BTreeMap<String, Value> {
    match m.get("p") {
        Some(Value::Map(p)) => p
            .iter()
            .map(|(k, x)| (k.clone(), engram_bench::untag_prop(x, unloadable)))
            .collect(),
        Some(Value::Null) | None => BTreeMap::new(),
        Some(other) => refuse(&format!("a record's 'p' is {other:?}, not a map")),
    }
}

/// Merge one row's properties into a group's column set, refusing a type
/// conflict. A column that is `long` for some rows and `string` for others has
/// no single Neo4j declaration; `datagen2jsonl` produces one exactly when its
/// `coerced_to_str` counter is non-zero, and that counter is in `meta.json`.
/// `--widen-mixed-numbers`: a column holding both `:long` and `:double`
/// values is declared `:double` instead of refused. FinBench's amounts are
/// doubles by the specification, and `finbench2jsonl` keeps an integral one as
/// an integer, so `:repay.amount` arrives as both. Off by default: for any
/// other corpus a mixed column is the fault it always was. Every integer a
/// widened column holds must be exact as a double (|x| <= 2^53), and every
/// widened column is named in the log and in `manifest.json`.
#[derive(Default)]
struct Widening {
    enabled: bool,
    /// `scope.column` -> the largest |integer| seen in it.
    max_int: BTreeMap<String, u64>,
    /// `scope.column`s declared `:double` because they also held integers.
    widened: BTreeSet<String>,
}

fn merge_cols(
    cols: &mut BTreeMap<String, ColTy>,
    cells: &mut u64,
    props: &BTreeMap<String, Value>,
    what: &str,
    scope: &str,
    widening: &mut Widening,
) {
    for (k, v) in props {
        if k == "gid" {
            refuse(&format!(
                "{what} carries a 'gid' property; 'gid' is the corpus id `snbload` writes and \
                 is this converter's ':ID' column, so a second one would collide"
            ));
        }
        if !bare_identifier(k) {
            refuse(&format!(
                "{what}: property key {k:?} is not a bare identifier"
            ));
        }
        if let Value::Str(s) = v
            && has_line_break(s)
        {
            refuse(&format!(
                "{what}: property {k} contains a line break; `neo4j-admin` reads \
                 --multiline-fields=false by default, so this row would be split and every \
                 later column shifted without an error"
            ));
        }
        let Some(ty) = col_ty(v) else { continue };
        let ty = match ty {
            Ok(t) => t,
            Err(e) => refuse(&format!("{what}: property {k} = {e}")),
        };
        *cells += 1;
        if let (true, Value::Int(i)) = (widening.enabled, v) {
            let m = widening.max_int.entry(format!("{scope}.{k}")).or_default();
            *m = (*m).max(i.unsigned_abs());
        }
        match cols.get(k) {
            None => {
                cols.insert(k.clone(), ty);
            }
            Some(&prev) if prev == ty => {}
            Some(&prev)
                if widening.enabled
                    && matches!(
                        (prev, ty),
                        (ColTy::Long, ColTy::Double) | (ColTy::Double, ColTy::Long)
                    ) =>
            {
                cols.insert(k.clone(), ColTy::Double);
                widening.widened.insert(format!("{scope}.{k}"));
            }
            Some(&prev) => refuse(&format!(
                "{what}: property {k} is ':{}' here but ':{}' on an earlier row — a Neo4j \
                 import column has ONE type. Check meta.json's `coerced_to_str`: a non-zero \
                 count means `datagen2jsonl` kept some typed values as strings, and the \
                 corpus, not this converter, is what needs fixing",
                ty.decl(),
                prev.decl()
            )),
        }
    }
}

// ── meta.json ───────────────────────────────────────────────────────────────

/// The census `datagen2jsonl` wrote: per most-specific label, per rel type.
/// `finbench2jsonl` writes its nodes as a TOTAL (`"nodes": 5580000`) and its
/// relationships per type under `rel_type_counts`; that census is read too, and
/// the nodes then reconcile against the total rather than label by label.
struct Census {
    /// Label → node count (empty for a total-only census).
    nodes: BTreeMap<String, u64>,
    /// The node total, when the census gives nodes only as a total.
    node_total: Option<u64>,
    /// Type → relationship count.
    rels: BTreeMap<String, u64>,
    /// Typed values `datagen2jsonl` had to keep as strings.
    coerced_to_str: u64,
}

/// Read `meta.json`, refusing anything that is not the census this converter
/// reconciles against.
fn read_census(path: &Path) -> Census {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| refuse(&format!("read {}: {e}", path.display())));
    let Ok(Value::Map(m)) = json::from_json(&raw) else {
        refuse(&format!("{}: not a JSON object", path.display()))
    };
    let section = |k: &str| -> BTreeMap<String, u64> {
        match m.get(k) {
            Some(Value::Map(s)) => s
                .iter()
                .map(|(name, v)| match v {
                    Value::Int(n) if *n >= 0 => (name.clone(), *n as u64),
                    other => refuse(&format!(
                        "{}: {k}.{name} is {other:?}, not a non-negative integer",
                        path.display()
                    )),
                })
                .collect(),
            _ => refuse(&format!(
                "{}: no '{k}' census. This converter reconciles its output against the census \
                 `datagen2jsonl` wrote; without it there is nothing to prove the output \
                 against, and an unproven conversion is the failure this binary exists to \
                 prevent",
                path.display()
            )),
        }
    };
    // finbench2jsonl's shape: a node total, and the relationships per type.
    if let (Some(Value::Int(total)), Some(Value::Map(_))) = (m.get("nodes"), m.get("rel_type_counts")) {
        if *total < 0 {
            refuse(&format!("{}: nodes is {total}, not a count", path.display()));
        }
        return Census {
            nodes: BTreeMap::new(),
            node_total: Some(*total as u64),
            rels: section("rel_type_counts"),
            coerced_to_str: 0,
        };
    }
    Census {
        nodes: section("nodes"),
        node_total: None,
        rels: section("rels"),
        coerced_to_str: match m.get("coerced_to_str") {
            Some(Value::Int(n)) if *n >= 0 => *n as u64,
            _ => 0,
        },
    }
}

// ── Reconciliation ──────────────────────────────────────────────────────────

/// One reconciled output file, as it appears in the manifest.
struct Line {
    /// File name inside the output directory.
    file: String,
    /// What it holds (`:Message:Post`, `:KNOWS`).
    what: String,
    /// The `meta.json` census key this file must equal.
    census_key: String,
    /// What the census says, if it says anything.
    expected: Option<u64>,
    /// Records routed to this file.
    rows_in: u64,
    /// Data rows the sink wrote.
    rows_out: u64,
    /// Data rows counted by re-reading the file.
    readback: Option<u64>,
    /// The header, column by column.
    columns: Vec<String>,
}

/// Compare the three counts (and the census) for one file, appending any
/// mismatch to `bad`.
fn reconcile(l: &Line, bad: &mut Vec<String>) {
    if l.rows_in != l.rows_out {
        bad.push(format!(
            "{}: {} row(s) read but {} written",
            l.file, l.rows_in, l.rows_out
        ));
    }
    if let Some(rb) = l.readback
        && rb != l.rows_out
    {
        bad.push(format!(
            "{}: {} row(s) written but {} read back from disk — a SHORT WRITE",
            l.file, l.rows_out, rb
        ));
    }
    match l.expected {
        None => bad.push(format!(
            "{}: '{}' is not a key in meta.json's census — the corpus and this converter \
             disagree about what exists",
            l.file, l.census_key
        )),
        Some(e) if e != l.rows_out => bad.push(format!(
            "{}: {} row(s) written but meta.json says '{}' has {}",
            l.file, l.rows_out, l.census_key, e
        )),
        Some(_) => {}
    }
}

// ── The manifest ────────────────────────────────────────────────────────────

/// Everything the run decided, printed and written to `manifest.json`.
struct Manifest {
    /// The corpus directory read.
    corpus: PathBuf,
    /// The output directory written.
    out: PathBuf,
    /// The database name the generated import command targets.
    database: String,
    /// Whether relationship properties were emitted.
    rel_props: bool,
    /// Whether every file was re-read to count its rows.
    readback: bool,
    /// Per node file.
    nodes: Vec<Line>,
    /// Per relationship file.
    rels: Vec<Line>,
    /// Total node rows.
    node_total: u64,
    /// Total relationship rows.
    rel_total: u64,
    /// Non-empty node property cells.
    node_cells: u64,
    /// Non-empty relationship property cells.
    rel_cells: u64,
    /// What the id presence structure cost.
    presence_bytes: usize,
    /// Per-prefix id-space description.
    dense: Vec<String>,
    /// Corpus ids that did not spell a dense integer.
    unstructured: usize,
    /// Property values the corpus codec could not untag.
    unloadable: usize,
    /// Columns `--widen-mixed-numbers` declared `:double` (`scope.column`).
    widened: Vec<String>,
    /// The longest CSV row written.
    max_row_bytes: usize,
    /// Wall time.
    seconds: f64,
}

/// The `neo4j-admin` invocation this CSV set is meant to be imported with.
/// Generated rather than documented: every flag here is one whose DEFAULT
/// would let a fault through quietly, or one whose default a future release
/// could change under us.
fn import_command(m: &Manifest) -> String {
    let mut s = String::new();
    s.push_str("neo4j-admin database import full \\\n");
    // A partial import that answers short is the failure this whole binary
    // exists to prevent, so every tolerance is pinned to zero.
    s.push_str("  --id-type=string \\\n");
    s.push_str("  --bad-tolerance=0 \\\n");
    s.push_str("  --skip-bad-relationships=false \\\n");
    s.push_str("  --skip-duplicate-nodes=false \\\n");
    s.push_str("  --strict=true \\\n");
    // Defaults today, pinned so that a release changing one breaks the command
    // rather than the corpus.
    s.push_str("  --normalize-types=true \\\n");
    s.push_str("  --trim-strings=false \\\n");
    s.push_str("  --ignore-empty-strings=false \\\n");
    s.push_str("  --ignore-extra-columns=false \\\n");
    s.push_str("  --legacy-style-quoting=false \\\n");
    s.push_str("  --array-delimiter=';' \\\n");
    s.push_str("  --delimiter=',' \\\n");
    s.push_str("  --quote='\"' \\\n");
    let _ = writeln!(
        s,
        "  --read-buffer-size={} \\",
        (m.max_row_bytes.next_power_of_two().saturating_mul(2)).max(4 * 1024 * 1024)
    );
    let _ = writeln!(
        s,
        "  --report-file={} \\",
        m.out.join("import.report").display()
    );
    s.push_str("  --overwrite-destination=true \\\n");
    for l in &m.nodes {
        let _ = writeln!(s, "  --nodes={} \\", m.out.join(&l.file).display());
    }
    for l in &m.rels {
        let _ = writeln!(
            s,
            "  --relationships={}={} \\",
            l.census_key,
            m.out.join(&l.file).display()
        );
    }
    // `--` is NOT decoration. `--relationships` has variable arity, so a bare
    // database name after the last one is swallowed as another CSV path and the
    // import dies with `File 'neo4j' doesn't exist` — which reads as a missing
    // file, not as an argument-parsing problem. Verified against neo4j-admin
    // 5.26.30: without the marker the import refuses, with it it runs. The
    // upstream docs' example omits it.
    let _ = write!(s, "  -- {}", m.database);
    s
}

/// `manifest.json`: per file rows in / rows out / rows read back / census, the
/// import command, and the counts the imported database must answer.
fn manifest_json(m: &Manifest) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    fn lines(v: &[Line]) -> String {
        let mut s = String::from("[");
        for (i, l) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let cols = l
                .columns
                .iter()
                .map(|c| format!("\"{}\"", esc(c)))
                .collect::<Vec<_>>()
                .join(",");
            let _ = write!(
                s,
                "{{\"file\":\"{}\",\"what\":\"{}\",\"census_key\":\"{}\",\
                 \"census_expected\":{},\"rows_in\":{},\"rows_out\":{},\"rows_readback\":{},\
                 \"columns\":[{cols}]}}",
                esc(&l.file),
                esc(&l.what),
                esc(&l.census_key),
                l.expected.map_or("null".into(), |e| e.to_string()),
                l.rows_in,
                l.rows_out,
                l.readback.map_or("null".into(), |e| e.to_string()),
            );
        }
        s.push(']');
        s
    }
    let mut s = String::from("{\"tool\":\"jsonl2neo4j\"");
    let _ = write!(
        s,
        ",\"corpus\":\"{}\"",
        esc(&m.corpus.display().to_string())
    );
    let _ = write!(s, ",\"out\":\"{}\"", esc(&m.out.display().to_string()));
    let _ = write!(s, ",\"database\":\"{}\"", esc(&m.database));
    let _ = write!(s, ",\"rel_properties_emitted\":{}", m.rel_props);
    let _ = write!(s, ",\"readback_verified\":{}", m.readback);
    let _ = write!(s, ",\"nodes\":{}", lines(&m.nodes));
    let _ = write!(s, ",\"relationships\":{}", lines(&m.rels));
    let _ = write!(
        s,
        ",\"totals\":{{\"nodes\":{},\"relationships\":{},\"node_property_cells\":{},\
         \"relationship_property_cells\":{}}}",
        m.node_total, m.rel_total, m.node_cells, m.rel_cells
    );
    // What the imported database must answer. Stated here so the check does
    // not have to be re-derived by whoever runs the import — in particular the
    // property total, which is the only cheap check that an empty CSV field
    // became an ABSENT property rather than an empty string.
    let _ = write!(
        s,
        ",\"expect_in_neo4j\":{{\"MATCH (n) RETURN count(n)\":{},\
         \"MATCH ()-[r]->() RETURN count(r)\":{},\
         \"MATCH (n) RETURN sum(size(keys(n)))\":{},\
         \"MATCH ()-[r]->() RETURN sum(size(keys(r)))\":{}}}",
        m.node_total,
        m.rel_total,
        // every node also carries `gid`, the :ID column stored as a property
        m.node_cells + m.node_total,
        m.rel_cells
    );
    let dense = m
        .dense
        .iter()
        .map(|d| format!("\"{}\"", esc(d)))
        .collect::<Vec<_>>()
        .join(",");
    let _ = write!(
        s,
        ",\"id_space\":{{\"prefixes\":[{dense}],\"unstructured_ids\":{},\"presence_bytes\":{}}}",
        m.unstructured, m.presence_bytes
    );
    let _ = write!(
        s,
        ",\"unloadable_values\":{},\"max_row_bytes\":{},\"seconds\":{:.1}",
        m.unloadable, m.max_row_bytes, m.seconds
    );
    let widened = m
        .widened
        .iter()
        .map(|w| format!("\"{}\"", esc(w)))
        .collect::<Vec<_>>()
        .join(",");
    let _ = write!(s, ",\"widened_to_double\":[{widened}]");
    let _ = write!(
        s,
        ",\"import_command\":\"{}\"",
        esc(&import_command(m).replace(" \\\n", " "))
    );
    s.push_str(",\"ok\":true}\n");
    s
}

/// Refuse if the output directory already holds a conversion. Two runs
/// interleaved in one directory produce a CSV set that reconciles against
/// nothing.
fn prepare_out(out: &Path, force: bool) {
    std::fs::create_dir_all(out)
        .unwrap_or_else(|e| refuse(&format!("create {}: {e}", out.display())));
    let mut stale: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(out) {
        for e in rd.flatten() {
            let Some(n) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if n.ends_with(".csv") || n.ends_with(".csv.part") || n == "manifest.json" {
                stale.push(n);
            }
        }
    }
    stale.sort();
    if stale.is_empty() {
        return;
    }
    if !force {
        refuse(&format!(
            "{} already holds a conversion ({} file(s), e.g. {}) — pass --force to replace it. \
             Mixing two runs in one directory produces a CSV set that reconciles against nothing",
            out.display(),
            stale.len(),
            stale[0]
        ));
    }
    for n in &stale {
        let p = out.join(n);
        std::fs::remove_file(&p)
            .unwrap_or_else(|e| refuse(&format!("remove {}: {e}", p.display())));
    }
    eprintln!(
        "[jsonl2neo4j] --force: removed {} stale file(s)",
        stale.len()
    );
}

/// Print usage and exit.
fn usage() -> ! {
    eprintln!("usage: jsonl2neo4j <corpus-dir> <out-dir> [--no-rel-props] [--skip-readback]");
    eprintln!("                   [--force] [--database <name>]");
    eprintln!();
    eprintln!("  Converts the SNB JSONL corpus (nodes.jsonl + rels.jsonl + meta.json, as");
    eprintln!("  written by datagen2jsonl and loaded by snbload) into CSV for");
    eprintln!("  `neo4j-admin database import`. It reads the JSONL, NOT the raw CsvComposite");
    eprintln!("  files: the dense-id remap is INHERITED rather than reimplemented, which is");
    eprintln!("  the whole point. See the file header.");
    eprintln!();
    eprintln!("  --no-rel-props   OMIT relationship properties. They are emitted by");
    eprintln!("                   DEFAULT, because snbload sends them whenever the corpus");
    eprintln!("                   carries them -- it has no flag not to -- so emitting");
    eprintln!("                   them is what parity with the Bolt-loaded engines means.");
    eprintln!("                   Use this only to reproduce a pre-2026-09 corpus.");
    eprintln!("  --skip-readback  do not re-read each written CSV to count its rows.");
    eprintln!("  --force          replace an existing conversion in <out-dir>.");
    eprintln!("  --database NAME  the database the generated import command targets.");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut positional: Vec<String> = Vec::new();
    // ON by default — see the header. `snbload` sends relationship properties
    // whenever the corpus carries them (it has no flag to not), so emitting
    // them here is what PARITY means; the old default was written when snbload
    // genuinely sent none.
    let mut rel_props = true;
    let mut readback = true;
    let mut widen = false;
    let mut force = false;
    let mut database = "neo4j".to_string();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rel-props" => rel_props = true, // kept: explicit, and the old spelling
            "--no-rel-props" => rel_props = false,
            "--skip-readback" => readback = false,
            "--widen-mixed-numbers" => widen = true,
            "--force" => force = true,
            "--database" => {
                database = it
                    .next()
                    .cloned()
                    .unwrap_or_else(|| refuse("--database needs a value"));
            }
            "-h" | "--help" => usage(),
            s if s.starts_with("--") => refuse(&format!("unknown flag {s:?}")),
            s => positional.push(s.to_string()),
        }
    }
    if positional.len() != 2 {
        usage();
    }
    let corpus = PathBuf::from(&positional[0]);
    let out = PathBuf::from(&positional[1]);
    let t0 = Instant::now();

    let nodes_path = corpus.join("nodes.jsonl");
    let rels_path = corpus.join("rels.jsonl");
    for p in [&nodes_path, &rels_path] {
        if !p.is_file() {
            refuse(&format!("{} does not exist", p.display()));
        }
    }
    let census = read_census(&corpus.join("meta.json"));
    if census.coerced_to_str > 0 {
        eprintln!(
            "[jsonl2neo4j] NOTE: meta.json reports coerced_to_str={} — datagen2jsonl kept some \
             typed values as strings. If any of them share a property key with a typed value, \
             the pre-flight below refuses the mixed column.",
            census.coerced_to_str
        );
    }

    // ── Pre-flight: resolve every column, check every id, write nothing ────
    let mut widening = Widening {
        enabled: widen,
        ..Widening::default()
    };
    let mut groups: BTreeMap<String, NodeGroup> = BTreeMap::new();
    let mut presence = Presence::default();
    let mut unloadable = 0usize;
    let mut read = 0u64;
    engram_bench::read_jsonl(&nodes_path, |v| {
        let Value::Map(m) = v else {
            refuse(&format!(
                "{}: a record is not an object",
                nodes_path.display()
            ))
        };
        read += 1;
        let gid = engram_bench::get_str(&m, "i");
        if gid.is_empty() {
            refuse(&format!(
                "{}: record {read} has no corpus id 'i' — it is this converter's :ID and the \
                 name every relationship refers to",
                nodes_path.display()
            ));
        }
        if has_line_break(&gid) {
            refuse(&format!(
                "{}: record {read}: corpus id {gid:?} contains a line break",
                nodes_path.display()
            ));
        }
        if !presence.insert(&gid) {
            refuse(&format!(
                "{}: corpus id {gid:?} names TWO nodes; neo4j-admin would import one and bind \
                 every relationship naming it to whichever it kept",
                nodes_path.display()
            ));
        }
        let labels = labels_of(&m, &gid);
        let props = props_of(&m, &mut unloadable);
        let key = labels.join(":");
        let scope = format!("node :{key}");
        let g = groups.entry(key).or_insert_with(|| NodeGroup {
            census_key: labels[labels.len() - 1].clone(),
            labels: labels.clone(),
            cols: BTreeMap::new(),
            rows_in: 0,
            cells: 0,
        });
        merge_cols(
            &mut g.cols,
            &mut g.cells,
            &props,
            &format!("node {gid:?}"),
            &scope,
            &mut widening,
        );
        g.rows_in += 1;
    });
    if unloadable > 0 {
        refuse(&format!(
            "{}: {unloadable} property value(s) carry a tag the corpus codec cannot untag \
             (they read back as \"<unloadable …>\"); snbload could not send them either, so \
             converting them would put a different value in Neo4j",
            nodes_path.display()
        ));
    }
    if read == 0 {
        refuse(&format!("{} is empty", nodes_path.display()));
    }
    // The census is the contract. Check it BEFORE writing anything.
    if let Some(total) = census.node_total {
        if read != total {
            refuse(&format!(
                "the corpus does not reconcile against its own census: {read} node(s) in \
                 nodes.jsonl but meta.json's total is {total}"
            ));
        }
    } else {
        let mut bad: Vec<String> = Vec::new();
        for (key, g) in &groups {
            match census.nodes.get(&g.census_key) {
                Some(&e) if e == g.rows_in => {}
                Some(&e) => bad.push(format!(
                    "label set :{key} — {} row(s) in nodes.jsonl but meta.json says '{}' has {e}",
                    g.rows_in, g.census_key
                )),
                None => bad.push(format!(
                    "label set :{key} — '{}' is not a key in meta.json's node census",
                    g.census_key
                )),
            }
        }
        let produced: BTreeSet<&str> = groups.values().map(|g| g.census_key.as_str()).collect();
        for k in census.nodes.keys() {
            if !produced.contains(k.as_str()) {
                bad.push(format!(
                    "meta.json declares node label '{k}' that nodes.jsonl never produced"
                ));
            }
        }
        if !bad.is_empty() {
            refuse(&format!(
                "the corpus does not reconcile against its own census:\n  {}",
                bad.join("\n  ")
            ));
        }
    }
    eprintln!(
        "[jsonl2neo4j] pre-flight: {read} node(s) in {} label set(s), {} id prefix(es), \
         {:.1} MB of id bitsets, nothing refused",
        groups.len(),
        presence.prefixes.len(),
        presence.bytes() as f64 / 1e6
    );
    for (p, s) in &presence.prefixes {
        eprintln!(
            "[jsonl2neo4j]   {p}: {} id(s), {}..={}{}",
            s.len,
            s.min,
            s.max,
            if s.dense() {
                " (dense 0..N)"
            } else {
                " — NOT DENSE"
            }
        );
    }

    prepare_out(&out, force);

    // ── Nodes ──────────────────────────────────────────────────────────────
    let part = |name: &str| out.join(format!("{name}.part"));
    let mut sinks: BTreeMap<String, Sink> = BTreeMap::new();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, g) in &groups {
        let name = format!("nodes_{}.csv", g.labels.join("_"));
        let mut cols: Vec<String> = vec!["gid:ID".into(), ":LABEL".into()];
        cols.extend(g.cols.iter().map(|(k, t)| format!("{k}:{}", t.decl())));
        sinks.insert(key.clone(), Sink::create(part(&name), &cols.join(",")));
        files.insert(key.clone(), name);
        headers.insert(key.clone(), cols);
    }
    let mut max_row_bytes = 0usize;
    let mut written = 0u64;
    let mut ignored = 0usize;
    engram_bench::read_jsonl(&nodes_path, |v| {
        let Value::Map(m) = v else { return };
        let gid = engram_bench::get_str(&m, "i");
        let labels = labels_of(&m, &gid);
        let props = props_of(&m, &mut ignored);
        let key = labels.join(":");
        let g = &groups[&key];
        let label_field = g.labels.join(";");
        let sink = sinks.get_mut(&key).expect("every group has a sink");
        let n = sink.row(|o| {
            csv_field(&gid, o);
            o.push(',');
            o.push_str(&label_field);
            for (k, ty) in &g.cols {
                o.push(',');
                if let Some(v) = props.get(k) {
                    render(v, *ty, o);
                }
            }
        });
        max_row_bytes = max_row_bytes.max(n);
        written += 1;
    });
    if written != read {
        refuse(&format!(
            "{} changed under this converter: {read} node(s) in the pre-flight, {written} in \
             the write pass",
            nodes_path.display()
        ));
    }
    let mut node_lines: Vec<Line> = Vec::new();
    let mut node_total = 0u64;
    let mut node_cells = 0u64;
    for (key, g) in &groups {
        let (path, rows) = sinks.remove(key).expect("sink").finish();
        node_total += rows;
        node_cells += g.cells;
        node_lines.push(Line {
            file: files[key].clone(),
            what: format!(":{key}"),
            census_key: g.census_key.clone(),
            // a total-only census was reconciled whole at the pre-flight, so
            // each label file answers to the rows the pre-flight counted
            expected: if census.node_total.is_some() {
                Some(g.rows_in)
            } else {
                census.nodes.get(&g.census_key).copied()
            },
            rows_in: g.rows_in,
            rows_out: rows,
            readback: readback.then(|| readback_rows(&path)),
            columns: headers[key].clone(),
        });
    }

    // ── Relationships ──────────────────────────────────────────────────────
    //
    // A refusal here happens with the node CSVs already on disk. They are
    // `.part` files and no manifest exists, so nothing is importable.
    let mut rgroups: BTreeMap<String, RelGroup> = BTreeMap::new();
    let mut rsinks: BTreeMap<String, Sink> = BTreeMap::new();
    let mut rfiles: BTreeMap<String, String> = BTreeMap::new();
    let mut rheaders: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rel_read = 0u64;
    let mut rel_cells = 0u64;

    // With --rel-props the column set must be known before the header is
    // written, so the relationship file gets its own pre-flight pass too.
    if rel_props {
        engram_bench::read_jsonl(&rels_path, |v| {
            let Value::Map(m) = v else {
                refuse(&format!(
                    "{}: a record is not an object",
                    rels_path.display()
                ))
            };
            let t = engram_bench::get_str(&m, "t");
            if !bare_identifier(&t) {
                refuse(&format!(
                    "{}: relationship type {t:?} is not a bare identifier",
                    rels_path.display()
                ));
            }
            let props = props_of(&m, &mut unloadable);
            let g = rgroups.entry(t.clone()).or_default();
            merge_cols(
                &mut g.cols,
                &mut g.cells,
                &props,
                &format!("relationship :{t}"),
                &format!("relationship :{t}"),
                &mut widening,
            );
        });
        if unloadable > 0 {
            refuse(&format!(
                "{}: {unloadable} relationship property value(s) cannot be untagged",
                rels_path.display()
            ));
        }
    }
    for c in &widening.widened {
        let m = widening.max_int.get(c).copied().unwrap_or(0);
        if m > (1u64 << 53) {
            refuse(&format!(
                "{c} would be widened to :double but holds an integer of magnitude {m}, \
                 beyond 2^53: a double cannot hold it exactly"
            ));
        }
    }
    if !widening.widened.is_empty() {
        eprintln!(
            "[jsonl2neo4j] widened to :double (--widen-mixed-numbers; every integer <= 2^53): {}",
            widening.widened.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    engram_bench::read_jsonl(&rels_path, |v| {
        let Value::Map(m) = v else {
            refuse(&format!(
                "{}: a record is not an object",
                rels_path.display()
            ))
        };
        rel_read += 1;
        let s = engram_bench::get_str(&m, "s");
        let d = engram_bench::get_str(&m, "d");
        let t = engram_bench::get_str(&m, "t");
        if s.is_empty() || d.is_empty() || t.is_empty() {
            refuse(&format!(
                "{}: record {rel_read} is missing 's', 'd' or 't' ({s:?} -[{t:?}]-> {d:?})",
                rels_path.display()
            ));
        }
        if !bare_identifier(&t) {
            refuse(&format!(
                "{}: record {rel_read}: relationship type {t:?} is not a bare identifier",
                rels_path.display()
            ));
        }
        // An endpoint nodes.jsonl never defined is a HARD refusal. `snbload`
        // counts and drops it; `neo4j-admin` would too, under its default
        // --bad-tolerance=1000. A corpus that quietly loses edges imports fine
        // and answers every traversal short.
        for (end, gid) in [("start", &s), ("end", &d)] {
            if !presence.contains(gid) {
                refuse(&format!(
                    "{}: record {rel_read}: :{t} {end} node {gid:?} is not in nodes.jsonl",
                    rels_path.display()
                ));
            }
        }
        let g = rgroups.entry(t.clone()).or_default();
        g.rows_in += 1;
        let cols = g.cols.clone();
        let sink = rsinks.entry(t.clone()).or_insert_with(|| {
            let name = format!("rels_{t}.csv");
            let mut hdr: Vec<String> = vec![":START_ID".into(), ":END_ID".into()];
            hdr.extend(cols.iter().map(|(k, ty)| format!("{k}:{}", ty.decl())));
            let sink = Sink::create(part(&name), &hdr.join(","));
            rfiles.insert(t.clone(), name);
            rheaders.insert(t.clone(), hdr);
            sink
        });
        let props = if rel_props {
            props_of(&m, &mut ignored)
        } else {
            BTreeMap::new()
        };
        let n = sink.row(|o| {
            csv_field(&s, o);
            o.push(',');
            csv_field(&d, o);
            for (k, ty) in &cols {
                o.push(',');
                if let Some(v) = props.get(k) {
                    render(v, *ty, o);
                    rel_cells += 1;
                }
            }
        });
        max_row_bytes = max_row_bytes.max(n);
    });

    let mut rel_lines: Vec<Line> = Vec::new();
    let mut rel_total = 0u64;
    for (t, g) in &rgroups {
        let Some(sink) = rsinks.remove(t) else {
            refuse(&format!(
                "relationship type :{t} was declared by the pre-flight pass but no row was \
                 written for it"
            ))
        };
        let (path, rows) = sink.finish();
        rel_total += rows;
        rel_lines.push(Line {
            file: rfiles[t].clone(),
            what: format!(":{t}"),
            census_key: t.clone(),
            expected: census.rels.get(t).copied(),
            rows_in: g.rows_in,
            rows_out: rows,
            readback: readback.then(|| readback_rows(&path)),
            columns: rheaders[t].clone(),
        });
    }

    // ── Reconcile, and only then publish ───────────────────────────────────
    let mut bad: Vec<String> = Vec::new();
    for l in node_lines.iter().chain(rel_lines.iter()) {
        reconcile(l, &mut bad);
    }
    let produced: BTreeSet<&str> = rel_lines.iter().map(|l| l.census_key.as_str()).collect();
    for k in census.rels.keys() {
        if !produced.contains(k.as_str()) {
            bad.push(format!(
                "meta.json declares relationship type '{k}' that rels.jsonl never produced"
            ));
        }
    }
    if rel_read != rel_total {
        bad.push(format!(
            "rels.jsonl: {rel_read} record(s) read, {rel_total} row(s) written"
        ));
    }
    if !bad.is_empty() {
        refuse(&format!(
            "the conversion does not reconcile:\n  {}\nThe .part files are left in place for \
             inspection; none of them is importable and no manifest was written.",
            bad.join("\n  ")
        ));
    }

    // Rename every .part to its final name. Only now is the set importable.
    for l in node_lines.iter().chain(rel_lines.iter()) {
        let from = part(&l.file);
        let to = out.join(&l.file);
        std::fs::rename(&from, &to).unwrap_or_else(|e| {
            refuse(&format!(
                "rename {} -> {}: {e}",
                from.display(),
                to.display()
            ))
        });
    }

    let m = Manifest {
        corpus,
        out: out.clone(),
        database,
        rel_props,
        readback,
        node_total,
        rel_total,
        node_cells,
        rel_cells,
        presence_bytes: presence.bytes(),
        dense: presence
            .prefixes
            .iter()
            .map(|(p, s)| {
                format!(
                    "{p}: {} id(s) {}..={}{}",
                    s.len,
                    s.min,
                    s.max,
                    if s.dense() { " dense" } else { " NOT DENSE" }
                )
            })
            .collect(),
        unstructured: presence.other.len(),
        unloadable,
        widened: widening.widened.iter().cloned().collect(),
        max_row_bytes,
        seconds: t0.elapsed().as_secs_f64(),
        nodes: node_lines,
        rels: rel_lines,
    };

    let cmd = import_command(&m);
    let script = out.join("import.sh");
    // Built line by line rather than as one continued literal: the header is
    // operational instruction, and a `\`-continued Rust string is exactly the
    // shape a formatter quietly rewrites into stray blank lines.
    let mut sh = String::new();
    for line in [
        "#!/bin/sh",
        "# Generated by jsonl2neo4j. Run INSIDE a Neo4j 5.26 container with the server",
        "# STOPPED and the target database empty or absent.",
        "#",
        "# Community Edition has exactly ONE user database, so <database> can only be",
        "# the default one: running this REPLACES whatever that server holds. Never",
        "# point it at a Neo4j that is serving anything else.",
        "#",
        "# --max-off-heap-memory is deliberately NOT set here, and you probably must",
        "# set it. neo4j-admin defaults to 90% of what it believes the machine has,",
        "# and inside a container `free` reports the HOST's memory, not the cgroup",
        "# limit: on a 12Gi pod of a 61 GB node that default targets ~55 GB and the",
        "# importer is OOM-killed. Size it to the POD, not to the node.",
        "#",
        "# Afterwards, check the imported database against manifest.json's",
        "# `expect_in_neo4j` block BEFORE quoting any timing.",
        "set -eu",
    ] {
        sh.push_str(line);
        sh.push('\n');
    }
    sh.push_str(&cmd);
    sh.push('\n');
    std::fs::write(&script, sh)
        .unwrap_or_else(|e| refuse(&format!("write {}: {e}", script.display())));

    // LAST. A manifest is the only thing that says this CSV set reconciled.
    let mj = out.join("manifest.json");
    std::fs::write(&mj, manifest_json(&m))
        .unwrap_or_else(|e| refuse(&format!("write {}: {e}", mj.display())));

    println!("[jsonl2neo4j] nodes: {} total", m.node_total);
    for l in &m.nodes {
        println!(
            "  {:<24} {:>12} rows (census {:>12})  {}",
            l.what,
            l.rows_out,
            l.expected.map_or("-".into(), |e| e.to_string()),
            l.file
        );
    }
    println!("[jsonl2neo4j] relationships: {} total", m.rel_total);
    for l in &m.rels {
        println!(
            "  {:<24} {:>12} rows (census {:>12})  {}",
            l.what,
            l.rows_out,
            l.expected.map_or("-".into(), |e| e.to_string()),
            l.file
        );
    }
    println!(
        "[jsonl2neo4j] every file reconciles: rows read == rows written{} == meta.json census",
        if m.readback {
            " == rows read back from disk"
        } else {
            ""
        }
    );
    println!(
        "[jsonl2neo4j] id presence cost {:.1} MB ({} unstructured id(s)); relationship \
         properties {}",
        m.presence_bytes as f64 / 1e6,
        m.unstructured,
        if m.rel_props {
            "EMITTED (the default: this MATCHES what snbload sends)"
        } else {
            "omitted, matching snbload"
        }
    );
    println!(
        "[jsonl2neo4j] DONE in {:.1}s -> {}\n  manifest: {}\n  import:   sh {}",
        m.seconds,
        out.display(),
        mj.display(),
        script.display()
    );
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn field(s: &str) -> String {
        let mut o = String::new();
        csv_field(s, &mut o);
        o
    }

    #[test]
    fn a_plain_field_is_not_quoted_and_a_delimiter_or_quote_forces_quoting() {
        assert_eq!(field("Mahinda"), "Mahinda");
        // The `;`-joined multi-valued columns datagen2jsonl keeps verbatim must
        // NOT be quoted or turned into an array: they are single strings, and
        // `;` is only the array delimiter for a column declared `[]`.
        assert_eq!(field("si;en"), "si;en");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("a,\"b\""), "\"a,\"\"b\"\"\"");
        assert_eq!(field(""), "");
    }

    #[test]
    fn a_temporal_column_is_declared_and_rendered_as_neo4j_reads_it() {
        // `datagen2jsonl` used to flatten SNB's `creationDate` to epoch millis
        // and BOTH engines got an integer -- consistent, and consistently
        // unable to answer the SNB BI queries, which compare against a
        // `datetime()` literal. Now that `snbload` inlines a real
        // `datetime(...)`, this converter has to declare the same type or the
        // two arms stop being comparable for a reason that is not the engine.
        //
        // `neo4j-admin import` reads `:date` and `:datetime` as ISO-8601.
        assert_eq!(col_ty(&Value::Date(15_203)).unwrap().unwrap(), ColTy::Date);
        assert_eq!(
            col_ty(&Value::DateTime {
                epoch_seconds: 1_313_561_140,
                nanos: 595_000_000,
                offset_seconds: 0,
                zone: None,
            })
            .unwrap()
            .unwrap(),
            ColTy::DateTime
        );
        assert_eq!(ColTy::Date.decl(), "date");
        assert_eq!(ColTy::DateTime.decl(), "datetime");

        // The VALUES, against an independently computed answer: day 15203 is
        // 2011-08-17, and 1313561140.595 is 2011-08-17T06:05:40.595Z -- the
        // same instant the old integer 1313561140595 carried.
        let mut out = String::new();
        render(&Value::Date(15_203), ColTy::Date, &mut out);
        assert_eq!(out, "2011-08-17");
        out.clear();
        render(
            &Value::DateTime {
                epoch_seconds: 1_313_561_140,
                nanos: 595_000_000,
                offset_seconds: 0,
                zone: None,
            },
            ColTy::DateTime,
            &mut out,
        );
        assert_eq!(
            out, "2011-08-17T06:05:40.595Z",
            "`epoch_seconds` is ALREADY UTC (untag_temporal subtracts the              offset), so the offset must not be applied a second time"
        );
    }

    #[test]
    fn a_line_break_in_a_value_is_detected_rather_than_emitted() {
        // --multiline-fields defaults to off, so an emitted newline would end
        // the record early and shift every later column with no error.
        assert!(has_line_break("a\nb"));
        assert!(has_line_break("a\rb"));
        assert!(!has_line_break("a;b"));
    }

    #[test]
    fn a_corpus_id_splits_into_prefix_and_dense_id() {
        assert_eq!(split_gid("p:412"), Some(("p", 412)));
        assert_eq!(split_gid("m:0"), Some(("m", 0)));
        assert_eq!(split_gid("country:11"), Some(("country", 11)));
        // Not structured: no colon, no digits, a sign, or past the dense bound.
        assert_eq!(split_gid("weird"), None);
        assert_eq!(split_gid("p:"), None);
        assert_eq!(split_gid(":7"), None);
        assert_eq!(split_gid("p:-1"), None);
        assert_eq!(split_gid("p:x"), None);
        assert_eq!(split_gid(&format!("p:{}", MAX_DENSE_ID + 1)), None);
    }

    #[test]
    fn presence_answers_membership_and_catches_a_duplicate() {
        let mut p = Presence::default();
        assert!(p.insert("p:0"));
        assert!(p.insert("p:2"));
        assert!(p.insert("m:0")); // a different prefix is a different space
        assert!(p.insert("weird"));
        assert!(!p.insert("p:0"), "a repeated corpus id must be refused");
        assert!(!p.insert("weird"));
        assert!(p.contains("p:0"));
        assert!(p.contains("m:0"));
        assert!(p.contains("weird"));
        assert!(!p.contains("p:1"));
        assert!(!p.contains("f:0"));
        assert!(!p.contains("other"));
    }

    #[test]
    fn presence_costs_a_bit_per_node_not_a_map_entry() {
        // The whole reason this converter does not inherit datagen2jsonl's
        // ~1 GB of BTreeMaps at SF10 (sf10-plan §4.8).
        let mut p = Presence::default();
        for i in 0..1_000_000u64 {
            p.insert(&format!("m:{i}"));
        }
        assert_eq!(p.bytes(), 1_000_000 / 8, "one bit per node, exactly");
    }

    #[test]
    fn density_is_reported_for_a_contiguous_space_and_denied_for_a_gap() {
        let mut s = IdSet::default();
        for i in 0..5 {
            s.insert(i);
        }
        assert!(s.dense());
        let mut g = IdSet::default();
        g.insert(0);
        g.insert(2);
        assert!(!g.dense(), "a gap is not the dense 0..N stress.rs assumes");
        let mut o = IdSet::default();
        o.insert(1);
        o.insert(2);
        assert!(!o.dense(), "a space that does not start at 0 is not dense");
    }

    #[test]
    fn column_types_follow_the_value_and_null_writes_an_empty_field() {
        assert_eq!(col_ty(&Value::Int(1)).unwrap().unwrap(), ColTy::Long);
        assert_eq!(
            col_ty(&Value::Str("x".into())).unwrap().unwrap(),
            ColTy::Str
        );
        assert_eq!(col_ty(&Value::Bool(true)).unwrap().unwrap(), ColTy::Boolean);
        assert_eq!(col_ty(&Value::Float(1.5)).unwrap().unwrap(), ColTy::Double);
        // Null is "no property", exactly what snbload's `null` literal does and
        // what an empty CSV field does — not a refusal, and not "".
        assert!(col_ty(&Value::Null).is_none());
        // Anything snbload could not inline either.
        assert!(
            col_ty(&Value::List((vec![Value::Int(1)]).into()))
                .unwrap()
                .is_err()
        );
        assert!(col_ty(&Value::Float(f64::NAN)).unwrap().is_err());
    }

    #[test]
    fn a_mixed_type_column_is_refused_rather_than_coerced() {
        // datagen2jsonl's `coerced_to_str` counter is exactly this case: a
        // typed field that failed to parse and was kept as a string. A Neo4j
        // import column has one type, so the corpus must be fixed, not this.
        let mut cols = BTreeMap::new();
        let mut cells = 0u64;
        let mut a = BTreeMap::new();
        a.insert("creationDate".to_string(), Value::Int(1));
        merge_cols(&mut cols, &mut cells, &a, "node \"p:0\"", "node :P", &mut Widening::default());
        assert_eq!(cols["creationDate"], ColTy::Long);
        assert_eq!(cells, 1);
        // The conflicting row would call refuse(), which exits the process; the
        // type table it consults is what a test can check directly.
        let mut b = BTreeMap::new();
        b.insert("creationDate".to_string(), Value::Str("2010-01-01".into()));
        assert_ne!(
            col_ty(&b["creationDate"]).unwrap().unwrap(),
            cols["creationDate"],
            "the two rows must disagree, which is what merge_cols refuses on"
        );
    }

    #[test]
    fn property_keys_must_be_bare_identifiers_like_snbload_requires() {
        assert!(bare_identifier("creationDate"));
        assert!(bare_identifier("sourceId"));
        assert!(bare_identifier("HAS_CREATOR"));
        assert!(!bare_identifier(""));
        assert!(!bare_identifier("a b"));
        assert!(!bare_identifier("a-b"));
        assert!(!bare_identifier("a.b"));
    }

    #[test]
    fn a_value_renders_as_the_column_declares() {
        let mut o = String::new();
        render(&Value::Int(-42), ColTy::Long, &mut o);
        assert_eq!(o, "-42");
        o.clear();
        render(&Value::Bool(false), ColTy::Boolean, &mut o);
        assert_eq!(o, "false");
        o.clear();
        render(&Value::Null, ColTy::Str, &mut o);
        assert_eq!(o, "", "null is an empty field: Neo4j sets no property");
        o.clear();
        render(&Value::Str("a,b".into()), ColTy::Str, &mut o);
        assert_eq!(o, "\"a,b\"");
    }

    #[test]
    fn reconcile_catches_each_of_the_ways_a_file_can_be_wrong() {
        let base = |rin, rout, rb, exp| Line {
            file: "nodes_Person.csv".into(),
            what: ":Person".into(),
            census_key: "Person".into(),
            expected: exp,
            rows_in: rin,
            rows_out: rout,
            readback: rb,
            columns: vec![],
        };
        let mut bad = Vec::new();
        reconcile(&base(10, 10, Some(10), Some(10)), &mut bad);
        assert!(bad.is_empty(), "the agreeing case must be silent");

        let mut bad = Vec::new();
        reconcile(&base(10, 9, Some(9), Some(10)), &mut bad);
        assert_eq!(
            bad.len(),
            2,
            "a dropped row fails both the write and the census check"
        );

        let mut bad = Vec::new();
        reconcile(&base(10, 10, Some(7), Some(10)), &mut bad);
        assert!(bad[0].contains("SHORT WRITE"));

        let mut bad = Vec::new();
        reconcile(&base(10, 10, Some(10), Some(11)), &mut bad);
        assert!(bad[0].contains("meta.json says"));

        let mut bad = Vec::new();
        reconcile(&base(10, 10, Some(10), None), &mut bad);
        assert!(bad[0].contains("not a key in meta.json"));
    }

    /// A manifest for the two shapes the generator emits.
    fn sample_manifest() -> Manifest {
        Manifest {
            corpus: PathBuf::from("/datasets/jsonl/sf1"),
            out: PathBuf::from("/datasets/neo4j-csv/sf1"),
            database: "neo4j".into(),
            rel_props: false,
            readback: true,
            nodes: vec![Line {
                file: "nodes_Person.csv".into(),
                what: ":Person".into(),
                census_key: "Person".into(),
                expected: Some(1),
                rows_in: 1,
                rows_out: 1,
                readback: Some(1),
                columns: vec!["gid:ID".into()],
            }],
            rels: vec![Line {
                file: "rels_KNOWS.csv".into(),
                what: ":KNOWS".into(),
                census_key: "KNOWS".into(),
                expected: Some(1),
                rows_in: 1,
                rows_out: 1,
                readback: Some(1),
                columns: vec![":START_ID".into(), ":END_ID".into()],
            }],
            node_total: 1,
            rel_total: 1,
            node_cells: 3,
            rel_cells: 0,
            presence_bytes: 8,
            dense: vec![],
            unstructured: 0,
            unloadable: 0,
            widened: vec![],
            max_row_bytes: 64,
            seconds: 0.1,
        }
    }

    #[test]
    fn the_generated_import_command_pins_every_tolerance_to_zero() {
        let cmd = import_command(&sample_manifest());
        // Each of these defaults to a value that lets a fault through quietly.
        assert!(cmd.contains("--bad-tolerance=0"));
        assert!(cmd.contains("--skip-bad-relationships=false"));
        assert!(cmd.contains("--skip-duplicate-nodes=false"));
        assert!(cmd.contains("--strict=true"));
        assert!(cmd.contains("--id-type=string"));
        assert!(cmd.contains("--trim-strings=false"));
        // Paths are rendered by `Path::display`, so the separator is the host's.
        let out = PathBuf::from("/datasets/neo4j-csv/sf1");
        assert!(cmd.contains(&format!(
            "--relationships=KNOWS={}",
            out.join("rels_KNOWS.csv").display()
        )));
        assert!(cmd.contains(&format!(
            "--nodes={}",
            out.join("nodes_Person.csv").display()
        )));
        // The end-of-options marker: `--relationships` has variable arity, so a
        // bare database name after the last group is parsed as another CSV path
        // and the import dies with "File 'neo4j' doesn't exist".
        assert!(cmd.ends_with("  -- neo4j"));
    }

    #[test]
    fn a_finbench_census_is_read_as_a_node_total_and_per_type_relationships() {
        // Named for the test, not only the process: a pid-named directory is
        // shared by every test in this binary.
        let dir = std::env::temp_dir().join(format!("j2n-census-finbench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p = dir.join("meta.json");
        std::fs::write(
            &p,
            r#"{"family":"finbench","nodes":5,"rels":3,"rel_type_counts":{"transfer":2,"own":1}}"#,
        )
        .expect("write");
        let c = read_census(&p);
        assert_eq!(c.node_total, Some(5));
        assert!(c.nodes.is_empty());
        assert_eq!(c.rels.get("transfer"), Some(&2));
        assert_eq!(c.rels.get("own"), Some(&1));
        // and datagen2jsonl's shape is read as it always was
        std::fs::write(&p, r#"{"nodes":{"Person":4},"rels":{"KNOWS":2}}"#).expect("write");
        let c = read_census(&p);
        assert_eq!(c.node_total, None);
        assert_eq!(c.nodes.get("Person"), Some(&4));
        assert_eq!(c.rels.get("KNOWS"), Some(&2));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mixed_number_column_widens_to_double_only_when_asked() {
        let rows: Vec<BTreeMap<String, Value>> = vec![
            [("amount".to_string(), Value::Int(1000))].into_iter().collect(),
            [("amount".to_string(), Value::Float(12.5))].into_iter().collect(),
            [("amount".to_string(), Value::Int(7))].into_iter().collect(),
        ];
        let mut w = Widening {
            enabled: true,
            ..Widening::default()
        };
        let (mut cols, mut cells) = (BTreeMap::new(), 0u64);
        for r in &rows {
            merge_cols(&mut cols, &mut cells, r, "relationship :repay", "relationship :repay", &mut w);
        }
        assert_eq!(cols.get("amount"), Some(&ColTy::Double));
        assert!(w.widened.contains("relationship :repay.amount"));
        assert_eq!(w.max_int.get("relationship :repay.amount"), Some(&1000));
        let mut out = String::new();
        render(&Value::Int(1000), ColTy::Double, &mut out);
        assert_eq!(out, "1000", "an integer in a widened column is its digits, which Neo4j reads as a double");
    }

    #[test]
    fn the_manifest_states_what_the_imported_database_must_answer() {
        let j = manifest_json(&sample_manifest());
        assert!(j.contains("\"MATCH (n) RETURN count(n)\":1"));
        // 3 property cells + one `gid` per node: the only cheap check that an
        // empty CSV field became an ABSENT property, not an empty string.
        assert!(j.contains("\"MATCH (n) RETURN sum(size(keys(n)))\":4"));
        assert!(j.contains("\"MATCH ()-[r]->() RETURN sum(size(keys(r)))\":0"));
        assert!(j.contains("\"rel_properties_emitted\":false"));
        assert!(j.ends_with("\"ok\":true}\n"));
    }
}
