//! The typed query builder's renderer.
//!
//! Each SDK offers a fluent builder in its own language, because that is what
//! makes a query pleasant to write. What the builder *produces* is rendered
//! here, once, for all of them: a query built the same way in TypeScript and in
//! Python must reach the server as the same bytes, or "the typed builder is the
//! canonical form" (`02-api-wire-protocol.md` §4) is three canonical forms.
//!
//! # Values never become syntax
//!
//! Every value a caller supplies becomes a bound parameter with a generated
//! name. There is no path from a value to the query text — not for a string
//! with a quote in it, not for one with a semicolon, not for anything. The
//! server parses the text into typed plan nodes and binds the parameters
//! separately (`docs/INVARIANTS.md` invariant 4, `04-threat-model-security.md` §4).
//!
//! That is why rendering is here rather than in each binding: this is the rule
//! that must not have three implementations.

use std::collections::BTreeMap;

use serde::Deserialize;

/// A query as a binding's builder produced it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryAst {
    pub table: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub filter: Option<Predicate>,
    #[serde(default)]
    pub order_by: Vec<Sort>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sort {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Predicate {
    Compare {
        column: String,
        op: CompareOp,
        value: serde_json::Value,
    },
    In {
        column: String,
        values: Vec<serde_json::Value>,
    },
    IsNull {
        column: String,
    },
    NotNull {
        column: String,
    },
    And {
        terms: Vec<Predicate>,
    },
    Or {
        terms: Vec<Predicate>,
    },
    Not {
        term: Box<Predicate>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CompareOp {
    fn as_sql(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Lte => "<=",
            CompareOp::Gt => ">",
            CompareOp::Gte => ">=",
        }
    }
}

/// A rendered query: text with placeholders, and the values they stand for.
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct Rendered {
    pub sql: String,
    /// Parameter name to canonical JSON. Sorted, so the same query renders to
    /// the same bytes whatever order a host's map iterates in.
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RenderError {
    #[error(
        "`{0}` is not a usable identifier. Table and column names are written into the \
         query text, so they are restricted to letters, digits and underscores — a name \
         that needed quoting would be a way to write syntax."
    )]
    BadIdentifier(String),
    #[error("a query must name at least one column, or select every column explicitly")]
    NoColumns,
}

/// Render an AST to SQL-subset source and bound parameters.
pub fn render(ast: &QueryAst) -> Result<Rendered, RenderError> {
    let mut params = BTreeMap::new();
    let mut next = 0usize;

    let columns = match ast.columns.is_empty() {
        true => "*".to_string(),
        false => ast
            .columns
            .iter()
            .map(|c| identifier(c))
            .collect::<Result<Vec<_>, _>>()?
            .join(", "),
    };

    let mut sql = format!("SELECT {columns} FROM {}", identifier(&ast.table)?);

    if let Some(filter) = &ast.filter {
        sql.push_str(" WHERE ");
        sql.push_str(&predicate(filter, &mut params, &mut next)?);
    }

    if !ast.order_by.is_empty() {
        let terms = ast
            .order_by
            .iter()
            .map(|s| {
                Ok(match s.descending {
                    true => format!("{} DESC", identifier(&s.column)?),
                    false => identifier(&s.column)?,
                })
            })
            .collect::<Result<Vec<_>, RenderError>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&terms.join(", "));
    }

    // Limit and offset are integers the builder holds, never caller text, so
    // they are written directly. A parameter would be safer against a bug here
    // and is not available: the SQL subset does not bind them.
    if let Some(limit) = ast.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    if let Some(offset) = ast.offset {
        sql.push_str(&format!(" OFFSET {offset}"));
    }

    Ok(Rendered { sql, params })
}

fn predicate(
    p: &Predicate,
    params: &mut BTreeMap<String, String>,
    next: &mut usize,
) -> Result<String, RenderError> {
    Ok(match p {
        Predicate::Compare { column, op, value } => {
            let name = bind(value, params, next)?;
            format!("{} {} ${name}", identifier(column)?, op.as_sql())
        }
        Predicate::In { column, values } => {
            let names = values
                .iter()
                .map(|v| Ok(format!("${}", bind(v, params, next)?)))
                .collect::<Result<Vec<_>, RenderError>>()?;
            format!("{} IN ({})", identifier(column)?, names.join(", "))
        }
        Predicate::IsNull { column } => format!("{} IS NULL", identifier(column)?),
        Predicate::NotNull { column } => format!("{} IS NOT NULL", identifier(column)?),
        Predicate::And { terms } => join(terms, "AND", params, next)?,
        Predicate::Or { terms } => join(terms, "OR", params, next)?,
        Predicate::Not { term } => format!("NOT ({})", predicate(term, params, next)?),
    })
}

fn join(
    terms: &[Predicate],
    op: &str,
    params: &mut BTreeMap<String, String>,
    next: &mut usize,
) -> Result<String, RenderError> {
    let rendered = terms
        .iter()
        .map(|t| predicate(t, params, next))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(match rendered.len() {
        0 => "TRUE".to_string(),
        1 => rendered.into_iter().next().expect("one term"),
        // Always parenthesised: `a OR b AND c` means something different from
        // what a caller who wrote `.or(a, b).and(c)` intended, and precedence
        // is not a thing to leave to the reader.
        _ => format!("({})", rendered.join(&format!(" {op} "))),
    })
}

