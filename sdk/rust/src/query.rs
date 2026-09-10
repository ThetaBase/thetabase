//! The typed query builder.
//!
//! Chainable, because that is what makes a query pleasant to write. What it
//! produces is a [`QueryAst`], and the AST is rendered to SQL-subset source and
//! bound parameters by the Scribe core — the same renderer the TypeScript,
//! Python and Go builders reach through WebAssembly. A query built the same way
//! in any of them arrives as the same bytes, and the rule that a value never
//! becomes query text has one implementation.
//!
//! Nothing here interpolates. `filter(eq("name", user_input))` puts
//! `user_input` in the parameter map and `$p0` in the text, whatever
//! `user_input` contains.
//!
//! # `filter`, not `where`
//!
//! `where` is a Rust keyword, so the method that the other three SDKs spell
//! `where` is `filter` here. Named rather than escaped as `r#where`: a raw
//! identifier at every call site is a worse tax than one method name that reads
//! differently, and `filter` is what a Rust caller would reach for anyway.

use serde_json::Value;
use theta_scribe_wasm::query::{CompareOp, Predicate, QueryAst, Sort};

pub use theta_scribe_wasm::query::{RenderError, Rendered};

fn compare(column: &str, op: CompareOp, value: impl Into<Value>) -> Predicate {
    Predicate::Compare {
        column: column.to_string(),
        op,
        value: value.into(),
    }
}

/// Keep rows where `column` equals `value`.
pub fn eq(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Eq, value)
}

/// Keep rows where `column` does not equal `value`.
pub fn ne(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Ne, value)
}

/// Keep rows where `column` is less than `value`.
pub fn lt(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Lt, value)
}

/// Keep rows where `column` is at most `value`.
pub fn lte(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Lte, value)
}

/// Keep rows where `column` is greater than `value`.
pub fn gt(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Gt, value)
}

/// Keep rows where `column` is at least `value`.
pub fn gte(column: &str, value: impl Into<Value>) -> Predicate {
    compare(column, CompareOp::Gte, value)
}

/// Keep rows where `column` is one of `values`.
pub fn is_in(column: &str, values: impl IntoIterator<Item = Value>) -> Predicate {
    Predicate::In {
        column: column.to_string(),
        values: values.into_iter().collect(),
    }
}

/// Keep rows where `column` is null.
pub fn is_null(column: &str) -> Predicate {
    Predicate::IsNull {
        column: column.to_string(),
    }
}

/// Keep rows where `column` is not null.
pub fn not_null(column: &str) -> Predicate {
    Predicate::NotNull {
        column: column.to_string(),
    }
}

/// Keep rows matching every term.
pub fn and(terms: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::And {
        terms: terms.into_iter().collect(),
    }
}

/// Keep rows matching any term.
pub fn or(terms: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::Or {
        terms: terms.into_iter().collect(),
    }
}

/// Invert a term.
pub fn not(term: Predicate) -> Predicate {
    Predicate::Not {
        term: Box::new(term),
    }
}

/// Start a query against `name`.
pub fn table(name: &str) -> Query {
    Query {
        ast: QueryAst {
            table: name.to_string(),
            columns: Vec::new(),
            filter: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        },
    }
}

/// A typed plan under construction.
///
/// Owned and consumed by each method rather than cloned, which is what Rust
/// offers and the other SDKs cannot: a builder that returned a new value from a
/// borrow would copy the AST on every call, and one that mutated in place would
/// let a shared base query change under whoever else held it. Moving makes the
/// second impossible at compile time.
#[derive(Debug)]
pub struct Query {
    ast: QueryAst,
}

impl Query {
    /// Add columns to the projection. No projection means every column.
    pub fn select(mut self, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.ast.columns.extend(columns.into_iter().map(Into::into));
        self
    }

    /// Keep rows matching `predicate`. Repeated calls are ANDed.
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.ast.filter = Some(match self.ast.filter.take() {
            Some(existing) => Predicate::And {
                terms: vec![existing, predicate],
            },
            None => predicate,
        });
        self
    }

    /// Add a sort key.
    pub fn order_by(mut self, column: &str, descending: bool) -> Self {
        self.ast.order_by.push(Sort {
            column: column.to_string(),
            descending,
        });
        self
    }

    /// Cap the number of rows returned.
    pub fn limit(mut self, count: u64) -> Self {
        self.ast.limit = Some(count);
        self
    }

    /// Skip rows before returning any.
    pub fn offset(mut self, count: u64) -> Self {
        self.ast.offset = Some(count);
        self
    }

    /// The plan, for a caller that wants to inspect it.
    pub fn ast(&self) -> &QueryAst {
        &self.ast
    }

    /// Render to SQL-subset source and bound parameters.
    ///
    /// Fails if an identifier could carry syntax. Table and column names are
    /// written into the query text, so they are restricted to letters, digits
    /// and underscores — a name that needed quoting would be a way to write
    /// syntax, and the renderer refuses rather than escaping.
    pub fn render(&self) -> Result<Rendered, RenderError> {
        theta_scribe_wasm::query::render(&self.ast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_never_reaches_the_query_text() {
        // The property the whole builder exists for.
        let hostile = "'; DROP TABLE users; --";
        let rendered = table("users")
            .filter(eq("name", hostile))
            .render()
            .expect("renders");

        assert!(
            !rendered.sql.contains("DROP"),
            "the value reached the text: {}",
            rendered.sql
        );
        assert_eq!(rendered.sql, "SELECT * FROM users WHERE name = $p0");
        assert_eq!(
            rendered.params.get("p0").map(String::as_str),
            Some("\"'; DROP TABLE users; --\"")
        );
    }

    #[test]
    fn repeated_filters_are_anded_rather_than_replaced() {
        let rendered = table("users")
            .filter(eq("email", "a@example.com"))
            .filter(gt("age", 30))
            .render()
            .expect("renders");
        assert!(
            rendered.sql.contains(" AND "),
            "the second filter replaced the first: {}",
            rendered.sql
        );
        assert_eq!(rendered.params.len(), 2);
    }

    #[test]
    fn an_identifier_that_could_carry_syntax_is_refused() {
        // Refused rather than escaped. Escaping would mean deciding what is safe
        // to quote, and the safe answer is that a name needing quotes is not a
        // name this subset accepts.
        let refused = table("users; DROP TABLE x").render();
        assert!(matches!(refused, Err(RenderError::BadIdentifier(_))));
    }

    #[test]
    fn a_query_with_no_projection_selects_everything() {
        // Go's binding got this wrong in a way Rust cannot: an empty `Vec` is an
        // empty `Vec`, where an empty Go slice can be nil and marshal to `null`.
        // Kept as a test anyway so the two SDKs are known to agree here.
        let rendered = table("users").render().expect("renders");
        assert_eq!(rendered.sql, "SELECT * FROM users");
    }

    #[test]
    fn the_builder_moves_rather_than_sharing() {
        // Compile-time proof that a base query cannot change under a holder: the
        // AST is owned, so two derived queries cannot exist over one buffer the
        // way Go's slices can. What this asserts is the visible half — that the
        // derived query carries both columns.
        let rendered = table("users")
            .select(["id"])
            .select(["email"])
            .render()
            .expect("renders");
        assert_eq!(rendered.sql, "SELECT id, email FROM users");
    }
}
