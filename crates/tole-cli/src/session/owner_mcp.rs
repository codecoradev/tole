//! The MCP client servers the OWNER configured for server sessions (#946).
//!
//! `tole serve` and `tole acp` read the owner's server list once, at
//! startup, from the command line or the USER config, and install it here.
//! A server session ([`crate::session::host::open_session_with`]) reads the
//! list from here and from nowhere else: not from a request, a header, the
//! session's overrides, the served repository's files or the `PATH` (there is
//! no auto-preset on a server face). The list is set at most once, before
//! the first session opens, and cannot be changed afterwards.
//!
//! Naming a server here only says "start it if the plugin lock approves it":
//! every server still goes through the same lock and sandbox checks as on
//! the CLI (`tool_roster::build`), and its tools stay `Write`.

use std::sync::OnceLock;

static OWNER: OnceLock<Vec<String>> = OnceLock::new();

/// Installs the owner's server specs (`name=command args`), once at startup;
/// an empty list is a choice too ("none configured"). Each spec is parsed here
/// so a malformed one stops startup. A second call with another list fails:
/// the list cannot be replaced. In a build without MCP support a non-empty
/// list is refused (nothing could attach it).
pub fn set(specs: Vec<String>) -> Result<(), String> {
    #[cfg(not(feature = "mcp"))]
    if !specs.is_empty() {
        return Err("MCP client servers need the `mcp` feature (this build lacks it)".into());
    }
    #[cfg(feature = "mcp")]
    for s in &specs {
        tole_core::mcp::McpServerConfig::parse(s)?;
    }
    match OWNER.set(specs.clone()) {
        Ok(()) => Ok(()),
        Err(_) if OWNER.get() == Some(&specs) => Ok(()),
        Err(_) => Err("the owner's MCP servers were already set".to_string()),
    }
}

/// The owner's server specs, or `None` when the entrypoint never called
/// [`set`] (an error for a session host, never a silent default).
pub fn get() -> Option<&'static [String]> {
    OWNER.get().map(Vec::as_slice)
}

/// [`get`], empty when unset (for readers that only report).
pub fn specs() -> &'static [String] {
    get().unwrap_or(&[])
}