/// Bind one value and return the parameter name standing for it.
fn bind(
    value: &serde_json::Value,
    params: &mut BTreeMap<String, String>,
    next: &mut usize,
) -> Result<String, RenderError> {
    let name = format!("p{next}");
    *next += 1;
    params.insert(
        name.clone(),
        serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    );
    Ok(name)
}

/// A table or column name, checked.
///
/// Identifiers are the one thing a builder writes into the query text, so they
/// are the one thing that could carry syntax. Restricted rather than quoted:
/// quoting means deciding what to escape, and a name that needs escaping is a
/// name worth rejecting.
fn identifier(name: &str) -> Result<String, RenderError> {
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit());

    match valid {
        true => Ok(name.to_string()),
        false => Err(RenderError::BadIdentifier(name.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ast(json: serde_json::Value) -> QueryAst {
        serde_json::from_value(json).expect("a valid AST")
    }

    #[test]
    fn a_plain_select_renders_without_parameters() {
        let out = render(&ast(serde_json::json!({ "table": "users" }))).expect("renders");
        assert_eq!(out.sql, "SELECT * FROM users");
        assert!(out.params.is_empty());
    }

    #[test]
    fn a_value_becomes_a_bound_parameter_and_never_query_text() {
        // The property the whole renderer exists for.
        let out = render(&ast(serde_json::json!({
            "table": "users",
            "filter": { "kind": "compare", "column": "email", "op": "eq", "value": "a@b.c" },
        })))
        .expect("renders");

        assert_eq!(out.sql, "SELECT * FROM users WHERE email = $p0");
        assert_eq!(out.params["p0"], "\"a@b.c\"");
    }

    #[test]
    fn a_value_carrying_sql_is_still_only_a_value() {
        let hostile = "'; DROP TABLE users; --";
        let out = render(&ast(serde_json::json!({
            "table": "users",
            "filter": { "kind": "compare", "column": "name", "op": "eq", "value": hostile },
        })))
        .expect("renders");

        assert_eq!(
            out.sql, "SELECT * FROM users WHERE name = $p0",
            "a value reached the query text"
        );
        assert!(!out.sql.contains("DROP"));
        assert_eq!(out.params["p0"], serde_json::to_string(hostile).unwrap());
    }

    #[test]
    fn an_identifier_that_could_carry_syntax_is_refused_rather_than_quoted() {
        // Identifiers are the one thing written into the text. Quoting means
        // deciding what to escape; refusing does not.
        for bad in ["users; DROP TABLE x", "users\"", "1users", "", "us ers"] {
            assert!(
                matches!(
                    render(&ast(serde_json::json!({ "table": bad }))),
                    Err(RenderError::BadIdentifier(_))
                ),
                "`{bad}` was accepted as a table name"
            );
        }
    }

    #[test]
    fn every_value_in_an_in_list_is_bound_separately() {
        let out = render(&ast(serde_json::json!({
            "table": "users",
            "filter": { "kind": "in", "column": "id", "values": [1, 2, 3] },
        })))
        .expect("renders");

        assert_eq!(out.sql, "SELECT * FROM users WHERE id IN ($p0, $p1, $p2)");
        assert_eq!(out.params.len(), 3);
    }

    #[test]
    fn a_mixed_boolean_is_parenthesised_rather_than_left_to_precedence() {
        // `a OR b AND c` is not what someone who wrote them in that order meant.
        let out = render(&ast(serde_json::json!({
            "table": "users",
            "filter": {
                "kind": "and",
                "terms": [
                    { "kind": "or", "terms": [
                        { "kind": "compare", "column": "a", "op": "eq", "value": 1 },
                        { "kind": "compare", "column": "b", "op": "eq", "value": 2 }
                    ]},
                    { "kind": "compare", "column": "c", "op": "eq", "value": 3 }
                ]
            },
        })))
        .expect("renders");

        assert_eq!(
            out.sql,
            "SELECT * FROM users WHERE ((a = $p0 OR b = $p1) AND c = $p2)"
        );
    }

    #[test]
    fn parameters_are_numbered_in_the_order_they_appear() {
        // So a rendered query is stable: the same builder calls produce the
        // same text and the same bindings, which is what lets a plan cache
        // hit at all.
        let query = serde_json::json!({
            "table": "users",
            "columns": ["email"],
            "filter": { "kind": "and", "terms": [
                { "kind": "compare", "column": "age", "op": "gte", "value": 18 },
                { "kind": "compare", "column": "city", "op": "eq", "value": "Berlin" }
            ]},
            "orderBy": [{ "column": "email", "descending": true }],
            "limit": 10,
            "offset": 5,
        });

        let a = render(&ast(query.clone())).expect("renders");
        let b = render(&ast(query)).expect("renders");
        assert_eq!(a, b);
        assert_eq!(
            a.sql,
            "SELECT email FROM users WHERE (age >= $p0 AND city = $p1) \
             ORDER BY email DESC LIMIT 10 OFFSET 5"
        );
    }

    #[test]
    fn a_null_check_needs_no_parameter() {
        let out = render(&ast(serde_json::json!({
            "table": "users",
            "filter": { "kind": "isNull", "column": "deleted_at" },
        })))
        .expect("renders");

        assert_eq!(out.sql, "SELECT * FROM users WHERE deleted_at IS NULL");
        assert!(out.params.is_empty());
    }
}
