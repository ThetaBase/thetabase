//! Plan execution.
//!
//! Executes a typed [`Plan`] against a branch's materialized view. Two
//! properties matter more than speed here:
//!
//! * **No model call, ever.** This is the hot path
//!   (`09-sla-performance.md` §1), and `no_llm_on_hot_path` asserts structurally
//!   that nothing in this crate's dependency closure could make one.
//! * **Parameters are bound, never interpolated.** A [`Expr::Param`] is resolved
//!   from the caller's bindings at execution time. There is no code path that
//!   turns a value into query text, which is what makes injection structurally
//!   impossible rather than filtered-against
//!   (`04-threat-model-security.md` §4).

use std::collections::BTreeMap;

use theta_core::address;
use theta_core::{RowSource, Value, ValueType};

use crate::plan::{AggFunc, Aggregate, Expr, Plan, Predicate, SortOrder};
use crate::result::{Column, ResultSet};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ExecError {
    #[error("parameter `{name}` was not bound")]
    UnboundParameter { name: String },

    #[error("parameter `{name}` is declared {declared} but was bound to {actual}")]
    ParameterType {
        name: String,
        declared: ValueType,
        actual: ValueType,
    },

    #[error("cannot compare {left} with {right}")]
    Incomparable { left: ValueType, right: ValueType },

    #[error("`{0}` is not implemented yet — lands in ROADMAP M3")]
    Unsupported(&'static str),
}

/// Values bound to a plan's parameters for one execution.
pub type Bindings = BTreeMap<String, Value>;

/// One row in flight: its primary key plus its value.
#[derive(Debug, Clone)]
struct Row {
    primary_key: String,
    value: Value,
}

impl Row {
    fn column(&self, name: &str) -> Option<Value> {
        address::column(&self.value, &self.primary_key, name)
    }
}

/// Execute `plan`, producing a result set.
pub fn execute(
    plan: &Plan,
    source: &impl RowSource,
    bindings: &Bindings,
) -> Result<ResultSet, ExecError> {
    // Parameters are checked once, up front, so a plan cannot half-execute and
    // then fail on an unbound value in row 900,000.
    check_bindings(plan, bindings)?;

    let table = plan.source_table().to_string();
    // No budget at the top: the caller asked for the whole result. A `LIMIT`
    // inside the plan introduces one for the sub-tree beneath it.
    let rows = eval(plan, source, bindings, None)?;
    Ok(materialize(rows, plan, source, &table))
}

fn check_bindings(plan: &Plan, bindings: &Bindings) -> Result<(), ExecError> {
    for (name, declared) in plan.params() {
        let Some(value) = bindings.get(name) else {
            return Err(ExecError::UnboundParameter {
                name: name.to_string(),
            });
        };
        // Only a parameter whose type was declared is checked. An untyped one —
        // all the SQL front end can produce — accepts what it is bound to, and
        // the comparison it feeds simply will not match across incomparable
        // types.
        if let Some(declared) = declared {
            if !declared.accepts(value) {
                return Err(ExecError::ParameterType {
                    name: name.to_string(),
                    declared,
                    actual: value.inferred_type(),
                });
            }
        }
    }
    Ok(())
}

/// Evaluate `plan`, producing at most `budget` rows where a budget is given.
///
/// The budget is how `LIMIT` stops paying for rows nobody asked for. It travels
/// *down* the tree, and only through operators that cannot change which rows
/// come out on top: a `Sort` or an `Aggregate` has to see everything before it
/// knows what its first row even is, so both refuse to pass a budget on. A
/// `Filter` keeps its own budget - it can stop once it has produced enough -
/// but does not hand one to its input, because it has no idea how many rows it
/// must read to produce that many.
///
/// Returning fewer than `budget` rows is always allowed. Returning more is
/// allowed too: the budget is permission to stop early, not an obligation, so
/// an operator that cannot honour it cheaply simply ignores it.
fn eval(
    plan: &Plan,
    source: &impl RowSource,
    bindings: &Bindings,
    budget: Option<usize>,
) -> Result<Vec<Row>, ExecError> {
    match plan {
        Plan::Scan { table } => Ok(source
            .scan_up_to(table, budget)
            .into_iter()
            .map(|(primary_key, value)| Row { primary_key, value })
            .collect()),

        Plan::PointLookup { table, key } => {
            let key = match resolve(key, bindings)? {
                Value::Text(text) => text,
                other => encode_key(&other),
            };
            Ok(source
                .row(table, &key)
                .map(|value| {
                    vec![Row {
                        primary_key: key,
                        value,
                    }]
                })
                .unwrap_or_default())
        }

        Plan::IndexScan {
            table, predicate, ..
        } => {
            // Ask the source for candidates. What comes back is a *superset*:
            // the filter below runs either way, so an index that is generous
            // costs a discarded row rather than a wrong answer. `None` means no
            // index covers this predicate, and the scan is the documented
            // degraded mode (`01-system-architecture.md` §7) rather than an
            // error — what must never happen is unchecked raw execution.
            let rows = match index_candidates(table, predicate, source, bindings) {
                Some(keys) => keys
                    .into_iter()
                    .filter_map(|primary_key| {
                        source
                            .row(table, &primary_key)
                            .map(|value| Row { primary_key, value })
                    })
                    .collect(),
                // No budget: the filter below decides which rows survive,
                // so stopping the scan early could stop it before the rows that
                // match.
                None => eval(
                    &Plan::Scan {
                        table: table.clone(),
                    },
                    source,
                    bindings,
                    None,
                )?,
            };
            filter(rows, predicate, bindings)
        }

        Plan::Filter { input, predicate } => {
            // The input gets no budget - how many rows it must produce for the
            // filter to yield `budget` is unknowable without running it - but
            // the filter itself stops once it has enough.
            let rows = eval(input, source, bindings, None)?;
            filter_up_to(rows, predicate, bindings, budget)
        }

        // Projection changes the shape of the output, not the set of rows, so it
        // is applied when the result set is materialized.
        Plan::Project { input, .. } => eval(input, source, bindings, budget),

        Plan::Sort { input, by } => {
            // A sort cannot take a budget from above: which rows come first is
            // exactly what it is about to decide, so reading fewer would be
            // choosing the answer before sorting it.
            let mut rows = eval(input, source, bindings, None)?;
            rows.sort_by(|a, b| {
                for (column, order) in by {
                    let ordering = compare(&a.column(column), &b.column(column));
                    let ordering = match order {
                        SortOrder::Asc => ordering,
                        SortOrder::Desc => ordering.reverse(),
                    };
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                // Ties break on primary key, so a sort is deterministic rather
                // than merely sorted — two runs of the same query must agree.
                a.primary_key.cmp(&b.primary_key)
            });
            Ok(rows)
        }

        Plan::Limit {
            input,
            count,
            offset,
        } => {
            // Everything this sub-tree will ever need: the rows skipped plus
            // the rows kept. Narrowed further by any budget already in force,
            // since a limit inside a smaller limit cannot widen it.
            let needed = (*offset as usize).saturating_add(*count as usize);
            let inner = Some(match budget {
                Some(outer) => needed.min(outer),
                None => needed,
            });
            let rows = eval(input, source, bindings, inner)?;
            Ok(rows
                .into_iter()
                .skip(*offset as usize)
                .take(*count as usize)
                .collect())
        }

        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            // An aggregate over a budgeted input would be an aggregate over an
            // arbitrary subset - a `COUNT(*)` that counts as far as it felt
            // like. Everything, always.
            let rows = eval(input, source, bindings, None)?;
            aggregate(rows, group_by, aggregates)
        }
    }
}

fn filter(
    rows: Vec<Row>,
    predicate: &Predicate,
    bindings: &Bindings,
) -> Result<Vec<Row>, ExecError> {
    filter_up_to(rows, predicate, bindings, None)
}

/// Filter, stopping once `budget` rows have passed.
///
/// Stopping early is only sound because nothing downstream of a `LIMIT` cares
/// which rows it got, only how many — a `Sort` between the two would have
/// consumed the budget itself rather than passing it here.
///
/// It also stops evaluating the predicate over the remaining rows, which is the
/// point: the predicate is the expensive part, and `LIMIT 10` over a million
/// rows should not run it a million times.
fn filter_up_to(
    rows: Vec<Row>,
    predicate: &Predicate,
    bindings: &Bindings,
    budget: Option<usize>,
) -> Result<Vec<Row>, ExecError> {
    let mut out = Vec::new();
    for row in rows {
        if budget.is_some_and(|budget| out.len() >= budget) {
            break;
        }
        if matches(&row, predicate, bindings)? {
            out.push(row);
        }
    }
    Ok(out)
}

fn matches(row: &Row, predicate: &Predicate, bindings: &Bindings) -> Result<bool, ExecError> {
    use std::cmp::Ordering::{Equal, Greater, Less};

    let cmp = |column: &str, value: &Expr| -> Result<Option<std::cmp::Ordering>, ExecError> {
        let left = row.column(column);
        let right = resolve(value, bindings)?;
        Ok(order_of(&left, &Some(right)))
    };

    Ok(match predicate {
        Predicate::True => true,
        // A missing column compares to nothing, so every comparison against it
        // is false — SQL's null semantics, and the behaviour a reader expects.
        Predicate::Eq { column, value } => cmp(column, value)? == Some(Equal),
        Predicate::Ne { column, value } => matches!(cmp(column, value)?, Some(Less | Greater)),
        Predicate::Lt { column, value } => cmp(column, value)? == Some(Less),
        Predicate::Lte { column, value } => matches!(cmp(column, value)?, Some(Less | Equal)),
        Predicate::Gt { column, value } => cmp(column, value)? == Some(Greater),
        Predicate::Gte { column, value } => matches!(cmp(column, value)?, Some(Greater | Equal)),
        Predicate::In { column, values } => {
            let mut found = false;
            for value in values {
                if cmp(column, value)? == Some(Equal) {
                    found = true;
                    break;
                }
            }
            found
        }
        Predicate::IsNull { column } => {
            matches!(row.column(column), None | Some(Value::Null))
        }
        Predicate::And(preds) => {
            let mut all = true;
            for pred in preds {
                if !matches(row, pred, bindings)? {
                    all = false;
                    break;
                }
            }
            all
        }
        Predicate::Or(preds) => {
            let mut any = false;
            for pred in preds {
                if matches(row, pred, bindings)? {
                    any = true;
                    break;
                }
            }
            any
        }
        Predicate::Not(inner) => !matches(row, inner, bindings)?,
    })
}

/// Resolve an expression to a value. This is the only place a parameter becomes
/// a value, and it produces a [`Value`] — never text that could be spliced into
/// a query.
fn resolve(expr: &Expr, bindings: &Bindings) -> Result<Value, ExecError> {
    match expr {
        Expr::Literal(literal) => Ok(literal.0.clone()),
        Expr::Param { name, .. } => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundParameter { name: name.clone() }),
        // A bare column reference outside a comparison has no row to read from.
        Expr::Column(name) => Err(ExecError::Unsupported(match name.is_empty() {
            true => "empty column reference",
            false => "column reference in a value position",
        })),
    }
}

