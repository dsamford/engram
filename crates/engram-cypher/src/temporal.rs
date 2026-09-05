//! Temporal — the calendar arithmetic, ISO parsing and formatting behind
//! `datetime()` (~458 corpus sites) and `duration()` (~63).
//!
//! The calendar kernel is the civil-days algorithm (exact for the proleptic
//! Gregorian calendar, no dependency); zone HANDLING is deliberately
//! honest about its limits: fixed offsets and UTC are computed, named IANA
//! zones are CARRIED but not resolved (no tzdata in the zero-dep core), and
//! a construction that would need tz rules refuses by name instead of
//! guessing an offset.

/// Days from civil date (proleptic Gregorian) — Hinnant's algorithm.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Civil date from days since the epoch.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO-8601 date in any of the calendar, ordinal, or week forms, extended
/// (`-`-separated) OR basic (separator-less): `YYYY-MM-DD`, `YYYYMMDD`,
/// `YYYY-MM`, `YYYY`, `YYYY-DDD`/`YYYYDDD` (ordinal), `YYYY-Www-D`/`YYYYWwwD`
/// (ISO week), and the truncated `YYYY-Www`/`YYYY-MM` forms.
pub fn parse_date(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    // Extended (`-`-separated) years may exceed four digits (`-999999999-01-01`);
    // a basic, separator-less string pins the year to the first four so the trailing
    // digits stay available as month/day/ordinal fields.
    let lead: usize = body.bytes().take_while(u8::is_ascii_digit).count();
    if lead < 4 {
        return None;
    }
    let year_len = if body[lead..].starts_with('-') {
        lead
    } else {
        4
    };
    let y0: i64 = body[..year_len].parse().ok()?;
    let y = if neg { -y0 } else { y0 };
    let rest = &body[year_len..];

    // Week date: [-]Www with an optional [-]D weekday (default Monday).
    if let Some(w) = rest.strip_prefix("-W").or_else(|| rest.strip_prefix('W')) {
        let (week, r) = take_2(w)?;
        let (dow, r) = match r.strip_prefix('-') {
            Some(r2) => take_1(r2)?,
            None if r.as_bytes().first().is_some_and(u8::is_ascii_digit) => take_1(r)?,
            None => (1, r),
        };
        if !r.is_empty() || !(1..=53).contains(&week) || !(1..=7).contains(&dow) {
            return None;
        }
        let jan4 = days_from_civil(y, 1, 4);
        let week1_monday = jan4 - (jan4 + 3).rem_euclid(7);
        return Some(week1_monday + i64::from(week - 1) * 7 + i64::from(dow - 1));
    }

    if rest.is_empty() {
        return Some(days_from_civil(y, 1, 1)); // year only → Jan 1
    }

    let extended = rest.starts_with('-');
    let rest = if extended { &rest[1..] } else { rest };

    // Ordinal day-of-year: exactly three digits, nothing after.
    if rest.len() == 3 && rest.bytes().all(|b| b.is_ascii_digit()) {
        let doy: i64 = rest.parse().ok()?;
        if !(1..=366).contains(&doy) {
            return None;
        }
        return Some(days_from_civil(y, 1, 1) + doy - 1);
    }

    // Calendar month[-day] (day defaults to the 1st when omitted).
    let (m, r) = take_2(rest)?;
    let (d, r) = if r.is_empty() {
        (1, r)
    } else if extended {
        take_2(r.strip_prefix('-')?)?
    } else {
        take_2(r)?
    };
    if !r.is_empty() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

fn take_1(s: &str) -> Option<(u32, &str)> {
    let b = *s.as_bytes().first()?;
    if !b.is_ascii_digit() {
        return None;
    }
    Some((u32::from(b - b'0'), &s[1..]))
}

fn take_2(s: &str) -> Option<(u32, &str)> {
    if s.len() < 2 || !s.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((s[..2].parse().ok()?, &s[2..]))
}

/// Time of day: `HH[:MM[:SS[.fffffffff]]]` (extended) or `HHMMSS.fff` (basic) →
/// nanos since midnight, plus any unconsumed tail (a zone suffix). Minutes and
/// seconds continue on a `:` or, in the basic form, directly on a digit; a
/// `Z`/`+`/`-`/`[`/end terminates the time and is left for the zone parser.
pub fn parse_time_of_day(s: &str) -> Option<(i64, &str)> {
    let (h, rest) = take_2(s)?;
    if h > 23 {
        return None;
    }
    let mut nanos = i64::from(h) * 3_600_000_000_000;
    let (mm, colon, mut rest) = match rest.strip_prefix(':') {
        Some(r) => {
            let (m, r) = take_2(r)?;
            (Some(m), true, r)
        }
        None if rest.as_bytes().first().is_some_and(u8::is_ascii_digit) => {
            let (m, r) = take_2(rest)?;
            (Some(m), false, r)
        }
        None => (None, false, rest),
    };
    if let Some(m) = mm {
        if m > 59 {
            return None;
        }
        nanos += i64::from(m) * 60_000_000_000;
        let sec_rest = if colon {
            rest.strip_prefix(':')
        } else if rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            Some(rest)
        } else {
            None
        };
        if let Some(r) = sec_rest {
            let (sec, r) = take_2(r)?;
            if sec > 59 {
                return None;
            }
            nanos += i64::from(sec) * 1_000_000_000;
            rest = r;
            if let Some(r) = rest.strip_prefix('.') {
                let digits: usize = r.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 || digits > 9 {
                    return None;
                }
                let frac: i64 = r[..digits].parse().ok()?;
                nanos += frac * 10i64.pow(9 - digits as u32);
                rest = &r[digits..];
            }
        }
    }
    Some((nanos, rest))
}

