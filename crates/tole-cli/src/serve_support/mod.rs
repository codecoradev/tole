//! Support modules of the serve surface: `images` (inline image input),
//! `events` (the event stream), `protocol` (wire types) and `auth` (the
//! bearer token). The `serve`, `mcp_http` and `acp` commands themselves are
//! binary-only modules in `main.rs` and stay flat.

pub mod auth;
pub mod events;
pub mod images;
pub mod protocol;
