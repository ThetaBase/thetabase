//! SQL-subset front end.
//!
//! This module is a security boundary, not a convenience. Its **only** legal
//! output is a typed [`Plan`]: every literal in the source becomes a
//! [`Literal`] or a bound [`Expr::Param`] node, and no path returns text that is
//! later executed. That is what makes injection structurally impossible rather
//! than filtered-against (`04-threat-model-security.md` §4).
//!
//! # Supported subset
//!
//! ```sql
//! SELECT <columns> FROM <table>
//!   [WHERE <predicate>]
//!   [GROUP BY <columns>]
//!   [ORDER BY <column> [ASC|DESC], ...]
//!   [LIMIT <n> [OFFSET <n>]]
//! ```
//!
//! Columns may be `*`, identifiers, or aggregate calls (`COUNT`, `SUM`, `MIN`,
//! `MAX`, `AVG`) with an optional `AS` alias. Predicates support `=`, `!=`/`<>`,
//! `<`, `<=`, `>`, `>=`, `IN`, `IS NULL`, `IS NOT NULL`, `AND`, `OR`, `NOT`, and
//! parentheses. Parameters are written `$name`.
//!
//! Deliberately excluded, and refused rather than partially honoured: joins,
//! subqueries, `UNION`, `HAVING`, `DISTINCT`, stored procedures and triggers.
//! Anything with side effects outside the log is out of scope by design
//! (`01-system-architecture.md` §8), and anything merely unimplemented says so
//! rather than quietly returning a plan that means something else.
//!
//! The subset is read-only by construction: there is no statement form for a
//! write, so a query can never mutate.

use theta_core::Value;
use thiserror::Error;

use crate::plan::{AggFunc, Aggregate, Expr, Literal, Plan, Predicate, SortOrder};

#[derive(Debug, Error, PartialEq)]
pub enum SqlError {
    #[error("unsupported syntax at position {position}: {detail}")]
    Unsupported { position: usize, detail: String },

    #[error("parse error at position {position}: {detail}")]
    Parse { position: usize, detail: String },

    #[error("unterminated string literal starting at position {position}")]
    UnterminatedString { position: usize },
}

/// Compile a SQL-subset statement into a typed plan.
pub fn compile(sql: &str) -> Result<Plan, SqlError> {
    let tokens = tokenize(sql)?;
    Parser { tokens, at: 0 }.parse_select()
}

// ---- tokens -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted string. Kept distinct from `Word` so an identifier can never be
    /// confused with a literal, in either direction.
    Str(String),
    Int(i64),
    Float(f64),
    Param(String),
    Symbol(String),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    position: usize,
}

fn tokenize(sql: &str) -> Result<Vec<Token>, SqlError> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let start = i;
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Comments run to end of line. Accepted so pasted SQL works, and
        // discarded rather than carried anywhere near execution.
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if c == '\'' {
            i += 1;
            let mut text = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(SqlError::UnterminatedString { position: start }),
                    // '' is an escaped quote inside a string, as in standard SQL.
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        text.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        text.push(*ch);
                        i += 1;
                    }
                }
            }
            tokens.push(Token {
                tok: Tok::Str(text),
                position: start,
            });
            continue;
        }

        if c == '$' {
            i += 1;
            let mut name = String::new();
            while let Some(ch) = chars.get(i) {
                if ch.is_alphanumeric() || *ch == '_' {
                    name.push(*ch);
                    i += 1;
                } else {
                    break;
                }
            }
            if name.is_empty() {
                return Err(SqlError::Parse {
                    position: start,
                    detail: "expected a parameter name after `$`".into(),
                });
            }
            tokens.push(Token {
                tok: Tok::Param(name),
                position: start,
            });
            continue;
        }

        if c.is_ascii_digit() {
            let mut text = String::new();
            let mut is_float = false;
            while let Some(ch) = chars.get(i) {
                if ch.is_ascii_digit() {
                    text.push(*ch);
                    i += 1;
                } else if *ch == '.' && !is_float {
                    is_float = true;
                    text.push(*ch);
                    i += 1;
                } else {
                    break;
                }
            }
            let tok = match is_float {
                true => Tok::Float(text.parse().map_err(|_| SqlError::Parse {
                    position: start,
                    detail: format!("`{text}` is not a number"),
                })?),
                false => Tok::Int(text.parse().map_err(|_| SqlError::Parse {
                    position: start,
                    detail: format!("`{text}` does not fit in a 64-bit integer"),
                })?),
            };
            tokens.push(Token {
                tok,
                position: start,
            });
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let mut word = String::new();
            while let Some(ch) = chars.get(i) {
                if ch.is_alphanumeric() || *ch == '_' {
                    word.push(*ch);
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.push(Token {
                tok: Tok::Word(word),
                position: start,
            });
            continue;
        }

        // Two-character operators first, so `<=` never tokenizes as `<` then `=`.
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if matches!(two.as_str(), "<=" | ">=" | "!=" | "<>") {
            tokens.push(Token {
                tok: Tok::Symbol(two),
                position: start,
            });
            i += 2;
            continue;
        }

        if "=<>(),*.".contains(c) {
            tokens.push(Token {
                tok: Tok::Symbol(c.to_string()),
                position: start,
            });
            i += 1;
            continue;
        }

        // Anything unrecognised stops the parse. A character we do not
        // understand must never be skipped: skipping is how a parser silently
        // changes what a statement means.
        return Err(SqlError::Parse {
            position: start,
            detail: format!("unexpected character `{c}`"),
        });
    }

    Ok(tokens)
}

