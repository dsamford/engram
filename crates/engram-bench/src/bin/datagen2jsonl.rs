//! `datagen2jsonl <datagen-dir> <out-dir>` — convert official LDBC SNB Datagen
//! (Interactive v1, Hadoop v0.3.5) CSV output into the `nodes.jsonl` /
//! `rels.jsonl` / `meta.json` corpus that `snbload` loads over Bolt and
//! `engram_bench::load_export` loads in-process.
//!
//! # Dense ids — the load-bearing invariant
//!
//! Datagen ids are sparse 64-bit ints per entity. `stress.rs` SNB mode derives
//! its key space by counting `MATCH (p:Person) RETURN p.id` rows and then
//! probes ids `0..N` directly — against a sparse id space every probe misses
//! and the run silently measures the index's negative path. So every entity
//! family is remapped to a dense `id` property `0..N` in first-seen (file
//! row) order, exactly the invariant `snbgen` guarantees: Person, Forum, Tag
//! and TagClass are dense per label; Continent / Country / City are each
//! dense within the Place file; University / Company within Organisation;
//! and Post + Comment share ONE dense Message id space (all posts first,
//! then comments — total messages always exceeds persons at every SF, which
//! is what the is7-replies key derivation needs). The original Datagen id is
//! preserved as a `sourceId` property — provenance and debuggability only,
//! nothing in the workspace queries it. Corpus-global `"i"` ids reuse
//! snbgen's prefixes (`p:`/`f:`/`m:`/`tag:`/`tc:`/`cont:`/`country:`/`city:`/
//! `univ:`/`company:`) so the two generators' corpora are interchangeable —
//! and the number after the prefix IS the dense `id`, which `snbload` relies
//! on: its relationship pass parses (prefix, id) out of every endpoint string
//! instead of holding a per-node map, so a corpus whose `i` stopped spelling
//! its `id` would load through the loader's per-node fallback map and cost
//! the memory this shape exists to avoid.
//!
//! # Value types
//!
//! `snbload`'s Cypher renderer accepts only Int / Float / Bool / Str / Null
//! and panics on anything else, so: dates and datetimes become epoch-ms ints
//! (both `StringDateFormatter` ISO strings and `LongDateFormatter` plain
//! millis are handled, per field, so either archive variant converts); ints
//! are emitted `~bigint`-tagged like snbgen's (safe for any i64); everything
//! else is a plain JSON string. A typed field that fails to parse is kept as
//! a string and counted — the summary warns rather than the load panicking.
//! Multi-valued attributes are NOT lists (a list panics snbload's renderer):
//! CsvComposite's `;`-joined `language` / `email` columns are kept verbatim,
//! and CsvBasic's one-value-per-row side files are aggregated back into the
//! same `;`-joined single string.
//!
//! # What is deliberately not duplicated
//!
//! KNOWS is emitted once per CSV row. Datagen stores one row per undirected
//! pair, and every consumer here (LSQB q2/q3/q6/q9, stress) matches KNOWS
//! undirected, so a mirrored second edge would only double the stored degree.
//! Relationship properties ARE emitted — `snbload` ignores them but
//! `load_export` loads them, and one corpus feeds both paths.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

// ── JSONL emission (same wire shape as snbgen's) ────────────────────────────

/// Escape a string into a JSON string literal, appended to `out`.
fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A property value. Ints are emitted `~bigint`-tagged (Datagen ids are 64-bit
/// and a bare JSON int above 2^53 silently degrades to a lossy float in the
/// corpus codec); strings as-is. Nothing else occurs in Datagen CSVs.
enum P {
    Int(i64),
    Str(String),
}

fn write_props(props: &[(String, P)], out: &mut String) {
    out.push_str(",\"p\":{");
    for (i, (k, v)) in props.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        esc(k, out);
        out.push(':');
        match v {
            P::Int(n) => {
                out.push_str("{\"~bigint\":\"");
                let _ = write!(out, "{n}");
                out.push_str("\"}");
            }
            P::Str(s) => esc(s, out),
        }
    }
    out.push('}');
}

fn emit_node(id: &str, labels: &[&str], props: &[(String, P)], out: &mut String) {
    out.push_str("{\"i\":");
    esc(id, out);
    out.push_str(",\"l\":[");
    for (i, l) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        esc(l, out);
    }
    out.push(']');
    write_props(props, out);
    out.push_str("}\n");
}

fn emit_rel(s: &str, d: &str, t: &str, props: &[(String, P)], out: &mut String) {
    out.push_str("{\"s\":");
    esc(s, out);
    out.push_str(",\"d\":");
    esc(d, out);
    out.push_str(",\"t\":");
    esc(t, out);
    write_props(props, out);
    out.push_str("}\n");
}

/// A buffered JSONL sink flushing every ~4 MB — SF10 inputs are GB-scale and
/// the corpus must never be held in memory whole.
struct Sink {
    f: std::io::BufWriter<std::fs::File>,
    buf: String,
    lines: u64,
}
impl Sink {
    fn create(path: &Path) -> Self {
        Sink {
            f: std::io::BufWriter::new(
                std::fs::File::create(path)
                    .unwrap_or_else(|e| panic!("create {}: {e}", path.display())),
            ),
            buf: String::with_capacity(1 << 22),
            lines: 0,
        }
    }
    fn write_with(&mut self, build: impl FnOnce(&mut String)) {
        build(&mut self.buf);
        self.lines += 1;
        if self.buf.len() >= (1 << 22) {
            self.f.write_all(self.buf.as_bytes()).expect("write jsonl");
            self.buf.clear();
        }
    }
    fn finish(mut self) -> u64 {
        self.f.write_all(self.buf.as_bytes()).expect("write jsonl");
        self.f.flush().expect("flush jsonl");
        self.lines
    }
}

// ── Date parsing ────────────────────────────────────────────────────────────

/// A plain (optionally negative) decimal integer — the LongDateFormatter
/// representation of every date column, passed through as epoch millis.
fn parse_plain_int(s: &str) -> Option<i64> {
    let t = s.strip_prefix('-').unwrap_or(s);
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `days` since 1970-01-01 for a civil date (Howard Hinnant's algorithm —
/// the inverse of snbgen's `civil_from_days`). No calendar dependency.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// A UTC offset suffix: empty (treated as UTC), `Z`, `±HH`, `±HHMM`, `±HH:MM`.
/// Datagen always writes `+0000`; the rest is tolerance, not speculation.
fn parse_offset_secs(s: &str) -> Option<i64> {
    if s.is_empty() || s == "Z" || s == "z" {
        return Some(0);
    }
    let sign: i64 = match s.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let r = &s[1..];
    let (hh, mm): (i64, i64) = match r.len() {
        2 => (r.parse().ok()?, 0),
        4 => (r[0..2].parse().ok()?, r[2..4].parse().ok()?),
        5 if r.as_bytes()[2] == b':' => (r[0..2].parse().ok()?, r[3..5].parse().ok()?),
        _ => return None,
    };
    if hh > 18 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60))
}

/// A Date column (`yyyy-MM-dd`, or LongDateFormatter epoch millis) → epoch ms.
fn parse_date_ms(s: &str) -> Option<i64> {
    if let Some(ms) = parse_plain_int(s) {
        return Some(ms);
    }
    let (y, m, d) = parse_ymd(s)?;
    Some(days_from_civil(y, m, d) * 86_400_000)
}

