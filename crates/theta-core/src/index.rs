//! Order-preserving encoding for secondary indexes.
//!
//! An index has to answer range questions, and [`Value`] has no total order to
//! give it: `compare_values` in the executor is deliberately *partial* — `Null`
//! compares to nothing, and a `Text` compares to no number. Deriving `Ord` over
//! that would mean inventing answers the query semantics refuse to give.
//!
//! So values are encoded to bytes whose byte order matches the executor's value
//! order wherever the executor has one, and the index is keyed on those.
//!
//! **The index may over-approximate and must never under-approximate.** It
//! returns *candidate* rows and the executor re-applies the predicate to them,
//! which is what makes the encoding free to be lossy where being exact would be
//! delicate:
//!
//! * `Int` and `Float` share one encoding, because the executor compares them
//!   to each other. `5` and `5.0` land in the same bucket, which is what
//!   `WHERE x = 5` matching a stored `5.0` requires.
//! * A large `i64` loses precision as `f64`, so two distinct integers can
//!   collide. A collision costs a row that the filter then discards; it cannot
//!   cost a wrong answer.
//! * `Null`, `Map` and `List` are not indexed at all. The executor's comparison
//!   returns nothing for them, so no range predicate can match one, and a
//!   predicate that cannot be answered from the index falls back to a scan.

use crate::Value;

/// Type ranks. Values of different ranks never compare in the executor, so
/// keeping them in disjoint spans of the key order means a range over one rank
/// cannot reach another — the same conclusion the filter reaches.
const RANK_BOOL: u8 = 0x10;
const RANK_NUMBER: u8 = 0x20;
const RANK_TIMESTAMP: u8 = 0x30;
const RANK_TEXT: u8 = 0x40;
const RANK_BYTES: u8 = 0x50;

/// Encode `value` into a byte string whose ordering matches the executor's.
///
/// `None` means the value cannot be indexed, and the caller must scan.
pub fn encode(value: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(9);
    match value {
        Value::Bool(b) => {
            out.push(RANK_BOOL);
            out.push(u8::from(*b));
        }
        // One rank for both, because the executor compares them to each other.
        Value::Int(i) => {
            out.push(RANK_NUMBER);
            out.extend_from_slice(&order_preserving_f64(*i as f64));
        }
        Value::Float(f) => {
            out.push(RANK_NUMBER);
            out.extend_from_slice(&order_preserving_f64(*f));
        }
        Value::Timestamp(t) => {
            out.push(RANK_TIMESTAMP);
            out.extend_from_slice(&order_preserving_i64(*t));
        }
        Value::Text(s) => {
            out.push(RANK_TEXT);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(RANK_BYTES);
            out.extend_from_slice(b);
        }
        // Null compares to nothing, and a Map or List has no ordering the
        // executor will use. Unindexable rather than arbitrarily ordered.
        Value::Null | Value::Map(_) | Value::List(_) => return None,
    }
    Some(out)
}

/// Map an `f64` onto a `u64` whose big-endian bytes sort in the same order.
///
/// The standard transform: flip the sign bit for positives, flip everything for
/// negatives, so that the two's-complement-ish float layout becomes monotonic
/// unsigned. `NaN` sorts to one end, which is harmless — the executor's
/// comparison refuses `NaN` anyway, so the filter drops whatever a range over
/// it collects.
fn order_preserving_f64(value: f64) -> [u8; 8] {
    let bits = value.to_bits();
    let flipped = if bits & (1 << 63) != 0 {
        !bits
    } else {
        bits | (1 << 63)
    };
    flipped.to_be_bytes()
}

fn order_preserving_i64(value: i64) -> [u8; 8] {
    ((value as u64) ^ (1 << 63)).to_be_bytes()
}

/// What an index is being asked for.
///
/// Deliberately smaller than the predicate language: `Ne` and `IsNull` cannot be
/// answered by narrowing a range — they match almost everything or nothing an
/// index holds — so they are not expressible here and their plans scan.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexBound {
    Eq(Value),
    /// Any of these values. Used for `IN`.
    AnyOf(Vec<Value>),
    /// An open or half-open range. `None` means unbounded on that side.
    Range {
        low: Option<(Value, bool)>,
        high: Option<(Value, bool)>,
    },
}

/// One contiguous stretch of the key order to sweep.
///
/// `IN` produces several — one per member — which is why this is a span rather
/// than a single pair of bounds.
pub type EncodedSpan = (std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>);

