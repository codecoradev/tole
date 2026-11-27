//! The typed problem of a settled turn (#1023): the `problem` of a prompt answer.

use serde::{Deserialize, Serialize};

/// Why a turn stopped without an answer: the `problem` next to `stopReason` in
/// the `POST /sessions/{id}/prompt` answer. It carries no wording; a client
/// words it from `kind` and `subject`. `kind` is a string, not a closed set:
/// a newer server may send a kind this client does not know, which then reads
/// as a problem of an unknown kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Problem {
    /// `provider`, `storage`, `loop`, `unknown_tool`, `stop_gate`, `approval`,
    /// `internal` or `budget` today.
    pub kind: String,
    /// What it is about: the tool of a loop, an unknown tool or an approval.
    pub subject: Option<String>,
    /// A plain retry of the same turn may succeed (a transient provider failure).
    pub retryable: bool,
    /// The technical text for a "show details" disclosure: redacted with the
    /// session's secrets and cut by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
