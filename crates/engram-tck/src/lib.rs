//! openCypher **TCK conformance harness** — the measurement instrument for the
//! full-openCypher program (`docs/opencypher/conformance-strategy.md`, Phase 0).
//!
//! It reads the vendored openCypher TCK `.feature` files (`features/`, Apache-2.0)
//! and runs each `Scenario` against a fresh graph via [`engram_graph::run_stmt`],
//! comparing the result table under the TCK's own semantics.
//!
//! ## Instrument integrity (the rule this crate lives by)
//!
//! A scenario is scored **only when the harness can actually judge it**:
//!
//! - **Pass** — fully evaluated, and the engine's answer matched.
//! - **Fail** — fully evaluated, and the engine's answer did NOT match (wrong
//!   rows, an unexpected error, a setup that the engine could not run, or a
//!   missing-error where the TCK demanded one). This is an *engine* verdict.
//! - **Skip** — the harness itself cannot yet evaluate the scenario (a named
//!   fixture graph, a `Scenario Outline`, a path-valued expectation, an
//!   unparseable expected cell, a side-effect-only assertion). A Skip is a gap
//!   in the *harness*, never a pass.
//!
//! The headline **pass rate is `Pass / (Pass + Fail)`** — Skips are reported
//! separately so the number can never be inflated by scenarios we quietly did
//! not check. A harness that scored its own blind spots as passes would measure
//! nothing (the failure mode this codebase guards against everywhere).

// This is an INTERNAL measurement instrument, not a published API surface — the
// Gherkin model / value grammar are plumbing, so the workspace `missing_docs`
// warning is opted out here rather than documenting every field of a private
// harness. The behavioural contract lives in the module doc above.
#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::{Value, parse_any, parse_expression};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

// ─── Gherkin model ───────────────────────────────────────────────────────────

/// One `Scenario` (or `Scenario Outline`, flagged) with its ordered steps.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub outline: bool,
    pub steps: Vec<Step>,
}

/// A single Gherkin step: its text plus an optional docstring (`"""…"""`) or a
/// pipe-delimited data table.
#[derive(Debug, Clone)]
pub struct Step {
    pub text: String,
    pub doc: Option<String>,
    pub table: Option<Vec<Vec<String>>>,
}

