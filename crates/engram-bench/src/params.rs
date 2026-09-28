//! Substitution parameters: the catalogue's declared types, and the refusal
//! that stops a query running with the wrong ones.
//!
//! # Why this module exists at all
//!
//! Every LDBC read battery is parameterised, and **a parameter is part of the
//! question**. Two measurements in this project's own history are not results
//! because of it:
//!
//! * `bi12` ran `languages ['en','de']` against a corpus carrying `uz`, `tk`
//!   and `ar`. It matched zero rows. The recorded figure — 1 row, 160 s — is a
//!   timing for a filter that excluded everything.
//! * `bi16` ran dates on which its tag had no messages, twice, and returned an
//!   empty result each time.
//!
//! Neither failed. Both produced a well-formed answer that looked like a fast
//! query, and both were written into a comparison table. That is the shape this
//! module is built against: **an empty result is indistinguishable from a
//! working query unless something checks.**
//!
//! # The two rules
//!
//! 1. **A declared parameter with no supplied value is a REFUSAL**, never a
//!    default and never a silent omission. [`bind`] names the missing parameter
//!    and the query it belongs to.
//! 2. **Coercion follows the catalogue's declared type, not the value's
//!    shape.** `"2012-09-16"` is a `DATE` because `bi16` says `dateA` is a
//!    `DATE`, not because it looks like one. Guessing from shape is how a date
//!    becomes a string comparison against a temporal column — which matches
//!    nothing, and reads as a working query.

use engram_cypher::Value;
use std::collections::BTreeMap;

/// One parameter as the catalogue declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    /// The name the statement binds — `$dateA` in Cypher, `:dateA` in SQL.
    pub name: String,
    /// The LDBC type: `ID`, `INT`, `INT32`, `DATE`, `DATETIME`, `STRING`,
    /// `STRING[]`, `FLOAT`.
    pub ty: String,
}

/// Days from the civil date, by Howard Hinnant's algorithm.
///
/// # Errors
/// If `s` is not `YYYY-MM-DD`.
pub fn iso_to_days(s: &str) -> Result<i64, String> {
    let head = s.split(['T', ' ']).next().unwrap_or(s);
    let p: Vec<&str> = head.split('-').collect();
    if p.len() != 3 {
        return Err(format!("`{s}` is not a YYYY-MM-DD date"));
    }
    let y: i64 = p[0]
        .parse()
        .map_err(|_| format!("`{s}` has a non-numeric year"))?;
    let m: i64 = p[1]
        .parse()
        .map_err(|_| format!("`{s}` has a non-numeric month"))?;
    let d: i64 = p[2]
        .parse()
        .map_err(|_| format!("`{s}` has a non-numeric day"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("`{s}` is not a calendar date"));
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

/// Seconds since the epoch from an ISO date or date-time.
///
/// A bare date is midnight UTC, which is what LDBC's `datetime('2012-09-16')`
/// means.
///
/// # Errors
/// If the date part does not parse, or the time part is not `HH:MM[:SS]`.
pub fn iso_to_secs(s: &str) -> Result<i64, String> {
    let days = iso_to_days(s)?;
    let time = s.split(['T', ' ']).nth(1).unwrap_or("");
    let time = time.trim_end_matches('Z');
    // Drop a sub-second part; LDBC's parameters carry none.
    let time = time.split('.').next().unwrap_or("");
    let mut secs = 0i64;
    if !time.is_empty() {
        let p: Vec<&str> = time.split(':').collect();
        if p.len() < 2 {
            return Err(format!("`{s}` has a time part that is not HH:MM[:SS]"));
        }
        let h: i64 = p[0]
            .parse()
            .map_err(|_| format!("`{s}` has a non-numeric hour"))?;
        let mi: i64 = p[1]
            .parse()
            .map_err(|_| format!("`{s}` has a non-numeric minute"))?;
        let se: i64 = p.get(2).map_or(Ok(0), |x| {
            x.parse()
                .map_err(|_| format!("`{s}` has a non-numeric second"))
        })?;
        secs = h * 3600 + mi * 60 + se;
    }
    Ok(days * 86_400 + secs)
}

/// Sub-second nanoseconds from an ISO timestamp's fractional part.
///
/// [`iso_to_secs`] returns whole seconds, which is enough for a window bound
/// written by a human but NOT for one read back out of a corpus: `snbparams`
/// picks `min(creationDate)` and `max(creationDate)` as its `startDate` and
/// `endDate`, and truncating the max DOWN to the second excludes the very
/// message it was derived from. The window then legitimately matches one row
/// fewer than the corpus holds — a small error, but one that makes the
/// parameter file a lossy round trip of the data it was read from.
///
/// # Errors
/// If the fractional part is not numeric.
pub fn iso_nanos(s: &str) -> Result<u32, String> {
    let Some(frac) = s.split('.').nth(1) else {
        return Ok(0);
    };
    let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Ok(0);
    }
    let scaled: String = digits
        .chars()
        .chain(std::iter::repeat('0'))
        .take(9)
        .collect();
    scaled
        .parse::<u32>()
        .map_err(|_| format!("`{s}` has a non-numeric fractional second"))
}

/// How the CORPUS stores its temporal properties.
///
/// # Why this is not an implementation detail
///
/// The catalogue declares `bi1.datetime` as a `DATETIME` because LDBC's Cypher
/// says `datetime(...)`. Whether that binds correctly depends on something the
/// catalogue cannot know: how the loaded corpus stores `creationDate`.
///
/// Both typings exist in this project. `docs/bench/snb-datetime-corpus-build.sh`
/// exists precisely because the ordinary SNB corpus loads `creationDate` as an
/// epoch-millisecond INTEGER, and `ldbc-coverage-plan.md` §0.3.0 records that
/// the two SNB families need different corpus typings.
///
/// Bind a temporal against an integer column and the comparison matches
/// nothing — silently, and in a fraction of the real query's time. That is the
/// bi16 failure arriving by a second route, so the encoding is carried
/// explicitly and stamped into the parameter file rather than assumed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TemporalEncoding {
    /// `creationDate` is a real temporal; a `DATE`/`DATETIME` parameter binds
    /// as one.
    #[default]
    Typed,
    /// `creationDate` is an epoch-millisecond integer; a `DATE`/`DATETIME`
    /// parameter must bind as that integer or match nothing.
    EpochMillis,
}