/// A zone suffix: `Z`, `+HH:MM`, `-HH[:]MM`, `+HH`, or a bracketed zone id
/// (`[America/New_York]` after an offset, as ISO/Neo4j prints). Returns
/// (offset seconds, zone id, rest).
pub fn parse_zone(s: &str) -> Option<(Option<i32>, Option<String>, &str)> {
    let (offset, rest) = if let Some(r) = s.strip_prefix('Z') {
        (Some(0), r)
    } else if let Some(sign) = s.chars().next().filter(|c| *c == '+' || *c == '-') {
        let r = &s[1..];
        let (h, r) = take_2(r)?;
        let (m, r) = match r.strip_prefix(':') {
            Some(r2) => take_2(r2)?,
            None if r.len() >= 2 && r.as_bytes()[..2].iter().all(u8::is_ascii_digit) => take_2(r)?,
            None => (0, r),
        };
        // Optional second-precision component (`+HH:MM:SS`, ISO/Neo4j) — offsets
        // are usually minute-aligned but the historical ones are not.
        let (sec, r) = match r.strip_prefix(':') {
            Some(r2) => take_2(r2)?,
            None if r.len() >= 2 && r.as_bytes()[..2].iter().all(u8::is_ascii_digit) => take_2(r)?,
            None => (0, r),
        };
        let secs = (h as i32) * 3600 + (m as i32) * 60 + (sec as i32);
        (Some(if sign == '-' { -secs } else { secs }), r)
    } else {
        (None, s)
    };
    if let Some(r) = rest.strip_prefix('[') {
        let end = r.find(']')?;
        return Some((offset, Some(r[..end].to_string()), &r[end + 1..]));
    }
    Some((offset, None, rest))
}

/// Average Gregorian month, in seconds (365.2425/12 days) — how Neo4j cascades a
/// fractional month/year of a duration down into the second field.
const AVG_MONTH_SECS: i128 = 2_629_746;

/// A signed number `[-]digits[.digits]`: whole part, and the fraction expressed
/// as nanoseconds-of-one-unit (`frac_int * 10^(9-fd)`, in `[0, 1e9)`), then the
/// tail. Both carry the component's own sign.
fn parse_signed(s: &str) -> Option<(i64, i64, &str)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let whole: i64 = s[..digits].parse().ok()?;
    let mut r = &s[digits..];
    let mut frac_nanos = 0i64;
    if let Some(fr) = r.strip_prefix('.') {
        let fd = fr.bytes().take_while(u8::is_ascii_digit).count();
        if fd == 0 || fd > 9 {
            return None;
        }
        frac_nanos = fr[..fd].parse::<i64>().ok()? * 10i64.pow(9 - fd as u32);
        r = &fr[fd..];
    }
    let sign = if neg { -1 } else { 1 };
    Some((sign * whole, sign * frac_nanos, r))
}

