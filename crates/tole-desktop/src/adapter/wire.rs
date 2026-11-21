//! Reading the keys the client keeps unmodelled, and the raw JSON of a frame.
//!
//! `Approval` models the fields serve's own rows have; a delegated child's row
//! has others, which the client keeps in `extra` as the server's JSON.

use serde_json::Value;
use tole_client::Approval;

/// A JSON value as text: a string as it is, `None` for `null`, and for any
/// other JSON type its JSON text. A value that is present but not a string is
/// never read as "not reported": that would make a risk the server did state
/// look like no risk at all, or a request look like it has no tool. The JSON
/// text is not a code the model knows, so the model reads it as its most
/// careful value (an unknown risk is a danger) and shows it as it is.
pub(super) fn json_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The text at `key` of the row's unmodelled fields ([`json_text`]); `None` for
/// a missing key.
pub(super) fn extra_text(row: &Approval, key: &str) -> Option<String> {
    row.extra.get(key).and_then(json_text)
}

/// A non-negative number of the raw data as whole units: an integer as it is,
/// a fraction rounded down, and `None` for a missing key, a negative number, a
/// non-number or one that is not finite.
pub(super) fn whole_number(data: &Value, key: &str) -> Option<u64> {
    let v = data.get(key)?;
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
    })
}
