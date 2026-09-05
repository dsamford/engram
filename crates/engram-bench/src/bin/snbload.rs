//! `snbload <corpus dir> <bolt addr> [--match-on id|gid] [--neo4j]` — load an
//! SNB corpus into ANY Bolt server.
//!
//! # Why this exists when `load_export` already loads a corpus
//!
//! `load_export` loads in-process, through `Graph`, and therefore only into
//! this engine. Comparing against another database needs a loader that speaks
//! the wire, and — more importantly — comparing FAIRLY needs BOTH sides loaded
//! the same way.
//!
//! Two engines loaded by two different paths differ by more than their engines:
//! the corpora can end up with different property types, different label sets,
//! a synthetic index one side has and the other does not. Every one of those
//! shows up later as a performance difference and gets attributed to the query
//! engine. Loading both through this one binary removes the load path and the
//! data shape as variables, leaving the thing actually under test.
//!
//! # Inlined literals, not parameters
//!
//! The Bolt client here sends an empty parameter map — it was built for a
//! throughput harness that issues fixed statements. Rather than grow a
//! parameter codec for a loader, values are escaped and inlined. That is fine
//! for a loader and would not be fine for the measured workload, which is why
//! the measured workload does not do it.
//!
//! # The `gid` property, and what the relationship pass keys on
//!
//! The corpus identifies nodes by string ids (`p:412`, `m:9001`) and its
//! relationship file refers to them. Every node is written with that id as a
//! `gid` property, on BOTH engines: it is what makes a node addressable across
//! engines after the load, and removing it from millions of nodes costs more
//! than it is worth — while leaving it on only one side would be exactly the
//! asymmetry this binary exists to avoid.
//!
//! The relationship pass does NOT match on it, by default. Matching on the
//! string needs a string index per label on the server (131 B/node against
//! 83 for an integer one) and, in the loader, a gid→label map for every node
//! (~140 B/node) plus every endpoint pair as two Strings (~80 B/pair) — at
//! SF3 (9.9M nodes / 54M rels) that loader does not fit beside the server on
//! a 40 GiB pod. Both corpus generators (`snbgen`, `datagen2jsonl`) guarantee
//! a dense integer `id` property per entity family and spell the corpus id as
//! `<prefix>:<id>`, so an endpoint's (label, id) is parsed from the id string
//! itself and the lookup index is an INTEGER index. The loader holds no
//! per-node map: one bitset per prefix (which ids exist) and one per label
//! (is `id` unique under it) — a few bits per node — decide every endpoint,
//! and a pair is two `u32`s. A node whose corpus id does not spell its `id`
//! falls back to a small per-node map, so an unstructured corpus still loads;
//! a node with no usable `id` is refused, loudly, because silently matching on
//! something else would load a graph that answers every traversal short.
//!
//! The two partitions — parsed ids and the fallback map — are cross-checked:
//! one corpus id string must name ONE node. A fallback entry spelled `p:0`
//! (its `id` is something else) beside a structured `p:0` would otherwise
//! shadow it, and every relationship naming `p:0` would bind to the shadow
//! with exit 0. That is a refusal naming the gid and both ids.
//!
//! Which label to key on is learned from the data, not from the prefix: `m:`
//! is shared by `:Message:Post` and `:Message:Comment` (one dense counter, so
//! keyed on `Message`); `cont:`/`country:`/`city:` are each dense from 0, so
//! `:Place` is ambiguous and they key on their specific label. The rule: the
//! most specific label common to every node of the prefix under which `id`
//! never repeats.
//!
//! `--match-on gid` keeps the string path, for a corpus that has no dense ids.
//!
//! # Refuse before the first statement
//!
//! Nodes are streamed, so a refusal raised while streaming would leave the
//! target holding part of the corpus, with nothing but the exit code to say
//! so. The node file is therefore read TWICE: a pre-flight pass that parses
//! every record, checks every id, label and property, and resolves the keying
//! — sending nothing — and then the pass that sends. Everything a corpus can
//! trip is tripped while the target is still untouched; the price is one
//! extra parse of `nodes.jsonl`, which is a small fraction of the load. The
//! failures that remain late are the server's own (a refused statement), and
//! those say what the target now holds.
//!
//! # Indexes exist only for the relationship pass, and are declared up front
//!
//! One lookup index per key label is declared BEFORE the relationship file is
//! read and dropped right after the last group that needs it — the groups are
//! ordered so the heaviest label's groups run back to back, keeping its index
//! alive for the shortest span. Up front rather than just-in-time because on
//! Neo4j (this loader's other target) population is asynchronous and a
//! POPULATING index is not used by the planner: an index created right before
//! its first batch degrades that label's first batches to label scans, on
//! exactly the heaviest label. Declaring them before the relationship read
//! lets the population overlap the parse, and `CALL db.awaitIndexes()` is
//! sent before the first MATCH — to Neo4j only, identified by its HELLO agent
//! (or `--neo4j`); on this engine a CREATE INDEX is a schema row, there is
//! nothing to await, and the procedure would be refused. Leaving an index
//! behind would put one in the measured configuration that no benchmark
//! asked for, and the write profiles would then be paying to maintain it.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use engram_bolt::client::Client;
use engram_cypher::Value;

/// Rows per statement. Large enough that the round trip is amortised, small
/// enough that one statement stays inside any sane message cap.
const BATCH: usize = 500;

/// Dense ids are `0..N` per family. The presence bitsets are sized by the
/// largest id, so an id past this is not dense and is refused rather than
/// sized for.
const MAX_DENSE_ID: i64 = u32::MAX as i64;

/// `db.awaitIndexes` defaults to 300 s and raises when population is still
/// running at the deadline — which at SF3 it is, on the `Message` index. A
/// raised await is a late failure over a fully streamed node set, so the
/// deadline is set past any population this loader will see.
const NEO4J_AWAIT_INDEXES_SECS: u64 = 86_400;

/// Escape a string for a single-quoted Cypher literal.
fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Whether `render` can spell this value. Nothing else appears in an SNB
/// corpus; checked in the pre-flight pass so that a corpus carrying one is
/// refused before anything is sent rather than mid-stream.
fn renderable(v: &Value) -> bool {
    matches!(
        v,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_) | Value::Null
    )
}

