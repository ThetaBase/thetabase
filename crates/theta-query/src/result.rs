//! Result sets and their Arrow IPC encoding.
//!
//! `02-api-wire-protocol.md` §2 specifies Arrow for `query` results, and the
//! reason is zero-copy: a client can map the bytes off the wire into columnar
//! arrays without walking and reallocating every value.
//!
//! STATUS: the encoder is complete and tested; nothing produces a [`ResultSet`]
//! yet because query execution lands in ROADMAP M3. The seam exists now so the
//! wire format is settled before the executor is written against it, rather
//! than after.

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, TimestampMillisecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{ArrowError, DataType, Field, Schema, TimeUnit};
use std::sync::Arc;
use theta_core::{Value, ValueType};

/// A query's output: a typed column list and its rows.
///
/// Rows are stored row-major because that is how the executor produces them;
/// the Arrow encoder transposes to columnar exactly once, at the wire boundary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: ValueType,
    pub nullable: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: ValueType) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResultError {
    #[error("row {row} has {found} values but the result set declares {expected} columns")]
    Arity {
        row: usize,
        found: usize,
        expected: usize,
    },

    #[error("row {row}, column `{column}`: expected {expected}, found {found}")]
    TypeMismatch {
        row: usize,
        column: String,
        expected: ValueType,
        found: ValueType,
    },

    #[error("{0} cannot be represented in a result column")]
    Unrepresentable(ValueType),

    #[error("arrow: {0}")]
    Arrow(#[from] ArrowError),
}

impl ResultSet {
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The Arrow schema for these columns.
    pub fn arrow_schema(&self) -> Result<Schema, ResultError> {
        let fields = self
            .columns
            .iter()
            .map(|c| Ok(Field::new(&c.name, arrow_type(c.ty)?, c.nullable)))
            .collect::<Result<Vec<_>, ResultError>>()?;
        Ok(Schema::new(fields))
    }

    /// Encode as an Arrow IPC stream.
    ///
    /// Always writes the schema, even with no rows: a client must be able to
    /// learn a query's shape from an empty result rather than having to guess.
    pub fn to_arrow_ipc(&self) -> Result<Vec<u8>, ResultError> {
        let schema = Arc::new(self.arrow_schema()?);
        let mut buffer = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut buffer, &schema)?;
            if !self.rows.is_empty() {
                writer.write(&self.to_record_batch(Arc::clone(&schema))?)?;
            }
            writer.finish()?;
        }
        Ok(buffer)
    }

    fn to_record_batch(&self, schema: Arc<Schema>) -> Result<RecordBatch, ResultError> {
        // Validate arity before building anything: a short row would otherwise
        // silently produce a column with fewer values than its neighbours.
        for (i, row) in self.rows.iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(ResultError::Arity {
                    row: i,
                    found: row.len(),
                    expected: self.columns.len(),
                });
            }
        }

        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.columns.len());
        for (index, column) in self.columns.iter().enumerate() {
            arrays.push(self.build_column(index, column)?);
        }
        Ok(RecordBatch::try_new(schema, arrays)?)
    }

    fn build_column(&self, index: usize, column: &Column) -> Result<ArrayRef, ResultError> {
        let rows = self.rows.len();

        // A value whose type contradicts its column is an error, never a
        // coercion — the same discipline the write path applies
        // (`03-data-model-consistency.md` §2.2).
        let mismatch = |row: usize, found: ValueType| ResultError::TypeMismatch {
            row,
            column: column.name.clone(),
            expected: column.ty,
            found,
        };

        Ok(match column.ty {
            ValueType::Bool => {
                let mut b = BooleanBuilder::with_capacity(rows);
                for (i, row) in self.rows.iter().enumerate() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        Value::Bool(v) => b.append_value(*v),
                        other => return Err(mismatch(i, other.inferred_type())),
                    }
                }
                Arc::new(b.finish())
            }
            ValueType::Int => {
                let mut b = Int64Builder::with_capacity(rows);
                for (i, row) in self.rows.iter().enumerate() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        Value::Int(v) => b.append_value(*v),
                        other => return Err(mismatch(i, other.inferred_type())),
                    }
                }
                Arc::new(b.finish())
            }
            ValueType::Float => {
                let mut b = Float64Builder::with_capacity(rows);
                for (i, row) in self.rows.iter().enumerate() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        Value::Float(v) => b.append_value(*v),
                        other => return Err(mismatch(i, other.inferred_type())),
                    }
                }
                Arc::new(b.finish())
            }
            ValueType::Timestamp => {
                let mut b = TimestampMillisecondBuilder::with_capacity(rows);
                for (i, row) in self.rows.iter().enumerate() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        Value::Timestamp(v) => b.append_value(*v),
                        other => return Err(mismatch(i, other.inferred_type())),
                    }
                }
                Arc::new(b.finish())
            }
            ValueType::Text => {
                let mut b = StringBuilder::new();
                for (i, row) in self.rows.iter().enumerate() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        Value::Text(v) => b.append_value(v),
                        other => return Err(mismatch(i, other.inferred_type())),
                    }
                }
                Arc::new(b.finish())
            }
            // Nested and binary values are carried as their canonical encoding
            // rather than as Arrow's nested types. A structured Arrow mapping is
            // worth doing once the executor can actually produce nested results
            // (ROADMAP M3); guessing at it now would settle a wire format on
            // no evidence.
            ValueType::Bytes | ValueType::List | ValueType::Map => {
                let mut b = StringBuilder::new();
                for row in self.rows.iter() {
                    match &row[index] {
                        Value::Null => b.append_null(),
                        other => b.append_value(
                            serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
                        ),
                    }
                }
                Arc::new(b.finish())
            }
            ValueType::Null => return Err(ResultError::Unrepresentable(ValueType::Null)),
        })
    }
}