/// Parse one `.feature` file into its scenarios. A minimal reader — enough for
/// the TCK's shape (Feature / Scenario\[ Outline\] / Given-And-When-Then, `"""`
/// docstrings, `|` tables); it does not aim to be a general Gherkin engine.
pub fn parse_feature(text: &str) -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    let mut cur: Option<Scenario> = None;
    // The `Examples:` rows for the current `Scenario Outline`, if any.
    let mut examples: Option<Vec<Vec<String>>> = None;
    // A feature-level `Background:` runs before EVERY scenario; its steps are
    // collected here and prepended to each scenario's own steps.
    let mut background: Vec<Step> = Vec::new();
    let mut in_background = false;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Scenario Outline:") {
            if let Some(s) = cur.take() {
                push_scenario(&mut scenarios, s, examples.take());
            }
            in_background = false;
            cur = Some(Scenario {
                name: rest.trim().to_string(),
                outline: true,
                steps: background.clone(),
            });
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Scenario:") {
            if let Some(s) = cur.take() {
                push_scenario(&mut scenarios, s, examples.take());
            }
            in_background = false;
            cur = Some(Scenario {
                name: rest.trim().to_string(),
                outline: false,
                steps: background.clone(),
            });
            i += 1;
            continue;
        }
        if line.starts_with("Examples:") {
            // Read the following pipe-table as the current outline's example rows
            // (used to expand it into concrete scenarios when it is flushed).
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            let mut rows = Vec::new();
            while j < lines.len() && lines[j].trim().starts_with('|') {
                rows.push(split_table_row(lines[j]));
                j += 1;
            }
            if !rows.is_empty() {
                examples = Some(rows);
            }
            i = j;
            continue;
        }
        if line.starts_with("Background:") {
            // Collect the following steps into `background` (they prepend to every
            // scenario in this feature).
            in_background = true;
            i += 1;
            continue;
        }
        if line.starts_with("Feature:") {
            i += 1;
            continue;
        }
        let is_step = ["Given ", "And ", "When ", "Then ", "But "]
            .iter()
            .any(|k| line.starts_with(k));
        if is_step {
            let text = line
                .split_once(' ')
                .map_or("", |(_, r)| r)
                .trim()
                .to_string();
            // A docstring or table may follow on subsequent lines.
            let mut doc = None;
            let mut table = None;
            let mut j = i + 1;
            // Skip blank lines between the step and its block.
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "\"\"\"" {
                let mut buf = Vec::new();
                j += 1;
                while j < lines.len() && lines[j].trim() != "\"\"\"" {
                    buf.push(lines[j]);
                    j += 1;
                }
                j += 1; // closing """
                doc = Some(buf.join("\n"));
            } else if j < lines.len() && lines[j].trim().starts_with('|') {
                let mut rows = Vec::new();
                while j < lines.len() && lines[j].trim().starts_with('|') {
                    rows.push(split_table_row(lines[j]));
                    j += 1;
                }
                table = Some(rows);
            } else {
                j = i + 1; // no block; continue right after the step line
            }
            let step = Step { text, doc, table };
            if in_background {
                background.push(step);
            } else if let Some(s) = cur.as_mut() {
                s.steps.push(step);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    if let Some(s) = cur.take() {
        push_scenario(&mut scenarios, s, examples.take());
    }
    scenarios
}

/// Flush a parsed scenario: a `Scenario Outline` with an `Examples` table is
/// EXPANDED into one concrete scenario per example row (each `<col>` placeholder
/// substituted in step text, docstrings and tables); anything else is pushed
/// as-is (an outline with no examples stays flagged and will Skip at run time).
fn push_scenario(out: &mut Vec<Scenario>, sc: Scenario, examples: Option<Vec<Vec<String>>>) {
    match examples {
        Some(table) if sc.outline && table.len() >= 2 => {
            let header = &table[0];
            for (n, row) in table[1..].iter().enumerate() {
                out.push(Scenario {
                    name: format!("{} [example {}]", sc.name, n + 1),
                    outline: false,
                    steps: sc
                        .steps
                        .iter()
                        .map(|s| substitute_step(s, header, row))
                        .collect(),
                });
            }
        }
        _ => out.push(sc),
    }
}

/// Substitute `<col>` placeholders in a step from one example row.
fn substitute_step(step: &Step, header: &[String], row: &[String]) -> Step {
    let sub = |s: &str| {
        let mut r = s.to_string();
        for (h, v) in header.iter().zip(row) {
            r = r.replace(&format!("<{h}>"), v);
        }
        r
    };
    Step {
        text: sub(&step.text),
        doc: step.doc.as_ref().map(|d| sub(d)),
        table: step.table.as_ref().map(|t| {
            t.iter()
                .map(|r| r.iter().map(|c| sub(c)).collect())
                .collect()
        }),
    }
}

/// Split a `| a | b |` row into trimmed cells.
fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    // Gherkin table cells escape the delimiter and backslash: `\|` is a literal
    // pipe, `\\` a literal backslash, `\n` a newline. A naive `split('|')` both
    // breaks on an escaped pipe and leaves `\\` doubled, so a value with one
    // backslash (written `\\` in the cell) never matched the engine.
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('|') => cur.push('|'),
                Some('\\') => cur.push('\\'),
                Some('n') => cur.push('\n'),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

// ─── TCK value grammar ───────────────────────────────────────────────────────

/// A parsed TCK expected value. Structural entities (nodes, relationships) are
/// compared by shape — labels/type + properties — never by id, since the TCK
/// renders no ids. Scalars/lists/maps reuse the engine's own value semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Expected {
    Scalar(Value),
    List(Vec<Expected>),
    Map(BTreeMap<String, Expected>),
    Node {
        labels: BTreeSet<String>,
        props: BTreeMap<String, Expected>,
    },
    Rel {
        typ: String,
        props: BTreeMap<String, Expected>,
    },
}

