#![allow(non_snake_case)]
//! Temporal — the calendar kernel, ISO round trips, arithmetic, components.

use engram_cypher::temporal::{civil_from_days, days_from_civil, parse_duration};
use engram_cypher::{
    EvalError, Scope, Value, eval, eval_with, parse_expression, temporal_to_string,
};

fn v(src: &str) -> Value {
    eval(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &Scope::default(),
    )
    .unwrap_or_else(|e| panic!("eval `{src}`: {e}"))
}

fn v_at(src: &str, now_ms: i64) -> Value {
    let scope = Scope {
        now_ms: Some(now_ms),
        ..Scope::default()
    };
    eval_with(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &scope,
        None,
    )
    .unwrap_or_else(|e| panic!("eval `{src}`: {e}"))
}

// ─── The calendar kernel ────────────────────────────────────────────────────

#[test]
fn civil_days_known_values() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(1970, 1, 2), 1);
    assert_eq!(days_from_civil(1969, 12, 31), -1);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(days_from_civil(2026, 8, 19), 20_684);
    assert_eq!(civil_from_days(20_684), (2026, 8, 19));
}

#[test]
fn civil_days_round_trips_across_leap_boundaries() {
    // Every day of four years spanning a leap year and a century rule.
    for base in [days_from_civil(1999, 1, 1), days_from_civil(2099, 1, 1)] {
        for offset in 0..1500 {
            let d = base + offset;
            let (y, m, day) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, day), d, "{y}-{m}-{day}");
        }
    }
    assert_eq!(
        civil_from_days(days_from_civil(2024, 2, 29)),
        (2024, 2, 29),
        "leap day exists"
    );
    assert_eq!(
        civil_from_days(days_from_civil(1900, 2, 28) + 1),
        (1900, 3, 1),
        "1900 is NOT leap"
    );
    assert_eq!(
        civil_from_days(days_from_civil(2000, 2, 28) + 1),
        (2000, 2, 29),
        "2000 IS leap"
    );
}

// ─── Constructors and ISO round trips ───────────────────────────────────────

#[test]
fn datetime_parses_iso_with_every_zone_shape() {
    for (src, secs, off) in [
        ("datetime('2026-08-19T12:00:00Z')", 1_787_140_800i64, 0),
        ("datetime('2026-08-19T12:00:00+02:00')", 1_787_133_600, 7200),
        (
            "datetime('2026-08-19T12:00:00-0530')",
            1_787_160_600,
            -19800,
        ),
        ("datetime('2026-08-19T12:00:00')", 1_787_140_800, 0),
    ] {
        let Value::DateTime {
            epoch_seconds,
            offset_seconds,
            ..
        } = v(src)
        else {
            panic!("{src} did not build a datetime");
        };
        assert_eq!((epoch_seconds, offset_seconds), (secs, off), "{src}");
    }
    // A carried zone id after an offset.
    let Value::DateTime {
        zone,
        offset_seconds,
        ..
    } = v("datetime('2026-08-19T12:00:00+02:00[Europe/Berlin]')")
    else {
        panic!()
    };
    assert_eq!(zone.as_deref(), Some("Europe/Berlin"));
    assert_eq!(offset_seconds, 7200);
    // A NAMED zone the BUNDLED tz database does not know, with no offset,
    // needs an injected provider — refused BY NAME, not guessed.
    let e = eval(
        &parse_expression("datetime('2026-08-19T12:00:00[Fake/Zone]')").expect("parses"),
        &Scope::default(),
    )
    .unwrap_err();
    assert!(matches!(e, EvalError::Function { detail, .. } if detail.contains("tzdata")));
}

#[test]
fn iso_strings_round_trip_through_toString() {
    for iso in ["2026-08-19", "0044-03-15"] {
        assert_eq!(
            v(&format!("toString(date('{iso}'))")),
            Value::Str(iso.into())
        );
    }
    assert_eq!(
        v("toString(datetime('2026-08-19T12:30:15.25+02:00'))"),
        Value::Str("2026-08-19T12:30:15.250+02:00".into())
    );
    // openCypher canonical form omits the seconds field when it is zero.
    assert_eq!(
        v("toString(datetime('2026-08-19T00:00:00Z'))"),
        Value::Str("2026-08-19T00:00Z".into())
    );
    assert_eq!(
        v("toString(duration('P1Y2M3DT4H5M6S'))"),
        Value::Str("P1Y2M3DT4H5M6S".into())
    );
    assert_eq!(
        v("toString(duration('PT0.5S'))"),
        Value::Str("PT0.5S".into())
    );
    assert_eq!(
        v("toString(localtime('09:05'))"),
        Value::Str("09:05".into())
    );
}

