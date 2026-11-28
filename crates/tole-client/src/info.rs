//! `GET /info` (#1024): what a serve says about itself.

use serde::{Deserialize, Serialize};

/// `GET /info`: the server's version, the version of the wire contract and the
/// names of what it has. A server older than the route answers `404`, which
/// [`Client::info`](crate::Client::info) returns as an error: such a server has
/// none of the features that were added after it. Keys this client does not
/// know are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// The `tole` binary's version.
    pub version: String,
    /// The version of the serve wire contract, the same number `GET /health`
    /// reports as `protocol`.
    pub protocol_version: u32,
    /// Sorted, stable feature names (`entries`, `events.sse`, `missions`, ...).
    pub features: Vec<String>,
    /// What the token this client sent may do: `full` or `read-only` (an open
    /// vocabulary: treat an unknown word as `read-only`). `None` from a server
    /// that does not report it, which has no read-only tokens either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Info {
    /// Whether the server lists `feature`.
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}