impl TemporalEncoding {
    /// The name stamped into a parameter file.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TemporalEncoding::Typed => "typed",
            TemporalEncoding::EpochMillis => "epoch_millis",
        }
    }

    /// Parse the name back.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "typed" => Some(TemporalEncoding::Typed),
            "epoch_millis" => Some(TemporalEncoding::EpochMillis),
            _ => None,
        }
    }
}

/// Which identifier space a run binds ids in.
///
/// # Why one parameter file must carry two
///
/// The arms do not agree on what a Person's id IS. engram's `id` is DENSE
/// (0, 1, 2, ...) because its loader remaps; the PostgreSQL arm loaded raw
/// Datagen CSV with no remap, so its ids are LDBC's own. Measured on SF3
/// 2026-09-21: the Person engram calls `17049` carries `sourceId`
/// 15,393,162,799,074.
///
/// Handing both arms `personId = 17049` does not fail. It selects a DIFFERENT
/// ENTITY on one of them, and the comparison table looks fine.
///
/// The fix is not to fork the query text — the question ("this person") is the
/// same, and forking would turn a Bolt comparison into a comparison of two
/// Cyphers, which is the thing the shared-dialect rule exists to prevent. It
/// is to carry BOTH identifiers for the same parameter and bind whichever the
/// engine's id space uses. `snbparams` emits `personId` and `personId@ldbc`;
/// this chooses between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IdSpace {
    /// The corpus's own dense ids — engram and the Neo4j arm loaded from the
    /// same JSONL.
    #[default]
    Dense,
    /// LDBC's published ids — the PostgreSQL arm, loaded from raw CSV.
    Ldbc,
}

/// The suffix marking a parameter's LDBC-id companion value.
pub const LDBC_SUFFIX: &str = "@ldbc";