/// Render one property value as a Cypher literal.
fn render(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => {
            // A float literal that renders as an integer parses back as one.
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => lit(s),
        Value::Null => "null".into(),
        // Unreachable after `check_props`; a panic here means the two
        // disagree, and a loud stop still beats a node with a silently
        // missing property.
        other => panic!("snbload: unsupported property value {other:?}"),
    }
}

/// Property KEYS are identifiers, not literals. The corpus generators emit
/// plain alphanumerics; anything else is refused rather than escaped into
/// something that parses differently on the two engines.
fn bare_identifier(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Everything `props_literal` would panic on, as a pre-flight refusal.
fn check_props(p: &BTreeMap<String, Value>) -> Result<(), String> {
    for (k, v) in p {
        if k == "gid" {
            return Err("carries a 'gid' property, which is reserved for the corpus id".into());
        }
        if !bare_identifier(k) {
            return Err(format!("property key {k:?} is not a bare identifier"));
        }
        if !renderable(v) {
            return Err(format!(
                "property {k} = {v:?} is not a value this loader can inline"
            ));
        }
    }
    Ok(())
}

fn props_literal(p: &BTreeMap<String, Value>, gid: &str) -> String {
    let mut parts = Vec::with_capacity(p.len() + 1);
    parts.push(format!("gid: {}", lit(gid)));
    for (k, v) in p {
        if !bare_identifier(k) {
            panic!("snbload: property key {k:?} is not a bare identifier");
        }
        parts.push(format!("{k}: {}", render(v)));
    }
    parts.join(", ")
}

/// A refusal raised BEFORE any statement was sent. Every corpus-triggered
/// refusal goes through here; the pre-flight pass is what makes that true.
fn refuse(why: &str) -> ! {
    eprintln!("[snbload] REFUSING: {why}\n  nothing was sent — the target is untouched");
    std::process::exit(1);
}

/// Where statements go: a server, or — under `SNBLOAD_DUMP` — a plan file.
///
/// The dump exists so the in-process attribution driver replays THIS loader's
/// own plan (its keying, its grouping, its group order, its index lifetimes)
/// rather than a second implementation of it that could drift. Nothing is
/// sent in dump mode and no server is contacted.
enum Sink {
    Bolt(Box<Client>),
    Dump(std::io::BufWriter<std::fs::File>),
}

/// The one connection, counting acknowledged statements so that a failure
/// can say what the target now holds instead of leaving a partial load that
/// only the exit code hints at.
struct Conn {
    sink: Sink,
    acked: u64,
}

impl Conn {
    fn dumping(&self) -> bool {
        matches!(self.sink, Sink::Dump(_))
    }

    /// Write one plan line, or exit — a truncated plan would be replayed as a
    /// smaller load and read as a faster one.
    fn line(&mut self, s: &str) {
        use std::io::Write;
        let Sink::Dump(w) = &mut self.sink else {
            return;
        };
        if let Err(e) = writeln!(w, "{s}") {
            eprintln!("[snbload] writing the plan dump failed: {e}");
            std::process::exit(1);
        }
    }

    fn run(&mut self, stmt: &str, what: &str) {
        match &mut self.sink {
            Sink::Dump(_) => {
                let line = format!("S\t{stmt}");
                self.line(&line);
            }
            Sink::Bolt(c) => {
                if let Err(e) = c.run(stmt) {
                    let head: String = stmt.chars().take(160).collect();
                    eprintln!(
                        "[snbload] {what} failed: {e}\n  statement began: {head}\n  \
                         the target now holds a PARTIAL load — {} statement(s) were acknowledged \
                         before this one; drop it before loading again",
                        self.acked
                    );
                    std::process::exit(1);
                }
            }
        }
        self.acked += 1;
    }
}

/// Neo4j announces itself as `Neo4j/<version>` in HELLO's `server`; this
/// engine as `engram/<version>`. The one behaviour that hangs on it is the
/// index await, which Neo4j needs and this engine refuses.
fn is_neo4j(agent: &str) -> bool {
    agent.starts_with("Neo4j/")
}

// ── Structured corpus ids ───────────────────────────────────────────────────

/// `<prefix>:<n>` with `n` a canonical non-negative decimal — no sign, no
/// leading zero — so that exactly one string spells each (prefix, n) and a
/// relationship's endpoint string can be parsed instead of looked up.
/// Anything else is unstructured (the fallback map handles it).
fn parse_structured(gid: &str) -> Option<(&str, i64)> {
    let (prefix, digits) = gid.rsplit_once(':')?;
    if prefix.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some((prefix, n))
}

/// A set of dense ids, one bit each.
#[derive(Default)]
struct IdSet {
    words: Vec<u64>,
    len: u64,
}

impl IdSet {
    /// Insert; `false` when the id was already present.
    fn insert(&mut self, id: u32) -> bool {
        let (w, b) = ((id / 64) as usize, id % 64);
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let was = self.words[w] & (1u64 << b) != 0;
        self.words[w] |= 1u64 << b;
        if !was {
            self.len += 1;
        }
        !was
    }

    fn contains(&self, id: u32) -> bool {
        let (w, b) = ((id / 64) as usize, id % 64);
        self.words.get(w).is_some_and(|x| x & (1u64 << b) != 0)
    }
}

/// The most specific label (last in the list) under which `id` never
/// repeated — `None` when every candidate has a duplicate, in which case no
/// `MATCH (n:L {id: k})` can name one node.
fn pick_key_label<'a>(candidates: &'a [String], dups: &BTreeMap<String, u64>) -> Option<&'a str> {
    candidates
        .iter()
        .rev()
        .find(|l| dups.get(l.as_str()).copied().unwrap_or(0) == 0)
        .map(String::as_str)
}

/// What the node pass learned that the relationship pass needs, in the
/// dense-id mode: a few bits per node, no per-node strings.
struct NodePass {
    /// Per label: the ids seen under it and how many repeated.
    by_label: BTreeMap<String, (IdSet, u64)>,
    /// Per prefix: the labels EVERY node with that prefix carries (in the
    /// first node's order), and which ids exist.
    prefixes: BTreeMap<String, (Vec<String>, IdSet)>,
    /// Interned label lists, for the fallback entries.
    label_lists: Vec<Vec<String>>,
    /// Nodes whose corpus id does not spell their `id`: gid → (list, id).
    fallback: BTreeMap<String, (u32, u32)>,
}

