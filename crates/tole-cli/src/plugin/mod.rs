//! Plugin support: the approval `gate` for plugin specs and the `plugin`
//! subcommand implementation (`cmd`, behind the `mcp` feature).

#[cfg(feature = "mcp")]
pub mod cmd;
pub mod gate;