/// One text value, coerced against the declared type AND the corpus's
/// temporal encoding.
///
/// # Errors
/// As [`coerce`].
pub fn coerce_with(text: &str, ty: &str, enc: TemporalEncoding) -> Result<Value, String> {
    let t = ty.trim().to_ascii_uppercase();
    // `DATETIME_EPOCH_MILLIS` names a STORAGE form, not a logical type, and
    // `snb-interactive` declares five of its parameters that way. It is right
    // for the corpus it was written against and wrong for the other one: bound
    // as an integer against a typed-temporal column it matches NOTHING, which
    // is the same false-empty as the BI family's mis-declared DATEs arriving
    // from the opposite direction.
    //
    // So the declaration is read as "a datetime, which that corpus happened to
    // store as millis" and the CORPUS decides the binding.
    if matches!(
        t.as_str(),
        "DATETIME_EPOCH_MILLIS" | "EPOCH_MILLIS" | "TIMESTAMP_EPOCH_MILLIS"
    ) && enc == TemporalEncoding::Typed
    {
        return coerce(text, "DATETIME");
    }
    if enc == TemporalEncoding::EpochMillis && matches!(t.as_str(), "DATE" | "DATETIME") {
        let trimmed = text.trim();
        // Already an epoch-millisecond integer (which is what `snbparams`
        // reads straight out of such a corpus).
        if let Ok(ms) = trimmed.parse::<i64>() {
            return Ok(Value::Int(ms));
        }
        // Written as a date by a human; convert rather than refuse, so a
        // hand-edited parameter file still binds against this corpus.
        return Ok(Value::Int(iso_to_secs(trimmed)? * 1000));
    }
    coerce(text, ty)
}

/// One text value, coerced to the type the catalogue declared for it.
///
/// # Errors
/// If the text does not parse as that type, or the type is one this function
/// does not know — an unknown type is refused rather than passed through as a
/// string, because a string that should have been a date is the exact failure
/// this module exists to stop.
pub fn coerce(text: &str, ty: &str) -> Result<Value, String> {
    let t = ty.trim().to_ascii_uppercase();
    // An array type is its element type, repeated. LDBC's CSV parameter files
    // separate elements with ';'.
    if let Some(elem) = t.strip_suffix("[]") {
        let items = text
            .split(';')
            .map(|x| coerce(x.trim(), elem))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::List(std::sync::Arc::new(items)));
    }
    Ok(match t.as_str() {
        "ID" | "INT" | "INTEGER" | "INT32" | "INT64" | "LONG" | "SHORT" => Value::Int(
            text.trim()
                .parse()
                .map_err(|_| format!("`{text}` is not an integer ({ty})"))?,
        ),
        "FLOAT" | "FLOAT32" | "FLOAT64" | "DOUBLE" | "DECIMAL" => Value::Float(
            text.trim()
                .parse()
                .map_err(|_| format!("`{text}` is not a number ({ty})"))?,
        ),
        "BOOL" | "BOOLEAN" => match text.trim() {
            "true" | "TRUE" | "1" => Value::Bool(true),
            "false" | "FALSE" | "0" => Value::Bool(false),
            _ => return Err(format!("`{text}` is not a boolean")),
        },
        // The catalogue's OWN way of saying the corpus holds this as a number.
        // `snb-interactive` declares five of its parameters this way, which is
        // a more precise statement than `DATETIME` plus a corpus-wide
        // encoding guess -- so it wins wherever it appears.
        "DATETIME_EPOCH_MILLIS" | "EPOCH_MILLIS" | "TIMESTAMP_EPOCH_MILLIS" => {
            let t = text.trim();
            Value::Int(match t.parse::<i64>() {
                Ok(ms) => ms,
                Err(_) => iso_to_secs(t)? * 1000,
            })
        }
        "DATE" => Value::Date(iso_to_days(text.trim())?),
        "DATETIME" => Value::DateTime {
            epoch_seconds: iso_to_secs(text.trim())?,
            nanos: iso_nanos(text.trim())?,
            offset_seconds: 0,
            zone: None,
        },
        "STRING" | "TEXT" => Value::Str(text.to_string()),
        _ => {
            return Err(format!(
                "no coercion for catalogue parameter type `{ty}`; refusing \
                 rather than passing it through as a string"
            ));
        }
    })
}

/// Epoch-days as `YYYY-MM-DD`, by the civil-from-days algorithm.
#[must_use]
pub fn days_to_iso(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Epoch-seconds plus nanoseconds as an ISO-8601 instant.
///
/// `div_euclid`/`rem_euclid` so a pre-1970 timestamp floors into the right day
/// rather than truncating toward zero and landing a day late.
#[must_use]
pub fn secs_to_iso(epoch_seconds: i64, nanos: u32) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    let rem = epoch_seconds.rem_euclid(86_400);
    let (h, m, sec) = (rem / 3600, (rem / 60) % 60, rem % 60);
    format!(
        "{}T{h:02}:{m:02}:{sec:02}.{:03}Z",
        days_to_iso(days),
        nanos / 1_000_000
    )
}

