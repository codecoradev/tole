//! Typed blocking client for the `tole serve` protocol (#1040, part of the UI
//! epic #523).
//!
//! Every graphical client (desktop, later mobile and web) talks to `tole
//! serve`; this crate is the Rust side of that contract. It holds no agent
//! logic and no session host: requests go out, typed answers and a live event
//! stream come back.
//!
//! ```no_run
//! use tole_client::{Client, Event, NewSession, PromptRequest, ReconnectPolicy, StreamItem};
//!
//! let client = Client::connect_tcp("http://127.0.0.1:7777", Some("token".into()), false)?;
//! let id = client.create_session(&NewSession::default())?.session_id;
//! let mut events = client.follow_events(&id, 0, ReconnectPolicy::default());
//! let (done_tx, done_rx) = std::sync::mpsc::channel();
//! std::thread::scope(|s| -> tole_client::Result<()> {
//!     // The prompt call blocks until the turn stops; the stream shows it live.
//!     s.spawn(|| done_tx.send(client.prompt(&id, &PromptRequest::text("hello"))));
//!     for item in events.by_ref() {
//!         match item? {
//!             StreamItem::Event(Event::TextDelta { text }) => print!("{text}"),
//!             // Entries, tool events, approvals, resync...
//!             StreamItem::Event(_) => {}
//!             StreamItem::Heartbeat => {}
//!             // Streamed text of an unfinished turn may have gaps now.
//!             StreamItem::Reconnected { .. } => {}
//!         }
//!         // Checked after every item (a heartbeat arrives within 15 s).
//!         if let Ok(reply) = done_rx.try_recv() {
//!             println!("\nstopped: {}", reply?.stop_reason);
//!             break;
//!         }
//!     }
//!     Ok(())
//! })?;
//! # Ok::<(), tole_client::Error>(())
//! ```
//!
//! # Transports
//!
//! A [`Transport`] opens the byte connection; HTTP and SSE framing sit above
//! it. Today: HTTP over TCP ([`TcpTransport`]), plain `http://` only to
//! loopback, or to a private-network address or host when the caller sets
//! `trusted_private_network` (a public IP is never accepted). On macOS and
//! Linux a Unix socket (`UnixTransport`, for `tole serve --socket`) is the
//! local transport of the desktop; it refuses a socket in a directory that is
//! not owner-only. HTTPS (#1044) and the Windows named pipe (#1051) are the
//! same trait; see [`transport`]. A transport is chosen when the [`Client`] is
//! built; nothing else in it depends on which one:
//!
//! ```no_run
//! # #[cfg(unix)] {
//! use tole_client::Client;
//!
//! let client = Client::connect_unix("/run/user/1000/tole/serve.sock", Some("token".into()))?;
//! println!("{:?}", client.health()?);
//! # }
//! # Ok::<(), tole_client::Error>(())
//! ```
//!
//! # Secrets
//!
//! The bearer token is sent in the `Authorization` header only. It is not in
//! `Debug` output, errors or any log (this crate does not log).
//!
//! # Not yet
//!
//! Routes beyond the first desktop set (interrupt, detach, fork, tree,
//! compact, name, model, effort, images, missions, and the discussion, cron
//! and profile routes of #1042) and the generated JSON Schema. The types
//! derive `Serialize` and `Deserialize`, so a schema can be added without
//! reshaping them.

mod client;
mod error;
mod http;
mod resume;
mod sse;
pub mod transport;
mod types;
#[cfg(unix)]
mod unix;

pub use client::{Client, IndexQuery, Route};
pub use error::{Error, Result};
pub use http::ResponseBody;
pub use resume::{ReconnectPolicy, ResumingStream, StreamItem};
pub use sse::{Event, Frame, FrameReader, Ready, SseItem, SseParser};
pub use transport::{Connection, TcpTransport, Transport};
pub use types::*;
#[cfg(unix)]
pub use unix::UnixTransport;
