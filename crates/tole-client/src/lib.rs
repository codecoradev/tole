//! Typed blocking client for the `tole serve` protocol (#1040, part of the UI
//! epic #523).
//!
//! Every graphical client (desktop, later mobile and web) talks to `tole
//! serve`; this crate is the Rust side of that contract. It holds no agent
//! logic and no session host: requests go out, typed answers come back.
//!
//! ```no_run
//! use tole_client::{Client, NewSession, PromptRequest};
//!
//! let client = Client::connect_tcp("http://127.0.0.1:7777", Some("token".into()), false)?;
//! let id = client.create_session(&NewSession::default())?.session_id;
//! // Blocks until the turn stops (an answer, or a Write waiting for approval).
//! let reply = client.prompt(&id, &PromptRequest::text("hello"))?;
//! println!("{}: {:?}", reply.stop_reason, reply.text);
//! # Ok::<(), tole_client::Error>(())
//! ```
//!
//! # Transports
//!
//! A [`Transport`] opens the byte connection; HTTP framing sits above it.
//! Today: HTTP over TCP ([`TcpTransport`]), plain `http://` only to
//! loopback, or to a private-network address or host when the caller sets
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
//! The live event stream (`GET /sessions/{id}/events`, with resume), routes
//! beyond the first desktop set (interrupt, detach, fork, tree, compact,
//! name, model, effort, images, missions, and the discussion, cron and
//! profile routes of #1042) and the generated JSON Schema. The types derive
//! `Serialize` and `Deserialize`, so a schema can be added without reshaping
//! them.

mod client;
mod error;
mod http;
pub mod transport;
mod types;

pub use client::{Client, IndexQuery, Route};
pub use error::{Error, Result};
pub use transport::{Connection, TcpTransport, Transport};
pub use types::*;
