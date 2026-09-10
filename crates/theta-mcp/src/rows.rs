//! Arrow IPC to something a model can read.
//!
//! Query results cross the wire as Arrow IPC, which is right for the protocol —
//! it is columnar, typed and cheap — and useless to a language model, which
//! reads text. A tool that returned the bytes would be a tool no agent could
//! use, so the decoding happens here rather than being left to the caller.

use arrow_array::{
    Array, BooleanArray, Float64Array, Int64Array, LargeStringArray, StringArray,
    TimestampMillisecondArray,
};
use arrow_ipc::reader::StreamReader;
use serde_json::{json, Map, Value};

/// Decode an Arrow IPC stream into rows of JSON objects.
///
/// `limit` bounds what is returned. A query that matched fifty thousand rows
/// must not put fifty thousand rows into a model's context: the agent asked a
/// question, and an answer that fills the window is worse than a truncated one
/// that says it was truncated.
pub fn decode(ipc: &[u8], limit: usize) -> Result<Value, String> {
    if ipc.is_empty() {
        return Ok(json!({ "columns": [], "rows": [], "rowCount": 0 }));
    }

    let reader = StreamReader::try_new(std::io::Cursor::new(ipc), None)
        .map_err(|e| format!("the result set is not readable as Arrow IPC: {e}"))?;

    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut total = 0usize;

    for batch in reader {
        let batch = batch.map_err(|e| format!("cannot read a result batch: {e}"))?;

        if columns.is_empty() {
            columns = batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
        }

        for row in 0..batch.num_rows() {
            total += 1;
            if rows.len() >= limit {
                continue;
            }
            let mut object = Map::new();
            for (index, name) in columns.iter().enumerate() {
                object.insert(name.clone(), cell(batch.column(index).as_ref(), row));
            }
            rows.push(Value::Object(object));
        }
    }

    let truncated = total > rows.len();
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "rowCount": total,
        // Said explicitly rather than left to be inferred from a short array. An
        // agent that counts the rows it received and reports that number is
        // reporting the limit, not the answer.
        "truncated": truncated,
        "note": if truncated {
            Value::String(format!(
                "showing {} of {total} rows. Narrow the query rather than raising the \
                 limit — the rest are not here.",
                rows.len()
            ))
        } else {
            Value::Null
        }
    }))
}

/// One cell, as JSON.
///
/// Every arm is spelled out. A catch-all rendering unknown types as a debug
/// string would put a plausible-looking value in front of a model that is not
/// the value, and the model has no way to tell.
fn cell(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }

    if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
        return json!(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
        return json!(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
        return json!(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        return json!(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
        return json!(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<TimestampMillisecondArray>() {
        return json!(a.value(row));
    }

    // Named as unrenderable rather than guessed at. A model told the type is
    // unsupported can ask for it differently; one handed a debug string will
    // treat it as data.
    json!({ "unrenderable": array.data_type().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_result_is_empty_rather_than_an_error() {
        // A query that matched nothing is a valid answer, and the shape has to
        // be the same as a query that matched something — an agent that has to
        // branch on the shape of an empty result will get it wrong once.
        let decoded = decode(&[], 10).expect("an empty result decodes");
        assert_eq!(decoded["rowCount"], 0);
        assert!(decoded["rows"].as_array().expect("rows").is_empty());
        assert!(decoded["columns"].is_array());
    }

    #[test]
    fn malformed_bytes_are_reported_rather_than_panicking() {
        // The bytes come off a socket. A panic here takes down the MCP server
        // and the agent sees its tools vanish.
        let result = decode(&[0xde, 0xad, 0xbe, 0xef], 10);
        assert!(result.is_err(), "garbage decoded as a result set");
    }
}
