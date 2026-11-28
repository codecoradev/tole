//! Support modules of the serve surface: `images` (inline image input), `events` (the event
//! stream), `protocol` (wire types), `transport` (TCP or local-socket listener, #1043), `shutdown`
//! (SIGTERM/SIGINT and the drain, #1014), `auth` (the bearer token), `device_tokens` (per-device
//! named tokens, #1320), `profiles` (per-session profiles, #740) and `ready` (`GET /ready`, #1017).
//! The `serve`, `mcp_http` and `acp` commands are binary-only modules in `main.rs` and stay flat.

pub mod auth;
pub mod device_tokens;
pub mod events;
pub mod images;
pub mod profiles;
pub mod protocol;
#[cfg(feature = "shell-tools")]
pub mod ready;
pub mod shutdown;
pub mod transport;