/// ISO-8601 duration: the unit form `PnYnMnWnDTnHnMnS` (each component may be
/// signed and may carry a fraction that cascades into seconds), or the
/// alternative date-time form `PYYYY-MM-DDThh:mm:ss`. Returns (months, days,
/// seconds, nanos). All accumulation is exact integer arithmetic (i128) — a
/// fractional month is `frac × 2_629_746 s`, a fractional day `frac × 86_400 s`.
pub fn parse_duration(s: &str) -> Option<(i64, i64, i64, i32)> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let body = body.strip_prefix('P')?;
    let (mut months, mut days) = (0i64, 0i64);
    // Two exact nanosecond accumulators. A fractional YEAR/MONTH/WEEK/DAY cascades
    // into `date_frac_ns` — from which WHOLE days promote to the `days` field
    // (`P0.75M` = 22 days + 19h…, not 547h). Time components stay in `time_ns` and
    // NEVER promote to days (a duration keeps days and seconds distinct).
    const DAY_NS: i128 = 86_400 * 1_000_000_000;
    let mut date_frac_ns: i128 = 0;
    let mut time_ns: i128 = 0;

    // Alternative form: a `YYYY-MM-DD` date part (four year digits then `-`)
    // splits at `T` from an `hh:mm:ss` time part; the fields are literal
    // years/months/days & hours/minutes/seconds, not a calendar instant.
    if body.len() >= 5
        && body.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && body.as_bytes()[4] == b'-'
    {
        let (dp, tp) = body.split_once('T')?;
        let mut dparts = dp.split('-');
        let y: i64 = dparts.next()?.parse().ok()?;
        let mo: i64 = dparts.next()?.parse().ok()?;
        let d: i64 = dparts.next()?.parse().ok()?;
        if dparts.next().is_some() {
            return None;
        }
        months = y * 12 + mo;
        days = d;
        let mut tparts = tp.split(':');
        let h: i64 = tparts.next()?.parse().ok()?;
        let mi: i64 = tparts.next()?.parse().ok()?;
        let (sw, sf, r) = parse_signed(tparts.next()?)?;
        if r.is_empty() && tparts.next().is_none() {
            time_ns = (i128::from(h) * 3600 + i128::from(mi) * 60 + i128::from(sw)) * 1_000_000_000
                + i128::from(sf);
        } else {
            return None;
        }
    } else {
        let mut rest = body;
        let mut in_time = false;
        let mut any = false;
        while !rest.is_empty() {
            if !in_time {
                if let Some(r) = rest.strip_prefix('T') {
                    in_time = true;
                    rest = r;
                    continue;
                }
            }
            let (whole, frac_ns, r) = parse_signed(rest)?;
            let mut r = r;
            let unit = r.chars().next()?;
            r = &r[unit.len_utf8()..];
            let fns = i128::from(frac_ns);
            match (in_time, unit) {
                (false, 'Y') => {
                    months += whole * 12;
                    date_frac_ns += fns * 12 * AVG_MONTH_SECS;
                }
                (false, 'M') => {
                    months += whole;
                    date_frac_ns += fns * AVG_MONTH_SECS;
                }
                (false, 'W') => {
                    days += whole * 7;
                    date_frac_ns += fns * 7 * 86_400;
                }
                (false, 'D') => {
                    days += whole;
                    date_frac_ns += fns * 86_400;
                }
                (true, 'H') => time_ns += (i128::from(whole) * 3600) * 1_000_000_000 + fns * 3600,
                (true, 'M') => time_ns += (i128::from(whole) * 60) * 1_000_000_000 + fns * 60,
                (true, 'S') => time_ns += i128::from(whole) * 1_000_000_000 + fns,
                _ => return None,
            }
            any = true;
            rest = r;
        }
        if !any {
            return None;
        }
    }

    // Apply a whole-duration `-` before splitting.
    if neg {
        months = -months;
        days = -days;
        date_frac_ns = -date_frac_ns;
        time_ns = -time_ns;
    }
    // Promote whole days out of the date cascade (truncating, so the day and the
    // sub-day remainder keep one sign); the time part is added to the remainder.
    days += (date_frac_ns / DAY_NS) as i64;
    let total_ns = date_frac_ns % DAY_NS + time_ns;
    let seconds = total_ns.div_euclid(1_000_000_000) as i64;
    let nanos = total_ns.rem_euclid(1_000_000_000) as i32;
    Some((months, days, seconds, nanos))
}