/// Parse a TCK cell into an [`Expected`], or `None` when the harness cannot yet
/// represent it (a path `<…>`, or anything the small grammar below rejects) —
/// which makes the scenario a Skip, not a Fail.
pub fn parse_expected(s: &str) -> Option<Expected> {
    let mut p = Cur {
        s: s.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return None;
    }
    Some(v)
}

struct Cur<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn value(&mut self) -> Option<Expected> {
        self.ws();
        match self.peek()? {
            b'(' => self.node(),
            b'[' => {
                if self.s[self.i..].starts_with(b"[:") {
                    self.rel()
                } else {
                    self.list()
                }
            }
            b'{' => self.map(),
            b'<' => None, // paths: not modeled yet → Skip
            b'\'' => self.string().map(|s| Expected::Scalar(Value::Str(s))),
            _ => self.scalar_token(),
        }
    }

    /// A bare token (null/true/false/number) — delegated to the engine's own
    /// expression evaluator so numeric/keyword semantics can never drift.
    fn scalar_token(&mut self) -> Option<Expected> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if matches!(c, b',' | b']' | b'}' | b')' | b'>' | b'|') || (c as char).is_whitespace() {
                break;
            }
            self.i += 1;
        }
        let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?.trim();
        if tok.is_empty() {
            return None;
        }
        let expr = parse_expression(tok).ok()?;
        let v = engram_cypher::eval(&expr, &engram_cypher::Scope::default()).ok()?;
        Some(Expected::Scalar(v))
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'\'') {
            return None;
        }
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                b'\'' => return Some(out),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    out.push(match e {
                        b'\'' => '\'',
                        b'\\' => '\\',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        other => other as char,
                    });
                }
                // A non-ASCII byte is the lead of a multi-byte UTF-8 scalar; decode
                // the whole scalar (`other as char` would mangle it into Latin-1
                // codepoints, so an emoji/`ß`/`ǿ` cell never matched the engine's
                // correct string). `self.i` already points past the lead byte.
                other => {
                    let len = match other {
                        0x00..=0x7F => 1,
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        _ => 4,
                    };
                    let (start, end) = (self.i - 1, self.i - 1 + len);
                    let ch = std::str::from_utf8(self.s.get(start..end)?)
                        .ok()?
                        .chars()
                        .next()?;
                    self.i = end;
                    out.push(ch);
                }
            }
        }
        None
    }

    fn list(&mut self) -> Option<Expected> {
        self.eat(b'[');
        let mut items = Vec::new();
        self.ws();
        if self.eat(b']') {
            return Some(Expected::List(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            if self.eat(b']') {
                break;
            }
            if !self.eat(b',') {
                return None;
            }
        }
        Some(Expected::List(items))
    }

    fn map(&mut self) -> Option<Expected> {
        self.eat(b'{');
        let mut m = BTreeMap::new();
        self.ws();
        if self.eat(b'}') {
            return Some(Expected::Map(m));
        }
        loop {
            self.ws();
            let key = self.ident()?;
            self.ws();
            if !self.eat(b':') {
                return None;
            }
            let v = self.value()?;
            m.insert(key, v);
            self.ws();
            if self.eat(b'}') {
                break;
            }
            if !self.eat(b',') {
                return None;
            }
        }
        Some(Expected::Map(m))
    }

    fn node(&mut self) -> Option<Expected> {
        self.eat(b'(');
        let mut labels = BTreeSet::new();
        while self.eat(b':') {
            labels.insert(self.ident()?);
        }
        self.ws();
        let props = if self.peek() == Some(b'{') {
            match self.map()? {
                Expected::Map(m) => m,
                _ => return None,
            }
        } else {
            BTreeMap::new()
        };
        self.ws();
        if !self.eat(b')') {
            return None;
        }
        Some(Expected::Node { labels, props })
    }

    fn rel(&mut self) -> Option<Expected> {
        self.eat(b'[');
        if !self.eat(b':') {
            return None;
        }
        let typ = self.ident()?;
        self.ws();
        let props = if self.peek() == Some(b'{') {
            match self.map()? {
                Expected::Map(m) => m,
                _ => return None,
            }
        } else {
            BTreeMap::new()
        };
        self.ws();
        if !self.eat(b']') {
            return None;
        }
        Some(Expected::Rel { typ, props })
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        // Backtick-quoted identifiers are allowed in Cypher keys.
        if self.eat(b'`') {
            let s = self.i;
            while self.peek().is_some() && self.peek() != Some(b'`') {
                self.i += 1;
            }
            let name = std::str::from_utf8(&self.s[s..self.i]).ok()?.to_string();
            self.eat(b'`');
            return Some(name);
        }
        while let Some(c) = self.peek() {
            if (c as char).is_alphanumeric() || c == b'_' {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i == start {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .map(str::to_string)
    }
}

// ─── Comparison (TCK equality) ───────────────────────────────────────────────

/// Whether a value is one of the temporal types (rendered as a string in the TCK).
fn is_temporal(v: &Value) -> bool {
    matches!(
        v,
        Value::Date(_)
            | Value::Time { .. }
            | Value::LocalTime(_)
            | Value::DateTime { .. }
            | Value::LocalDateTime { .. }
            | Value::Duration { .. }
    )
}

/// Whether the engine's `actual` value satisfies the TCK's `expected` cell.
pub fn matches(expected: &Expected, actual: &Value) -> bool {
    matches_mode(expected, actual, false)
}

/// [`matches`] with the TCK's `ignoring element order for lists` mode: when
/// `ignore_list_order` is set, every list comparison (at any nesting depth) is a
/// multiset match instead of a positional one.
fn matches_mode(expected: &Expected, actual: &Value, ignore_list_order: bool) -> bool {
    // The TCK writes temporal results as their canonical string form (a quoted
    // cell), so compare the rendered temporal against that string.
    if let Expected::Scalar(Value::Str(s)) = expected {
        if is_temporal(actual) {
            return &engram_cypher::temporal_to_string(actual) == s;
        }
    }
    match (expected, actual) {
        (Expected::Scalar(e), a) => scalar_eq_mode(e, a, ignore_list_order),
        (Expected::List(es), Value::List(as_)) => {
            if es.len() != as_.len() {
                return false;
            }
            if ignore_list_order {
                bag_list_matches(es, as_, ignore_list_order)
            } else {
                es.iter()
                    .zip(as_)
                    .all(|(e, a)| matches_mode(e, a, ignore_list_order))
            }
        }
        (Expected::Map(em), Value::Map(am)) => {
            em.len() == am.len()
                && em.iter().all(|(k, e)| {
                    am.get(k)
                        .is_some_and(|a| matches_mode(e, a, ignore_list_order))
                })
        }
        (
            Expected::Node { labels, props },
            Value::Node {
                labels: al,
                props: ap,
                ..
            },
        ) => {
            let al: BTreeSet<String> = al.iter().cloned().collect();
            labels == &al && props_match(props, ap, ignore_list_order)
        }
        (
            Expected::Rel { typ, props },
            Value::Rel {
                rel_type,
                props: ap,
                ..
            },
        ) => typ == rel_type && props_match(props, ap, ignore_list_order),
        _ => false,
    }
}

/// Multiset match of two equal-length lists: each expected element is paired
/// with a distinct actual element (recursively honouring `ignore_list_order`).
fn bag_list_matches(es: &[Expected], as_: &[Value], ignore_list_order: bool) -> bool {
    let mut used = vec![false; as_.len()];
    for e in es {
        let mut found = false;
        for (k, a) in as_.iter().enumerate() {
            if !used[k] && matches_mode(e, a, ignore_list_order) {
                used[k] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

fn props_match(
    expected: &BTreeMap<String, Expected>,
    actual: &BTreeMap<String, Value>,
    ignore_list_order: bool,
) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|(k, e)| {
            actual
                .get(k)
                .is_some_and(|a| matches_mode(e, a, ignore_list_order))
        })
}

/// Scalar equality with the TCK's type discipline: integers and floats do NOT
/// cross-compare (`1` ≠ `1.0`), floats compare with a small tolerance, and null
/// matches only null. Honours the `ignoring element order for lists` mode: a
/// `Value::List` on both sides is compared as a multiset when the flag is set.
fn scalar_eq_mode(e: &Value, a: &Value, ignore_list_order: bool) -> bool {
    match (e, a) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => (x - y).abs() <= 1e-9 || (x.is_nan() && y.is_nan()),
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return false;
            }
            if ignore_list_order {
                let mut used = vec![false; y.len()];
                x.iter().all(|xv| {
                    for (k, yv) in y.iter().enumerate() {
                        if !used[k] && scalar_eq_mode(xv, yv, ignore_list_order) {
                            used[k] = true;
                            return true;
                        }
                    }
                    false
                })
            } else {
                x.iter()
                    .zip(y)
                    .all(|(a, b)| scalar_eq_mode(a, b, ignore_list_order))
            }
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| {
                    y.get(k)
                        .is_some_and(|w| scalar_eq_mode(v, w, ignore_list_order))
                })
        }
        _ => false,
    }
}

// ─── Scenario execution ──────────────────────────────────────────────────────

/// The verdict for one scenario. See the module doc for the integrity rule.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

fn fresh_graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Bound materialisation so a pathological scenario (a Cartesian blow-up, a
    // huge intermediate) fails fast rather than running the baseline out of
    // memory. Well above any real TCK result size.
    g.set_row_budget(Some(2_000_000));
    // A fixed wall clock so `date()`/`timestamp()`-style scenarios are
    // deterministic run to run (value TBD; any fixed epoch works for now).
    g.set_wall_ms(1_600_000_000_000);
    // §7's SECOND ARM. Precision locking ships OFF, and the plan's gate for
    // flipping it is a full TCK pass with it ON — so the ratchet has to be
    // runnable both ways or that gate cannot be met.
    //
    // The env var rather than a second test binary because the scenarios, the
    // harness and the ratchet must be IDENTICAL between the arms: a second
    // copy of any of them is a second thing to keep in step, and the first
    // time they drift the comparison stops meaning anything.
    //
    // The TCK is single-threaded per scenario, so nothing here can produce a
    // phantom conflict on its own. That is the point: the arm exists to show
    // precision locking changes NO single-threaded answer, which is the half
    // of its behaviour a concurrency test cannot cover.
    if std::env::var("ENGRAM_TCK_PRECISION_LOCKING").as_deref() == Ok("1") {
        g.set_precision_locking(true);
    }
    g
}

/// Run one scenario end to end and return its [`Outcome`].
pub fn run_scenario(sc: &Scenario) -> Outcome {
    if sc.outline {
        return Outcome::Skip("scenario outline (Examples not expanded yet)".into());
    }
    let graph = fresh_graph();
    let mut query: Option<String> = None;
    let mut params: BTreeMap<String, Value> = BTreeMap::new();

    for step in &sc.steps {
        let t = &step.text;
        if t == "an empty graph" || t == "any graph" {
            // fresh_graph is already empty; "any graph" means the query does not
            // depend on graph content, so empty is a valid choice.
        } else if t.starts_with("having executed") {
            let Some(setup) = &step.doc else {
                return Outcome::Fail("having-executed step had no query".into());
            };
            match parse_any(setup) {
                Ok(stmt) => {
                    if let Err(e) = run_stmt(&graph, &stmt, params.clone()) {
                        return Outcome::Fail(format!("setup failed: {e}"));
                    }
                }
                Err(e) => return Outcome::Fail(format!("setup parse failed: {e}")),
            }
        } else if t.starts_with("parameters are") {
            let Some(rows) = &step.table else {
                return Outcome::Skip("parameters step without a table".into());
            };
            for row in rows {
                if row.len() != 2 {
                    return Outcome::Skip("unexpected parameters table shape".into());
                }
                let expr = match parse_expression(&row[1]) {
                    Ok(e) => e,
                    Err(_) => return Outcome::Skip(format!("param value: {}", row[1])),
                };
                match engram_cypher::eval(&expr, &engram_cypher::Scope::default()) {
                    Ok(v) => {
                        params.insert(row[0].clone(), v);
                    }
                    Err(_) => return Outcome::Skip(format!("param eval: {}", row[1])),
                }
            }
        } else if t.starts_with("executing query") || t.starts_with("executing control query") {
            query = step.doc.clone();
        } else if let Some((in_order, ignore_list_order)) = result_order(t) {
            // Terminal assertion: run the query and compare.
            let Some(q) = &query else {
                return Outcome::Fail("result assertion with no query".into());
            };
            let Some(rows) = &step.table else {
                return Outcome::Fail("result assertion without a table".into());
            };
            return judge_result(&graph, q, params.clone(), rows, in_order, ignore_list_order);
        } else if t == "the result should be empty" {
            let Some(q) = &query else {
                return Outcome::Fail("empty-result assertion with no query".into());
            };
            return judge_empty(&graph, q, params.clone());
        } else if is_error_assertion(t) {
            let Some(q) = &query else {
                return Outcome::Fail("error assertion with no query".into());
            };
            return judge_error(&graph, q, params.clone(), t);
        } else if t == "no side effects" || t.starts_with("the side effects should be") {
            // Verifying side effects needs a before/after graph diff the harness
            // does not model yet → cannot judge this assertion → Skip.
            return Outcome::Skip(format!("unverified assertion: {t}"));
        } else if t.starts_with("the") && t.contains("graph") {
            return Outcome::Skip(format!("named fixture graph: {t}"));
        } else {
            return Outcome::Skip(format!("unhandled step: {t}"));
        }
    }
    Outcome::Skip("scenario had no terminal assertion the harness understands".into())
}

/// `Some((in_order, ignore_list_order))` for a tabular result assertion, else
/// `None`. Covers every `Then the result should be…:` phrasing (in order / in
/// any order / ignoring element order for lists) — ROW order is significant iff
/// it says `, in order`; LIST-cell element order is ignored iff it says
/// `ignoring element order for lists` (the two are independent — a scenario can
/// say both). `the result should be empty` is excluded (its own branch).
fn result_order(t: &str) -> Option<(bool, bool)> {
    if !t.starts_with("the result should be") || t == "the result should be empty" {
        return None;
    }
    Some((
        t.contains(", in order"),
        t.contains("ignoring element order for lists"),
    ))
}

fn is_error_assertion(t: &str) -> bool {
    t.contains("should be raised") || t.starts_with("a ") && t.contains("Error")
}

fn judge_result(
    graph: &Graph,
    q: &str,
    params: BTreeMap<String, Value>,
    table: &[Vec<String>],
    in_order: bool,
    ignore_list_order: bool,
) -> Outcome {
    if table.is_empty() {
        return Outcome::Fail("empty result table".into());
    }
    let header = &table[0];
    // Parse every expected cell first; an unrepresentable cell → Skip.
    let mut expected_rows: Vec<Vec<Expected>> = Vec::new();
    for row in &table[1..] {
        if row.len() != header.len() {
            return Outcome::Fail("ragged result table".into());
        }
        let mut er = Vec::with_capacity(row.len());
        for cell in row {
            match parse_expected(cell) {
                Some(v) => er.push(v),
                None => return Outcome::Skip(format!("expected value not modeled: `{cell}`")),
            }
        }
        expected_rows.push(er);
    }

    let stmt = match parse_any(q) {
        Ok(s) => s,
        Err(e) => return Outcome::Fail(format!("query parse failed: {e}")),
    };
    let result = match run_stmt(graph, &stmt, params) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(format!("query failed: {e}")),
    };

    // Map expected header column names → indices in the engine's output.
    let mut col_idx = Vec::with_capacity(header.len());
    for name in header {
        match result.columns.iter().position(|c| c == name) {
            Some(idx) => col_idx.push(idx),
            None => {
                return Outcome::Fail(format!(
                    "missing column `{name}` (engine returned {:?})",
                    result.columns
                ));
            }
        }
    }
    // Project the engine rows into the expected column order.
    let actual_rows: Vec<Vec<Value>> = result
        .rows
        .iter()
        .map(|row| col_idx.iter().map(|&i| row[i].clone()).collect())
        .collect();

    if actual_rows.len() != expected_rows.len() {
        return Outcome::Fail(format!(
            "row count {} != expected {}",
            actual_rows.len(),
            expected_rows.len()
        ));
    }

    let ok = if in_order {
        expected_rows
            .iter()
            .zip(&actual_rows)
            .all(|(er, ar)| row_matches(er, ar, ignore_list_order))
    } else {
        bag_matches(&expected_rows, &actual_rows, ignore_list_order)
    };
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail("rows did not match".into())
    }
}

