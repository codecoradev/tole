//! The `data:` payloads of the event frames that carry more than a shared
//! type. Private to the decoder, except [`Changed`]; the committed JSON Schema
//! describes them (#1068), which is why they are named types and not local
//! structs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `text_delta` and `reasoning`.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct TextData {
    pub(crate) text: String,
}
/// `tool_started`.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ToolStartedData {
    pub(crate) tool: String,
    pub(crate) input: Value,
}
/// `tool_finished`.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ToolFinishedData {
    pub(crate) tool: String,
    pub(crate) ok: bool,
    pub(crate) preview: String,
}
/// `resync`: ephemeral frames this slow stream lost.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ResyncData {
    pub(crate) dropped: u64,
}
/// `closed`: the session left the server.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ClosedData {
    pub(crate) id: String,
}

/// `changed` (discussion stream, #1094): the ledger of a discussion is now at
/// `seq`. A signal, not the ledger's lines: read the discussion with
/// `GET /discussions/{id}`. Several ledger steps may
/// coalesce into one frame, and no contribution or text is ever in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Changed {
    /// Position of the last ledger line; the `after` a resume would use.
    pub seq: u64,
    /// The round being worked on.
    pub round: u32,
    /// What the ledger says must happen next: `contribute`, `summarize`,
    /// `check`, `decide`, `finish` or `done`. A string, not a closed set.
    pub step: String,
    pub tokens_used: u64,
    pub closed: bool,
}

/// `state` (discussion stream): this server's view of the discussion flipped
/// (`running`, `interrupted`; the ledger does not record it).
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct StateData {
    pub(crate) state: String,
}