/// Format days-since-epoch as `YYYY-MM-DD`.
pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    if y >= 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!("-{:04}-{m:02}-{d:02}", -y)
    }
}

/// Format nanos-since-midnight as `HH:MM:SS[.fffffffff]` (trailing zeros
/// trimmed to millisecond groups, as Neo4j prints).
pub fn format_time_of_day(nanos: i64) -> String {
    let h = nanos / 3_600_000_000_000;
    let m = (nanos / 60_000_000_000) % 60;
    let s = (nanos / 1_000_000_000) % 60;
    let n = nanos % 1_000_000_000;
    if s == 0 && n == 0 {
        // openCypher canonical form omits the seconds field when both seconds
        // and sub-seconds are zero (`12:00`, not `12:00:00`).
        format!("{h:02}:{m:02}")
    } else if n == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else if n % 1_000_000 == 0 {
        format!("{h:02}:{m:02}:{s:02}.{:03}", n / 1_000_000)
    } else if n % 1_000 == 0 {
        format!("{h:02}:{m:02}:{s:02}.{:06}", n / 1_000)
    } else {
        format!("{h:02}:{m:02}:{s:02}.{n:09}")
    }
}

/// Format a UTC offset as `Z`, `±HH:MM`, or `±HH:MM:SS` when the offset carries
/// non-zero seconds (Neo4j prints the seconds field only then).
pub fn format_offset(seconds: i32) -> String {
    if seconds == 0 {
        return "Z".to_string();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let a = seconds.abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}

/// Format a duration as ISO-8601, Neo4j style (`P1M2DT3.5S`).
pub fn format_duration(months: i64, days: i64, seconds: i64, nanos: i32) -> String {
    if months == 0 && days == 0 && seconds == 0 && nanos == 0 {
        return "PT0S".to_string();
    }
    let mut out = String::from("P");
    if months != 0 {
        let y = months / 12;
        let m = months % 12;
        if y != 0 {
            out.push_str(&format!("{y}Y"));
        }
        if m != 0 {
            out.push_str(&format!("{m}M"));
        }
    }
    if days != 0 {
        out.push_str(&format!("{days}D"));
    }
    if seconds != 0 || nanos != 0 {
        out.push('T');
        // Split from the COMBINED signed total, not the seconds field alone: storage
        // keeps nanos ≥ 0 (euclidean), so `-86400 s + 100000000 ns` is `-86399.9 s`
        // and must render `-23H-59M-59.9S`, never `-24H0.1S`. Truncating division
        // then gives H, M, and the within-minute remainder all one consistent sign.
        let total_ns = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
        let h = total_ns / 3_600_000_000_000;
        let mi = total_ns % 3_600_000_000_000 / 60_000_000_000;
        let sub = total_ns % 60_000_000_000;
        if h != 0 {
            out.push_str(&format!("{h}H"));
        }
        if mi != 0 {
            out.push_str(&format!("{mi}M"));
        }
        if sub != 0 {
            let sign = if sub < 0 { "-" } else { "" };
            let abs = sub.unsigned_abs();
            let (whole, frac_ns) = (abs / 1_000_000_000, abs % 1_000_000_000);
            if frac_ns == 0 {
                out.push_str(&format!("{sign}{whole}S"));
            } else {
                let mut frac = format!("{frac_ns:09}");
                while frac.ends_with('0') {
                    frac.pop();
                }
                out.push_str(&format!("{sign}{whole}.{frac}S"));
            }
        }
    }
    out
}

// ─── The zone provider seam ─────────────────────────────────────────────────

/// Timezone RULES are a dependency, injected like time itself (D1). The
/// zero-dep core resolves only what needs no database: UTC and the fixed
/// Etc/GMT family. Anything else refuses BY NAME unless the embedder
/// installs a provider that knows tzdata — a guessed offset is wrong half
/// the year, which is worse than a refusal.
// `Send + Sync` because a `Graph` (which caches an installed provider) is now
// `Send + Sync` for the concurrent-write program; the provider it holds must be
// too. Every impl is a stateless resolver, so this costs nothing.
pub trait ZoneProvider: Send + Sync {
    /// The UTC offset (seconds) for `zone` at the given LOCAL wall-clock
    /// time (seconds since the epoch read as wall time). Local-time lookup
    /// is the construction case; a provider resolves fold ambiguity by its
    /// own documented rule. `None` = this provider does not know the zone.
    fn resolve(&self, zone: &str, local_seconds: i64) -> Option<i32>;
}

/// The built-in provider: UTC spellings and the fixed `Etc/GMT±N` family.
///
/// NOTE the POSIX SIGN INVERSION, faithfully kept: `Etc/GMT+5` is UTC-5.
/// Getting this backwards is the classic tz bug, so the table is explicit
/// and the test pins both directions.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixedZones;

/// The ordinal day of the last Sunday of `(year, month)`.
fn last_sunday(year: i64, month: u32) -> i64 {
    let first_next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    let last = first_next - 1;
    let dow = (last + 3).rem_euclid(7); // Monday = 0 … Sunday = 6
    last - (dow + 1).rem_euclid(7)
}

/// The ordinal day of the `n`-th (1-based) Sunday of `(year, month)`.
fn nth_sunday(year: i64, month: u32, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let dow = (first + 3).rem_euclid(7); // Monday = 0 … Sunday = 6
    first + (6 - dow).rem_euclid(7) + (n - 1) * 7
}

/// A small built-in set of IANA zones covering the openCypher TCK — the
/// fixed-offset ones plus the EU / US daylight-saving families, resolved by
/// their standard transition rules (EU: last Sunday of March→October; US: 2nd
/// Sunday of March→1st Sunday of November), compared at day granularity. This
/// keeps named-zone support dependency-free; a full tzdata `ZoneProvider` can
/// still be injected for anything outside this set.
#[derive(Debug, Clone, Copy, Default)]
pub struct IanaZones;

impl ZoneProvider for IanaZones {
    fn resolve(&self, zone: &str, local_seconds: i64) -> Option<i32> {
        let day = local_seconds.div_euclid(86_400);
        let (year, _, _) = civil_from_days(day);
        // The EEC/EU rule ended summer time on the last Sunday of SEPTEMBER until
        // 1996, when it moved to October; the start has been the last Sunday of
        // March throughout the modern era.
        let eu_end_month = if year < 1996 { 9 } else { 10 };
        let eu_dst = day >= last_sunday(year, 3) && day < last_sunday(year, eu_end_month);
        let us_dst = day >= nth_sunday(year, 3, 2) && day < nth_sunday(year, 11, 1);
        Some(match zone {
            "Pacific/Honolulu" => -36_000,
            "Australia/Eucla" => 31_500,
            "Europe/Stockholm" => {
                if eu_dst {
                    7_200
                } else {
                    3_600
                }
            }
            "Europe/London" => {
                if eu_dst {
                    3_600
                } else {
                    0
                }
            }
            "America/New_York" => {
                if us_dst {
                    -14_400
                } else {
                    -18_000
                }
            }
            _ => return None,
        })
    }
}

impl ZoneProvider for FixedZones {
    fn resolve(&self, zone: &str, _local_seconds: i64) -> Option<i32> {
        match zone {
            "UTC" | "Etc/UTC" | "Etc/Universal" | "Universal" | "Z" | "UT" | "GMT" | "Etc/GMT"
            | "Etc/GMT0" | "GMT0" => Some(0),
            _ => {
                let rest = zone.strip_prefix("Etc/GMT")?;
                let (sign, digits) = match rest.as_bytes().first()? {
                    b'+' => (-1i32, &rest[1..]), // POSIX inversion: GMT+N is west
                    b'-' => (1i32, &rest[1..]),
                    _ => return None,
                };
                let n: i32 = digits.parse().ok()?;
                if n > 14 {
                    return None;
                }
                Some(sign * n * 3600)
            }
        }
    }
}