/// One bound value as statement TEXT, for the catalogues whose templates
/// substitute rather than bind.
///
/// # This exists under protest
///
/// Binding is the correct mechanism and the rest of this module is built on
/// it. But `snb-interactive`'s Cypher carries `${personId}` and
/// `'${firstName}'` — LDBC-harness-style substitution, with the quotes already
/// written into the template — where `snb-bi`'s carries native `$tagA`. The
/// two catalogues were transcribed from different upstreams and genuinely
/// differ, and a lane that could only bind would report all 21 Interactive
/// queries as unrunnable.
///
/// So a template that demands substitution gets it, the document RECORDS that
/// it was rendered rather than bound, and the value still passes through
/// [`coerce_with`] first — so a `DATE` against an epoch-millisecond corpus
/// renders as the integer the column holds, not as a date string that would
/// match nothing.
#[must_use]
pub fn render_text(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        // Bare: the template supplies its own quotes.
        Value::Str(s) => s.clone(),
        // A TEMPORAL LITERAL, not a number.
        //
        // These templates substitute bare — `message.creationDate <= ${maxDate}`
        // — so whatever lands there is the right-hand side of a comparison
        // against a temporal column. Emitting the epoch integer compares a
        // DateTime to a number and matches NOTHING: measured on SF3
        // 2026-09-21, five Interactive queries (IC2, IC3, IC4, IC5, IC9)
        // returned zero rows for exactly this reason, each in a plausible
        // amount of time.
        //
        // On an epoch-millis corpus the value never reaches here as a
        // temporal: `coerce_with` has already made it a `Value::Int`, which
        // renders as the integer that corpus stores.
        Value::Date(d) => format!("date('{}')", days_to_iso(*d)),
        Value::DateTime {
            epoch_seconds,
            nanos,
            ..
        }
        | Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => format!("datetime('{}')", secs_to_iso(*epoch_seconds, *nanos)),
        Value::List(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|i| match i {
                    Value::Str(s) => format!("'{}'", s.replace('\'', "\\'")),
                    other => render_text(other),
                })
                .collect();
            format!("[{}]", inner.join(", "))
        }
        other => format!("{other:?}"),
    }
}