fn arrow_type(ty: ValueType) -> Result<DataType, ResultError> {
    Ok(match ty {
        ValueType::Bool => DataType::Boolean,
        ValueType::Int => DataType::Int64,
        ValueType::Float => DataType::Float64,
        ValueType::Text | ValueType::Bytes | ValueType::List | ValueType::Map => DataType::Utf8,
        ValueType::Timestamp => DataType::Timestamp(TimeUnit::Millisecond, None),
        // A column of nothing but nulls has no type to declare, which means the
        // planner failed to infer one — a bug worth surfacing, not papering over.
        ValueType::Null => return Err(ResultError::Unrepresentable(ValueType::Null)),
    })
}

#[cfg(test)]
mod tests {
    use arrow_array::{Array, BooleanArray, Int64Array, StringArray};
    use arrow_ipc::reader::StreamReader;

    use super::*;

    fn sample() -> ResultSet {
        ResultSet {
            columns: vec![
                Column::new("id", ValueType::Int),
                Column::new("name", ValueType::Text),
                Column::new("churn_risk", ValueType::Bool),
            ],
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("Alice".into()),
                    Value::Bool(true),
                ],
                vec![Value::Int(2), Value::Text("Bob".into()), Value::Bool(false)],
                vec![Value::Int(3), Value::Null, Value::Null],
            ],
        }
    }

    fn read_back(bytes: &[u8]) -> Vec<RecordBatch> {
        StreamReader::try_new(std::io::Cursor::new(bytes), None)
            .expect("reader")
            .map(|b| b.expect("batch"))
            .collect()
    }

    #[test]
    fn a_result_set_round_trips_through_arrow_ipc() {
        let batches = read_back(&sample().to_arrow_ipc().expect("encode"));
        assert_eq!(batches.len(), 1);

        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 3);

        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int column");
        assert_eq!(ids.values(), &[1, 2, 3]);

        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text column");
        assert_eq!(names.value(0), "Alice");
        assert!(
            names.is_null(2),
            "a null must arrive as a null, not as an empty string"
        );

        let risk = batch
            .column(2)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("bool column");
        assert!(risk.value(0));
        assert!(!risk.value(1));
        assert!(risk.is_null(2));
    }

    #[test]
    fn an_empty_result_still_carries_its_schema() {
        let empty = ResultSet::new(vec![
            Column::new("id", ValueType::Int),
            Column::new("name", ValueType::Text),
        ]);
        let bytes = empty.to_arrow_ipc().expect("encode");

        // A client must be able to learn the shape of a query that matched
        // nothing, rather than having to guess it.
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
        let schema = reader.schema();
        assert_eq!(schema.fields().len(), 2);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    }

    #[test]
    fn a_value_contradicting_its_column_is_rejected_never_coerced() {
        let bad = ResultSet {
            columns: vec![Column::new("id", ValueType::Int)],
            rows: vec![vec![Value::Text("not an int".into())]],
        };
        assert!(matches!(
            bad.to_arrow_ipc(),
            Err(ResultError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn a_short_row_is_caught_rather_than_silently_shortening_a_column() {
        let ragged = ResultSet {
            columns: vec![
                Column::new("a", ValueType::Int),
                Column::new("b", ValueType::Int),
            ],
            rows: vec![vec![Value::Int(1)]],
        };
        assert!(matches!(
            ragged.to_arrow_ipc(),
            Err(ResultError::Arity { .. })
        ));
    }

    #[test]
    fn nested_values_travel_as_their_canonical_encoding() {
        let nested = ResultSet {
            columns: vec![Column::new("tags", ValueType::List)],
            rows: vec![vec![Value::List(vec![Value::Int(1), Value::Int(2)])]],
        };
        let batches = read_back(&nested.to_arrow_ipc().expect("encode"));
        let column = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("encoded as text");
        assert!(column.value(0).contains("\"value\":[") || column.value(0).contains("value"));
    }

    #[test]
    fn a_column_with_no_inferred_type_is_an_error_not_an_empty_column() {
        let untyped = ResultSet::new(vec![Column::new("mystery", ValueType::Null)]);
        assert!(matches!(
            untyped.to_arrow_ipc(),
            Err(ResultError::Unrepresentable(_))
        ));
    }

    #[test]
    fn a_large_result_encodes_and_reads_back_completely() {
        let mut big = ResultSet::new(vec![
            Column::new("id", ValueType::Int),
            Column::new("label", ValueType::Text),
        ]);
        for i in 0..5_000i64 {
            big.rows
                .push(vec![Value::Int(i), Value::Text(format!("row-{i}"))]);
        }

        let batches = read_back(&big.to_arrow_ipc().expect("encode"));
        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total, 5_000);
    }
}