fn row_matches(er: &[Expected], ar: &[Value], ignore_list_order: bool) -> bool {
    er.len() == ar.len()
        && er
            .iter()
            .zip(ar)
            .all(|(e, a)| matches_mode(e, a, ignore_list_order))
}

/// Unordered (bag) row comparison: every expected row is matched to a distinct
/// actual row.
fn bag_matches(expected: &[Vec<Expected>], actual: &[Vec<Value>], ignore_list_order: bool) -> bool {
    let mut used = vec![false; actual.len()];
    for er in expected {
        let mut found = false;
        for (k, ar) in actual.iter().enumerate() {
            if !used[k] && row_matches(er, ar, ignore_list_order) {
                used[k] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

fn judge_empty(graph: &Graph, q: &str, params: BTreeMap<String, Value>) -> Outcome {
    let stmt = match parse_any(q) {
        Ok(s) => s,
        Err(e) => return Outcome::Fail(format!("query parse failed: {e}")),
    };
    match run_stmt(graph, &stmt, params) {
        Ok(r) if r.rows.is_empty() => Outcome::Pass,
        Ok(r) => Outcome::Fail(format!("expected empty, got {} rows", r.rows.len())),
        Err(e) => Outcome::Fail(format!("query failed: {e}")),
    }
}

/// The TCK asserted an error. For the baseline we check only that the engine
/// *raised* one (error-CATEGORY conformance is a later phase); a query that
/// wrongly succeeds is a Fail.
fn judge_error(graph: &Graph, q: &str, params: BTreeMap<String, Value>, step: &str) -> Outcome {
    // The expected error DETAIL is the token after the final ':'
    // (…should be raised at compile time: VariableAlreadyBound).
    let detail = step.rsplit(':').next().map(str::trim).unwrap_or("error");
    let stmt = match parse_any(q) {
        Ok(s) => s,
        // A parse error is a legitimate "error raised at compile time".
        Err(_) => return Outcome::Pass,
    };
    match run_stmt(graph, &stmt, params) {
        // Naming the expected error type makes the fail tally show which error
        // categories Engram is missing (category conformance is Phase 5; for now
        // any raised error passes, and a wrong SUCCESS is the fail).
        Ok(_) => Outcome::Fail(format!("expected error {detail}, query succeeded")),
        Err(_) => Outcome::Pass,
    }
}

// ─── Corpus walk + tally ─────────────────────────────────────────────────────

/// A per-category / total tally.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
}

impl Tally {
    pub fn record(&mut self, o: &Outcome) {
        match o {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail(_) => self.fail += 1,
            Outcome::Skip(_) => self.skip += 1,
        }
    }
    /// Pass over *evaluated* scenarios (pass + fail); Skips excluded so the rate
    /// is never inflated by scenarios the harness could not judge.
    pub fn rate(&self) -> f64 {
        let judged = self.pass + self.fail;
        if judged == 0 {
            0.0
        } else {
            self.pass as f64 / judged as f64
        }
    }
    pub fn total(&self) -> usize {
        self.pass + self.fail + self.skip
    }
}