/// A DateTime column (`yyyy-MM-dd'T'HH:mm:ss[.SSS]±ZZZZ`, or epoch millis)
/// → epoch ms. Fractions longer than millis truncate; shorter ones pad.
fn parse_datetime_ms(s: &str) -> Option<i64> {
    if let Some(ms) = parse_plain_int(s) {
        return Some(ms);
    }
    let (date, rest) = s.split_at_checked(10)?;
    let (y, m, d) = parse_ymd(date)?;
    let rest = rest.strip_prefix('T')?;
    if rest.len() < 8 {
        return None;
    }
    let rb = rest.as_bytes();
    if rb[2] != b':' || rb[5] != b':' {
        return None;
    }
    let h: i64 = rest[0..2].parse().ok()?;
    let mi: i64 = rest[3..5].parse().ok()?;
    let se: i64 = rest[6..8].parse().ok()?;
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut idx = 8;
    let mut millis: i64 = 0;
    if rb.get(idx) == Some(&b'.') {
        let start = idx + 1;
        let mut end = start;
        while end < rb.len() && rb[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return None;
        }
        let frac = &rb[start..end];
        for k in 0..3 {
            millis = millis * 10 + frac.get(k).map_or(0, |c| i64::from(c - b'0'));
        }
        idx = end;
    }
    let offset = parse_offset_secs(&rest[idx..])?;
    let days = days_from_civil(y, m, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) - offset) * 1000 + millis)
}

// ── Column typing ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Int,
    Date,
    DateTime,
}

/// Column name → type, shared by every entity/edge file (the names never
/// collide across files with conflicting types in the Interactive v1 layout).
/// Unknown columns pass through as strings.
fn column_ty(name: &str) -> Ty {
    match name {
        "birthday" => Ty::Date,
        "creationDate" | "joinDate" => Ty::DateTime,
        "length" | "classYear" | "workFrom" => Ty::Int,
        _ => Ty::Str,
    }
}

fn coerce(raw: &str, ty: Ty, coerced_to_str: &mut u64) -> P {
    let parsed = match ty {
        Ty::Str => return P::Str(raw.to_string()),
        Ty::Int => parse_plain_int(raw),
        Ty::Date => parse_date_ms(raw),
        Ty::DateTime => parse_datetime_ms(raw),
    };
    match parsed {
        Some(n) => P::Int(n),
        None => {
            *coerced_to_str += 1;
            P::Str(raw.to_string())
        }
    }
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ── Entities and the dense-id maps ──────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Place,
    TagClass,
    Tag,
    Organisation,
    Person,
    Forum,
    Post,
    Comment,
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Place => "place",
        Kind::TagClass => "tagclass",
        Kind::Tag => "tag",
        Kind::Organisation => "organisation",
        Kind::Person => "person",
        Kind::Forum => "forum",
        Kind::Post => "post",
        Kind::Comment => "comment",
    }
}

/// Datagen id → corpus id, per entity. Place and Organisation store the full
/// corpus id string (their subtype decides the prefix and dense family, but
/// edge files reference them by the shared `Place.id` / `Organisation.id`
/// space); everything else stores the dense int under a fixed prefix.
#[derive(Default)]
struct Maps {
    place: BTreeMap<i64, String>,
    organisation: BTreeMap<i64, String>,
    tagclass: BTreeMap<i64, i64>,
    tag: BTreeMap<i64, i64>,
    person: BTreeMap<i64, i64>,
    forum: BTreeMap<i64, i64>,
    post: BTreeMap<i64, i64>,
    comment: BTreeMap<i64, i64>,
    continent_n: i64,
    country_n: i64,
    city_n: i64,
    univ_n: i64,
    company_n: i64,
    tagclass_n: i64,
    tag_n: i64,
    person_n: i64,
    forum_n: i64,
    /// Post and Comment share this counter — one dense Message id space.
    msg_n: i64,
}

