//! A minimal JSON codec for the `apoc.convert.*` trio — the only JSON this
//! crate needs, written here rather than pulling a dependency into the
//! zero-dep core.

use std::collections::BTreeMap;

use crate::value::Value;

/// Render a value as JSON. Null renders as `null`; floats use Rust's
/// shortest-round-trip formatting; non-finite floats render as `null`
/// (JSON has no NaN, and refusing the whole document for one NaN would make
/// serialisation data-dependent in a way callers cannot see).
pub fn to_json(v: &Value) -> String {
    let mut s = String::new();
    write_json(v, &mut s);
    s
}

fn write_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => {
            if f.is_finite() {
                out.push_str(&format!("{f:?}"));
            } else {
                out.push_str("null");
            }
        }
        Value::Str(s) => write_json_string(s, out),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Map(entries) => {
            out.push('{');
            for (i, (k, item)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
        // apoc.convert.toJson on a node/relationship renders the PROPERTY
        // map — APOC's behaviour, and the only lossless JSON reading.
        Value::Node { props, .. } | Value::Rel { props, .. } => {
            write_json(&Value::Map(props.clone()), out);
        }
        // Temporals render as their ISO strings — what APOC does.
        other => write_json_string(&crate::temporal_to_string(other), out),
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse JSON into a value. Strict: trailing input, bad escapes and malformed
/// numbers refuse with a position.
pub fn from_json(src: &str) -> Result<Value, String> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let v = parse_value(b, &mut i)?;
    skip_ws(b, &mut i);
    if i != b.len() {
        return Err(format!("trailing input at byte {i}"));
    }
    Ok(v)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\r' | b'\n') {
        *i += 1;
    }
}

fn parse_value(b: &[u8], i: &mut usize) -> Result<Value, String> {
    skip_ws(b, i);
    match b.get(*i) {
        None => Err("unexpected end of JSON".into()),
        Some(b'n') => lit(b, i, "null", Value::Null),
        Some(b't') => lit(b, i, "true", Value::Bool(true)),
        Some(b'f') => lit(b, i, "false", Value::Bool(false)),
        Some(b'"') => Ok(Value::Str(parse_string(b, i)?)),
        Some(b'[') => {
            *i += 1;
            let mut items = Vec::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Ok(Value::List(items));
            }
            loop {
                items.push(parse_value(b, i)?);
                skip_ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b']') => {
                        *i += 1;
                        return Ok(Value::List(items));
                    }
                    _ => return Err(format!("expected `,` or `]` at byte {i}", i = *i)),
                }
            }
        }
        Some(b'{') => {
            *i += 1;
            let mut map = BTreeMap::new();
            skip_ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Ok(Value::Map(map));
            }
            loop {
                skip_ws(b, i);
                let key = parse_string(b, i)?;
                skip_ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return Err(format!("expected `:` at byte {i}", i = *i));
                }
                *i += 1;
                map.insert(key, parse_value(b, i)?);
                skip_ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b'}') => {
                        *i += 1;
                        return Ok(Value::Map(map));
                    }
                    _ => return Err(format!("expected `,` or `}}` at byte {i}", i = *i)),
                }
            }
        }
        Some(_) => parse_number(b, i),
    }
}

fn lit(b: &[u8], i: &mut usize, word: &str, v: Value) -> Result<Value, String> {
    if b[*i..].starts_with(word.as_bytes()) {
        *i += word.len();
        Ok(v)
    } else {
        Err(format!("malformed literal at byte {i}", i = *i))
    }
}

fn parse_string(b: &[u8], i: &mut usize) -> Result<String, String> {
    if b.get(*i) != Some(&b'"') {
        return Err(format!("expected `\"` at byte {i}", i = *i));
    }
    *i += 1;
    let mut s = String::new();
    loop {
        match b.get(*i) {
            None => return Err("unterminated JSON string".into()),
            Some(b'"') => {
                *i += 1;
                return Ok(s);
            }
            Some(b'\\') => {
                let esc = b.get(*i + 1).ok_or("unterminated escape")?;
                match esc {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'/' => s.push('/'),
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    b'b' => s.push('\u{8}'),
                    b'f' => s.push('\u{c}'),
                    b'u' => {
                        let hex = b
                            .get(*i + 2..*i + 6)
                            .ok_or("truncated \\u escape")
                            .and_then(|h| std::str::from_utf8(h).map_err(|_| "bad \\u escape"))?;
                        let cp = u32::from_str_radix(hex, 16).map_err(|_| "bad \\u escape")?;
                        // Surrogate pairs: JSON's one wrinkle.
                        if (0xD800..0xDC00).contains(&cp) {
                            let lo_hex = b
                                .get(*i + 8..*i + 12)
                                .filter(|_| b.get(*i + 6..*i + 8) == Some(b"\\u"))
                                .ok_or("lone high surrogate")
                                .and_then(|h| {
                                    std::str::from_utf8(h).map_err(|_| "bad low surrogate")
                                })?;
                            let lo =
                                u32::from_str_radix(lo_hex, 16).map_err(|_| "bad low surrogate")?;
                            if !(0xDC00..0xE000).contains(&lo) {
                                return Err("invalid low surrogate".into());
                            }
                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            s.push(char::from_u32(c).ok_or("invalid surrogate pair")?);
                            *i += 12;
                            continue;
                        }
                        s.push(char::from_u32(cp).ok_or("invalid code point")?);
                        *i += 6;
                        continue;
                    }
                    _ => return Err(format!("unknown escape at byte {i}", i = *i)),
                }
                *i += 2;
            }
            Some(_) => {
                let start = *i;
                while *i < b.len() && b[*i] != b'"' && b[*i] != b'\\' {
                    *i += 1;
                }
                s.push_str(
                    std::str::from_utf8(&b[start..*i]).map_err(|_| "invalid UTF-8 in JSON")?,
                );
            }
        }
    }
}

fn parse_number(b: &[u8], i: &mut usize) -> Result<Value, String> {
    let start = *i;
    if b.get(*i) == Some(&b'-') {
        *i += 1;
    }
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    let mut is_float = false;
    if b.get(*i) == Some(&b'.') {
        is_float = true;
        *i += 1;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    if matches!(b.get(*i), Some(b'e') | Some(b'E')) {
        is_float = true;
        *i += 1;
        if matches!(b.get(*i), Some(b'+') | Some(b'-')) {
            *i += 1;
        }
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    let text = std::str::from_utf8(&b[start..*i]).map_err(|_| "bad number")?;
    if text.is_empty() || text == "-" {
        return Err(format!("malformed number at byte {start}"));
    }
    if is_float {
        text.parse()
            .map(Value::Float)
            .map_err(|_| format!("malformed number `{text}`"))
    } else {
        // An integer too large for i64 falls back to float rather than
        // refusing: JSON in the wild carries them, and the alternative loses
        // the whole document for one large id — but the fallback is LOSSY,
        // which is why the manifest and wire layers never route ids through
        // JSON.
        match text.parse::<i64>() {
            Ok(v) => Ok(Value::Int(v)),
            Err(_) => text
                .parse()
                .map(Value::Float)
                .map_err(|_| format!("malformed number `{text}`")),
        }
    }
}
