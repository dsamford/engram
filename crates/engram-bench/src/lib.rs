//! Shared canonicalisation + comparison for the decoded-values and port
//! benchmarks — ONE set of rules both instruments apply to both engines,
//! so the two runs cannot quietly diverge in what "identical" means.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::temporal::{parse_date, parse_duration, parse_time_of_day, parse_zone};
use engram_cypher::{Value, json, temporal_to_string};

// ── Temporal string canonicalisation (applied to BOTH sides) ───────────────

/// `+00:00` → `Z`, fractional trailing zeros trimmed, bracket zone dropped,
/// durations re-rendered through one formatter.
pub fn canon_temporal_str(s: &str) -> String {
    let b = s.as_bytes();
    let date_like = b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit();
    let time_like = b.len() >= 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b':'
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && b[5] == b':';
    if !date_like && !time_like {
        // Both engines render durations as `P…` but split components
        // differently (the driver never carries months into years; Engram
        // does). Any P-string that parses as a duration is re-rendered.
        if s.starts_with('P') {
            if let Some((mo, d, sec, ns)) = parse_duration(s) {
                return engram_cypher::temporal::format_duration(mo, d, sec, ns);
            }
        }
        return s.to_string();
    }
    if date_like && !s.contains('T') {
        return s.to_string(); // plain date — one rendering exists
    }
    let mut out = s.to_string();
    if let (Some(lb), true) = (out.find('['), out.ends_with(']')) {
        out.truncate(lb);
    }
    if out.ends_with("+00:00") {
        out.truncate(out.len() - 6);
        out.push('Z');
    }
    // Trim fractional trailing zeros: `.500000000` → `.5`, `.000` → gone.
    let tsep = out.find('T').map_or(0, |i| i + 1);
    if let Some(dot_rel) = out[tsep..].find('.') {
        let dot = tsep + dot_rel;
        let frac_end = out[dot + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .map_or(out.len(), |i| dot + 1 + i);
        let mut keep = frac_end;
        while keep > dot + 1 && out.as_bytes()[keep - 1] == b'0' {
            keep -= 1;
        }
        if keep == dot + 1 {
            keep = dot; // fraction was all zeros — drop the dot too
        }
        out.replace_range(keep..frac_end, "");
    }
    out
}

// ── Loading the export ──────────────────────────────────────────────────────

/// Rebuild a temporal `Value` from a tagged export property.
pub fn untag_temporal(tag: &str, s: &str) -> Option<Value> {
    match tag {
        "~date" => parse_date(s).map(Value::Date),
        "~ltime" => match parse_time_of_day(s) {
            Some((n, "")) => Some(Value::LocalTime(n)),
            _ => None,
        },
        "~time" => {
            let (n, rest) = parse_time_of_day(s)?;
            let (offset, _zone, rest) = parse_zone(rest)?;
            if !rest.is_empty() {
                return None;
            }
            Some(Value::Time {
                nanos: n,
                offset_seconds: offset.unwrap_or(0),
            })
        }
        "~dt" | "~ldt" => {
            let t_at = s.find('T')?;
            let days = parse_date(&s[..t_at])?;
            let (tod, rest) = parse_time_of_day(&s[t_at + 1..])?;
            let (offset, zone, rest) = parse_zone(rest)?;
            if !rest.is_empty() {
                return None;
            }
            let local_seconds = days * 86_400 + tod.div_euclid(1_000_000_000);
            let nanos = tod.rem_euclid(1_000_000_000) as u32;
            if tag == "~ldt" {
                return Some(Value::LocalDateTime {
                    epoch_seconds: local_seconds,
                    nanos,
                });
            }
            let offset_seconds = match (offset, &zone) {
                (Some(o), _) => o,
                (None, Some(z)) if z == "UTC" || z == "Z" || z == "Etc/UTC" => 0,
                _ => return None, // a named zone with no offset needs tzdata
            };
            Some(Value::DateTime {
                epoch_seconds: local_seconds - i64::from(offset_seconds),
                nanos,
                offset_seconds,
                zone: None,
            })
        }
        "~dur" => parse_duration(s).map(|(mo, d, sec, ns)| Value::Duration {
            months: mo,
            days: d,
            seconds: sec,
            nanos: ns,
        }),
        _ => None,
    }
}

/// Convert an exported property value into what the store should hold:
/// tagged temporals become real temporal values, everything else stays.
pub fn untag_prop(v: &Value, unloadable: &mut usize) -> Value {
    match v {
        Value::Map(m) => {
            if m.len() == 1 {
                let (k, inner) = m.iter().next().expect("len checked");
                if k.starts_with('~') {
                    if let Value::Str(s) = inner {
                        if k == "~bigint" {
                            return s
                                .parse::<i64>()
                                .map(Value::Int)
                                .unwrap_or_else(|_| Value::Str(s.clone()));
                        }
                        if let Some(t) = untag_temporal(k, s) {
                            return t;
                        }
                    }
                    *unloadable += 1;
                    return Value::Str(format!("<unloadable {k}>"));
                }
            }
            Value::Map(
                m.iter()
                    .map(|(k, x)| (k.clone(), untag_prop(x, unloadable)))
                    .collect(),
            )
        }
        Value::List(items) => {
            Value::List(items.iter().map(|x| untag_prop(x, unloadable)).collect())
        }
        other => other.clone(),
    }
}

// ── Result-row canonicalisation ─────────────────────────────────────────────

/// Integral floats collapse to ints; non-finite becomes null.
pub fn canon_float(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    if f.fract() == 0.0 && f.abs() < 9.0e15 {
        return Value::Int(f as i64);
    }
    Value::Float(f)
}

/// Canonicalise a LIST as a MULTISET — map each element with `f`, then sort by a
/// total key (the element's JSON). `collect()` order is language-UNSPECIFIED
/// (openCypher/GQL, exactly as SQL `array_agg` without an inner `ORDER BY`), so a
/// list column that differs only in order from the incumbent is EQUAL, not a
/// divergence — and the engine stays free to pick a deterministic traversal order
/// of its own. The LDBC corpus's only list columns are `collect()` results, so
/// sorting every list is correct here; an ORDER-SIGNIFICANT list column (a list
/// comprehension the caller ordered) would need per-column provenance the corpus
/// does not carry. `collect(x ORDER BY …)` is the spec-blessed way to force order.
fn canon_list(items: &[Value], f: fn(&Value) -> Value) -> Value {
    let mut out: Vec<Value> = items.iter().map(f).collect();
    out.sort_by_key(json::to_json);
    Value::List(out)
}

/// Canonicalise an Engram result value into the JSON-carrier shape the
/// incumbent capture used: nodes `{"~n":[labels,props]}`, rels
/// `{"~r":[type,props]}`, temporals as canonical strings, integral floats
/// as ints.
pub fn canon_engram(v: &Value) -> Value {
    match v {
        Value::Null | Value::Bool(_) | Value::Int(_) => v.clone(),
        Value::Float(f) => canon_float(*f),
        Value::Str(s) => Value::Str(canon_temporal_str(s)),
        Value::List(items) => canon_list(items, canon_engram),
        Value::Map(m) => Value::Map(
            m.iter()
                .map(|(k, x)| (k.clone(), canon_engram(x)))
                .collect(),
        ),
        Value::Node { labels, props, .. } => {
            let mut ls: Vec<String> = labels.clone();
            ls.sort();
            let props = Value::Map(
                props
                    .iter()
                    .map(|(k, x)| (k.clone(), canon_engram(x)))
                    .collect(),
            );
            Value::Map(BTreeMap::from([(
                "~n".to_string(),
                Value::List(vec![
                    Value::List(ls.into_iter().map(Value::Str).collect()),
                    props,
                ]),
            )]))
        }
        Value::Rel {
            rel_type, props, ..
        } => {
            let props = Value::Map(
                props
                    .iter()
                    .map(|(k, x)| (k.clone(), canon_engram(x)))
                    .collect(),
            );
            Value::Map(BTreeMap::from([(
                "~r".to_string(),
                Value::List(vec![Value::Str(rel_type.clone()), props]),
            )]))
        }
        temporal => Value::Str(canon_temporal_str(&temporal_to_string(temporal))),
    }
}

/// Canonicalise an incumbent (already-JSON) value with the SAME rules, so
/// both sides meet at one shape.
pub fn canon_incumbent(v: &Value) -> Value {
    match v {
        Value::Float(f) => canon_float(*f),
        Value::Str(s) => Value::Str(canon_temporal_str(s)),
        Value::List(items) => canon_list(items, canon_incumbent),
        Value::Map(m) => {
            if m.len() == 1 {
                if let Some(Value::Str(s)) = m.get("~bigint") {
                    return s
                        .parse::<i64>()
                        .map(Value::Int)
                        .unwrap_or_else(|_| Value::Str(s.clone()));
                }
            }
            Value::Map(
                m.iter()
                    .map(|(k, x)| (k.clone(), canon_incumbent(x)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

// ── Tie-boundary detection ──────────────────────────────────────────────────

/// The one divergence ORDER BY … LIMIT legitimately produces: rows tied on
/// the sort key at the cut, where WHICH tied row survives is engine choice.
/// Verified, never assumed: the sort keys must all be projected aliases,
/// counts must match, and every row in the symmetric difference must carry
/// the same single sort-key tuple on both sides.
pub fn tie_at_limit_boundary(
    text: &str,
    cols: &[String],
    only_inc: &[&Value],
    only_eng: &[&Value],
) -> bool {
    if only_inc.len() != only_eng.len() || only_inc.is_empty() {
        return false;
    }
    // ASCII-only uppercase: length-preserving, so byte offsets found in
    // `upper` stay valid in `text` (statements carry non-ASCII literals).
    let upper: String = text.chars().map(|c| c.to_ascii_uppercase()).collect();
    if !upper.contains("LIMIT") {
        return false;
    }
    let Some(ob) = upper.rfind("ORDER BY") else {
        return false;
    };
    let tail = &text[ob + "ORDER BY".len()..];
    let tail_upper = &upper[ob + "ORDER BY".len()..];
    let end = tail_upper.find("LIMIT").unwrap_or(tail.len());
    let mut idxs = Vec::new();
    for part in tail[..end].split(',') {
        let mut tok = part.trim();
        for suffix in ["DESC", "desc", "ASC", "asc"] {
            if let Some(stripped) = tok.strip_suffix(suffix) {
                tok = stripped.trim();
                break;
            }
        }
        match cols.iter().position(|c| c == tok) {
            Some(i) => idxs.push(i),
            None => return false, // key is not a projected alias — cannot verify
        }
    }
    let key_of = |row: &Value| -> Option<String> {
        let Value::List(items) = row else { return None };
        let parts: Option<Vec<String>> = idxs
            .iter()
            .map(|&i| items.get(i).map(json::to_json))
            .collect();
        parts.map(|p| p.join("\u{1}"))
    };
    let mut keys: Vec<String> = Vec::new();
    for row in only_inc.iter().chain(only_eng.iter()) {
        match key_of(row) {
            Some(k) => keys.push(k),
            None => return false,
        }
    }
    keys.sort();
    keys.dedup();
    keys.len() == 1
}

// ── Replay ──────────────────────────────────────────────────────────────────

/// A string (or stringified int) field of a JSON map, or empty.
pub fn get_str(m: &BTreeMap<String, Value>, k: &str) -> String {
    match m.get(k) {
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Int(i)) => i.to_string(),
        _ => String::new(),
    }
}
/// A boolean field of a JSON map, defaulting false.
pub fn get_bool(m: &BTreeMap<String, Value>, k: &str) -> bool {
    matches!(m.get(k), Some(Value::Bool(true)))
}
/// A list field of a JSON map, or the empty slice.
pub fn get_list<'a>(m: &'a BTreeMap<String, Value>, k: &'a str) -> &'a [Value] {
    match m.get(k) {
        Some(Value::List(items)) => items,
        _ => &[],
    }
}

/// What one export load produced.
#[derive(Debug, Clone, Default)]
pub struct LoadStats {
    /// Nodes created.
    pub nodes: u64,
    /// Relationships created.
    pub rels: u64,
    /// Relationships skipped because an end did not load.
    pub dangling: u64,
    /// Property values the export carried that the engine cannot hold.
    pub unloadable: usize,
    /// Wall time of the load.
    pub load_ms: u128,
}

/// Read a `.jsonl` file, one JSON value per non-empty line.
pub fn read_jsonl(path: &std::path::Path, mut f: impl FnMut(Value)) {
    use std::io::BufRead;
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let reader = std::io::BufReader::with_capacity(1 << 20, file);
    for (n, line) in reader.lines().enumerate() {
        let line = line.expect("read line");
        if line.trim().is_empty() {
            continue;
        }
        match json::from_json(&line) {
            Ok(v) => f(v),
            Err(e) => panic!("{path:?} line {}: {e}", n + 1),
        }
    }
}

/// Load a production export (`meta.json`, `nodes.jsonl`, `rels.jsonl`)
/// into `graph` under bulk ingest: the wall clock from `captured_at`,
/// every node, then every relationship whose ends loaded. The load's own
/// log is shipped state (the export is the archive) and is truncated.
/// Used by the port benchmark and by `portserve`, which serves the same
/// world over Bolt for the cutover measurement.
pub fn load_export(graph: &engram_graph::Graph, dir: &std::path::Path) -> LoadStats {
    let started = std::time::Instant::now();
    if let Ok(meta_raw) = std::fs::read_to_string(dir.join("meta.json")) {
        if let Ok(Value::Map(meta)) = json::from_json(&meta_raw) {
            let at = get_str(&meta, "captured_at");
            if let Some(Value::DateTime {
                epoch_seconds,
                nanos,
                ..
            }) = untag_temporal("~dt", &at)
            {
                graph.set_wall_ms(epoch_seconds * 1_000 + i64::from(nanos / 1_000_000));
            }
        }
    }
    graph.set_bulk_ingest(true).expect("bulk on");
    let mut stats = LoadStats::default();
    let mut id_map: BTreeMap<String, u64> = BTreeMap::new();
    read_jsonl(&dir.join("nodes.jsonl"), |v| {
        let Value::Map(m) = v else { return };
        let labels: Vec<String> = get_list(&m, "l")
            .iter()
            .filter_map(|l| match l {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let props: BTreeMap<String, Value> = match m.get("p") {
            Some(Value::Map(p)) => p
                .iter()
                .map(|(k, x)| (k.clone(), untag_prop(x, &mut stats.unloadable)))
                .collect(),
            _ => BTreeMap::new(),
        };
        let id = graph.create_node(&labels, &props).expect("create node");
        id_map.insert(get_str(&m, "i"), id);
        stats.nodes += 1;
        if stats.nodes % 200_000 == 0 {
            eprintln!("[load] nodes loaded: {}", stats.nodes);
        }
    });
    read_jsonl(&dir.join("rels.jsonl"), |v| {
        let Value::Map(m) = v else { return };
        let (Some(&src), Some(&dst)) =
            (id_map.get(&get_str(&m, "s")), id_map.get(&get_str(&m, "d")))
        else {
            stats.dangling += 1;
            return;
        };
        let props: BTreeMap<String, Value> = match m.get("p") {
            Some(Value::Map(p)) => p
                .iter()
                .map(|(k, x)| (k.clone(), untag_prop(x, &mut stats.unloadable)))
                .collect(),
            _ => BTreeMap::new(),
        };
        graph
            .create_rel(src, &get_str(&m, "t"), dst, &props)
            .expect("create rel");
        stats.rels += 1;
        if stats.rels % 500_000 == 0 {
            eprintln!("[load] rels loaded: {}", stats.rels);
        }
    });
    stats.load_ms = started.elapsed().as_millis();
    graph.set_bulk_ingest(false).expect("bulk exit");
    let log_len = graph.shared_store().log_len();
    graph.shared_store().truncate_log_below(log_len);
    stats
}

#[cfg(test)]
mod canon_tests {
    use super::{canon_engram, canon_incumbent};
    use engram_cypher::Value;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    /// A `collect()` list that differs only in ORDER (the IC12 case) canonicalises
    /// equal on both sides — collect order is language-unspecified, so this is not
    /// a divergence. Row `["Bo", [Tag1, Tag2, Tag3]]` vs `["Bo", [Tag3, Tag1, Tag2]]`.
    #[test]
    fn collect_list_order_is_ignored() {
        let engram = Value::List(vec![
            s("Bo"),
            Value::List(vec![s("Tag1"), s("Tag2"), s("Tag3")]),
        ]);
        let neo4j = Value::List(vec![
            s("Bo"),
            Value::List(vec![s("Tag3"), s("Tag1"), s("Tag2")]),
        ]);
        assert_eq!(
            canon_engram(&engram),
            canon_incumbent(&neo4j),
            "a collect() list differing only in order must canonicalise equal"
        );
    }

    /// But a list with DIFFERENT elements still diverges (the fix is order-only,
    /// not a blanket equality).
    #[test]
    fn different_list_elements_still_diverge() {
        let a = Value::List(vec![s("Tag1"), s("Tag2")]);
        let b = Value::List(vec![s("Tag1"), s("Tag9")]);
        assert_ne!(canon_engram(&a), canon_incumbent(&b));
    }
}
