//! The typed query builder.
//!
//! This is the canonical way to construct a query (`02-api-wire-protocol.md`
//! §4); the SQL front end exists for teams migrating from Postgres-family tools
//! and compiles down to the same plan nodes.
//!
//! Building rather than parsing has a property parsing cannot: there is no
//! string to get wrong. A value passed here is a [`Value`], and the only way to
//! reach the executor is as a `Literal` or a bound `Param`. An agent writing
//! against this API cannot construct an injection even by trying, because there
//! is no text to inject into.
//!
//! ```
//! use theta_core::{Value, ValueType};
//! use theta_query::builder::table;
//!
//! let plan = table("users")
//!     .filter(theta_query::builder::col("churn_risk").eq(Value::Bool(true)))
//!     .order_by_desc("last_seen")
//!     .limit(50)
//!     .build();
//!
//! assert_eq!(plan.source_table(), "users");
//! ```

use theta_core::{Value, ValueType};

use crate::plan::{AggFunc, Aggregate, Expr, Literal, Plan, Predicate, SortOrder};

/// Start a query against `name`.
pub fn table(name: impl Into<String>) -> QueryBuilder {
    QueryBuilder {
        plan: Plan::Scan { table: name.into() },
        sort: Vec::new(),
        limit: None,
    }
}

/// Reference a column, to build a predicate against it.
pub fn col(name: impl Into<String>) -> ColumnRef {
    ColumnRef { name: name.into() }
}

/// A named parameter, bound at execution time.
///
/// `ty` is checked against the bound value, which is the difference between a
/// builder-declared parameter and one the SQL front end produces: syntax cannot
/// know the type, but a caller building a plan does.
pub fn param(name: impl Into<String>, ty: ValueType) -> Expr {
    Expr::Param {
        name: name.into(),
        ty: Some(ty),
    }
}

/// A literal value.
pub fn value(v: Value) -> Expr {
    Expr::Literal(Literal(v))
}

#[derive(Debug, Clone)]
pub struct ColumnRef {
    name: String,
}

impl ColumnRef {
    fn compare(
        self,
        make: impl FnOnce(String, Expr) -> Predicate,
        operand: impl Into<Operand>,
    ) -> Predicate {
        make(self.name, operand.into().0)
    }

    pub fn eq(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Eq { column, value }, operand)
    }

    pub fn ne(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Ne { column, value }, operand)
    }

    pub fn lt(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Lt { column, value }, operand)
    }

    pub fn lte(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Lte { column, value }, operand)
    }

    pub fn gt(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Gt { column, value }, operand)
    }

    pub fn gte(self, operand: impl Into<Operand>) -> Predicate {
        self.compare(|column, value| Predicate::Gte { column, value }, operand)
    }

    pub fn is_in(self, values: impl IntoIterator<Item = Value>) -> Predicate {
        Predicate::In {
            column: self.name,
            values: values
                .into_iter()
                .map(|v| Expr::Literal(Literal(v)))
                .collect(),
        }
    }

    pub fn is_null(self) -> Predicate {
        Predicate::IsNull { column: self.name }
    }
}

/// Either side of a comparison: a plain value, or a parameter.
///
/// A newtype rather than a bare `Expr` so `col("a").eq(Value::Int(1))` reads
/// naturally while `col("a").eq(some_column)` does not compile — the plan IR has
/// no column-to-column comparison, and a builder that appeared to offer one
/// would be lying.
#[derive(Debug, Clone)]
pub struct Operand(Expr);

impl From<Value> for Operand {
    fn from(value: Value) -> Self {
        Operand(Expr::Literal(Literal(value)))
    }
}

impl From<Expr> for Operand {
    fn from(expr: Expr) -> Self {
        Operand(expr)
    }
}

impl From<&str> for Operand {
    fn from(text: &str) -> Self {
        Operand(Expr::Literal(Literal(Value::Text(text.to_string()))))
    }
}

impl From<i64> for Operand {
    fn from(n: i64) -> Self {
        Operand(Expr::Literal(Literal(Value::Int(n))))
    }
}

impl From<bool> for Operand {
    fn from(b: bool) -> Self {
        Operand(Expr::Literal(Literal(Value::Bool(b))))
    }
}

/// Combine predicates.
pub fn all(predicates: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::And(predicates.into_iter().collect())
}

pub fn any(predicates: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::Or(predicates.into_iter().collect())
}

pub fn not(predicate: Predicate) -> Predicate {
    Predicate::Not(Box::new(predicate))
}

