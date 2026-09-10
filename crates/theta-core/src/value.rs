//! Values and their canonical types.
//!
//! Spec constraint (`03-data-model-consistency.md` §2.2): the engine never
//! silently reinterprets a value's type after the fact. A write whose value does
//! not match the field's declared type is *rejected*, never coerced — see
//! [`ValueType::accepts`] and the `widens_to` widening lattice, which is the only
//! sanctioned form of type change and is always non-destructive.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// Milliseconds since the Unix epoch, UTC.
    Timestamp(i64),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// The narrowest type that describes this value. Used for type inference in
    /// dynamic (prototyping) mode and for reporting the actual type of a
    /// rejected write.
    pub fn inferred_type(&self) -> ValueType {
        match self {
            Value::Null => ValueType::Null,
            Value::Bool(_) => ValueType::Bool,
            Value::Int(_) => ValueType::Int,
            Value::Float(_) => ValueType::Float,
            Value::Text(_) => ValueType::Text,
            Value::Bytes(_) => ValueType::Bytes,
            Value::Timestamp(_) => ValueType::Timestamp,
            Value::List(_) => ValueType::List,
            Value::Map(_) => ValueType::Map,
        }
    }

    /// Interpret plain JSON as a value.
    ///
    /// # Why this exists
    ///
    /// [`Value`] serialises adjacently tagged — `{"kind":"int","value":30}` —
    /// because a value's type is never inferred after the fact and the encoding
    /// has to say which type was meant. An SDK's caller does not write that
    /// form; they write `30`. Something has to convert, and the whole point of
    /// one Scribe core is that it converts once rather than once per language.
    ///
    /// It exists because it was missing. Query parameters crossed the WASM
    /// boundary as plain JSON and were forwarded to the server unchanged, so the
    /// server refused every parameterised query with "value is not a valid
    /// encoding". The conformance suite had a case for it named *"a SQL literal
    /// is bound and never becomes syntax"* — and it passed, because all three
    /// bindings failed identically and the harness only compared them to each
    /// other.
    ///
    /// # What it cannot produce
    ///
    /// [`Value::Bytes`] and [`Value::Timestamp`] have no plain-JSON form: bytes
    /// would be indistinguishable from a base64 string, and a timestamp from an
    /// integer. Guessing is the coercion `03-data-model-consistency.md` §2.2
    /// forbids, so this does not guess — a number becomes an [`Value::Int`] or a
    /// [`Value::Float`] and nothing else. Comparing against a timestamp column
    /// with an epoch-millisecond `Int` is the working path today; a parameter
    /// that must *be* a timestamp needs the tagged encoding, and giving the
    /// builders a way to say so is open work.
    pub fn from_json(json: &serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                // Integers stay integers. Widening every number to a float
                // would make `WHERE id = 9007199254740993` match the wrong row,
                // silently, above 2^53 — the same trap the TypeScript binding
                // avoids by using bigint.
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Value::Text(s.clone()),
            serde_json::Value::Array(items) => {
                Value::List(items.iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(fields) => Value::Map(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Render as plain JSON, for a caller that wrote plain JSON.
    ///
    /// The inverse of [`Value::from_json`] for everything a host can express,
    /// and the read half of the same problem: a host that writes
    /// `{"email": "..."}` and reads back
    /// `{"kind":"map","value":{"email":{"kind":"text","value":"..."}}}` has not
    /// round-tripped its own row.
    ///
    /// # Two types do not survive the trip
    ///
    /// [`Value::Bytes`] renders as a base64 string and [`Value::Timestamp`] as
    /// an integer, and [`Value::from_json`] will read those back as a
    /// [`Value::Text`] and a [`Value::Int`]. That is a real loss and it is
    /// stated rather than papered over: the fix is a typed accessor on each
    /// SDK's read path, which is open work, and not a tagged encoding leaking
    /// into an API whose other ninety per cent is plain JSON.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                // NaN and the infinities have no JSON form. Null rather than a
                // string: a reader that has to guess whether "NaN" is a number
                // or a word is worse off than one told there is nothing there.
                .unwrap_or(serde_json::Value::Null),
            Value::Text(s) => serde_json::Value::String(s.clone()),
            Value::Bytes(bytes) => serde_json::Value::String(base64(bytes)),
            Value::Timestamp(ms) => serde_json::Value::from(*ms),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(Value::to_json).collect())
            }
            Value::Map(fields) => serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
        }
    }
}

/// Standard base64, no padding omitted.
///
/// Hand-rolled rather than pulling `base64` into `theta-core`. This is the only
/// caller, it is twenty lines, and the crate is on the hot path — a dependency
/// added for one function is one more thing in the closure the no-LLM test walks.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    Null,
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    Timestamp,
    List,
    Map,
}