/// Allot the next dense id in a family and record the mapping. Returns
/// (corpus id, dense id, labels — FIRST label is what snbload's rel pass
/// matches on, so the order must stay identical to snbgen's).
fn assign_node(
    maps: &mut Maps,
    kind: Kind,
    subtype: &str,
    src_id: i64,
) -> Result<(String, i64, &'static [&'static str]), String> {
    fn take(
        map: &mut BTreeMap<i64, i64>,
        ctr: &mut i64,
        prefix: &str,
        labels: &'static [&'static str],
        src_id: i64,
    ) -> Result<(String, i64, &'static [&'static str]), String> {
        let dense = *ctr;
        if map.insert(src_id, dense).is_some() {
            return Err(format!("duplicate {} id {src_id}", labels[labels.len() - 1]));
        }
        *ctr += 1;
        Ok((format!("{prefix}:{dense}"), dense, labels))
    }
    match kind {
        Kind::Place => {
            let (prefix, labels): (&'static str, &'static [&'static str]) = match subtype {
                "continent" => ("cont", &["Place", "Continent"]),
                "country" => ("country", &["Place", "Country"]),
                "city" => ("city", &["Place", "City"]),
                other => return Err(format!("unknown place type {other:?}")),
            };
            let ctr = match subtype {
                "continent" => &mut maps.continent_n,
                "country" => &mut maps.country_n,
                _ => &mut maps.city_n,
            };
            let dense = *ctr;
            let cid = format!("{prefix}:{dense}");
            if maps.place.insert(src_id, cid.clone()).is_some() {
                return Err(format!("duplicate place id {src_id}"));
            }
            *ctr += 1;
            Ok((cid, dense, labels))
        }
        Kind::Organisation => {
            let (prefix, labels): (&'static str, &'static [&'static str]) = match subtype {
                "university" => ("univ", &["Organisation", "University"]),
                "company" => ("company", &["Organisation", "Company"]),
                other => return Err(format!("unknown organisation type {other:?}")),
            };
            let ctr = if subtype == "university" {
                &mut maps.univ_n
            } else {
                &mut maps.company_n
            };
            let dense = *ctr;
            let cid = format!("{prefix}:{dense}");
            if maps.organisation.insert(src_id, cid.clone()).is_some() {
                return Err(format!("duplicate organisation id {src_id}"));
            }
            *ctr += 1;
            Ok((cid, dense, labels))
        }
        Kind::TagClass => take(&mut maps.tagclass, &mut maps.tagclass_n, "tc", &["TagClass"], src_id),
        Kind::Tag => take(&mut maps.tag, &mut maps.tag_n, "tag", &["Tag"], src_id),
        Kind::Person => take(&mut maps.person, &mut maps.person_n, "p", &["Person"], src_id),
        Kind::Forum => take(&mut maps.forum, &mut maps.forum_n, "f", &["Forum"], src_id),
        Kind::Post => take(&mut maps.post, &mut maps.msg_n, "m", &["Message", "Post"], src_id),
        Kind::Comment => take(&mut maps.comment, &mut maps.msg_n, "m", &["Message", "Comment"], src_id),
    }
}

fn resolve(maps: &Maps, kind: Kind, id: i64) -> Option<String> {
    match kind {
        Kind::Place => maps.place.get(&id).cloned(),
        Kind::Organisation => maps.organisation.get(&id).cloned(),
        Kind::TagClass => maps.tagclass.get(&id).map(|n| format!("tc:{n}")),
        Kind::Tag => maps.tag.get(&id).map(|n| format!("tag:{n}")),
        Kind::Person => maps.person.get(&id).map(|n| format!("p:{n}")),
        Kind::Forum => maps.forum.get(&id).map(|n| format!("f:{n}")),
        Kind::Post => maps.post.get(&id).map(|n| format!("m:{n}")),
        Kind::Comment => maps.comment.get(&id).map(|n| format!("m:{n}")),
    }
}

// ── The file inventory ──────────────────────────────────────────────────────

/// Node files in PROCESSING order — posts before comments is load-bearing
/// (the shared Message counter), the rest is just static-before-dynamic.
const NODE_FILES: &[(&str, Kind)] = &[
    ("place", Kind::Place),
    ("tagclass", Kind::TagClass),
    ("tag", Kind::Tag),
    ("organisation", Kind::Organisation),
    ("person", Kind::Person),
    ("forum", Kind::Forum),
    ("post", Kind::Post),
    ("comment", Kind::Comment),
];

struct EdgeSpec {
    base: &'static str,
    src: Kind,
    dst: Kind,
    t: &'static str,
    /// The third column's property name, when the file has one.
    prop: Option<&'static str>,
}

const EDGE_FILES: &[EdgeSpec] = &[
    EdgeSpec { base: "place_isPartOf_place", src: Kind::Place, dst: Kind::Place, t: "IS_PART_OF", prop: None },
    EdgeSpec { base: "tagclass_isSubclassOf_tagclass", src: Kind::TagClass, dst: Kind::TagClass, t: "IS_SUBCLASS_OF", prop: None },
    EdgeSpec { base: "tag_hasType_tagclass", src: Kind::Tag, dst: Kind::TagClass, t: "HAS_TYPE", prop: None },
    EdgeSpec { base: "organisation_isLocatedIn_place", src: Kind::Organisation, dst: Kind::Place, t: "IS_LOCATED_IN", prop: None },
    EdgeSpec { base: "person_isLocatedIn_place", src: Kind::Person, dst: Kind::Place, t: "IS_LOCATED_IN", prop: None },
    EdgeSpec { base: "person_studyAt_organisation", src: Kind::Person, dst: Kind::Organisation, t: "STUDY_AT", prop: Some("classYear") },
    EdgeSpec { base: "person_workAt_organisation", src: Kind::Person, dst: Kind::Organisation, t: "WORK_AT", prop: Some("workFrom") },
    EdgeSpec { base: "person_hasInterest_tag", src: Kind::Person, dst: Kind::Tag, t: "HAS_INTEREST", prop: None },
    EdgeSpec { base: "person_knows_person", src: Kind::Person, dst: Kind::Person, t: "KNOWS", prop: Some("creationDate") },
    EdgeSpec { base: "person_likes_post", src: Kind::Person, dst: Kind::Post, t: "LIKES", prop: Some("creationDate") },
    EdgeSpec { base: "person_likes_comment", src: Kind::Person, dst: Kind::Comment, t: "LIKES", prop: Some("creationDate") },
    EdgeSpec { base: "forum_hasModerator_person", src: Kind::Forum, dst: Kind::Person, t: "HAS_MODERATOR", prop: None },
    EdgeSpec { base: "forum_hasMember_person", src: Kind::Forum, dst: Kind::Person, t: "HAS_MEMBER", prop: Some("joinDate") },
    EdgeSpec { base: "forum_containerOf_post", src: Kind::Forum, dst: Kind::Post, t: "CONTAINER_OF", prop: None },
    EdgeSpec { base: "forum_hasTag_tag", src: Kind::Forum, dst: Kind::Tag, t: "HAS_TAG", prop: None },
    EdgeSpec { base: "post_hasCreator_person", src: Kind::Post, dst: Kind::Person, t: "HAS_CREATOR", prop: None },
    EdgeSpec { base: "post_hasTag_tag", src: Kind::Post, dst: Kind::Tag, t: "HAS_TAG", prop: None },
    EdgeSpec { base: "post_isLocatedIn_place", src: Kind::Post, dst: Kind::Place, t: "IS_LOCATED_IN", prop: None },
    EdgeSpec { base: "comment_hasCreator_person", src: Kind::Comment, dst: Kind::Person, t: "HAS_CREATOR", prop: None },
    EdgeSpec { base: "comment_hasTag_tag", src: Kind::Comment, dst: Kind::Tag, t: "HAS_TAG", prop: None },
    EdgeSpec { base: "comment_isLocatedIn_place", src: Kind::Comment, dst: Kind::Place, t: "IS_LOCATED_IN", prop: None },
    EdgeSpec { base: "comment_replyOf_post", src: Kind::Comment, dst: Kind::Post, t: "REPLY_OF", prop: None },
    EdgeSpec { base: "comment_replyOf_comment", src: Kind::Comment, dst: Kind::Comment, t: "REPLY_OF", prop: None },
];

// ── Discovery ───────────────────────────────────────────────────────────────

/// `<base>.csv` or `<base>_<digits>_<digits>.csv` — and nothing else, so
/// `person` never swallows `person_knows_person_0_0.csv` (the suffix after
/// the underscore must be exactly two numeric parts).
fn is_part_name(name: &str, base: &str) -> bool {
    let Some(rest) = name.strip_prefix(base) else {
        return false;
    };
    if rest == ".csv" {
        return true;
    }
    let Some(rest) = rest.strip_prefix('_') else {
        return false;
    };
    let Some(nums) = rest.strip_suffix(".csv") else {
        return false;
    };
    let mut parts = nums.split('_');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && a.bytes().all(|c| c.is_ascii_digit())
        && b.bytes().all(|c| c.is_ascii_digit())
}

fn subdirs(d: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The directories a Datagen layout can hide entity files in: the input dir
/// itself (flat), its subdirectories (`static/`, `dynamic/`), and one level
/// deeper (an extraction parent holding `social_network-sf…/static/`).
fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for d1 in subdirs(root) {
        dirs.push(d1.clone());
        dirs.extend(subdirs(&d1));
    }
    dirs
}

/// All part files for one entity, from the FIRST candidate dir that has any —
/// mixing parts of one entity across directories would double-load it.
fn find_parts(dirs: &[PathBuf], base: &str) -> Vec<PathBuf> {
    for d in dirs {
        let mut matches: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                if is_part_name(name, base) {
                    matches.push(e.path());
                }
            }
        }
        if !matches.is_empty() {
            matches.sort();
            return matches;
        }
    }
    Vec::new()
}

struct Inputs {
    node_files: Vec<(Kind, Vec<PathBuf>)>,
    edge_files: Vec<(&'static EdgeSpec, Vec<PathBuf>)>,
    /// CsvBasic-only side files; empty for CsvComposite.
    email: Vec<PathBuf>,
    speaks: Vec<PathBuf>,
}

fn refusal(root: &Path, dirs: &[PathBuf], missing: &[&str]) -> String {
    let mut msg = format!(
        "missing required input file(s) under {} (searched {} dir(s), depth <= 2):\n",
        root.display(),
        dirs.len()
    );
    for m in missing {
        let _ = writeln!(msg, "  {m}.csv / {m}_0_0.csv");
    }
    msg.push_str("CSV files actually found:\n");
    let mut any = false;
    for d in dirs {
        if let Ok(rd) = std::fs::read_dir(d) {
            let mut names: Vec<String> = rd
                .flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_str()?.to_string();
                    (n.ends_with(".csv") && !n.starts_with('.')).then_some(n)
                })
                .collect();
            names.sort();
            for n in names {
                any = true;
                let _ = writeln!(msg, "  {}", d.join(n).display());
            }
        }
    }
    if !any {
        msg.push_str("  (none)\n");
    }
    msg.push_str(
        "expected LDBC SNB Interactive v1 Datagen CSV output — static/ + dynamic/ \
         (or flat), files named like person_0_0.csv or person.csv",
    );
    msg
}

