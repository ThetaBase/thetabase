//! Turning a Postgres value into a ThetaBase [`Value`].
//!
//! Every column is read as **text** rather than through a typed getter, and
//! then parsed according to the type the reflection reported. That looks like
//! the long way round and is deliberate:
//!
//! * Postgres has far more types than `tokio_postgres` has `FromSql` impls, and
//!   a migration that fell over on the first user-defined enum would migrate
//!   nothing. `::text` is defined for every type there is.
//! * It keeps one code path. A typed getter for the types we know plus a text
//!   fallback for the rest would mean two ways for a value to arrive, and the
//!   verification pass would have to trust both.
//! * The text rendering is what an unmappable type keeps anyway, so the
//!   fallback and the primary path agree by construction.
//!
//! Parsing is strict. A value that does not parse as its declared type is
//! reported, never coerced and never dropped — invariant 3, and the whole
//! reason a migration has a verification pass at all.

use std::collections::BTreeMap;

use theta_core::{Value, ValueType};

use crate::EjectError;

/// Parse one column's text rendering into a lattice value.
///
/// `None` input is SQL `NULL`, which becomes [`Value::Null`] regardless of the
/// declared type — nullability lives on the field, not on the type.
pub fn parse(
    text: Option<&str>,
    ty: ValueType,
    table: &str,
    column: &str,
) -> Result<Value, EjectError> {
    let Some(text) = text else {
        return Ok(Value::Null);
    };

    let bad = |expected: &str| EjectError::Unparseable {
        table: table.to_string(),
        column: column.to_string(),
        expected: expected.to_string(),
        // Truncated: an error message is not the place to reproduce a row, and
        // a column can hold a megabyte.
        found: text.chars().take(120).collect(),
    };

    Ok(match ty {
        ValueType::Bool => match text {
            "t" | "true" | "TRUE" | "yes" | "on" | "1" => Value::Bool(true),
            "f" | "false" | "FALSE" | "no" | "off" | "0" => Value::Bool(false),
            _ => return Err(bad("bool")),
        },
        ValueType::Int => Value::Int(text.parse::<i64>().map_err(|_| bad("int8"))?),
        ValueType::Float => Value::Float(text.parse::<f64>().map_err(|_| bad("float8"))?),
        ValueType::Text => Value::Text(text.to_string()),
        ValueType::Bytes => Value::Bytes(parse_bytea(text).ok_or_else(|| bad("bytea"))?),
        ValueType::Timestamp => {
            Value::Timestamp(parse_timestamp(text).ok_or_else(|| bad("an ISO-8601 timestamp"))?)
        }
        ValueType::Map => {
            // json/jsonb arrives as its own text form, which is exactly what
            // serde_json reads.
            let json: serde_json::Value = serde_json::from_str(text).map_err(|_| bad("json"))?;
            from_json(json)
        }
        ValueType::List => Value::List(parse_array(text)),
        // Nothing maps to Null, so reaching here means the mapping produced a
        // type it should not have.
        ValueType::Null => Value::Null,
    })
}

