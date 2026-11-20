//! Typed blocking client for the `tole serve` protocol (#1040, part of the UI
//! epic #523).
//!
//! Every graphical client (desktop, later mobile and web) talks to `tole
//! serve`; this crate is the Rust side of that contract. It holds no agent
//! logic and no session host.
//!
//! This first piece has the [`Transport`] layer and the error type. The typed
//! requests and responses, the HTTP framing, the `Client` and the event stream
//! follow.
//!
//! # Transports
//!
//! A [`Transport`] opens the byte connection. Today: HTTP over TCP
//! ([`TcpTransport`]), plain `http://` only to loopback, or to a
//! private-network address or host when the caller sets
//! `trusted_private_network` (a public IP is never accepted). A Unix socket
//! (after #1043) and HTTPS (#1044) are the same trait; see [`transport`].
//!
//! # Secrets
//!
//! The bearer token is sent in the `Authorization` header only. It is not in
//! `Debug` output, errors or any log (this crate does not log).
//!
//! # Not yet
//!
//! The wire types, the HTTP layer and the `Client`, the live event stream, routes beyond the
//! first desktop set (interrupt, detach, fork, tree, compact, name, model,
//! effort, images, missions, and the discussion, cron and profile routes of
//! #1042) and the generated JSON Schema. The types derive `Serialize` and
//! `Deserialize`, so a schema can be added without reshaping them.

mod error;
pub mod transport;

pub use error::{Error, Result};
pub use transport::{Connection, TcpTransport, Transport};