fn discover(root: &Path) -> Result<Inputs, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let dirs = candidate_dirs(root);
    let mut missing: Vec<&str> = Vec::new();
    let mut node_files = Vec::new();
    for (base, kind) in NODE_FILES {
        let parts = find_parts(&dirs, base);
        if parts.is_empty() {
            missing.push(base);
        } else {
            node_files.push((*kind, parts));
        }
    }
    let mut edge_files = Vec::new();
    for spec in EDGE_FILES {
        let parts = find_parts(&dirs, spec.base);
        if parts.is_empty() {
            missing.push(spec.base);
        } else {
            edge_files.push((spec, parts));
        }
    }
    if !missing.is_empty() {
        return Err(refusal(root, &dirs, &missing));
    }
    Ok(Inputs {
        node_files,
        edge_files,
        email: find_parts(&dirs, "person_email_emailaddress"),
        speaks: find_parts(&dirs, "person_speaks_language"),
    })
}

// ── Conversion ──────────────────────────────────────────────────────────────

/// What one conversion did — printed to stdout and recorded in `meta.json`.
#[derive(Default, Debug)]
struct Summary {
    nodes: u64,
    rels: u64,
    node_counts: BTreeMap<&'static str, u64>,
    rel_counts: BTreeMap<&'static str, u64>,
    /// Empty CSV fields, whose property was omitted rather than written as "".
    empty_fields_omitted: u64,
    /// Typed fields that failed to parse and were kept as strings.
    coerced_to_str: u64,
    persons: i64,
    messages: i64,
}

/// CsvBasic multi-valued attributes, aggregated per person before the person
/// node pass (the values must land ON the node record, and person files are
/// small — the GB-scale inputs are posts/comments, which stream).
#[derive(Default)]
struct SideData {
    email: BTreeMap<i64, Vec<String>>,
    speaks: BTreeMap<i64, Vec<String>>,
}

fn load_side_values(paths: &[PathBuf]) -> Result<BTreeMap<i64, Vec<String>>, String> {
    let mut out: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for path in paths {
        let file =
            std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut lines = std::io::BufReader::with_capacity(1 << 20, file).lines().enumerate();
        match lines.next() {
            None => continue, // empty file — side data is optional
            Some((_, Err(e))) => return Err(format!("{}: read: {e}", path.display())),
            Some((_, Ok(_header))) => {}
        }
        for (n, line) in lines {
            let line = line.map_err(|e| format!("{}: read: {e}", path.display()))?;
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let mut it = line.split('|');
            let (Some(id_raw), Some(val), None) = (it.next(), it.next(), it.next()) else {
                return Err(format!(
                    "{}: line {}: expected exactly 2 |-separated fields",
                    path.display(),
                    n + 1
                ));
            };
            let id = parse_plain_int(id_raw).ok_or_else(|| {
                format!("{}: line {}: unparseable person id {id_raw:?}", path.display(), n + 1)
            })?;
            out.entry(id).or_default().push(val.to_string());
        }
    }
    Ok(out)
}

fn convert_node_file(
    path: &Path,
    kind: Kind,
    maps: &mut Maps,
    side: &SideData,
    sink: &mut Sink,
    sum: &mut Summary,
) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut lines = std::io::BufReader::with_capacity(1 << 20, file).lines().enumerate();
    let header_line = match lines.next() {
        None => return Err(format!("{}: empty file (no header row)", path.display())),
        Some((_, Err(e))) => return Err(format!("{}: read: {e}", path.display())),
        Some((_, Ok(l))) => l,
    };
    let header: Vec<String> = header_line
        .trim_start_matches('\u{feff}')
        .trim_end_matches('\r')
        .split('|')
        .map(str::to_string)
        .collect();
    for h in &header {
        if !ident_ok(h) {
            return Err(format!(
                "{}: header column {h:?} is not a bare identifier — node columns become \
                 property keys, which the loader requires to be [A-Za-z0-9_]",
                path.display()
            ));
        }
    }
    let id_col = header
        .iter()
        .position(|h| h == "id")
        .ok_or_else(|| format!("{}: no 'id' column in header {header:?}", path.display()))?;
    let type_col = if matches!(kind, Kind::Place | Kind::Organisation) {
        Some(header.iter().position(|h| h == "type").ok_or_else(|| {
            format!("{}: no 'type' column in header {header:?}", path.display())
        })?)
    } else {
        None
    };
    let has_language = header.iter().any(|h| h.as_str() == "language");
    let has_email = header.iter().any(|h| h.as_str() == "email");
    let mut rows = 0u64;
    for (n, line) in lines {
        let lineno = n + 1;
        let line = line.map_err(|e| format!("{}: read: {e}", path.display()))?;
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() != header.len() {
            return Err(format!(
                "{}: line {lineno}: {} field(s) but the header has {} — the dialect has no \
                 quoting, so this is a malformed row, not a quoted delimiter",
                path.display(),
                fields.len(),
                header.len()
            ));
        }
        let src_id = parse_plain_int(fields[id_col]).ok_or_else(|| {
            format!("{}: line {lineno}: unparseable id {:?}", path.display(), fields[id_col])
        })?;
        let subtype = type_col.map_or("", |c| fields[c]);
        let (cid, dense, labels) = assign_node(maps, kind, subtype, src_id)
            .map_err(|e| format!("{}: line {lineno}: {e}", path.display()))?;
        let mut props: Vec<(String, P)> = Vec::with_capacity(header.len() + 2);
        props.push(("id".to_string(), P::Int(dense)));
        for (ci, h) in header.iter().enumerate() {
            if ci == id_col {
                continue;
            }
            let raw = fields[ci];
            if raw.is_empty() {
                sum.empty_fields_omitted += 1;
                continue;
            }
            props.push((h.clone(), coerce(raw, column_ty(h), &mut sum.coerced_to_str)));
        }
        if kind == Kind::Person {
            if !has_language {
                if let Some(v) = side.speaks.get(&src_id) {
                    props.push(("language".to_string(), P::Str(v.join(";"))));
                }
            }
            if !has_email {
                if let Some(v) = side.email.get(&src_id) {
                    props.push(("email".to_string(), P::Str(v.join(";"))));
                }
            }
        }
        props.push(("sourceId".to_string(), P::Int(src_id)));
        sink.write_with(|out| emit_node(&cid, labels, &props, out));
        *sum.node_counts.entry(labels[labels.len() - 1]).or_default() += 1;
        rows += 1;
    }
    eprintln!("[datagen2jsonl] {}: {rows} node row(s)", path.display());
    Ok(())
}