// ---- parser -----------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

/// One entry in the SELECT list.
enum Selected {
    All,
    Column(String),
    Aggregate(Aggregate),
}

impl Parser {
    fn position(&self) -> usize {
        self.tokens
            .get(self.at)
            .or_else(|| self.tokens.last())
            .map(|t| t.position)
            .unwrap_or(0)
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.at).map(|t| t.tok.clone());
        if tok.is_some() {
            self.at += 1;
        }
        tok
    }

    /// Consume `word` if it is next, case-insensitively.
    fn eat_word(&mut self, word: &str) -> bool {
        match self.peek() {
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word) => {
                self.at += 1;
                true
            }
            _ => false,
        }
    }

    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        match self.peek() {
            Some(Tok::Symbol(s)) if s == symbol => {
                self.at += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_word(&mut self, word: &str) -> Result<(), SqlError> {
        match self.eat_word(word) {
            true => Ok(()),
            false => Err(SqlError::Parse {
                position: self.position(),
                detail: format!("expected `{word}`"),
            }),
        }
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), SqlError> {
        match self.eat_symbol(symbol) {
            true => Ok(()),
            false => Err(SqlError::Parse {
                position: self.position(),
                detail: format!("expected `{symbol}`"),
            }),
        }
    }

    fn identifier(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Some(Tok::Word(word)) => Ok(word),
            _ => Err(SqlError::Parse {
                position: self.position(),
                detail: "expected an identifier".into(),
            }),
        }
    }

    fn unsupported(&self, what: &str) -> SqlError {
        SqlError::Unsupported {
            position: self.position(),
            detail: format!("{what} is not supported"),
        }
    }

    fn parse_select(&mut self) -> Result<Plan, SqlError> {
        self.expect_word("SELECT")?;

        if self.peek_word("DISTINCT") {
            return Err(self.unsupported("DISTINCT"));
        }

        let selected = self.parse_select_list()?;
        self.expect_word("FROM")?;
        let table = self.identifier()?;

        // Refused explicitly rather than ignored: a JOIN silently dropped would
        // return a plausible answer to a different question.
        for unsupported in ["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "UNION"] {
            if self.peek_word(unsupported) {
                return Err(self.unsupported(unsupported));
            }
        }

        let mut plan = Plan::Scan { table };

        if self.eat_word("WHERE") {
            let predicate = self.parse_predicate()?;
            plan = Plan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        let group_by = match self.eat_word("GROUP") {
            true => {
                self.expect_word("BY")?;
                let mut columns = vec![self.identifier()?];
                while self.eat_symbol(",") {
                    columns.push(self.identifier()?);
                }
                columns
            }
            false => Vec::new(),
        };

        if self.peek_word("HAVING") {
            return Err(self.unsupported("HAVING"));
        }

        plan = self.apply_projection(plan, selected, group_by)?;

        if self.eat_word("ORDER") {
            self.expect_word("BY")?;
            let mut by = Vec::new();
            loop {
                let column = self.identifier()?;
                let order = if self.eat_word("DESC") {
                    SortOrder::Desc
                } else {
                    self.eat_word("ASC");
                    SortOrder::Asc
                };
                by.push((column, order));
                if !self.eat_symbol(",") {
                    break;
                }
            }
            plan = Plan::Sort {
                input: Box::new(plan),
                by,
            };
        }

        if self.eat_word("LIMIT") {
            let count = self.expect_unsigned("LIMIT")?;
            let offset = match self.eat_word("OFFSET") {
                true => self.expect_unsigned("OFFSET")?,
                false => 0,
            };
            plan = Plan::Limit {
                input: Box::new(plan),
                count,
                offset,
            };
        }

        if self.at < self.tokens.len() {
            return Err(SqlError::Parse {
                position: self.position(),
                detail: "unexpected trailing input".into(),
            });
        }
        Ok(plan)
    }

    /// LIMIT and OFFSET take literal counts, never parameters: a plan's row
    /// bound is part of its shape, and letting it vary per execution would make
    /// the cost estimate in EXPLAIN meaningless.
    fn expect_unsigned(&mut self, clause: &str) -> Result<u64, SqlError> {
        match self.next() {
            Some(Tok::Int(n)) if n >= 0 => Ok(n as u64),
            Some(Tok::Int(n)) => Err(SqlError::Parse {
                position: self.position(),
                detail: format!("{clause} cannot be negative (got {n})"),
            }),
            _ => Err(SqlError::Parse {
                position: self.position(),
                detail: format!("expected a number after {clause}"),
            }),
        }
    }

    fn parse_select_list(&mut self) -> Result<Vec<Selected>, SqlError> {
        let mut selected = Vec::new();
        loop {
            selected.push(self.parse_selected()?);
            if !self.eat_symbol(",") {
                break;
            }
        }
        Ok(selected)
    }

    fn parse_selected(&mut self) -> Result<Selected, SqlError> {
        if self.eat_symbol("*") {
            return Ok(Selected::All);
        }

        let name = self.identifier()?;

        // An identifier followed by `(` is an aggregate call.
        if self.eat_symbol("(") {
            let func = match name.to_ascii_uppercase().as_str() {
                "COUNT" => AggFunc::Count,
                "SUM" => AggFunc::Sum,
                "MIN" => AggFunc::Min,
                "MAX" => AggFunc::Max,
                "AVG" => AggFunc::Avg,
                other => return Err(self.unsupported(&format!("function `{other}`"))),
            };

            let column = match self.eat_symbol("*") {
                true => None,
                false => Some(self.identifier()?),
            };
            self.expect_symbol(")")?;

            let default_alias = match &column {
                Some(c) => format!("{}_{c}", name.to_ascii_lowercase()),
                None => name.to_ascii_lowercase(),
            };
            let alias = match self.eat_word("AS") {
                true => self.identifier()?,
                false => default_alias,
            };

            return Ok(Selected::Aggregate(Aggregate {
                func,
                column,
                alias,
            }));
        }

        // `AS` on a plain column is accepted but has nowhere to go: the plan IR
        // projects by name and has no rename operator. Refusing is honest;
        // silently ignoring the alias would return columns under the wrong name.
        if self.eat_word("AS") {
            return Err(self.unsupported("aliasing a non-aggregate column"));
        }

        Ok(Selected::Column(name))
    }

    fn apply_projection(
        &mut self,
        plan: Plan,
        selected: Vec<Selected>,
        group_by: Vec<String>,
    ) -> Result<Plan, SqlError> {
        let aggregates: Vec<Aggregate> = selected
            .iter()
            .filter_map(|s| match s {
                Selected::Aggregate(a) => Some(a.clone()),
                _ => None,
            })
            .collect();

        if !aggregates.is_empty() || !group_by.is_empty() {
            let plain: Vec<String> = selected
                .iter()
                .filter_map(|s| match s {
                    Selected::Column(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();

            // A plain column alongside an aggregate must be in GROUP BY, or the
            // value returned for it is arbitrary. Postgres rejects this too, and
            // a migrating user would not expect it to be accepted.
            for column in &plain {
                if !group_by.contains(column) {
                    return Err(SqlError::Parse {
                        position: self.position(),
                        detail: format!(
                            "column `{column}` must appear in GROUP BY or be used in an aggregate"
                        ),
                    });
                }
            }

            return Ok(Plan::Aggregate {
                input: Box::new(plan),
                group_by,
                aggregates,
            });
        }

        if selected.iter().any(|s| matches!(s, Selected::All)) {
            // `SELECT *` projects nothing explicitly; the executor returns every
            // column the rows carry.
            return Ok(plan);
        }

        let columns: Vec<String> = selected
            .into_iter()
            .filter_map(|s| match s {
                Selected::Column(c) => Some(c),
                _ => None,
            })
            .collect();

        Ok(Plan::Project {
            input: Box::new(plan),
            columns,
        })
    }

    // ---- predicates ---------------------------------------------------------

    fn parse_predicate(&mut self) -> Result<Predicate, SqlError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Predicate, SqlError> {
        let mut terms = vec![self.parse_and()?];
        while self.eat_word("OR") {
            terms.push(self.parse_and()?);
        }
        Ok(match terms.len() {
            1 => terms.pop().expect("just checked"),
            _ => Predicate::Or(terms),
        })
    }

    fn parse_and(&mut self) -> Result<Predicate, SqlError> {
        let mut terms = vec![self.parse_not()?];
        while self.eat_word("AND") {
            terms.push(self.parse_not()?);
        }
        Ok(match terms.len() {
            1 => terms.pop().expect("just checked"),
            _ => Predicate::And(terms),
        })
    }

    fn parse_not(&mut self) -> Result<Predicate, SqlError> {
        if self.eat_word("NOT") {
            return Ok(Predicate::Not(Box::new(self.parse_not()?)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Predicate, SqlError> {
        if self.eat_symbol("(") {
            let inner = self.parse_predicate()?;
            self.expect_symbol(")")?;
            return Ok(inner);
        }

        let column = self.identifier()?;

        if self.eat_word("IS") {
            let negated = self.eat_word("NOT");
            self.expect_word("NULL")?;
            let predicate = Predicate::IsNull { column };
            return Ok(match negated {
                true => Predicate::Not(Box::new(predicate)),
                false => predicate,
            });
        }

        if self.eat_word("IN") {
            self.expect_symbol("(")?;
            let mut values = vec![self.parse_value()?];
            while self.eat_symbol(",") {
                values.push(self.parse_value()?);
            }
            self.expect_symbol(")")?;
            return Ok(Predicate::In { column, values });
        }

        let operator = match self.next() {
            Some(Tok::Symbol(s)) => s,
            _ => {
                return Err(SqlError::Parse {
                    position: self.position(),
                    detail: format!("expected a comparison operator after `{column}`"),
                })
            }
        };
        let value = self.parse_value()?;

        Ok(match operator.as_str() {
            "=" => Predicate::Eq { column, value },
            "!=" | "<>" => Predicate::Ne { column, value },
            "<" => Predicate::Lt { column, value },
            "<=" => Predicate::Lte { column, value },
            ">" => Predicate::Gt { column, value },
            ">=" => Predicate::Gte { column, value },
            other => {
                return Err(SqlError::Parse {
                    position: self.position(),
                    detail: format!("unknown operator `{other}`"),
                })
            }
        })
    }

    /// Parse a value position.
    ///
    /// This is the function that makes injection impossible: it returns a
    /// [`Literal`] or a [`Expr::Param`], both of which are *values*. There is no
    /// branch that produces text for later interpretation.
    fn parse_value(&mut self) -> Result<Expr, SqlError> {
        let position = self.position();
        match self.next() {
            Some(Tok::Str(text)) => Ok(Expr::Literal(Literal(Value::Text(text)))),
            Some(Tok::Int(n)) => Ok(Expr::Literal(Literal(Value::Int(n)))),
            Some(Tok::Float(f)) => Ok(Expr::Literal(Literal(Value::Float(f)))),
            // Syntax alone cannot say what `$who` should be, so the parameter is
            // untyped and accepts whatever it is bound to. A typed builder
            // declares the type; the SQL front end honestly cannot.
            Some(Tok::Param(name)) => Ok(Expr::Param { name, ty: None }),
            Some(Tok::Word(word)) => match word.to_ascii_uppercase().as_str() {
                "TRUE" => Ok(Expr::Literal(Literal(Value::Bool(true)))),
                "FALSE" => Ok(Expr::Literal(Literal(Value::Bool(false)))),
                "NULL" => Ok(Expr::Literal(Literal(Value::Null))),
                // A bare identifier here would be a column-to-column comparison,
                // which the plan IR does not express. Refused rather than
                // reinterpreted as a string literal — that reinterpretation is
                // precisely how `WHERE name = admin` silently becomes true.
                _ => Err(SqlError::Unsupported {
                    position,
                    detail: "comparing two columns is not supported".into(),
                }),
            },
            _ => Err(SqlError::Parse {
                position,
                detail: "expected a value".into(),
            }),
        }
    }
}