#[test]
fn map_constructors_and_epoch_forms() {
    let Value::DateTime {
        epoch_seconds,
        nanos,
        offset_seconds,
        ..
    } = v(
        "datetime({year: 2026, month: 8, day: 19, hour: 12, minute: 30, second: 15, \
         millisecond: 250, timezone: '+02:00'})",
    )
    else {
        panic!()
    };
    assert_eq!(epoch_seconds, 1_787_135_415);
    assert_eq!(nanos, 250_000_000);
    assert_eq!(offset_seconds, 7200);
    assert_eq!(
        v("date({year: 2026, month: 8, day: 19})"),
        Value::Date(20_684)
    );
    let Value::DateTime { epoch_seconds, .. } = v("datetime({epochMillis: 1787140800123})") else {
        panic!()
    };
    assert_eq!(epoch_seconds, 1_787_140_800);
    let Value::DateTime { epoch_seconds, .. } = v("datetime({epochSeconds: 1787140800})") else {
        panic!()
    };
    assert_eq!(epoch_seconds, 1_787_140_800);
}

#[test]
fn now_comes_from_the_INJECTED_clock_or_refuses() {
    // 2026-08-19T12:00:00.123Z
    let ms = 1_787_140_800_123i64;
    let Value::DateTime {
        epoch_seconds,
        nanos,
        ..
    } = v_at("datetime()", ms)
    else {
        panic!()
    };
    assert_eq!((epoch_seconds, nanos), (1_787_140_800, 123_000_000));
    assert_eq!(v_at("timestamp()", ms), Value::Int(ms));
    assert_eq!(v_at("date()", ms), Value::Date(20_684));
    // No clock: a NAMED refusal, never an ambient read.
    let e = eval(
        &parse_expression("datetime()").expect("parses"),
        &Scope::default(),
    )
    .unwrap_err();
    assert!(matches!(e, EvalError::Function { detail, .. } if detail.contains("clock")));
}

#[test]
fn duration_parsing_covers_the_forms() {
    assert_eq!(parse_duration("P1Y2M3DT4H5M6S"), Some((14, 3, 14_706, 0)));
    assert_eq!(parse_duration("P2W"), Some((0, 14, 0, 0)));
    assert_eq!(parse_duration("PT0.5S"), Some((0, 0, 0, 500_000_000)));
    assert_eq!(parse_duration("-P1D"), Some((0, -1, 0, 0)));
    assert_eq!(parse_duration("P"), None, "an empty duration refuses");
    assert_eq!(parse_duration("P1H"), None, "H without T refuses");
    assert_eq!(
        v("duration({days: 1, hours: 2, milliseconds: 1500})"),
        Value::Duration {
            months: 0,
            days: 1,
            seconds: 7201,
            nanos: 500_000_000
        }
    );
}

// ─── Arithmetic ─────────────────────────────────────────────────────────────

#[test]
fn month_addition_clamps_at_month_end() {
    assert_eq!(
        v("date('2026-01-31') + duration('P1M')"),
        v("date('2026-02-28')"),
        "Jan 31 + 1 month clamps to Feb 28"
    );
    assert_eq!(
        v("date('2024-01-31') + duration('P1M')"),
        v("date('2024-02-29')"),
        "…and to Feb 29 in a leap year"
    );
    assert_eq!(
        v("date('2026-08-19') + duration('P1Y')"),
        v("date('2027-08-19')")
    );
}