fn convert_edge_file(
    path: &Path,
    spec: &EdgeSpec,
    maps: &Maps,
    sink: &mut Sink,
    sum: &mut Summary,
) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut lines = std::io::BufReader::with_capacity(1 << 20, file).lines().enumerate();
    let header_line = match lines.next() {
        None => return Err(format!("{}: empty file (no header row)", path.display())),
        Some((_, Err(e))) => return Err(format!("{}: read: {e}", path.display())),
        Some((_, Ok(l))) => l,
    };
    let want = 2 + usize::from(spec.prop.is_some());
    let header_cols = header_line
        .trim_start_matches('\u{feff}')
        .trim_end_matches('\r')
        .split('|')
        .count();
    if header_cols != want {
        return Err(format!(
            "{}: header has {header_cols} column(s), expected {want} for {}",
            path.display(),
            spec.t
        ));
    }
    // A header is words, not data: the first field of a real Datagen edge
    // header ("Person.id", "Comment.id", ...) never parses as an integer. A
    // headerless variant would otherwise lose exactly one edge per part file
    // — silently, because the column COUNT matches. Refuse loudly instead.
    let first_field = header_line
        .trim_start_matches('\u{feff}')
        .trim_end_matches('\r')
        .split('|')
        .next()
        .unwrap_or("");
    if first_field.parse::<i64>().is_ok() {
        return Err(format!(
            "{}: first row looks like DATA, not a header ({first_field}|…) — headerless \
             layout; refusing rather than silently dropping one edge row",
            path.display()
        ));
    }
    let mut rows = 0u64;
    for (n, line) in lines {
        let lineno = n + 1;
        let line = line.map_err(|e| format!("{}: read: {e}", path.display()))?;
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() != want {
            return Err(format!(
                "{}: line {lineno}: {} field(s), expected {want}",
                path.display(),
                fields.len()
            ));
        }
        let s_id = parse_plain_int(fields[0]).ok_or_else(|| {
            format!("{}: line {lineno}: unparseable id {:?}", path.display(), fields[0])
        })?;
        let d_id = parse_plain_int(fields[1]).ok_or_else(|| {
            format!("{}: line {lineno}: unparseable id {:?}", path.display(), fields[1])
        })?;
        // An unknown endpoint is a HARD error, not a skip: snbload would skip
        // the rel with only a warning, and a corpus that quietly loses edges
        // loads fine and answers every traversal short.
        let s_cid = resolve(maps, spec.src, s_id).ok_or_else(|| {
            format!(
                "{}: line {lineno}: {} edge references unknown {} id {s_id}",
                path.display(),
                spec.t,
                kind_name(spec.src)
            )
        })?;
        let d_cid = resolve(maps, spec.dst, d_id).ok_or_else(|| {
            format!(
                "{}: line {lineno}: {} edge references unknown {} id {d_id}",
                path.display(),
                spec.t,
                kind_name(spec.dst)
            )
        })?;
        let mut props: Vec<(String, P)> = Vec::new();
        if let Some(pname) = spec.prop {
            let raw = fields[2];
            if raw.is_empty() {
                sum.empty_fields_omitted += 1;
            } else {
                props.push((pname.to_string(), coerce(raw, column_ty(pname), &mut sum.coerced_to_str)));
            }
        }
        sink.write_with(|out| emit_rel(&s_cid, &d_cid, spec.t, &props, out));
        *sum.rel_counts.entry(spec.t).or_default() += 1;
        rows += 1;
    }
    eprintln!("[datagen2jsonl] {}: {rows} edge row(s)", path.display());
    Ok(())
}

fn counts_json(c: &BTreeMap<&'static str, u64>) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in c.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "\"{k}\":{v}");
    }
    s.push('}');
    s
}

/// `meta.json`, in snbgen's shape: `captured_at` (the only field the load
/// consumes — end of the SNB window, same instant snbgen stamps) plus
/// provenance, with the conversion summary folded in.
fn write_meta(out_dir: &Path, sum: &Summary) -> Result<(), String> {
    let mut m = String::new();
    m.push_str("{\"captured_at\":{\"~dt\":\"2013-01-01T00:00:00.000+0000\"}");
    m.push_str(",\"generator\":\"datagen2jsonl\"");
    let _ = write!(m, ",\"persons\":{},\"messages\":{}", sum.persons, sum.messages);
    let _ = write!(m, ",\"nodes\":{}", counts_json(&sum.node_counts));
    let _ = write!(m, ",\"rels\":{}", counts_json(&sum.rel_counts));
    let _ = write!(
        m,
        ",\"empty_fields_omitted\":{},\"coerced_to_str\":{}}}",
        sum.empty_fields_omitted, sum.coerced_to_str
    );
    std::fs::write(out_dir.join("meta.json"), m)
        .map_err(|e| format!("write {}: {e}", out_dir.join("meta.json").display()))
}

fn convert(in_dir: &Path, out_dir: &Path) -> Result<Summary, String> {
    let inputs = discover(in_dir)?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("create {}: {e}", out_dir.display()))?;
    let side = SideData {
        email: load_side_values(&inputs.email)?,
        speaks: load_side_values(&inputs.speaks)?,
    };
    let mut maps = Maps::default();
    let mut sum = Summary::default();

    let mut nodes = Sink::create(&out_dir.join("nodes.jsonl"));
    for (kind, paths) in &inputs.node_files {
        for path in paths {
            convert_node_file(path, *kind, &mut maps, &side, &mut nodes, &mut sum)?;
        }
    }
    sum.nodes = nodes.finish();

    let mut rels = Sink::create(&out_dir.join("rels.jsonl"));
    for (spec, paths) in &inputs.edge_files {
        for path in paths {
            convert_edge_file(path, spec, &maps, &mut rels, &mut sum)?;
        }
    }
    sum.rels = rels.finish();

    sum.persons = maps.person_n;
    sum.messages = maps.msg_n;
    write_meta(out_dir, &sum)?;
    Ok(sum)
}

