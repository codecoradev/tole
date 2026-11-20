//! Support modules of the serve surface: `images` (inline image input),
//! `events` (the event stream), `protocol` (wire types), `transport` (TCP or local-socket listener, #1043) and `auth` (the
//! bearer token) and `profiles` (per-session profiles, #740). The `serve`, `mcp_http` and `acp` commands themselves are
//! binary-only modules in `main.rs` and stay flat.

pub mod auth;
pub mod events;
pub mod images;
pub mod profiles;
pub mod protocol;
pub mod transport;