#[test]
fn datetime_plus_duration_and_the_25_hour_date() {
    assert_eq!(
        v("datetime('2026-08-19T23:30:00Z') + duration('PT45M')"),
        v("datetime('2026-08-20T00:15:00Z')")
    );
    assert_eq!(
        v("date('2026-08-19') + duration('PT25H')"),
        v("date('2026-08-20')"),
        "clock components floor into days on a date"
    );
    assert_eq!(
        v("datetime('2026-08-19T12:00:00Z') - duration('P30D')"),
        v("datetime('2026-07-20T12:00:00Z')")
    );
}

#[test]
fn duration_arithmetic_carries_nanos() {
    assert_eq!(
        v("duration('PT0.7S') + duration('PT0.6S')"),
        Value::Duration {
            months: 0,
            days: 0,
            seconds: 1,
            nanos: 300_000_000
        }
    );
    assert_eq!(
        v("duration('P1D') * 3"),
        Value::Duration {
            months: 0,
            days: 3,
            seconds: 0,
            nanos: 0
        }
    );
    assert_eq!(
        v("-duration('P1M')"),
        Value::Duration {
            months: -1,
            days: 0,
            seconds: 0,
            nanos: 0
        }
    );
}

// ─── Comparison ─────────────────────────────────────────────────────────────

#[test]
fn datetimes_compare_by_INSTANT_across_offsets() {
    assert_eq!(
        v("datetime('2026-08-19T14:00:00+02:00') = datetime('2026-08-19T12:00:00Z')"),
        Value::Bool(true),
        "one instant, two presentations"
    );
    assert_eq!(
        v("datetime('2026-08-19T12:00:00Z') < datetime('2026-08-19T12:00:01Z')"),
        Value::Bool(true)
    );
    assert_eq!(
        v("date('2026-01-01') < date('2026-01-02')"),
        Value::Bool(true)
    );
}

#[test]
fn durations_are_equal_componentwise_and_NOT_orderable() {
    assert_eq!(v("duration('P1M') = duration('P1M')"), Value::Bool(true));
    assert_eq!(
        v("duration('P1M') = duration('P30D')"),
        Value::Bool(false),
        "a month is not 30 days"
    );
    assert_eq!(
        v("duration('P1M') < duration('P30D')"),
        Value::Null,
        "no answer exists"
    );
}

// ─── Components ─────────────────────────────────────────────────────────────

#[test]
fn components_read_through_property_access() {
    let d = "datetime('2026-08-19T12:30:15.25+02:00')";
    assert_eq!(v(&format!("{d}.year")), Value::Int(2026));
    assert_eq!(v(&format!("{d}.month")), Value::Int(8));
    assert_eq!(v(&format!("{d}.day")), Value::Int(19));
    assert_eq!(
        v(&format!("{d}.hour")),
        Value::Int(12),
        "components read in LOCAL time"
    );
    assert_eq!(v(&format!("{d}.minute")), Value::Int(30));
    assert_eq!(v(&format!("{d}.millisecond")), Value::Int(250));
    assert_eq!(v(&format!("{d}.epochSeconds")), Value::Int(1_787_135_415));
    assert_eq!(
        v(&format!("{d}.epochMillis")),
        Value::Int(1_787_135_415_250)
    );
    assert_eq!(v(&format!("{d}.offsetSeconds")), Value::Int(7200));
    assert_eq!(v(&format!("{d}.offset")), Value::Str("+02:00".into()));
    assert_eq!(
        v("date('2026-08-19').dayOfWeek"),
        Value::Int(3),
        "a Wednesday; Monday is 1"
    );
    assert_eq!(v("date('2026-08-19').quarter"), Value::Int(3));
    assert_eq!(v("date('2026-01-01').week"), Value::Int(1));
    assert_eq!(
        v("date('2027-01-01').week"),
        Value::Int(53),
        "2027-01-01 is a Friday — ISO week 53 of 2026"
    );
    assert_eq!(v("duration('P1Y2M3DT4H').months"), Value::Int(14));
    assert_eq!(v("duration('P1Y2M3DT4H').hours"), Value::Int(4));
    // An unknown component refuses by name.
    let e = eval(
        &parse_expression("date('2026-01-01').hour").expect("p"),
        &Scope::default(),
    )
    .unwrap_err();
    assert!(
        matches!(e, EvalError::Function { .. }),
        "a date has no hour"
    );
}