fn print_summary(sum: &Summary) {
    println!("[datagen2jsonl] nodes: {} total", sum.nodes);
    for (k, v) in &sum.node_counts {
        println!("  {k}: {v}");
    }
    println!("[datagen2jsonl] rels: {} total", sum.rels);
    for (k, v) in &sum.rel_counts {
        println!("  {k}: {v}");
    }
    println!(
        "[datagen2jsonl] persons={} messages={} — `id` is DENSE 0..N per family \
         (the invariant stress.rs relies on); original Datagen ids kept as `sourceId`",
        sum.persons, sum.messages
    );
    println!(
        "[datagen2jsonl] empty fields omitted: {}; typed values kept as strings: {}",
        sum.empty_fields_omitted, sum.coerced_to_str
    );
    if sum.coerced_to_str > 0 {
        println!(
            "[datagen2jsonl] WARNING: {} value(s) failed typed parsing and were written \
             as strings — grep the corpus before trusting date-ordered queries",
            sum.coerced_to_str
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: datagen2jsonl <datagen-dir> <out-dir>");
        eprintln!(
            "  converts LDBC SNB Interactive v1 Datagen CSV output (CsvBasic or CsvComposite, \
             String- or LongDateFormatter) into the nodes.jsonl/rels.jsonl/meta.json corpus \
             that snbload and load_export read."
        );
        std::process::exit(2);
    }
    let t0 = Instant::now();
    match convert(Path::new(&args[1]), Path::new(&args[2])) {
        Ok(sum) => {
            print_summary(&sum);
            println!(
                "[datagen2jsonl] DONE in {:.1}s -> {}",
                t0.elapsed().as_secs_f64(),
                args[2]
            );
        }
        Err(e) => {
            eprintln!("datagen2jsonl: {e}");
            std::process::exit(1);
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use engram_cypher::{Value, json};
    use std::collections::BTreeSet;

    // ── fixture ─────────────────────────────────────────────────────────────

    fn tdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("datagen2jsonl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fname(base: &str, numbered: bool) -> String {
        if numbered { format!("{base}_0_0.csv") } else { format!("{base}.csv") }
    }

    fn wf(dir: &Path, base: &str, numbered: bool, content: &str) {
        std::fs::write(dir.join(fname(base, numbered)), content).unwrap();
    }

    /// A miniature but COMPLETE Datagen output: sparse ids, both date
    /// formatter styles mixed, an empty optional field, a multi-valued
    /// attribute, a literal double-quote (the dialect has no quoting).
    fn write_fixture(root: &Path, nested: bool, numbered: bool, basic: bool) {
        let (stat, dynd) = if nested {
            (
                root.join("social_network").join("static"),
                root.join("social_network").join("dynamic"),
            )
        } else {
            (root.to_path_buf(), root.to_path_buf())
        };
        std::fs::create_dir_all(&stat).unwrap();
        std::fs::create_dir_all(&dynd).unwrap();

        // static
        wf(&stat, "place", numbered,
            "id|name|url|type\n\
             0|Africa|http://x/Africa|continent\n\
             10|India|http://x/India|country\n\
             55|Mumbai|http://x/Mumbai|city\n");
        wf(&stat, "tagclass", numbered, "id|name|url\n3|Thing|http://x/Thing\n");
        wf(&stat, "tag", numbered, "id|name|url\n1386|Hamid_Karzai|http://x/HK\n");
        wf(&stat, "organisation", numbered,
            "id|type|name|url\n\
             2755|university|MIT|http://x/MIT\n\
             900|company|Acme|http://x/Acme\n");
        wf(&stat, "place_isPartOf_place", numbered, "Place.id|Place.id\n55|10\n10|0\n");
        wf(&stat, "tagclass_isSubclassOf_tagclass", numbered, "TagClass.id|TagClass.id\n");
        wf(&stat, "tag_hasType_tagclass", numbered, "Tag.id|TagClass.id\n1386|3\n");
        wf(&stat, "organisation_isLocatedIn_place", numbered,
            "Organisation.id|Place.id\n2755|55\n900|10\n");

        // dynamic — person rows: sparse ids 933, 12, 4398046511151; row for
        // person 12 uses LongDateFormatter-style epoch millis in the same
        // columns; person 4398046511151 has a literal `"` in lastName.
        if basic {
            wf(&dynd, "person", numbered,
                "id|firstName|lastName|gender|birthday|creationDate|locationIP|browserUsed\n\
                 933|Mahinda|Perera|male|1989-12-03|2010-02-14T15:32:10.447+0000|119.235.7.103|Firefox\n\
                 12|Jane|Doe|female|628646400000|1266161530447|10.0.0.1|Chrome\n\
                 4398046511151|Bob|Sm\"ith|male|1985-01-01|2011-01-01T00:00:00.000+0000|10.0.0.2|Safari\n");
            wf(&dynd, "person_email_emailaddress", numbered,
                "Person.id|email\n933|a@x.com\n933|b@y.com\n4398046511151|bob@x.com\n");
            wf(&dynd, "person_speaks_language", numbered,
                "Person.id|language\n933|si\n933|en\n4398046511151|en\n");
        } else {
            wf(&dynd, "person", numbered,
                "id|firstName|lastName|gender|birthday|creationDate|locationIP|browserUsed|language|email\n\
                 933|Mahinda|Perera|male|1989-12-03|2010-02-14T15:32:10.447+0000|119.235.7.103|Firefox|si;en|a@x.com;b@y.com\n\
                 12|Jane|Doe|female|628646400000|1266161530447|10.0.0.1|Chrome||\n\
                 4398046511151|Bob|Sm\"ith|male|1985-01-01|2011-01-01T00:00:00.000+0000|10.0.0.2|Safari|en|bob@x.com\n");
        }
        wf(&dynd, "forum", numbered,
            "id|title|creationDate\n37|Wall of Mahinda Perera|2010-02-15T00:46:00.000+0000\n");
        wf(&dynd, "post", numbered,
            "id|imageFile|creationDate|locationIP|browserUsed|language|content|length\n\
             618475290624||2011-08-17T06:05:40.595+0000|49.14.113.213|Firefox|en|About stuff|11\n\
             618475290625|photo.jpg|2011-08-17T06:05:41.000+0000|49.14.113.213|Firefox|||0\n");
        wf(&dynd, "comment", numbered,
            "id|creationDate|locationIP|browserUsed|content|length\n\
             1030792151058|2012-01-01T00:00:00.000+0000|10.1.1.1|Chrome|hi \"there\"|10\n");
        wf(&dynd, "person_isLocatedIn_place", numbered, "Person.id|Place.id\n933|55\n");
        wf(&dynd, "person_hasInterest_tag", numbered, "Person.id|Tag.id\n933|1386\n");
        wf(&dynd, "person_studyAt_organisation", numbered,
            "Person.id|Organisation.id|classYear\n933|2755|2006\n");
        wf(&dynd, "person_workAt_organisation", numbered,
            "Person.id|Organisation.id|workFrom\n12|900|2010\n");
        wf(&dynd, "person_knows_person", numbered,
            "Person.id|Person.id|creationDate\n933|12|2010-07-30T15:19:53.298+0000\n");
        wf(&dynd, "person_likes_post", numbered,
            "Person.id|Post.id|creationDate\n12|618475290624|2011-09-01T00:00:00.000+0000\n");
        wf(&dynd, "person_likes_comment", numbered,
            "Person.id|Comment.id|creationDate\n933|1030792151058|2012-02-01T00:00:00.000+0000\n");
        wf(&dynd, "forum_hasModerator_person", numbered, "Forum.id|Person.id\n37|933\n");
        wf(&dynd, "forum_hasMember_person", numbered,
            "Forum.id|Person.id|joinDate\n37|12|2010-03-01T00:00:00.000+0000\n");
        wf(&dynd, "forum_containerOf_post", numbered,
            "Forum.id|Post.id\n37|618475290624\n37|618475290625\n");
        wf(&dynd, "forum_hasTag_tag", numbered, "Forum.id|Tag.id\n37|1386\n");
        wf(&dynd, "post_hasCreator_person", numbered, "Post.id|Person.id\n618475290624|933\n");
        wf(&dynd, "post_hasTag_tag", numbered, "Post.id|Tag.id\n618475290624|1386\n");
        wf(&dynd, "post_isLocatedIn_place", numbered, "Post.id|Place.id\n618475290624|10\n");
        wf(&dynd, "comment_hasCreator_person", numbered,
            "Comment.id|Person.id\n1030792151058|12\n");
        wf(&dynd, "comment_hasTag_tag", numbered, "Comment.id|Tag.id\n");
        wf(&dynd, "comment_isLocatedIn_place", numbered,
            "Comment.id|Place.id\n1030792151058|10\n");
        wf(&dynd, "comment_replyOf_post", numbered,
            "Comment.id|Post.id\n1030792151058|618475290624\n");
        wf(&dynd, "comment_replyOf_comment", numbered, "Comment.id|Comment.id\n");
    }

    // ── output readers ──────────────────────────────────────────────────────

    fn read_jsonl_values(p: &Path) -> Vec<Value> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| json::from_json(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}")))
            .collect()
    }

    fn as_map(v: &Value) -> &BTreeMap<String, Value> {
        match v {
            Value::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        }
    }

    fn node<'a>(nodes: &'a [Value], i: &str) -> &'a BTreeMap<String, Value> {
        nodes
            .iter()
            .map(as_map)
            .find(|m| matches!(m.get("i"), Some(Value::Str(s)) if s == i))
            .unwrap_or_else(|| panic!("no node with i={i}"))
    }

    fn labels(m: &BTreeMap<String, Value>) -> Vec<String> {
        match m.get("l") {
            Some(Value::List(ls)) => ls
                .iter()
                .map(|v| match v {
                    Value::Str(s) => s.clone(),
                    other => panic!("non-string label {other:?}"),
                })
                .collect(),
            other => panic!("no label list: {other:?}"),
        }
    }

    /// A property, decoded exactly the way the loaders decode it.
    fn prop(m: &BTreeMap<String, Value>, k: &str) -> Value {
        let Some(Value::Map(p)) = m.get("p") else { panic!("no props map") };
        let raw = p.get(k).unwrap_or_else(|| panic!("no prop {k}: {p:?}"));
        let mut unloadable = 0usize;
        let v = engram_bench::untag_prop(raw, &mut unloadable);
        assert_eq!(unloadable, 0, "prop {k} was unloadable");
        v
    }

    fn has_prop(m: &BTreeMap<String, Value>, k: &str) -> bool {
        matches!(m.get("p"), Some(Value::Map(p)) if p.contains_key(k))
    }

    fn rel<'a>(rels: &'a [Value], s: &str, t: &str, d: &str) -> &'a BTreeMap<String, Value> {
        rels.iter()
            .map(as_map)
            .find(|m| {
                matches!(m.get("s"), Some(Value::Str(x)) if x == s)
                    && matches!(m.get("t"), Some(Value::Str(x)) if x == t)
                    && matches!(m.get("d"), Some(Value::Str(x)) if x == d)
            })
            .unwrap_or_else(|| panic!("no rel ({s})-[:{t}]->({d})"))
    }

    // ── tests ───────────────────────────────────────────────────────────────

    #[test]
    fn dense_remap_and_shared_message_space() {
        let root = tdir("dense");
        write_fixture(&root, false, true, false);
        let out = root.join("out");
        let sum = convert(&root, &out).expect("convert");
        let nodes = read_jsonl_values(&out.join("nodes.jsonl"));
        let rels_v = read_jsonl_values(&out.join("rels.jsonl"));

        // Sparse person ids 933, 12, 4398046511151 → dense 0, 1, 2 in file order.
        for (cid, dense, source) in
            [("p:0", 0, 933), ("p:1", 1, 12), ("p:2", 2, 4_398_046_511_151)]
        {
            let p = node(&nodes, cid);
            assert_eq!(prop(p, "id"), Value::Int(dense));
            assert_eq!(prop(p, "sourceId"), Value::Int(source));
        }
        // Posts take the first dense message ids, comments continue the SAME space.
        assert_eq!(labels(node(&nodes, "m:0")), ["Message", "Post"]);
        assert_eq!(labels(node(&nodes, "m:1")), ["Message", "Post"]);
        assert_eq!(labels(node(&nodes, "m:2")), ["Message", "Comment"]);
        assert_eq!(prop(node(&nodes, "m:2"), "id"), Value::Int(2));
        // Place subtypes are each dense from 0; organisations likewise.
        assert_eq!(labels(node(&nodes, "cont:0")), ["Place", "Continent"]);
        assert_eq!(labels(node(&nodes, "country:0")), ["Place", "Country"]);
        assert_eq!(labels(node(&nodes, "city:0")), ["Place", "City"]);
        assert_eq!(labels(node(&nodes, "univ:0")), ["Organisation", "University"]);
        assert_eq!(labels(node(&nodes, "company:0")), ["Organisation", "Company"]);

        // Edges were remapped through the same maps.
        rel(&rels_v, "city:0", "IS_PART_OF", "country:0");
        rel(&rels_v, "country:0", "IS_PART_OF", "cont:0");
        rel(&rels_v, "m:2", "REPLY_OF", "m:0");
        rel(&rels_v, "p:1", "LIKES", "m:0");
        rel(&rels_v, "univ:0", "IS_LOCATED_IN", "city:0");
        let k = rel(&rels_v, "p:0", "KNOWS", "p:1");
        assert!(matches!(prop(k, "creationDate"), Value::Int(n) if n > 0));
        let st = rel(&rels_v, "p:0", "STUDY_AT", "univ:0");
        assert_eq!(prop(st, "classYear"), Value::Int(2006));

        // Summary counts.
        assert_eq!(sum.nodes, 14);
        assert_eq!(sum.rels, 23);
        assert_eq!(sum.node_counts["Person"], 3);
        assert_eq!(sum.node_counts["Post"], 2);
        assert_eq!(sum.node_counts["Comment"], 1);
        assert_eq!(sum.rel_counts["KNOWS"], 1);
        assert_eq!(sum.rel_counts["IS_LOCATED_IN"], 5);
        assert_eq!(sum.persons, 3);
        assert_eq!(sum.messages, 3);
        assert_eq!(sum.coerced_to_str, 0);
        assert_eq!(sum.empty_fields_omitted, 5);

        // meta.json parses and keeps snbgen's captured_at shape.
        let meta = std::fs::read_to_string(out.join("meta.json")).unwrap();
        let Value::Map(m) = json::from_json(&meta).unwrap() else { panic!("meta not a map") };
        assert!(matches!(m.get("captured_at"), Some(Value::Map(_))));
        assert!(meta.contains("\"persons\":3"));
        assert!(meta.contains("\"messages\":3"));
    }

    #[test]
    fn dates_parse_to_epoch_ms_in_both_formatter_variants() {
        let root = tdir("dates");
        write_fixture(&root, false, true, false);
        let out = root.join("out");
        convert(&root, &out).expect("convert");
        let nodes = read_jsonl_values(&out.join("nodes.jsonl"));
        // StringDateFormatter row.
        let p0 = node(&nodes, "p:0");
        assert_eq!(prop(p0, "birthday"), Value::Int(628_646_400_000));
        assert_eq!(prop(p0, "creationDate"), Value::Int(1_266_161_530_447));
        // LongDateFormatter-style row (plain epoch millis) — same values.
        let p1 = node(&nodes, "p:1");
        assert_eq!(prop(p1, "birthday"), Value::Int(628_646_400_000));
        assert_eq!(prop(p1, "creationDate"), Value::Int(1_266_161_530_447));
    }

    #[test]
    fn datetime_parser_anchors() {
        // Anchors verified against the extracted SF0.1 String/Long archives.
        assert_eq!(parse_datetime_ms("2010-02-14T15:32:10.447+0000"), Some(1_266_161_530_447));
        assert_eq!(parse_datetime_ms("2010-01-01T00:00:00.000+0000"), Some(1_262_304_000_000));
        assert_eq!(parse_datetime_ms("2013-01-01T00:00:00.000+0000"), Some(1_356_998_400_000));
        // An offset shifts the instant.
        assert_eq!(parse_datetime_ms("2010-01-01T01:00:00.000+0100"), Some(1_262_304_000_000));
        assert_eq!(parse_datetime_ms("2010-01-01T00:00:00Z"), Some(1_262_304_000_000));
        // LongDateFormatter passthrough.
        assert_eq!(parse_datetime_ms("1266161530447"), Some(1_266_161_530_447));
        assert_eq!(parse_datetime_ms("not-a-date"), None);
        assert_eq!(parse_date_ms("1989-12-03"), Some(628_646_400_000));
        assert_eq!(parse_date_ms("628646400000"), Some(628_646_400_000));
        assert_eq!(parse_date_ms("12-03-1989"), None);
    }

    #[test]
    fn quotes_are_literal_text_not_csv_quoting() {
        // The Datagen dialect has NO quoting and no escape character — a `"`
        // in a field is data and must survive to the JSON verbatim.
        let root = tdir("quotes");
        write_fixture(&root, false, true, false);
        let out = root.join("out");
        convert(&root, &out).expect("convert");
        let nodes = read_jsonl_values(&out.join("nodes.jsonl"));
        assert_eq!(prop(node(&nodes, "p:2"), "lastName"), Value::Str("Sm\"ith".into()));
        assert_eq!(prop(node(&nodes, "m:2"), "content"), Value::Str("hi \"there\"".into()));
    }

    #[test]
    fn pipe_in_a_field_is_a_hard_error_not_a_misparse() {
        // No quoting means a raw `|` in content CANNOT be represented; a row
        // whose field count disagrees with the header must refuse loudly.
        let root = tdir("pipefield");
        write_fixture(&root, false, true, false);
        std::fs::write(
            root.join("comment_0_0.csv"),
            "id|creationDate|locationIP|browserUsed|content|length\n\
             1030792151058|2012-01-01T00:00:00.000+0000|10.1.1.1|Chrome|hi|there|10\n",
        )
        .unwrap();
        let err = convert(&root, &root.join("out")).unwrap_err();
        assert!(err.contains("comment_0_0.csv"), "{err}");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn unknown_edge_endpoint_names_file_and_line() {
        let root = tdir("dangling");
        write_fixture(&root, false, true, false);
        std::fs::write(
            root.join("person_knows_person_0_0.csv"),
            "Person.id|Person.id|creationDate\n933|999|2010-07-30T15:19:53.298+0000\n",
        )
        .unwrap();
        let err = convert(&root, &root.join("out")).unwrap_err();
        assert!(err.contains("person_knows_person"), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("999"), "{err}");
        assert!(err.contains("KNOWS"), "{err}");
    }

    #[test]
    fn multivalued_attributes_join_to_semicolon_strings() {
        // CsvComposite: the ;-joined columns pass through verbatim.
        let root = tdir("composite-mv");
        write_fixture(&root, false, true, false);
        let out = root.join("out");
        convert(&root, &out).expect("convert");
        let nodes = read_jsonl_values(&out.join("nodes.jsonl"));
        let p0 = node(&nodes, "p:0");
        assert_eq!(prop(p0, "language"), Value::Str("si;en".into()));
        assert_eq!(prop(p0, "email"), Value::Str("a@x.com;b@y.com".into()));
        // Empty optional fields are omitted, not written as "".
        let p1 = node(&nodes, "p:1");
        assert!(!has_prop(p1, "language"));
        assert!(!has_prop(p1, "email"));

        // CsvBasic: side files aggregate back into one ;-joined string —
        // and this fixture is ALSO nested (social_network/{static,dynamic})
        // with unnumbered file names, exercising discovery tolerance.
        let root_b = tdir("basic-mv");
        write_fixture(&root_b, true, false, true);
        let out_b = root_b.join("out");
        convert(&root_b, &out_b).expect("convert basic");
        let nodes_b = read_jsonl_values(&out_b.join("nodes.jsonl"));
        let b0 = node(&nodes_b, "p:0");
        assert_eq!(prop(b0, "email"), Value::Str("a@x.com;b@y.com".into()));
        assert_eq!(prop(b0, "language"), Value::Str("si;en".into()));
        let b1 = node(&nodes_b, "p:1");
        assert!(!has_prop(b1, "email"));
        assert!(!has_prop(b1, "language"));
    }

    #[test]
    fn refuses_loudly_listing_what_was_found() {
        let root = tdir("refuse");
        std::fs::write(root.join("forum_0_0.csv"), "id|title|creationDate\n").unwrap();
        let err = convert(&root, &root.join("out")).unwrap_err();
        assert!(err.contains("missing required"), "{err}");
        assert!(err.contains("person"), "{err}");
        // The listing of what WAS found.
        assert!(err.contains("forum_0_0.csv"), "{err}");
    }

    #[test]
    fn output_satisfies_the_snbload_contract() {
        let root = tdir("contract");
        write_fixture(&root, false, true, false);
        let out = root.join("out");
        convert(&root, &out).expect("convert");
        let nodes = read_jsonl_values(&out.join("nodes.jsonl"));
        let rels_v = read_jsonl_values(&out.join("rels.jsonl"));

        let mut ids: BTreeSet<String> = BTreeSet::new();
        for n in &nodes {
            let m = as_map(n);
            let Some(Value::Str(i)) = m.get("i") else { panic!("'i' is not a string") };
            assert!(ids.insert(i.clone()), "duplicate corpus id {i}");
            let ls = labels(m);
            assert!(!ls.is_empty(), "label-less node {i} — snbload would skip its rels");
            for l in &ls {
                assert!(ident_ok(l), "label {l:?} is not a bare identifier");
            }
            let Some(Value::Map(p)) = m.get("p") else { panic!("no props") };
            assert!(!p.contains_key("gid"), "'gid' is reserved by snbload");
            // The structured-id contract snbload's relationship pass parses:
            // `<prefix>:<n>` with `n` the node's dense `id`, canonically spelled.
            let (prefix, digits) = i.rsplit_once(':').unwrap_or_else(|| panic!("{i}: no ':'"));
            assert!(!prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_alphabetic()), "{i}");
            assert!(!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()), "{i}");
            assert!(digits.len() == 1 || !digits.starts_with('0'), "{i}: non-canonical id");
            assert_eq!(prop(m, "id"), Value::Int(digits.parse::<i64>().unwrap()), "{i}");
            for (k, v) in p {
                assert!(ident_ok(k), "property key {k:?} would panic snbload");
                let mut unloadable = 0usize;
                let u = engram_bench::untag_prop(v, &mut unloadable);
                assert_eq!(unloadable, 0);
                assert!(
                    matches!(
                        u,
                        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_) | Value::Null
                    ),
                    "prop {k}={u:?} would panic snbload's renderer"
                );
            }
        }
        for r in &rels_v {
            let m = as_map(r);
            let (Some(Value::Str(s)), Some(Value::Str(d)), Some(Value::Str(t))) =
                (m.get("s"), m.get("d"), m.get("t"))
            else {
                panic!("rel record with non-string s/d/t: {m:?}")
            };
            assert!(ids.contains(s), "rel endpoint {s} not in nodes.jsonl");
            assert!(ids.contains(d), "rel endpoint {d} not in nodes.jsonl");
            assert!(ident_ok(t), "rel type {t:?} is not a bare identifier");
        }

        // The dense-id invariant stress.rs relies on: Person ids are exactly
        // 0..persons and Message ids exactly 0..messages.
        let dense_of = |label: &str| -> Vec<i64> {
            let mut v: Vec<i64> = nodes
                .iter()
                .map(as_map)
                .filter(|m| labels(m).iter().any(|l| l == label))
                .map(|m| match prop(m, "id") {
                    Value::Int(n) => n,
                    other => panic!("non-int id {other:?}"),
                })
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(dense_of("Person"), vec![0, 1, 2]);
        assert_eq!(dense_of("Message"), vec![0, 1, 2]);
    }
}