/// The resolved keying: prefix → (key label, present ids), plus the
/// fallback entries with their key label chosen.
struct Keyed {
    prefixes: BTreeMap<String, (String, IdSet)>,
    fallback: BTreeMap<String, (String, u32)>,
    /// Distinct nodes per key label — the weight that orders the groups.
    weight: BTreeMap<String, u64>,
}

impl NodePass {
    fn new() -> Self {
        NodePass {
            by_label: BTreeMap::new(),
            prefixes: BTreeMap::new(),
            label_lists: Vec::new(),
            fallback: BTreeMap::new(),
        }
    }

    /// Record one node. `id` has already been checked to be a dense id.
    ///
    /// A corpus id string must name ONE node across both partitions. The
    /// same string can reach both: `p:0` spelling id 0 is structured, while
    /// a second `p:0` whose id is 120 is not (it does not spell it) and would
    /// land in the fallback map — which `resolve` consults first, so every
    /// relationship naming `p:0` would bind to the shadow node and the load
    /// would exit 0. Either arrival order is refused, naming both ids.
    fn observe(&mut self, gid: &str, labels: &[String], id: u32) -> Result<(), String> {
        for l in labels {
            let (set, dups) = self.by_label.entry(l.clone()).or_default();
            if !set.insert(id) {
                *dups += 1;
            }
        }
        match parse_structured(gid) {
            Some((prefix, n)) if n == i64::from(id) => {
                if let Some((_, other)) = self.fallback.get(gid) {
                    return Err(shadow(gid, id, *other));
                }
                let entry = self
                    .prefixes
                    .entry(prefix.to_string())
                    .or_insert_with(|| (labels.to_vec(), IdSet::default()));
                entry.0.retain(|l| labels.contains(l));
                if !entry.1.insert(id) {
                    return Err(format!("duplicate corpus id {gid:?}"));
                }
            }
            parsed => {
                // Unstructured, or structured but spelling some OTHER id: in
                // the latter case the spelled (prefix, n) may already be a
                // structured node, and this one would shadow it.
                if let Some((prefix, n)) = parsed {
                    let spelled = u32::try_from(n).ok();
                    let taken = spelled.is_some_and(|n| {
                        self.prefixes
                            .get(prefix)
                            .is_some_and(|(_, ids)| ids.contains(n))
                    });
                    if taken {
                        return Err(shadow(gid, spelled.expect("checked"), id));
                    }
                }
                let idx = match self.label_lists.iter().position(|l| l == labels) {
                    Some(i) => i,
                    None => {
                        self.label_lists.push(labels.to_vec());
                        self.label_lists.len() - 1
                    }
                };
                if self
                    .fallback
                    .insert(gid.to_string(), (idx as u32, id))
                    .is_some()
                {
                    return Err(format!("duplicate corpus id {gid:?}"));
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Keyed, String> {
        let dups: BTreeMap<String, u64> = self
            .by_label
            .iter()
            .map(|(l, (_, d))| (l.clone(), *d))
            .collect();
        let describe = |candidates: &[String]| -> String {
            candidates
                .iter()
                .map(|l| {
                    format!(
                        "{l} ({} duplicate id(s))",
                        dups.get(l).copied().unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut prefixes = BTreeMap::new();
        for (prefix, (common, ids)) in self.prefixes {
            let Some(label) = pick_key_label(&common, &dups) else {
                return Err(format!(
                    "prefix {prefix:?}: no label common to its {} node(s) has a unique 'id' \
                     — candidates: {}",
                    ids.len,
                    describe(&common)
                ));
            };
            prefixes.insert(prefix, (label.to_string(), ids));
        }
        let mut fallback = BTreeMap::new();
        for (gid, (idx, id)) in self.fallback {
            let list = &self.label_lists[idx as usize];
            let Some(label) = pick_key_label(list, &dups) else {
                return Err(format!(
                    "node {gid:?}: none of its labels has a unique 'id' — candidates: {}",
                    describe(list)
                ));
            };
            fallback.insert(gid, (label.to_string(), id));
        }
        let mut weight: BTreeMap<String, u64> = BTreeMap::new();
        for (label, ids) in prefixes.values() {
            *weight.entry(label.clone()).or_default() += ids.len;
        }
        for (label, _) in fallback.values() {
            *weight.entry(label.clone()).or_default() += 1;
        }
        Ok(Keyed {
            prefixes,
            fallback,
            weight,
        })
    }
}

/// The refusal for one corpus id naming two nodes: the one that spells its
/// `id` and the one that does not.
fn shadow(gid: &str, structured_id: u32, fallback_id: u32) -> String {
    format!(
        "corpus id {gid:?} names TWO nodes: id {structured_id} (the string spells it) and \
         id {fallback_id} (it does not) — every relationship naming {gid:?} would bind to \
         one of them silently"
    )
}

impl Keyed {
    /// The (key label, id) a relationship endpoint matches on, or `None`
    /// when no node of the corpus has that id.
    fn resolve(&self, gid: &str) -> Option<(&str, u32)> {
        if let Some((label, id)) = self.fallback.get(gid) {
            return Some((label, *id));
        }
        let (prefix, n) = parse_structured(gid)?;
        let id = u32::try_from(n).ok()?;
        let (label, ids) = self.prefixes.get(prefix)?;
        ids.contains(id).then_some((label.as_str(), id))
    }
}

// ── Relationship groups ─────────────────────────────────────────────────────

/// (type, source label, destination label) — one statement shape each.
type GroupKey = (String, String, String);

/// Order the groups so that the heaviest label's groups run back to back:
/// its index — the one that costs — then lives for the shortest span. Ties
/// and the order inside a cluster are the key's own order, so the sequence
/// is deterministic for a given corpus.
fn order_groups(
    keys: impl IntoIterator<Item = GroupKey>,
    weight: &BTreeMap<String, u64>,
) -> Vec<GroupKey> {
    let w = |l: &str| weight.get(l).copied().unwrap_or(0);
    let mut v: Vec<(u64, u64, GroupKey)> = keys
        .into_iter()
        .map(|k| {
            let (a, b) = (w(&k.1), w(&k.2));
            (a.max(b), a.min(b), k)
        })
        .collect();
    v.sort_by(|x, y| y.0.cmp(&x.0).then(y.1.cmp(&x.1)).then(x.2.cmp(&y.2)));
    v.into_iter().map(|(_, _, k)| k).collect()
}

/// For each label, the index of the LAST group whose endpoints name it —
/// the point after which its lookup index can go.
fn last_use(ordered: &[GroupKey]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (i, (_, s, d)) in ordered.iter().enumerate() {
        out.insert(s.clone(), i);
        out.insert(d.clone(), i);
    }
    out
}

/// The endpoint pairs of one group, in the mode's key type.
enum Pairs {
    Ids(Vec<(u32, u32)>),
    Gids(Vec<(String, String)>),
}

impl Pairs {
    fn len(&self) -> usize {
        match self {
            Pairs::Ids(v) => v.len(),
            Pairs::Gids(v) => v.len(),
        }
    }

    /// One inlined `[[s, d], ...]` list literal per chunk of `BATCH` pairs.
    fn lists(&self) -> Vec<String> {
        match self {
            Pairs::Ids(v) => v
                .chunks(BATCH)
                .map(|c| {
                    c.iter()
                        .map(|(s, d)| format!("[{s}, {d}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .collect(),
            Pairs::Gids(v) => v
                .chunks(BATCH)
                .map(|c| {
                    c.iter()
                        .map(|(s, d)| format!("[{}, {}]", lit(s), lit(d)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .collect(),
        }
    }

    /// Every pair as its own rendered `[s, d]` element — the pieces `lists`
    /// joins. The attribution driver re-chunks these at other batch sizes, so
    /// that a batch-size sweep measures THIS loader's pairs rather than a
    /// second rendering of them that could drift from it.
    fn elements(&self) -> Vec<String> {
        match self {
            Pairs::Ids(v) => v.iter().map(|(s, d)| format!("[{s}, {d}]")).collect(),
            Pairs::Gids(v) => v
                .iter()
                .map(|(s, d)| format!("[{}, {}]", lit(s), lit(d)))
                .collect(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchOn {
    Id,
    Gid,
}

impl MatchOn {
    fn property(self) -> &'static str {
        match self {
            MatchOn::Id => "id",
            MatchOn::Gid => "gid",
        }
    }
}

/// One `nodes.jsonl` record, decoded the same way by both node passes.
struct NodeRec {
    gid: String,
    labels: Vec<String>,
    props: BTreeMap<String, Value>,
}

fn node_rec(m: &BTreeMap<String, Value>, unloadable: &mut usize) -> NodeRec {
    NodeRec {
        gid: engram_bench::get_str(m, "i"),
        labels: engram_bench::get_list(m, "l")
            .iter()
            .filter_map(|l| match l {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        props: match m.get("p") {
            Some(Value::Map(p)) => p
                .iter()
                .map(|(k, x)| (k.clone(), engram_bench::untag_prop(x, unloadable)))
                .collect(),
            _ => BTreeMap::new(),
        },
    }
}

fn usage() -> ! {
    eprintln!("usage: snbload <corpus dir> <bolt addr> [--match-on id|gid] [--neo4j]");
    eprintln!("  loads an SNB corpus (nodes.jsonl + rels.jsonl) over Bolt.");
    eprintln!("  --match-on id   (default) relationships match on the dense integer 'id'");
    eprintln!("  --match-on gid  relationships match on the corpus id string");
    eprintln!("  --neo4j         await index population before the relationship pass even");
    eprintln!("                  when the server's HELLO agent does not say 'Neo4j/'");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage();
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let addr = args[2].clone();
    let mut match_on = MatchOn::Id;
    let mut neo4j_flag = false;
    let mut i = 3;
    while i < args.len() {
        match (args[i].as_str(), args.get(i + 1).map(String::as_str)) {
            ("--match-on", Some("id")) => {
                match_on = MatchOn::Id;
                i += 2;
            }
            ("--match-on", Some("gid")) => {
                match_on = MatchOn::Gid;
                i += 2;
            }
            ("--neo4j", _) => {
                neo4j_flag = true;
                i += 1;
            }
            (flag, _) => {
                eprintln!("[snbload] unknown or incomplete option {flag:?}");
                usage();
            }
        }
    }

    // `SNBLOAD_DUMP=<path>` writes the plan instead of sending it. No server
    // is contacted, so `addr` is unused and Neo4j's index await — a server
    // behaviour — does not apply.
    let dump = std::env::var("SNBLOAD_DUMP").ok();
    let (sink, neo4j) = match &dump {
        Some(path) => {
            let f = match std::fs::File::create(path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[snbload] cannot write the plan dump {path}: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!("[snbload] PLAN DUMP to {path} — nothing will be sent");
            (
                Sink::Dump(std::io::BufWriter::with_capacity(1 << 20, f)),
                false,
            )
        }
        None => {
            let c = match Client::connect(&addr) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[snbload] cannot reach {addr}: {e}");
                    std::process::exit(1);
                }
            };
            let neo4j = neo4j_flag || is_neo4j(c.server_agent());
            eprintln!(
                "[snbload] server agent {:?}{}",
                c.server_agent(),
                if neo4j {
                    " — Neo4j: index population will be awaited before the relationship pass"
                } else {
                    ""
                }
            );
            (Sink::Bolt(Box::new(c)), neo4j)
        }
    };
    let mut conn = Conn { sink, acked: 0 };
    let t0 = Instant::now();

    // ── Pre-flight: read every node, send nothing ───────────────────────────
    //
    // Everything the corpus can be refused for — a node with no dense id or
    // no label, a property the renderer cannot spell, a corpus id naming two
    // nodes, a prefix with no unique key label — is found here, before the
    // first statement. The keying is resolved here too, so the second pass
    // has nothing left to learn and nothing left to refuse.
    let nodes_path = dir.join("nodes.jsonl");
    let mut pass = NodePass::new();
    let mut gid_label: BTreeMap<String, String> = BTreeMap::new();
    let mut unloadable = 0usize;
    let mut checked = 0u64;
    engram_bench::read_jsonl(&nodes_path, |v| {
        let Value::Map(m) = v else { return };
        let rec = node_rec(&m, &mut unloadable);
        let (gid, labels) = (&rec.gid, &rec.labels);
        if let Err(e) = check_props(&rec.props) {
            refuse(&format!("node {gid:?} (labels {labels:?}) {e}"));
        }
        match match_on {
            MatchOn::Id => {
                let id = match rec.props.get("id") {
                    Some(Value::Int(n)) if (0..=MAX_DENSE_ID).contains(n) => *n as u32,
                    other => refuse(&format!(
                        "node {gid:?} (labels {labels:?}) has no dense integer 'id' \
                         (found {other:?}); the relationship pass is keyed on it — \
                         pass --match-on gid for a corpus without one"
                    )),
                };
                if labels.is_empty() {
                    refuse(&format!(
                        "node {gid:?} carries no label — nothing to match it by"
                    ));
                }
                if let Err(e) = pass.observe(gid, labels, id) {
                    refuse(&e);
                }
            }
            MatchOn::Gid => {
                // The FIRST label is the one relationships match on. Recorded
                // from the data, not inferred from an id prefix — a mapping
                // that would be silently wrong the day the generator changes.
                if let Some(first) = labels.first() {
                    gid_label.insert(gid.clone(), first.clone());
                }
            }
        }
        checked += 1;
    });
    let keyed = match match_on {
        MatchOn::Id => match pass.finish() {
            Ok(k) => Some(k),
            Err(e) => refuse(&e),
        },
        MatchOn::Gid => None,
    };
    if let Some(k) = &keyed {
        let table = k
            .prefixes
            .iter()
            .map(|(p, (l, ids))| format!("{p}:→{l} ({})", ids.len))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[snbload] keyed on 'id': {table}; {} node(s) with an unstructured corpus id",
            k.fallback.len()
        );
    }
    eprintln!(
        "[snbload] pre-flight: {checked} node(s) checked in {:.1}s, nothing refused — loading",
        t0.elapsed().as_secs_f64()
    );

    // ── Nodes, streamed, one statement shape per LABEL SET ─────────────────
    //
    // Cypher cannot parameterise a label, so each distinct label set gets its
    // own statement; an SNB corpus has a handful. Patterns are buffered per
    // label set only up to one statement's worth and sent as they fill, so
    // the loader never holds the corpus.
    let t_nodes = Instant::now();
    let mut pending: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut per_labels: BTreeMap<String, u64> = BTreeMap::new();
    let mut nodes = 0u64;
    let mut discard = 0usize; // counted in the pre-flight pass
    engram_bench::read_jsonl(&nodes_path, |v| {
        let Value::Map(m) = v else { return };
        let rec = node_rec(&m, &mut discard);
        let key = rec.labels.join(":");
        let buf = pending.entry(key.clone()).or_default();
        buf.push(format!(
            "(:{} {{{}}})",
            key,
            props_literal(&rec.props, &rec.gid)
        ));
        *per_labels.entry(key).or_default() += 1;
        if buf.len() == BATCH {
            conn.run(&format!("CREATE {}", buf.join(", ")), "node create");
            buf.clear();
        }
        nodes += 1;
    });
    for (labels, buf) in &pending {
        if !buf.is_empty() {
            conn.run(&format!("CREATE {}", buf.join(", ")), "node create");
        }
        eprintln!("[snbload] :{labels} — {} node(s)", per_labels[labels]);
    }
    drop(pending);
    if nodes != checked {
        // The keying was resolved over the file the pre-flight pass read; a
        // file that changed under the loader would bind relationships by a
        // map that no longer describes the nodes that were sent.
        eprintln!(
            "[snbload] nodes.jsonl changed between the pre-flight pass ({checked} node(s)) and \
             the load ({nodes} node(s)); the target now holds a PARTIAL load — drop it before \
             loading again"
        );
        std::process::exit(1);
    }
    eprintln!(
        "[snbload] {nodes} node(s) in {:.1}s",
        t_nodes.elapsed().as_secs_f64()
    );

    // ── The lookup indexes: every key label, declared before the rel read ──
    //
    // Only labels the relationship pass can name are key labels, and every
    // one of them is declared here, before `rels.jsonl` is parsed: on Neo4j
    // the population then runs while the loader reads. A key label that no
    // relationship turns out to name is dropped as soon as the groups are
    // known, below, without ever being matched on.
    let prop = match_on.property();
    let weight: BTreeMap<String, u64> = match &keyed {
        Some(k) => k.weight.clone(),
        None => {
            let mut w = BTreeMap::new();
            for l in gid_label.values() {
                *w.entry(l.clone()).or_default() += 1;
            }
            w
        }
    };
    let t_index = Instant::now();
    let mut live: BTreeSet<String> = BTreeSet::new();
    for l in weight.keys() {
        conn.run(
            &format!("CREATE INDEX snbload_{prop}_{l} IF NOT EXISTS FOR (n:{l}) ON (n.{prop})"),
            "lookup index",
        );
        live.insert(l.clone());
    }
    let created = live.len();
    eprintln!("[snbload] {created} lookup index(es) declared on '{prop}'");

    // ── Relationships, grouped by TYPE and by the label pair they join ─────
    let t1 = Instant::now();
    let mut groups: BTreeMap<GroupKey, Pairs> = BTreeMap::new();
    let mut rels = 0u64;
    let mut skipped = 0u64;
    engram_bench::read_jsonl(&dir.join("rels.jsonl"), |v| {
        let Value::Map(m) = v else { return };
        let s = engram_bench::get_str(&m, "s");
        let d = engram_bench::get_str(&m, "d");
        let t = engram_bench::get_str(&m, "t");
        // An endpoint whose node was not in nodes.jsonl is counted, never
        // silently dropped: a corpus that loses edges loads fine and answers
        // every traversal short.
        match &keyed {
            Some(k) => {
                let (Some((sl, si)), Some((dl, di))) = (k.resolve(&s), k.resolve(&d)) else {
                    skipped += 1;
                    return;
                };
                match groups
                    .entry((t, sl.to_string(), dl.to_string()))
                    .or_insert_with(|| Pairs::Ids(Vec::new()))
                {
                    Pairs::Ids(v) => v.push((si, di)),
                    Pairs::Gids(_) => unreachable!("id mode holds id pairs"),
                }
            }
            None => {
                let (Some(sl), Some(dl)) = (gid_label.get(&s), gid_label.get(&d)) else {
                    skipped += 1;
                    return;
                };
                match groups
                    .entry((t, sl.clone(), dl.clone()))
                    .or_insert_with(|| Pairs::Gids(Vec::new()))
                {
                    Pairs::Gids(v) => v.push((s, d)),
                    Pairs::Ids(_) => unreachable!("gid mode holds gid pairs"),
                }
            }
        }
        rels += 1;
    });
    drop(gid_label);
    let ordered = order_groups(groups.keys().cloned(), &weight);
    let last = last_use(&ordered);

    // A key label no group names gets no MATCH; its index goes now.
    for l in weight.keys() {
        if !last.contains_key(l) && live.remove(l) {
            conn.run(
                &format!("DROP INDEX snbload_{prop}_{l} IF EXISTS"),
                "drop lookup index",
            );
        }
    }
    if neo4j {
        // Population is asynchronous on Neo4j and a POPULATING index is not
        // used by the planner; without this the first batches of the
        // heaviest label run as label scans. This engine refuses the
        // procedure — its CREATE INDEX is a schema row with nothing to await.
        conn.run(
            &format!("CALL db.awaitIndexes({NEO4J_AWAIT_INDEXES_SECS})"),
            "await index population",
        );
    }
    eprintln!(
        "[snbload] {rels} relationship(s) read; {} index(es) live for the pass, {:.1}s since \
         the indexes were declared{}",
        live.len(),
        t_index.elapsed().as_secs_f64(),
        if neo4j { " (population awaited)" } else { "" }
    );

    // ── The relationship pass, dropping each index after its last group ────
    let mut statements = 0usize;
    for (gi, key) in ordered.iter().enumerate() {
        let (t, sl, dl) = key;
        let pairs = &groups[key];
        if conn.dumping() {
            // The GROUP, not its statements: the driver re-chunks these pairs
            // to sweep batch size. Tab-separated — every rendered element is
            // tab-free (`lit` escapes a tab as `\t`).
            let line = format!(
                "G\t{t}\t{sl}\t{dl}\t{prop}\t{}",
                pairs.elements().join("\t")
            );
            conn.line(&line);
            statements += pairs.len().div_ceil(BATCH);
        } else {
            for list in pairs.lists() {
                // One statement per pair keeps the plan trivial on both engines;
                // batching is by UNWIND over an inlined list of id pairs.
                conn.run(
                    &format!(
                        "UNWIND [{list}] AS pair \
                         MATCH (a:{sl} {{{prop}: pair[0]}}), (b:{dl} {{{prop}: pair[1]}}) \
                         CREATE (a)-[:{t}]->(b)"
                    ),
                    "rel create",
                );
                statements += 1;
            }
        }
        eprintln!(
            "[snbload] {t} {sl}->{dl}: {} rel(s) at {:.1}s",
            pairs.len(),
            t1.elapsed().as_secs_f64()
        );
        for l in [sl, dl] {
            if last[l] == gi && live.remove(l) {
                conn.run(
                    &format!("DROP INDEX snbload_{prop}_{l} IF EXISTS"),
                    "drop lookup index",
                );
            }
        }
    }
    eprintln!(
        "[snbload] {rels} relationship(s) in {:.1}s across {} (type, label-pair) group(s), \
         {statements} statement(s), {created} lookup index(es) created and dropped",
        t1.elapsed().as_secs_f64(),
        groups.len()
    );
    if !live.is_empty() {
        // Cannot happen — every declared label is either unnamed (dropped
        // above) or has a last use — but an index left behind would sit in
        // the measured configuration, so it is said out loud.
        eprintln!("[snbload] WARNING: lookup index(es) left behind on {live:?}");
    }
    if skipped > 0 {
        eprintln!(
            "[snbload] WARNING: {skipped} relationship(s) skipped — an endpoint id was not in nodes.jsonl"
        );
    }
    if unloadable > 0 {
        eprintln!("[snbload] WARNING: {unloadable} property value(s) could not be decoded");
    }

    if let Sink::Dump(w) = &mut conn.sink {
        use std::io::Write;
        // A buffered plan that was never flushed is a plan missing its tail,
        // and the driver would replay a smaller load as a faster one.
        if let Err(e) = w.flush() {
            eprintln!("[snbload] flushing the plan dump failed: {e}");
            std::process::exit(1);
        }
    }

    eprintln!(
        "[snbload] DONE: {nodes} nodes, {rels} rels into {} in {:.1}s ({} statement(s))",
        if dump.is_some() { "the plan dump" } else { &addr },
        t0.elapsed().as_secs_f64(),
        conn.acked
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn structured_ids_parse_in_both_generators_spellings() {
        // snbgen / datagen2jsonl prefixes.
        assert_eq!(parse_structured("p:7"), Some(("p", 7)));
        assert_eq!(parse_structured("m:9001"), Some(("m", 9001)));
        assert_eq!(parse_structured("tag:0"), Some(("tag", 0)));
        assert_eq!(parse_structured("country:12"), Some(("country", 12)));
        // A label-spelled prefix parses the same way.
        assert_eq!(parse_structured("Person:7"), Some(("Person", 7)));
        // The LAST colon splits, so a prefix may itself contain one.
        assert_eq!(parse_structured("a:b:3"), Some(("a:b", 3)));
    }

    #[test]
    fn unstructured_ids_are_not_parsed() {
        for s in [
            "",
            "p",
            "p:",
            ":7",
            "p:-1",
            "p:+1",
            "p:x",
            "p:07",
            "p:1 ",
            "p: 1",
            "p:99999999999999999999",
        ] {
            assert_eq!(parse_structured(s), None, "{s:?} must be unstructured");
        }
        // The one canonical zero.
        assert_eq!(parse_structured("p:0"), Some(("p", 0)));
    }

    #[test]
    fn idset_insert_reports_repeats_and_contains_answers() {
        let mut s = IdSet::default();
        assert!(s.insert(0));
        assert!(s.insert(63));
        assert!(s.insert(64));
        assert!(s.insert(1_000_000));
        assert!(!s.insert(64));
        assert_eq!(s.len, 4);
        assert!(s.contains(0) && s.contains(63) && s.contains(64) && s.contains(1_000_000));
        assert!(!s.contains(1) && !s.contains(65) && !s.contains(1_000_001));
    }

    #[test]
    fn key_label_is_the_most_specific_unique_one() {
        let mut dups = BTreeMap::new();
        dups.insert("Place".to_string(), 12u64);
        let place_city = labels(&["Place", "City"]);
        assert_eq!(pick_key_label(&place_city, &dups), Some("City"));
        let message = labels(&["Message"]);
        assert_eq!(pick_key_label(&message, &dups), Some("Message"));
        dups.insert("City".to_string(), 1);
        assert_eq!(pick_key_label(&place_city, &dups), None);
    }

    #[test]
    fn shared_message_counter_keys_on_message_and_places_on_their_subtype() {
        // The two generators' shape: `m:` is shared by Post and Comment over
        // ONE dense counter; cont/country/city are each dense from 0, so
        // `Place` repeats every id.
        let mut p = NodePass::new();
        p.observe("m:0", &labels(&["Message", "Post"]), 0).unwrap();
        p.observe("m:1", &labels(&["Message", "Post"]), 1).unwrap();
        p.observe("m:2", &labels(&["Message", "Comment"]), 2)
            .unwrap();
        p.observe("cont:0", &labels(&["Place", "Continent"]), 0)
            .unwrap();
        p.observe("country:0", &labels(&["Place", "Country"]), 0)
            .unwrap();
        p.observe("city:0", &labels(&["Place", "City"]), 0).unwrap();
        p.observe("city:1", &labels(&["Place", "City"]), 1).unwrap();
        p.observe("p:0", &labels(&["Person"]), 0).unwrap();
        let k = p.finish().unwrap();
        assert_eq!(k.resolve("m:0"), Some(("Message", 0)));
        assert_eq!(k.resolve("m:2"), Some(("Message", 2)));
        assert_eq!(k.resolve("cont:0"), Some(("Continent", 0)));
        assert_eq!(k.resolve("country:0"), Some(("Country", 0)));
        assert_eq!(k.resolve("city:1"), Some(("City", 1)));
        assert_eq!(k.resolve("p:0"), Some(("Person", 0)));
        // Endpoints that are not nodes: an unknown prefix, an absent id, an
        // id present under ANOTHER prefix only.
        assert_eq!(k.resolve("f:0"), None);
        assert_eq!(k.resolve("m:3"), None);
        assert_eq!(k.resolve("cont:1"), None);
        assert_eq!(k.resolve("nonsense"), None);
        assert_eq!(k.weight["Message"], 3);
        assert_eq!(k.weight["City"], 2);
        assert!(k.fallback.is_empty());
    }

    #[test]
    fn a_node_whose_gid_does_not_spell_its_id_goes_through_the_fallback_map() {
        let mut p = NodePass::new();
        p.observe("p:0", &labels(&["Person"]), 0).unwrap();
        p.observe("person-nine", &labels(&["Person"]), 9).unwrap();
        // Spells 7 but IS 8: structured parse would find the wrong node.
        p.observe("p:7", &labels(&["Person"]), 8).unwrap();
        let k = p.finish().unwrap();
        assert_eq!(k.fallback.len(), 2);
        assert_eq!(k.resolve("person-nine"), Some(("Person", 9)));
        assert_eq!(k.resolve("p:7"), Some(("Person", 8)));
        assert_eq!(k.resolve("p:0"), Some(("Person", 0)));
        // `p:8` names no node even though id 8 exists under the prefix's label.
        assert_eq!(k.resolve("p:8"), None);
        assert_eq!(k.weight["Person"], 3);
    }

    #[test]
    fn one_corpus_id_naming_two_nodes_across_the_partitions_is_refused() {
        // The reviewer's scenario: a structured `p:0` (id 0) and a second node
        // ALSO spelled `p:0` whose id (120) is unique under Person. Without
        // the cross-check the second went into the fallback map, `resolve`
        // consulted it first, and every relationship naming `p:0` bound to
        // the shadow — id 120 got the rels, id 0 got none, exit 0.
        let mut p = NodePass::new();
        p.observe("p:0", &labels(&["Person"]), 0).unwrap();
        let err = p.observe("p:0", &labels(&["Person"]), 120).unwrap_err();
        assert!(err.contains("\"p:0\""), "{err}");
        assert!(err.contains("id 0 ") && err.contains("id 120 "), "{err}");
        assert!(err.contains("TWO nodes"), "{err}");

        // The same corpus with the shadow FIRST: the structured arrival is
        // the one that must refuse.
        let mut p = NodePass::new();
        p.observe("p:0", &labels(&["Person"]), 120).unwrap();
        let err = p.observe("p:0", &labels(&["Person"]), 0).unwrap_err();
        assert!(
            err.contains("\"p:0\"") && err.contains("id 0 ") && err.contains("id 120 "),
            "{err}"
        );

        // The same string in both spellings, either order: `p:7` (id 7,
        // structured) and `p:7` (id 8, fallback).
        let mut p = NodePass::new();
        p.observe("p:7", &labels(&["Person"]), 8).unwrap();
        let err = p.observe("p:7", &labels(&["Person"]), 7).unwrap_err();
        assert!(
            err.contains("\"p:7\"") && err.contains("id 7 ") && err.contains("id 8 "),
            "{err}"
        );
        let mut p = NodePass::new();
        p.observe("p:7", &labels(&["Person"]), 7).unwrap();
        let err = p.observe("p:7", &labels(&["Person"]), 8).unwrap_err();
        assert!(
            err.contains("\"p:7\"") && err.contains("id 7 ") && err.contains("id 8 "),
            "{err}"
        );

        // Not a collision: a fallback spelled like a structured id whose
        // spelled (prefix, n) is NOT a node — `p:7` is id 8 and no node is
        // `p:7` id 7 — and an unstructured string cannot shadow anything.
        let mut p = NodePass::new();
        p.observe("p:0", &labels(&["Person"]), 0).unwrap();
        p.observe("p:7", &labels(&["Person"]), 8).unwrap();
        p.observe("weird", &labels(&["Person"]), 9).unwrap();
        let k = p.finish().unwrap();
        assert_eq!(k.resolve("p:0"), Some(("Person", 0)));
        assert_eq!(k.resolve("p:7"), Some(("Person", 8)));
        assert_eq!(k.resolve("weird"), Some(("Person", 9)));

        // A fallback `p:7` (id 8) beside a structured `p:8` (id 8): two
        // nodes with one Person id — not a shadow, but `finish` refuses it
        // because no MATCH on `id` can name one of them.
        let mut p = NodePass::new();
        p.observe("p:7", &labels(&["Person"]), 8).unwrap();
        p.observe("p:8", &labels(&["Person"]), 8).unwrap();
        let Err(err) = p.finish() else {
            panic!("one Person id on two nodes must refuse")
        };
        assert!(err.contains("Person (1 duplicate id(s))"), "{err}");
    }

    #[test]
    fn duplicate_ids_under_every_candidate_label_are_refused() {
        let mut p = NodePass::new();
        p.observe("x:0", &labels(&["X"]), 0).unwrap();
        p.observe("y:0", &labels(&["X"]), 0).unwrap();
        let Err(err) = p.finish() else {
            panic!("a repeated id under the only label must refuse")
        };
        assert!(
            err.contains("prefix \"x\"") || err.contains("prefix \"y\""),
            "{err}"
        );
        assert!(err.contains("X (1 duplicate id(s))"), "{err}");

        let mut p = NodePass::new();
        p.observe("p:1", &labels(&["Person"]), 1).unwrap();
        assert!(
            p.observe("p:1", &labels(&["Person"]), 1)
                .unwrap_err()
                .contains("duplicate corpus id")
        );
    }

    #[test]
    fn pre_flight_property_check_refuses_what_the_renderer_would_panic_on() {
        let mut ok = BTreeMap::new();
        ok.insert("id".to_string(), Value::Int(1));
        ok.insert("name_2".to_string(), Value::Str("x".into()));
        ok.insert("score".to_string(), Value::Float(1.5));
        ok.insert("flag".to_string(), Value::Bool(true));
        ok.insert("none".to_string(), Value::Null);
        assert_eq!(check_props(&ok), Ok(()));
        // A list is not something the renderer inlines.
        let mut bad = ok.clone();
        bad.insert("emails".to_string(), Value::List(vec![]));
        assert!(check_props(&bad).unwrap_err().contains("emails"));
        // A key that is not a bare identifier.
        let mut bad = ok.clone();
        bad.insert("first name".to_string(), Value::Int(1));
        assert!(check_props(&bad).unwrap_err().contains("first name"));
        // `gid` is written by the loader; a corpus carrying its own would
        // load one of the two silently.
        let mut bad = ok.clone();
        bad.insert("gid".to_string(), Value::Str("p:1".into()));
        assert!(check_props(&bad).unwrap_err().contains("gid"));
    }

    #[test]
    fn neo4j_is_recognised_by_its_hello_agent_and_this_engine_is_not() {
        assert!(is_neo4j("Neo4j/5.26.0"));
        assert!(is_neo4j("Neo4j/4.4.12"));
        assert!(!is_neo4j("engram/0.1.0"));
        assert!(!is_neo4j(""));
        // A server that merely mentions the word is not it.
        assert!(!is_neo4j("something (neo4j-compatible)"));
    }

    fn key(t: &str, s: &str, d: &str) -> GroupKey {
        (t.into(), s.into(), d.into())
    }

    #[test]
    fn groups_cluster_by_their_heaviest_label_then_keep_key_order() {
        let mut w = BTreeMap::new();
        w.insert("Message".to_string(), 1_000_000u64);
        w.insert("Forum".to_string(), 10_000);
        w.insert("Person".to_string(), 1_000);
        w.insert("Tag".to_string(), 100);
        let keys = vec![
            key("HAS_TAG", "Forum", "Tag"),
            key("KNOWS", "Person", "Person"),
            key("LIKES", "Person", "Message"),
            key("HAS_MEMBER", "Forum", "Person"),
            key("CONTAINER_OF", "Forum", "Message"),
            key("HAS_INTEREST", "Person", "Tag"),
            key("REPLY_OF", "Message", "Message"),
        ];
        let ordered = order_groups(keys, &w);
        assert_eq!(
            ordered,
            vec![
                // Message groups first, back to back; among them the one whose
                // OTHER end is heaviest first, then key order.
                key("REPLY_OF", "Message", "Message"),
                key("CONTAINER_OF", "Forum", "Message"),
                key("LIKES", "Person", "Message"),
                key("HAS_MEMBER", "Forum", "Person"),
                key("HAS_TAG", "Forum", "Tag"),
                key("KNOWS", "Person", "Person"),
                key("HAS_INTEREST", "Person", "Tag"),
            ]
        );
        // The unweighted (unknown label) case sorts last but stays deterministic.
        let ordered = order_groups(vec![key("Z", "Q", "Q"), key("A", "Q", "Q")], &w);
        assert_eq!(ordered, vec![key("A", "Q", "Q"), key("Z", "Q", "Q")]);
    }

    #[test]
    fn last_use_is_the_last_group_naming_each_label() {
        let ordered = vec![
            key("REPLY_OF", "Message", "Message"),
            key("LIKES", "Person", "Message"),
            key("KNOWS", "Person", "Person"),
            key("HAS_TAG", "Forum", "Tag"),
        ];
        let last = last_use(&ordered);
        assert_eq!(last["Message"], 1);
        assert_eq!(last["Person"], 2);
        assert_eq!(last["Forum"], 3);
        assert_eq!(last["Tag"], 3);
        assert_eq!(last.len(), 4);
        // A key label no group names has no last use: its index is dropped
        // before the pass rather than living through it.
        assert!(!last.contains_key("TagClass"));
    }

    #[test]
    fn pair_lists_chunk_at_batch_and_inline_the_mode_s_literal() {
        let ids = Pairs::Ids((0..BATCH as u32 + 1).map(|i| (i, i + 1)).collect());
        let lists = ids.lists();
        assert_eq!(lists.len(), 2);
        assert!(lists[0].starts_with("[0, 1], [1, 2]"));
        assert_eq!(lists[1], format!("[{BATCH}, {}]", BATCH + 1));
        let gids = Pairs::Gids(vec![("p:1".into(), "m:2".into())]);
        assert_eq!(gids.lists(), vec!["['p:1', 'm:2']".to_string()]);
        assert_eq!(gids.len(), 1);
    }
}
