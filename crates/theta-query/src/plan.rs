//! The typed plan IR — the canonical form of every query.
//!
//! A [`Plan`] is a tree of relational operators over already-typed values. There
//! is no variant that carries raw SQL text into execution, which is what makes
//! injection structurally impossible rather than filtered-against.

use serde::{Deserialize, Serialize};
use theta_core::{Value, ValueType};

/// Cache key for a compiled plan. Identical plans hash identically, so a
/// repeated query skips parse and planning entirely.
///
/// # Serialized as a string, not a number
///
/// A plan hash is what a reviewer compares to confirm the plan they approved is
/// the plan that ran, so it has to survive the trip exactly. As a JSON number it
/// does not: `JSON.parse` in any JavaScript runtime rounds above 2^53, and a
/// real hash turned `17778716309262963995` into `17778716309262965000` — close
/// enough to look right and wrong enough to make the comparison meaningless.
///
/// This is the same reasoning the generated SDK bindings already apply to every
/// 64-bit field by rendering them as `bigint`. JSON has no `bigint`, so a string
/// is the equivalent.
///
/// Deserialization accepts a number as well, so anything written before this
/// still loads — but it is the reader that is lenient, never the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanHash(pub u64);

impl Serialize for PlanHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for PlanHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = PlanHash;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a plan hash, as a decimal string or a 64-bit number")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<PlanHash, E> {
                value.parse().map(PlanHash).map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<PlanHash, E> {
                Ok(PlanHash(value))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Externally tagged, deliberately. Internal tagging (`tag = "node"`) routes
/// every nested value through serde's `TaggedSerializer`, and because this enum
/// is recursive that instantiates a new serializer type per nesting level until
/// rustc gives up. External tagging costs nothing here and keeps the derive
/// finite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    /// Full scan of a table.
    Scan { table: String },
    /// Point lookup by primary key — the `get()` hot path.
    PointLookup { table: String, key: Box<Expr> },
    /// Range or equality scan served by a named index.
    IndexScan {
        table: String,
        index: String,
        predicate: Predicate,
    },
    Filter {
        input: Box<Plan>,
        predicate: Predicate,
    },
    Project {
        input: Box<Plan>,
        columns: Vec<String>,
    },
    Sort {
        input: Box<Plan>,
        by: Vec<(String, SortOrder)>,
    },
    Limit {
        input: Box<Plan>,
        count: u64,
        offset: u64,
    },
    Aggregate {
        input: Box<Plan>,
        group_by: Vec<String>,
        aggregates: Vec<Aggregate>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub func: AggFunc,
    pub column: Option<String>,
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

/// Externally tagged for the same reason as [`Plan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    True,
    Eq { column: String, value: Expr },
    Ne { column: String, value: Expr },
    Lt { column: String, value: Expr },
    Lte { column: String, value: Expr },
    Gt { column: String, value: Expr },
    Gte { column: String, value: Expr },
    In { column: String, values: Vec<Expr> },
    IsNull { column: String },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
}

/// Scalar expressions. Note there is no `Raw(String)` variant, and there never
/// will be: a bound parameter is a [`Expr::Param`] resolved from the request's
/// context vars, never spliced into a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    Literal(Literal),
    Column(String),
    /// Named parameter, bound from the caller's context vars at execution time.
    ///
    /// `ty` is `None` when the type is not known from syntax — the SQL front end
    /// cannot tell what `$who` should be — and `Some` when a typed builder
    /// declared it. An untyped parameter accepts whatever it is bound to; a
    /// typed one is checked. Using `ValueType::Null` to mean "any" would be
    /// wrong in the other direction, since that type accepts only nulls.
    Param {
        name: String,
        ty: Option<ValueType>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Literal(pub Value);

impl Plan {
    /// Stable structural hash, used as the plan-cache key.
    ///
    /// Encoded by hand rather than through serde: the cache key has to stay
    /// identical across releases, so it must not depend on a derive's field
    /// order or on serde_json's output format changing under us.
    pub fn hash(&self) -> PlanHash {
        let mut enc = CanonicalEncoder::default();
        enc.plan(self);
        let digest = theta_core::ContentHash::of(&enc.0);
        PlanHash(u64::from_le_bytes(
            digest.0[..8].try_into().expect("32-byte digest"),
        ))
    }

    /// Table this plan ultimately reads from.
    pub fn source_table(&self) -> &str {
        match self {
            Plan::Scan { table }
            | Plan::PointLookup { table, .. }
            | Plan::IndexScan { table, .. } => table,
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Aggregate { input, .. } => input.source_table(),
        }
    }

    /// Every parameter this plan expects to have bound before execution, with
    /// its declared type where one is known.
    pub fn params(&self) -> Vec<(&str, Option<ValueType>)> {
        let mut out = Vec::new();
        self.collect_params(&mut out);
        out
    }

    fn collect_params<'a>(&'a self, out: &mut Vec<(&'a str, Option<ValueType>)>) {
        match self {
            Plan::Scan { .. } => {}
            Plan::PointLookup { key, .. } => collect_expr_params(key, out),
            Plan::IndexScan { predicate, .. } => collect_pred_params(predicate, out),
            Plan::Filter { input, predicate } => {
                input.collect_params(out);
                collect_pred_params(predicate, out);
            }
            Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Aggregate { input, .. } => input.collect_params(out),
        }
    }
}

fn collect_expr_params<'a>(expr: &'a Expr, out: &mut Vec<(&'a str, Option<ValueType>)>) {
    if let Expr::Param { name, ty } = expr {
        out.push((name, *ty));
    }
}

fn collect_pred_params<'a>(pred: &'a Predicate, out: &mut Vec<(&'a str, Option<ValueType>)>) {
    match pred {
        Predicate::True | Predicate::IsNull { .. } => {}
        Predicate::Eq { value, .. }
        | Predicate::Ne { value, .. }
        | Predicate::Lt { value, .. }
        | Predicate::Lte { value, .. }
        | Predicate::Gt { value, .. }
        | Predicate::Gte { value, .. } => collect_expr_params(value, out),
        Predicate::In { values, .. } => values.iter().for_each(|v| collect_expr_params(v, out)),
        Predicate::And(preds) | Predicate::Or(preds) => {
            preds.iter().for_each(|p| collect_pred_params(p, out))
        }
        Predicate::Not(inner) => collect_pred_params(inner, out),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_plan_hash_survives_a_javascript_json_parser() {
        // The value that caught this: as a JSON number it comes back as
        // 17778716309262965000, which is wrong by about a thousand and looks
        // entirely plausible.
        use super::PlanHash;

        let hash = PlanHash(17_778_716_309_262_963_995);
        let json = serde_json::to_string(&hash).expect("serializes");
        assert_eq!(
            json, "\"17778716309262963995\"",
            "a plan hash must travel as a string; as a number it rounds in JS"
        );
        assert_eq!(
            serde_json::from_str::<PlanHash>(&json).expect("round trips"),
            hash
        );
    }

    #[test]
    fn a_hash_written_as_a_number_still_loads() {
        // The reader is lenient so nothing persisted before this stops loading.
        use super::PlanHash;

        assert_eq!(
            serde_json::from_str::<PlanHash>("17778716309262963995").expect("loads"),
            PlanHash(17_778_716_309_262_963_995)
        );
    }

    use super::*;

    #[test]
    fn identical_plans_share_a_hash() {
        let a = Plan::Scan {
            table: "users".into(),
        };
        let b = Plan::Scan {
            table: "users".into(),
        };
        assert_eq!(a.hash(), b.hash());
        assert_ne!(
            a.hash(),
            Plan::Scan {
                table: "orders".into()
            }
            .hash()
        );
    }

    #[test]
    fn parameters_are_discoverable_before_execution() {
        let plan = Plan::Filter {
            input: Box::new(Plan::Scan {
                table: "users".into(),
            }),
            predicate: Predicate::Eq {
                column: "churn_risk".into(),
                value: Expr::Param {
                    name: "risk".into(),
                    ty: Some(ValueType::Bool),
                },
            },
        };
        assert_eq!(plan.params(), vec![("risk", Some(ValueType::Bool))]);
        assert_eq!(plan.source_table(), "users");
    }
}

/// Canonical byte encoding of a plan, used only for [`Plan::hash`].
///
/// Every node writes a distinct tag byte before its contents, and every
/// variable-length field is length-prefixed, so two structurally different plans
/// can never produce the same bytes. Tags are append-only: changing an existing
/// tag invalidates every cached plan in every deployment.
#[derive(Default)]
struct CanonicalEncoder(Vec<u8>);

impl CanonicalEncoder {
    fn tag(&mut self, tag: u8) {
        self.0.push(tag);
    }

    fn str(&mut self, s: &str) {
        self.0.extend_from_slice(&(s.len() as u64).to_le_bytes());
        self.0.extend_from_slice(s.as_bytes());
    }

    fn u64(&mut self, n: u64) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }

    fn plan(&mut self, plan: &Plan) {
        match plan {
            Plan::Scan { table } => {
                self.tag(1);
                self.str(table);
            }
            Plan::PointLookup { table, key } => {
                self.tag(2);
                self.str(table);
                self.expr(key);
            }
            Plan::IndexScan {
                table,
                index,
                predicate,
            } => {
                self.tag(3);
                self.str(table);
                self.str(index);
                self.predicate(predicate);
            }
            Plan::Filter { input, predicate } => {
                self.tag(4);
                self.plan(input);
                self.predicate(predicate);
            }
            Plan::Project { input, columns } => {
                self.tag(5);
                self.plan(input);
                self.u64(columns.len() as u64);
                for c in columns {
                    self.str(c);
                }
            }
            Plan::Sort { input, by } => {
                self.tag(6);
                self.plan(input);
                self.u64(by.len() as u64);
                for (col, order) in by {
                    self.str(col);
                    self.tag(match order {
                        SortOrder::Asc => 0,
                        SortOrder::Desc => 1,
                    });
                }
            }
            Plan::Limit {
                input,
                count,
                offset,
            } => {
                self.tag(7);
                self.plan(input);
                self.u64(*count);
                self.u64(*offset);
            }
            Plan::Aggregate {
                input,
                group_by,
                aggregates,
            } => {
                self.tag(8);
                self.plan(input);
                self.u64(group_by.len() as u64);
                for g in group_by {
                    self.str(g);
                }
                self.u64(aggregates.len() as u64);
                for agg in aggregates {
                    self.tag(match agg.func {
                        AggFunc::Count => 0,
                        AggFunc::Sum => 1,
                        AggFunc::Min => 2,
                        AggFunc::Max => 3,
                        AggFunc::Avg => 4,
                    });
                    self.str(agg.column.as_deref().unwrap_or(""));
                    self.str(&agg.alias);
                }
            }
        }
    }

    fn predicate(&mut self, pred: &Predicate) {
        match pred {
            Predicate::True => self.tag(20),
            Predicate::Eq { column, value } => self.cmp(21, column, value),
            Predicate::Ne { column, value } => self.cmp(22, column, value),
            Predicate::Lt { column, value } => self.cmp(23, column, value),
            Predicate::Lte { column, value } => self.cmp(24, column, value),
            Predicate::Gt { column, value } => self.cmp(25, column, value),
            Predicate::Gte { column, value } => self.cmp(26, column, value),
            Predicate::In { column, values } => {
                self.tag(27);
                self.str(column);
                self.u64(values.len() as u64);
                for v in values {
                    self.expr(v);
                }
            }
            Predicate::IsNull { column } => {
                self.tag(28);
                self.str(column);
            }
            Predicate::And(preds) => self.group(29, preds),
            Predicate::Or(preds) => self.group(30, preds),
            Predicate::Not(inner) => {
                self.tag(31);
                self.predicate(inner);
            }
        }
    }

    fn cmp(&mut self, tag: u8, column: &str, value: &Expr) {
        self.tag(tag);
        self.str(column);
        self.expr(value);
    }

    fn group(&mut self, tag: u8, preds: &[Predicate]) {
        self.tag(tag);
        self.u64(preds.len() as u64);
        for p in preds {
            self.predicate(p);
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Literal(lit) => {
                self.tag(40);
                self.value(&lit.0);
            }
            Expr::Column(name) => {
                self.tag(41);
                self.str(name);
            }
            // A parameter hashes by name and type, never by its bound value —
            // that is what lets one compiled plan serve every set of arguments.
            Expr::Param { name, ty } => {
                self.tag(42);
                self.str(name);
                // An untyped parameter hashes distinctly from a typed one, so
                // a builder-declared plan never collides with a SQL-parsed plan
                // that happens to share its text.
                self.str(&ty.map(|t| t.to_string()).unwrap_or_default());
            }
        }
    }

    fn value(&mut self, value: &Value) {
        match value {
            Value::Null => self.tag(50),
            Value::Bool(b) => {
                self.tag(51);
                self.tag(u8::from(*b));
            }
            Value::Int(n) => {
                self.tag(52);
                self.u64(*n as u64);
            }
            Value::Float(f) => {
                self.tag(53);
                self.u64(f.to_bits());
            }
            Value::Text(s) => {
                self.tag(54);
                self.str(s);
            }
            Value::Bytes(b) => {
                self.tag(55);
                self.u64(b.len() as u64);
                self.0.extend_from_slice(b);
            }
            Value::Timestamp(t) => {
                self.tag(56);
                self.u64(*t as u64);
            }
            Value::List(items) => {
                self.tag(57);
                self.u64(items.len() as u64);
                for item in items {
                    self.value(item);
                }
            }
            // BTreeMap iterates in key order, so map encoding is canonical.
            Value::Map(map) => {
                self.tag(58);
                self.u64(map.len() as u64);
                for (k, v) in map {
                    self.str(k);
                    self.value(v);
                }
            }
        }
    }
}

impl Plan {
    /// Canonical JSON encoding of this plan.
    ///
    /// Goes through `serde_json::Value` rather than straight to a writer. The
    /// plan IR is deeply recursive, and serializing it directly to an
    /// `io::Write` pushes rustc's trait resolution past its default depth in
    /// whatever crate performs the call — so every consumer would otherwise need
    /// its own `recursion_limit` attribute. Containing it here keeps that off
    /// the callers.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_value(self).map(|value| value.to_string())
    }

    /// Parse a plan from its canonical JSON encoding.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}
