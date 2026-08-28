//! Namespaced mutable registers.
//!
//! Registers are the only mutable state in the logical model (everything else
//! is append-only). A register cell is addressed by `(namespace, key)` where
//! the namespace is a dotted string whose first segment must be one of the
//! four core families:
//!
//! | Family     | Meaning                                          |
//! |------------|--------------------------------------------------|
//! | `lane`     | cursor into the entry tree (e.g. `lane.leaf`)    |
//! | `op`       | in-flight operation state (e.g. `op.state`)      |
//! | `pending`  | effect-sandwich pending intents                  |
//! | `fact`     | agent-maintained facts / long-lived scratch data |
//!
//! On the wire (JSONL) each write is one object with a `"kind":"register"`
//! discriminator added by the storage layer:
//!
//! ```json
//! {"kind":"register","op":"set","seq":102,"namespace":"op.state","key":"op_9","value":{...}}
//! {"kind":"register","op":"delete","seq":131,"namespace":"op.state","key":"op_9"}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The register families understood by this build.
pub const FAMILIES: [&str; 4] = ["lane", "op", "pending", "fact"];

/// The mutation applied to a register cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegisterOp {
    /// Overwrite the cell's value (create or replace).
    Set,
    /// Remove the cell. Deleting a missing key is a legal no-op.
    Delete,
}

/// Returns `true` when `namespace` is rooted in a known family
/// (`"<family>"` or `"<family>.<sub>"`).
pub fn is_valid_namespace(namespace: &str) -> bool {
    match namespace.split_once('.') {
        Some((family, sub)) => FAMILIES.contains(&family) && !sub.is_empty() && !sub.contains('.'),
        None => FAMILIES.contains(&namespace),
    }
}

/// One register mutation inside a [`Commit`](crate::storage::Commit).
#[derive(Debug, Clone)]
pub struct RegisterWrite {
    /// Set or delete.
    pub op: RegisterOp,
    /// Dotted namespace, e.g. `op.state`. See [`FAMILIES`].
    pub namespace: String,
    /// Cell key, unique within the namespace.
    pub key: String,
    /// Value for [`RegisterOp::Set`] (required); ignored/`None` for delete.
    pub value: Option<Value>,
}

impl RegisterWrite {
    /// Overwrites a register cell.
    pub fn set(namespace: &str, key: impl Into<String>, value: Value) -> Self {
        Self {
            op: RegisterOp::Set,
            namespace: namespace.to_string(),
            key: key.into(),
            value: Some(value),
        }
    }

    /// Removes a register cell (idempotent).
    pub fn delete(namespace: &str, key: impl Into<String>) -> Self {
        Self {
            op: RegisterOp::Delete,
            namespace: namespace.to_string(),
            key: key.into(),
            value: None,
        }
    }
}
