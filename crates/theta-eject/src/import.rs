//! Streaming, resumable import.
//!
//! Two properties, and the second is the one that costs design effort:
//!
//! **Streaming.** Rows are read in keyset-ordered batches, so peak memory is a
//! batch rather than a table. A migration that has to hold the source in memory
//! is a migration that cannot run on the databases most worth migrating.
//!
//! **Resumable.** A migration is a long operation over a network, so it *will*
//! be interrupted. Resuming re-reads from the last committed cursor, which
//! means rows around the boundary can be imported twice — so every write is
//! made idempotent by construction: a row is addressed by its primary key, and
//! writing the same key with the same value twice is one row, not two. That is
//! the only reason at-least-once delivery is safe here.
//!
//! The cursor is committed *after* the batch it describes, never before. The
//! other order loses rows on a crash between the two, and a lost row in a
//! migration is discovered months later by the person who needed it.

use serde::{Deserialize, Serialize};
use theta_core::Value;
use tokio_postgres::Client;

use crate::plan::TablePlan;
use crate::{values, EjectError};

/// How far a table's import has got.
///
/// Serialized between runs. The keyset is the last primary key committed, not a
/// row offset: an `OFFSET` walks the rows it skips, which turns a resume into a
/// re-read of everything before it, and is not stable if the source changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Rendered primary key of the last row committed, or `None` before the
    /// first batch.
    pub after: Option<Vec<String>>,
    pub rows_imported: u64,
}

/// One row, ready to write.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedRow {
    /// The ThetaBase address for this row: the primary key, rendered.
    pub primary_key: String,
    pub value: Value,
}

/// A batch of rows plus the cursor that follows them.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub rows: Vec<ImportedRow>,
    pub cursor: Cursor,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Read the next batch after `cursor`.
///
/// Every column is selected as text (see [`crate::values`]), and ordering is by
/// the primary key so that "after" is well defined. A table with no primary key
/// never reaches here — [`crate::plan`] refuses it, because a row ThetaBase
/// cannot address is a row it cannot store or verify.
pub async fn next_batch(
    client: &Client,
    plan: &TablePlan,
    cursor: &Cursor,
    batch_size: usize,
) -> Result<Batch, EjectError> {
    let key_list = plan
        .primary_key
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    let select_list = plan
        .columns
        .iter()
        .map(|c| format!("{}::text", quote_ident(&c.source_name)))
        .collect::<Vec<_>>()
        .join(", ");

    // Key columns are read as text like every other column, so one parser
    // handles them all.
    let key_select = plan
        .primary_key
        .iter()
        .map(|c| format!("{}::text", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");

    // Keyset pagination via a row constructor: exactly the ">" semantics a
    // composite key wants, and an index can serve it.
    //
    // The cursor is carried as text, so each parameter is cast back to its
    // column's own type rather than the column being cast to text. Comparing
    // as text would order `10` before `9`, which disagrees with the ORDER BY
    // below - and a pagination whose predicate and ordering disagree skips rows
    // silently, which is the one failure a migration must not have.
    let (predicate, params): (String, Vec<String>) = match &cursor.after {
        Some(after) => {
            let placeholders = plan
                .primary_key
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    let ty = plan
                        .columns
                        .iter()
                        .find(|c| &c.source_name == column)
                        .map(|c| c.source_type.as_str())
                        .unwrap_or("text");
                    // `::text::<ty>` and not `::<ty>`: the latter makes
                    // Postgres infer the *parameter* as that type, and the
                    // cursor is carried as text. Text first, then converted.
                    format!("${}::text::{}", i + 1, quote_ident(ty))
                })
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!("WHERE ({key_list}) > ({placeholders})"),
                after.clone(),
            )
        }
        None => (String::new(), Vec::new()),
    };

    let sql = format!(
        "SELECT {select_list}, {key_select} FROM {}.{} {predicate} ORDER BY {key_list} LIMIT {batch_size}",
        quote_ident(&plan.source_schema),
        quote_ident(&plan.source_name),
    );

    let typed: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
        .iter()
        .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();

    let rows = client.query(sql.as_str(), &typed).await?;

    let column_count = plan.columns.len();
    let mut out = Vec::with_capacity(rows.len());
    let mut last_key: Option<Vec<String>> = None;

    for row in &rows {
        let mut fields = std::collections::BTreeMap::new();
        for (i, column) in plan.columns.iter().enumerate() {
            let text: Option<String> = row.get(i);
            let value = values::parse(
                text.as_deref(),
                column.mapping.ty,
                &plan.source_name,
                &column.source_name,
            )?;
            // A null is absence. Storing it explicitly would make "the column
            // is not set" and "the column is set to null" the same thing, and
            // the verification pass could not tell them apart either.
            if value != Value::Null {
                fields.insert(column.target_name.clone(), value);
            }
        }

        let key_parts: Vec<String> = (0..plan.primary_key.len())
            .map(|i| {
                row.get::<_, Option<String>>(column_count + i)
                    .unwrap_or_default()
            })
            .collect();

        out.push(ImportedRow {
            primary_key: render_key(&key_parts),
            value: Value::Map(fields),
        });
        last_key = Some(key_parts);
    }

    Ok(Batch {
        cursor: Cursor {
            after: last_key.or_else(|| cursor.after.clone()),
            rows_imported: cursor.rows_imported + rows.len() as u64,
        },
        rows: out,
    })
}

/// Render a composite primary key into one ThetaBase row key.
///
/// Parts are separated by a unit separator, which cannot appear in a Postgres
/// text rendering of a scalar. Joining on something printable would let
/// `("a|b", "c")` and `("a", "b|c")` collide into one row — a silent merge of
/// two rows into one, which is the worst thing a migration can do quietly.
pub fn render_key(parts: &[String]) -> String {
    parts.join("\u{001f}")
}

/// Quote an identifier for interpolation.
///
/// Identifiers cannot be bound as parameters, so they are quoted instead, with
/// embedded quotes doubled per the SQL standard. This is the only place in the
/// crate that builds SQL from a string, and it never does so from user data —
/// only from names the source database itself reported.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_composite_key_cannot_be_confused_with_another_split_of_the_same_text() {
        // The collision that would merge two rows into one.
        let a = render_key(&["a|b".to_string(), "c".to_string()]);
        let b = render_key(&["a".to_string(), "b|c".to_string()]);
        assert_ne!(a, b);
    }

    #[test]
    fn identifiers_with_quotes_are_escaped_rather_than_terminating_the_quote() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        // The shape someone would use to break out of the quoting.
        assert_eq!(
            quote_ident("a\" ; drop table x --"),
            "\"a\"\" ; drop table x --\""
        );
    }

    #[test]
    fn a_fresh_cursor_starts_before_every_row() {
        let cursor = Cursor::default();
        assert!(cursor.after.is_none());
        assert_eq!(cursor.rows_imported, 0);
    }
}