/// A query under construction.
///
/// Sort and limit are held aside rather than wrapped immediately, so that
/// `.limit(10).order_by("x")` and `.order_by("x").limit(10)` build the same
/// plan. Applying them in call order would make the first form silently return
/// the wrong ten rows.
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    plan: Plan,
    sort: Vec<(String, SortOrder)>,
    limit: Option<(u64, u64)>,
}

impl QueryBuilder {
    /// Read one row by primary key. The `get` hot path.
    pub fn key(mut self, primary_key: impl Into<Operand>) -> Self {
        let table = self.plan.source_table().to_string();
        self.plan = Plan::PointLookup {
            table,
            key: Box::new(primary_key.into().0),
        };
        self
    }

    /// Keep rows matching `predicate`. Repeated calls are ANDed together, which
    /// is what a reader expects from successive `.filter()` calls.
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.plan = match self.plan {
            Plan::Filter {
                input,
                predicate: existing,
            } => Plan::Filter {
                input,
                predicate: match existing {
                    Predicate::And(mut terms) => {
                        terms.push(predicate);
                        Predicate::And(terms)
                    }
                    other => Predicate::And(vec![other, predicate]),
                },
            },
            input => Plan::Filter {
                input: Box::new(input),
                predicate,
            },
        };
        self
    }

    pub fn select(mut self, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.plan = Plan::Project {
            input: Box::new(self.plan),
            columns: columns.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn order_by(mut self, column: impl Into<String>) -> Self {
        self.sort.push((column.into(), SortOrder::Asc));
        self
    }

    pub fn order_by_desc(mut self, column: impl Into<String>) -> Self {
        self.sort.push((column.into(), SortOrder::Desc));
        self
    }

    pub fn limit(mut self, count: u64) -> Self {
        let offset = self.limit.map(|(_, o)| o).unwrap_or(0);
        self.limit = Some((count, offset));
        self
    }

    pub fn offset(mut self, offset: u64) -> Self {
        let count = self.limit.map(|(c, _)| c).unwrap_or(u64::MAX);
        self.limit = Some((count, offset));
        self
    }

    /// Group and aggregate. Consumes any pending projection, since a query
    /// cannot both project raw columns and aggregate them.
    pub fn aggregate(
        mut self,
        group_by: impl IntoIterator<Item = impl Into<String>>,
        aggregates: impl IntoIterator<Item = Aggregate>,
    ) -> Self {
        // A projection under an aggregate would hide the columns the aggregate
        // needs, so it is dropped rather than silently narrowing the input.
        let input = match self.plan {
            Plan::Project { input, .. } => *input,
            other => other,
        };
        self.plan = Plan::Aggregate {
            input: Box::new(input),
            group_by: group_by.into_iter().map(Into::into).collect(),
            aggregates: aggregates.into_iter().collect(),
        };
        self
    }

    /// Finish, producing the plan.
    pub fn build(self) -> Plan {
        let mut plan = self.plan;

        // Sort before limit, always: limiting first would take an arbitrary
        // subset and then order it, which is almost never what was meant.
        if !self.sort.is_empty() {
            plan = Plan::Sort {
                input: Box::new(plan),
                by: self.sort,
            };
        }
        if let Some((count, offset)) = self.limit {
            plan = Plan::Limit {
                input: Box::new(plan),
                count,
                offset,
            };
        }
        plan
    }
}

/// Aggregate constructors.
pub fn count() -> Aggregate {
    Aggregate {
        func: AggFunc::Count,
        column: None,
        alias: "count".into(),
    }
}

pub fn count_of(column: impl Into<String>) -> Aggregate {
    let column = column.into();
    Aggregate {
        func: AggFunc::Count,
        alias: format!("count_{column}"),
        column: Some(column),
    }
}

pub fn sum(column: impl Into<String>) -> Aggregate {
    let column = column.into();
    Aggregate {
        func: AggFunc::Sum,
        alias: format!("sum_{column}"),
        column: Some(column),
    }
}

pub fn min(column: impl Into<String>) -> Aggregate {
    let column = column.into();
    Aggregate {
        func: AggFunc::Min,
        alias: format!("min_{column}"),
        column: Some(column),
    }
}

pub fn max(column: impl Into<String>) -> Aggregate {
    let column = column.into();
    Aggregate {
        func: AggFunc::Max,
        alias: format!("max_{column}"),
        column: Some(column),
    }
}

pub fn avg(column: impl Into<String>) -> Aggregate {
    let column = column.into();
    Aggregate {
        func: AggFunc::Avg,
        alias: format!("avg_{column}"),
        column: Some(column),
    }
}

impl Aggregate {
    /// Rename this aggregate's output column.
    pub fn as_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = alias.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_plan_scans_its_table() {
        assert_eq!(
            table("users").build(),
            Plan::Scan {
                table: "users".into()
            }
        );
    }

    #[test]
    fn successive_filters_are_anded_rather_than_replacing_each_other() {
        let plan = table("users")
            .filter(col("a").eq(1i64))
            .filter(col("b").eq(2i64))
            .filter(col("c").eq(3i64))
            .build();

        let Plan::Filter { predicate, .. } = plan else {
            panic!("expected a filter");
        };
        match predicate {
            Predicate::And(terms) => assert_eq!(terms.len(), 3, "a filter was dropped"),
            other => panic!("expected AND, got {other:?}"),
        }
    }

    #[test]
    fn sort_and_limit_build_the_same_plan_whatever_order_they_are_called_in() {
        // Applying them in call order would make `.limit(10).order_by("x")`
        // take an arbitrary ten rows and then sort those.
        let a = table("t").limit(10).order_by("x").build();
        let b = table("t").order_by("x").limit(10).build();
        assert_eq!(a, b);

        // ...and the sort is inside the limit, not outside.
        match a {
            Plan::Limit { input, count, .. } => {
                assert_eq!(count, 10);
                assert!(matches!(*input, Plan::Sort { .. }), "limit must wrap sort");
            }
            other => panic!("expected a limit, got {other:?}"),
        }
    }

    #[test]
    fn a_typed_parameter_carries_its_type_for_checking() {
        let plan = table("users")
            .filter(col("age").gte(param("min_age", ValueType::Int)))
            .build();
        assert_eq!(plan.params(), vec![("min_age", Some(ValueType::Int))]);
    }

    #[test]
    fn a_hostile_string_is_a_value_and_has_nowhere_to_become_syntax() {
        let plan = table("users")
            .filter(col("name").eq("Robert'); DROP TABLE students; --"))
            .build();

        let Plan::Filter { predicate, .. } = plan else {
            panic!("expected a filter");
        };
        let Predicate::Eq { value, .. } = predicate else {
            panic!("expected equality");
        };
        // There is no string in the plan for it to escape from.
        assert!(matches!(value, Expr::Literal(_)));
    }

    #[test]
    fn a_point_lookup_replaces_the_scan() {
        let plan = table("users").key("123").build();
        assert!(matches!(plan, Plan::PointLookup { .. }));
        assert_eq!(plan.source_table(), "users");
    }

    #[test]
    fn aggregating_drops_a_pending_projection_rather_than_narrowing_the_input() {
        // Projecting `name` and then summing `amount` would leave the aggregate
        // nothing to read.
        let plan = table("orders")
            .select(["name"])
            .aggregate(["status"], [sum("amount")])
            .build();

        let Plan::Aggregate { input, .. } = plan else {
            panic!("expected an aggregate");
        };
        assert!(
            matches!(*input, Plan::Scan { .. }),
            "projection should be gone"
        );
    }

    #[test]
    fn aggregate_aliases_default_to_the_function_and_column() {
        assert_eq!(count().alias, "count");
        assert_eq!(sum("amount").alias, "sum_amount");
        assert_eq!(avg("age").as_alias("mean").alias, "mean");
    }

    #[test]
    fn offset_without_limit_still_pages() {
        let plan = table("t").offset(5).build();
        match plan {
            Plan::Limit { count, offset, .. } => {
                assert_eq!(offset, 5);
                assert_eq!(
                    count,
                    u64::MAX,
                    "no limit means take everything after the offset"
                );
            }
            other => panic!("expected a limit, got {other:?}"),
        }
    }

    #[test]
    fn combinators_compose() {
        let predicate = all([
            col("active").eq(true),
            any([col("plan").eq("pro"), col("plan").eq("team")]),
            not(col("banned").eq(true)),
        ]);
        match predicate {
            Predicate::And(terms) => assert_eq!(terms.len(), 3),
            other => panic!("expected AND, got {other:?}"),
        }
    }

    #[test]
    fn a_builder_plan_and_the_equivalent_sql_agree() {
        // Two front ends, one IR. If these diverged, the SQL path and the typed
        // path would silently mean different things.
        let built = table("users").filter(col("age").gt(30i64)).build();
        let parsed = crate::sql::compile("SELECT * FROM users WHERE age > 30").expect("compile");
        assert_eq!(built, parsed);
        assert_eq!(built.hash(), parsed.hash());
    }
}
