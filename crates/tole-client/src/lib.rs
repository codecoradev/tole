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
//! `trusted_private_network` (a public IP is never accepted); and HTTPS over
//! TCP ([`TlsTransport`], #1071) for a remote serve behind a TLS reverse
//! proxy, to any host, with the certificate always verified. On macOS and
//! Linux a Unix socket (`UnixTransport`, for `tole serve --socket`) is the
//! local transport of the desktop; it refuses a socket in a directory that is
//! not owner-only. The Windows named pipe (#1051) is the same trait; see
//! [`transport`]. A transport is chosen when the [`Client`] is built; nothing
//! else in it depends on which one. [`Client::connect`] picks plain HTTP or
//! HTTPS from the scheme of the url:
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
//! # HTTPS
//!
//! `tole serve` does not speak TLS; a remote one is reached through a reverse
//! proxy that terminates it (`docs/mobile-control.md`). The client verifies the
//! proxy's certificate against the bundled public roots, and there is no way to
//! turn that off: a certificate that does not verify is [`Error::Tls`], raised
//! before the token is written. A server with a private CA or a self-signed
//! certificate is reached by trusting exactly that certificate, which replaces
//! the public roots for the connection:
//!
//! ```no_run
//! use tole_client::{Client, TlsTransport};
//!
//! let ca = std::fs::read("serve-ca.pem")?;
//! let transport = TlsTransport::new("https://tole.example.com")?.with_ca_pem(&ca)?;
//! let client = Client::new(transport).with_token("token");
//! println!("{:?}", client.health()?);
//! // Or, with the public roots: Client::connect_https("https://tole.example.com", Some(token))
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Redirects are never followed (a 3xx is an [`Error::Status`]), so the token
//! cannot be carried to another host.
//!
//! # Secrets
//!
//! The bearer token is sent in the `Authorization` header only: never in a url
//! (a url with a user, path or query is refused, without echoing it), never on a
//! command line. It is not in `Debug` output, errors or any log (this crate does
//! not log), and over HTTPS it is written only after the handshake verified the
//! server.
//!
//! # JSON Schema
//!
//! `schema/tole-client.schema.json` is generated from these types (a test, no
//! dependency of the crate) and a check fails when it is out of date; see
//! `scripts/client-schema.sh`. Mobile and web clients build their models from
//! it.
//!
//! # Routes
//!
//! One method per route of the desktop's set: sessions (create, resume, prompt,
//! cancel, interrupt, detach, status, entries, events, fork, tree, compact,
//! name, model, effort, images), approvals, models, missions, and the
//! discussion, cron, profile, skill and tool routes of #1042. [`Route::ALL`]
//! lists them. `crates/tole-cli/tests/client_routes.rs` compares that list with
//! the routes `tole serve` registers and fails on a route on one side only,
//! except for the few the client leaves out on purpose.

mod client;
mod error;
mod http;
mod resume;
#[cfg(test)]
mod schema;
mod sse;
mod tls;
pub mod transport;
mod types;
#[cfg(unix)]
mod unix;

pub use client::{Client, IndexQuery, Route};
pub use error::{Error, Result, MAX_STATUS_MESSAGE};
pub use http::ResponseBody;
pub use resume::{ReconnectPolicy, ResumingStream, StreamItem};
pub use sse::{Event, Frame, FrameReader, Ready, SseItem, SseParser};
pub use tls::TlsTransport;
pub use transport::{Connection, TcpTransport, Transport};
pub use types::*;
#[cfg(unix)]
pub use unix::UnixTransport;
