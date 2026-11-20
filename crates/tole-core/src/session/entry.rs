//! Write-once conversation tree entries.
//!
//! Entries are the append-only, immutable log of the conversation. Every entry
//! references its parent (`parentId`), so the history forms a tree: branching
//! (exploration, replanning) produces siblings, corrections append new entries
//! — existing entries are never edited or deleted.
//!
//! On the wire (JSONL) an entry is written as a single object with an extra
//! `"kind":"entry"` discriminator added by the storage layer:
//!
//! ```json
//! {"kind":"entry","seq":101,"id":"e_50","parentId":"e_41","type":"message","timestamp":1756200000000,"payload":{...}}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Kind of an entry, serialized as the `"type"` field.
///
/// Deliberately a transparent string newtype instead of a closed enum: files
/// written by newer binaries may carry entry types this build does not know,
/// and replay must not fail on them (forward compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryType(String);

impl EntryType {
    /// A user or assistant message.
    pub const MESSAGE: &'static str = "message";
    /// An intent to execute a tool (effect sandwich, step 1).
    pub const INTENT: &'static str = "intent";
    /// The result of a tool execution (effect sandwich, step 3).
    pub const TOOL_RESULT: &'static str = "tool_result";
    /// An error observation.
    pub const ERROR: &'static str = "error";

    /// Creates a typed entry kind from a static string constant.
    pub fn new(kind: &'static str) -> Self {
        Self(kind.to_string())
    }

    /// Creates an entry kind from an owned string (e.g. deserialized input).
    pub fn from_string(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The wire name of this entry kind.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&'static str> for EntryType {
    fn eq(&self, other: &&'static str) -> bool {
        self.0.as_str() == *other
    }
}

/// A committed, immutable conversation entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Global monotonic sequence number assigned at commit time.
    pub seq: u64,
    /// Unique entry id (`e_<seq>` when auto-assigned).
    pub id: String,
    /// Parent entry id; `None` only for the session root entry.
    #[serde(rename = "parentId", default)]
    pub parent_id: Option<String>,
    /// What kind of entry this is (see [`EntryType`]).
    #[serde(rename = "type")]
    pub kind: EntryType,
    /// Creation time, milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// Type-specific payload. Schema is owned by the entry kind.
    pub payload: Value,
}

/// Input for a new entry inside a [`Commit`](crate::storage::Commit).
///
/// The storage layer assigns `seq` and (when omitted) `id` atomically at
/// commit time, so callers never manage sequence numbers themselves.
#[derive(Debug, Clone)]
pub struct NewEntry {
    /// Explicit entry id; `None` → auto `e_<seq>`.
    pub id: Option<String>,
    /// Parent entry id. Must reference an already-committed entry.
    pub parent_id: Option<String>,
    /// Entry kind.
    pub kind: EntryType,
    /// Type-specific payload.
    pub payload: Value,
    /// Creation time in ms; `0` → now.
    pub timestamp: u64,
}

impl NewEntry {
    /// Builds a new entry input with an explicit parent.
    pub fn with_parent(parent_id: impl Into<String>, kind: EntryType, payload: Value) -> Self {
        Self {
            id: None,
            parent_id: Some(parent_id.into()),
            kind,
            payload,
            timestamp: 0,
        }
    }

    /// Builds a root entry input (no parent).
    pub fn root(kind: EntryType, payload: Value) -> Self {
        Self {
            id: None,
            parent_id: None,
            kind,
            payload,
            timestamp: 0,
        }
    }
}