/// Postgres renders `bytea` as `\x` followed by hex, under the default
/// `bytea_output = hex`.
fn parse_bytea(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Milliseconds since the epoch, from Postgres's ISO-8601 rendering.
///
/// Hand-parsed rather than pulling in a date library: the input shape is fixed
/// by `DateStyle = ISO`, which `eject` sets on the session precisely so that it
/// is. Sub-millisecond precision is discarded, and that loss is reported by the
/// type mapping rather than hidden here.
fn parse_timestamp(text: &str) -> Option<i64> {
    // `2024-01-31 12:34:56.789+00`, or with a `T`, or with no zone — and a
    // `date` column renders with no time part at all. That last one becomes
    // midnight UTC, which is precisely the precision the type mapping warns a
    // `date` gains on the way across.
    let text = text.trim();
    let (date, rest) = match text.split_once(['T', ' ']) {
        Some(split) => split,
        None => (text, "00:00:00"),
    };

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    // Strip the zone. Anything not UTC would need real timezone handling, and
    // `eject` sets the session to UTC so there is nothing to convert.
    let time = rest
        .split_once('+')
        .map(|(t, _)| t)
        .or_else(|| rest.rsplit_once('-').map(|(t, _)| t))
        .unwrap_or(rest)
        .trim_end_matches('Z');

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let seconds = time_parts.next().unwrap_or("0");
    let (whole, frac) = match seconds.split_once('.') {
        Some((w, f)) => (w, f),
        None => (seconds, ""),
    };
    let second: i64 = whole.parse().ok()?;
    // Pad or truncate to milliseconds.
    let millis: i64 = if frac.is_empty() {
        0
    } else {
        let mut f = frac.to_string();
        f.truncate(3);
        while f.len() < 3 {
            f.push('0');
        }
        f.parse().ok()?
    };

    Some(
        days_from_civil(year, month, day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + millis,
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which is exact for the whole range and
/// avoids a dependency for one function.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Postgres array literal: `{a,b,"c,d",NULL}`.
///
/// Elements are kept as text. Typing them would mean re-deriving the element
/// type and re-entering this parser, and a list of mixed-looking text is closer
/// to what the source held than a list of guesses.
fn parse_array(text: &str) -> Vec<Value> {
    let inner = text
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(text);
    if inner.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut inside_quotes = false;
    let mut escaped = false;
    // Whether *this element* was ever quoted, which is the only thing that
    // distinguishes the null element from the four-character string "NULL".
    // Tracked separately from `inside_quotes`, because by the time the element
    // ends the quotes have been consumed and the text alone cannot say.
    let mut was_quoted = false;

    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            inside_quotes = !inside_quotes;
            was_quoted = true;
        } else if ch == ',' && !inside_quotes {
            out.push(array_element(std::mem::take(&mut current), was_quoted));
            current = String::new();
            was_quoted = false;
        } else {
            current.push(ch);
        }
    }
    out.push(array_element(current, was_quoted));
    out
}

fn array_element(raw: String, was_quoted: bool) -> Value {
    // An unquoted NULL is the null element; a quoted "NULL" is the string.
    if raw == "NULL" && !was_quoted {
        Value::Null
    } else {
        Value::Text(raw)
    }
}

fn from_json(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => Value::Text(s),
        serde_json::Value::Array(items) => Value::List(items.into_iter().map(from_json).collect()),
        serde_json::Value::Object(fields) => Value::Map(
            fields
                .into_iter()
                .map(|(k, v)| (k, from_json(v)))
                .collect::<BTreeMap<_, _>>(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str, ty: ValueType) -> Value {
        parse(Some(text), ty, "t", "c").expect("parses")
    }

    #[test]
    fn null_is_null_whatever_the_declared_type() {
        for ty in [ValueType::Int, ValueType::Text, ValueType::Timestamp] {
            assert_eq!(parse(None, ty, "t", "c").unwrap(), Value::Null);
        }
    }

    #[test]
    fn postgres_boolean_spellings_all_parse() {
        assert_eq!(p("t", ValueType::Bool), Value::Bool(true));
        assert_eq!(p("f", ValueType::Bool), Value::Bool(false));
        assert_eq!(p("true", ValueType::Bool), Value::Bool(true));
    }

    #[test]
    fn a_value_that_is_not_its_declared_type_is_reported_not_coerced() {
        // Invariant 3. Coercing here would produce a migration that "succeeded"
        // and changed the data.
        let err = parse(Some("not-a-number"), ValueType::Int, "users", "age");
        let err = err.expect_err("must not parse");
        let rendered = err.to_string();
        assert!(rendered.contains("users"), "got: {rendered}");
        assert!(rendered.contains("age"), "got: {rendered}");
    }

    #[test]
    fn bytea_round_trips_through_its_hex_rendering() {
        assert_eq!(
            p("\\xdeadbeef", ValueType::Bytes),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(p("\\x", ValueType::Bytes), Value::Bytes(vec![]));
    }

    #[test]
    fn timestamps_parse_to_epoch_milliseconds() {
        assert_eq!(
            p("1970-01-01 00:00:00+00", ValueType::Timestamp),
            Value::Timestamp(0)
        );
        assert_eq!(
            p("2024-01-31 12:34:56.789+00", ValueType::Timestamp),
            Value::Timestamp(1_706_704_496_789)
        );
        // A `T` separator and a `Z` zone are the same instant.
        assert_eq!(
            p("2024-01-31T12:34:56.789Z", ValueType::Timestamp),
            Value::Timestamp(1_706_704_496_789)
        );
    }

    #[test]
    fn a_bare_date_becomes_midnight_utc() {
        // A `date` column renders with no time part. Requiring one made every
        // date column unparseable, which failed the migration rather than
        // migrating it - found by running against a real Postgres.
        assert_eq!(
            p("2024-02-01", ValueType::Timestamp),
            Value::Timestamp(1_706_745_600_000)
        );
    }

    #[test]
    fn a_date_before_the_epoch_parses_to_a_negative_instant() {
        assert_eq!(
            p("1969-12-31 00:00:00+00", ValueType::Timestamp),
            Value::Timestamp(-86_400_000)
        );
    }

    #[test]
    fn arrays_split_on_commas_outside_quotes() {
        assert_eq!(
            p("{a,b,c}", ValueType::List),
            Value::List(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into())
            ])
        );
        // A comma inside a quoted element is part of the element, not a
        // separator - the case a naive split gets wrong.
        assert_eq!(
            p("{\"a,b\",c}", ValueType::List),
            Value::List(vec![Value::Text("a,b".into()), Value::Text("c".into())])
        );
        assert_eq!(p("{}", ValueType::List), Value::List(vec![]));
    }

    #[test]
    fn an_unquoted_null_element_is_null_and_a_quoted_one_is_text() {
        assert_eq!(
            p("{NULL,\"NULL\"}", ValueType::List),
            Value::List(vec![Value::Null, Value::Text("NULL".into())])
        );
    }

    #[test]
    fn json_keeps_its_structure() {
        assert_eq!(
            p(r#"{"a":1,"b":["x",true]}"#, ValueType::Map),
            Value::Map(
                [
                    ("a".to_string(), Value::Int(1)),
                    (
                        "b".to_string(),
                        Value::List(vec![Value::Text("x".into()), Value::Bool(true)])
                    ),
                ]
                .into_iter()
                .collect()
            )
        );
    }
}