// ─── Null and rendering ─────────────────────────────────────────────────────

#[test]
fn null_propagates_and_json_renders_iso() {
    assert_eq!(v("datetime(null)"), Value::Null);
    assert_eq!(v("duration(null)"), Value::Null);
    assert_eq!(v("date('2026-08-19') + null"), Value::Null);
    assert_eq!(
        v("apoc.convert.toJson({at: datetime('2026-08-19T12:00:00Z')})"),
        Value::Str(r#"{"at":"2026-08-19T12:00Z"}"#.into())
    );
    assert_eq!(temporal_to_string(&Value::Date(0)), "1970-01-01");
}

// ─── The zone provider seam ─────────────────────────────────────────────────

#[test]
fn fixed_zones_resolve_and_the_POSIX_inversion_is_faithful() {
    use engram_cypher::{FixedZones, ZoneProvider};
    let z = FixedZones;
    assert_eq!(z.resolve("UTC", 0), Some(0));
    assert_eq!(z.resolve("Etc/UTC", 0), Some(0));
    // The classic tz bug, pinned in BOTH directions: Etc/GMT+5 is WEST.
    assert_eq!(z.resolve("Etc/GMT+5", 0), Some(-18_000));
    assert_eq!(z.resolve("Etc/GMT-3", 0), Some(10_800));
    assert_eq!(
        z.resolve("Etc/GMT+15", 0),
        None,
        "past the real range refuses"
    );
    assert_eq!(z.resolve("America/New_York", 0), None, "no tzdata here");
}

#[test]
fn a_named_zone_resolves_through_an_INSTALLED_provider_and_refuses_without() {
    use engram_cypher::{Scope, ZoneProvider, eval_with, parse_expression};
    struct TestTz;
    impl ZoneProvider for TestTz {
        fn resolve(&self, zone: &str, _local: i64) -> Option<i32> {
            (zone == "Fake/Zone").then_some(19_800)
        }
    }
    let expr = parse_expression("datetime('2026-08-19T12:00:00[Fake/Zone]')").expect("p");

    // Without a provider: the refusal names the fix.
    let e = eval_with(&expr, &Scope::default(), None).unwrap_err();
    assert!(format!("{e}").contains("ZoneProvider"), "{e}");

    // With one: resolved through the injected rules.
    let scope = Scope {
        zones: Some(std::sync::Arc::new(TestTz)),
        ..Scope::default()
    };
    let Value::DateTime {
        epoch_seconds,
        offset_seconds,
        zone,
        ..
    } = eval_with(&expr, &scope, None).expect("resolves")
    else {
        panic!()
    };
    assert_eq!(offset_seconds, 19_800);
    assert_eq!(
        epoch_seconds,
        1_787_140_800 - 19_800,
        "local noon, five and a half hours east of UTC"
    );
    assert_eq!(zone.as_deref(), Some("Fake/Zone"), "the id is CARRIED");

    // The fixed table cannot be shadowed by a provider: UTC stays UTC.
    struct EvilTz;
    impl ZoneProvider for EvilTz {
        fn resolve(&self, _z: &str, _l: i64) -> Option<i32> {
            Some(3_600)
        }
    }
    let scope = Scope {
        zones: Some(std::sync::Arc::new(EvilTz)),
        ..Scope::default()
    };
    let expr = parse_expression("datetime('2026-08-19T12:00:00[UTC]')").expect("p");
    let Value::DateTime { offset_seconds, .. } = eval_with(&expr, &scope, None).expect("ok") else {
        panic!()
    };
    assert_eq!(offset_seconds, 0, "the built-in table resolves FIRST");
}

#[test]
fn the_map_constructor_takes_a_fixed_zone_name() {
    // Etc/GMT-2 = UTC+2 (the inversion again), through the {timezone: …} path.
    let Value::DateTime {
        epoch_seconds,
        offset_seconds,
        ..
    } = v("datetime({year: 2026, month: 8, day: 19, hour: 12, timezone: 'Etc/GMT-2'})")
    else {
        panic!()
    };
    assert_eq!(offset_seconds, 7_200);
    assert_eq!(epoch_seconds, 1_787_133_600);
}