/// The encoded byte ranges to sweep for a bound.
///
/// `None` means the bound cannot be served from an index and the caller must
/// scan — an unencodable value, or a range whose two ends are different types
/// and so describe nothing the executor would match.
pub fn encoded_range(bound: &IndexBound) -> Option<Vec<EncodedSpan>> {
    use std::ops::Bound as B;
    match bound {
        IndexBound::Eq(value) => {
            let key = encode(value)?;
            Some(vec![(B::Included(key.clone()), B::Included(key))])
        }
        IndexBound::AnyOf(values) => {
            let mut spans = Vec::with_capacity(values.len());
            for value in values {
                // One unencodable member makes the whole set unanswerable: the
                // index cannot tell us which rows it would have matched.
                let key = encode(value)?;
                spans.push((B::Included(key.clone()), B::Included(key)));
            }
            Some(spans)
        }
        IndexBound::Range { low, high } => {
            let start = match low {
                Some((value, inclusive)) => {
                    let key = encode(value)?;
                    if *inclusive {
                        B::Included(key)
                    } else {
                        B::Excluded(key)
                    }
                }
                None => B::Unbounded,
            };
            let end = match high {
                Some((value, inclusive)) => {
                    let key = encode(value)?;
                    if *inclusive {
                        B::Included(key)
                    } else {
                        B::Excluded(key)
                    }
                }
                None => B::Unbounded,
            };
            Some(vec![(start, end)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: Value) -> Vec<u8> {
        encode(&value).expect("encodable")
    }

    #[test]
    fn integers_sort_in_numeric_order_including_negatives() {
        let mut keys = vec![
            key(Value::Int(3)),
            key(Value::Int(-7)),
            key(Value::Int(0)),
            key(Value::Int(-1)),
            key(Value::Int(100)),
        ];
        keys.sort();
        let expected: Vec<Vec<u8>> = [-7i64, -1, 0, 3, 100]
            .into_iter()
            .map(|i| key(Value::Int(i)))
            .collect();
        assert_eq!(keys, expected, "byte order must be numeric order");
    }

    #[test]
    fn floats_sort_in_numeric_order_including_negatives() {
        let mut keys = vec![
            key(Value::Float(1.5)),
            key(Value::Float(-2.25)),
            key(Value::Float(0.0)),
            key(Value::Float(-0.5)),
        ];
        keys.sort();
        let expected: Vec<Vec<u8>> = [-2.25f64, -0.5, 0.0, 1.5]
            .into_iter()
            .map(|f| key(Value::Float(f)))
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn an_integer_and_the_same_number_as_a_float_share_a_key() {
        // The executor compares Int and Float to each other, so `x = 5` has to
        // find a stored 5.0. Different keys would make the index miss it.
        assert_eq!(key(Value::Int(5)), key(Value::Float(5.0)));
        assert_eq!(key(Value::Int(-3)), key(Value::Float(-3.0)));
    }

    #[test]
    fn integers_and_floats_interleave_by_value_not_by_type() {
        let mut keys = vec![
            key(Value::Int(2)),
            key(Value::Float(1.5)),
            key(Value::Int(1)),
            key(Value::Float(2.5)),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                key(Value::Int(1)),
                key(Value::Float(1.5)),
                key(Value::Int(2)),
                key(Value::Float(2.5)),
            ]
        );
    }

    #[test]
    fn text_sorts_lexicographically() {
        let mut keys = vec![key(v("pear")), key(v("apple")), key(v("banana"))];
        keys.sort();
        assert_eq!(
            keys,
            vec![key(v("apple")), key(v("banana")), key(v("pear"))]
        );
    }

    fn v(text: &str) -> Value {
        Value::Text(text.into())
    }

    #[test]
    fn different_types_occupy_disjoint_spans() {
        // A range over numbers must not be able to reach a text value: the
        // executor's comparison returns nothing across those types, so the
        // filter would match none of them anyway.
        let number = key(Value::Int(i64::MAX));
        let text = key(v(""));
        assert!(number < text, "every number must sort below every text");

        let boolean = key(Value::Bool(true));
        assert!(boolean < key(Value::Int(i64::MIN)));
    }

    #[test]
    fn values_the_executor_cannot_compare_are_not_indexable() {
        assert!(encode(&Value::Null).is_none());
        assert!(encode(&Value::List(vec![])).is_none());
        assert!(encode(&Value::Map(Default::default())).is_none());
    }

    #[test]
    fn an_unencodable_bound_refuses_rather_than_returning_an_empty_range() {
        // Returning an empty range would silently answer "no rows match", which
        // for `x = NULL` is a different claim from "an index cannot answer".
        assert!(encoded_range(&IndexBound::Eq(Value::Null)).is_none());
        assert!(
            encoded_range(&IndexBound::AnyOf(vec![Value::Int(1), Value::Null])).is_none(),
            "one unencodable member makes the set unanswerable"
        );
    }

    #[test]
    fn a_range_carries_its_inclusivity_through_to_the_bounds() {
        use std::ops::Bound as B;
        let spans = encoded_range(&IndexBound::Range {
            low: Some((Value::Int(1), false)),
            high: Some((Value::Int(9), true)),
        })
        .expect("encodable");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, B::Excluded(key(Value::Int(1))));
        assert_eq!(spans[0].1, B::Included(key(Value::Int(9))));
    }
}