/// Total order over values of comparable types. `None` when the two are not
/// comparable, which every predicate treats as "does not match".
fn order_of(left: &Option<Value>, right: &Option<Value>) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Some(a), Some(b)) => compare_values(a, b),
        _ => None,
    }
}

/// Translate a predicate into an index bound and ask the source to serve it.
///
/// `None` at any step means "scan instead", never "no rows match" — the two are
/// indistinguishable in the return type of a lookup, so the distinction is kept
/// here where it can still be made. `Ne` and `IsNull` are absent deliberately:
/// neither narrows to a range an index can sweep, and pretending otherwise
/// would mean sweeping the whole index to save a scan of the same rows.
fn index_candidates(
    table: &str,
    predicate: &Predicate,
    source: &impl RowSource,
    bindings: &Bindings,
) -> Option<Vec<String>> {
    let (column, bound) = match predicate {
        Predicate::Eq { column, value } => (
            column,
            theta_core::IndexBound::Eq(resolve(value, bindings).ok()?),
        ),
        Predicate::In { column, values } => {
            let mut resolved = Vec::with_capacity(values.len());
            for value in values {
                resolved.push(resolve(value, bindings).ok()?);
            }
            (column, theta_core::IndexBound::AnyOf(resolved))
        }
        Predicate::Lt { column, value } => (
            column,
            theta_core::IndexBound::Range {
                low: None,
                high: Some((resolve(value, bindings).ok()?, false)),
            },
        ),
        Predicate::Lte { column, value } => (
            column,
            theta_core::IndexBound::Range {
                low: None,
                high: Some((resolve(value, bindings).ok()?, true)),
            },
        ),
        Predicate::Gt { column, value } => (
            column,
            theta_core::IndexBound::Range {
                low: Some((resolve(value, bindings).ok()?, false)),
                high: None,
            },
        ),
        Predicate::Gte { column, value } => (
            column,
            theta_core::IndexBound::Range {
                low: Some((resolve(value, bindings).ok()?, true)),
                high: None,
            },
        ),
        // `AND` matches a subset of every conjunct, so any *one* conjunct an
        // index can serve is already a valid superset — take the first, and let
        // the filter apply the rest.
        Predicate::And(parts) => {
            return parts
                .iter()
                .find_map(|part| index_candidates(table, part, source, bindings))
        }

        // `OR` matches the union, so serving only some disjuncts would return a
        // *subset* and silently lose rows. All or nothing.
        Predicate::Or(parts) => {
            let mut union = std::collections::BTreeSet::new();
            for part in parts {
                union.extend(index_candidates(table, part, source, bindings)?);
            }
            return Some(union.into_iter().collect());
        }

        // A negation matches whatever the index does not hold, which is not a
        // range. `Ne` and `IsNull` are the same shape of problem, and `True`
        // narrows nothing.
        Predicate::True | Predicate::Ne { .. } | Predicate::IsNull { .. } | Predicate::Not(_) => {
            return None
        }
    };
    source.index_candidates(table, column, &bound)
}

fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value::*;
    match (a, b) {
        (Null, Null) => Some(std::cmp::Ordering::Equal),
        // Null compares to nothing else, so a comparison against it is false
        // rather than an arbitrary winner.
        (Null, _) | (_, Null) => None,
        (Bool(a), Bool(b)) => Some(a.cmp(b)),
        (Int(a), Int(b)) => Some(a.cmp(b)),
        (Timestamp(a), Timestamp(b)) => Some(a.cmp(b)),
        (Float(a), Float(b)) => a.partial_cmp(b),
        // Int and Float compare because the widening is lossless in that
        // direction; nothing else is coerced.
        (Int(a), Float(b)) => (*a as f64).partial_cmp(b),
        (Float(a), Int(b)) => a.partial_cmp(&(*b as f64)),
        (Text(a), Text(b)) => Some(a.cmp(b)),
        (Bytes(a), Bytes(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Sort ordering, which must be total even where comparison is not.
///
/// A missing or incomparable column sorts as *greater* than any value, which
/// means nulls come last ascending and first descending. That is Postgres's
/// default and it is deliberate: the migrating-founder case
/// (`05-prd.md` §2) is badly served by a database that orders rows almost the
/// same way as the one they came from.
fn compare(a: &Option<Value>, b: &Option<Value>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => compare_values(x, y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn aggregate(
    rows: Vec<Row>,
    group_by: &[String],
    aggregates: &[Aggregate],
) -> Result<Vec<Row>, ExecError> {
    // A global aggregate — no GROUP BY — produces exactly one row even over an
    // empty table. `SELECT COUNT(*) FROM users WHERE <nothing matches>` must
    // answer 0, not answer nothing; returning no rows there would break every
    // caller that reads a single scalar out of the result.
    if group_by.is_empty() {
        let mut fields: BTreeMap<String, Value> = BTreeMap::new();
        for agg in aggregates {
            fields.insert(agg.alias.clone(), apply_aggregate(agg, &rows));
        }
        return Ok(vec![Row {
            primary_key: String::new(),
            value: Value::Map(fields),
        }]);
    }

    // Groups are keyed by their canonical encoding so that grouping is stable
    // across runs, and ordered so the output is deterministic.
    let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        let key = group_by
            .iter()
            .map(|column| encode_key(&row.column(column).unwrap_or(Value::Null)))
            .collect::<Vec<_>>()
            .join("\u{1}");
        groups.entry(key).or_default().push(row);
    }

    let mut out = Vec::new();
    for (key, members) in groups {
        let mut fields: BTreeMap<String, Value> = BTreeMap::new();

        for column in group_by {
            let value = members
                .first()
                .and_then(|row| row.column(column))
                .unwrap_or(Value::Null);
            fields.insert(column.clone(), value);
        }

        for agg in aggregates {
            fields.insert(agg.alias.clone(), apply_aggregate(agg, &members));
        }

        out.push(Row {
            primary_key: key,
            value: Value::Map(fields),
        });
    }
    Ok(out)
}

fn apply_aggregate(agg: &Aggregate, rows: &[Row]) -> Value {
    let values = || -> Vec<Value> {
        let Some(column) = &agg.column else {
            return Vec::new();
        };
        rows.iter()
            .filter_map(|row| row.column(column))
            .filter(|v| !matches!(v, Value::Null))
            .collect()
    };

    match agg.func {
        // COUNT(*) counts rows; COUNT(col) counts non-null values. The
        // difference matters and is not a detail to smooth over.
        AggFunc::Count => match agg.column {
            None => Value::Int(rows.len() as i64),
            Some(_) => Value::Int(values().len() as i64),
        },
        AggFunc::Sum => sum_of(&values()),
        AggFunc::Avg => {
            let values = values();
            if values.is_empty() {
                // The average of nothing is null, not zero.
                return Value::Null;
            }
            match sum_of(&values) {
                Value::Int(total) => Value::Float(total as f64 / values.len() as f64),
                Value::Float(total) => Value::Float(total / values.len() as f64),
                other => other,
            }
        }
        AggFunc::Min => extreme(&values(), std::cmp::Ordering::Less),
        AggFunc::Max => extreme(&values(), std::cmp::Ordering::Greater),
    }
}

fn sum_of(values: &[Value]) -> Value {
    // Stays integral while every value is an integer, so summing counts does not
    // silently acquire floating-point error.
    let mut int_total: i64 = 0;
    let mut float_total = 0.0f64;
    let mut any_float = false;

    for value in values {
        match value {
            Value::Int(v) => {
                int_total = int_total.saturating_add(*v);
                float_total += *v as f64;
            }
            Value::Float(v) => {
                any_float = true;
                float_total += v;
            }
            _ => {}
        }
    }

    match any_float {
        true => Value::Float(float_total),
        false => Value::Int(int_total),
    }
}

fn extreme(values: &[Value], want: std::cmp::Ordering) -> Value {
    let mut best: Option<&Value> = None;
    for value in values {
        best = match best {
            None => Some(value),
            Some(current) => match compare_values(value, current) {
                Some(ordering) if ordering == want => Some(value),
                _ => Some(current),
            },
        };
    }
    best.cloned().unwrap_or(Value::Null)
}

/// Canonical text for a value used as a key. Deterministic because every map
/// inside a `Value` is ordered.
fn encode_key(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Turn rows into a typed result set.
fn materialize(rows: Vec<Row>, plan: &Plan, source: &impl RowSource, table: &str) -> ResultSet {
    let names = projected_columns(plan, &rows);

    let columns = names
        .iter()
        .map(|name| {
            // Declared type first; otherwise infer from the data, which is what
            // dynamic mode leaves us with.
            let ty = source
                .column_type(table, name)
                .or_else(|| infer_column_type(&rows, name))
                .unwrap_or(ValueType::Text);
            Column::new(name.clone(), ty)
        })
        .collect();

    let mut result = ResultSet {
        columns,
        rows: Vec::with_capacity(rows.len()),
    };
    for row in &rows {
        result.rows.push(
            names
                .iter()
                .map(|name| row.column(name).unwrap_or(Value::Null))
                .collect(),
        );
    }
    result
}

/// The columns a plan produces: its projection if it has one, otherwise the
/// union of what the rows carry.
fn projected_columns(plan: &Plan, rows: &[Row]) -> Vec<String> {
    if let Some(columns) = explicit_projection(plan) {
        return columns;
    }

    let mut names: Vec<String> = vec![address::KEY_COLUMN.to_string()];
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in rows {
        for column in address::columns_of(&row.value) {
            if seen.insert(column.clone()) {
                names.push(column);
            }
        }
    }
    names
}

fn explicit_projection(plan: &Plan) -> Option<Vec<String>> {
    match plan {
        Plan::Project { columns, .. } => Some(columns.clone()),
        Plan::Aggregate {
            group_by,
            aggregates,
            ..
        } => Some(
            group_by
                .iter()
                .cloned()
                .chain(aggregates.iter().map(|a| a.alias.clone()))
                .collect(),
        ),
        Plan::Sort { input, .. } | Plan::Limit { input, .. } => explicit_projection(input),
        _ => None,
    }
}

/// Infer a column's type from the first row that has a non-null value for it.
fn infer_column_type(rows: &[Row], name: &str) -> Option<ValueType> {
    if name == address::KEY_COLUMN {
        return Some(ValueType::Text);
    }
    rows.iter()
        .filter_map(|row| row.column(name))
        .find(|value| !matches!(value, Value::Null))
        .map(|value| value.inferred_type())
}