impl ValueType {
    /// Whether a value of this declared type accepts `value` as written.
    ///
    /// Deliberately strict: `Int` does not accept a `Float`, `Text` does not
    /// accept an `Int`. Prevention over silent correction
    /// (`07-agent-safety-layer.md` §2).
    pub fn accepts(self, value: &Value) -> bool {
        // Null is the absence of a value and is accepted by every nullable field;
        // nullability is tracked on the field, not the type.
        matches!(value, Value::Null) || value.inferred_type() == self
    }

    /// Whether changing a field from `self` to `target` is lossless. Lossless
    /// widenings classify as non-destructive; every other type change is
    /// destructive by default (`07-agent-safety-layer.md` §3).
    pub fn widens_to(self, target: ValueType) -> bool {
        use ValueType::*;
        if self == target {
            return true;
        }
        matches!(
            (self, target),
            (Null, _) | (Int, Float) | (Int, Text) | (Bool, Text) | (Timestamp, Int)
        )
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ValueType::Null => "null",
            ValueType::Bool => "bool",
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Text => "text",
            ValueType::Bytes => "bytes",
            ValueType::Timestamp => "timestamp",
            ValueType::List => "list",
            ValueType::Map => "map",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_types_do_not_coerce() {
        assert!(!ValueType::Int.accepts(&Value::Text("7".into())));
        assert!(!ValueType::Int.accepts(&Value::Float(7.0)));
        assert!(ValueType::Int.accepts(&Value::Int(7)));
    }

    #[test]
    fn nulls_are_accepted_by_any_type() {
        assert!(ValueType::Text.accepts(&Value::Null));
    }

    #[test]
    fn narrowing_is_not_a_widening() {
        assert!(ValueType::Int.widens_to(ValueType::Float));
        assert!(!ValueType::Float.widens_to(ValueType::Int));
        assert!(!ValueType::Text.widens_to(ValueType::Int));
    }

    #[test]
    fn plain_json_becomes_a_value_the_server_accepts() {
        // The bug this was written for: every SDK sent the host's plain JSON as
        // a row, and the server refused it with "missing field `kind`". The
        // conformance suite reported three bindings in perfect agreement.
        let row = Value::from_json(&serde_json::json!({
            "email": "alice@example.com",
            "age": 30,
        }));
        let encoded = serde_json::to_string(&row).expect("encodes");
        assert!(
            encoded.contains("\"kind\""),
            "a row must encode adjacently tagged, or the server cannot read it: {encoded}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).expect("decodes"),
            row,
            "the encoding the server reads did not round-trip"
        );
    }

    #[test]
    fn a_number_stays_an_integer_rather_than_widening() {
        // Above 2^53 a float silently changes value, so `WHERE id = n` would
        // match the wrong row. The same trap the TypeScript binding avoids by
        // using bigint.
        assert_eq!(Value::from_json(&serde_json::json!(30)), Value::Int(30));
        assert_eq!(
            Value::from_json(&serde_json::json!(9007199254740993i64)),
            Value::Int(9007199254740993)
        );
        assert_eq!(Value::from_json(&serde_json::json!(1.5)), Value::Float(1.5));
    }

    #[test]
    fn a_row_written_as_plain_json_reads_back_as_plain_json() {
        // What a host actually needs: write `{"email": ...}`, read
        // `{"email": ...}`. Not the tagged encoding it never wrote.
        let original = serde_json::json!({
            "email": "alice@example.com",
            "tags": ["a", "b"],
            "active": true,
            "score": 1.5,
            "missing": null,
        });
        assert_eq!(Value::from_json(&original).to_json(), original);
    }

    #[test]
    fn bytes_and_timestamps_do_not_survive_the_round_trip() {
        // Asserted rather than assumed, because it is a real loss and the kind
        // that is easy to stop believing. A test that goes red if someone
        // "fixes" it by guessing is the point: guessing is the coercion
        // `03-data-model-consistency.md` §2.2 forbids.
        let stamp = Value::Timestamp(1_700_000_000_000);
        assert_eq!(
            Value::from_json(&stamp.to_json()),
            Value::Int(1_700_000_000_000)
        );

        let bytes = Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(bytes.to_json(), serde_json::json!("3q2+7w=="));
        assert!(matches!(Value::from_json(&bytes.to_json()), Value::Text(_)));
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foob"), "Zm9vYg==");
        assert_eq!(super::base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_float_with_no_json_form_becomes_null_rather_than_a_word() {
        assert_eq!(Value::Float(f64::NAN).to_json(), serde_json::Value::Null);
        assert_eq!(
            Value::Float(f64::INFINITY).to_json(),
            serde_json::Value::Null
        );
    }
}