/// Bind a variant's declared parameters from supplied text values.
///
/// # Errors
/// If a declared parameter has no supplied value, or a value does not coerce.
/// Both name the parameter: a run that stops because `dateA` is missing is
/// recoverable; a run that proceeds without it is not.
pub fn bind(
    query: &str,
    specs: &[ParamSpec],
    supplied: &BTreeMap<String, String>,
    enc: TemporalEncoding,
    ids: IdSpace,
) -> Result<crate::backend::Params, String> {
    let mut out = crate::backend::Params::new();
    for s in specs {
        // In the LDBC id space, prefer this parameter's `@ldbc` companion
        // when the file carries one. Its ABSENCE is not an error: most
        // parameters are names or dates, which mean the same thing on every
        // arm and have no second form.
        let ldbc_key = format!("{}{LDBC_SUFFIX}", s.name);
        let chosen = if ids == IdSpace::Ldbc {
            supplied.get(&ldbc_key).or_else(|| supplied.get(&s.name))
        } else {
            supplied.get(&s.name)
        };
        let Some(text) = chosen else {
            return Err(format!(
                "{query}: no value supplied for the declared parameter `{}` \
                 ({}); the catalogue declares {} parameter(s) for this variant \
                 and {} were supplied",
                s.name,
                s.ty,
                specs.len(),
                supplied.len()
            ));
        };
        out.insert(
            s.name.clone(),
            coerce_with(text, &s.ty, enc).map_err(|e| format!("{query}.{}: {e}", s.name))?,
        );
    }
    // A supplied value nobody declared is a typo in the parameter file, and a
    // typo that binds nothing would leave the REAL parameter on its default.
    for k in supplied.keys() {
        // A `<name>@ldbc` companion is declared by its base name.
        let base = k.strip_suffix(LDBC_SUFFIX).unwrap_or(k);
        if !specs.iter().any(|s| s.name == base) {
            return Err(format!(
                "{query}: a value was supplied for `{k}`, which this variant \
                 does not declare; declared: {}",
                specs
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::{
        IdSpace, ParamSpec, TemporalEncoding, bind, coerce, coerce_with, iso_to_days, iso_to_secs,
        render_text,
    };
    use engram_cypher::Value;
    use std::collections::BTreeMap;

    fn spec(name: &str, ty: &str) -> ParamSpec {
        ParamSpec {
            name: name.into(),
            ty: ty.into(),
        }
    }

    #[test]
    fn a_date_round_trips_through_the_civil_algorithm() {
        assert_eq!(iso_to_days("1970-01-01"), Ok(0));
        assert_eq!(iso_to_days("2012-09-16"), Ok(15_599));
        assert_eq!(iso_to_days("1969-12-31"), Ok(-1));
        // A leap day, which a naive 365-day arithmetic gets wrong.
        assert_eq!(iso_to_days("2012-02-29"), Ok(15_399));
    }

    #[test]
    fn a_bare_date_is_midnight_utc() {
        assert_eq!(iso_to_secs("1970-01-01"), Ok(0));
        assert_eq!(iso_to_secs("2012-09-16"), Ok(15_599 * 86_400));
        assert_eq!(
            iso_to_secs("2012-09-16T12:30:05"),
            Ok(15_599 * 86_400 + 12 * 3600 + 30 * 60 + 5)
        );
    }

    #[test]
    fn coercion_follows_the_declared_type_not_the_shape() {
        // The same text, three declared types, three different values. This is
        // the whole point: shape-guessing would make all three a string.
        assert_eq!(coerce("2012-09-16", "DATE"), Ok(Value::Date(15_599)));
        assert!(matches!(
            coerce("2012-09-16", "DATETIME"),
            Ok(Value::DateTime { .. })
        ));
        assert_eq!(
            coerce("2012-09-16", "STRING"),
            Ok(Value::Str("2012-09-16".into()))
        );
    }

    #[test]
    fn a_string_array_splits_on_semicolons() {
        let v = coerce("uz;tk;ar", "STRING[]").unwrap();
        let Value::List(items) = v else {
            panic!("expected a list")
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Value::Str("uz".into()));
    }

    #[test]
    fn a_storage_specific_type_is_resolved_by_the_CORPUS_not_the_declaration() {
        // `snb-interactive` declares five parameters `DATETIME_EPOCH_MILLIS`.
        // That names a STORAGE form, and it is only right for the corpus it
        // was written against: bound as an integer against a typed-temporal
        // column it matches NOTHING — the same false-empty as the BI family's
        // mis-declared DATEs, arriving from the opposite direction.
        //
        // Measured on SF3 2026-09-21: seven Interactive parameters were
        // refused for matching zero rows until the corpus decided the binding.
        let iso = "2011-05-09T06:21:15.141Z";
        assert!(
            matches!(
                coerce_with(iso, "DATETIME_EPOCH_MILLIS", TemporalEncoding::Typed),
                Ok(Value::DateTime { .. })
            ),
            "a typed corpus must bind it as a datetime"
        );
        assert!(
            matches!(
                coerce_with(iso, "DATETIME_EPOCH_MILLIS", TemporalEncoding::EpochMillis),
                Ok(Value::Int(_))
            ),
            "an epoch-millis corpus must bind it as the integer it stores"
        );
        // And the reverse case, already fixed for the BI family.
        assert!(
            matches!(
                coerce_with("2012-09-16", "DATE", TemporalEncoding::EpochMillis),
                Ok(Value::Int(_))
            ),
            "a DATE against an integer column binds as the integer"
        );
    }

    #[test]
    fn a_rendered_temporal_is_a_cypher_literal_not_a_number() {
        // `snb-interactive` substitutes bare — `creationDate <= ${maxDate}` —
        // so an epoch integer there compares a DateTime to a number and
        // matches NOTHING. Measured on SF3 2026-09-21: IC2, IC3, IC4, IC5 and
        // IC9 all returned zero rows in plausible time for exactly this.
        let dt = Value::DateTime {
            epoch_seconds: 1_326_225_509,
            nanos: 414_000_000,
            offset_seconds: 0,
            zone: None,
        };
        let r = render_text(&dt);
        assert!(r.starts_with("datetime('"), "{r}");
        assert!(r.contains("2012-01-10T19:58:29.414Z"), "{r}");
        assert_eq!(render_text(&Value::Date(15_599)), "date('2012-09-16')");
        // An epoch-millis corpus never reaches that arm: coercion has already
        // made it the integer the corpus stores.
        assert_eq!(render_text(&Value::Int(1_326_225_509_414)), "1326225509414");
    }

    #[test]
    fn an_unknown_type_is_refused_rather_than_stringified() {
        let e = coerce("x", "GEOGRAPHY").unwrap_err();
        assert!(e.contains("no coercion"), "{e}");
    }

    #[test]
    fn a_missing_parameter_is_refused_and_names_itself() {
        // bi16's shape: five declared, four supplied. Running would measure a
        // query with a defaulted date.
        let specs = [spec("tagA", "STRING"), spec("dateA", "DATE")];
        let mut supplied = BTreeMap::new();
        supplied.insert("tagA".to_string(), "Meryl_Streep".to_string());
        let e = bind(
            "bi16",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Dense,
        )
        .unwrap_err();
        assert!(e.contains("dateA"), "{e}");
        assert!(e.contains("bi16"), "{e}");
    }

    #[test]
    fn an_id_binds_in_the_ID_SPACE_THE_ENGINE_USES() {
        // THE HAZARD, measured on SF3 2026-09-21. engram's `id` is DENSE and
        // the PostgreSQL arm carries LDBC's own: the Person engram calls
        // 17049 has sourceId 15,393,162,799,074. Handing both arms `17049`
        // does not fail — it selects a DIFFERENT ENTITY on one of them, and
        // the comparison table looks perfectly healthy.
        //
        // One file, both identifiers, the dialect choosing. The query text is
        // NOT forked: the question is the same, only the spelling of the
        // subject differs.
        let specs = [spec("personId", "ID")];
        let mut supplied = BTreeMap::new();
        supplied.insert("personId".into(), "17049".into());
        supplied.insert("personId@ldbc".into(), "15393162799074".into());

        let dense = bind(
            "bi1",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Dense,
        )
        .unwrap();
        assert_eq!(dense["personId"], Value::Int(17049));

        let ldbc = bind(
            "bi1",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Ldbc,
        )
        .unwrap();
        assert_eq!(ldbc["personId"], Value::Int(15_393_162_799_074));

        // The companion is declared by its BASE name, so it must not trip the
        // undeclared-parameter refusal.
        assert!(
            bind(
                "bi1",
                &specs,
                &supplied,
                TemporalEncoding::Typed,
                IdSpace::Dense
            )
            .is_ok()
        );
    }

    #[test]
    fn a_parameter_without_a_companion_is_the_same_on_every_arm() {
        // Names and dates mean one thing everywhere and carry no second form.
        // Asking for the LDBC space must fall back, not fail.
        let specs = [spec("tagA", "STRING")];
        let mut supplied = BTreeMap::new();
        supplied.insert("tagA".into(), "Louis_IX_of_France".into());
        let v = bind(
            "bi16",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Ldbc,
        )
        .unwrap();
        assert_eq!(v["tagA"], Value::Str("Louis_IX_of_France".into()));
    }

    #[test]
    fn a_parameter_nobody_declared_is_refused() {
        // A typo in the parameter file would otherwise bind nothing and leave
        // the real parameter missing — or, worse, silently unused.
        let specs = [spec("tagA", "STRING")];
        let mut supplied = BTreeMap::new();
        supplied.insert("tagA".to_string(), "x".to_string());
        supplied.insert("tagB".to_string(), "y".to_string());
        let e = bind(
            "bi16",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Dense,
        )
        .unwrap_err();
        assert!(e.contains("tagB"), "{e}");
    }

    #[test]
    fn a_complete_binding_carries_every_declared_type() {
        let specs = [
            spec("startDate", "DATE"),
            spec("lengthThreshold", "INT"),
            spec("languages", "STRING[]"),
        ];
        let mut supplied = BTreeMap::new();
        supplied.insert("startDate".into(), "2012-09-16".into());
        supplied.insert("lengthThreshold".into(), "20".into());
        supplied.insert("languages".into(), "uz;tk".into());
        let bound = bind(
            "bi12",
            &specs,
            &supplied,
            TemporalEncoding::Typed,
            IdSpace::Dense,
        )
        .unwrap();
        assert_eq!(bound["startDate"], Value::Date(15_599));
        assert_eq!(bound["lengthThreshold"], Value::Int(20));
        assert!(matches!(bound["languages"], Value::List(_)));
    }
}
