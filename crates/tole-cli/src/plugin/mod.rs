//! Plugin support: the approval `gate` for plugin specs and the `plugin`
//! subcommand implementation (`cmd`, behind the `mcp` feature).

pub mod bundle;
#[cfg(feature = "mcp")]
pub mod cmd;
pub mod gate;
pub mod warn;
